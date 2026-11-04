//! Validated, versioned session trees, durable JSONL transactions, and artifact storage.
//!
//! This Linux-first component has no CLI, terminal, provider, executor, or Tokio
//! dependency. See `docs/decisions/0003-jsonl-storage.md` for implemented decisions.

#![doc = include_str!("../../../../docs/session-storage.md")]

mod artifacts;
mod catalog;
pub(crate) mod codec;
pub(crate) mod coverage;
mod error;
mod identity;
pub(crate) mod io;
mod maintenance;
mod manifest;
pub(crate) mod state;
pub(crate) mod store;
mod types;
pub(crate) mod undo;
mod validation;

pub use artifacts::DataRoot;
pub use catalog::{
    CatalogStatus, SESSION_TITLE_CHARS, SessionCatalogEntry, session_title, session_title_of,
};
pub use codec::{CURRENT_VERSION, MAX_COUNTER, MAX_LINE_BYTES, SUPPORTED_VERSIONS};
pub use error::{Diagnostic, DiagnosticKind, StoreError};
pub use identity::*;
pub use maintenance::{
    ARTIFACT_GRACE, GcCandidate, GcOptions, GcReport, MaintenanceError, markdown_export,
    write_markdown_export,
};
pub use manifest::compaction_manifest;
pub use state::{
    CommandReceipt, SessionSnapshot, StoredEntry, TurnInfo, UndoInProgress, UnresolvedInvocation,
};
pub use store::{CommitAck, Inspection, JournalStore};
pub use types::*;
pub use undo::{UndoConflict, UndoSelection};

#[cfg(test)]
mod tests;
