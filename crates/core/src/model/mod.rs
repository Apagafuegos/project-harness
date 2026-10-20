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

use futures_core::Stream;
use tokio_util::sync::CancellationToken;

use crate::session::{FinishReason, ModelError, ModelRef, ResponseId, Usage};

pub mod scripted;

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
}

/// The logical model input. Step 5's context builder adds the projected
/// instructions, conversation items, tools, options and continuation; until
/// then a request names only the model.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInput {
    /// Provider instance and model.
    pub model: ModelRef,
}

/// Caller-owned controls for one stream.
#[derive(Clone, Debug, Default)]
pub struct OperationControl {
    /// Cancelled when the runtime asks the stream to stop. The stream should
    /// end promptly, ideally with `Finished { outcome: Cancelled }`; the runtime
    /// keeps polling for that confirmation until its grace period ends, then
    /// drops the stream. Dropping also cancels this token.
    pub cancel: CancellationToken,
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
#[derive(Clone, Debug, PartialEq)]
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
    },
}
