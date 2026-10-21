//! The projection: what the model should see, and its fingerprint.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::PROJECTION_VERSION;
use crate::{
    model::ModelInput,
    session::{
        ContentBlock, Digest, EntryId, ModelRef, ProviderContinuation, ResponseOutcome, ToolResult,
    },
};

/// Who an instruction speaks for, in RFC-001 order of authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionLevel {
    /// System instruction.
    System,
    /// Developer instruction.
    Developer,
}

/// One instruction from the session configuration artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instruction {
    /// Level.
    pub level: InstructionLevel,
    /// Text.
    pub text: String,
}

/// One item of the projection, in the order the model reads them. Items name
/// the journal entry they come from, so the report can account for them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContextItem {
    /// The configured instructions; always first when present.
    Instructions {
        /// In configured order.
        instructions: Vec<Instruction>,
    },
    /// A user message, all of its blocks.
    User {
        /// Source entry.
        entry_id: EntryId,
        /// Text and media blocks.
        content: Vec<ContentBlock>,
    },
    /// An assistant response with only its included blocks, in order.
    Assistant {
        /// Source entry.
        entry_id: EntryId,
        /// `completed`, or `cancelled`/`interrupted` for retained partial text.
        outcome: ResponseOutcome,
        /// Text, answered tool calls, media and provider state owned by the
        /// projection's model.
        content: Vec<ContentBlock>,
        /// The response's continuation, when owned by the projection's model.
        continuation: Option<ProviderContinuation>,
    },
    /// A tool result, directly after the response proposing its call.
    ToolResult {
        /// Source entry.
        entry_id: EntryId,
        /// The stored result.
        result: ToolResult,
    },
    /// An active compaction summary, at the position of its first covered entry.
    Summary {
        /// The compaction entry.
        entry_id: EntryId,
        /// Summary text.
        summary: String,
    },
}

impl ContextItem {
    /// The journal entry this item comes from; `None` for instructions.
    pub fn entry_id(&self) -> Option<&EntryId> {
        match self {
            Self::Instructions { .. } => None,
            Self::User { entry_id, .. }
            | Self::Assistant { entry_id, .. }
            | Self::ToolResult { entry_id, .. }
            | Self::Summary { entry_id, .. } => Some(entry_id),
        }
    }
}

/// The projection for one head and model, with its fingerprint. It is never
/// persisted; the same history always rebuilds it identically.
#[derive(Clone, Debug, PartialEq)]
pub struct Projection {
    /// Head whose branch was projected.
    pub head: Option<EntryId>,
    /// Model and items, ready for an adapter.
    pub input: ModelInput,
    /// SHA-256 of the canonical projection; see [`fingerprint`].
    pub fingerprint: Digest,
    /// Built under a model override. The core never sends a preview.
    pub preview: bool,
}

/// SHA-256 of the canonical JSON of `{"items", "model", "projection_version"}`.
///
/// Canonical means `serde_json`'s compact output of these types: struct fields
/// in declared order, enum tags first, map keys (tool-call arguments) sorted
/// bytewise, no insignificant whitespace. Map keys are sorted because
/// `serde_json`'s `preserve_order` feature is off, which a test enforces.
/// Changing any item type changes these bytes, so it must bump
/// [`PROJECTION_VERSION`]. Responses, requests and the report are not hashed.
pub fn fingerprint(model: &ModelRef, items: &[ContextItem]) -> Digest {
    fingerprint_with_version(PROJECTION_VERSION, model, items)
}

pub(super) fn fingerprint_with_version(
    version: u32,
    model: &ModelRef,
    items: &[ContextItem],
) -> Digest {
    #[derive(Serialize)]
    struct Canonical<'a> {
        items: &'a [ContextItem],
        model: &'a ModelRef,
        projection_version: u32,
    }
    let mut hasher = Hashing(Sha256::new());
    serde_json::to_writer(
        &mut hasher,
        &Canonical {
            items,
            model,
            projection_version: version,
        },
    )
    .expect("projection items serialize to JSON");
    format!("{:x}", hasher.0.finalize())
        .try_into()
        .expect("SHA-256 hex is a digest")
}

/// Streams serialized bytes into the hash without buffering the projection.
struct Hashing(Sha256);

impl std::io::Write for Hashing {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn canonical_bytes(model: &ModelRef, items: &[ContextItem]) -> Vec<u8> {
    #[derive(Serialize)]
    struct Canonical<'a> {
        items: &'a [ContextItem],
        model: &'a ModelRef,
        projection_version: u32,
    }
    serde_json::to_vec(&Canonical {
        items,
        model,
        projection_version: PROJECTION_VERSION,
    })
    .unwrap()
}
