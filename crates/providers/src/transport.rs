//! Non-retrying, direct HTTPS streaming POST transport.
use crate::{credentials::Credential, error::TransportError};
use futures_core::Stream;
use futures_util::StreamExt;
use harness_core::{model::OperationControl, session::ModelErrorCode};
use reqwest::{Client, Url};
use std::{future::Future, pin::Pin, time::Duration};

/// Owned byte stream; dropping it releases the response connection.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, TransportError>> + Send>>;
/// HTTP connection/setup policy. No system proxy is consulted.
#[derive(Clone)]
pub struct TransportConfig {
    /// Maximum connection establishment duration.
    pub connect_timeout: Duration,
    /// Maximum time until response headers and the first body byte.
    pub first_byte_timeout: Duration,
    /// Explicit test-only opt-in to numeric loopback HTTP URLs.
    pub allow_loopback_http: bool,
    /// Explicit proxy URL, if desired. Never inferred from environment variables.
    pub proxy: Option<String>,
}
impl std::fmt::Debug for TransportConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportConfig")
            .field("connect_timeout", &self.connect_timeout)
            .field("first_byte_timeout", &self.first_byte_timeout)
            .field("allow_loopback_http", &self.allow_loopback_http)
            .field("proxy", &"[redacted]")
            .finish()
    }
}
impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            first_byte_timeout: Duration::from_secs(30),
            allow_loopback_http: false,
            proxy: None,
        }
    }
}
/// Cloneable shared client. Inference requests are never automatically retried.
#[derive(Clone)]
pub struct Transport {
    client: Client,
    config: TransportConfig,
}
impl Transport {
    /// Build a rustls-only client with redirects, retries and ambient proxies disabled.
    pub fn new(config: TransportConfig) -> Result<Self, TransportError> {
        let mut builder = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(config.connect_timeout)
            .pool_max_idle_per_host(0)
            .http1_only()
            .user_agent(concat!("harness/", env!("CARGO_PKG_VERSION")));
        if let Some(proxy) = &config.proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|_| invalid())?);
        }
        let client = builder.build().map_err(|_| network())?;
        Ok(Self { client, config })
    }
    /// POST JSON with a short-lived credential. Setup is controlled and bounded;
    /// non-success bodies are bounded to 64 KiB for adapter refinement.
    pub async fn post(
        &self,
        endpoint: &str,
        json: &serde_json::Value,
        credential: Credential,
        control: OperationControl,
    ) -> Result<ByteStream, TransportError> {
        let url = Url::parse(endpoint).map_err(|_| invalid())?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]"));
        if !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || self.config.allow_loopback_http && url.scheme() == "http" && loopback)
        {
            return Err(invalid());
        }
        let request = self
            .client
            .post(url)
            .headers(credential.request_headers())
            .json(json);
        drop(credential);
        let setup = async {
            let response = request.send().await.map_err(|_| network())?;
            let status = response.status();
            let headers = response.headers().clone();
            let mut bytes = response.bytes_stream();
            let first = bytes.next().await.transpose().map_err(|_| network())?;
            Ok::<_, TransportError>((status, headers, bytes, first))
        };
        let (status, headers, mut bytes, first) = controlled(&control, async {
            tokio::time::timeout(self.config.first_byte_timeout, setup)
                .await
                .map_err(|_| TransportError::new(ModelErrorCode::Transport, "first byte timeout"))?
        })
        .await?;
        if !status.is_success() {
            let mut body = Vec::new();
            if let Some(first) = first {
                body.extend_from_slice(&first[..first.len().min(64 * 1024)]);
            }
            controlled(&control, async {
                while body.len() < 64 * 1024 {
                    let Some(chunk) = bytes.next().await else {
                        break;
                    };
                    let chunk = chunk.map_err(|_| network())?;
                    body.extend_from_slice(&chunk[..chunk.len().min(64 * 1024 - body.len())]);
                }
                Ok(())
            })
            .await?;
            return Err(TransportError::http(status.as_u16(), &headers, body));
        }
        if !headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
            })
        {
            return Err(TransportError::new(
                ModelErrorCode::MalformedStream,
                "expected text/event-stream",
            ));
        }
        Ok(Box::pin(async_stream::stream! {
            if let Some(first) = first { yield Ok(first.to_vec()); }
            loop {
                match controlled(&control, async { bytes.next().await.transpose().map_err(|_| network()) }).await {
                    Ok(Some(chunk)) => yield Ok(chunk.to_vec()),
                    Ok(None) => break,
                    Err(e) => { drop(bytes); yield Err(e); return; }
                }
            }
        }))
    }
}
pub(crate) async fn controlled<T>(
    control: &OperationControl,
    future: impl Future<Output = Result<T, TransportError>>,
) -> Result<T, TransportError> {
    tokio::select! { biased;
        _ = control.cancel.cancelled() => Err(TransportError::stopped(false)),
        _ = async { match control.deadline { Some(d) => tokio::time::sleep_until(d).await, None => std::future::pending().await } } => Err(TransportError::stopped(true)),
        result = future => result,
    }
}
fn network() -> TransportError {
    TransportError::new(ModelErrorCode::Transport, "HTTP transport failed")
}
fn invalid() -> TransportError {
    TransportError::new(
        ModelErrorCode::InvalidRequest,
        "invalid transport endpoint or proxy",
    )
}
