# 0024: The DeepSeek profile of the Messages adapter

Status: Implemented for Milestone 6, step 00 (branch `m6-deepseek`). No core
change: the existing model seam covers a second Messages-compatible backend
unchanged.

Date: 2026-10-31

## Decision

DeepSeek serves an **Anthropic-compatible Messages API**. Milestone 6's live
experiments run on DeepSeek (M6 settled decision 2), so the harness gains a
`deepseek` provider instance — but not a second wire format and not a second
adapter (M6 decision 10).

`harness_providers::anthropic::AnthropicMessages` gains an explicit `Profile`,
chosen at construction:

```rust,ignore
pub enum Profile { Anthropic, DeepSeek }

AnthropicMessages::from_env()          // Profile::Anthropic, ANTHROPIC_API_KEY
AnthropicMessages::deepseek_from_env() // Profile::DeepSeek,  DEEPSEEK_API_KEY
AnthropicMessages::with_profile(profile, credential, transport)
```

The profile is **data plus small mapping hooks**, read by the existing request
and event mapping. There is no branch on a provider name anywhere, no duplicated
adapter, and no change outside `crates/providers` and the wiring's instance
table. The Anthropic profile is byte-for-byte unchanged: every existing
Anthropic test passes with its assertions untouched, the golden request bodies
are identical, and the only edits to that test module are the call sites that now
name `Profile::Anthropic`.

Sources, all read on **2026-10-31**:

- Anthropic-format guide: `api-docs.deepseek.com/guides/anthropic_api` —
  endpoint, authentication, model mapping, the compatibility tables, thinking and
  tool-loop rules, and the streaming and token-usage section.
- Messages reference: `api-docs.deepseek.com/api/create-message` — request
  fields, `thinking`, `output_config.effort`, `tools`, `tool_choice`, `max_tokens`.
- Models and pricing: `api-docs.deepseek.com/quick_start/pricing` — model
  versions, context length, maximum output, feature matrix, vision support.

## Endpoint, credential and headers

| Item | Anthropic profile | DeepSeek profile | Source |
| --- | --- | --- | --- |
| endpoint | `https://api.anthropic.com/v1/messages` | `https://api.deepseek.com/anthropic/v1/messages` | guide: base URL plus `/v1/messages` |
| credential | `ANTHROPIC_API_KEY` | `DEEPSEEK_API_KEY` | guide: `x-api-key` or `Authorization: Bearer` |
| scheme | `x-api-key` | `x-api-key` | guide: "Authenticate with either `x-api-key` … or `Authorization: Bearer`" |
| `accept` | `text/event-stream` | `text/event-stream` | streaming request |
| `anthropic-version` | `2026-10-21` | **not sent** | guide: "Ignored" |
| `anthropic-beta` | only for preserved thinking | **never sent** | guide: "Ignored for `/messages`" |

Both backends accept `x-api-key`, so the profile keeps one credential scheme for
the whole wire format; no new `CredentialScheme` variant was needed. A key is read
once, at construction, and is never printed: `Debug` shows
`credential: "[redacted]"`, `Runtime` debug and every event are sentinel-checked,
and `DEEPSEEK_API_KEY` is in the sentinel-secret tests.

## The control list

Each documented request control the adapter can send, and what this profile does
with it. "Dropped" means the DeepSeek profile never sends it; "kept" means it is
mapped exactly as on the Anthropic profile.

| Control | DeepSeek profile | Why |
| --- | --- | --- |
| `thinking.type` | **kept, different value**: `enabled` when an effort is configured | guide: `enabled`/`disabled` supported, `adaptive` treated as `enabled`; thinking is on by default |
| `output_config.effort` | **kept** (`low`/`medium`/`high`) | reference: "Only `effort` is supported"; the portable effort maps onto its values |
| `thinking.block_binding` | **dropped** | not documented by DeepSeek at all; it is the Anthropic preserved-thinking control, and the API it belongs to is not this one |
| `anthropic-beta: thinking-binding-controls-…` | **dropped** | the guide ignores `anthropic-beta` for `/messages`; the header only ever existed to permit `block_binding` |
| `anthropic-version` | **dropped** | guide: "Ignored". Sending a version the backend never reads would be a control that silently does nothing |
| `service_tier` | **kept** | guide: "Ignored", i.e. documented and accepted. One provider option (`anthropic.service_tier`) keeps one meaning across both profiles, and the field cannot change any DeepSeek model's behavior |
| `is_error` on `tool_result` | **kept** | guide: "Ignored"; the block is the same shape and Anthropic reads it |
| `cache_control` | **never sent by either profile** | guide: ignored; the harness does not use prompt caching controls |
| `top_k`, `top_p`, `temperature`, `stop_sequences`, `metadata`, `container`, `mcp_servers` | **never sent by either profile** | not part of the harness's portable inference options; adding them is a separate decision |
| `tool_choice` | **never sent by either profile** | the adapter never forces a tool; DeepSeek returns 400 for a named tool in thinking mode, so never sending one keeps the profiles uniform |
| `tools[].type` | **never sent by either profile** | guide: omit `type` for a client tool; `type: "custom"` returns 422 |
| `redacted_thinking` blocks | **dropped** (not stored, not replayed) | guide: "Not Supported" |
| `document` blocks | **never sent by either profile** | guide: "Not Supported"; the adapter refuses file input |
| `thinking.budget_tokens` | **never sent by either profile** | guide: ignored; Anthropic rejects it on this family |

The one predicate that decides `thinking.block_binding` and its beta header on the
Anthropic profile still governs both (`request::replays_thinking`), so they can
never disagree — and on the DeepSeek profile the same predicate yields neither.

## Request mapping

`request::build` and `request::collect` now take the profile; everything else is
unchanged, because the wire format is the same.

| Input | DeepSeek body |
| --- | --- |
| model | `deepseek-flash` / `deepseek-v4-pro` (catalogued), or any name the caller passes |
| instructions | `system` string, unchanged |
| user text/image | unchanged; an image needs a sourced capability (see catalog) |
| assistant text / `tool_use` | unchanged; `type` is never added to a tool |
| assistant `ProviderState` | replayed verbatim under this profile's owner and schema (below) |
| `tool_result` | unchanged, `is_error` kept (ignored upstream) |
| summary | user text block, unchanged |
| `options.max_output_tokens` | `max_tokens`, unchanged; DeepSeek documents it as optional and caps values above the model limit |
| `options.reasoning_effort` | `thinking: {"type":"enabled"}` plus `output_config.effort` |
| `anthropic.service_tier` | `service_tier`, unchanged |

**Thinking type.** Anthropic's current family accepts only `adaptive`, so that
profile keeps sending it. DeepSeek documents `enabled`/`disabled` with `adaptive`
treated as enabled and thinking on by default; the profile therefore asks for
`enabled` explicitly when an effort is configured and sends no thinking object at
all otherwise, which is the documented default.

**Tool loops.** DeepSeek requires the client to send the assistant's complete
`content` back — thinking and tool-use blocks — before the matching
`tool_result` blocks, and warns that recovering omitted thinking from call IDs
may fail with a 400. The adapter already replays the projection's assistant
content verbatim and in order (tool results are pushed as their own `user` turn,
merged before any following text), so nothing changed. Recovery is never relied
on: the state is stored and replayed by the harness itself.

## Thinking, signature and replay

State schema: `deepseek.messages.thinking.v1` (`DEEPSEEK_STATE_SCHEMA`), beside
Anthropic's `anthropic.messages.thinking.v1` (`STATE_SCHEMA`).

- **Owner.** A `ProviderState` block is replayed only when its owner equals the
  request's `ModelRef`, whose `provider_instance` is `deepseek` or `anthropic`.
  Core already excludes non-portable state; the adapter skips it again and never
  recovers it.
- **Schema.** A block whose schema is the other profile's is
  `unsupported_capability`, even for the same owner. Schema and owner are two
  independent halves of one guarantee: thinking written by one instance can never
  be replayed through the other, in either direction.
- **Signature.** Anthropic requires a `signature` and fills it with a
  `signature_delta`; an unsigned block is `malformed_stream`. DeepSeek documents a
  `thinking` content block as supported and never mentions a signature, so this
  profile accepts a block with no `signature` (or an empty one) and stores it
  verbatim; a signature that is present must still be a string. A fixture keeps
  each behavior: `fixtures/deepseek/native_state.sse` has no signature at all,
  `fixtures/anthropic/native_state.sse` sends one in a delta.
- **Redacted thinking.** Not part of DeepSeek's vocabulary: the mapper counts the
  block as unknown and stores nothing (so no unreplayable state is ever written),
  and the request builder refuses a stored `redacted_thinking` payload.
- **Dropped controls on replay.** A replaying request adds nothing to the body and
  no beta header. There is no prefix-binding check to relax on this backend, so
  the Anthropic `drop_block` policy has no counterpart and is not sent.

## Event mapping

The Messages vocabulary is shared, so the 17-case mapping table in ADR 0018
applies unchanged with three documented differences.

| Native event | DeepSeek behavior |
| --- | --- |
| `message_start` | `Started`, plus `UsageUpdated` when usage is present (a partial snapshot) |
| `content_block_start` (`thinking`) | fields validated; `signature` optional here |
| `content_block_start` (`redacted_thinking`) | ignored, counted as unknown (not this backend's vocabulary) |
| `content_block_stop` (`thinking`) | `ProviderState`, schema `deepseek.messages.thinking.v1`, payload = the block's JSON bytes, ≤256 KiB; no signature check |
| `message_delta` | stop reason plus `UsageUpdated`: **the final `message_delta.usage` is the request's usage**, replaced not added, so the last snapshot wins and the initial one is never summed |
| unnamed `data` frame with only `{"error":{…}}` | `Finished { Failed }` with a coded `ModelError`: `provider_failure`, no provider code, fixed message `DeepSeek request failed`, never a hang and never a success |
| any other type | ignored, counted as unknown (unchanged) |

The unnamed frame is the documented start-up timeout: no SSE event name, no
top-level `type`, then the connection closes. The mapper accepts it only as an
error object — a frame with neither a `type` nor an `error` is still
`malformed_stream`. A connection that closes before `message_stop` remains
`malformed_stream` (the guide says such a response is incomplete), and with no
`data: [DONE]` there is nothing else to tolerate; keep-alive comment lines are
already ignored by the SSE reader.

`cache_creation_input_tokens` is always 0 upstream and is not part of the stored
`Usage`; `cache_read_input_tokens` becomes `cached_input_tokens`. Note that
DeepSeek's `input_tokens` excludes cache hits, exactly as Anthropic's does, so the
two profiles report the same fields with the same meaning.

## Catalog

`anthropic::catalog(Profile::DeepSeek, instance)` and
`anthropic::capabilities(Profile::DeepSeek, model)` read one table. An
uncatalogued model is text-plus-tools with no reasoning control, as on the
Anthropic profile.

| Model | Context | Max output | Capabilities | Source |
| --- | --- | --- | --- | --- |
| `deepseek-flash` | 1,000,000 | 384,000 | image input (user side), thinking/effort, tools | pricing page rows "CONTEXT LENGTH 1M", "MAX OUTPUT … 384K", "Vision ✓"; guide: images in user messages and `tool_result` |
| `deepseek-v4-pro` | 1,000,000 | 384,000 | thinking/effort, tools; **no images** | pricing page "Vision — Not supported"; guide: "does not process images and replaces them with the text `[Unsupported Image]`" |

The page states one context and one maximum for both models, so the numbers repeat
per row rather than being inferred. `384K` is recorded as `384_000` and `1M` as
`1_000_000`, matching the decimal style already used for the Claude rows.

**Images are user-input-only, and that is exactly what the capability model
expresses.** `input_media` describes media the adapter may *send*; the adapter
only ever serialises images from `ContextItem::User`, and an image inside an
assistant item is already `invalid_request`. `deepseek-flash` therefore lists
`Image`, while its documented rejection of an image inside an *assistant* message
cannot arise. `deepseek-v4-pro` lists no image capability at all, because the
docs say it does not process images.

**Default `max_output_tokens`.** `models::DEFAULT_MAX_OUTPUT_TOKENS` is **4096**.
Both DeepSeek models document a 384K maximum output, and the harness's own
default output reserve is 4096 tokens, so 4096 fits both models with a very wide
margin while leaving the 1M context budget to the conversation. Nothing here
raises the request above the model ceiling, and DeepSeek documents that values
above the limit are capped rather than rejected. The constant exists so a caller
that builds a request itself (the live check) uses one justified number instead
of the ceiling.

**Model-name aliases.** DeepSeek maps unsupported `claude*` names to its own
models. The catalog does not enumerate aliases: they are not models with their own
sourced limits, and every limit in this table is cited to a row of the pricing
page. A caller that passes an alias gets the core's unknown-model default (text
and tools, no reasoning control) and should use `deepseek-flash` or
`deepseek-v4-pro` instead.

## Wiring and CLI

- New instance constant `wiring::DEEPSEEK = "deepseek"`, registered by
  `wiring::providers()` only when `DEEPSEEK_API_KEY` is present, with the existing
  skip-or-fail rule: fatal only when `deepseek` is the selected provider. The
  sourced rows are added under the same name.
- `provider_instance("deepseek", …)` returns `deepseek`,
  `default_model("deepseek", …)` returns `deepseek-flash`, and
  `--provider deepseek` works for `harness ask` and the TUI through the existing
  argument table. A missing key is the existing `authentication` failure with exit
  code 3 and no Codex hint. `--auth` is now refused for every provider that does
  not take one, through `wiring::openai_auth_applies`, so the CLI holds no
  provider-specific branch.
- Switching is unchanged: `deepseek` is just another router instance, and its
  thinking state is optional native state under the existing portability rules —
  omitted or rejected on a switch, never replayed elsewhere.

## Opt-in live check

`scripts/live-check.sh deepseek` runs `examples/live_deepseek.rs` through the real
adapter against a real durable session on loopback-free (real) endpoint. It runs
**one streamed turn with a tool round trip and a thinking replay**: the first turn
reads a workspace file with the `read` tool (so the model must return a thinking
block plus a `tool_use`, then accept the matching `tool_result` and finish), and
the second turn's projection replays the thinking state the first turn stored —
the leg `live_anthropic` cannot exercise, because a standalone example has no
stored artifacts.

Model: `HARNESS_LIVE_DEEPSEEK_MODEL`, default `deepseek-flash`. Without
`DEEPSEEK_API_KEY` the script prints
`live deepseek: skipped (no DEEPSEEK_API_KEY)` and exits **77**, distinct from 0
(pass) and 1 (fail). It prints only pass/fail lines, the model name and stable
turn codes — never a key, account data, session or response ID. It is never part
of `check.sh` or CI.

**This step had no key: the live check is unrun.** The recorded invocation is the
skip path, exactly as for Anthropic in M4. The orchestrator runs it with the key
and reports any mismatch.

## Evidence

`crates/providers/src/anthropic/tests.rs` keeps the Anthropic module untouched
apart from call sites, and adds a `deepseek` module whose every assertion is new:

- the **complete 17-case public conformance manifest** over the profile, through
  the real adapter and the attributed loopback server — the same suite, run the
  same way as for Anthropic;
- the plain-turn body golden (model, `thinking.type: enabled`, effort,
  `service_tier`, messages, tools) and the tool-loop replay golden (unsigned
  thinking block verbatim, `tool_use`, matching `tool_result`, no `thinking`
  object, no `block_binding`);
- the absent Anthropic-only controls, on the body and on the wire
  (`x-api-key` present, no `authorization`, no `anthropic-version`, no
  `anthropic-beta`);
- an unsigned thinking block accepted by DeepSeek on both the stream and the
  replay path, and rejected by Anthropic on both;
- `redacted_thinking` neither stored nor replayed;
- the unnamed error frame as a coded failure, and a typeless non-error frame still
  `malformed_stream`;
- usage from the final `message_delta` (two snapshots, the last one wins);
- owner scoping in both directions, plus the schema refusal and the mapper's own
  schema;
- the catalog rows, the profile constants and the DeepSeek instance name;
- the credential: sentinel key in `x-api-key` and nowhere else, `Debug`
  redaction, `deepseek_from_env()` in a child process (so no test mutates the
  environment).

`crates/cli/tests/ask_cli.rs` adds: help lists `deepseek`; `--provider deepseek`
with no key is `authentication`/exit 3 without a Codex hint and opens nothing;
`--auth` with it is `auth_not_applicable`; and, in child processes,
`wiring::providers` registers `deepseek` only with the key, skips it silently
otherwise and fails with exit 3 when it is selected.

The three DeepSeek-specific fixtures live in `crates/providers/fixtures/deepseek/`
(`native_state.sse` with no signature, `usage.sse` with
`cache_creation_input_tokens: 0`, `unnamed_error.sse`); every other case reuses
the shared Messages fixtures in `fixtures/anthropic/`, because the vocabulary is
identical. Fixtures are hand-written from the guide with made-up IDs and no
secrets.

### Break-on-purpose evidence

Each mutation compiled, made the named test fail, and was reverted before the
complete passing suite.

| Temporary mutation | Failing test | Result |
| --- | --- | --- |
| Send `thinking.block_binding` (and its beta) from the DeepSeek profile | `anthropic::tests::deepseek::the_profile_drops_the_anthropic_only_controls` | Detected |
| Accept unsigned thinking in the Anthropic profile | `anthropic::tests::a_thinking_block_without_a_signature_is_malformed`, `a_modified_thinking_payload_is_not_replayable`, `deepseek::an_unsigned_thinking_block_maps_only_on_deepseek`, `deepseek::an_unsigned_thinking_payload_replays_only_on_deepseek`, `every_conformance_case_passes_on_native_fixtures` | Detected (5 tests) |
| Give both profiles the same state schema | `anthropic::tests::deepseek::state_never_crosses_the_profile_owner_or_schema` | Detected |
| Treat the unnamed error frame as malformed | `anthropic::tests::deepseek::the_unnamed_error_frame_is_a_coded_failure` | Detected |
| Ignore `message_delta.usage` (leave only the `message_start` snapshot) | `anthropic::tests::deepseek::usage_comes_from_the_final_message_delta` | Detected |

## Where DeepSeek's documentation differed from the step's brief

The brief's intent held in every case, so the documentation was followed (the
standing API allowance):

1. **`anthropic-beta` is not unconditionally ignored.** The guide ignores it for
   `/messages` *except* that an image with `source.type: "file"` requires
   `files-api-2025-04-14`. This adapter sends no file images, so the profile sends
   no beta header at all; the exception is recorded above rather than implemented.
2. **Vision is not only "Flash rejects images in assistant messages".** The guide
   also says `deepseek-v4-pro` "does not process images and replaces them with the
   text `[Unsupported Image]`", and the pricing page marks Vision "Not supported"
   for it. The catalog therefore gives images to `deepseek-flash` only, rather than
   to both models with an assistant-side restriction.
3. **`max_tokens` is optional at the HTTP level**, with the effective limit
   depending on model, thinking mode and effort, and compatibility handling for
   certain Claude Code / OpenCode requests that can raise it above the request.
   The adapter keeps sending the harness's configured value, which is what the
   seam asks for.
4. **`output_config.effort` has a wider documented value set** (`none`, `minimal`,
   `low`, `medium`, `high`, `xhigh`, `ultra`, `max`, with aliases). The portable
   `ReasoningEffort` still maps to `low`/`medium`/`high`; the profile does not
   widen the harness's portable vocabulary.
5. **Thinking recovery from tool-call IDs exists but is not relied on.** The guide
   says omitted thinking "may" be recovered and that a failed recovery is a 400.
   The adapter always replays the stored block, so the fallback is never needed.
6. **`tool_result` ordering is stricter than stated.** The guide requires all
   `tool_result` blocks before any text in the following `user` message. The
   adapter's existing merge already emits tool results first, and a compaction
   summary after them; no change was needed, but the requirement is now recorded.
7. **The pricing page and the compatibility page disagree slightly about model
   identity**: the pricing page calls the models `DeepSeek-V4.1-Flash-1017` /
   `DeepSeek-V4-Pro-0813` and marks Vision unsupported for the Pro model, while
   the guide describes the Pro model as replacing images with placeholder text.
   The stricter reading (no image capability) is implemented.

Nothing in the brief's acceptance items, or in a settled M6 decision, was broken
by these differences.

## Out of scope

Server tools (`web_search`, `server_tool_use`), the `document`, `search_result`
and `mcp_*` content blocks, file images and the Files API, `tool_choice`,
prefix completion, FIM, JSON mode, `metadata.user_id`, prompt-cache controls, the
`claude*` model aliases, and any Anthropic-only beta header. Core is untouched:
this step adds a provider, not a runtime feature.

## Dependencies

None added. The profile reuses the existing transport, SSE reader, credential
scheme, `serde_json` and `base64`.
