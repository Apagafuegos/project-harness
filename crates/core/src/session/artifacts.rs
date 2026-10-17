//! Descriptor-relative private storage and durable content-addressed artifacts.

use super::{
    ArtifactRef, Digest, SessionId, StoreError,
    codec::MAX_COUNTER,
    error::Result,
    io::{Io, Point},
};
use rustix::fs::{self, Mode, OFlags, RenameFlags};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

/// Private local storage rooted outside the authorized workspace.
///
/// The root's parent must already exist. Each traversed component is opened
/// without following symlinks. The root and its two children must be owned by
/// the effective user and mode 0700; stored files must be single-link mode 0600.
#[derive(Debug)]
pub struct DataRoot {
    pub(crate) sessions: File,
    pub(crate) artifacts: File,
    pub(crate) path: PathBuf,
    pub(crate) io: Io,
}

fn open_dir(parent: &File, name: &std::ffi::OsStr) -> Result<File> {
    Ok(fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?
    .into())
}
fn private_dir(parent: &File, name: &std::ffi::OsStr, create: bool) -> Result<File> {
    if create {
        match fs::mkdirat(parent, name, Mode::RWXU) {
            Ok(()) => parent.sync_all()?,
            Err(rustix::io::Errno::EXIST) => (),
            Err(error) => return Err(error.into()),
        }
    }
    let dir = open_dir(parent, name)?;
    verify_private(&dir, true)?;
    Ok(dir)
}
pub(crate) fn verify_private(file: &File, directory: bool) -> Result<()> {
    let meta = file.metadata()?;
    if meta.uid() != rustix::process::geteuid().as_raw()
        || (meta.mode() & 0o7777) != if directory { 0o700 } else { 0o600 }
        || if directory {
            !meta.is_dir()
        } else {
            !meta.is_file() || meta.nlink() != 1
        }
    {
        return Err(StoreError::UnsafePath);
    }
    Ok(())
}

impl DataRoot {
    /// Resolve the documented XDG default without creating it.
    pub fn default_path() -> Result<PathBuf> {
        if let Some(value) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(StoreError::UnsafePath);
            }
            return Ok(path.join("project-harness"));
        }
        std::env::var_os("HOME")
            .map(|v| PathBuf::from(v).join(".local/state/project-harness"))
            .ok_or(StoreError::UnsafePath)
    }

    /// Open an existing root, or create the final directory and its children.
    /// No missing ancestor directories are created and no permissions are relaxed.
    pub fn open(path: impl AsRef<Path>, create: bool) -> Result<Self> {
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err(StoreError::UnsafePath);
        }
        let mut components = path.components().peekable();
        let mut dir = File::open("/")?;
        let mut root = None;
        while let Some(component) = components.next() {
            match component {
                Component::RootDir => (),
                Component::Normal(name) if components.peek().is_none() => {
                    root = Some(private_dir(&dir, name, create)?)
                }
                Component::Normal(name) => dir = open_dir(&dir, name)?,
                _ => return Err(StoreError::UnsafePath),
            }
        }
        let root = root.ok_or(StoreError::UnsafePath)?;
        let sessions = private_dir(&root, std::ffi::OsStr::new("sessions"), create)?;
        let artifacts = private_dir(&root, std::ffi::OsStr::new("artifacts"), create)?;
        Ok(Self {
            sessions,
            artifacts,
            path: path.to_path_buf(),
            io: Io::default(),
        })
    }

    /// Return the explicitly selected root path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn journal(&self, id: &SessionId, create: bool, write: bool) -> Result<File> {
        let mut flags = OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
        flags |= if write {
            OFlags::RDWR | OFlags::APPEND
        } else {
            OFlags::RDONLY
        };
        if create {
            flags |= OFlags::CREATE | OFlags::EXCL;
        }
        let file: File = fs::openat(
            &self.sessions,
            format!("{id}.jsonl"),
            flags,
            Mode::RUSR | Mode::WUSR,
        )?
        .into();
        verify_private(&file, false)?;
        Ok(file)
    }

    fn artifact_file(&self, digest: &Digest) -> Result<File> {
        let file: File = fs::openat(
            &self.artifacts,
            digest.as_str(),
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|_| StoreError::Artifact)?
        .into();
        verify_private(&file, false).map_err(|_| StoreError::Artifact)?;
        Ok(file)
    }

    /// Stream an artifact into a private file and publish it only after file sync,
    /// no-replace rename, and directory sync. Existing bytes are always verified.
    pub fn put_artifact(&self, bytes: &[u8]) -> Result<ArtifactRef> {
        self.put_artifact_reader(&mut &*bytes)
    }

    /// Streaming variant for large media/output without an in-memory size assumption.
    pub fn put_artifact_reader(&self, reader: &mut impl Read) -> Result<ArtifactRef> {
        use sha2::{Digest as _, Sha256};
        let name = format!(".tmp-{}", uuid::Uuid::now_v7());
        let mut temp: File = fs::openat(
            &self.artifacts,
            name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )?
        .into();
        let result = (|| {
            let mut hash = Sha256::new();
            let mut bytes = 0u64;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                bytes = bytes
                    .checked_add(n as u64)
                    .filter(|n| *n <= MAX_COUNTER)
                    .ok_or(StoreError::Limit)?;
                temp.write_all(&buf[..n])?;
                hash.update(&buf[..n]);
            }
            self.io.check(Point::ArtifactWritten)?;
            temp.flush()?;
            temp.sync_all()?;
            self.io.check(Point::ArtifactSynced)?;
            let reference = ArtifactRef {
                sha256: format!("{:x}", hash.finalize()).parse().expect("SHA-256"),
                bytes,
            };
            match fs::renameat_with(
                &self.artifacts,
                name.as_str(),
                &self.artifacts,
                reference.sha256.as_str(),
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => (),
                Err(rustix::io::Errno::EXIST) => self.verify_artifact(&reference)?,
                Err(error) => return Err(error.into()),
            }
            self.io.check(Point::ArtifactRenamed)?;
            self.artifacts.sync_all()?;
            self.io.check(Point::ArtifactDirectorySynced)?;
            Ok(reference)
        })();
        // A successful rename removed this name; otherwise remove the private temp.
        let _ = fs::unlinkat(&self.artifacts, name.as_str(), fs::AtFlags::empty());
        result
    }

    /// Verify exact digest and length without loading the artifact into memory.
    pub fn verify_artifact(&self, reference: &ArtifactRef) -> Result<()> {
        use sha2::{Digest as _, Sha256};
        if reference.bytes > MAX_COUNTER {
            return Err(StoreError::Limit);
        }
        let mut file = self.artifact_file(&reference.sha256)?;
        if file.metadata()?.len() != reference.bytes {
            return Err(StoreError::Artifact);
        }
        let mut hash = Sha256::new();
        let mut length = 0u64;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            length += n as u64;
            if length > reference.bytes {
                return Err(StoreError::Artifact);
            }
            hash.update(&buf[..n]);
        }
        let digest: Digest = format!("{:x}", hash.finalize()).parse().expect("SHA-256");
        if length != reference.bytes || digest != reference.sha256 {
            return Err(StoreError::Artifact);
        }
        file.sync_all()?;
        self.artifacts.sync_all()?;
        Ok(())
    }

    /// Read a verified artifact, refusing allocation beyond the caller's limit.
    /// Callers must keep provider-owned opaque bytes out of logs and model text.
    pub fn read_artifact(&self, reference: &ArtifactRef, max_bytes: usize) -> Result<Vec<u8>> {
        if reference.bytes > MAX_COUNTER || reference.bytes > max_bytes as u64 {
            return Err(StoreError::Limit);
        }
        let mut bytes = Vec::new();
        self.artifact_file(&reference.sha256)?
            .take(reference.bytes + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != reference.bytes || Digest::of(&bytes) != reference.sha256 {
            return Err(StoreError::Artifact);
        }
        Ok(bytes)
    }

    pub(crate) fn artifact_size(&self, digest: &Digest) -> Result<u64> {
        Ok(self.artifact_file(digest)?.metadata()?.len())
    }

    pub(crate) fn verify_digest(&self, digest: &Digest) -> Result<()> {
        let file = self.artifact_file(digest)?;
        self.verify_artifact(&ArtifactRef {
            sha256: digest.clone(),
            bytes: file.metadata()?.len(),
        })
    }
}
