//! The projection: what the model should see, and its fingerprint.

use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

use super::PROJECTION_VERSION;
use crate::{
    model::{InferenceOptions, ModelInput},
    session::{
        ArtifactRef, CallRef, ContentBlock, Digest, EntryId, InvocationId, ModelRef, Provenance,
        ResponseOutcome, ToolMetadata, ToolResult, ToolStatus,
    },
    tools::ToolDefinition,
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
    },
    /// A tool result, directly after the response proposing its call.
    ToolResult {
        /// Source entry.
        entry_id: EntryId,
        /// Who produced the result: an executor, the runtime, or a user who
        /// resolved an uncertain invocation. Adapters must let the model tell
        /// a user's statement from real tool output; how each provider labels
        /// it belongs to its adapter. Equal to `result.metadata.provenance`.
        provenance: Provenance,
        /// The stored result without its v2 `metadata.code`, which the model
        /// does not see: the text leads with the code. One result therefore
        /// projects and hashes alike whether it was written as v1 or v2.
        #[serde(serialize_with = "without_code")]
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

/// Serialize `result` in its pre-v2 shape, without `metadata.code`, so v2
/// added nothing to the projection's bytes. Destructuring both types in full
/// makes a new stored field a compile error here rather than a silent change.
fn without_code<S: Serializer>(result: &ToolResult, serializer: S) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Metadata<'a> {
        exit_code: &'a Option<i32>,
        duration_ms: &'a Option<u64>,
        output_truncated: bool,
        provenance: Provenance,
        artifacts: &'a [ArtifactRef],
    }
    #[derive(Serialize)]
    struct Projected<'a> {
        call: &'a CallRef,
        invocation_id: &'a Option<InvocationId>,
        status: ToolStatus,
        content: &'a [ContentBlock],
        metadata: Metadata<'a>,
    }
    let ToolResult {
        call,
        invocation_id,
        status,
        content,
        metadata,
    } = result;
    let ToolMetadata {
        exit_code,
        duration_ms,
        output_truncated,
        provenance,
        artifacts,
        code: _,
    } = metadata;
    Projected {
        call,
        invocation_id,
        status: *status,
        content,
        metadata: Metadata {
            exit_code,
            duration_ms,
            output_truncated: *output_truncated,
            provenance: *provenance,
            artifacts,
        },
    }
    .serialize(serializer)
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

/// SHA-256 of canonical `items`, `model`, `tools`, `options` and version.
/// Continuation, response identity and deadline are deliberately not hashed.
///
/// Canonical means `serde_json`'s compact output of these types: struct fields
/// in declared order, enum tags first, map keys (tool-call arguments) sorted
/// bytewise, no insignificant whitespace. Map keys are sorted because
/// `serde_json`'s `preserve_order` feature is off, which a test enforces.
/// Changing any item type changes these bytes, so it must bump
/// [`PROJECTION_VERSION`]. Responses, requests and the report are not hashed.
pub fn fingerprint(input: &ModelInput) -> Digest {
    fingerprint_with_version(PROJECTION_VERSION, input)
}

pub(super) fn fingerprint_with_version(version: u32, input: &ModelInput) -> Digest {
    #[derive(Serialize)]
    struct Canonical<'a> {
        items: &'a [ContextItem],
        model: &'a ModelRef,
        tools: &'a [ToolDefinition],
        options: &'a InferenceOptions,
        projection_version: u32,
    }
    let mut hasher = Hashing(Sha256::new());
    serde_json::to_writer(
        &mut hasher,
        &Canonical {
            items: &input.items,
            model: &input.model,
            tools: &input.tools,
            options: &input.options,
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
pub(super) fn canonical_bytes(input: &ModelInput) -> Vec<u8> {
    #[derive(Serialize)]
    struct Canonical<'a> {
        items: &'a [ContextItem],
        model: &'a ModelRef,
        tools: &'a [ToolDefinition],
        options: &'a InferenceOptions,
        projection_version: u32,
    }
    serde_json::to_vec(&Canonical {
        items: &input.items,
        model: &input.model,
        tools: &input.tools,
        options: &input.options,
        projection_version: PROJECTION_VERSION,
    })
    .unwrap()
}
