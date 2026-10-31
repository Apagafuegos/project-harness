//! Validated, versioned session trees, durable JSONL transactions, and artifact storage.
//!
//! This Linux-first component has no CLI, terminal, provider, executor, or Tokio
//! dependency. See `docs/decisions/0003-jsonl-storage.md` for implemented decisions.

#![doc = include_str!("../../../../docs/session-storage.md")]

mod artifacts;
mod catalog;
pub(crate) mod codec;
mod error;
mod identity;
pub(crate) mod io;
mod manifest;
pub(crate) mod state;
pub(crate) mod store;
mod types;
mod validation;

pub use artifacts::DataRoot;
pub use catalog::{CatalogStatus, SESSION_TITLE_CHARS, SessionCatalogEntry};
pub use codec::{CURRENT_VERSION, MAX_COUNTER, MAX_LINE_BYTES, SUPPORTED_VERSIONS};
pub use error::{Diagnostic, DiagnosticKind, StoreError};
pub use identity::*;
pub use manifest::compaction_manifest;
pub use state::{CommandReceipt, SessionSnapshot, StoredEntry, TurnInfo, UnresolvedInvocation};
pub use store::{CommitAck, Inspection, JournalStore};
pub use types::*;

#[cfg(test)]
mod tests;
