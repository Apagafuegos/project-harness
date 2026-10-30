//! One `ModelProvider` over several configured provider instances.
//!
//! A session's `ModelRef` names the provider instance, not the adapter type
//! (contract 1). The runtime driver holds exactly one
//! `Arc<dyn ModelProvider>`, so a session that may switch between adapters
//! registers each configured instance here and lets this router pick the
//! adapter for every request:
//!
//! ```rust,ignore
//! let provider = Arc::new(
//!     Router::new()
//!         .with("openai-codex", Arc::new(openai))
//!         .with("anthropic", Arc::new(anthropic)),
//! );
//! ```
//!
//! Dispatch is by `ModelRequest.input.model.provider_instance` only. Never by
//! model name, adapter type, request contents or a guess, because the instance
//! is what carries an account, an endpoint and the ownership of native state.
//! An unregistered instance is a request-level failure: no adapter is asked and
//! nothing reaches the network.
//!
//! The router adds no state, retry, queue or task. A registered adapter gets
//! the request and the caller's [`OperationControl`] unchanged, so
//! cancellation, deadlines, backpressure and drop-release behave exactly as
//! they do without it. It also does not translate provider state: core already
//! scopes every native block to its owner, and an adapter replays only its own.

use std::{collections::BTreeMap, sync::Arc};

use harness_core::{
    model::{
        ModelEventStream, ModelProvider, ModelRequest, OperationControl, ProviderEvent,
        ProviderOutcome,
    },
    session::{ModelError, ModelErrorCode},
};

/// A `ModelProvider` that dispatches by the request's provider instance.
#[derive(Default)]
pub struct Router {
    providers: BTreeMap<String, Arc<dyn ModelProvider>>,
}

impl Router {
    /// An empty router. Every request fails with `invalid_request` until an
    /// instance is registered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `provider` under `instance`, replacing any previous entry.
    ///
    /// An empty instance name is refused: no `ModelRef` may name it, so the
    /// entry could never be reached.
    pub fn register(
        &mut self,
        instance: impl Into<String>,
        provider: Arc<dyn ModelProvider>,
    ) -> Option<Arc<dyn ModelProvider>> {
        let instance = instance.into();
        if instance.is_empty() {
            return None;
        }
        self.providers.insert(instance, provider)
    }

    /// Builder form of [`register`](Self::register).
    pub fn with(mut self, instance: impl Into<String>, provider: Arc<dyn ModelProvider>) -> Self {
        self.register(instance, provider);
        self
    }

    /// Whether `instance` is registered. Callers validating a switch target use
    /// this instead of discovering the gap as a failed inference.
    pub fn contains(&self, instance: &str) -> bool {
        self.providers.contains_key(instance)
    }

    /// Registered instance names, in sorted order.
    pub fn instances(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Instance names are configuration, not credentials; adapters keep their
        // own redacting `Debug`.
        f.debug_struct("Router")
            .field("instances", &self.providers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ModelProvider for Router {
    fn stream(&self, request: ModelRequest, control: OperationControl) -> ModelEventStream {
        match self.providers.get(&request.input.model.provider_instance) {
            // The delegate owns the whole lifecycle from here: same request,
            // same control, no wrapper task, so drop and cancellation release
            // exactly what the adapter would have released.
            Some(provider) => provider.stream(request, control),
            None => Box::pin(async_stream::stream! {
                yield unknown_instance();
            }),
        }
    }
}

/// The single terminal event for an unregistered instance. The instance name is
/// configuration, but a fixed diagnostic keeps provider text, URLs and every
/// other request detail out of a persisted failure.
fn unknown_instance() -> ProviderEvent {
    ProviderEvent::Finished {
        outcome: ProviderOutcome::Failed,
        finish_reason: None,
        error: Some(ModelError {
            code: ModelErrorCode::InvalidRequest,
            message: "no provider instance is registered for this model".into(),
            provider_code: None,
            retry_after_ms: None,
        }),
        continuation: None,
    }
}
