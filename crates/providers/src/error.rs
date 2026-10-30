//! Safe, centralized transport error classification.

use std::{fmt, time::SystemTime};

use harness_core::session::{ModelError, ModelErrorCode};
use reqwest::header::HeaderMap;

/// A failure with no unrestricted request, URL, headers or body in diagnostics.
pub struct TransportError {
    model: ModelError,
    body: Vec<u8>,
    stopped: bool,
}
impl TransportError {
    /// Convert a local failure using a static, non-sensitive diagnostic.
    pub(crate) fn new(code: ModelErrorCode, message: &'static str) -> Self {
        Self {
            model: ModelError {
                code,
                message: message.into(),
                provider_code: None,
                retry_after_ms: None,
            },
            body: Vec::new(),
            stopped: false,
        }
    }
    pub(crate) fn stopped(deadline: bool) -> Self {
        let mut e = Self::new(
            ModelErrorCode::Transport,
            if deadline {
                "inference deadline reached"
            } else {
                "inference cancelled"
            },
        );
        e.stopped = true;
        e
    }
    /// Whether cancellation or deadline, rather than a provider failure, stopped work.
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }
    /// The single safe classification used by adapters and persisted failures.
    pub fn model_error(&self) -> ModelError {
        self.model.clone()
    }
    /// Bounded sensitive HTTP error bytes, solely for adapter refinement. Never log.
    pub fn body(&self) -> &[u8] {
        &self.body
    }
    pub(crate) fn http(status: u16, headers: &HeaderMap, body: Vec<u8>) -> Self {
        use ModelErrorCode::*;
        let text = String::from_utf8_lossy(&body).to_ascii_lowercase();
        let code = match status {
            401 | 403 => Authentication,
            413 => ContextOverflow,
            429 => RateLimit,
            _ if text.contains("context_length") || text.contains("context length") => {
                ContextOverflow
            }
            _ if text.contains("overloaded") => ProviderFailure,
            400 | 404 | 422 => InvalidRequest,
            _ => ProviderFailure,
        };
        let mut e = Self::new(code, "provider HTTP request failed");
        e.model.provider_code = Some(format!("http_{status}"));
        e.model.retry_after_ms = retry_after(headers, SystemTime::now());
        e.body = body;
        e
    }
}
impl fmt::Debug for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransportError")
            .field("model", &self.model)
            .field("body", &"[redacted]")
            .finish()
    }
}
impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.model.message)
    }
}
impl std::error::Error for TransportError {}

fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<u64> {
    if let Some(ms) = headers
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok()?.parse().ok())
    {
        return Some(ms);
    }
    let value = headers.get("retry-after")?.to_str().ok()?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds.saturating_mul(1000));
    }
    let duration = httpdate::parse_http_date(value)
        .ok()?
        .duration_since(now)
        .unwrap_or_default();
    Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}
