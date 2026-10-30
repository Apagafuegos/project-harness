# 0017: The OpenAI Responses adapter

Status: Implemented for Milestone 4, step 03. One wire format, two endpoint
profiles. No core type changed except the redaction and conformance amendments
recorded in ADR 0014 (M4 decisions 15 and 16).

Date: 2026-10-30

## Decision

`harness_providers::openai::OpenAiResponses` implements the existing
`ModelProvider` seam over the OpenAI **Responses** streaming API. It is a pure
request/event mapping plus the shared transport from ADR 0016: no retry, no
hidden state, no task of its own beyond the transport's reader. `harness-core`
and `harness-cli` are untouched by this step, apart from the two narrow `model/`
amendments listed in ADR 0014.

Requests are stateless (M4 decision 14): every request sends `store: false` and
never uses `previous_response_id`. Reasoning crosses turns only as
owner-scoped encrypted `ProviderState`.

Sources for the profile facts below, all read at
**`openai/codex` commit `f752c97f7d0be0cbef5cece327f52a16ad6fb1c6`**:

- `codex-rs/model-provider-info/src/lib.rs`: `CHATGPT_CODEX_BASE_URL`.
- `codex-rs/codex-api/src/endpoint/responses.rs`: `POST /responses`,
  `Accept: text/event-stream`.
- `codex-rs/login/src/auth/default_client.rs`: `DEFAULT_ORIGINATOR`.
- `codex-rs/codex-api/src/common.rs`: the `ResponsesApiRequest` field set.
- `codex-rs/core/src/client.rs`: `store: false`, `stream: true`,
  `include: ["reasoning.encrypted_content"]`, instruction prefixing.
- `codex-rs/core/src/context/base_instructions.rs`: developer-role fragments.
- `codex-rs/models-manager/models.json`: ChatGPT-side context windows.

## Endpoint profiles

| | `Platform` | `ChatGptCodex` |
| --- | --- | --- |
| Endpoint | `https://api.openai.com/v1/responses` (official Responses reference) | `https://chatgpt.com/backend-api/codex/responses` (`CHATGPT_CODEX_BASE_URL` + `/responses`) |
| Credential | `EnvApiKey("OPENAI_API_KEY")` | `CodexLogin` (ADR 0016) |
| Extra headers | `Accept: text/event-stream` | `Accept: text/event-stream`, `originator: codex_cli_rs` |
| Account header | none | `chatgpt-account-id`, supplied by `CodexLogin` |
| Instructions | top-level `instructions` string | leading `developer` input item, as the Codex client does |
| `max_output_tokens` | sent from `InferenceOptions` | **omitted**: the Codex request struct has no such field |
| `tool_choice` | only when tools are present | always `"auto"` (the field is mandatory in the Codex struct) |
| `parallel_tool_calls` | omitted (platform default is `true`) | always `true`, as the Codex client sets it |
| `store`, `stream`, `include` | `false`, `true`, `["reasoning.encrypted_content"]` | identical |
| Tools | `function` definitions | identical |
| 401 diagnostic | `provider authentication failed` | same, plus `run \`codex login\`` |

Two differences are deliberately **not** modelled, with reasons:

- **Session and thread headers.** `session-id`, `thread-id` and
  `x-client-request-id` are Codex-CLI bookkeeping for its own conversation
  store. Stateless harness requests have no equivalent identity to send, and the
  adapter must not invent one.
- **`text` and `client_metadata`.** Both are optional in the Codex request
  struct and carry CLI-specific controls (verbosity, internal telemetry) that
  belong to a client, not to this seam.

Instructions are the one structural difference taken from the source: Codex puts
its base instructions in the input as a `developer` message
(`base_instructions.rs`: `role() == "developer"`, body `input_text`) and the
platform documents a top-level `instructions` field. `ChatGptCodex` therefore
prefixes one `developer` message with the joined instruction texts. On both
profiles instruction texts keep their projection order and are joined with a
blank line.

## Request mapping

`request::build` is a pure function from `ModelInput` (plus already-read artifact
bytes) to the JSON body. Nothing in it performs I/O, reads a clock or sees a
credential.

| Input | Body |
| --- | --- |
| `model` | `model` |
| `ContextItem::Instructions` | `instructions` (Platform) or a leading `developer` message (ChatGptCodex) |
| `ContextItem::User` text | `{"type":"message","role":"user","content":[{"type":"input_text",…}]}` |
| `ContextItem::User` image | `{"type":"input_image","image_url":"data:<mime>;base64,…"}`, only if the model's sourced capabilities include images |
| `ContextItem::User` audio/file | `unsupported_capability` (no catalogued OpenAI model accepts them) |
| `ContextItem::Assistant` text | `{"type":"message","role":"assistant","content":[{"type":"output_text",…}]}` |
| `ContextItem::Assistant` `ToolCall` | `{"type":"function_call","call_id":<native id>,"name":…,"arguments":"<compact JSON>"}` |
| `ContextItem::Assistant` `ProviderState` | the stored reasoning item, replayed verbatim, same owner only |
| `ContextItem::Assistant` `ReasoningSummary` | dropped: display-only, never resent as thought |
| `ContextItem::ToolResult` | `{"type":"function_call_output","call_id":<native id>,"output":<text>}` |
| `ContextItem::Summary` | user message `Summary of earlier conversation:\n\n…` |
| `tools` | `{"type":"function","name","description","parameters"}` per definition |
| `options.max_output_tokens` | `max_output_tokens` (**`Platform` only**) |
| `options.reasoning_effort` | `reasoning.effort` (`low`/`medium`/`high`), only for models whose sourced capabilities allow it |
| `openai.reasoning_summary` | `reasoning.summary` (`auto`/`concise`/`detailed`) |
| `openai.prompt_cache_key` | `prompt_cache_key` |
| `openai.service_tier` | `service_tier` |

Fixed fields: `store: false`, `stream: true`,
`include: ["reasoning.encrypted_content"]`, and `tool_choice: "auto"` whenever
tools are present. `ChatGptCodex` additionally always sends `tool_choice` and
`parallel_tool_calls: true`, and never sends `max_output_tokens`.

**The output budget on the subscription profile.** `InferenceOptions`
`.max_output_tokens` is required by the seam, but the ChatGPT Codex backend's
request vocabulary has no output-budget field: `ResponsesApiRequest` in
`codex-rs/codex-api/src/common.rs` has no such member and
`codex-rs/core/src/client.rs` never sets one. Following the source for this
profile therefore means the field is not transmitted, and the subscription
backend selects the output budget. The context builder still uses the same
reserve for planning and budget accounting; it is simply not a wire control on
this endpoint. `Platform` sends it, as the official reference documents.

**`strict` is omitted.** OpenAI's strict mode requires every property to be
required and `additionalProperties: false`, which the harness's own schemas do
not guarantee (some have optional fields). Requesting it would make the provider
reject valid harness tools, and the harness already validates arguments against
the same schema in the tool runtime after `ToolCallReady`. The adapter therefore
does not claim a guarantee it cannot keep.

**Tool-result text.** `function_call_output.output` is a deterministic plain-text
rendering: `status`, `provenance`, `exit_code`, `output_truncated`, a blank line
and the result's text. Provenance is explicit so the model can tell a user's
statement from real tool output; media blocks become `[artifact: <mime>]`
placeholders rather than inline bytes.

**Provider options** are namespaced and validated before submission:

| Value | Result |
| --- | --- |
| `openai.reasoning_summary` not `auto`/`concise`/`detailed` | `invalid_request` |
| `openai.<unknown key>` | `invalid_request`, naming the key |
| `openai.*` value not a short nonempty string | `invalid_request` |
| any other namespace, e.g. `anthropic` | `unsupported_capability`, naming the namespace |
| `reasoning_effort` on a model without sourced reasoning support | `unsupported_capability` |
| image input on a model without sourced image support | `unsupported_capability` |

## Event mapping

`events::Mapper` is total over the documented vocabulary. Each row is covered by
a hand-written fixture under `crates/providers/fixtures/openai/` and the public
conformance manifest.

| Responses event | `ProviderEvent` |
| --- | --- |
| `response.created`, `response.in_progress` | `Started` exactly once (lazily, before the first content); `UsageUpdated` when the response carries usage |
| `response.output_item.added` (`message`) | allocates the item's text block |
| `response.output_item.added` (`function_call`) | `ToolCallStarted { name, provider_call_id: item.call_id }` at a new block |
| `response.output_item.added` (`reasoning`) | nothing yet; the summary block is allocated by its first delta |
| `response.output_item.added` (other, e.g. hosted tools) | ignored, counted as unknown |
| `response.output_text.delta` | `TextDelta` |
| `response.refusal.delta` | `TextDelta`; the response still finishes `Refusal` |
| `response.reasoning_summary_text.delta` | `ReasoningSummaryDelta` |
| `response.reasoning_text.delta` | ignored: raw reasoning is never displayed |
| `response.function_call_arguments.delta` | `ToolArgumentsDelta` |
| `response.function_call_arguments.done` | ignored; `output_item.done` is authoritative |
| `response.output_item.done` (`function_call`) | `ToolCallReady` after the assembled fragments parse as one JSON object |
| `response.output_item.done` (`reasoning`, encrypted content present) | `ProviderState` at a **fresh** block index, schema `openai.responses.reasoning.v1`, payload = the item's JSON bytes, ≤256 KiB |
| `response.output_item.done` (`message`, `reasoning` without encrypted content) | nothing |
| `response.output_text.done`, `response.content_part.*`, `response.refusal.done`, `response.reasoning_summary_*` `.done`/`.added`, `response.queued` | ignored, counted as deliberately ignored |
| `response.completed` | `UsageUpdated` (if present), then `Finished { Completed, finish_reason }` |
| `response.incomplete` | `UsageUpdated` (if present), then `Finished { Completed }` with `OutputLimit` (`max_output_tokens`) or `Refusal` (`content_filter`) |
| `response.failed` | `UsageUpdated` (if present), then `Finished { Failed }` with the mapped error |
| `error` | `Finished { Failed }` with the mapped error |
| any other type | ignored, counted in the unknown counter; never fatal |

Finish reason on `response.completed` is `ToolCalls` when the response produced
call blocks (every one of them must be ready), otherwise `Refusal` if refusal
content arrived, otherwise `Stop`.

Malformed streams — a delta for an unknown item, a delta for the wrong block
kind, a second `output_item.added`, invalid JSON in `data`, a fragment for a
ready call, tool arguments that never form one JSON object, a completed response
with an unfinished call, or a stream that ends without a terminal event — become
`Finished { Failed }` with `malformed_stream`. The adapter never invents a
successful finish. `Finished` carries no `NativeContinuation`, ever.

At most one `ProviderState` item per completed reasoning item; its block index is
allocated after any summary block, because decision 12 requires a fresh index and
forbids reusing one. Native payloads are never logged, rendered as text or
included in `Debug` (M4 decision 16).

## Stateless replay

`store: false` and no `previous_response_id` mean the server keeps nothing
between requests, so there is no response handle worth returning:
`NativeContinuation` is never emitted, and the ADR records that the seam's
optional continuation stays `None` for this adapter. The harness journal is the
only history.

Reasoning is carried as `ProviderState` instead:

- **Request side.** `include: ["reasoning.encrypted_content"]` asks for encrypted
  reasoning. On `response.output_item.done` for a reasoning item the adapter
  stores that item's JSON bytes. The streaming reference notes that
  `encrypted_content` may be incomplete while the item is in progress and that
  the `output_item.done` item is the one to replay, which is why only the
  completed item is captured.
- **Replay side.** A projected `ProviderState` block is replayed verbatim into
  `input` when its owner equals the request's model and its schema is this
  adapter's. A foreign owner is skipped, never recovered: the core already
  scopes state, and the adapter must not widen it. An unknown schema for the same
  owner is `unsupported_capability`, because guessing a replay format would be
  worse than failing.
- **Bytes.** Payloads are read through `ModelRequest.artifacts`, the core's
  request-scoped allowlist; nothing else is read. A payload over 256 KiB is
  `limit_exceeded` before it reaches the core, which would enforce the same bound.

## Error refinements

Transport classification from ADR 0016 is preserved. The adapter refines it with
the bounded `TransportError::body()` (never logged) and with the `error`/
`response.error` objects:

| Evidence | Category |
| --- | --- |
| 401/403, `invalid_api_key`, `authentication_error`, `token_expired` | `authentication` |
| 429, `rate_limit_exceeded`, `insufficient_quota`, `usage_limit_reached`, `billing_hard_limit_reached` | `rate_limit` |
| context-length hint in a code or message, or the transport's `context_overflow` | `context_overflow` |
| `invalid_request_error`, `invalid_prompt`, `invalid_value`, `missing_required_parameter`, `unknown_parameter`, `model_not_found` | `invalid_request` |
| `server_error` and everything else | `provider_failure` |
| invalid UTF-8/framing/truncation, or a mapping violation | `malformed_stream` |
| event or response bound | `limit_exceeded` |
| cancellation or deadline (`TransportError::is_stopped`) | `Finished { Cancelled }` |

Authentication and 429 status take precedence over body hints, as ADR 0016
requires. `provider_code` is the provider's own code when a body supplies one,
otherwise the transport's `http_<status>`; `retry_after_ms` comes from
`retry-after-ms`/`Retry-After`. Diagnostics are fixed strings per category — the
provider's message is never echoed, because it can quote request content. On
`ChatGptCodex` an authentication failure appends `run \`codex login\``.

There is **no automatic retry** (M4 decision 8): a rate limit fails the turn with
its `retry_after_ms` and the user starts a new turn.

## Catalog sources

`openai::catalog(profile, instance)` returns `ModelCatalog` entries built from
one table, and `capabilities(profile, model)` is the adapter's own validation
input. An uncatalogued model is text-plus-tools with no reasoning control, so the
adapter never offers a control it cannot verify.

| Model | Profile | Context | Max output | Capabilities | Source |
| --- | --- | --- | --- | --- | --- |
| `gpt-5.5` | `Platform` | 1,050,000 | 128,000 | image input, reasoning effort, tools | `developers.openai.com/api/docs/models/gpt-5.5` |
| `gpt-5.5` | `ChatGptCodex` | 272,000 | unknown | image input, reasoning effort, tools | `openai/codex` `models-manager/models.json`, `"context_window": 272000` |

The ChatGPT-side output limit is left **unknown, not invented**: the Codex model
catalog carries no `max_output_tokens` for this model, and the backend's client
sends no output-budget field at all. Unknown is the honest value; `ModelLimits`
treats `None` as unknown rather than zero, and the context builder still refuses
to infer when a *context* window is unknown (here it is known). Applications must
use `openai::catalog` for the provider instance so the builder and the adapter
agree; a different context window on the two profiles for the same model name is
the reason the catalog is profile-aware.

## Opt-in live check

`scripts/live-check.sh openai [--auth codex|api-key]` runs
`examples/live_openai.rs` through the real adapter. It is never part of
`check.sh` or CI. Default auth is `codex` (the subscription login); `--auth
api-key` uses `OPENAI_API_KEY`. The model is `HARNESS_LIVE_OPENAI_MODEL`,
defaulting to `gpt-5.5`, the highest-priority list-visible model in the Codex
catalog and available to every plan the catalog names. It covers streaming text,
one tool-call round trip with a scripted approval, usage, an invalid model name
and a cancellation mid-stream, prints a pass/fail summary only, and never prints
tokens, account IDs or response IDs.

Observed on 2026-10-30, twice (once before and once after aligning the profile
with the Codex request field set), on this machine's ChatGPT login:

- `CodexLogin`, the endpoint, the `originator` header and the request shape all
  work: the backend returns normalized JSON errors that the adapter maps, rather
  than transport or authentication failures.
- Both runs ended `invalid_request`; the second showed
  `provider_code: model_not_found` for `gpt-5.5`, so this account's subscription
  does not serve that model even though the shipped catalog lists it for every
  plan. The text, tool and cancellation cases therefore did not complete.
- The invalid-model case passed in both runs, exercising real error mapping.

This is a model-availability fact, not a request-shape defect: with the
source-conformant body the backend reaches model resolution and reports the
model, which it could not do for a body it rejected outright. The default stays
as sourced; users point `HARNESS_LIVE_OPENAI_MODEL` at a model their plan
serves, and the check is not retried in a loop.

## Evidence and scope

`crates/providers/src/openai/tests.rs` runs the **complete 17-case public
conformance manifest on both profiles** over hand-written fixtures, through the
real adapter and a loopback fake server (`request_preservation` decodes the
actual submitted JSON). Because `ChatGptCodex` serializes no output budget, its
rows carry `max_output_tokens: 0`, the value the decoded wire request yields;
every field the profile does serialize still has to round-trip exactly. Unit
tests cover the golden Platform and ChatGptCodex request bodies, capability and
provider-option refusal, owner-scoped state replay, unknown/ignored event
counters, malformed-stream detection, error classification, the Codex login
hint, no-retry behaviour and secret redaction. No test reads the real
`~/.codex/auth.json` or opens a network connection outside loopback.

Out of scope for this step: hosted tools (`web_search`, `file_search`, image
generation), audio/file inputs, `strict` tool schemas, WebSocket transport,
`background` mode and any server-side conversation state.
