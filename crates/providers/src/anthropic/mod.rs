//! The Anthropic Messages adapter, on two endpoint profiles.
//!
//! One wire format, `POST /v1/messages`: the Anthropic profile targets
//! `https://api.anthropic.com/v1/messages` with `ANTHROPIC_API_KEY` sent as
//! `x-api-key` (M4 decision 13: no subscription login), and the DeepSeek profile
//! targets `https://api.deepseek.com/anthropic/v1/messages` with
//! `DEEPSEEK_API_KEY` (M6 decision 10). Requests are stateless — neither API
//! keeps anything between calls — so thinking continuity travels as
//! owner-scoped `ProviderState` under the profile's own schema.
//!
//! The adapter is a pure mapping plus the shared transport: no retry, no hidden
//! state and no task of its own. A [`Profile`] is data plus small mapping hooks;
//! there is no second adapter and no provider branch outside this crate.

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

pub use models::{
    DEEPSEEK_LIVE_CHECK_MODEL, DEFAULT_MAX_OUTPUT_TOKENS, LIVE_CHECK_MODEL, capabilities, catalog,
    extend_catalog, info,
};

/// Native state schema for one replayed Anthropic thinking block.
pub const STATE_SCHEMA: &str = "anthropic.messages.thinking.v1";

/// Native state schema for one replayed DeepSeek thinking block.
///
/// Two schemas, not one: a block written by one profile must be impossible to
/// replay through the other, and the schema is the adapter-side half of that
/// guarantee (the block's owner is the other half).
pub const DEEPSEEK_STATE_SCHEMA: &str = "deepseek.messages.thinking.v1";

/// Endpoint of the Anthropic Messages API.
pub const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// Endpoint of DeepSeek's Anthropic-compatible Messages API.
pub const DEEPSEEK_ENDPOINT: &str = "https://api.deepseek.com/anthropic/v1/messages";

/// API version sent as `anthropic-version`. The version is required on every
/// Anthropic request; this is the current one from the official versioning
/// reference. DeepSeek documents the header as ignored, so its profile omits it.
pub const API_VERSION: &str = "2026-10-21";

/// Which Messages-compatible backend one instance speaks.
///
/// DeepSeek serves an Anthropic-compatible Messages API, so one adapter speaks
/// both unchanged. What differs is data: the endpoint, the credential variable,
/// the headers, which documented request controls the backend accepts, whether a
/// thinking block carries a signature, and the owner/schema of native state.
/// `request` and `events` read these hooks and never branch on a provider name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Anthropic's Messages API, authenticated with `ANTHROPIC_API_KEY`.
    Anthropic,
    /// DeepSeek's Anthropic-compatible Messages API, authenticated with
    /// `DEEPSEEK_API_KEY`.
    DeepSeek,
}

impl Profile {
    /// The one endpoint this profile posts to.
    pub fn endpoint(self) -> &'static str {
        match self {
            Self::Anthropic => ENDPOINT,
            Self::DeepSeek => DEEPSEEK_ENDPOINT,
        }
    }

    /// The environment variable holding this profile's API key.
    pub fn credential_variable(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::DeepSeek => "DEEPSEEK_API_KEY",
        }
    }

    /// How the key is sent. Both backends accept `x-api-key`, and DeepSeek also
    /// documents `Authorization: Bearer`; the adapter keeps the header form on
    /// both profiles so one credential scheme covers the whole wire format.
    pub fn credential_scheme(self) -> CredentialScheme {
        CredentialScheme::ApiKeyHeader
    }

    /// Human-readable adapter name, used only in fixed diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::DeepSeek => "DeepSeek",
        }
    }

    /// Native-state schema of one replayed thinking block.
    pub fn state_schema(self) -> &'static str {
        match self {
            Self::Anthropic => STATE_SCHEMA,
            Self::DeepSeek => DEEPSEEK_STATE_SCHEMA,
        }
    }

    /// `thinking.type` sent when a reasoning effort is configured.
    ///
    /// Anthropic's current family accepts only adaptive thinking. DeepSeek
    /// documents `enabled` and `disabled` (with `adaptive` treated as enabled)
    /// and a default of thinking on, so the profile asks for `enabled`
    /// explicitly.
    pub fn thinking_type(self) -> &'static str {
        match self {
            Self::Anthropic => "adaptive",
            Self::DeepSeek => "enabled",
        }
    }

    /// Whether a request that replays a thinking block asks the API to drop a
    /// block whose prefix changed (`thinking.block_binding`, with its beta).
    ///
    /// Anthropic documents the control for exactly this; DeepSeek documents no
    /// such control, ignores `anthropic-beta` on `/messages`, and needs nothing
    /// from it, so its profile sends neither.
    pub fn drops_failing_thinking_blocks(self) -> bool {
        matches!(self, Self::Anthropic)
    }

    /// Whether a `thinking` block must carry a non-empty `signature`.
    ///
    /// Anthropic requires it. DeepSeek's compatibility page lists a `thinking`
    /// content block as supported and never mentions a signature, so the profile
    /// accepts a block without one instead of rejecting a documented shape.
    pub fn requires_thinking_signature(self) -> bool {
        matches!(self, Self::Anthropic)
    }

    /// Whether `redacted_thinking` blocks are part of this backend's vocabulary.
    /// DeepSeek documents them as not supported, so they are neither stored as
    /// state nor replayed.
    pub fn supports_redacted_thinking(self) -> bool {
        matches!(self, Self::Anthropic)
    }

    /// Fixed diagnostic for an in-stream provider error. Provider text is never
    /// echoed, so each profile carries its own constant.
    pub fn error_message(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic request failed",
            Self::DeepSeek => "DeepSeek request failed",
        }
    }

    /// Headers every request carries, beyond the credential.
    ///
    /// Anthropic requires `anthropic-version`, and the preserved-thinking beta
    /// is legal only together with the `thinking.block_binding` control that
    /// `request::build` puts in the body for exactly that case. DeepSeek ignores
    /// both headers and documents no binding control, so its profile sends
    /// neither.
    fn headers(self, replays_thinking: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if self == Self::Anthropic {
            headers.insert("anthropic-version", HeaderValue::from_static(API_VERSION));
        }
        if replays_thinking && self.drops_failing_thinking_blocks() {
            headers.insert("anthropic-beta", HeaderValue::from_static(preserved::BETA));
        }
        headers
    }
}

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
    profile: Profile,
    credential: Arc<dyn CredentialSource>,
    transport: Transport,
    sse: SseConfig,
    endpoint: String,
    counters: Arc<Counters>,
}

impl std::fmt::Debug for AnthropicMessages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicMessages")
            .field("profile", &self.profile)
            .field("endpoint", &self.endpoint)
            .field("credential", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl AnthropicMessages {
    /// Build an Anthropic-profile instance over `credential` with the default
    /// transport.
    pub fn new(credential: Arc<dyn CredentialSource>) -> Result<Self, ModelError> {
        Ok(Self::with_transport(
            credential,
            Transport::new(TransportConfig::default()).map_err(|error| error.model_error())?,
        ))
    }

    /// Build an Anthropic-profile instance over an explicit transport, for a
    /// proxy or fixtures.
    pub fn with_transport(credential: Arc<dyn CredentialSource>, transport: Transport) -> Self {
        Self::with_profile(Profile::Anthropic, credential, transport)
    }

    /// Build an instance of `profile` over an explicit transport.
    pub fn with_profile(
        profile: Profile,
        credential: Arc<dyn CredentialSource>,
        transport: Transport,
    ) -> Self {
        Self {
            profile,
            credential,
            transport,
            sse: SseConfig::default(),
            endpoint: profile.endpoint().to_owned(),
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

    /// Read `DEEPSEEK_API_KEY` now, sent as `x-api-key`.
    pub fn deepseek_from_env() -> Result<Self, ModelError> {
        let profile = Profile::DeepSeek;
        let credential =
            EnvApiKey::with_scheme(profile.credential_variable(), profile.credential_scheme())?;
        Ok(Self::with_profile(
            profile,
            Arc::new(credential),
            Transport::new(TransportConfig::default()).map_err(|error| error.model_error())?,
        ))
    }

    /// Which endpoint profile this instance speaks.
    pub fn profile(&self) -> Profile {
        self.profile
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

    /// Headers every request carries, beyond the credential, for `profile`.
    ///
    /// `replays_thinking` adds the preserved-thinking beta header on the profile
    /// that documents it, which is only legal together with the
    /// `thinking.block_binding` control that `build` puts in the body for exactly
    /// that case.
    fn headers(profile: Profile, replays_thinking: bool) -> HeaderMap {
        profile.headers(replays_thinking)
    }
}

impl ModelProvider for AnthropicMessages {
    fn stream(&self, request: ModelRequest, control: OperationControl) -> ModelEventStream {
        let profile = self.profile;
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
            let reads = match request::collect(profile, &request.input, &request.artifacts).await {
                Ok(reads) => reads,
                Err(error) => {
                    yield events::failed(error);
                    return;
                }
            };
            let body = match request::build(profile, &request.input, &reads) {
                Ok(body) => body,
                Err(error) => {
                    yield events::failed(error);
                    return;
                }
            };
            // The header follows the body: both are chosen by one predicate, so
            // `block_binding` can never travel without the beta that allows it.
            let credential = credential.with_extra_headers(AnthropicMessages::headers(
                profile,
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
            let mut mapper = events::Mapper::new(counters, profile);
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
    if let Some(kind) = kind.and_then(events::safe_provider_code) {
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
