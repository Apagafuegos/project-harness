//! Capture of trusted write/edit pre-images, all-or-nothing restoration and
//! advisory undo hints. No authorization decision is made here.

use std::{
    io::{self as stdio, Read},
    time::{Duration, Instant},
};

use rustix::{
    fd::{AsFd, OwnedFd},
    fs::{self, AtFlags, Mode, OFlags, RenameFlags},
    io,
};

use super::{
    FsWorkspace,
    execute::{Binding, edited_bytes},
    resolve::{Control, ObjectId, Resolved, Step, errno, hash, open_relative, read_bounded, stale},
};
use crate::{
    session::{
        ArtifactRef, DataRoot, Digest, FileImage, FileOutcome, ResultCode, StoreError,
        UndoConflict, UndoFileOutcome, UndoTarget,
    },
    tools::{
        FileKind, Plan, Protection, RecoveryHint, Refusal, WorkspaceIdentity, WorkspacePath,
        builtin::{MIB, Operation},
        exec::UndoExecution,
        plan::refuse_protection,
    },
};

/// Capture and restoration are bounded file work, never an unbounded stall.
const DEADLINE: Duration = Duration::from_secs(300);

fn infrastructure(detail: &str) -> Refusal {
    Refusal::new(ResultCode::InfrastructureFailure, detail)
}

/// A descriptor-rooted pre-image reader: every chunk is read from the verified
/// descriptor, so a large overwritten file is streamed, never buffered whole.
struct DescriptorReader<'f, 'c, 'h> {
    file: &'f OwnedFd,
    offset: u64,
    control: &'c mut Control<'h>,
    failure: Option<Refusal>,
}

impl Read for DescriptorReader<'_, '_, '_> {
    fn read(&mut self, buffer: &mut [u8]) -> stdio::Result<usize> {
        if let Err(refusal) = self.control.check(Step::Read) {
            self.failure = Some(refusal);
            return Err(stdio::Error::other("capture stopped"));
        }
        match io::pread(self.file, buffer, self.offset) {
            Ok(0) => Ok(0),
            Ok(count) => {
                self.offset += count as u64;
                Ok(count)
            }
            Err(error) => {
                let refusal = errno(error);
                self.failure = Some(refusal);
                Err(stdio::Error::other("capture read failed"))
            }
        }
    }
}

/// The captured pre-image facts of one target.
struct PreImage {
    /// Published pre-image bytes, absent when the target was absent.
    image: Option<ArtifactRef>,
    /// Ordinary permission bits, absent with the image.
    mode: Option<u32>,
    /// The exact text, read only for an edit.
    text: Option<String>,
}

/// One armed same-directory temporary, removed unless its rename committed.
struct Temporary {
    parent: OwnedFd,
    name: String,
    armed: bool,
}

impl Temporary {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::unlinkat(&self.parent, self.name.as_str(), AtFlags::empty());
        }
    }
}

impl FsWorkspace {
    /// Capture the pre-image (bytes or absence) and mode of a trusted
    /// write/edit plan, and compute the exact post-image digest. The raw bytes
    /// are published through the artifact store before the caller may commit
    /// the intent that references them.
    pub fn capture_image(&self, plan: &Plan, artifacts: &DataRoot) -> Result<FileImage, Refusal> {
        let started = Instant::now();
        let binding: Binding = serde_json::from_slice(plan.bytes())
            .map_err(|_| infrastructure("invalid internal plan binding"))?;
        let operation = plan.operation();
        if binding.workspace != self.identity.0 {
            return Err(infrastructure("plan workspace mismatch"));
        }
        let target = binding
            .target
            .ok_or_else(|| infrastructure("unsupported capture plan"))?;
        let path = WorkspacePath::new(&target.path)?;
        if operation.path() != Some(&path) {
            return Err(infrastructure("plan target mismatch"));
        }
        let want_text = matches!(operation, Operation::Edit { .. });
        let mut hook = |_: Step| Ok(());
        let mut control = Control {
            started,
            timeout: Some(DEADLINE),
            cancellation: None,
            hook: &mut hook,
        };
        let resolved = self.resolve(&path, &mut control)?;
        refuse_protection(resolved.protection, true)?;
        if !resolved.complete {
            return Err(Refusal::new(ResultCode::NotFound, "parent does not exist"));
        }
        let parent = open_relative(
            self.root.as_fd(),
            path.parent().as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
            &mut control,
            Step::Resolve,
        )?;
        control.check(Step::Stat)?;
        let parent_id = ObjectId::of(&fs::fstat(&parent).map_err(errno)?);
        control.check(Step::Stat)?;
        if parent_id != ObjectId::of(&fs::fstat(&resolved.parent).map_err(errno)?) {
            return Err(stale());
        }
        let pre = self.capture_pre(&resolved, &target, want_text, artifacts, &mut control)?;
        let post_image = match operation {
            Operation::Write { text, .. } => {
                let digest = Digest::of(text.as_bytes());
                if target.content_sha256.as_ref() != Some(&digest) {
                    return Err(infrastructure("plan content digest mismatch"));
                }
                digest
            }
            Operation::Edit {
                old_text, new_text, ..
            } => {
                let text = pre.text.ok_or_else(stale)?;
                Digest::of(edited_bytes(&text, old_text, new_text)?.as_bytes())
            }
            _ => return Err(infrastructure("unsupported capture plan")),
        };
        Ok(FileImage {
            path: path.as_str().to_owned(),
            pre_image: pre.image,
            pre_mode: pre.mode,
            post_image,
        })
    }

    /// The pre-image bytes as a published artifact, its ordinary mode, and — for
    /// an edit — the exact UTF-8 text those bytes hold.
    fn capture_pre(
        &self,
        resolved: &Resolved,
        target: &super::execute::Target,
        want_text: bool,
        artifacts: &DataRoot,
        control: &mut Control<'_>,
    ) -> Result<PreImage, Refusal> {
        if target.pre_image == "absent" {
            if resolved.object.is_some() {
                return Err(stale());
            }
            return Ok(PreImage {
                image: None,
                mode: None,
                text: None,
            });
        }
        let (file, stat) = self.regular(resolved, true, control)?;
        control.check(Step::Stat)?;
        let mode = stat.st_mode & 0o777;
        let reference = {
            let mut reader = DescriptorReader {
                file: &file,
                offset: 0,
                control,
                failure: None,
            };
            let published = artifacts.put_artifact_reader(&mut reader);
            if let Some(refusal) = reader.failure.take() {
                return Err(refusal);
            }
            published.map_err(|error| match error {
                StoreError::Limit => Refusal::new(
                    ResultCode::TooLarge,
                    "pre-image exceeds the artifact size limit",
                ),
                _ => infrastructure("pre-image publication failed"),
            })?
        };
        // Check stat stability after reading and require the published digest
        // to be exactly the plan's pre-image.
        super::resolve::verify_unchanged(file.as_fd(), &stat, control)?;
        if reference.sha256.as_str() != target.pre_image || reference.bytes != stat.st_size as u64 {
            return Err(stale());
        }
        if !want_text {
            return Ok(PreImage {
                image: Some(reference),
                mode: Some(mode),
                text: None,
            });
        }
        if stat.st_size as u64 > MIB {
            return Err(Refusal::new(ResultCode::TooLarge, "file exceeds 1 MiB"));
        }
        let bytes = read_bounded(&file, &stat, MIB, control)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| Refusal::new(ResultCode::NotUtf8, "file is not UTF-8"))?
            .to_owned();
        Ok(PreImage {
            image: Some(reference),
            mode: Some(mode),
            text: Some(text),
        })
    }

    /// Restore every target, or change none. A complete preflight refusal is
    /// reported before any target is touched; a failure after a mutation is
    /// reported as incomplete and never retried.
    pub fn restore_undo(
        &self,
        workspace: &WorkspaceIdentity,
        targets: &[UndoTarget],
        artifacts: &DataRoot,
    ) -> UndoExecution {
        let started = Instant::now();
        if self.identity.0 != workspace.0 {
            return UndoExecution::Refused(
                targets
                    .iter()
                    .map(|target| UndoConflict {
                        path: target.path.clone(),
                        reason: "workspace_mismatch",
                    })
                    .collect(),
            );
        }
        let mut hook = |_: Step| Ok(());
        let mut control = Control {
            started,
            timeout: Some(DEADLINE),
            cancellation: None,
            hook: &mut hook,
        };
        if let Err(conflicts) = self.undo_preflight(targets, artifacts, &mut control) {
            return UndoExecution::Refused(conflicts);
        }
        // Stage the replacement temporaries for present pre-images. Staging is
        // not a target mutation, and its failure still changes nothing.
        let mut staged: Vec<Option<Temporary>> = Vec::new();
        for target in targets {
            let Some(pre_image) = &target.pre_image else {
                staged.push(None);
                continue;
            };
            match self.stage(target, pre_image, artifacts, &mut control) {
                Ok(temporary) => staged.push(Some(temporary)),
                Err(_) => {
                    return UndoExecution::Refused(vec![UndoConflict {
                        path: target.path.clone(),
                        reason: "staging_failed",
                    }]);
                }
            }
        }
        // Recheck the whole target set after staging, before the first mutation.
        if let Err(conflicts) = self.undo_preflight(targets, artifacts, &mut control) {
            return UndoExecution::Refused(conflicts);
        }
        let mut files: Vec<UndoFileOutcome> = Vec::new();
        for (index, target) in targets.iter().enumerate() {
            let path = target.path.clone();
            match self.apply(target, staged[index].as_mut(), artifacts, &mut control) {
                Ok(outcome) => files.push(UndoFileOutcome { path, outcome }),
                Err(_) => {
                    // This target may have changed; nothing after it is tried.
                    files.push(UndoFileOutcome {
                        path,
                        outcome: FileOutcome::Unknown,
                    });
                    files.extend(targets[index + 1..].iter().map(|target| UndoFileOutcome {
                        path: target.path.clone(),
                        outcome: FileOutcome::Unknown,
                    }));
                    return UndoExecution::Done {
                        files,
                        complete: false,
                    };
                }
            }
        }
        UndoExecution::Done {
            files,
            complete: true,
        }
    }

    /// Every target must still match its recorded post-image, under the current
    /// protection, descriptor, parent, type and link rules.
    fn undo_preflight(
        &self,
        targets: &[UndoTarget],
        artifacts: &DataRoot,
        control: &mut Control<'_>,
    ) -> Result<(), Vec<UndoConflict>> {
        let mut conflicts = Vec::new();
        for target in targets {
            if let Err(reason) = self.undo_check(target, artifacts, control) {
                conflicts.push(UndoConflict {
                    path: target.path.clone(),
                    reason,
                });
            }
        }
        if conflicts.is_empty() {
            Ok(())
        } else {
            Err(conflicts)
        }
    }

    fn undo_check(
        &self,
        target: &UndoTarget,
        artifacts: &DataRoot,
        control: &mut Control<'_>,
    ) -> Result<(), &'static str> {
        let path = WorkspacePath::new(&target.path).map_err(|_| "invalid_path")?;
        let resolved = self.resolve(&path, control).map_err(|_| "unresolved")?;
        if resolved.protection != Protection::None {
            return Err("protected_path");
        }
        let Some(object) = &resolved.object else {
            // A file undo would remove is missing; so is a pre-image target.
            return Err("missing");
        };
        control.check(Step::Stat).map_err(|_| "unresolved")?;
        let stat = fs::fstat(object).map_err(|_| "unresolved")?;
        if super::resolve::kind(&stat) != FileKind::Regular {
            return Err("not_regular_file");
        }
        if stat.st_nlink > 1 {
            return Err("hard_link");
        }
        let parent = open_relative(
            self.root.as_fd(),
            path.parent().as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
            control,
            Step::Resolve,
        )
        .map_err(|_| "parent_changed")?;
        control.check(Step::Stat).map_err(|_| "unresolved")?;
        if ObjectId::of(&fs::fstat(&parent).map_err(|_| "unresolved")?)
            != ObjectId::of(&fs::fstat(&resolved.parent).map_err(|_| "unresolved")?)
        {
            return Err("parent_changed");
        }
        let (file, stat) =
            self.regular(&resolved, true, control)
                .map_err(|refusal| match refusal.code {
                    ResultCode::ProtectedPath => "protected_path",
                    ResultCode::HardLinkRejected => "hard_link",
                    ResultCode::NotRegularFile => "not_regular_file",
                    ResultCode::NotFound => "missing",
                    _ => "unreadable",
                })?;
        if hash(&file, &stat, control)
            .map_err(|_| "unreadable")?
            .as_str()
            != target.post_image.as_str()
        {
            return Err("changed");
        }
        if let Some(pre) = &target.pre_image {
            artifacts
                .verify_artifact(pre)
                .map_err(|_| "pre_image_invalid")?;
            if target.pre_mode.is_none() {
                return Err("pre_image_invalid");
            }
        } else if target.pre_mode.is_some() {
            return Err("pre_image_invalid");
        }
        Ok(())
    }

    /// Stream one pre-image artifact into a same-directory temporary with the
    /// recorded mode, synced before any rename.
    fn stage(
        &self,
        target: &UndoTarget,
        pre_image: &ArtifactRef,
        artifacts: &DataRoot,
        control: &mut Control<'_>,
    ) -> Result<Temporary, Refusal> {
        let path = WorkspacePath::new(&target.path)?;
        let parent = open_relative(
            self.root.as_fd(),
            path.parent().as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
            control,
            Step::Resolve,
        )?;
        let name = format!(".harness-undo-{}.tmp", uuid::Uuid::now_v7().simple());
        let file: OwnedFd = open_relative(
            parent.as_fd(),
            name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            Mode::from_raw_mode(0o600),
            control,
            Step::CreateTemp,
        )?;
        let mut temporary = Temporary {
            parent,
            name,
            armed: true,
        };
        let mut source = artifacts
            .artifact_file(&pre_image.sha256)
            .map_err(|_| infrastructure("pre-image unreadable"))?;
        let mut buffer = [0u8; 64 * 1024];
        let mut written = 0u64;
        loop {
            control.check(Step::WriteTemp)?;
            let read = source
                .read(&mut buffer)
                .map_err(|_| infrastructure("pre-image read failed"))?;
            if read == 0 {
                break;
            }
            written += read as u64;
            let mut offset = 0;
            while offset < read {
                control.check(Step::WriteTemp)?;
                let count = io::write(&file, &buffer[offset..read]).map_err(errno)?;
                if count == 0 {
                    return Err(infrastructure("temporary file write made no progress"));
                }
                offset += count;
            }
        }
        if written != pre_image.bytes {
            return Err(infrastructure("pre-image length mismatch"));
        }
        control.check(Step::ChmodTemp)?;
        fs::fchmod(
            &file,
            Mode::from_raw_mode(target.pre_mode.unwrap_or(0o644) & 0o777),
        )
        .map_err(errno)?;
        control.check(Step::SyncTemp)?;
        fs::fsync(&file).map_err(errno)?;
        temporary.armed = true;
        Ok(temporary)
    }

    /// One target's mutation, after a fresh re-resolution from the pinned root.
    fn apply(
        &self,
        target: &UndoTarget,
        staged: Option<&mut Temporary>,
        artifacts: &DataRoot,
        control: &mut Control<'_>,
    ) -> Result<FileOutcome, ()> {
        self.undo_check(target, artifacts, control)
            .map_err(|_| ())?;
        let path = WorkspacePath::new(&target.path).map_err(|_| ())?;
        let resolved = self.resolve(&path, control).map_err(|_| ())?;
        let parent = open_relative(
            self.root.as_fd(),
            path.parent().as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
            control,
            Step::Resolve,
        )
        .map_err(|_| ())?;
        control.check(Step::Stat).map_err(|_| ())?;
        if ObjectId::of(&fs::fstat(&parent).map_err(|_| ())?)
            != ObjectId::of(&fs::fstat(&resolved.parent).map_err(|_| ())?)
        {
            return Err(());
        }
        match staged {
            Some(temporary) => {
                control.check(Step::Rename).map_err(|_| ())?;
                fs::renameat_with(
                    &parent,
                    temporary.name.as_str(),
                    &parent,
                    resolved.name.as_str(),
                    RenameFlags::empty(),
                )
                .map_err(|_| ())?;
                temporary.disarm();
                fs::fsync(&parent).map_err(|_| ())?;
                Ok(FileOutcome::Restored)
            }
            None => {
                fs::unlinkat(&parent, resolved.name.as_str(), AtFlags::empty()).map_err(|_| ())?;
                fs::fsync(&parent).map_err(|_| ())?;
                Ok(FileOutcome::Removed)
            }
        }
    }

    /// Ephemeral evidence about one undo target. Never persisted.
    pub fn undo_hint(&self, target: &UndoTarget) -> RecoveryHint {
        let Ok(path) = WorkspacePath::new(&target.path) else {
            return RecoveryHint::unavailable("invalid_path");
        };
        let mut hook = |_: Step| Ok(());
        let mut control = Control::planning(&mut hook);
        let current = match self.inspect_controlled(&path, &mut control) {
            Ok(facts) => facts,
            Err(refusal) => return RecoveryHint::unavailable(refusal.code.as_str()),
        };
        if current.protection != Protection::None {
            return RecoveryHint::unavailable("protected_path");
        }
        match current.kind {
            FileKind::Absent => {
                return match &target.pre_image {
                    // Absence is exactly what an absent pre-image restores.
                    None => RecoveryHint::MatchesPreImage,
                    Some(_) => RecoveryHint::Missing,
                };
            }
            FileKind::Regular => {}
            _ => return RecoveryHint::unavailable("not_regular_file"),
        }
        // Equal images prefer the post-image, as the write recovery hint does:
        // equality proves neither execution nor attribution.
        if current.digest.as_ref() == Some(&target.post_image) {
            return RecoveryHint::MatchesPostImage;
        }
        if let Some(pre) = &target.pre_image
            && current.digest.as_ref() == Some(&pre.sha256)
        {
            return RecoveryHint::MatchesPreImage;
        }
        RecoveryHint::ChangedSince
    }
}
