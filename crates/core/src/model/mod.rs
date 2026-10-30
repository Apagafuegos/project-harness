//! Contract 1: the model provider seam.
//!
//! A [`ModelProvider`] turns one [`ModelRequest`] into an owned, pull-driven
//! [`ModelEventStream`] of normalized [`ProviderEvent`]s. The runtime driver
//! polls it and feeds each event to the pure core, which validates the
//! sequence; an adapter is never trusted to be well formed. Setup failures
//! are terminal events, not errors from `stream`.
//!
//! [`scripted::ScriptedProvider`] is the deterministic mock. Native adapters
//! arrive in Milestone 4.

use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use futures_core::Stream;
use tokio_util::sync::CancellationToken;

use crate::{
    context::ContextItem,
    session::{FinishReason, ModelError, ModelRef, ProviderContinuation, ResponseId, Usage},
    tools::ToolDefinition,
};

mod artifacts;
#[cfg(any(test, feature = "conformance"))]
pub mod conformance;
#[cfg(test)]
mod redaction_tests;
pub mod scripted;
pub use artifacts::{ArtifactReadError, ArtifactSource};

/// An owned event stream. It does not borrow its provider and starts work only
/// when polled. Dropping it must cancel and release everything it started.
pub type ModelEventStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send + 'static>>;

/// Opens one inference. Object safe, so the driver holds `Arc<dyn ModelProvider>`.
pub trait ModelProvider: Send + Sync {
    /// Open a stream for `request`. Must not block; do the work when polled.
    fn stream(&self, request: ModelRequest, control: OperationControl) -> ModelEventStream;
}

/// One inference: the identity the response will be journaled under, plus the
/// logical input.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRequest {
    /// Chosen by the runtime, never by the provider.
    pub response_id: ResponseId,
    /// What to infer from.
    pub input: ModelInput,
    /// Verified, bounded reads of artifacts referenced by this input only.
    pub artifacts: ArtifactSource,
}

/// The logical model input: the context builder's projection. Adapters
/// translate it to a provider's wire format, preserving the supplied definitions
/// and options or reporting unsupported required capabilities.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInput {
    /// Provider instance and model.
    pub model: ModelRef,
    /// Instructions, dialogue, tool results and summaries, in order.
    pub items: Vec<ContextItem>,
    /// Enabled, available definitions, in registry order.
    pub tools: Vec<ToolDefinition>,
    /// Validated configuration; provider namespaces are validated by the adapter.
    pub options: InferenceOptions,
    /// Native continuation proven valid for the current logical prefix.
    pub continuation: Option<ProviderContinuation>,
}

/// Required output budget and configured provider controls.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceOptions {
    /// Equal to the context builder's configured output reserve.
    pub max_output_tokens: u32,
    /// Optional provider reasoning control.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Namespaced options. Adapters must reject unsupported required values.
    pub provider_options: Map<String, Value>,
}

/// Portable reasoning-effort levels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// Low effort.
    Low,
    /// Medium effort.
    Medium,
    /// High effort.
    High,
}

/// Complete native continuation payload. Identity and owner are core-owned.
#[derive(Clone, PartialEq, Eq)]
pub struct NativeContinuation {
    /// Provider-defined replay schema.
    pub schema: String,
    /// Opaque sensitive bytes; never rendered as text.
    pub payload: Vec<u8>,
}

impl std::fmt::Debug for NativeContinuation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeContinuation")
            .field("schema", &self.schema)
            .field("payload", &Opaque(&self.payload))
            .finish()
    }
}

/// Debug rendering of opaque native bytes: their length only, never content.
struct Opaque<'a>(&'a [u8]);
impl std::fmt::Debug for Opaque<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[redacted; {} bytes]", self.0.len())
    }
}

/// Caller-owned controls for one stream.
#[derive(Clone, Debug, Default)]
pub struct OperationControl {
    /// Cancelled when the runtime asks the stream to stop. The stream should
    /// end promptly, ideally with `Finished { outcome: Cancelled }`; the runtime
    /// keeps polling for that confirmation until its grace period ends, then
    /// drops the stream. Dropping also cancels this token.
    pub cancel: CancellationToken,
    /// Absolute per-inference deadline, also enforced by the core.
    pub deadline: Option<tokio::time::Instant>,
}

/// Terminal outcome reported by a provider's `Finished` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderOutcome {
    /// The response completed with a finish reason.
    Completed,
    /// The provider failed; an error is required.
    Failed,
    /// The provider reports that it cancelled the response.
    Cancelled,
}

/// Normalized provider stream events (Contract 1). Blocks are identified by a
/// response-local `block_index`; the core issues each tool call's `CallId`
/// when it sees `ToolCallStarted`, so a provider cannot choose journal IDs.
/// Native state cannot claim another owner: the event has no owner field.
///
/// ```compile_fail
/// use harness_core::model::ProviderEvent;
/// use harness_core::session::ModelRef;
/// let forged = ProviderEvent::ProviderState {
///     block_index: 0, schema: "native.v1".into(), payload: vec![],
///     owner: ModelRef { provider_instance: "other".into(), model: "other".into() },
/// };
/// ```
#[derive(Clone, PartialEq)]
pub enum ProviderEvent {
    /// The response opened. At most once, before any content.
    Started,
    /// Append text to a text block.
    TextDelta {
        /// Response-local block.
        block_index: u32,
        /// Appended text.
        text: String,
    },
    /// Append to a provider-designated displayable reasoning summary.
    ReasoningSummaryDelta {
        /// Response-local block.
        block_index: u32,
        /// Appended text.
        text: String,
    },
    /// Open a tool-call block.
    ToolCallStarted {
        /// Response-local block.
        block_index: u32,
        /// Tool name; must be nonempty.
        name: String,
        /// Native call identity, retained for replay.
        provider_call_id: Option<String>,
    },
    /// Append an argument fragment. Fragments may be incomplete JSON.
    ToolArgumentsDelta {
        /// Response-local tool-call block.
        block_index: u32,
        /// Appended fragment.
        fragment: String,
    },
    /// All fragments arrived; they must form one JSON object.
    ToolCallReady {
        /// Response-local tool-call block.
        block_index: u32,
    },
    /// Introduce one complete native block at a fresh index.
    ProviderState {
        /// Response-local block, not previously used by any block kind.
        block_index: u32,
        /// Provider-defined replay schema.
        schema: String,
        /// Opaque sensitive bytes; the core publishes their artifact.
        payload: Vec<u8>,
    },
    /// Cumulative usage so far. Replaces, never adds to, earlier figures.
    UsageUpdated {
        /// Usage snapshot.
        usage: Usage,
    },
    /// The single terminal event.
    Finished {
        /// Terminal outcome.
        outcome: ProviderOutcome,
        /// Required exactly when the outcome is `Completed`.
        finish_reason: Option<FinishReason>,
        /// Required exactly when the outcome is `Failed`.
        error: Option<ModelError>,
        /// Native continuation, permitted only on a completed response.
        continuation: Option<NativeContinuation>,
    },
}

/// Field-for-field `Debug` that replaces opaque payload bytes with their length.
/// A derive would print native replay data; decision 16 forbids that.
impl std::fmt::Debug for ProviderEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Started => f.write_str("Started"),
            Self::TextDelta { block_index, text } => f
                .debug_struct("TextDelta")
                .field("block_index", block_index)
                .field("text", text)
                .finish(),
            Self::ReasoningSummaryDelta { block_index, text } => f
                .debug_struct("ReasoningSummaryDelta")
                .field("block_index", block_index)
                .field("text", text)
                .finish(),
            Self::ToolCallStarted {
                block_index,
                name,
                provider_call_id,
            } => f
                .debug_struct("ToolCallStarted")
                .field("block_index", block_index)
                .field("name", name)
                .field("provider_call_id", provider_call_id)
                .finish(),
            Self::ToolArgumentsDelta {
                block_index,
                fragment,
            } => f
                .debug_struct("ToolArgumentsDelta")
                .field("block_index", block_index)
                .field("fragment", fragment)
                .finish(),
            Self::ToolCallReady { block_index } => f
                .debug_struct("ToolCallReady")
                .field("block_index", block_index)
                .finish(),
            Self::ProviderState {
                block_index,
                schema,
                payload,
            } => f
                .debug_struct("ProviderState")
                .field("block_index", block_index)
                .field("schema", schema)
                .field("payload", &Opaque(payload))
                .finish(),
            Self::UsageUpdated { usage } => f
                .debug_struct("UsageUpdated")
                .field("usage", usage)
                .finish(),
            Self::Finished {
                outcome,
                finish_reason,
                error,
                continuation,
            } => f
                .debug_struct("Finished")
                .field("outcome", outcome)
                .field("finish_reason", finish_reason)
                .field("error", error)
                .field("continuation", continuation)
                .finish(),
        }
    }
}
