//! Read-only credential sources. All diagnostics redact credential material.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use harness_core::session::{ModelError, ModelErrorCode};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use std::{
    fmt,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Authentication header selected by a credential source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CredentialScheme {
    /// `Authorization: Bearer <secret>` (the existing default).
    #[default]
    Bearer,
    /// `x-api-key: <secret>` (Anthropic API keys).
    ApiKeyHeader,
}

/// A short-lived secret and optional adapter headers and expiry.
#[derive(Clone)]
pub struct Credential {
    secret: String,
    scheme: CredentialScheme,
    headers: HeaderMap,
    expiry: Option<SystemTime>,
}
impl Credential {
    /// Construct a bearer credential. Invalid header bytes fail without echoing input.
    pub fn bearer(
        secret: String,
        headers: HeaderMap,
        expiry: Option<SystemTime>,
    ) -> Result<Self, ModelError> {
        Self::new(secret, CredentialScheme::Bearer, headers, expiry)
    }
    /// Construct an API-key-header credential, without exposing the key.
    pub fn api_key(
        secret: String,
        headers: HeaderMap,
        expiry: Option<SystemTime>,
    ) -> Result<Self, ModelError> {
        Self::new(secret, CredentialScheme::ApiKeyHeader, headers, expiry)
    }
    fn new(
        secret: String,
        scheme: CredentialScheme,
        headers: HeaderMap,
        expiry: Option<SystemTime>,
    ) -> Result<Self, ModelError> {
        if secret.is_empty() || HeaderValue::from_str(&secret).is_err() {
            return Err(auth("invalid credential"));
        }
        if headers.contains_key(AUTHORIZATION) || headers.contains_key("x-api-key") {
            return Err(auth("reserved credential header"));
        }
        Ok(Self {
            secret,
            scheme,
            headers,
            expiry,
        })
    }
    /// Expiry reported by the source (not a cryptographic authenticity claim).
    pub fn expiry(&self) -> Option<SystemTime> {
        self.expiry
    }
    /// Add adapter-owned headers (never `Authorization`) without reading the secret.
    ///
    /// The adapter profile supplies transport headers such as `Accept`, and the
    /// subscription profile its `originator`; the credential keeps the bearer
    /// token opaque to the adapter.
    pub(crate) fn with_extra_headers(mut self, extra: HeaderMap) -> Self {
        for (name, value) in extra {
            let Some(name) = name else { continue };
            if name == AUTHORIZATION || name == "x-api-key" {
                continue;
            }
            let mut value = value;
            value.set_sensitive(true);
            self.headers.insert(name, value);
        }
        self
    }
    pub(crate) fn request_headers(&self) -> HeaderMap {
        let mut headers = self.headers.clone();
        for value in headers.values_mut() {
            value.set_sensitive(true);
        }
        // Validated at construction; the secret is never mutable.
        let (name, text) = match self.scheme {
            CredentialScheme::Bearer => (AUTHORIZATION, format!("Bearer {}", self.secret)),
            CredentialScheme::ApiKeyHeader => (
                reqwest::header::HeaderName::from_static("x-api-key"),
                self.secret.clone(),
            ),
        };
        if let Ok(mut value) = HeaderValue::from_str(&text) {
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        headers
    }
}
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}
impl fmt::Display for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}
/// Synchronous bounded local reads; callers may use a blocking worker when needed.
pub trait CredentialSource: Send + Sync {
    /// Read credentials without refreshing, writing or making network requests.
    fn credential(&self) -> Result<Credential, ModelError>;
}
/// An environment API key captured once, at construction.
pub struct EnvApiKey {
    credential: Credential,
}
impl EnvApiKey {
    /// Read a named variable once as a bearer key; errors name the variable,
    /// never its value.
    pub fn new(variable: &str) -> Result<Self, ModelError> {
        Self::with_scheme(variable, CredentialScheme::Bearer)
    }

    /// Read a named variable once, sent with an explicit authentication scheme.
    pub fn with_scheme(variable: &str, scheme: CredentialScheme) -> Result<Self, ModelError> {
        // Restrict diagnostics to environment variable identifiers, not arbitrary input.
        if variable.is_empty()
            || !variable
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(auth("invalid environment variable name"));
        }
        let value = std::env::var(variable)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| auth(&format!("missing or empty credential variable {variable}")))?;
        Ok(Self {
            credential: Credential::new(value, scheme, HeaderMap::new(), None)?,
        })
    }
}
impl CredentialSource for EnvApiKey {
    fn credential(&self) -> Result<Credential, ModelError> {
        Ok(self.credential.clone())
    }
}
impl fmt::Debug for EnvApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}
impl fmt::Display for EnvApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}
/// Read-only Codex CLI login. Never refreshes or rewrites the file.
pub struct CodexLogin {
    path: PathBuf,
}
impl CodexLogin {
    /// Locate `~/.codex/auth.json`; does not read it until `credential()`.
    pub fn from_default_path() -> Result<Self, ModelError> {
        let home = std::env::var_os("HOME").ok_or_else(login)?;
        Ok(Self::from_path(Path::new(&home).join(".codex/auth.json")))
    }
    /// Configure a path (tests must use private fixtures).
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}
impl CredentialSource for CodexLogin {
    fn credential(&self) -> Result<Credential, ModelError> {
        let fd = rustix::fs::open(
            &self.path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| match error {
            rustix::io::Errno::ACCESS | rustix::io::Errno::PERM => auth("insecure credential file"),
            _ => login(),
        })?;
        let file = File::from(fd);
        let metadata = file.metadata().map_err(|_| login())?;
        #[cfg(unix)]
        {
            if !secure_file(&metadata, rustix::process::getuid().as_raw()) {
                return Err(auth("insecure credential file"));
            }
        }
        #[cfg(not(unix))]
        {
            return Err(auth("insecure credential file"));
        }
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| login())?;
        if bytes.len() > 64 * 1024 {
            return Err(login());
        }
        let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| login())?;
        let token = json
            .pointer("/tokens/access_token")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .ok_or_else(login)?;
        let account = json
            .pointer("/tokens/account_id")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .ok_or_else(login)?;
        let payload = token.split('.').nth(1).ok_or_else(login)?;
        let decoded = URL_SAFE_NO_PAD.decode(payload).map_err(|_| login())?;
        let claims: serde_json::Value = serde_json::from_slice(&decoded).map_err(|_| login())?;
        let exp = claims
            .get("exp")
            .and_then(|v| v.as_u64())
            .ok_or_else(login)?;
        let expiry = UNIX_EPOCH
            .checked_add(Duration::from_secs(exp))
            .ok_or_else(login)?;
        if expiry <= SystemTime::now() {
            return Err(login());
        }
        let mut headers = HeaderMap::new();
        let mut value = HeaderValue::from_str(account).map_err(|_| login())?;
        value.set_sensitive(true);
        headers.insert("chatgpt-account-id", value);
        Credential::bearer(token.into(), headers, Some(expiry)).map_err(|_| login())
    }
}
impl fmt::Debug for CodexLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}
impl fmt::Display for CodexLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}
#[cfg(unix)]
pub(crate) fn secure_file(metadata: &std::fs::Metadata, owner: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.is_file() && metadata.mode() & 0o077 == 0 && metadata.uid() == owner
}
fn auth(message: &str) -> ModelError {
    ModelError {
        code: ModelErrorCode::Authentication,
        message: message.into(),
        provider_code: None,
        retry_after_ms: None,
    }
}
fn login() -> ModelError {
    auth("missing, invalid or expired Codex login; run `codex login`")
}
