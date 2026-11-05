//! Descriptor-relative trusted file tools. Authorization remains the caller's job.
//!
//! Workspace resolution after root selection uses `openat2` relative to the
//! pinned root, with the same strict flags for every full relative prefix.
//! Temporary creation and replacement use the verified, pinned parent.

pub(super) mod execute;
mod resolve;
#[cfg(test)]
mod search_tests;
mod undo;
pub(crate) use undo::PreparedUndo;
mod walk;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod undo_tests;

use std::{collections::BTreeMap, path::Path};

use rustix::{
    fd::OwnedFd,
    fs::{self, Mode, OFlags, ResolveFlags},
    io::Errno,
};

use super::{Protection, WorkspaceIdentity, WorkspacePath};

use resolve::{Control, ObjectId, Step, errno};

const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS)
    .union(ResolveFlags::NO_XDEV);

/// Trusted startup exclusions, relative to the authorized workspace.
#[derive(Clone, Debug, Default)]
pub struct FsProtectedPaths {
    /// Configured protected paths; existing objects are pinned and denied.
    pub configured: Vec<WorkspacePath>,
    /// The harness data root when inside the workspace. The session owner must
    /// supply its workspace-relative location; an outside data root needs none.
    pub harness_data_root: Option<WorkspacePath>,
}

/// File tools cannot safely operate on this workspace. There is no fallback.
#[derive(Clone, Debug, thiserror::Error)]
#[error("trusted file tools unavailable: {reason}")]
pub struct FsUnavailable {
    /// Content-free startup diagnostic, including an errno when applicable.
    pub reason: String,
}

/// A pinned workspace capability and startup object-identity protection set.
/// This type neither authorizes calls nor executes external programs.
#[derive(Debug)]
pub struct FsWorkspace {
    root: OwnedFd,
    identity: WorkspaceIdentity,
    protected: BTreeMap<ObjectId, Protection>,
    excluded: Vec<WorkspacePath>,
    // Keep identities alive: inode numbers must not be recycled during a session.
    protected_handles: Vec<OwnedFd>,
}

impl FsWorkspace {
    /// Pin the explicitly authorized root, probe strict `openat2`, then pin
    /// configured exclusions, the in-workspace data root, and `.git` if present.
    /// Missing exclusions are ignored (policy still denies their configured
    /// spellings); other resolution failures make the capability unavailable.
    pub fn open(
        root: impl AsRef<Path>,
        protected: &FsProtectedPaths,
    ) -> Result<Self, FsUnavailable> {
        Self::open_probed(root.as_ref(), protected, |fd| {
            fs::openat2(
                fd,
                ".",
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
                RESOLVE,
            )
        })
    }

    fn open_probed(
        root: &Path,
        protected: &FsProtectedPaths,
        probe: impl FnOnce(&OwnedFd) -> Result<OwnedFd, Errno>,
    ) -> Result<Self, FsUnavailable> {
        let unavailable = |error: super::Refusal| FsUnavailable {
            reason: error.text(),
        };
        let identity = root.to_str().ok_or_else(|| FsUnavailable {
            reason: "workspace identity must be UTF-8".into(),
        })?;
        // The only ambient path open: trusted, explicit root selection. Never
        // reopen this string after pinning it, including if it is renamed.
        let root = fs::open(
            root,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|e| unavailable(errno(e)))?;
        let _probe = probe(&root).map_err(|e| FsUnavailable {
            reason: format!("required openat2 probe failed: {}", errno(e).detail),
        })?;
        let mut workspace = Self {
            root,
            identity: WorkspaceIdentity(identity.into()),
            protected: BTreeMap::new(),
            excluded: protected
                .configured
                .iter()
                .cloned()
                .chain(protected.harness_data_root.iter().cloned())
                .collect(),
            protected_handles: Vec::new(),
        };
        let git = WorkspacePath::new(".git").map_err(unavailable)?;
        let mut entries: Vec<_> = protected
            .configured
            .iter()
            .map(|p| (p, Protection::Denied))
            .collect();
        if let Some(path) = &protected.harness_data_root {
            entries.push((path, Protection::Denied));
        }
        entries.push((&git, Protection::ReadOnly));
        for (path, protection) in entries {
            let mut noop = |_| Ok(());
            let mut control = Control::planning(&mut noop);
            let resolved = workspace.resolve(path, &mut control).map_err(unavailable)?;
            if let Some(fd) = resolved.object {
                control.check(Step::Stat).map_err(unavailable)?;
                let stat = fs::fstat(&fd).map_err(|e| unavailable(errno(e)))?;
                workspace
                    .protected
                    .entry(ObjectId::of(&stat))
                    .and_modify(|p| *p = (*p).max(protection))
                    .or_insert(protection);
                workspace.protected_handles.push(fd);
            }
        }
        Ok(workspace)
    }
}
