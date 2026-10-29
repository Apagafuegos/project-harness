//! A deterministic provider that replays scripts, for tests and demos.
//!
//! Each [`ModelProvider::stream`] call takes the next [`Script`] in order. The
//! stream is pull-driven: every poll runs steps until one yields an event,
//! waits, or the script ends (EOF). It never reads a clock and has no internal
//! queue, so pacing comes only from [`Step::Wait`], [`Step::AwaitCancel`] and
//! [`Step::Stall`]. Events are yielded exactly as written, including invalid
//! sequences, so scripts can exercise the core's stream validation.
//!
//! ```
//! use harness_core::model::{scripted::*, *};
//! use harness_core::session::{FinishReason, ModelRef};
//!
//! // Text, then a cancellation confirmed as soon as it is requested.
//! let provider = ScriptedProvider::new([Script::new([
//!     Step::Event(ProviderEvent::Started),
//!     Step::text(0, "partial"),
//!     Step::AwaitCancel,
//!     Step::cancelled(),
//! ])]);
//! // A second inference that completes normally.
//! provider.push(Script::new([
//!     Step::Event(ProviderEvent::Started),
//!     Step::text(0, "done"),
//!     Step::completed(FinishReason::Stop),
//! ]));
//! assert_eq!(provider.remaining(), 2);
//! ```

use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, ready},
};

use futures_core::Stream;
use tokio::sync::oneshot;
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

use super::{
    ModelEventStream, ModelProvider, ModelRequest, NativeContinuation, OperationControl,
    ProviderEvent, ProviderOutcome,
};
use crate::session::{FinishReason, ModelError, ModelErrorCode};

/// One step of a script.
#[derive(Debug)]
pub enum Step {
    /// Yield this event on the next poll.
    Event(ProviderEvent),
    /// Stay pending until the sender fires or is dropped. Ignores cancellation,
    /// so a test controls exactly when the stream continues.
    Wait(oneshot::Receiver<()>),
    /// Stay pending until the operation's cancellation token is cancelled.
    AwaitCancel,
    /// Never continue. Only dropping the stream ends it, like a provider that
    /// ignores cancellation.
    Stall,
}

impl Step {
    /// A text delta for `block_index`.
    pub fn text(block_index: u32, text: impl Into<String>) -> Self {
        Self::Event(ProviderEvent::TextDelta {
            block_index,
            text: text.into(),
        })
    }

    /// One complete native block. The adapter cannot supply an owner.
    pub fn provider_state(block_index: u32, schema: impl Into<String>, payload: Vec<u8>) -> Self {
        Self::Event(ProviderEvent::ProviderState {
            block_index,
            schema: schema.into(),
            payload,
        })
    }

    /// Completed response with a native continuation payload.
    pub fn completed_with_continuation(
        reason: FinishReason,
        continuation: NativeContinuation,
    ) -> Self {
        Self::Event(ProviderEvent::Finished {
            outcome: ProviderOutcome::Completed,
            finish_reason: Some(reason),
            error: None,
            continuation: Some(continuation),
        })
    }

    /// A successful `Finished` with `reason`.
    pub fn completed(reason: FinishReason) -> Self {
        Self::Event(ProviderEvent::Finished {
            outcome: ProviderOutcome::Completed,
            finish_reason: Some(reason),
            error: None,
            continuation: None,
        })
    }

    /// A failed `Finished` carrying `error`.
    pub fn failed(error: ModelError) -> Self {
        Self::Event(ProviderEvent::Finished {
            outcome: ProviderOutcome::Failed,
            finish_reason: None,
            error: Some(error),
            continuation: None,
        })
    }

    /// The provider's confirmation that it cancelled the response.
    pub fn cancelled() -> Self {
        Self::Event(ProviderEvent::Finished {
            outcome: ProviderOutcome::Cancelled,
            finish_reason: None,
            error: None,
            continuation: None,
        })
    }
}

/// The ordered steps of one stream. EOF follows the last step.
#[derive(Debug, Default)]
pub struct Script(VecDeque<Step>);

impl Script {
    /// A script of `steps`, run in order.
    pub fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self(steps.into_iter().collect())
    }
}

/// What happened to one stream, as observed so far.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamRecord {
    /// The request the stream was opened for.
    pub request: ModelRequest,
    /// Deadline supplied by the driver, if configured.
    pub deadline: Option<tokio::time::Instant>,
    /// Events yielded so far.
    pub yielded: usize,
    /// Whether the operation's token has been cancelled.
    pub cancelled: bool,
    /// Whether the consumer has dropped the stream.
    pub dropped: bool,
}

struct Shared {
    request: ModelRequest,
    deadline: Option<tokio::time::Instant>,
    cancel: CancellationToken,
    yielded: usize,
    dropped: bool,
}

/// Replays queued scripts, one per stream. With no script left, a stream
/// reports a setup failure (`provider_failure`, `no_script`) and ends.
#[derive(Default)]
pub struct ScriptedProvider {
    scripts: Mutex<VecDeque<Script>>,
    streams: Mutex<Vec<Arc<Mutex<Shared>>>>,
}

impl ScriptedProvider {
    /// A provider with `scripts` queued in order.
    pub fn new(scripts: impl IntoIterator<Item = Script>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            streams: Mutex::default(),
        }
    }

    /// Queue another script after those already queued.
    pub fn push(&self, script: Script) {
        self.scripts.lock().unwrap().push_back(script);
    }

    /// Scripts not yet taken by a stream.
    pub fn remaining(&self) -> usize {
        self.scripts.lock().unwrap().len()
    }

    /// Every stream opened so far, in order.
    pub fn streams(&self) -> Vec<StreamRecord> {
        self.streams
            .lock()
            .unwrap()
            .iter()
            .map(|shared| {
                let shared = shared.lock().unwrap();
                StreamRecord {
                    request: shared.request.clone(),
                    deadline: shared.deadline,
                    yielded: shared.yielded,
                    cancelled: shared.cancel.is_cancelled(),
                    dropped: shared.dropped,
                }
            })
            .collect()
    }
}

impl ModelProvider for ScriptedProvider {
    fn stream(&self, request: ModelRequest, control: OperationControl) -> ModelEventStream {
        let script = self.scripts.lock().unwrap().pop_front().unwrap_or_else(|| {
            Script::new([Step::failed(ModelError {
                code: ModelErrorCode::ProviderFailure,
                message: "no_script: the scripted provider has no script left".into(),
                provider_code: None,
                retry_after_ms: None,
            })])
        });
        let shared = Arc::new(Mutex::new(Shared {
            request,
            deadline: control.deadline,
            cancel: control.cancel.clone(),
            yielded: 0,
            dropped: false,
        }));
        self.streams.lock().unwrap().push(shared.clone());
        Box::pin(ScriptStream {
            steps: script.0,
            cancel: control.cancel,
            cancelled: None,
            shared,
        })
    }
}

struct ScriptStream {
    steps: VecDeque<Step>,
    cancel: CancellationToken,
    cancelled: Option<Pin<Box<WaitForCancellationFutureOwned>>>,
    shared: Arc<Mutex<Shared>>,
}

impl Stream for ScriptStream {
    type Item = ProviderEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<ProviderEvent>> {
        let Self {
            steps,
            cancel,
            cancelled,
            shared,
        } = self.get_mut();
        loop {
            match steps.front_mut() {
                None => return Poll::Ready(None),
                Some(Step::Event(_)) => {
                    let Some(Step::Event(event)) = steps.pop_front() else {
                        unreachable!("front is an event")
                    };
                    shared.lock().unwrap().yielded += 1;
                    return Poll::Ready(Some(event));
                }
                Some(Step::Wait(release)) => {
                    // A dropped sender also releases, so a test cannot hang on it.
                    let _ = ready!(Pin::new(release).poll(cx));
                }
                Some(Step::AwaitCancel) => {
                    let wait =
                        cancelled.get_or_insert_with(|| Box::pin(cancel.clone().cancelled_owned()));
                    ready!(wait.as_mut().poll(cx));
                    *cancelled = None;
                }
                Some(Step::Stall) => return Poll::Pending,
            }
            steps.pop_front();
        }
    }
}

impl Drop for ScriptStream {
    fn drop(&mut self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.dropped = true;
        }
    }
}

#[cfg(test)]
mod tests;
