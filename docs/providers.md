# Provider transport and the OpenAI adapter (M4 steps 02–03)

`harness-providers` is the shared HTTP/SSE and credential boundary for adapters.
It depends on `harness-core`; core does not depend on it. Anthropic adapter is **not yet implemented**. The OpenAI Responses adapter is
implemented; see [ADR 0016](decisions/0016-provider-transport.md) and
[ADR 0017](decisions/0017-openai-adapter.md).

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

## OpenAI Responses adapter

`openai::OpenAiResponses` implements `ModelProvider` over the Responses streaming
API on two profiles: `Profile::Platform` with `OPENAI_API_KEY`, and
`Profile::ChatGptCodex` with the read-only Codex CLI login. Construct with
`platform_from_env()` or `codex_login()`, or `with_transport`/`with_endpoint` for
a proxy or loopback fixtures.

- Requests are stateless: `store: false`, no `previous_response_id`, and
  `include: ["reasoning.encrypted_content"]`. Encrypted reasoning returns as
  owner-scoped `ProviderState` and is replayed only to the same owner. No
  `NativeContinuation` is ever emitted.
- `openai::catalog(profile, instance)` supplies sourced limits for the context
  builder; capabilities for both profiles live in one table. Unknown models are
  text-plus-tools with no reasoning control.
- Provider options are namespaced under `openai.*` (`reasoning_summary`,
  `prompt_cache_key`, `service_tier`). Unknown keys are `invalid_request`;
  another provider's namespace is `unsupported_capability`.
- Errors keep ADR 0016's classification plus the provider's own error code, with
  fixed diagnostics: provider text is never echoed. `ChatGptCodex` adds the
  `run codex login` hint. There is no automatic retry.
- `scripts/live-check.sh openai [--auth codex|api-key]` is the opt-in real turn
  check. It is never part of `check.sh` or CI, and prints no secrets or IDs.

ADR 0017 holds the request mapping, the full event-mapping table, the profile
differences with their Codex sources, the catalog sources and the live check.

## Offline tests

`cargo test -p harness-providers --locked` runs only local fixture reads and
loopback servers. It includes the crate-private Echo SSE adapter with eleven
public core conformance cases, and the OpenAI adapter's complete 17-case
conformance manifest on both profiles over hand-written fixtures. No real
`~/.codex/auth.json` is opened and no test reaches the network.
The workspace gate is `CARGO_NET_OFFLINE=true ./scripts/check.sh`, after a
one-time `cargo fetch`, using the pinned Rust 1.99.1 toolchain.

Loopback listeners must attribute requests: host services can probe newly opened
ports. Fakes require the exact POST path (random per listener) and a random
run-specific `x-harness-test` header set by the test client. Only matching
requests affect request counts, submitted bodies and connection-close probes;
other peers are counted, logged and closed with bounded reads in independent
tasks. The run header is shared across redirect endpoints deliberately: a
redirected POST must still be counted at its target. Connection-refusal tests
hold a bound **non-listening** socket, reserving the port until HTTP and HTTPS
attempts finish. The HTTP peer-close test also requires a matching request;
TLS setup failure uses reserved refusal rather than an unattributable accept.

Run `./scripts/test-loopback-probes.sh` for 20 repetitions each of providers and
S02 tool-host tests, plus the S02 sandbox tests. This explicitly sets
`HARNESS_TEST_LOOPBACK_PROBE=1`: each listener registers its address with a
test-only background probe worker, which connects repeatedly with empty and junk
requests (including `GET / HTTP/1.1`), without scanning ports. Initial probes are
queued before the client starts. Workers stop and join when their registrations
are dropped. Logs are retained in the printed temporary directory. This stress
check is opt-in and is not added to `check.sh` or default CI.
