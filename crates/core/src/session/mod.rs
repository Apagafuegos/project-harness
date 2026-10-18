//! Validated v1 session trees, durable JSONL transactions, and artifact storage.
//!
//! This Linux-first component has no CLI, terminal, provider, executor, or Tokio
//! dependency. See `docs/decisions/0003-jsonl-storage.md` for implemented decisions.

#![doc = include_str!("../../../../docs/session-storage.md")]

mod artifacts;
pub(crate) mod codec;
mod error;
mod identity;
pub(crate) mod io;
pub(crate) mod state;
pub(crate) mod store;
mod types;
mod validation;

pub use artifacts::DataRoot;
pub use codec::{MAX_COUNTER, MAX_LINE_BYTES};
pub use error::{Diagnostic, DiagnosticKind, StoreError};
pub use identity::*;
pub use state::{CommandReceipt, SessionSnapshot, StoredEntry, TurnInfo, UnresolvedInvocation};
pub use store::{CommitAck, Inspection, JournalStore};
pub use types::*;

#[cfg(test)]
mod tests;
