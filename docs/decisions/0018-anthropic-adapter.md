# 0018: The Anthropic Messages adapter

Status: Implemented for Milestone 4, step 04. No core change was needed; the
seam covers this provider unchanged.

Date: 2026-10-30

## Decision

`harness_providers::anthropic::AnthropicMessages` implements the existing
`ModelProvider` seam over the Anthropic **Messages** streaming API. Like the
OpenAI adapter it is a pure request/event mapping plus the shared transport from
ADR 0016: no retry, no hidden state, no task of its own beyond the transport's
reader.

Authentication is `ANTHROPIC_API_KEY` sent as `x-api-key` (M4 decision 13: there
is no Anthropic subscription login). The API is stateless: no response handle is
ever returned as a `NativeContinuation`. Continuity that only the provider can
reproduce — signed `thinking` and opaque `redacted_thinking` blocks — travels as
owner-scoped `ProviderState`.

Sources, all read on **2026-10-30**:

- Messages reference: `platform.claude.com/docs/en/api/messages/create`.
- Streaming reference: `platform.claude.com/docs/en/build-with-claude/streaming`.
- Errors: `platform.claude.com/docs/en/api/errors`.
- Thinking: `platform.claude.com/docs/en/build-with-claude/thinking` and
  `.../extended-thinking`.
- Authentication: `platform.claude.com/docs/en/manage-claude/authentication`.
- Versions: `platform.claude.com/docs/en/api/versioning`.
- Models overview and per-model pages:
  `platform.claude.com/docs/en/about-claude/models/overview`,
  `.../models/{haiku,sonnet,opus}-5-5/overview`.

## Credential scheme extension

ADR 0016's credential carried a bearer secret. Anthropic's API uses a second
header scheme, so `credentials::CredentialScheme` now selects it:

```rust,ignore
pub enum CredentialScheme { Bearer, ApiKeyHeader }
```

`Credential::bearer` is unchanged (`Authorization: Bearer <secret>`), and
`Credential::api_key` selects `x-api-key: <secret>`. `EnvApiKey::new` always
reads a bearer key; `EnvApiKey::with_scheme` takes the scheme explicitly, and the
Anthropic adapter's `from_env` uses `with_scheme("ANTHROPIC_API_KEY",
CredentialScheme::ApiKeyHeader)`. The scheme is never inferred from a variable
name. Current Anthropic SDKs send API keys as `Authorization: Bearer`; the API
still accepts `x-api-key`, which this adapter keeps (the brief's acceptance item).

Every m4-02 redaction guarantee is preserved for both schemes:

- `Debug` and `Display` of `Credential`, `EnvApiKey` and `CodexLogin` are exactly
  `[redacted]`; the adapter's own `Debug` prints `credential: [redacted]`.
- The secret is validated once at construction (`HeaderValue` rejects control
  bytes) and is never formatted into a diagnostic. Invalid credentials fail with
  the fixed message `invalid credential`.
- The submitted header value is marked sensitive, and both reserved names
  (`authorization`, `x-api-key`) are refused in extra headers so an adapter
  cannot overwrite or duplicate the secret.
- Secrets never enter `ModelRequest`, events, journals, session files or the
  bounded error body.

OpenAI behavior is unchanged: `Credential::bearer` and
`EnvApiKey::new("OPENAI_API_KEY")` still produce `Authorization: Bearer`, and
`CodexLogin` still produces the bearer token plus `chatgpt-account-id`. No
OpenAI test changed.

## Endpoint and headers

| Header | Value | Why |
| --- | --- | --- |
| endpoint | `https://api.anthropic.com/v1/messages` | Messages reference |
| `x-api-key` | the credential secret | `manage-claude/authentication` |
| `anthropic-version` | `2026-10-21` | required on every request; current version |
| `accept` | `text/event-stream` | streaming request |
| `content-type` | `application/json` | set by the JSON POST |

**No beta header is sent.** Everything used here — adaptive thinking,
`output_config.effort`, `service_tier`, `tool_use`/`tool_result`, images, the
streaming event vocabulary — is part of the current stable API version. Beta
headers are added only when a documented feature requires one, and there is none
in this mapping.

One credential detail the brief inherited from the older documentation is now
stale, and the current reference wins under the standing allowance for
paraphrase: `x-api-key` is described as the *legacy* header, with
`Authorization: Bearer` as the current primary form. The brief's intent is a
distinct, non-bearer scheme for this provider and a sentinel-tested `x-api-key`
header, which this implementation keeps; both forms are accepted by the API
today. Switching to Bearer would have removed the credential-scheme extension
the step exists to prove, so the brief's explicit acceptance item is followed.

## Request mapping

`request::build` is pure: no I/O, clock or credential. Artifact bytes are read
beforehand by `request::collect`, which reads exactly user images and same-owner
`ProviderState` blocks referenced by the projection and nothing else.

| Input | Body |
| --- | --- |
| `model` | `model` |
| `ContextItem::Instructions` | `system` (string, joined in projection order with a blank line) |
| `ContextItem::User` text | `{"type":"text","text":…}` |
| `ContextItem::User` image | `{"type":"image","source":{"type":"base64","media_type":…,"data":…}}`, only if the model's sourced capabilities include images |
| `ContextItem::User` audio/file | `unsupported_capability` (no catalogued Claude model accepts them here) |
| `ContextItem::Assistant` text | `{"type":"text","text":…}` |
| `ContextItem::Assistant` `ToolCall` | `{"type":"tool_use","id":<native id>,"name":…,"input":<object>}` |
| `ContextItem::Assistant` `ProviderState` | the stored thinking block, replayed verbatim (see below), same owner only |
| `ContextItem::Assistant` `ReasoningSummary` | dropped: display-only, never resent as thought |
| `ContextItem::ToolResult` | `{"type":"tool_result","tool_use_id":<native id>,"content":<text>,"is_error":<status != succeeded>}` |
| `ContextItem::Summary` | user message `Summary of earlier conversation:\n\n…` |
| `tools` | `{"name","description","input_schema"}` per definition |
| `options.max_output_tokens` | `max_tokens` (required; `0` is `invalid_request`) |
| `options.reasoning_effort` | `thinking: {"type":"adaptive"}` plus `output_config: {"effort": low\|medium\|high}` |
| `anthropic.service_tier` | `service_tier` (`auto` or `standard_only`) |

Fixed fields: `stream: true`, and `tools` whenever the input has definitions.

**Alternating roles.** The Messages API combines consecutive same-role turns
itself and requires `user`/`assistant` alternation. The adapter does the same
merge locally, so the submitted body is already strictly alternating and content
order is preserved across item boundaries: a tool result and a following
compaction summary become one `user` turn with a `tool_result` block followed by
a text block. System and developer instruction levels collapse into the single
`system` field; the API has no per-level system roles, so their order and text
are preserved but the distinction is not (the seam passes no richer precedence).

**Thinking configuration.** The current reference documents adaptive thinking as
the accepted form for this family: `thinking: {"type":"enabled"}` with
`budget_tokens` is rejected on Claude 4.7 and later (including all three 5.5
models), and `{"type":"disabled"}` is rejected outright by always-on models and
by Sonnet 5.5. The adapter therefore never sends `enabled` or `disabled`. A
configured `ReasoningEffort` becomes `{"type":"adaptive"}` plus the matching
`output_config.effort` (`low`/`medium`/`high` map to the same strings; portable
effort is not widened to `xhigh`/`max`). With no configured effort the adapter
sends no thinking control at all and lets the model's documented default apply.

**Provider options** are namespaced and validated before submission. The
namespace is deliberately minimal — one documented control:

| Value | Result |
| --- | --- |
| `anthropic.service_tier` other than `auto`/`standard_only` | `invalid_request` |
| `anthropic.<unknown key>` | `invalid_request`, naming the key |
| `anthropic.*` value not a JSON object | `invalid_request` |
| any other namespace, e.g. `openai` | `unsupported_capability`, naming the namespace |
| `reasoning_effort` on a model without sourced reasoning support | `unsupported_capability` |
| `max_output_tokens == 0` | `invalid_request` |

**Tool-result text** is the same deterministic plain-text rendering the OpenAI
adapter uses: `status`, `provenance`, `exit_code`, `output_truncated`, a blank
line and the result's text, with media reduced to `[artifact: <mime>]`. `is_error`
is set for every status other than `succeeded`.

## Event mapping

`events::Mapper` is total over the documented vocabulary. Each row has a
hand-written fixture under `crates/providers/fixtures/anthropic/`, and the whole
table is exercised by the public 17-case conformance manifest.

| Native event | `ProviderEvent` |
| --- | --- |
| `message_start` | `Started` (validates the empty `assistant` message), then `UsageUpdated` when the message carries usage |
| `content_block_start` (`text`) | `TextDelta` for the block's initial text (empty in practice) |
| `content_block_start` (`thinking`) | nothing yet; fields validated, block kept for replay |
| `content_block_start` (`redacted_thinking`) | nothing yet; `data` validated, block kept for replay |
| `content_block_start` (`tool_use`) | `ToolCallStarted { name, provider_call_id: id }` |
| `content_block_start` (other, e.g. server tools) | ignored, counted as unknown |
| `content_block_delta` (`text_delta`) | `TextDelta` |
| `content_block_delta` (`thinking_delta`) | ignored, counted as deliberately ignored; appended to the block for verbatim replay |
| `content_block_delta` (`signature_delta`) | appended to the block's signature; no event |
| `content_block_delta` (`input_json_delta`) | `ToolArgumentsDelta` |
| `content_block_delta` (other) | ignored, counted as unknown |
| `content_block_stop` (`tool_use`) | `ToolCallReady` after the assembled fragments parse as one JSON object |
| `content_block_stop` (`thinking`, `redacted_thinking`) | `ProviderState` at the block's index, schema `anthropic.messages.thinking.v1`, payload = the block's JSON bytes, ≤256 KiB |
| `content_block_stop` (`text`) | nothing |
| `message_delta` | stop reason recorded, `UsageUpdated` for the cumulative snapshot |
| `message_stop` | `Finished { Completed, finish_reason }` |
| `ping` | ignored, counted as deliberately ignored |
| `error` | `Finished { Failed }` with the mapped error and `retry_after_ms` when the event carries one |
| any other type | ignored, counted in the unknown counter; never fatal |

**Thinking deltas are ignored, not displayed.** The core treats raw reasoning as
not for display, and `ReasoningSummaryDelta` is reserved for text the provider
designates as a summary. Anthropic's `thinking_delta` is the model's thinking
text; it is retained only to rebuild the signed block that must be replayed, and
is never turned into a visible summary. Displaying it would require requesting
`display: "summarized"` and trusting that setting; the adapter does not, so it
never claims a summary it cannot guarantee.

**Stop reasons:**

| `stop_reason` | Result |
| --- | --- |
| `end_turn`, `stop_sequence` | `Finished { Completed, Stop }` (no tool blocks, else malformed) |
| `tool_use` | `Finished { Completed, ToolCalls }` (requires tool blocks) |
| `max_tokens` | `Finished { Completed, OutputLimit }` |
| `refusal` | `Finished { Completed, Refusal }` |
| `model_context_window_exceeded` | `Finished { Failed, context_overflow }` — the request already was too large, so it is not a completed turn |
| `pause_turn` | `Finished { Failed, unsupported_capability }` — continuing needs the server-tool loop this adapter does not implement; silently reporting a completed turn would truncate the answer |
| unknown, or inconsistent with the content | `malformed_stream` |

**Malformed streams:** a content event before `message_start`, a duplicate
`message_start`, a delta or stop for an unknown/closed/duplicate block, a delta
for the wrong block kind, a non-JSON `data` field, a tool block whose arguments
never form one JSON object, a thinking block with an empty signature,
`message_delta` before every block closed, `message_stop` without a stop reason,
and a stream that ends without `message_stop`. All become `Finished { Failed }`
with `malformed_stream`; the adapter never invents a success. `Finished` carries
no `NativeContinuation`, ever.

## Thinking and signature replay

Each completed `thinking` block becomes one `ProviderState` block with schema
`anthropic.messages.thinking.v1`, a fresh index (its native index, which no other
block in the response used), and a payload of the block's JSON bytes:

```json
{"type":"thinking","thinking":"…","signature":"…"}
{"type":"redacted_thinking","data":"…"}
```

These are the two native shapes the reference documents. `thinking` keeps
`thinking` and `signature`; `redacted_thinking` keeps its opaque encrypted
`data` and has **no** `signature` field. M4 step 04's brief said "with their
signatures" for both; the API reference differs, and the orchestrator confirmed
that verbatim replay of the opaque state is the intent: the payload keeps
whichever fields the provider returned, and the two shapes stay distinguishable
so replay reconstructs the exact block. The API also requires these blocks back
"exactly as received", in order, including blocks whose thinking text is empty.

Replay rules:

- A `ProviderState` block is replayed only when its owner equals the request's
  model and its schema is this adapter's. A foreign owner is skipped, never
  recovered: the core already scopes state, and the adapter must not widen it.
- An unknown schema for the same owner is `unsupported_capability`.
- A payload that is not one of the two shapes (or a `thinking` block without a
  string `signature`, or a `redacted_thinking` block without a string `data`) is
  `invalid_request`: guessing a replay format is worse than failing.
- The block is inserted at its position in the assistant content, so the native
  order is preserved. Reasoning summaries are not replayed.
- Payloads are read through `ModelRequest.artifacts`, the core's request-scoped
  allowlist. A payload over 256 KiB is `limit_exceeded` before the core, which
  enforces the same bound.

The API additionally binds a signature to the model that produced it and to
everything before it (system prompt, tools, earlier messages). An append-only
journal keeps that prefix stable; a model switch or compaction invalidates it,
and the API then answers `invalid_request_error`, which the adapter reports as
`invalid_request` rather than retrying or silently dropping blocks (M4 decision
8: no automatic retry). No `preserved-thinking` or binding-control beta header is
sent; a request that fails the prefix check fails visibly.

## Error refinements

Transport classification from ADR 0016 is preserved, then refined from the
bounded `TransportError::body()` (never logged). Only HTTP-status/body failures
are refined; local `malformed_stream`, `limit_exceeded` and `transport` errors
keep their original code **and message**, because classifying an empty body must
not hide the transport diagnosis.

| Evidence | Category |
| --- | --- |
| 401/403, `authentication_error`, `permission_error` | `authentication` |
| 429, `rate_limit_error` | `rate_limit` (with `retry_after_ms`) |
| 413, or `request_too_large`/`invalid_request_error` whose message mentions the context/token limit | `context_overflow` |
| `invalid_request_error`, `request_too_large` without a context hint, `not_found_error` | `invalid_request` |
| `overloaded_error`, `api_error` and everything else | `provider_failure` |
| invalid UTF-8/framing/truncation or a mapping violation | `malformed_stream` |
| event or response byte bound | `limit_exceeded` |
| cancellation or deadline (`TransportError::is_stopped`) | `Finished { Cancelled }` |

Authentication, rate limit and context overflow from the HTTP status take
precedence over body hints. `provider_code` is the provider's own error `type`
when a body supplies one, otherwise the transport's `http_<status>`;
`retry_after_ms` comes from `Retry-After`, and from the event's own
`retry_after_ms` for an in-stream `error`. Diagnostics are fixed strings per
category: the provider's message is never echoed, because it can quote request
content or a key.

`not_found_error` maps to `invalid_request` rather than `provider_failure`: it is
how the API reports an unknown model, and the transport's own 404 rule already
says `invalid_request`.

There is **no automatic retry** (M4 decision 8). Cancel, deadline and drop close
the connection through the shared transport, before the terminal event is
announced.

## Catalog sources

`anthropic::catalog(instance)` returns `ModelCatalog` entries built from one
table, and `capabilities(model)` is the adapter's own validation input. An
uncatalogued model is text-plus-tools with no reasoning control.

| Model | Context | Max output | Capabilities | Source |
| --- | --- | --- | --- | --- |
| `claude-haiku-5-5` | 1,000,000 | 128,000 | image input, adaptive thinking/effort, tools | models overview + `models/haiku-5-5/overview`, read 2026-10-30 |
| `claude-sonnet-5-5` | 1,000,000 | 128,000 | image input, adaptive thinking/effort, tools | models overview + `models/sonnet-5-5/overview`, read 2026-10-30 |
| `claude-opus-5-5` | 1,000,000 | 128,000 | image input, adaptive thinking/effort, tools | models overview + `models/opus-5-5/overview`, read 2026-10-30 |

The overview's comparison table states `1M tokens` context and `128K tokens`
max output for all three; each model page repeats them. The pages also state
`Input → output: Text and images → text`, which is the image capability. No
number here is inferred from a model name, and a limit that could not be sourced
would stay `None`.

## Opt-in live check

`scripts/live-check.sh anthropic` runs `examples/live_anthropic.rs` through the
real adapter. It is never part of `check.sh` or CI. The model is
`HARNESS_LIVE_ANTHROPIC_MODEL`, defaulting to `claude-haiku-5-5`.

Without `ANTHROPIC_API_KEY` the script prints
`live anthropic: skipped (no ANTHROPIC_API_KEY)` and exits **77**, a distinct
code documented here and in `docs/providers.md` so a caller can distinguish
skipped (77) from passed (0) and failed (1). This machine has no Anthropic key:
the recorded invocation is that skip.

The check covers the same five cases as the OpenAI one: streaming text, usage
from that turn, a tool invocation round trip, an invalid model name, and
cancellation mid-stream. It prints pass/fail lines only — no keys, account data
or response IDs.

The tool case is honest about one example-only limit: replaying a signed
thinking block needs a stored artifact, which belongs to the runtime, not to a
standalone example. If the first turn returns signed thinking, the check verifies
the call and states that the replay leg was not exercised instead of sending a
history the API is documented to reject. Offline tests cover the replay itself.

## Evidence and scope

`crates/providers/src/anthropic/tests.rs` runs the **complete 17-case public
conformance manifest** over the hand-written fixtures through the real adapter
and an attributed loopback server (`request_preservation` decodes the actual
submitted JSON). Unit tests cover:

- the golden Messages body (system, merged roles, `tool_use`, `tool_result`,
  `is_error`, tools, adaptive thinking/effort, `service_tier`);
- the replay golden: `thinking` with its signature and `redacted_thinking` with
  its `data`, verbatim and in order; foreign owners skipped; malformed payloads
  refused;
- role merging and strict alternation;
- image input (base64 round trip) and capability refusal;
- provider-option validation by name;
- unknown/ignored counters, `ping`, ignored thinking deltas;
- malformed-stream detection, including a thinking block without a signature;
- error classification and the no-echo rule;
- `x-api-key` (and no `authorization`), `anthropic-version`, sensitive-value
  redaction in `Debug`, events, the submitted body and error messages;
- connection release at `Finished` without another poll, and fixture attribution
  for the T3 loopback probe;
- the sourced catalog rows and the `ANTHROPIC_API_KEY` scheme selection (in a
  child process, so no test mutates the environment).

The fixture server registers `tests/support/loopback.rs::SideProbe`, accepts
concurrently, and admits only its random per-server path plus the test run
header; unrelated connections are counted, logged and closed without changing
inference observations. Fixtures are hand-written from the reference with
made-up IDs and no secrets; `text.sse` omits usage so the `Text` case exercises
absent usage, while `usage.sse` carries the realistic `message_start` snapshot
and the cumulative `message_delta` one.

Out of scope for this step: server tools (web search, code execution, web
fetch), the `pause_turn` server-tool loop, prefill (an assistant-final request),
batch, files/PDF input, prompt caching controls, structured outputs, fast mode,
and the preserved-thinking binding-controls beta.

### Break-on-purpose evidence

Each mutation compiled, made the named test fail (Cargo exit 101), and was
reverted before the complete passing suite.

| Temporary mutation | Failing test | Result |
| --- | --- | --- |
| Send the key as `Authorization: Bearer` (`Credential::api_key` → `Bearer`) | `anthropic::tests::the_api_key_is_sent_as_x_api_key_and_never_rendered` | Detected |
| Drop `signature` from a replayed `thinking` block | `anthropic::tests::thinking_and_redacted_blocks_replay_verbatim_in_order` | Detected |
| Alter a replayed `redacted_thinking` block's `data` | `anthropic::tests::thinking_and_redacted_blocks_replay_verbatim_in_order` | Detected |
| Emit `ToolCallReady` in `content_block_start` | `every_conformance_case_passes_on_native_fixtures` (`one_tool: tool_json_object`) | Detected |
| Map `overloaded_error` to `rate_limit` | `every_conformance_case_passes_on_native_fixtures` (`provider_error: normalized_outcome`) | Detected |
| Include the key in the invalid-credential error | `anthropic::tests::the_api_key_is_sent_as_x_api_key_and_never_rendered` | Detected |

## Dependencies

None added. The adapter uses the existing transport, SSE reader, credentials,
`serde_json` and `base64` (already a provider dependency for images).
