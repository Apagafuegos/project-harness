use std::time::{Duration, Instant};

use crate::tools::{AuthorizedOperation, CancelToken, ExecutionOutcome, Executor};
use rustix::{
    fd::{AsFd, OwnedFd},
    fs::{self, AtFlags, Mode, OFlags, RenameFlags, Stat},
    io,
};
use serde::Deserialize;

use super::{
    FsWorkspace,
    resolve::{
        Control, ObjectId, Resolved, Step, errno, hash, open_relative, read_bounded, read_range,
        stale, verify_unchanged,
    },
};
use crate::{
    session::{
        ContentBlock, Digest, ExecutionBackend, FileEffect, FileImage, FileImageTarget,
        FilePathEffect, ResultCode, ToolStatus,
    },
    tools::{
        Plan, Refusal, WorkspacePath,
        builtin::{MIB, Operation},
        plan::refuse_protection,
    },
};

const OUTPUT_LIMIT: usize = 32 * 1024;
// Enough for the byte-range header even for u64 offsets. The aggregate text,
// including metadata, is bounded, not just the file payload.
const READ_BUDGET: usize = OUTPUT_LIMIT - 256;

// Extract binding metadata only from the opaque Plan's canonical bytes. This
// avoids changing the pure planning API or allowing model JSON to construct a
// Plan. Operations themselves are taken from the typed, private accessor.
#[derive(Deserialize)]
pub(super) struct Binding {
    pub(super) workspace: String,
    pub(super) timeout_ms: u64,
    pub(super) target: Option<Target>,
    pub(super) targets: Option<Vec<Target>>,
}
#[derive(Clone, Deserialize)]
pub(super) struct Target {
    pub(super) path: String,
    pub(super) pre_image: String,
    pub(super) content_sha256: Option<crate::session::Digest>,
}

struct PreparedEdit {
    path: WorkspacePath,
    target_name: String,
    target: Target,
    parent: OwnedFd,
    parent_id: ObjectId,
    object_id: ObjectId,
    mode: Mode,
    bytes: Vec<u8>,
    digest: Digest,
    temporary: Option<MultiTemporary>,
}

struct MultiTemporary {
    parent: OwnedFd,
    name: String,
    armed: bool,
}

impl Drop for MultiTemporary {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::unlinkat(&self.parent, self.name.as_str(), AtFlags::empty());
        }
    }
}

impl Executor for FsWorkspace {
    fn backend(&self) -> ExecutionBackend {
        ExecutionBackend::TrustedFs
    }

    fn execute(&self, operation: &AuthorizedOperation, cancel: &CancelToken) -> ExecutionOutcome {
        self.execute_with_image(operation.plan(), cancel, operation.image())
    }
}

impl FsWorkspace {
    /// Execute an already authorized read/write/edit plan. Recheck filesystem
    /// binding; honour cancellation/deadline until, but never after, the rename.
    /// This low-level API does not grant authorization or execute bash plans.
    pub fn execute(&self, plan: &Plan, cancellation: &CancelToken) -> ExecutionOutcome {
        self.execute_with_image(plan, cancellation, None)
    }

    /// Execute with the image the durable intent captured, so the executor can
    /// prove that the bytes it is about to write are the ones recorded.
    pub fn execute_with_image(
        &self,
        plan: &Plan,
        cancellation: &CancelToken,
        image: Option<&FileImage>,
    ) -> ExecutionOutcome {
        self.execute_hooked(plan, cancellation, &mut |_| Ok(()), None, image)
    }

    pub(super) fn execute_hooked(
        &self,
        plan: &Plan,
        cancellation: &CancelToken,
        hook: &mut dyn FnMut(Step) -> Result<(), Refusal>,
        timeout: Option<Duration>,
        image: Option<&FileImage>,
    ) -> ExecutionOutcome {
        let started = Instant::now();
        if plan.operation().search() {
            return self.execute_search(plan, cancellation, hook, timeout, started);
        }
        // A mutation's effect is classified by its rename commit point, never
        // inferred from the status this function ends up returning.
        let mut effect = plan
            .operation()
            .mutation()
            .then_some(FileEffect::NotApplied);
        let mut rename_attempted = false;
        let mut file_paths = Vec::new();
        let mut commit = || {
            if let Some(effect) = effect.as_mut() {
                *effect = FileEffect::Applied;
            }
        };
        let result = (|| {
            if plan.backend() != ExecutionBackend::TrustedFs {
                return Err(Refusal::new(
                    ResultCode::InfrastructureFailure,
                    "unsupported file executor backend",
                ));
            }
            let binding: Binding = serde_json::from_slice(plan.bytes()).map_err(|_| {
                Refusal::new(
                    ResultCode::InfrastructureFailure,
                    "invalid internal plan binding",
                )
            })?;
            let operation = plan.operation();
            if binding.workspace != self.identity.0 {
                return Err(Refusal::new(
                    ResultCode::InfrastructureFailure,
                    "plan workspace mismatch",
                ));
            }
            let mut control = Control {
                started,
                timeout: Some(timeout.unwrap_or(Duration::from_millis(binding.timeout_ms))),
                cancellation: Some(cancellation),
                hook,
            };
            if let Operation::MultiEdit { files } = operation {
                self.execute_multi_edit(
                    files,
                    binding.targets.as_deref().ok_or_else(|| {
                        Refusal::new(
                            ResultCode::InfrastructureFailure,
                            "multi_edit targets missing from plan",
                        )
                    })?,
                    image,
                    &mut control,
                    &mut file_paths,
                )
            } else {
                let target = binding.target.as_ref().ok_or_else(|| {
                    Refusal::new(
                        ResultCode::InfrastructureFailure,
                        "unsupported file executor plan",
                    )
                })?;
                if operation.path().map(WorkspacePath::as_str) != Some(target.path.as_str()) {
                    return Err(Refusal::new(
                        ResultCode::InfrastructureFailure,
                        "plan target mismatch",
                    ));
                }
                match operation {
                Operation::Read {
                    path,
                    offset_bytes,
                    max_bytes,
                } => self.execute_read(path, *offset_bytes, *max_bytes, target, &mut control),
                Operation::Write { path, text, .. } => self.replace(
                    path,
                    target,
                    Replacement::Write(text),
                    &mut control,
                    image,
                    &mut commit,
                    &mut rename_attempted,
                ),
                Operation::Edit {
                    path,
                    old_text,
                    new_text,
                } => self.replace(
                    path,
                    target,
                    Replacement::Edit(old_text, new_text),
                    &mut control,
                    image,
                    &mut commit,
                    &mut rename_attempted,
                ),
                Operation::Bash { .. }
                | Operation::MultiEdit { .. }
                | Operation::List { .. }
                | Operation::Glob { .. }
                | Operation::Grep { .. } => Err(Refusal::new(
                    ResultCode::InfrastructureFailure,
                    "unsupported file executor plan",
                )),
                }
            }
        })();
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let (status, code, mut text, output_truncated) = match result {
            Ok((text, truncated)) => (ToolStatus::Succeeded, None, text, truncated),
            Err(error) => {
                // Decision 22: never inherit Refusal::status at execution time.
                let status = match error.code {
                    ResultCode::Cancelled => ToolStatus::Cancelled,
                    ResultCode::Timeout => ToolStatus::TimedOut,
                    ResultCode::InfrastructureFailure => ToolStatus::InfrastructureFailure,
                    _ => ToolStatus::Failed,
                };
                // Only an attempted rename can have an ambiguous effect.
                // NOREPLACE EEXIST resets the marker: it proves no replacement.
                if effect == Some(FileEffect::NotApplied) && rename_attempted {
                    effect = Some(FileEffect::Unknown);
                }
                (status, Some(error.code), error.text(), false)
            }
        };
        if matches!(plan.operation(), Operation::MultiEdit { .. }) {
            effect = Some(if file_paths
                .iter()
                .any(|file: &FilePathEffect| file.effect == FileEffect::Unknown)
            {
                FileEffect::Unknown
            } else if file_paths
                .iter()
                .any(|file| file.effect == FileEffect::Applied)
            {
                FileEffect::Applied
            } else {
                FileEffect::NotApplied
            });
            if code.is_some() {
                let outcomes = file_paths
                    .iter()
                    .map(|file| format!("{}={:?}", file.path, file.effect).to_lowercase())
                    .collect::<Vec<_>>()
                    .join(", ");
                text.push_str(&format!("\nper-file effects: {outcomes}"));
            }
        }
        ExecutionOutcome {
            status,
            code,
            content: vec![ContentBlock::Text { text }],
            exit_code: None,
            duration_ms: Some(duration_ms),
            output_truncated,
            artifacts: Vec::new(),
            file_effect: effect,
            file_paths,
        }
    }

    fn execute_multi_edit(
        &self,
        files: &[super::super::builtin::MultiEditFile],
        targets: &[Target],
        image: Option<&FileImage>,
        control: &mut Control<'_>,
        outcomes: &mut Vec<FilePathEffect>,
    ) -> Result<(String, bool), Refusal> {
        if files.is_empty() || files.len() != targets.len() {
            return Err(Refusal::new(
                ResultCode::InfrastructureFailure,
                "multi_edit target count mismatch",
            ));
        }
        let image_targets: Vec<_> = image.map(FileImage::targets).into_iter().flatten().collect();
        if image.is_some() && image_targets.len() != files.len() {
            return Err(Refusal::new(
                ResultCode::InfrastructureFailure,
                "multi_edit image count mismatch",
            ));
        }
        let mut prepared = Vec::with_capacity(files.len());
        for (index, (file, target)) in files.iter().zip(targets).enumerate() {
            control.check(Step::Resolve)?;
            if target.path != file.path.as_str() {
                return Err(Refusal::new(
                    ResultCode::InfrastructureFailure,
                    "multi_edit target order mismatch",
                ));
            }
            let resolved = self.resolve(&file.path, control)?;
            refuse_protection(resolved.protection, true)?;
            if !resolved.complete {
                return Err(Refusal::new(ResultCode::NotFound, "parent does not exist"));
            }
            let parent = open_relative(
                self.root.as_fd(),
                file.path.parent().as_str(),
                OFlags::RDONLY | OFlags::DIRECTORY,
                Mode::empty(),
                control,
                Step::Resolve,
            )?;
            let parent_id = ObjectId::of(&fs::fstat(&parent).map_err(errno)?);
            if parent_id != ObjectId::of(&fs::fstat(&resolved.parent).map_err(errno)?) {
                return Err(stale());
            }
            let (source, stat) = self
                .pre_image(&resolved, target, control)?
                .ok_or_else(stale)?;
            if stat.st_size as u64 > MIB {
                return Err(Refusal::new(ResultCode::TooLarge, "file exceeds 1 MiB"));
            }
            let bytes = read_bounded(&source, &stat, MIB, control)?;
            let mut text = std::str::from_utf8(&bytes)
                .map_err(|_| Refusal::new(ResultCode::NotUtf8, "file is not UTF-8"))?
                .to_owned();
            for edit in &file.edits {
                text = edited_bytes(&text, &edit.old_text, &edit.new_text)?;
            }
            let digest = Digest::of(text.as_bytes());
            if let Some(captured) = image_targets.get(index)
                && (captured.path != file.path.as_str()
                    || captured.post_image != &digest
                    || captured.pre_image.as_ref().map(|pre| pre.sha256.as_str())
                        != Some(target.pre_image.as_str())
                    || captured.pre_mode != Some(stat.st_mode & 0o777))
            {
                return Err(stale());
            }
            prepared.push(PreparedEdit {
                path: file.path.clone(),
                target_name: resolved.name,
                target: target.clone(),
                parent,
                parent_id,
                object_id: ObjectId::of(&stat),
                mode: Mode::from_raw_mode(stat.st_mode & 0o777),
                bytes: text.into_bytes(),
                digest,
                temporary: None,
            });
        }

        // Stage every replacement before rechecking or committing any target.
        for item in &mut prepared {
            control.check(Step::CreateTemp)?;
            let name = format!(".harness-{}.tmp", uuid::Uuid::now_v7().simple());
            let file = open_relative(
                item.parent.as_fd(),
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                Mode::from_raw_mode(0o600),
                control,
                Step::CreateTemp,
            )?;
            let mut temp = MultiTemporary {
                parent: item.parent.try_clone().map_err(errno)?,
                name,
                armed: true,
            };
            let mut written = 0;
            while written < item.bytes.len() {
                control.check(Step::WriteTemp)?;
                let count = io::write(&file, &item.bytes[written..]).map_err(errno)?;
                if count == 0 {
                    return Err(Refusal::new(
                        ResultCode::InfrastructureFailure,
                        "temporary file write made no progress",
                    ));
                }
                written += count;
            }
            control.check(Step::ChmodTemp)?;
            fs::fchmod(&file, item.mode).map_err(errno)?;
            control.check(Step::SyncTemp)?;
            fs::fsync(&file).map_err(errno)?;
            item.temporary = Some(temp);
        }

        // Recheck the complete set after staging and before the first rename.
        for item in &prepared {
            control.check(Step::Recheck)?;
            let latest = self.resolve(&item.path, control)?;
            refuse_protection(latest.protection, true)?;
            if !latest.complete
                || ObjectId::of(&fs::fstat(&latest.parent).map_err(errno)?) != item.parent_id
            {
                return Err(stale());
            }
            let Some((_, stat)) = self.pre_image(&latest, &item.target, control)? else {
                return Err(stale());
            };
            if ObjectId::of(&stat) != item.object_id {
                return Err(stale());
            }
        }

        outcomes.extend(prepared.iter().map(|item| FilePathEffect {
            path: item.path.as_str().to_owned(),
            effect: FileEffect::NotApplied,
        }));
        let mut descriptions = Vec::with_capacity(prepared.len());
        for (index, item) in prepared.iter_mut().enumerate() {
            control.check(Step::Rename)?;
            let temp = item.temporary.as_mut().expect("every target staged");
            match fs::renameat_with(
                &item.parent,
                temp.name.as_str(),
                &item.parent,
                item.target_name.as_str(),
                RenameFlags::empty(),
            ) {
                Ok(()) => {
                    temp.armed = false;
                    outcomes[index].effect = FileEffect::Applied;
                }
                Err(error) => {
                    outcomes[index].effect = FileEffect::Unknown;
                    return Err(errno(error));
                }
            }
            // A rename committed; a later sync error must retain this fact.
            (control.hook)(Step::SyncParent)?;
            fs::fsync(&item.parent).map_err(errno)?;
            descriptions.push(format!(
                "{} sha256={} bytes={}",
                item.path.as_str(), item.digest, item.bytes.len()
            ));
        }
        Ok((descriptions.join("\n"), false))
    }

    fn execute_read(
        &self,
        path: &WorkspacePath,
        offset: u64,
        max: u64,
        target: &Target,
        control: &mut Control<'_>,
    ) -> Result<(String, bool), Refusal> {
        let resolved = self.resolve(path, control)?;
        let (file, stat) = self.regular(&resolved, false, control)?;
        if hash(&file, &stat, control)?.as_str() != target.pre_image {
            return Err(stale());
        }
        let start = offset.min(stat.st_size as u64);
        let requested_end = start.saturating_add(max).min(stat.st_size as u64);
        let end = start.saturating_add(READ_BUDGET as u64).min(requested_end);
        // Adjacent bytes distinguish a genuine split character from malformed
        // UTF-8 (especially an incomplete codepoint at EOF). They are never
        // included in the returned range.
        let context_start = start.saturating_sub(3);
        let context_end = end.saturating_add(3).min(stat.st_size as u64);
        let bytes = read_range(
            &file,
            context_start,
            (context_end - context_start) as usize,
            control,
        )?;
        verify_unchanged(file.as_fd(), &stat, control)?;
        let (adjusted_start, valid) = inward_utf8(
            &bytes,
            (start - context_start) as usize,
            (end - context_start) as usize,
        )?;
        let consumed_start = context_start + adjusted_start as u64;
        let consumed_end = consumed_start + valid.len() as u64;
        let more = consumed_end < stat.st_size as u64;
        let truncated = end < requested_end;
        Ok((
            format!(
                "bytes {consumed_start}..{consumed_end}; more={more}; output_truncated={truncated}\n{valid}"
            ),
            truncated,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn replace(
        &self,
        path: &WorkspacePath,
        target: &Target,
        replacement: Replacement<'_>,
        control: &mut Control<'_>,
        image: Option<&FileImage>,
        commit: &mut dyn FnMut(),
        rename_attempted: &mut bool,
    ) -> Result<(String, bool), Refusal> {
        let resolved = self.resolve(path, control)?;
        refuse_protection(resolved.protection, true)?;
        if !resolved.complete {
            return Err(Refusal::new(ResultCode::NotFound, "parent does not exist"));
        }
        let parent = open_relative(
            self.root.as_fd(),
            path.parent().as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
            control,
            Step::Resolve,
        )?;
        control.check(Step::Stat)?;
        let parent_id = ObjectId::of(&fs::fstat(&parent).map_err(errno)?);
        control.check(Step::Stat)?;
        if parent_id != ObjectId::of(&fs::fstat(&resolved.parent).map_err(errno)?) {
            return Err(stale());
        }
        let current = self.pre_image(&resolved, target, control)?;
        let (bytes, mode) = match (&replacement, &current) {
            (Replacement::Write(text), current) => (
                text.as_bytes().to_vec(),
                current
                    .as_ref()
                    .map_or(Mode::from_raw_mode(0o644), |(_, stat)| {
                        Mode::from_raw_mode(stat.st_mode & 0o777)
                    }),
            ),
            (Replacement::Edit(old, new), Some((file, stat))) => {
                let bytes = read_bounded(file, stat, MIB, control)?;
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| Refusal::new(ResultCode::NotUtf8, "file is not UTF-8"))?;
                (
                    edited_bytes(text, old, new)?.into_bytes(),
                    Mode::from_raw_mode(stat.st_mode & 0o777),
                )
            }
            _ => return Err(stale()),
        };
        let digest = Digest::of(&bytes);
        // The captured image is the only proof of what this execution may do:
        // the computed replacement must be exactly the recorded post-image,
        // over exactly the recorded pre-image and mode.
        if let Some(image) = image
            && (image.path != path.as_str()
                || digest != image.post_image
                || image.pre_image.is_some() != current.is_some()
                || current.as_ref().map(|(_, stat)| stat.st_mode & 0o777) != image.pre_mode)
        {
            return Err(stale());
        }
        let name = format!(".harness-{}.tmp", uuid::Uuid::now_v7().simple());
        let file = open_relative(
            parent.as_fd(),
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            Mode::from_raw_mode(0o600),
            control,
            Step::CreateTemp,
        )?;
        let mut temporary = Temporary {
            parent: &parent,
            name: &name,
            armed: true,
        };
        let result = (|| {
            let mut written = 0;
            while written < bytes.len() {
                control.check(Step::WriteTemp)?;
                let count = io::write(&file, &bytes[written..]).map_err(errno)?;
                if count == 0 {
                    return Err(Refusal::new(
                        ResultCode::InfrastructureFailure,
                        "temporary file write made no progress",
                    ));
                }
                written += count;
            }
            control.check(Step::ChmodTemp)?;
            fs::fchmod(&file, mode).map_err(errno)?;
            control.check(Step::SyncTemp)?;
            fs::fsync(&file).map_err(errno)?;
            control.check(Step::Recheck)?;
            // Re-resolve from the pinned root immediately before commit: catch
            // changed ancestors/protections as well as changed target contents.
            let latest = self.resolve(path, control)?;
            refuse_protection(latest.protection, true)?;
            control.check(Step::Stat)?;
            if !latest.complete
                || ObjectId::of(&fs::fstat(&latest.parent).map_err(errno)?) != parent_id
            {
                return Err(stale());
            }
            let checked = self.pre_image(&latest, target, control)?;
            if current.as_ref().map(|(_, s)| ObjectId::of(s))
                != checked.as_ref().map(|(_, s)| ObjectId::of(s))
            {
                return Err(stale());
            }
            control.check(Step::Rename)?;
            let flags = if target.pre_image == "absent" {
                RenameFlags::NOREPLACE
            } else {
                RenameFlags::empty()
            };
            *rename_attempted = true;
            fs::renameat_with(&parent, &name, &parent, &resolved.name, flags).map_err(|e| {
                if e == io::Errno::EXIST && target.pre_image == "absent" {
                    *rename_attempted = false;
                    stale()
                } else {
                    errno(e)
                }
            })?;
            temporary.armed = false;
            // This is the commit point: from here the effect is applied, even
            // if the directory sync below fails.
            commit();
            // Do not check cancellation or deadline after the commit point.
            // The hook makes post-commit cancellation deterministic in tests.
            (control.hook)(Step::SyncParent)?;
            fs::fsync(&parent).map_err(errno)?;
            Ok((format!("sha256={digest}; bytes={}", bytes.len()), false))
        })();
        if temporary.armed {
            temporary.cleanup()?;
        }
        result
    }

    fn pre_image(
        &self,
        resolved: &Resolved,
        target: &Target,
        control: &mut Control<'_>,
    ) -> Result<Option<(OwnedFd, Stat)>, Refusal> {
        if target.pre_image == "absent" {
            if resolved.object.is_some() {
                return Err(stale());
            }
            return Ok(None);
        }
        if resolved.object.is_none() {
            return Err(stale());
        }
        let (file, stat) = self.regular(resolved, true, control)?;
        if hash(&file, &stat, control)?.as_str() != target.pre_image {
            return Err(stale());
        }
        Ok(Some((file, stat)))
    }
}

/// The exact replacement bytes of one literal edit, under the rules
/// `FsWorkspace::replace` enforces: nonempty-ish literal match by bytes
/// (overlapping occurrences count), exactly one match, UTF-8 input and a
/// bounded result. Capture and execution share this function, so the digest an
/// intent records is the digest the executor writes.
pub(crate) fn edited_bytes(text: &str, old: &str, new: &str) -> Result<String, Refusal> {
    let mut matches = text
        .as_bytes()
        .windows(old.len())
        .enumerate()
        .filter(|(_, window)| *window == old.as_bytes());
    let (offset, _) = matches
        .next()
        .ok_or_else(|| Refusal::new(ResultCode::NoMatch, "old_text has no literal match"))?;
    if matches.next().is_some() {
        return Err(Refusal::new(
            ResultCode::MultipleMatches,
            "old_text has multiple literal matches",
        ));
    }
    if text.len() - old.len() + new.len() > MIB as usize {
        return Err(Refusal::new(
            ResultCode::TooLarge,
            "edited contents exceed 1 MiB",
        ));
    }
    let mut updated = String::with_capacity(text.len() - old.len() + new.len());
    updated.push_str(&text[..offset]);
    updated.push_str(new);
    updated.push_str(&text[offset + old.len()..]);
    Ok(updated)
}

enum Replacement<'a> {
    Write(&'a str),
    Edit(&'a str, &'a str),
}

struct Temporary<'a> {
    parent: &'a OwnedFd,
    name: &'a str,
    armed: bool,
}
impl Temporary<'_> {
    fn cleanup(&mut self) -> Result<(), Refusal> {
        fs::unlinkat(self.parent, self.name, AtFlags::empty()).map_err(errno)?;
        self.armed = false;
        Ok(())
    }
}
impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::unlinkat(self.parent, self.name, AtFlags::empty());
        }
    }
}

fn inward_utf8(bytes: &[u8], mut start: usize, end: usize) -> Result<(usize, &str), Refusal> {
    let invalid = || Refusal::new(ResultCode::NotUtf8, "range is not UTF-8");
    if start == end {
        return Ok((start, ""));
    }
    if bytes[start] & 0xc0 == 0x80 {
        let mut lead = start;
        while lead > 0 && start - lead < 3 && bytes[lead] & 0xc0 == 0x80 {
            lead -= 1;
        }
        let length = character_length(bytes[lead]).ok_or_else(invalid)?;
        let character_end = lead + length;
        let character = bytes.get(lead..character_end).ok_or_else(invalid)?;
        std::str::from_utf8(character).map_err(|_| invalid())?;
        if character_end <= start {
            return Err(invalid());
        }
        start = character_end.min(end);
    }
    match std::str::from_utf8(&bytes[start..end]) {
        Ok(text) => Ok((start, text)),
        Err(error) if error.error_len().is_none() => {
            let tail = start + error.valid_up_to();
            let length = character_length(bytes[tail]).ok_or_else(invalid)?;
            let character = bytes.get(tail..tail + length).ok_or_else(invalid)?;
            std::str::from_utf8(character).map_err(|_| invalid())?;
            let text = std::str::from_utf8(&bytes[start..tail]).map_err(|_| invalid())?;
            Ok((start, text))
        }
        Err(_) => Err(invalid()),
    }
}

fn character_length(lead: u8) -> Option<usize> {
    match lead {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}
