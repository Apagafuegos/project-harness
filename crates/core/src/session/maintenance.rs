//! Read-only export, journal-only deletion and explicit artifact reclamation.
//!
//! Artifacts are shared and content-addressed, so nothing here decides that a
//! writer is finished: deletion removes one journal under the writer lock, and
//! unreferenced bytes are reclaimed only by an explicit collection with a grace
//! window (decision 16). See
//! `docs/decisions/0030-session-export-deletion-and-gc.md`.

use super::{
    ArtifactRef, CallId, ContentBlock, ContextMode, DataRoot, Digest, EntryId, EntryPayload,
    Inspection, JournalEvent, ModelRef, Record, RecordPayload, ResponseOutcome, SessionId,
    SessionSnapshot, StoreError, StoredEntry, catalog::session_title, codec,
    error::Result as StoreResult, store,
};
use imbl::OrdMap;
use rustix::fs::{self, AtFlags, FlockOperation, Mode, OFlags, RenameFlags};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// An unreferenced artifact younger than this is never removed, so bytes
/// written before the journal commit that references them stay put.
pub const ARTIFACT_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// An export, deletion or collection that did not complete.
///
/// Every refusal's `Display` form is the stable code clients show and scripts
/// match; a storage failure keeps its own message.
#[derive(Debug, thiserror::Error)]
pub enum MaintenanceError {
    /// No journal has this session identity.
    #[error("session_not_found")]
    NotFound,
    /// Another engine holds the journal's exclusive lock.
    #[error("session_locked")]
    Locked,
    /// The output path already exists; exports never overwrite.
    #[error("export_exists")]
    Exists,
    /// A referenced artifact is absent, unsafe, or fails verification.
    #[error("export_artifact_invalid: {0}")]
    ArtifactInvalid(Digest),
    /// The journal has no complete valid history to export.
    #[error("journal_invalid: {0}")]
    JournalInvalid(String),
    /// One journal could not be validated, so collection refuses the whole run.
    #[error("gc_journal_unreadable: {0}")]
    JournalUnreadable(SessionId),
    /// An output path must name an existing parent directory and a file name.
    #[error("invalid_output_path")]
    InvalidPath,
    /// The underlying storage operation failed.
    #[error(transparent)]
    Storage(#[from] StoreError),
}
impl From<std::io::Error> for MaintenanceError {
    fn from(error: std::io::Error) -> Self {
        Self::Storage(StoreError::Io(error))
    }
}
impl From<rustix::io::Errno> for MaintenanceError {
    fn from(error: rustix::io::Errno) -> Self {
        Self::Storage(StoreError::Io(error.into()))
    }
}

type Result<T> = std::result::Result<T, MaintenanceError>;

/// What an artifact collection should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcOptions {
    /// List the candidates and their total size without removing anything.
    pub dry_run: bool,
    /// Minimum age of an unreferenced artifact before it may be removed.
    pub grace: Duration,
}
impl Default for GcOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            grace: ARTIFACT_GRACE,
        }
    }
}

/// One artifact a collection removed, or would remove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcCandidate {
    /// Content address of the artifact.
    pub sha256: Digest,
    /// Exact byte length that was, or would be, freed.
    pub bytes: u64,
}

/// Outcome of one explicit artifact collection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcReport {
    /// Candidates in content-address order: the removed artifacts of a real
    /// run, or the proposed ones of a dry run.
    pub candidates: Vec<GcCandidate>,
    /// Journals that were read and validated under shared locks.
    pub journals: usize,
    /// Whether nothing was removed.
    pub dry_run: bool,
}
impl GcReport {
    /// Total bytes removed, or total bytes a dry run would remove.
    pub fn bytes(&self) -> u64 {
        self.candidates
            .iter()
            .map(|candidate| candidate.bytes)
            .sum()
    }
}

fn open_error(error: StoreError) -> MaintenanceError {
    match error {
        StoreError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            MaintenanceError::NotFound
        }
        error => error.into(),
    }
}

fn lock(file: &File, exclusive: bool) -> Result<()> {
    // See `crate::spawn_gate`: a forked child keeps an inherited lock alive.
    #[cfg(test)]
    let _spawn_gate = crate::spawn_gate::hold();
    let operation = if exclusive {
        FlockOperation::NonBlockingLockExclusive
    } else {
        FlockOperation::NonBlockingLockShared
    };
    match fs::flock(file, operation) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(MaintenanceError::Locked),
        Err(error) => Err(StoreError::from(error).into()),
    }
}

/// The fully valid snapshot of an inspection, or the code that prevents export.
fn valid_snapshot(inspection: &Inspection) -> Result<SessionSnapshot> {
    match &inspection.diagnostic {
        Some(diagnostic) => Err(MaintenanceError::JournalInvalid(diagnostic.code.clone())),
        None => inspection
            .snapshot
            .clone()
            .ok_or_else(|| MaintenanceError::JournalInvalid("missing_header".into())),
    }
}

/// Every artifact a record can reference. This mirrors the storage validator
/// exactly: collection may never remove something replay would need.
fn record_references(record: &Record, out: &mut BTreeSet<Digest>) {
    fn blocks(content: &[ContentBlock], out: &mut BTreeSet<Digest>) {
        for block in content {
            match block {
                ContentBlock::Image { artifact, .. }
                | ContentBlock::Audio { artifact, .. }
                | ContentBlock::File { artifact, .. }
                | ContentBlock::ProviderState { artifact, .. } => {
                    out.insert(artifact.sha256.clone());
                }
                _ => (),
            }
        }
    }
    match &record.payload {
        RecordPayload::SessionCreated(header) => {
            out.insert(header.configuration.sha256.clone());
        }
        RecordPayload::Commit(commit) => {
            // A command fingerprint and a configuration fingerprint are bare
            // artifact digests rather than `ArtifactRef` values.
            if let Some(fingerprint) = &commit.command_fingerprint {
                out.insert(fingerprint.clone());
            }
            for entry in &commit.entries {
                match &entry.payload {
                    EntryPayload::UserMessage(message) => blocks(&message.content, out),
                    EntryPayload::AssistantResponse(response) => {
                        blocks(&response.content, out);
                        if let Some(continuation) = &response.continuation {
                            out.insert(continuation.artifact.sha256.clone());
                        }
                    }
                    EntryPayload::ToolResult(result) => {
                        blocks(&result.result.content, out);
                        for reference in &result.result.metadata.artifacts {
                            out.insert(reference.sha256.clone());
                        }
                    }
                    EntryPayload::Compaction(compaction) => {
                        out.insert(compaction.source_manifest.sha256.clone());
                        out.insert(compaction.configuration_fingerprint.clone());
                    }
                    // A note names entries and turns only: it references no
                    // artifact, and never pre-image bytes.
                    EntryPayload::UndoNote(_) => (),
                    EntryPayload::ModelChanged(_) | EntryPayload::ContextControl(_) => (),
                }
            }
            for event in &commit.events {
                match event {
                    JournalEvent::TurnStarted(start) => {
                        out.insert(start.configuration.sha256.clone());
                    }
                    JournalEvent::ConfigurationChanged(change) => {
                        out.insert(change.configuration.sha256.clone());
                    }
                    JournalEvent::ApprovalRequested(request) => {
                        out.insert(request.plan.sha256.clone());
                    }
                    JournalEvent::AuthorizationDecided(decision) => {
                        out.insert(decision.plan.sha256.clone());
                    }
                    JournalEvent::TailRepaired(repair) => {
                        out.insert(repair.quarantine_artifact.sha256.clone());
                    }
                    // Captured pre-images are referenced directly here, not
                    // only through the plan artifact, so collection keeps the
                    // bytes a later undo would restore.
                    JournalEvent::ExecutionIntent(intent) => {
                        if let Some(image) = &intent.file_image
                            && let Some(pre) = &image.pre_image
                        {
                            out.insert(pre.sha256.clone());
                        }
                    }
                    JournalEvent::UndoIntent(intent) => {
                        out.insert(intent.configuration.sha256.clone());
                        for target in &intent.targets {
                            if let Some(pre) = &target.pre_image {
                                out.insert(pre.sha256.clone());
                            }
                        }
                    }
                    _ => (),
                }
            }
        }
    }
}

/// Decode every record of a locked journal and collect its artifact
/// references. The caller has already proved the whole file valid, so each
/// physical line is bounded and LF-terminated.
fn references(file: &mut File) -> StoreResult<BTreeSet<Digest>> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut out = BTreeSet::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.len() > codec::MAX_LINE_BYTES || line.last() != Some(&b'\n') {
            return Err(StoreError::Invalid("unbounded_record"));
        }
        let (record, _) = codec::decode(&line[..line.len() - 1])?;
        record_references(&record, &mut out);
    }
    Ok(out)
}

impl DataRoot {
    /// Take the shared, non-blocking lock the catalog uses and return the open
    /// journal. Every export holds it for the whole read.
    fn export_file(&self, id: &SessionId) -> Result<File> {
        let file = self.journal(id, false, false).map_err(open_error)?;
        lock(&file, false)?;
        Ok(file)
    }

    /// Validate a session without writing and return its open journal plus the
    /// snapshot it validated.
    fn export_snapshot(&self, id: &SessionId) -> Result<(File, SessionSnapshot)> {
        let mut file = self.export_file(id)?;
        let inspection = store::scan(self, &mut file, id)?;
        let snapshot = valid_snapshot(&inspection)?;
        Ok((file, snapshot))
    }

    /// The catalog title of a session, read-only under the shared lock.
    pub fn session_title(&self, id: &SessionId) -> Result<Option<String>> {
        let (_, snapshot) = self.export_snapshot(id)?;
        Ok(session_title(&snapshot))
    }

    /// Render the session as deterministic Markdown. With `all_branches` every
    /// branch head is named and a shared prefix appears once; otherwise the
    /// export follows the active branch from the root to the head.
    pub fn export_markdown(&self, id: &SessionId, all_branches: bool) -> Result<String> {
        let (_, snapshot) = self.export_snapshot(id)?;
        Ok(markdown_export(&snapshot, all_branches))
    }

    /// Write a Markdown export to a path that must not exist. The bytes are
    /// staged beside the target and published by a no-replace rename.
    pub fn export_markdown_to(
        &self,
        id: &SessionId,
        path: &Path,
        all_branches: bool,
    ) -> Result<()> {
        let text = self.export_markdown(id, all_branches)?;
        write_markdown_export(path, &text)
    }

    /// Copy a verified journal and every artifact it references into a new
    /// directory that is itself a data root. The source is only read.
    pub fn export_journal(&self, id: &SessionId, destination: &Path) -> Result<()> {
        let (mut journal, _) = self.export_snapshot(id)?;
        let references = references(&mut journal)?;
        let parent = present_parent(destination)?;
        let name = destination
            .file_name()
            .ok_or(MaintenanceError::InvalidPath)?;
        let parent_file = File::open(&parent).map_err(StoreError::from)?;
        match fs::mkdirat(&parent_file, name, Mode::RWXU) {
            Ok(()) => (),
            Err(rustix::io::Errno::EXIST) => return Err(MaintenanceError::Exists),
            Err(error) => return Err(StoreError::from(error).into()),
        }
        let created = fs::openat(
            &parent_file,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(StoreError::from)?;
        let result = (|| {
            super::artifacts::verify_private(&created, true)?;
            let sessions = child_directory(&created, "sessions")?;
            let artifacts = child_directory(&created, "artifacts")?;
            let mut target = fs::openat(
                &sessions,
                format!("{id}.jsonl").as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::RUSR | Mode::WUSR,
            )
            .map(File::from)?;
            journal.seek(SeekFrom::Start(0))?;
            std::io::copy(&mut journal, &mut target)?;
            target.flush()?;
            target.sync_all()?;
            sessions.sync_all()?;
            for digest in &references {
                copy_artifact(self, &artifacts, digest)?;
            }
            artifacts.sync_all()?;
            created.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            discard_export(&created);
        }
        result
    }

    /// Remove one journal under the exclusive writer lock. This never removes
    /// an artifact: shared content-addressed bytes are reclaimed by
    /// [`Self::collect_artifacts`] alone (decision 16).
    pub fn delete_session(&self, id: &SessionId) -> Result<()> {
        let file = self.journal(id, false, true).map_err(open_error)?;
        lock(&file, true)?;
        match fs::unlinkat(
            &self.sessions,
            format!("{id}.jsonl").as_str(),
            AtFlags::empty(),
        ) {
            Ok(()) => (),
            Err(rustix::io::Errno::NOENT) => return Err(MaintenanceError::NotFound),
            Err(error) => return Err(StoreError::from(error).into()),
        }
        self.sessions.sync_all()?;
        // The descriptor, and so the lock, outlives the name; release it last.
        drop(file);
        Ok(())
    }

    /// Session identities with a journal in the private sessions directory.
    fn journal_ids(&self) -> StoreResult<Vec<SessionId>> {
        let mut ids = Vec::new();
        for item in fs::Dir::read_from(&self.sessions)? {
            let item = item?;
            let Ok(name) = item.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".jsonl") else {
                continue;
            };
            let Ok(id) = stem.parse::<SessionId>() else {
                continue;
            };
            ids.push(id);
        }
        ids.sort();
        Ok(ids)
    }

    /// Remove artifacts that no readable journal references and whose
    /// modification time is older than the grace window.
    ///
    /// Any journal that cannot be locked and validated refuses the whole run,
    /// because a damaged journal might reference anything.
    pub fn collect_artifacts(&self, options: &GcOptions) -> Result<GcReport> {
        let mut referenced = BTreeSet::new();
        let mut journals = 0;
        for id in &self.journal_ids()? {
            let mut file = match self.journal(id, false, false) {
                Ok(file) => file,
                // A journal deleted after enumeration has nothing to reference.
                Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    continue;
                }
                Err(_) => return Err(MaintenanceError::JournalUnreadable(id.clone())),
            };
            if fs::flock(&file, FlockOperation::NonBlockingLockShared).is_err() {
                return Err(MaintenanceError::JournalUnreadable(id.clone()));
            }
            let inspection = store::scan(self, &mut file, id)
                .map_err(|_| MaintenanceError::JournalUnreadable(id.clone()))?;
            if inspection.diagnostic.is_some() {
                return Err(MaintenanceError::JournalUnreadable(id.clone()));
            }
            referenced.extend(
                references(&mut file)
                    .map_err(|_| MaintenanceError::JournalUnreadable(id.clone()))?,
            );
            journals += 1;
        }
        // Serialize selection (including every mtime read) and unlink against
        // artifact publication/reuse, not against the earlier journal scan.
        let _artifact_lock = self.lock_artifacts(true)?;
        let now = SystemTime::now();
        let mut candidates = Vec::new();
        for item in fs::Dir::read_from(&self.artifacts)? {
            let item = item?;
            let Some(digest) = item
                .file_name()
                .to_str()
                .ok()
                .and_then(|name| name.parse::<Digest>().ok())
            else {
                continue;
            };
            if referenced.contains(&digest) {
                continue;
            }
            // An artifact that is unsafe, absent or unreadable is left alone:
            // collection removes only bytes it verified as private files.
            let Ok(file) = self.artifact_file(&digest) else {
                continue;
            };
            let metadata = file.metadata()?;
            let age = now.duration_since(metadata.modified()?).unwrap_or_default();
            if age < options.grace {
                continue;
            }
            candidates.push(GcCandidate {
                sha256: digest,
                bytes: metadata.len(),
            });
        }
        self.io.check(super::io::Point::GcSelected)?;
        if !options.dry_run {
            for candidate in &candidates {
                match fs::unlinkat(&self.artifacts, candidate.sha256.as_str(), AtFlags::empty()) {
                    Ok(()) | Err(rustix::io::Errno::NOENT) => (),
                    Err(error) => return Err(StoreError::from(error).into()),
                }
            }
            if !candidates.is_empty() {
                self.artifacts.sync_all()?;
            }
        }
        Ok(GcReport {
            candidates,
            journals,
            dry_run: options.dry_run,
        })
    }
}

/// The parent directory of an explicit output path, treating a bare file name
/// as relative to the current directory.
fn present_parent(path: &Path) -> Result<PathBuf> {
    if path.file_name().is_none() {
        return Err(MaintenanceError::InvalidPath);
    }
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Ok(parent.to_path_buf()),
        _ => Ok(PathBuf::from(".")),
    }
}

fn child_directory(parent: &File, name: &str) -> StoreResult<File> {
    fs::mkdirat(parent, name, Mode::RWXU)?;
    let directory = fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)?;
    super::artifacts::verify_private(&directory, true)?;
    Ok(directory)
}

/// Render a snapshot the caller already owns, such as an engine's live view of
/// the session it is writing. Validation is the caller's: a snapshot that was
/// accepted by storage is complete by construction.
pub fn markdown_export(snapshot: &SessionSnapshot, all_branches: bool) -> String {
    markdown(snapshot, Some(snapshot.created_at().as_str()), all_branches)
}

/// Write already rendered Markdown to a path that must not exist. The bytes are
/// staged beside the target and published by a no-replace rename, so an
/// existing export is refused rather than truncated.
pub fn write_markdown_export(path: &Path, text: &str) -> Result<()> {
    write_new_file(path, text.as_bytes())
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = present_parent(path)?;
    let name = path.file_name().ok_or(MaintenanceError::InvalidPath)?;
    let directory = File::open(&parent).map_err(StoreError::from)?;
    let temporary = format!(".{}.tmp-{}", name.to_string_lossy(), uuid::Uuid::now_v7());
    let mut file = fs::openat(
        &directory,
        temporary.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map(File::from)
    .map_err(StoreError::from)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        match fs::renameat_with(
            &directory,
            temporary.as_str(),
            &directory,
            name,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => Ok(()),
            Err(rustix::io::Errno::EXIST) => Err(MaintenanceError::Exists),
            Err(error) => Err(StoreError::from(error).into()),
        }
    })();
    let _ = fs::unlinkat(&directory, temporary.as_str(), AtFlags::empty());
    result
}

/// Copy one artifact into the exported root, verifying the source and the
/// exact bytes written.
fn copy_artifact(root: &DataRoot, directory: &File, digest: &Digest) -> Result<()> {
    let reference = ArtifactRef {
        sha256: digest.clone(),
        bytes: root.artifact_size(digest)?,
    };
    root.verify_artifact(&reference)
        .map_err(|_| MaintenanceError::ArtifactInvalid(digest.clone()))?;
    root.io.check(super::io::Point::ExportArtifactVerified)?;
    let mut source = root.artifact_file(digest)?;
    let mut target = fs::openat(
        directory,
        digest.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map(File::from)?;
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied += read as u64;
        hasher.update(&buffer[..read]);
        target.write_all(&buffer[..read])?;
    }
    target.flush()?;
    target.sync_all()?;
    if copied != reference.bytes || format!("{:x}", hasher.finalize()) != reference.sha256.as_str()
    {
        return Err(MaintenanceError::ArtifactInvalid(digest.clone()));
    }
    Ok(())
}

/// Remove exactly the tree `export_journal` created. Nothing outside the new
/// directory is touched, and no entry is followed.
fn discard_export(created: &File) {
    for name in ["sessions", "artifacts"] {
        let directory = fs::openat(
            created,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(File::from);
        if let Ok(directory) = directory {
            if let Ok(items) = fs::Dir::read_from(&directory) {
                for item in items.flatten() {
                    let _ = fs::unlinkat(&directory, item.file_name(), AtFlags::empty());
                }
            }
            let _ = fs::unlinkat(created, name, AtFlags::REMOVEDIR);
        }
    }
    // The directory descriptor stays valid after its name is removed.
    let _ = created.sync_all();
}

fn heading(out: &mut String, level: &str, text: &str) {
    out.push_str(level);
    out.push(' ');
    out.push_str(text);
    out.push_str("\n\n");
}

fn paragraph(out: &mut String, text: &str) {
    out.push_str(text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
}

/// A note line carries one fact and never record payload bytes.
fn note(out: &mut String, text: &str) {
    out.push('_');
    out.push_str(text);
    out.push_str("_\n\n");
}

/// Fence content with more backticks than it contains, so a block can never
/// close early and the output stays byte-identical for the same journal.
fn fenced(out: &mut String, language: &str, body: &str) {
    let longest = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat((longest + 1).max(3));
    out.push_str(&fence);
    out.push_str(language);
    out.push('\n');
    out.push_str(body);
    if !body.is_empty() && !body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push_str("\n\n");
}

fn model_name(model: &ModelRef) -> String {
    format!("{}/{}", model.provider_instance, model.model)
}

fn reference_line(kind: &str, mime: &str, artifact: &ArtifactRef) -> String {
    format!(
        "- {kind}: `{mime}`, {} bytes, sha256 `{}`\n",
        artifact.bytes,
        digest_prefix(artifact)
    )
}

/// Twelve hexadecimal characters are enough to identify an artifact in text.
fn digest_prefix(artifact: &ArtifactRef) -> String {
    artifact.sha256.as_str().chars().take(12).collect()
}

/// Append the content blocks of one entry. Media is referenced, never embedded,
/// and opaque provider state is named without its bytes.
fn blocks(out: &mut String, content: &[ContentBlock]) {
    for block in content {
        match block {
            ContentBlock::Text { text } => paragraph(out, text),
            ContentBlock::ReasoningSummary { text } => {
                for line in text.lines() {
                    out.push_str(if line.is_empty() { ">" } else { "> " });
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }
            ContentBlock::ToolCall {
                name, arguments, ..
            } => {
                out.push_str(&format!("Tool call `{name}`:\n\n"));
                let value = serde_json::Value::Object(arguments.clone());
                let body = serde_json::to_string_pretty(&value).unwrap_or_default();
                fenced(out, "json", &body);
            }
            ContentBlock::Image {
                artifact,
                mime_type,
            } => out.push_str(&reference_line("Image", mime_type, artifact)),
            ContentBlock::Audio {
                artifact,
                mime_type,
            } => out.push_str(&reference_line("Audio", mime_type, artifact)),
            ContentBlock::File {
                artifact,
                mime_type,
                name,
            } => out.push_str(&reference_line(
                &format!("File `{name}`"),
                mime_type,
                artifact,
            )),
            ContentBlock::ProviderState {
                owner,
                schema,
                artifact,
            } => note(
                out,
                &format!(
                    "Opaque provider state omitted: `{}`, schema `{schema}`, {} bytes",
                    model_name(owner),
                    artifact.bytes
                ),
            ),
        }
        while !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
    }
}

/// A tool result's content: text is fenced in full, media stays a reference.
fn tool_content(out: &mut String, content: &[ContentBlock]) {
    for block in content {
        match block {
            ContentBlock::Text { text } => fenced(out, "text", text),
            ContentBlock::Image {
                artifact,
                mime_type,
            } => paragraph(out, reference_line("Image", mime_type, artifact).trim_end()),
            ContentBlock::Audio {
                artifact,
                mime_type,
            } => paragraph(out, reference_line("Audio", mime_type, artifact).trim_end()),
            ContentBlock::File {
                artifact,
                mime_type,
                name,
            } => paragraph(
                out,
                reference_line(&format!("File `{name}`"), mime_type, artifact).trim_end(),
            ),
            // Storage rejects any other block in a result.
            _ => (),
        }
    }
}

/// The name of the tool a result answers, from the call itself.
fn call_name(snapshot: &SessionSnapshot, call_id: &CallId) -> Option<String> {
    snapshot
        .entries()
        .filter_map(|(_, stored)| match &stored.entry.payload {
            EntryPayload::AssistantResponse(response) => Some(&response.content),
            _ => None,
        })
        .flat_map(|content| content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolCall {
                call_id: id, name, ..
            } if id == call_id => Some(name.clone()),
            _ => None,
        })
}

fn entry_section(out: &mut String, snapshot: &SessionSnapshot, stored: &StoredEntry, level: &str) {
    match &stored.entry.payload {
        EntryPayload::UserMessage(message) => {
            heading(out, level, "You");
            blocks(out, &message.content);
        }
        EntryPayload::AssistantResponse(response) => {
            heading(
                out,
                level,
                &format!("Assistant ({})", model_name(&response.model)),
            );
            blocks(out, &response.content);
            if response.outcome != ResponseOutcome::Completed {
                note(
                    out,
                    &format!("Outcome: `{}`", outcome_name(response.outcome)),
                );
            }
        }
        EntryPayload::ToolResult(result) => {
            let tool = call_name(snapshot, &result.result.call.call_id)
                .unwrap_or_else(|| result.result.call.call_id.to_string());
            heading(out, level, &format!("Tool result ({tool})"));
            let code = result
                .result
                .metadata
                .code
                .as_deref()
                .map(|code| format!(", code `{code}`"))
                .unwrap_or_default();
            out.push_str(&format!(
                "- status: `{}`{code}\n",
                status_name(result.result.status)
            ));
            if let Some(exit) = result.result.metadata.exit_code {
                out.push_str(&format!("- exit code: {exit}\n"));
            }
            if let Some(duration) = result.result.metadata.duration_ms {
                out.push_str(&format!("- duration: {duration} ms\n"));
            }
            if result.result.metadata.output_truncated {
                out.push_str("- the executor reported output truncation\n");
            }
            for artifact in &result.result.metadata.artifacts {
                out.push_str(&format!(
                    "- Artifact: {} bytes, sha256 `{}`\n",
                    artifact.bytes,
                    digest_prefix(artifact)
                ));
            }
            out.push('\n');
            tool_content(out, &result.result.content);
        }
        EntryPayload::Compaction(compaction) => {
            let model = compaction
                .model
                .as_ref()
                .map(|model| format!(", by `{}`", model_name(model)))
                .unwrap_or_default();
            note(
                out,
                &format!(
                    "Compaction `{}` version `{}` covers {} entries{model}",
                    compaction.strategy,
                    compaction.strategy_version,
                    compaction.covered_entry_ids.len()
                ),
            );
        }
        EntryPayload::ContextControl(control) => {
            let mode = match control.mode {
                ContextMode::Pinned => "pinned",
                ContextMode::Excluded => "excluded",
                ContextMode::Default => "default",
            };
            note(
                out,
                &format!("Context control: turn `{}` {mode}", control.turn_id),
            );
        }
        EntryPayload::ModelChanged(change) => note(
            out,
            &format!(
                "Model changed to `{}` ({} policy)",
                model_name(&change.model),
                portability_name(change.portability)
            ),
        ),
        // The recorded note text only: a note names paths and calls, never
        // pre-image bytes, artifact references or commands.
        EntryPayload::UndoNote(note) => {
            heading(out, level, "Undo");
            fenced(out, "text", &note.text);
        }
    }
}

fn status_name(status: super::ToolStatus) -> &'static str {
    match status {
        super::ToolStatus::Succeeded => "succeeded",
        super::ToolStatus::Failed => "failed",
        super::ToolStatus::InvalidArguments => "invalid_arguments",
        super::ToolStatus::Denied => "denied",
        super::ToolStatus::TimedOut => "timed_out",
        super::ToolStatus::Cancelled => "cancelled",
        super::ToolStatus::InfrastructureFailure => "infrastructure_failure",
        super::ToolStatus::Uncertain => "uncertain",
    }
}

fn outcome_name(outcome: ResponseOutcome) -> &'static str {
    match outcome {
        ResponseOutcome::Completed => "completed",
        ResponseOutcome::Failed => "failed",
        ResponseOutcome::Cancelled => "cancelled",
        ResponseOutcome::Interrupted => "interrupted",
    }
}

fn portability_name(portability: super::Portability) -> &'static str {
    match portability {
        super::Portability::Reject => "reject",
        super::Portability::OmitOptional => "omit_optional",
        super::Portability::ConvertSupported => "convert_supported",
    }
}

fn header(
    out: &mut String,
    snapshot: &SessionSnapshot,
    created: Option<&str>,
    model_at_head: &str,
) {
    heading(out, "#", &format!("Session {}", snapshot.session_id()));
    out.push_str(&format!("- Workspace: `{}`\n", snapshot.header().workspace));
    out.push_str(&format!("- Created: `{}`\n", created.unwrap_or("unknown")));
    out.push_str(&format!(
        "- Initial model: `{}`\n",
        model_name(&snapshot.header().initial_model)
    ));
    out.push_str(&format!("- Model at head: `{model_at_head}`\n\n"));
}

/// Children of every parent in journal order, and the branch heads: every leaf,
/// plus the durable head when it is not one.
fn tree(snapshot: &SessionSnapshot) -> (OrdMap<Option<EntryId>, Vec<&StoredEntry>>, Vec<EntryId>) {
    let mut children: OrdMap<Option<EntryId>, Vec<&StoredEntry>> = OrdMap::new();
    for (_, stored) in snapshot.entries() {
        children
            .entry(stored.entry.parent_id.clone())
            .or_default()
            .push(stored);
    }
    for parent in children.keys().cloned().collect::<Vec<_>>() {
        let mut siblings = children.remove(&parent).unwrap_or_default();
        siblings.sort_by_key(|stored| (stored.sequence, stored.entry_index));
        children.insert(parent, siblings);
    }
    let internal: BTreeSet<Option<EntryId>> = children.keys().cloned().collect();
    let mut heads: Vec<EntryId> = snapshot
        .entries()
        .filter(|(id, _)| !internal.contains(&Some((*id).clone())))
        .map(|(id, _)| id.clone())
        .collect();
    if let Some(head) = snapshot.head()
        && !heads.contains(head)
    {
        heads.push(head.clone());
    }
    heads.sort();
    (children, heads)
}

/// `## Shared history`, `## Branch <head>` or `## Branches <a>, <b>`: the run of
/// entries every named head still has in common, or the one head they belong to.
fn scope_heading(scope: &[EntryId], all: &[EntryId]) -> String {
    if scope.len() == 1 {
        // One head: name it, so even a single branch shows where it ends.
        format!("## Branch {}", scope[0])
    } else if scope == all {
        "## Shared history".into()
    } else {
        format!(
            "## Branches {}",
            scope
                .iter()
                .map(|id| id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn markdown(snapshot: &SessionSnapshot, created: Option<&str>, all_branches: bool) -> String {
    let head_model = snapshot
        .model_at(snapshot.head())
        .map(model_name)
        .unwrap_or_else(|_| "unknown".into());
    let mut out = String::new();
    header(&mut out, snapshot, created, &head_model);
    if all_branches {
        markdown_all_branches(&mut out, snapshot);
    } else {
        let ancestry = snapshot.ancestry(snapshot.head()).unwrap_or_default();
        for stored in &ancestry {
            entry_section(&mut out, snapshot, stored, "##");
        }
        if ancestry.is_empty() {
            note(&mut out, "No entries on this branch");
        }
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    out
}

fn markdown_all_branches(out: &mut String, snapshot: &SessionSnapshot) {
    let (children, heads) = tree(snapshot);
    if heads.is_empty() {
        note(out, "No entries on this branch");
        return;
    }
    // Each entry's scope: the branch heads whose ancestry contains it.
    let mut scopes: BTreeMap<EntryId, Vec<EntryId>> = BTreeMap::new();
    for head in &heads {
        for stored in snapshot.ancestry(Some(head)).unwrap_or_default() {
            scopes
                .entry(stored.entry.id.clone())
                .or_default()
                .push(head.clone());
        }
    }
    for scope in scopes.values_mut() {
        scope.sort();
        scope.dedup();
    }
    // Pre-order over journal order. A scope is one contiguous run, so a shared
    // prefix is emitted once and never repeated under a later branch heading.
    let mut stack: Vec<&StoredEntry> = Vec::new();
    let mut roots = children.get(&None).cloned().unwrap_or_default();
    roots.reverse();
    stack.extend(roots);
    let mut previous: Option<Vec<EntryId>> = None;
    while let Some(stored) = stack.pop() {
        let scope = scopes
            .get(&stored.entry.id)
            .cloned()
            .unwrap_or_else(|| vec![stored.entry.id.clone()]);
        if previous.as_ref() != Some(&scope) {
            out.push_str(&scope_heading(&scope, &heads));
            out.push_str("\n\n");
            previous = Some(scope);
        }
        entry_section(out, snapshot, stored, "###");
        let mut below = children
            .get(&Some(stored.entry.id.clone()))
            .cloned()
            .unwrap_or_default();
        below.reverse();
        stack.extend(below);
    }
}
