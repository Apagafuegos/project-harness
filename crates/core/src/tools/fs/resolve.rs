use std::time::{Duration, Instant};

use crate::tools::CancelToken;
use rustix::{
    fd::{AsFd, BorrowedFd, OwnedFd},
    fs::{self, FileType, Mode, OFlags, Stat},
    io::{self, Errno},
};
use sha2::{Digest as _, Sha256};

use super::{FsWorkspace, RESOLVE};
use crate::{
    session::{Digest, ResultCode},
    tools::{
        FileFacts, FileKind, Protection, Refusal, WorkspaceIdentity, WorkspacePath, WorkspaceView,
        plan::refuse_protection,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ObjectId(u64, u64);
impl ObjectId {
    pub(super) fn of(stat: &Stat) -> Self {
        Self(stat.st_dev, stat.st_ino)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Resolve,
    Stat,
    OpenFile,
    HashRead,
    Read,
    CreateTemp,
    WriteTemp,
    SyncTemp,
    ChmodTemp,
    Recheck,
    Rename,
    SyncParent,
}

pub(super) struct Control<'a> {
    pub(super) started: Instant,
    pub(super) timeout: Option<Duration>,
    pub(super) cancellation: Option<&'a CancelToken>,
    pub(super) hook: &'a mut dyn FnMut(Step) -> Result<(), Refusal>,
}
impl<'a> Control<'a> {
    pub(super) fn planning(hook: &'a mut dyn FnMut(Step) -> Result<(), Refusal>) -> Self {
        Self {
            started: Instant::now(),
            timeout: None,
            cancellation: None,
            hook,
        }
    }
    pub(super) fn check(&mut self, step: Step) -> Result<(), Refusal> {
        (self.hook)(step)?;
        if self.cancellation.is_some_and(CancelToken::is_cancelled) {
            return Err(Refusal::new(
                ResultCode::Cancelled,
                "cancelled before commit",
            ));
        }
        if self.timeout.is_some_and(|t| self.started.elapsed() >= t) {
            return Err(Refusal::new(
                ResultCode::Timeout,
                "file tool deadline exceeded",
            ));
        }
        Ok(())
    }
}

pub(super) struct Resolved {
    pub(super) parent: OwnedFd,
    pub(super) name: String,
    pub(super) object: Option<OwnedFd>,
    pub(super) protection: Protection,
    pub(super) complete: bool,
    pub(super) path: WorkspacePath,
    ancestors: Vec<(String, ObjectId)>,
}

pub(super) fn open_relative(
    parent: BorrowedFd<'_>,
    name: &str,
    flags: OFlags,
    mode: Mode,
    control: &mut Control<'_>,
    step: Step,
) -> Result<OwnedFd, Refusal> {
    control.check(step)?;
    #[cfg(test)]
    super::tests::record_open();
    fs::openat2(parent, name, flags | OFlags::CLOEXEC, mode, RESOLVE).map_err(errno)
}

impl FsWorkspace {
    /// Enumerate immediate real directories beneath the pinned root, retaining
    /// at most `limit` lexicographically first paths. Symlinks are never returned.
    /// `keep` filters names before the bound; `None` selects the root itself.
    /// Reopens and ancestor checks use the same strict resolution as file reads.
    pub fn list_directories(
        &self,
        path: Option<&WorkspacePath>,
        limit: usize,
        keep: impl Fn(&str) -> bool,
    ) -> Result<(Vec<WorkspacePath>, bool), Refusal> {
        let mut noop = |_| Ok(());
        let mut control = Control::planning(&mut noop);
        let resolved = path.map(|p| self.resolve(p, &mut control)).transpose()?;
        if let Some(r) = &resolved {
            refuse_protection(r.protection, false)?;
        }
        let directory = open_relative(
            self.root.as_fd(),
            path.map_or(".", WorkspacePath::as_str),
            OFlags::RDONLY | OFlags::DIRECTORY,
            Mode::empty(),
            &mut control,
            Step::OpenFile,
        )?;
        let stat = fs::fstat(&directory).map_err(errno)?;
        if let Some(r) = &resolved {
            let pinned = r
                .object
                .as_ref()
                .ok_or_else(|| Refusal::new(ResultCode::NotFound, "directory absent"))?;
            if ObjectId::of(&stat) != ObjectId::of(&fs::fstat(pinned).map_err(errno)?) {
                return Err(stale());
            }
            self.verify_ancestors(&r.ancestors, &mut control)?;
        }
        let mut paths = std::collections::BTreeSet::new();
        let mut limited = false;
        for entry in fs::Dir::read_from(&directory).map_err(errno)? {
            let entry = entry.map_err(errno)?;
            if !matches!(entry.file_type(), FileType::Directory | FileType::Unknown) {
                continue;
            }
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if name == "." || name == ".." || !keep(name) {
                continue;
            }
            let relative =
                path.map_or_else(|| name.to_owned(), |p| format!("{}/{name}", p.as_str()));
            // Some filesystems do not report d_type. Resolve unknown entries
            // strictly instead of silently omitting their real directories.
            if entry.file_type() == FileType::Unknown {
                match open_relative(
                    self.root.as_fd(),
                    &relative,
                    OFlags::PATH | OFlags::DIRECTORY,
                    Mode::empty(),
                    &mut control,
                    Step::Resolve,
                ) {
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.code,
                            ResultCode::NotRegularFile
                                | ResultCode::SymlinkRejected
                                | ResultCode::NotFound
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }
            if let Ok(path) = WorkspacePath::new(&relative) {
                paths.insert(path.as_str().to_owned());
                if paths.len() > limit {
                    paths.pop_last();
                    limited = true;
                }
            }
        }
        // A retained descriptor is not authority to read a moved-out ancestor.
        if let Some(r) = &resolved {
            self.verify_ancestors(&r.ancestors, &mut control)?;
        }
        let current = open_relative(
            self.root.as_fd(),
            path.map_or(".", WorkspacePath::as_str),
            OFlags::PATH | OFlags::DIRECTORY,
            Mode::empty(),
            &mut control,
            Step::Resolve,
        )?;
        if ObjectId::of(&stat) != ObjectId::of(&fs::fstat(&current).map_err(errno)?) {
            return Err(stale());
        }
        Ok((
            paths
                .into_iter()
                .map(|p| WorkspacePath::new(&p).expect("validated path"))
                .collect(),
            limited,
        ))
    }

    fn protection(&self, stat: &Stat) -> Protection {
        self.protected
            .get(&ObjectId::of(stat))
            .copied()
            .unwrap_or(Protection::None)
    }

    pub(super) fn resolve(
        &self,
        path: &WorkspacePath,
        control: &mut Control<'_>,
    ) -> Result<Resolved, Refusal> {
        let mut parent = open_relative(
            self.root.as_fd(),
            ".",
            OFlags::PATH | OFlags::DIRECTORY,
            Mode::empty(),
            control,
            Step::Resolve,
        )?;
        control.check(Step::Stat)?;
        let mut protection = self.protection(&fs::fstat(&parent).map_err(errno)?);
        let mut components = path.as_str().split('/').peekable();
        let mut prefix = String::new();
        let mut ancestors = Vec::new();
        while let Some(name) = components.next() {
            let last = components.peek().is_none();
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(name);
            let opened = open_relative(
                self.root.as_fd(),
                &prefix,
                OFlags::PATH,
                Mode::empty(),
                control,
                Step::Resolve,
            );
            let object = match opened {
                Ok(fd) => fd,
                Err(error) if error.code == ResultCode::NotFound => {
                    self.verify_ancestors(&ancestors, control)?;
                    return Ok(Resolved {
                        parent,
                        name: name.into(),
                        object: None,
                        protection,
                        complete: last,
                        path: path.clone(),
                        ancestors,
                    });
                }
                Err(error) => return Err(error),
            };
            control.check(Step::Stat)?;
            let stat = fs::fstat(&object).map_err(errno)?;
            protection = protection.max(self.protection(&stat));
            if last {
                self.verify_ancestors(&ancestors, control)?;
                return Ok(Resolved {
                    parent,
                    name: name.into(),
                    object: Some(object),
                    protection,
                    complete: true,
                    path: path.clone(),
                    ancestors,
                });
            }
            if kind(&stat) != FileKind::Directory {
                return Err(Refusal::new(
                    ResultCode::NotRegularFile,
                    "ancestor is not a directory",
                ));
            }
            ancestors.push((prefix.clone(), ObjectId::of(&stat)));
            parent = object;
        }
        Err(Refusal::new(
            ResultCode::InfrastructureFailure,
            "validated path has no components",
        ))
    }

    fn verify_ancestors(
        &self,
        ancestors: &[(String, ObjectId)],
        control: &mut Control<'_>,
    ) -> Result<(), Refusal> {
        for (prefix, expected) in ancestors {
            let directory = open_relative(
                self.root.as_fd(),
                prefix,
                OFlags::PATH | OFlags::DIRECTORY,
                Mode::empty(),
                control,
                Step::Resolve,
            )?;
            control.check(Step::Stat)?;
            if ObjectId::of(&fs::fstat(&directory).map_err(errno)?) != *expected {
                return Err(stale());
            }
        }
        Ok(())
    }

    pub(super) fn regular(
        &self,
        resolved: &Resolved,
        mutation: bool,
        control: &mut Control<'_>,
    ) -> Result<(OwnedFd, Stat), Refusal> {
        refuse_protection(resolved.protection, mutation)?;
        let object = resolved
            .object
            .as_ref()
            .ok_or_else(|| Refusal::new(ResultCode::NotFound, "target does not exist"))?;
        control.check(Step::Stat)?;
        let pinned = fs::fstat(object).map_err(errno)?;
        require_regular(&pinned)?;
        let file = open_relative(
            self.root.as_fd(),
            resolved.path.as_str(),
            OFlags::RDONLY | OFlags::NONBLOCK,
            Mode::empty(),
            control,
            Step::OpenFile,
        )?;
        control.check(Step::Stat)?;
        let stat = fs::fstat(&file).map_err(errno)?;
        require_regular(&stat)?;
        if ObjectId::of(&stat) != ObjectId::of(&pinned) {
            return Err(stale());
        }
        self.verify_ancestors(&resolved.ancestors, control)?;
        refuse_protection(self.protection(&stat), mutation)?;
        Ok((file, stat))
    }
}

impl WorkspaceView for FsWorkspace {
    fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }

    fn inspect(&self, path: &WorkspacePath) -> Result<FileFacts, Refusal> {
        let mut noop = |_| Ok(());
        self.inspect_controlled(path, &mut Control::planning(&mut noop))
    }

    fn read_text(&self, path: &WorkspacePath, max_bytes: u64) -> Result<String, Refusal> {
        let mut noop = |_| Ok(());
        let mut control = Control::planning(&mut noop);
        let resolved = self.resolve(path, &mut control)?;
        let (file, stat) = self.regular(&resolved, false, &mut control)?;
        let bytes = read_bounded(&file, &stat, max_bytes, &mut control)?;
        String::from_utf8(bytes).map_err(|_| Refusal::new(ResultCode::NotUtf8, "file is not UTF-8"))
    }
}

impl FsWorkspace {
    // Linux st_nlink is u32 on aarch64 and u64 on x86_64.
    #[allow(clippy::useless_conversion)]
    pub(super) fn inspect_controlled(
        &self,
        path: &WorkspacePath,
        control: &mut Control<'_>,
    ) -> Result<FileFacts, Refusal> {
        let resolved = self.resolve(path, control)?;
        let Some(object) = &resolved.object else {
            return Ok(FileFacts {
                kind: FileKind::Absent,
                size: 0,
                nlink: 0,
                digest: None,
                protection: resolved.protection,
            });
        };
        control.check(Step::Stat)?;
        let stat = fs::fstat(object).map_err(errno)?;
        let kind = kind(&stat);
        // No whole-file cap: digest every byte in bounded chunks. A changed
        // file fails rather than combining facts from different pre-images.
        let digest = if kind == FileKind::Regular {
            let file = open_relative(
                self.root.as_fd(),
                resolved.path.as_str(),
                OFlags::RDONLY | OFlags::NONBLOCK,
                Mode::empty(),
                control,
                Step::OpenFile,
            )?;
            control.check(Step::Stat)?;
            let current = fs::fstat(&file).map_err(errno)?;
            if !unchanged(&stat, &current) || kind_of_other(&current) {
                return Err(stale());
            }
            self.verify_ancestors(&resolved.ancestors, control)?;
            Some(hash(&file, &current, control)?)
        } else {
            None
        };
        Ok(FileFacts {
            kind,
            size: stat.st_size as u64,
            nlink: u64::from(stat.st_nlink),
            digest,
            protection: resolved.protection,
        })
    }
}

fn kind_of_other(stat: &Stat) -> bool {
    kind(stat) != FileKind::Regular
}

pub(super) fn kind(stat: &Stat) -> FileKind {
    match FileType::from_raw_mode(stat.st_mode) {
        FileType::RegularFile => FileKind::Regular,
        FileType::Directory => FileKind::Directory,
        _ => FileKind::Other,
    }
}

pub(super) fn require_regular(stat: &Stat) -> Result<(), Refusal> {
    if kind(stat) != FileKind::Regular {
        return Err(Refusal::new(
            ResultCode::NotRegularFile,
            "target is not a regular file",
        ));
    }
    if stat.st_nlink > 1 {
        return Err(Refusal::new(
            ResultCode::HardLinkRejected,
            "multiply linked files are not supported",
        ));
    }
    Ok(())
}

pub(super) fn stale() -> Refusal {
    Refusal::new(ResultCode::StalePlan, "workspace pre-image changed")
}

fn unchanged(before: &Stat, after: &Stat) -> bool {
    ObjectId::of(before) == ObjectId::of(after)
        && before.st_size == after.st_size
        && before.st_nlink == after.st_nlink
        && before.st_mtime == after.st_mtime
        && before.st_mtime_nsec == after.st_mtime_nsec
        && before.st_ctime == after.st_ctime
        && before.st_ctime_nsec == after.st_ctime_nsec
}

pub(super) fn verify_unchanged(
    file: BorrowedFd<'_>,
    stat: &Stat,
    control: &mut Control<'_>,
) -> Result<(), Refusal> {
    control.check(Step::Stat)?;
    if unchanged(stat, &fs::fstat(file).map_err(errno)?) {
        Ok(())
    } else {
        Err(stale())
    }
}

pub(super) fn hash(
    file: &OwnedFd,
    stat: &Stat,
    control: &mut Control<'_>,
) -> Result<Digest, Refusal> {
    let mut hasher = Sha256::new();
    let mut buffer = [0; 65536];
    let mut offset = 0;
    loop {
        control.check(Step::HashRead)?;
        let count = io::pread(file, &mut buffer[..], offset).map_err(errno)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        offset += count as u64;
    }
    verify_unchanged(file.as_fd(), stat, control)?;
    if offset != stat.st_size as u64 {
        return Err(stale());
    }
    format!("{:x}", hasher.finalize()).parse().map_err(|_| {
        Refusal::new(
            ResultCode::InfrastructureFailure,
            "invalid SHA-256 encoding",
        )
    })
}

pub(super) fn read_bounded(
    file: &OwnedFd,
    stat: &Stat,
    max: u64,
    control: &mut Control<'_>,
) -> Result<Vec<u8>, Refusal> {
    if stat.st_size as u64 > max {
        return Err(Refusal::new(
            ResultCode::TooLarge,
            "file exceeds byte limit",
        ));
    }
    let bytes = read_range(file, 0, stat.st_size as usize, control)?;
    verify_unchanged(file.as_fd(), stat, control)?;
    Ok(bytes)
}

pub(super) fn read_range(
    file: &OwnedFd,
    offset: u64,
    count: usize,
    control: &mut Control<'_>,
) -> Result<Vec<u8>, Refusal> {
    let mut bytes = vec![0; count];
    let mut read = 0;
    while read < count {
        control.check(Step::Read)?;
        let n = io::pread(file, &mut bytes[read..], offset + read as u64).map_err(errno)?;
        if n == 0 {
            return Err(stale());
        }
        read += n;
    }
    Ok(bytes)
}

pub(super) fn errno(error: Errno) -> Refusal {
    let (code, name, detail) = match error {
        Errno::LOOP => (ResultCode::SymlinkRejected, "ELOOP", "symlink rejected"),
        Errno::XDEV => (ResultCode::ProtectedPath, "EXDEV", "crosses a mount"),
        Errno::NOENT => (ResultCode::NotFound, "ENOENT", "target does not exist"),
        Errno::NOTDIR => (
            ResultCode::NotRegularFile,
            "ENOTDIR",
            "ancestor is not a directory",
        ),
        _ => (
            ResultCode::InfrastructureFailure,
            errno_name(error),
            "filesystem operation failed",
        ),
    };
    Refusal::new(code, &format!("{detail}: {name} ({error})"))
}

fn errno_name(error: Errno) -> &'static str {
    match error {
        Errno::PERM => "EPERM",
        Errno::ACCESS => "EACCES",
        Errno::IO => "EIO",
        Errno::NOSYS => "ENOSYS",
        Errno::INVAL => "EINVAL",
        Errno::NOSPC => "ENOSPC",
        Errno::DQUOT => "EDQUOT",
        Errno::ROFS => "EROFS",
        Errno::EXIST => "EEXIST",
        Errno::INTR => "EINTR",
        Errno::AGAIN => "EAGAIN",
        Errno::NOMEM => "ENOMEM",
        Errno::MFILE => "EMFILE",
        Errno::NFILE => "ENFILE",
        Errno::FBIG => "EFBIG",
        Errno::NAMETOOLONG => "ENAMETOOLONG",
        Errno::BADF => "EBADF",
        Errno::BUSY => "EBUSY",
        Errno::NXIO => "ENXIO",
        Errno::ISDIR => "EISDIR",
        Errno::NOTEMPTY => "ENOTEMPTY",
        Errno::STALE => "ESTALE",
        Errno::OPNOTSUPP => "EOPNOTSUPP",
        _ => "UNKNOWN_ERRNO",
    }
}
