//! The OpenAI Responses adapter: one wire format, two endpoint profiles.
//!
//! `Platform` targets `https://api.openai.com/v1/responses` with
//! `OPENAI_API_KEY`. `ChatGptCodex` targets the ChatGPT Codex backend that the
//! official Codex CLI uses with a subscription login, and takes its endpoint,
//! headers and request constraints from that source
//! (`docs/decisions/0017-openai-adapter.md`).
//!
//! The adapter is a pure mapping plus the shared transport: no retry, no hidden
//! state and no task of its own. Requests are stateless (M4 decision 14):
//! `store: false`, no `previous_response_id`, and reasoning carried across turns
//! only as owner-scoped encrypted `ProviderState`.

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
    credentials::{CodexLogin, CredentialSource, EnvApiKey},
    error::TransportError,
    sse::{self, SseConfig},
    transport::{Transport, TransportConfig},
};

mod events;
pub mod models;
mod request;
#[cfg(test)]
mod tests;

pub use models::{LIVE_CHECK_MODEL, capabilities, catalog, info};

/// Native state schema for one replayed encrypted reasoning item.
pub const STATE_SCHEMA: &str = "openai.responses.reasoning.v1";

/// Which endpoint and request shape this instance speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// The platform Responses API, authenticated with an API key.
    Platform,
    /// The ChatGPT Codex backend, authenticated with the CLI's subscription login.
    ChatGptCodex,
}

impl Profile {
    /// The single endpoint this profile posts to.
    pub fn endpoint(self) -> &'static str {
        match self {
            Self::Platform => "https://api.openai.com/v1/responses",
            Self::ChatGptCodex => "https://chatgpt.com/backend-api/codex/responses",
        }
    }

    /// Whether instructions travel as a leading `developer` input item instead
    /// of the top-level `instructions` field. The Codex client does the former.
    pub fn instructions_as_input(self) -> bool {
        matches!(self, Self::ChatGptCodex)
    }

    /// Headers this profile adds beyond the bearer credential.
    fn headers(self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if self == Self::ChatGptCodex {
            // openai/codex `login/auth/default_client.rs`: DEFAULT_ORIGINATOR.
            headers.insert("originator", HeaderValue::from_static("codex_cli_rs"));
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
    /// Event types with no normalized form, such as hosted-tool activity.
    pub fn unknown_events(&self) -> u64 {
        self.unknown_events.load(Ordering::Relaxed)
    }

    /// Known event types deliberately mapped to nothing.
    pub fn ignored_events(&self) -> u64 {
        self.ignored_events.load(Ordering::Relaxed)
    }
}

/// Configured Responses adapter. Construct one per provider instance.
pub struct OpenAiResponses {
    profile: Profile,
    credential: Arc<dyn CredentialSource>,
    transport: Transport,
    sse: SseConfig,
    endpoint: String,
    counters: Arc<Counters>,
}

impl std::fmt::Debug for OpenAiResponses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiResponses")
            .field("profile", &self.profile)
            .field("endpoint", &self.endpoint)
            .field("credential", &"[redacted]")
            .finish_non_exhaustive()
    }
}

impl OpenAiResponses {
    /// Build an instance over `credential` with the default transport.
    pub fn new(
        profile: Profile,
        credential: Arc<dyn CredentialSource>,
    ) -> Result<Self, ModelError> {
        Ok(Self::with_transport(
            profile,
            credential,
            Transport::new(TransportConfig::default()).map_err(|error| error.model_error())?,
        ))
    }

    /// Build an instance over an explicit transport, for a proxy or fixtures.
    pub fn with_transport(
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

    /// The platform profile, reading `OPENAI_API_KEY` now.
    pub fn platform_from_env() -> Result<Self, ModelError> {
        Self::new(
            Profile::Platform,
            Arc::new(EnvApiKey::new("OPENAI_API_KEY")?),
        )
    }

    /// The ChatGPT subscription profile, over the read-only Codex CLI login.
    pub fn codex_login() -> Result<Self, ModelError> {
        Self::new(
            Profile::ChatGptCodex,
            Arc::new(CodexLogin::from_default_path()?),
        )
    }

    /// Replace the endpoint. Intended for loopback fixtures, never for routine
    /// configuration; the transport still enforces its own URL policy.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Which profile this instance speaks.
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// Debug counters for this instance's streams.
    pub fn counters(&self) -> &Counters {
        &self.counters
    }
}

impl ModelProvider for OpenAiResponses {
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
                Ok(credential) => credential.with_extra_headers(profile.headers()),
                Err(error) => {
                    yield events::finished_failed(safe_setup(profile, error));
                    return;
                }
            };
            let reads = match request::collect(&request.input, &request.artifacts).await {
                Ok(reads) => reads,
                Err(error) => {
                    yield events::finished_failed(safe_setup(profile, error));
                    return;
                }
            };
            let body = match request::build(&request.input, profile, &reads) {
                Ok(body) => body,
                Err(error) => {
                    yield events::finished_failed(safe_setup(profile, error));
                    return;
                }
            };
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
            let mut mapper = events::Mapper::new(profile, counters);
            while let Some(item) = stream.next().await {
                let mapped = match item {
                    Ok(event) => mapper.push(&event),
                    Err(error) => {
                        drop(stream);
                        yield transport_terminal(profile, error);
                        return;
                    }
                };
                match mapped {
                    Ok(emitted) => {
                        let finished = mapper.finished();
                        for event in emitted {
                            yield event;
                        }
                        if finished {
                            // Release the connection before announcing the end.
                            drop(stream);
                            return;
                        }
                    }
                    Err(error) => {
                        drop(stream);
                        yield events::finished_failed(error);
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
                Err(error) => yield events::finished_failed(error),
            }
        })
    }
}

/// Setup failures keep an authentication hint for the subscription profile.
fn safe_setup(profile: Profile, error: ModelError) -> ModelError {
    if error.code == ModelErrorCode::Authentication {
        ModelError {
            message: events::safe_message(error.code, profile),
            ..error
        }
    } else {
        error
    }
}

/// Turn a transport failure into the single terminal event it deserves.
fn transport_terminal(profile: Profile, error: TransportError) -> ProviderEvent {
    if error.is_stopped() {
        return events::finished_cancelled();
    }
    let mut model = error.model_error();
    model = refine_http(profile, model, error.body());
    events::finished_failed(model)
}

/// Refine an HTTP failure from its bounded body without echoing it.
///
/// Authentication, 429 and explicit context-length statuses keep the transport's
/// precedence; otherwise the body's error code decides the category.
fn refine_http(profile: Profile, mut model: ModelError, body: &[u8]) -> ModelError {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok();
    let code = parsed
        .as_ref()
        .and_then(|value| {
            value
                .pointer("/error/code")
                .or_else(|| value.pointer("/error/type"))
                .or_else(|| value.get("code"))
        })
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
        model.code = events::classify(code, message);
    }
    if let Some(code) = code {
        model.provider_code = Some(code.to_owned());
    }
    model.message = events::safe_message(model.code, profile);
    model
}
