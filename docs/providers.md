# Provider transport (implemented M4 step 02)

`harness-providers` is the shared HTTP/SSE and credential boundary for adapters.
It depends on `harness-core`; core does not depend on it. Native OpenAI and
Anthropic adapters, endpoints, request formats and live checks are **not yet
implemented**. See [ADR 0016](decisions/0016-provider-transport.md).

## Adapter use

1. Build `transport::Transport` from `TransportConfig`. Defaults require HTTPS,
   bypass ambient proxies, forbid redirects/retries and send a versioned harness
   User-Agent. A proxy must be explicitly configured. Loopback HTTP requires
   `allow_loopback_http: true` and accepts only literal 127.0.0.1 or [::1].
2. Obtain a short-lived `credentials::Credential` through `CredentialSource`.
   `EnvApiKey::new("OPENAI_API_KEY")` captures once. `CodexLogin::from_path` or
   `from_default_path` rereads the official CLI login on each call, read-only,
   and rejects insecure permissions, ownership and expiry. Never refresh it.
3. In `ModelProvider::stream`, pass an owned async setup future calling
   `Transport::post(endpoint, json, credential, control)` to `sse::read` with
   `SseConfig` and the same `OperationControl`. The reader owns setup, response
   and its only worker. No network work starts until the stream is polled.
4. Translate `SseEvent` into normalized `ProviderEvent` values. Require your own
   terminal vocabulary: SSE EOF is not model success. Drop the SSE stream before
   emitting a native terminal event, so resources are released immediately.
5. Map `TransportError::model_error()` without copying headers, URLs or bodies
   into diagnostics. `is_stopped()` permits a Cancelled outcome. `body()` is a
   sensitive, bounded (64 KiB) refinement input, never a loggable diagnostic.

The byte stream returned directly by `post` is pull-driven; use `sse::read` to
own the entire setup/stream lifecycle and enforce cancellation/deadline even
while the consumer is paused. Dropping either stream releases its resources;
no persistent inference producer is detached.

## Bounds and credentials

Defaults: 256 KiB per framed event, 4 MiB per response, 1 MiB / 256 queued
events. Queue reservations are conservative (one full event budget per slot),
so default byte capacity allows at most four queued/in-progress events. It
waits cancellably before reading another event rather than dropping fragments.
Event and response overflow fail with `limit_exceeded` and close the response.

Credential/source Debug and Display are `[redacted]`. Invalid credentials fail
with `authentication`. Codex failures suggest `run codex login`; only the CLI
owns refresh. JWT exp decoding reports expiry and is not signature validation.
The Codex account ID becomes a sensitive `chatgpt-account-id` header. No
Anthropic consumer subscription credentials are supported: use an API key.
Never put credentials in `ModelRequest`, events, journals or logs.

## Offline tests

`cargo test -p harness-providers --locked` runs only local fixture reads and
loopback servers, including a crate-private Echo SSE adapter and eleven public
core conformance cases. No real `~/.codex/auth.json` is opened. Native tool,
usage, replay and unsupported-option conformance belongs to the real adapters.
The workspace gate is `CARGO_NET_OFFLINE=true ./scripts/check.sh`, after a
one-time `cargo fetch`, using the pinned Rust 1.99.1 toolchain.
