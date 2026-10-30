//! The Anthropic Messages adapter.
//!
//! One wire format: `POST https://api.anthropic.com/v1/messages` with
//! `ANTHROPIC_API_KEY` sent as `x-api-key` (M4 decision 13: no subscription
//! login). Requests are stateless — the API keeps nothing between calls, so
//! thinking continuity travels as owner-scoped signed `ProviderState`.
//!
//! The adapter is a pure mapping plus the shared transport: no retry, no hidden
//! state and no task of its own.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use futures_util::StreamExt;
use harness_core::{
    model::{ModelEventStream, ModelProvider, ModelRequest, OperationControl, ProviderEvent},
    session::{ModelError, ModelErrorCode},
};
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};

use crate::{
    credentials::{CredentialScheme, CredentialSource, EnvApiKey},
    error::TransportError,
    sse::{self, SseConfig},
    transport::{Transport, TransportConfig},
};

mod events;
pub mod models;
mod preserved;
mod request;
#[cfg(test)]
mod tests;

pub use models::{LIVE_CHECK_MODEL, capabilities, catalog, extend_catalog, info};

/// Native state schema for one replayed thinking block.
pub const STATE_SCHEMA: &str = "anthropic.messages.thinking.v1";

/// Endpoint of the Messages API.
pub const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// API version sent as `anthropic-version`. The version is required on every
/// request; this is the current one from the official versioning reference.
pub const API_VERSION: &str = "2026-10-21";

/// Debug counters for events the adapter did not map. Never content.
#[derive(Debug, Default)]
pub struct Counters {
    unknown_events: AtomicU64,
    ignored_events: AtomicU64,
}

impl Counters {
    /// Event or delta types with no normalized form.
    pub fn unknown_events(&self) -> u64 {
        self.unknown_events.load(Ordering::Relaxed)
    }

    /// Known types deliberately mapped to nothing, such as `ping` and raw
    /// thinking deltas.
    pub fn ignored_events(&self) -> u64 {
        self.ignored_events.load(Ordering::Relaxed)
    }
}

/// Configured Messages adapter. Construct one per provider instance.
pub struct AnthropicMessages {
    credential: Arc<dyn CredentialSource>,
    transport: Transport,
    sse: SseConfig,
    endpoint: String,
    counters: Arc<Counters>,
}

impl std::fmt::Debug for AnthropicMessages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicMessages")
            .field("endpoint", &self.endpoint)
            .field("credential", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl AnthropicMessages {
    /// Build an instance over `credential` with the default transport.
    pub fn new(credential: Arc<dyn CredentialSource>) -> Result<Self, ModelError> {
        Ok(Self::with_transport(
            credential,
            Transport::new(TransportConfig::default()).map_err(|error| error.model_error())?,
        ))
    }

    /// Build an instance over an explicit transport, for a proxy or fixtures.
    pub fn with_transport(credential: Arc<dyn CredentialSource>, transport: Transport) -> Self {
        Self {
            credential,
            transport,
            sse: SseConfig::default(),
            endpoint: ENDPOINT.to_owned(),
            counters: Arc::new(Counters::default()),
        }
    }

    /// Read `ANTHROPIC_API_KEY` now, sent as `x-api-key`.
    pub fn from_env() -> Result<Self, ModelError> {
        Self::new(Arc::new(EnvApiKey::with_scheme(
            "ANTHROPIC_API_KEY",
            CredentialScheme::ApiKeyHeader,
        )?))
    }

    /// Replace the endpoint. Intended for loopback fixtures, never for routine
    /// configuration; the transport still enforces its own URL policy.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Debug counters for this instance's streams.
    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// Headers every request carries, beyond the credential.
    ///
    /// `replays_thinking` adds the preserved-thinking beta header, which is only
    /// legal together with the `thinking.block_binding` control that `build`
    /// puts in the body for exactly that case.
    fn headers(replays_thinking: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert("anthropic-version", HeaderValue::from_static(API_VERSION));
        // No other beta header: adaptive thinking, `output_config.effort`,
        // `service_tier` and tool use are part of the current stable API
        // version. The preserved-thinking control is the one exception, and
        // only on a request that replays signed thinking blocks.
        if replays_thinking {
            headers.insert("anthropic-beta", HeaderValue::from_static(preserved::BETA));
        }
        headers
    }
}

impl ModelProvider for AnthropicMessages {
    fn stream(&self, request: ModelRequest, control: OperationControl) -> ModelEventStream {
        let credential_source = self.credential.clone();
        let transport = self.transport.clone();
        let config = self.sse.clone();
        let endpoint = self.endpoint.clone();
        let counters = self.counters.clone();
        Box::pin(async_stream::stream! {
            // Setup failures are terminal events, never early errors.
            let credential = match credential_source.credential() {
                Ok(credential) => credential,
                Err(error) => {
                    yield events::failed(error);
                    return;
                }
            };
            let reads = match request::collect(&request.input, &request.artifacts).await {
                Ok(reads) => reads,
                Err(error) => {
                    yield events::failed(error);
                    return;
                }
            };
            let body = match request::build(&request.input, &reads) {
                Ok(body) => body,
                Err(error) => {
                    yield events::failed(error);
                    return;
                }
            };
            // The header follows the body: both are chosen by one predicate, so
            // `block_binding` can never travel without the beta that allows it.
            let credential = credential.with_extra_headers(Self::headers(
                request::replays_thinking(&body),
            ));
            let setup = {
                let transport = transport.clone();
                let endpoint = endpoint.clone();
                let body = body.clone();
                let setup_control = control.clone();
                async move {
                    transport
                        .post(&endpoint, &body, credential, setup_control)
                        .await
                }
            };
            let mut stream = sse::read(setup, config, control.clone());
            let mut mapper = events::Mapper::new(counters);
            while let Some(item) = stream.next().await {
                let mapped = match item {
                    Ok(event) => mapper.push(&event),
                    Err(error) => {
                        drop(stream);
                        yield transport_terminal(error);
                        return;
                    }
                };
                match mapped {
                    Ok(emitted) => {
                        if mapper.finished() {
                            // The consumer may never poll again after Finished.
                            // Release the SSE reader before yielding it.
                            drop(stream);
                            for event in emitted {
                                yield event;
                            }
                            return;
                        }
                        for event in emitted {
                            yield event;
                        }
                    }
                    Err(error) => {
                        drop(stream);
                        yield events::failed(error);
                        return;
                    }
                }
            }
            drop(stream);
            match mapper.finish_eof() {
                Ok(emitted) => {
                    for event in emitted {
                        yield event;
                    }
                }
                Err(error) => yield events::failed(error),
            }
        })
    }
}

/// Turn a transport failure into the single terminal event it deserves.
fn transport_terminal(error: TransportError) -> ProviderEvent {
    if error.is_stopped() {
        return finished_cancelled();
    }
    let model = refine_http(error.model_error(), error.body());
    events::failed(model)
}

fn finished_cancelled() -> ProviderEvent {
    ProviderEvent::Finished {
        outcome: harness_core::model::ProviderOutcome::Cancelled,
        finish_reason: None,
        error: None,
        continuation: None,
    }
}

/// Refine an HTTP failure from its bounded body without echoing it.
///
/// Local transport/parser/bound errors carry no HTTP status or response body;
/// refining those with an empty provider error would hide their diagnosis.
fn refine_http(mut model: ModelError, body: &[u8]) -> ModelError {
    let has_http_status = model
        .provider_code
        .as_deref()
        .is_some_and(|code| code.starts_with("http_"));
    if !has_http_status && body.is_empty() {
        return model;
    }
    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok();
    let kind = parsed
        .as_ref()
        .and_then(|value| value.pointer("/error/type"))
        .and_then(serde_json::Value::as_str);
    let message = parsed
        .as_ref()
        .and_then(|value| value.pointer("/error/message"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let precedent = matches!(
        model.code,
        ModelErrorCode::Authentication
            | ModelErrorCode::RateLimit
            | ModelErrorCode::ContextOverflow
    );
    if !precedent {
        model.code = classify(kind, message);
    }
    if let Some(kind) = kind {
        model.provider_code = Some(kind.to_owned());
    }
    model.message = safe_message(model.code);
    model
}

/// Anthropic error type to normalized category.
pub(crate) fn classify(kind: Option<&str>, message: &str) -> ModelErrorCode {
    let message = message.to_ascii_lowercase();
    let context = message.contains("context")
        || message.contains("prompt is too long")
        || message.contains("too many tokens")
        || message.contains("maximum context");
    match kind.unwrap_or_default() {
        "authentication_error" | "permission_error" => ModelErrorCode::Authentication,
        "rate_limit_error" => ModelErrorCode::RateLimit,
        "invalid_request_error" | "request_too_large" => {
            if context {
                ModelErrorCode::ContextOverflow
            } else {
                ModelErrorCode::InvalidRequest
            }
        }
        "overloaded_error" | "api_error" | "not_found_error" => ModelErrorCode::ProviderFailure,
        _ => ModelErrorCode::ProviderFailure,
    }
}

/// Fixed diagnostic per category; provider text is never echoed.
pub(crate) fn safe_message(code: ModelErrorCode) -> String {
    match code {
        ModelErrorCode::Authentication => "provider authentication failed",
        ModelErrorCode::RateLimit => "provider rate limit reached; start a new turn",
        ModelErrorCode::ContextOverflow => "the request exceeded the model's context window",
        ModelErrorCode::InvalidRequest => "the provider rejected the request",
        ModelErrorCode::UnsupportedCapability => "the provider does not support the request",
        ModelErrorCode::Transport => "the connection to the provider failed",
        ModelErrorCode::MalformedStream => "the provider stream was malformed",
        ModelErrorCode::LimitExceeded => "a provider bound was exceeded",
        ModelErrorCode::ProviderFailure => "the provider failed the response",
    }
    .to_owned()
}
