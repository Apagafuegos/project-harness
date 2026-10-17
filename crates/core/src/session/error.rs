//! Storage failures and byte-accurate inspection diagnostics.

/// A stable diagnostic at the start of the first unaccepted physical record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// Byte offset of the first unaccepted record or unterminated suffix.
    pub offset: u64,
    /// Classification; only `TornTail` permits in-place repair.
    pub kind: DiagnosticKind,
    /// Stable reason without journal content or artifact payloads.
    pub code: String,
}

/// How an invalid journal must be handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// Uncommitted bytes lacking a final LF.
    TornTail,
    /// A complete invalid record; ordinary repair must refuse it.
    Corrupt,
    /// An unsupported record format.
    UnsupportedVersion,
    /// Referenced bytes are absent, unsafe, or fail integrity verification.
    Artifact,
    /// A physical record exceeds the bounded reader limit.
    Limit,
}

/// Fail-closed errors from the session and artifact stores.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Another engine holds the journal's exclusive lock.
    #[error("session_locked")]
    Locked,
    /// Inspection is required before writable open can succeed.
    #[error("journal rejected at byte {0:?}")]
    Journal(Diagnostic),
    /// Supplied data violates the persisted schema or semantic rules.
    #[error("invalid transaction: {0}")]
    Invalid(&'static str),
    /// Compare-and-set selected a head different from the current head.
    #[error("stale_head")]
    StaleHead,
    /// A previously accepted command identity was reused with different content.
    #[error("stale_command")]
    StaleCommand,
    /// An I/O failure made the current writer unusable until reopen.
    #[error("writer_poisoned")]
    Poisoned,
    /// Repair's expected size or digest no longer matches the file.
    #[error("stale_repair")]
    StaleRepair,
    /// The source has no repairable tail, or lacks a valid header.
    #[error("tail_not_repairable")]
    NotRepairable,
    /// A storage object has unsafe ownership, mode, links, or type.
    #[error("unsafe_storage_object")]
    UnsafePath,
    /// An artifact does not exist or its exact hash/length is wrong.
    #[error("artifact_missing_or_invalid")]
    Artifact,
    /// A bounded input or integer limit was exceeded.
    #[error("storage_limit_exceeded")]
    Limit,
    /// The operating system failed a storage operation.
    #[error("storage I/O: {0}")]
    Io(#[from] std::io::Error),
}
impl From<rustix::io::Errno> for StoreError {
    fn from(value: rustix::io::Errno) -> Self {
        Self::Io(value.into())
    }
}

pub(crate) type Result<T> = std::result::Result<T, StoreError>;
