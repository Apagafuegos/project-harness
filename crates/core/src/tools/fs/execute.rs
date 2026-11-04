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
    session::{ContentBlock, Digest, ExecutionBackend, ResultCode, ToolStatus},
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
struct Binding {
    workspace: String,
    timeout_ms: u64,
    target: Option<Target>,
}
#[derive(Deserialize)]
struct Target {
    path: String,
    pre_image: String,
}

impl Executor for FsWorkspace {
    fn backend(&self) -> ExecutionBackend {
        ExecutionBackend::TrustedFs
    }

    fn execute(&self, operation: &AuthorizedOperation, cancel: &CancelToken) -> ExecutionOutcome {
        FsWorkspace::execute(self, operation.plan(), cancel)
    }
}

impl FsWorkspace {
    /// Execute an already authorized read/write/edit plan. Recheck filesystem
    /// binding; honour cancellation/deadline until, but never after, the rename.
    /// This low-level API does not grant authorization or execute bash plans.
    pub fn execute(&self, plan: &Plan, cancellation: &CancelToken) -> ExecutionOutcome {
        self.execute_hooked(plan, cancellation, &mut |_| Ok(()), None)
    }

    pub(super) fn execute_hooked(
        &self,
        plan: &Plan,
        cancellation: &CancelToken,
        hook: &mut dyn FnMut(Step) -> Result<(), Refusal>,
        timeout: Option<Duration>,
    ) -> ExecutionOutcome {
        let started = Instant::now();
        if plan.operation().search() {
            return self.execute_search(plan, cancellation, hook, timeout, started);
        }
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
            let target = binding.target.as_ref().ok_or_else(|| {
                Refusal::new(
                    ResultCode::InfrastructureFailure,
                    "unsupported file executor plan",
                )
            })?;
            if binding.workspace != self.identity.0
                || operation.path().map(WorkspacePath::as_str) != Some(target.path.as_str())
            {
                return Err(Refusal::new(
                    ResultCode::InfrastructureFailure,
                    "plan workspace or target mismatch",
                ));
            }
            let mut control = Control {
                started,
                timeout: Some(timeout.unwrap_or(Duration::from_millis(binding.timeout_ms))),
                cancellation: Some(cancellation),
                hook,
            };
            match operation {
                Operation::Read {
                    path,
                    offset_bytes,
                    max_bytes,
                } => self.execute_read(path, *offset_bytes, *max_bytes, target, &mut control),
                Operation::Write { path, text, .. } => {
                    self.replace(path, target, Replacement::Write(text), &mut control)
                }
                Operation::Edit {
                    path,
                    old_text,
                    new_text,
                } => self.replace(
                    path,
                    target,
                    Replacement::Edit(old_text, new_text),
                    &mut control,
                ),
                Operation::Bash { .. }
                | Operation::List { .. }
                | Operation::Glob { .. }
                | Operation::Grep { .. } => Err(Refusal::new(
                    ResultCode::InfrastructureFailure,
                    "unsupported file executor plan",
                )),
            }
        })();
        let duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let (status, code, text, output_truncated) = match result {
            Ok((text, truncated)) => (ToolStatus::Succeeded, None, text, truncated),
            Err(error) => {
                // Decision 22: never inherit Refusal::status at execution time.
                let status = match error.code {
                    ResultCode::Cancelled => ToolStatus::Cancelled,
                    ResultCode::Timeout => ToolStatus::TimedOut,
                    ResultCode::InfrastructureFailure => ToolStatus::InfrastructureFailure,
                    _ => ToolStatus::Failed,
                };
                (status, Some(error.code), error.text(), false)
            }
        };
        ExecutionOutcome {
            status,
            code,
            content: vec![ContentBlock::Text { text }],
            exit_code: None,
            duration_ms: Some(duration_ms),
            output_truncated,
            artifacts: Vec::new(),
        }
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

    fn replace(
        &self,
        path: &WorkspacePath,
        target: &Target,
        replacement: Replacement<'_>,
        control: &mut Control<'_>,
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
                let mut matches = text
                    .as_bytes()
                    .windows(old.len())
                    .enumerate()
                    .filter(|(_, b)| *b == old.as_bytes());
                let (offset, _) = matches.next().ok_or_else(|| {
                    Refusal::new(ResultCode::NoMatch, "old_text has no literal match")
                })?;
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
                (
                    updated.into_bytes(),
                    Mode::from_raw_mode(stat.st_mode & 0o777),
                )
            }
            _ => return Err(stale()),
        };
        let digest = Digest::of(&bytes);
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
            fs::renameat_with(&parent, &name, &parent, &resolved.name, flags).map_err(|e| {
                if e == io::Errno::EXIST && target.pre_image == "absent" {
                    stale()
                } else {
                    errno(e)
                }
            })?;
            temporary.armed = false;
            // This is the commit point. Do not check cancellation or deadline.
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
