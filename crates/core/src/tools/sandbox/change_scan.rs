//! Bounded, descriptor-rooted workspace change detection for bash feedback.
//!
//! Snapshots retain path keys and content digests only in memory. Public results
//! intentionally contain no paths, contents, or filesystem diagnostics.

use std::{
    collections::BTreeMap,
    ffi::CString,
    io::{self, Read},
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use rustix::{
    fd::AsFd,
    fs::{self, AtFlags, FileType, Mode, OFlags, ResolveFlags},
};
use sha2::{Digest as _, Sha256};
use serde::Deserialize;

use crate::{
    session::ContentBlock,
    tools::{CancelToken, ExecutionOutcome},
};

const MAX_ENTRIES: usize = 16_384;
const MAX_DEPTH: usize = 64;
const MAX_CONTENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SCAN_TIME: Duration = Duration::from_secs(3);
const READLINK_LIMIT: usize = 4096;
const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS)
    .union(ResolveFlags::NO_XDEV);
const PROTECTED_NAMES: [&[u8]; 3] = [
    b"credentials",
    b"credentials.toml",
    b".git-credentials",
];

#[derive(Deserialize)]
struct PlanMounts {
    mounts: Vec<PlanMount>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanMount {
    path: String,
    mode: String,
}

/// Before/after scans share one root descriptor and the exact plan exclusions.
pub(super) struct ScanSession {
    root_path: PathBuf,
    root_fd: OwnedFd,
    root_identity: (u64, u64),
    excluded: Vec<Vec<u8>>,
    before: Result<Snapshot, Unavailable>,
}

/// A complete bounded scan. Its fields are private to prevent path disclosure.
pub(super) struct Snapshot {
    entries: BTreeMap<Vec<u8>, [u8; 32]>,
}

/// Detection was incomplete. Callers must not interpret this as no changes.
#[derive(Debug)]
pub(super) struct Unavailable;

/// Comparison result with no path or content details.
pub(super) enum Feedback {
    Changed(usize),
    Unchanged,
    Unavailable,
}

impl Snapshot {
    fn scan(
        root: &OwnedFd,
        excluded: &[Vec<u8>],
        cancel: &CancelToken,
        limits: Limits,
    ) -> Result<Self, Unavailable> {
        let mut state = ScanState {
            entries: BTreeMap::new(),
            visited: 0,
            content_bytes: 0,
            started: Instant::now(),
            cancel,
            limits,
        };
        state.walk(root, &[], 0, excluded)?;
        Ok(Self {
            entries: state.entries,
        })
    }

    /// Count changed, added, and removed entries without returning their names.
    pub(super) fn compare(&self, after: &Self) -> Feedback {
        let changed = self
            .entries
            .iter()
            .filter(|(path, digest)| after.entries.get(*path) != Some(*digest))
            .count()
            + after
                .entries
                .keys()
                .filter(|path| !self.entries.contains_key(*path))
                .count();
        if changed == 0 {
            Feedback::Unchanged
        } else {
            Feedback::Changed(changed)
        }
    }
}

impl ScanSession {
    /// Pin one workspace capability and scan against the approved plan mounts.
    pub(super) fn begin(
        root_path: &Path,
        plan_bytes: &[u8],
        cancel: &CancelToken,
    ) -> Result<Self, Unavailable> {
        let excluded = plan_exclusions(plan_bytes)?;
        let root_fd = fs::open(
            root_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|_| Unavailable)?;
        let stat = fs::fstat(&root_fd).map_err(|_| Unavailable)?;
        let root_identity = (stat.st_dev, stat.st_ino);
        let before = Snapshot::scan(&root_fd, &excluded, cancel, Limits::default());
        Ok(Self {
            root_path: root_path.to_owned(),
            root_fd,
            root_identity,
            excluded,
            before,
        })
    }

    /// Scan the pinned root again; a path rename/replacement invalidates detection.
    pub(super) fn finish(self, cancel: &CancelToken) -> Feedback {
        if !path_still_names_root(&self.root_path, &self.root_fd, self.root_identity) {
            return Feedback::Unavailable;
        }
        let after = Snapshot::scan(
            &self.root_fd,
            &self.excluded,
            cancel,
            Limits::default(),
        );
        feedback(self.before, after)
    }
}

fn plan_exclusions(plan_bytes: &[u8]) -> Result<Vec<Vec<u8>>, Unavailable> {
    let plan: PlanMounts = serde_json::from_slice(plan_bytes).map_err(|_| Unavailable)?;
    let mut root_mount = false;
    let mut excluded = Vec::new();
    for mount in plan.mounts {
        if mount.path == "." {
            if root_mount || !matches!(mount.mode.as_str(), "rw" | "ro") {
                return Err(Unavailable);
            }
            root_mount = true;
            continue;
        }
        if !matches!(mount.mode.as_str(), "ro" | "masked") {
            return Err(Unavailable);
        }
        let components: Vec<_> = mount.path.split('/').collect();
        if mount.path.is_empty()
            || mount.path.starts_with('/')
            || components
                .iter()
                .any(|part| part.is_empty() || matches!(*part, "." | ".."))
        {
            return Err(Unavailable);
        }
        excluded.push(mount.path.into_bytes());
    }
    if !root_mount {
        return Err(Unavailable);
    }
    Ok(excluded)
}

fn path_still_names_root(path: &Path, root: &OwnedFd, identity: (u64, u64)) -> bool {
    let Ok(current) = fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) else {
        return false;
    };
    let (Ok(pinned), Ok(named)) = (fs::fstat(root), fs::fstat(&current)) else {
        return false;
    };
    (pinned.st_dev, pinned.st_ino) == identity
        && (named.st_dev, named.st_ino) == identity
}

/// Convert two bounded scans into an honest result; either incomplete scan wins.
pub(super) fn feedback(
    before: Result<Snapshot, Unavailable>,
    after: Result<Snapshot, Unavailable>,
) -> Feedback {
    match (before, after) {
        (Ok(before), Ok(after)) => before.compare(&after),
        _ => Feedback::Unavailable,
    }
}

/// Add only bounded, non-sensitive guidance without changing execution status.
pub(super) fn add_result_feedback(
    outcome: &mut ExecutionOutcome,
    feedback: Feedback,
    text_limit: usize,
) {
    let note = match feedback {
        Feedback::Changed(count) => format!(
            "{count} workspace changes; bash changes are outside undo. Use edit/multi-edit for reversible edits."
        ),
        Feedback::Unchanged => return,
        Feedback::Unavailable => {
            "Detection unavailable; bash changes may be outside undo. Prefer edit/multi-edit for reversibility."
                .to_owned()
        }
    };
    let existing = outcome
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let body_limit = text_limit.saturating_sub(note.len() + 1);
    let (body, body_truncated) = super::output::content(existing.as_bytes(), body_limit);
    let existing = body
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let combined = if existing.is_empty() {
        note
    } else {
        format!("{note} {existing}")
    };
    let (content, truncated) = super::output::content(combined.as_bytes(), text_limit);
    outcome.content = content;
    outcome.output_truncated |= body_truncated || truncated;
}

#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    depth: usize,
    content_bytes: u64,
    scan_time: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: MAX_ENTRIES,
            depth: MAX_DEPTH,
            content_bytes: MAX_CONTENT_BYTES,
            scan_time: MAX_SCAN_TIME,
        }
    }
}

struct ScanState {
    entries: BTreeMap<Vec<u8>, [u8; 32]>,
    visited: usize,
    content_bytes: u64,
    started: Instant,
    cancel: &'a CancelToken,
    limits: Limits,
}

impl ScanState {
    fn walk(
        &mut self,
        directory: &OwnedFd,
        prefix: &[u8],
        depth: usize,
    ) -> Result<(), Unavailable> {
        if depth > self.limits.depth {
            return Err(Unavailable);
        }
        let before = fs::fstat(directory).map_err(|_| Unavailable)?;
        let iter = fs::Dir::read_from(directory).map_err(|_| Unavailable)?;
        for item in iter {
            let item = item.map_err(|_| Unavailable)?;
            let name = item.file_name();
            let bytes = name.to_bytes();
            if matches!(bytes, b"." | b"..") || is_protected(bytes) {
                continue;
            }
            self.visited = self.visited.checked_add(1).ok_or(Unavailable)?;
            if self.visited > self.limits.entries {
                return Err(Unavailable);
            }
            let name = CString::new(bytes).map_err(|_| Unavailable)?;
            let path = joined(prefix, bytes);
            let stat =
                fs::statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW).map_err(|_| Unavailable)?;
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => {
                    self.entries.insert(path.clone(), directory_digest());
                    let child = fs::openat2(
                        directory,
                        &name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                        Mode::empty(),
                        RESOLVE,
                    )
                    .map_err(|_| Unavailable)?;
                    let opened = fs::fstat(&child).map_err(|_| Unavailable)?;
                    if !same_object(&stat, &opened) {
                        return Err(Unavailable);
                    }
                    self.walk(&child, &path, depth + 1)?;
                }
                FileType::RegularFile => {
                    let file = fs::openat2(
                        directory,
                        &name,
                        OFlags::RDONLY | OFlags::CLOEXEC,
                        Mode::empty(),
                        RESOLVE,
                    )
                    .map_err(|_| Unavailable)?;
                    let opened = fs::fstat(&file).map_err(|_| Unavailable)?;
                    if !same_object(&stat, &opened)
                        || FileType::from_raw_mode(opened.st_mode) != FileType::RegularFile
                    {
                        return Err(Unavailable);
                    }
                    let digest = self.hash_file(file, opened)?;
                    self.entries.insert(path, digest);
                }
                FileType::Symlink => {
                    let mut target = [0u8; READLINK_LIMIT];
                    let target_len = fs::readlinkat_raw(directory, &name, &mut target[..])
                        .map_err(|_| Unavailable)?;
                    if target_len == READLINK_LIMIT {
                        return Err(Unavailable);
                    }
                    let digest = Sha256::digest(&target[..target_len]).into();
                    self.entries.insert(path, digest);
                }
                _ => {
                    // Special files are never opened. Their kind is enough to
                    // detect replacement without risking blocking device/FIFO reads.
                    let mut hash = Sha256::new();
                    hash.update(stat.st_mode.to_ne_bytes());
                    hash.update(stat.st_size.to_ne_bytes());
                    self.entries.insert(path, hash.finalize().into());
                }
            }
        }
        let after = fs::fstat(directory).map_err(|_| Unavailable)?;
        if !stable_directory(&before, &after) {
            return Err(Unavailable);
        }
        Ok(())
    }

    fn hash_file(
        &mut self,
        fd: OwnedFd,
        before: rustix::fs::Stat,
    ) -> Result<[u8; 32], Unavailable> {
        let size: u64 = before.st_size.try_into().map_err(|_| Unavailable)?;
        let remaining = self
            .limits
            .content_bytes
            .checked_sub(self.content_bytes)
            .ok_or(Unavailable)?;
        if size > remaining {
            return Err(Unavailable);
        }
        let mut file = std::fs::File::from(fd);
        let mut hash = Sha256::new();
        let mut file_bytes = 0u64;
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let remaining_capacity = remaining.checked_sub(file_bytes).ok_or(Unavailable)?;
            let read_limit = if remaining_capacity < buffer.len() as u64 {
                remaining_capacity as usize + 1
            } else {
                buffer.len()
            };
            let count = match file.read(&mut buffer[..read_limit]) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(Unavailable),
            };
            file_bytes = file_bytes.checked_add(count as u64).ok_or(Unavailable)?;
            if file_bytes > remaining {
                return Err(Unavailable);
            }
            hash.update(&buffer[..count]);
        }
        let after = rustix::fs::fstat(&file).map_err(|_| Unavailable)?;
        if !stable_file(&before, &after) || file_bytes != size {
            return Err(Unavailable);
        }
        self.content_bytes = self
            .content_bytes
            .checked_add(file_bytes)
            .ok_or(Unavailable)?;
        Ok(hash.finalize().into())
    }
}

fn is_protected(name: &[u8]) -> bool {
    PROTECTED_NAMES.contains(&name)
}

fn directory_digest() -> [u8; 32] {
    Sha256::digest(b"directory").into()
}

fn joined(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(prefix.len() + name.len() + usize::from(!prefix.is_empty()));
    if !prefix.is_empty() {
        path.extend_from_slice(prefix);
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

fn same_object(left: &rustix::fs::Stat, right: &rustix::fs::Stat) -> bool {
    left.st_dev == right.st_dev
        && left.st_ino == right.st_ino
        && FileType::from_raw_mode(left.st_mode) == FileType::from_raw_mode(right.st_mode)
}

fn stable_file(left: &rustix::fs::Stat, right: &rustix::fs::Stat) -> bool {
    same_object(left, right)
        && left.st_size == right.st_size
        && left.st_mtime == right.st_mtime
        && left.st_mtime_nsec == right.st_mtime_nsec
        && left.st_ctime == right.st_ctime
        && left.st_ctime_nsec == right.st_ctime_nsec
}

fn stable_directory(left: &rustix::fs::Stat, right: &rustix::fs::Stat) -> bool {
    same_object(left, right)
        && left.st_mtime == right.st_mtime
        && left.st_mtime_nsec == right.st_mtime_nsec
        && left.st_ctime == right.st_ctime
        && left.st_ctime_nsec == right.st_ctime_nsec
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    fn snapshot(root: &Path) -> Snapshot {
        Snapshot::capture(root).expect("complete test scan")
    }

    #[test]
    fn detects_content_change_even_when_file_size_is_unchanged() {
        let root = tempdir().unwrap();
        let file = root.path().join("work.txt");
        std::fs::write(&file, b"old").unwrap();
        let before = snapshot(root.path());
        std::fs::write(&file, b"new").unwrap();
        let after = snapshot(root.path());
        assert!(matches!(before.compare(&after), Feedback::Changed(1)));
    }

    #[test]
    fn detects_added_and_deleted_files_without_disclosing_names() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("old-private-name"), b"old").unwrap();
        let before = snapshot(root.path());
        std::fs::remove_file(root.path().join("old-private-name")).unwrap();
        std::fs::write(root.path().join("new-private-name"), b"new").unwrap();
        let after = snapshot(root.path());
        match before.compare(&after) {
            Feedback::Changed(count) => assert_eq!(count, 2),
            _ => panic!("expected changed entries"),
        }
    }

    #[test]
    fn same_contents_are_unchanged_even_after_rewrite() {
        let root = tempdir().unwrap();
        let file = root.path().join("stable.txt");
        std::fs::write(&file, b"identical bytes").unwrap();
        let before = snapshot(root.path());
        std::fs::write(&file, b"identical bytes").unwrap();
        let after = snapshot(root.path());
        assert!(matches!(before.compare(&after), Feedback::Unchanged));
    }

    #[test]
    fn protected_names_are_not_scanned_or_reported() {
        let root = tempdir().unwrap();
        for name in PROTECTED_NAMES {
            let path = root.path().join(std::ffi::OsStr::from_bytes(name));
            if name == b".git" {
                std::fs::create_dir(&path).unwrap();
                std::fs::write(path.join("secret"), b"initial secret").unwrap();
            } else {
                std::fs::write(&path, b"initial secret").unwrap();
            }
        }
        let before = snapshot(root.path());
        std::fs::write(root.path().join(".git/secret"), b"changed secret").unwrap();
        std::fs::write(root.path().join("credentials"), b"changed secret").unwrap();
        let after = snapshot(root.path());
        assert!(matches!(before.compare(&after), Feedback::Unchanged));
        assert!(matches!(
            feedback(Ok(before), Ok(after)),
            Feedback::Unchanged
        ));
    }

    #[test]
    fn symlink_is_fingerprinted_without_following_its_target() {
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"outside secret").unwrap();
        symlink(outside.path(), root.path().join("link")).unwrap();
        let before = snapshot(root.path());
        std::fs::write(outside.path().join("secret"), b"changed outside").unwrap();
        let after = snapshot(root.path());
        assert!(matches!(before.compare(&after), Feedback::Unchanged));
    }

    #[test]
    fn missing_root_and_exceeded_limits_are_unavailable() {
        let missing_parent = tempdir().unwrap();
        let missing = missing_parent.path().join("missing");
        assert!(Snapshot::capture(&missing).is_err());

        let root = tempdir().unwrap();
        std::fs::write(root.path().join("one"), b"1").unwrap();
        std::fs::write(root.path().join("two"), b"2").unwrap();
        let fd = fs::open(
            root.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .unwrap();
        assert!(
            Snapshot::from_root(
                fd,
                Limits {
                    entries: 1,
                    ..Limits::default()
                }
            )
            .is_err()
        );

        let fd = fs::open(
            root.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .unwrap();
        assert!(
            Snapshot::from_root(
                fd,
                Limits {
                    content_bytes: 0,
                    ..Limits::default()
                }
            )
            .is_err()
        );

        let nested = root.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("three"), b"3").unwrap();
        let fd = fs::open(
            root.path(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .unwrap();
        assert!(
            Snapshot::from_root(
                fd,
                Limits {
                    depth: 0,
                    ..Limits::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn incomplete_scans_never_mean_unchanged() {
        assert!(matches!(
            feedback(Err(Unavailable), Err(Unavailable)),
            Feedback::Unavailable
        ));
    }

    use std::os::unix::ffi::OsStrExt;
}
