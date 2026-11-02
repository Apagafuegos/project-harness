# Provider transport, adapters and the router (M4 steps 02–04, 07)

`harness-providers` is the shared HTTP/SSE and credential boundary for adapters.
It depends on `harness-core`; core does not depend on it. Both native wire
formats are implemented: the OpenAI Responses adapter
([ADR 0016](decisions/0016-provider-transport.md),
[ADR 0017](decisions/0017-openai-adapter.md)) and the Anthropic Messages adapter
([ADR 0018](decisions/0018-anthropic-adapter.md)), plus the router that selects
between them per request. The Messages adapter serves two endpoint profiles: Anthropic
itself and DeepSeek's Anthropic-compatible API
([ADR 0024](decisions/0024-deepseek-profile.md)).

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

`CredentialScheme` selects the authentication header. `Credential::bearer` and
`EnvApiKey::new("OPENAI_API_KEY")` keep `Authorization: Bearer`;
`Credential::api_key` and `EnvApiKey::with_scheme("ANTHROPIC_API_KEY",
CredentialScheme::ApiKeyHeader)` send `x-api-key`; the scheme is always explicit. Both schemes keep the ADR 0016 guarantees: `[redacted]`
`Debug`/`Display`, sensitive header values, reserved names refused as extra
headers, and no secret in errors, events or logs. OpenAI behavior is unchanged.

## Router: one provider seam, several instances

```rust,ignore
let provider: Arc<dyn ModelProvider> = Arc::new(
    Router::new()
        .with("openai-codex", Arc::new(openai))
        .with("anthropic", Arc::new(anthropic))
        .with("deepseek", Arc::new(deepseek)),
);
let mut catalog = ModelCatalog::new();
openai::extend_catalog(&mut catalog, Profile::ChatGptCodex, "openai-codex");
anthropic::extend_catalog(&mut catalog, anthropic::Profile::Anthropic, "anthropic");
anthropic::extend_catalog(&mut catalog, anthropic::Profile::DeepSeek, "deepseek");
```

`harness_providers::Router` implements `ModelProvider` by dispatching each
request to the adapter registered under
`request.input.model.provider_instance`. The runtime driver holds one
`Arc<dyn ModelProvider>`, and a session's `ModelRef` may name any instance, so
the router is what lets a session switch provider mid-flight without a core
change (`ModelRef` already names the instance; core already scopes native state
to its owner).

- `register(instance, Arc<dyn ModelProvider>)` / `with(..)` add or replace an
  instance; `contains` and `instances()` let a client validate a switch target
  before using it. Empty names are refused: no `ModelRef` may name one.
- An unregistered instance yields exactly one `Finished { Failed }` with
  `invalid_request` and a fixed diagnostic, and no adapter is called, so no
  network work starts. Nothing about the request (model, URL, headers, body) is
  echoed.
- A registered instance gets the request and the caller's `OperationControl`
  unchanged, and the router returns the adapter's own stream: no wrapper task,
  no queue, no retry, so cancellation, deadlines, backpressure and drop-release
  behave exactly as they do without it.
- Dispatch is never by model name, adapter type or request content. Two
  instances can serve the same model name with different accounts, and mixing
  them would replay one account's native state under another's identity.
- One catalog covers every registered instance: `openai::extend_catalog` and
  `anthropic::extend_catalog` (once per Messages profile) add each adapter's
  sourced rows to a single `ModelCatalog`, because a session that may switch
  needs limits for every instance it can reach. Core's `ModelCatalog` is not iterable, so the adapters
  add their own rows instead of the router merging catalogs.
- The Echo adapter's public conformance cases run through the router
  (`conformance_cases_pass_through_the_router`, the same eleven cases as without
  it), so dispatch is proven not to change any observed behavior, and the
  end-to-end switching tests
  (`crates/providers/tests/switch.rs`) run a real session with both adapters
  against loopback fixtures.

Native call ids travel with the stored turn. Each adapter sends the id the
provider issued when it is a plain ASCII identifier of at most 64 bytes, and
otherwise a deterministic substitute derived from the harness `CallId`, used for
both the call and its result — see `native_id` and the ADR 0018 amendment. After
a switch, an id from the other provider therefore reaches the API verbatim
(`call_…` to Anthropic, `toolu_…` to OpenAI) or is replaced consistently, never
in a form that breaks the pairing or edits the preserved-thinking prefix.

`scripts/live-check.sh switch [--auth codex|api-key]` is the opt-in real check:
one durable session, OpenAI → Anthropic → OpenAI with one short turn each and a
tool round trip. Without `ANTHROPIC_API_KEY` it runs the same-provider variant
(the live-check model → another served model → back) and reports the
cross-provider part as `skipped (no ANTHROPIC_API_KEY)`. It prints pass/fail
lines, model names and stable turn codes only, and is never part of `check.sh`
or CI.

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
- `ChatGptCodex` follows the Codex client's request shape: instructions as a
  leading `developer` item, `tool_choice`/`parallel_tool_calls` always present,
  and **no** `max_output_tokens` (that backend selects the output budget).
- `openai::catalog(profile, instance)` supplies sourced limits for the context
  builder; capabilities for both profiles live in one table. Unknown models are
  text-plus-tools with no reasoning control.
- Provider options are namespaced under `openai.*` (`reasoning_summary`,
  `prompt_cache_key`, `service_tier`). Unknown keys are `invalid_request`;
  another provider's namespace is `unsupported_capability`.
- Errors keep ADR 0016's classification plus the provider's own error code, with
  fixed diagnostics: provider text is never echoed. `ChatGptCodex` adds the
  `run codex login` hint. Refinement is limited to HTTP-status/body errors;
  non-HTTP transport errors retain their original code and message. A missing
  `Content-Type` on 2xx is allowed (the Codex backend omits it); explicit
  non-event-stream types are still rejected. There is no automatic retry.
- `scripts/live-check.sh openai [--auth codex|api-key]` is the opt-in real turn
  check. It is never part of `check.sh` or CI, and prints no secrets or IDs. Set
  `HARNESS_LIVE_OPENAI_MODEL` to override the default `gpt-6.1-sol`, sourced from
  the account's fetched `~/.codex/models_cache.json` (2026-10-30, CLI 0.166.0).
  Its context window is 500,000 tokens; output limit is unknown. Portable
  low/medium/high efforts map directly to backend low/medium/high. Platform
  metadata retains `gpt-5.5`; use a platform model override for `--auth api-key`.
- Set `HARNESS_LIVE_OPENAI_RECORDING` to a new path to retain the first completed
  text stream as a labeled fixture. The opt-in recorder replaces IDs, prose and
  encrypted payloads **before** writing. No raw capture is ever saved. The round-3
  live check passed all five cases; `fixtures/openai/recorded-codex-text.sse`
  (under `crates/providers`) is its sanitized stream, replayed by a mapper test
  (ADR 0017).

ADR 0017 holds the request mapping, the full event-mapping table, the profile
differences with their Codex sources, the catalog sources and the live check.

## Anthropic Messages adapter

`anthropic::AnthropicMessages` implements `ModelProvider` over the Messages
streaming API at `https://api.anthropic.com/v1/messages`. Construct with
`from_env()` (`ANTHROPIC_API_KEY`, sent as `x-api-key`), or
`with_transport`/`with_endpoint` for a proxy or loopback fixtures. Every request
carries `anthropic-version: 2026-10-21`; replayed thinking additionally uses
the preserved-thinking beta described below.

- The API is stateless, so no `NativeContinuation` is ever emitted. Signed
  `thinking` blocks (with `thinking` and `signature`) and opaque
  `redacted_thinking` blocks (with `data`) return as owner-scoped
  `ProviderState` (`anthropic.messages.thinking.v1`, ≤256 KiB) and are replayed
  verbatim, in order, only to the same owner. Raw thinking text is never shown:
  `thinking_delta` is retained for the signed block and counted as ignored.
- **Preserved thinking.** The API binds each replayed block to the prefix that
  produced it (`system`, `tools`, and every earlier message), and a changed
  prefix is a 400 on accounts it enforces this for. Compaction is exactly such an
  edit. So any request that replays a thinking block is sent with the
  `thinking-binding-controls-2026-08-01` beta header and
  `thinking: {"type":"adaptive","block_binding":{"prefix_mismatch_behavior":"drop_block"}}`;
  the API then drops a failing block instead of rejecting the request. Requests
  that replay nothing are unchanged (no beta header, no `thinking` object unless
  an effort is configured). See the ADR 0018 amendment.
- Messages strictly alternate: adjacent same-role items merge into one turn, as
  the API itself does. Instructions become the single `system` string (join
  order preserved, levels collapsed). Tool results carry `is_error` and the same
  deterministic status/provenance text as the OpenAI adapter.
- A configured `ReasoningEffort` becomes `thinking: {"type":"adaptive"}` plus
  `output_config.effort`; `enabled`+`budget_tokens` and `disabled` are rejected
  by the current family, so the adapter sends neither. No effort means no
  thinking control and the model's default.
- Provider options are namespaced under `anthropic.*`
  (`service_tier`: `auto`/`standard_only`). Unknown keys are `invalid_request`;
  another provider's namespace is `unsupported_capability`.
- HTTP errors are refined from the error body (`authentication_error`,
  `permission_error`, `invalid_request_error`, `request_too_large`,
  `rate_limit_error`, `overloaded_error`, `api_error`, `not_found_error`) with
  fixed diagnostics and `retry-after`; local transport errors keep their code.
  `pause_turn` and `model_context_window_exceeded` fail rather than claim a
  completed answer. No automatic retry.
- `anthropic::catalog(instance)` supplies the sourced 1M-context / 128K-output
  rows for `claude-haiku-5-5`, `claude-sonnet-5-5` and `claude-opus-5-5`
  (models overview, read 2026-10-30). Unknown models are text-plus-tools.
- `scripts/live-check.sh anthropic` is the opt-in real check (text, usage, tool
  round trip, invalid model, cancellation) against
  `HARNESS_LIVE_ANTHROPIC_MODEL` (default `claude-haiku-5-5`). With no
  `ANTHROPIC_API_KEY` it prints
  `live anthropic: skipped (no ANTHROPIC_API_KEY)` and exits **77**
  (skipped), distinct from 0 (pass) and 1 (fail).

ADR 0018 holds the request mapping, the complete event table, the thinking and
signature replay rules, the error refinements, the credential scheme, the
catalog sources and the live check.

## DeepSeek profile of the Messages adapter

`anthropic::AnthropicMessages` carries an explicit `Profile`
(`Profile::Anthropic` or `Profile::DeepSeek`) chosen at construction:
`from_env()` reads `ANTHROPIC_API_KEY`, `deepseek_from_env()` reads
`DEEPSEEK_API_KEY`, and `with_profile` builds either over an explicit transport.
The profile fixes the endpoint, the credential variable and scheme, the headers,
which documented request controls the backend accepts, whether a thinking block
needs a signature, and the native-state schema. There is no second adapter and no
provider branch in the request or event mapping.

- Endpoint `https://api.deepseek.com/anthropic/v1/messages`; the key is sent as
  `x-api-key` (DeepSeek also accepts `Authorization: Bearer`). No
  `anthropic-version` and no `anthropic-beta`: DeepSeek documents both as ignored
  and this profile sends neither.
- Thinking is on by default. A configured `ReasoningEffort` becomes
  `thinking: {"type":"enabled"}` plus `output_config.effort`
  (`low`/`medium`/`high`; DeepSeek also accepts its own wider set and ignores
  `budget_tokens`). Requests that replay a thinking block add nothing: DeepSeek
  documents no `thinking.block_binding` control, so the Anthropic
  preserved-thinking beta and control are never sent from this profile.
- A thinking block is documented as supported and never as signed, so this
  profile accepts an unsigned block on both the stream and the replay path, while
  the Anthropic profile still rejects one. State is stored under
  `deepseek.messages.thinking.v1`; owner and schema each prevent cross-profile
  replay. `redacted_thinking` is not in DeepSeek's vocabulary: it is neither
  stored nor replayed.
- `service_tier` stays mapped (documented as ignored); `is_error` and
  `cache_control` are ignored by DeepSeek and harmless as sent.
- Usage is the closing `message_delta` snapshot, which is the last
  `UsageUpdated`; `cache_creation_input_tokens` is always 0. A start-up timeout
  can arrive as an unnamed `data` frame holding only an object with `error`, which
  maps to a coded `provider_failure` instead of a malformed-stream guess.
- `anthropic::catalog(Profile::DeepSeek, instance)` supplies
  `deepseek-flash` and `deepseek-v4-pro` (1M context, 384K maximum output, thinking
  and tools; images on `deepseek-flash` only, user-side only). Unknown models are
  text-plus-tools.
- `scripts/live-check.sh deepseek` is the opt-in real check: one streamed turn
  with a tool round trip and one follow-up turn that replays the stored thinking
  block, against `HARNESS_LIVE_DEEPSEEK_MODEL` (default `deepseek-flash`). With no
  `DEEPSEEK_API_KEY` it prints `live deepseek: skipped (no DEEPSEEK_API_KEY)` and
  exits **77**. It was unrun at m6-00 because that step had no key; the profile's
  live endpoint is instead evidenced by the two bounded DeepSeek Flash matrices
  in [context experiments](context-experiments.md), which made real summary,
  probe and continuation requests (and stored/replayed native state through the
  same profile) with zero mechanical errors in the second run.
- `--provider deepseek` is available to both `harness ask` and the plain
  `harness` TUI through the shared wiring, with `deepseek-flash` as the new-session
  model default. See [Milestone 6 acceptance](milestone-6-acceptance.md).

ADR 0024 holds the control list, the signature behavior, the catalog sources and
the date read.

## Offline tests

`cargo test -p harness-providers --locked` runs only local fixture reads and
loopback servers. It includes the crate-private Echo SSE adapter with eleven
public core conformance cases, the OpenAI adapter's complete 17-case conformance
manifest on both profiles, and the Messages adapter's complete 17-case manifest
over hand-written fixtures on both of its profiles (Anthropic and DeepSeek). No
real `~/.codex/auth.json` is opened, no Anthropic or DeepSeek credential is read,
and no test reaches the external network.
The [M4 acceptance](milestone-4-acceptance.md) adds terminal-success dispatch,
cumulative-usage replacement and multimodal/native replay through reopen for
both real adapters, plus the deterministic `provider_session` tour.
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
