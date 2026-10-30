# Context builder

Status: Milestone 2, step 5 is implemented as `harness_core::context`: a pure
builder, its report and fingerprint, the core's use of it in `Preparing`,
`SessionHandle::inspect_context` and the read-only `harness context` command.
[Decision 0006](decisions/0006-context-builder.md) records the design.
Milestone 4 step 01a adds tool definitions, inference options and validated
native continuation selection ([decision 0014](decisions/0014-model-seam.md)).
Milestone 4 steps 05–06 add rolling summaries, manual preview/commit/cancel,
inspection diffs and idle-only automatic triggering
([decision 0019](decisions/0019-compaction.md)). Both native wire adapters are
implemented in `harness-providers`; see [providers](providers.md).

## Three layers

| Layer | What it is | Where it lives |
| --- | --- | --- |
| History | The append-only journal. Never changes. | `JournalStore`, `SessionSnapshot` |
| Projection | What the model should see, derived purely from one branch of history | `Projection`, `ModelInput { model, items, tools, options, continuation }` |
| Provider request | The projection in one provider's wire format | Native adapters, Milestone 4 |

`ContextBuilder::build` is the only path from history to anything a provider
sees. It returns the projection and a `ContextReport` from the same walk, so the
report cannot disagree with the projection. Neither is persisted: both can
always be rebuilt from history.

```rust,ignore
let builder = ContextBuilder::new(&config, &session_configuration).with_tools(&definitions);
let built = builder.build(&snapshot, head, model_override)?; // BuiltContext { projection, report }
```

The builder reads only the snapshot, the head, a `ContextConfig`, a decoded
`SessionConfiguration` and a `TokenEstimator`. It performs no I/O, reads no
clock, appends nothing and calls no tool or provider. The model is
`snapshot.model_at(head)`; a caller can name another model only as an explicit
preview (`model_override`), which sets the report's `preview` flag.

## Inclusion rules

The builder fetches `ancestry(head)` once, oldest first. A first pass over it
finds summaries, what they supersede and cover, and which calls have results. A
second pass gives each entry exactly one decision, and each part of a
non-empty, non-failed assistant response (every content block and its
continuation) exactly one decision. A response none of whose parts is included
is decided by those part exclusions alone and is no item. Exclusion reasons are
the closed enum `ExclusionReason`.

| Entry or part | Decision | Reason |
| --- | --- | --- |
| User message (text, image, audio, file) | Included as a `user` item | — |
| Assistant response, completed | Included as an `assistant` item if any part is included | — |
| Assistant response, cancelled or interrupted | The same; the user saw its text | — |
| Assistant response with no blocks and no continuation | Excluded | `empty_response` |
| Text and media blocks of an included response | Included | — |
| Tool-call block with a result on the branch | Included | — |
| Tool-call block without a result on the branch | Excluded | `orphan_tool_call` |
| Reasoning-summary block | Excluded; display-only, never replayed | `reasoning_display_only` |
| Provider-state block owned by the projection's model | Included in its response | — |
| Continuation owned by the projection model, from the last included assistant, matching its rebuilt parent prefix | Included in `ModelInput.continuation` | — |
| Earlier continuation | Excluded | `continuation_not_latest` |
| Continuation whose current rebuilt parent prefix differs | Excluded | `continuation_prefix_mismatch` |
| The same, owned by another model | Excluded, listed under `portability` | `provider_state_not_portable` |
| Assistant response, failed | Excluded, with all its parts | `failed_response` |
| Tool result | Included as a `tool_result` item with its `provenance`, directly after its call | — |
| `model_changed` | Excluded from dialogue; it decides the model | `model_change_marker` |
| Compaction summary listed in a later summary's `supersedes` | Excluded | `superseded_summary` |
| Entry covered by a summary that is not superseded | Excluded | `covered_by_summary` |
| Compaction summary otherwise | Included as a `summary` item at the position of its first covered entry | — |

Notes:

- **Orphans.** Storage lets cancelled and interrupted responses keep tool-call
  blocks (storage review, finding 3). They never have results. A completed
  `tool_calls` response at a head inside its tool exchange also has calls
  without results on that branch, as in the `interrupted-execution` fixture.
  Both are `orphan_tool_call`: no provider ever sees an unanswered call.
- **Pairing.** Storage keeps results directly after their response and never
  lets a compaction split a turn, so a call and its result are covered
  together or not at all. The checks below still verify it after every build.
- **Summaries.** `superseded_summary` and `covered_by_summary` carry
  `summary_id`: the first later summary that supersedes the entry, or the
  summary that covers it. A superseded summary that is also covered is reported
  as superseded. A summary that is not superseded but whose own entry is covered
  by a later summary is excluded as covered; the entries it covers still name
  it. Only summaries on the selected branch apply.
- **Effective summary.** When `summary_id` is itself superseded or covered, the
  exclusion also carries `effective_summary`: the summary that stands for the
  entry in the projection, found by following those links until one is a
  projection item. It is omitted when `summary_id` is that item, as it always
  is without nesting.
- **No empty items.** A response whose every part is excluded, for example a
  cancelled response holding only reasoning and an unanswered call, is no item:
  its part exclusions account for it. A response with no parts at all is
  excluded as `empty_response`. An `assistant` item has content or is the source of the valid selected
  request-level continuation. Invalid continuation-only assistants are omitted.
- **Provenance.** A `tool_result` item carries the result's `provenance`:
  `executor`, `runtime`, `user_observed` or `user_acknowledged_unknown`. A
  result the user supplied while resolving an uncertain invocation (see
  [recovery](runtime.md#recovery)) is shown to the model in later turns, so the
  model must be able to tell it from real tool output. The same value is in
  `result.metadata.provenance`; the top-level field makes it explicit, and a
  check rejects a mismatch. Both native adapters label status and provenance in deterministic tool-result
  text; Anthropic also sets `is_error`.
- **Result codes.** A `tool_result` item leaves out the stored v2
  `metadata.code`, in the item and in the hashed bytes. The model reads the
  code through the text, which leads with it. A result written as v1 or as v2
  therefore projects identically, and the code did not need a projection
  version.
- **Siblings.** Entries off the selected branch never appear, in either list.
  RFC-001's `other_branch` reason is not used; inspect another branch by
  passing its head.

## Instructions

The session's configuration artifact may hold an `instructions` field: an
ordered array of `{"level": "system" | "developer", "text": "..."}`, as in
RFC-001 section 3.1. The artifact must be one JSON object without duplicate
keys. The builder also parses `inference` as described below; unrelated fields
are ignored here. `SessionConfiguration::load` reads and
decodes it, which is I/O the builder never performs. The storage worker does it
once when a session opens, and `harness context` does it under its shared lock.
An open fails with `invalid_instructions` or `configuration_not_an_object`
rather than projecting without them.

When the array has entries, the projection starts with one `instructions` item,
estimated and counted like any other. The report's `instructions` section is
`{"status": "included", "count", "estimated_tokens"}`, `{"status": "empty"}`
for `[]` (the RFC-001 fixtures), or `{"status": "absent"}` when the field is
missing. The builder refuses a `SessionConfiguration` decoded from any artifact
other than the snapshot's current configuration (`configuration_mismatch`).

## Tools, options and continuation

The runtime supplies enabled and backend-available definitions through
`ContextBuilder::with_tools`; callers inspecting outside the runtime must supply
the corresponding definitions themselves. Empty is the default. The same
input is fingerprinted and sent; definitions are no longer a separate effect
field. Tool headroom still accounts for definition/wire overhead.

The optional session `inference` object accepts `reasoning_effort`
(`low | medium | high`, nullable/optional) and `provider_options`, an object
mapping nonempty provider namespaces to objects. Malformed/unknown fields fail
with `invalid_inference`. `max_output_tokens` is not independently configured:
it equals `ContextConfig.output_reserve`. Namespaced options pass through intact;
adapters must reject unsupported required options, never silently drop them.

The selected continuation is request-level, not attached to assistant items.
The builder rebuilds the last included assistant response's parent under current
instructions, tools, options and model, reproducing prior valid
continuation-only assistants, and
compares that fingerprint to `input_fingerprint`. Only a match with the same
owner is replayed. A later assistant without a handle prevents fallback. A
changed branch, instructions, tools, options or model can therefore invalidate
replay. Reports explain omissions with the closed reasons above; covered or
failed responses keep their existing whole-entry exclusion.

## Budget and estimates

`ContextConfig` is runtime configuration, part of `RuntimeConfig::context`:

| Field | Default | Meaning |
| --- | --- | --- |
| `catalog` | empty | `ModelCatalog`: `ModelRef -> ModelLimits { context_tokens, max_output_tokens }` |
| `output_reserve` | 4,096 | Tokens kept for the response |
| `tool_headroom` | 8,192 | Tokens kept for tool definitions and call overhead |
| `safety_margin` | 4,096 | Tokens kept for estimation error and wire overhead |
| `compaction_trigger_permille` | 850 | 0.85, as thousandths; must be 1–1000 |
| `estimator` | see below | `HeuristicEstimator` media values |

The defaults match the RFC-001 example configuration's reserves. Limits come
only from the catalog; the provider is never consulted, and the
configuration artifact's own `context_limit` is not read.

```text
usable = context_tokens - output_reserve - tool_headroom - safety_margin   (0 if negative)
```

- A model missing from the catalog, or with `context_tokens: None`, has an
  **unknown** budget: `{"status": "unknown", "reason": "model_not_in_catalog" |
  "context_limit_not_configured", ...}` and the `budget_unknown` flag. There is
  no overflow check and no compaction flag. A limit is never guessed.
  Inspection and `harness context` report it; the core refuses to infer with it
  and fails the turn with `context_budget_unknown` (RFC-001 C01).
- **Overflow:** estimated total `>` usable. `overflow_tokens` says by how much.
  Before C1 the core compacts once while idle if configured and eligible, or
  rejects `StartTurn` with `context_budget_exceeded` and its `NotEligible`
  reason. No user entry or turn is written on rejection. A later tool-loop
  overflow fails the active turn with `context_budget_exceeded`, without
  compaction inside the turn.
- **Compaction recommended:** estimated total `>=`
  `ceil(usable * compaction_trigger_permille / 1000)`. With automatic compaction
  configured, a terminal turn acknowledgement checks this flag while idle and
  makes at most one generation/commit attempt. No retry loop follows a failure.

**Estimates.** `TokenEstimator` returns `Estimate { tokens, method }`; the
builder refuses mixed methods. `HeuristicEstimator` (method `heuristic`,
version `bytes4-flat-media-v1`) estimates text as `ceil(utf8_bytes / 4)` and
each image, audio or file block at a configured flat value (defaults 1,600,
2,000 and 4,000). A tool call is its name plus its compact JSON arguments. A
tool result is its text and media. Estimates cover content only, not wire
overhead; that is what `safety_margin` is for. Opaque provider state and
continuations are never estimated: their artifact bytes are reported as
`opaque_bytes`, `opaque_provider_state_bytes` and `unestimated_opaque_bytes`.

**Reported usage.** If the most recent completed response on the branch
recorded usage, `reported_usage` shows it with `basis: "provider_reported"` and
that response's entry ID. It describes the request that produced that response,
not the projection being built, and is never added to an estimate. A newer
completed response without usage hides older figures.

## Report

`ContextReport` is plain data with stable Serde serialization. Field names
label every number: `estimated_*` (from the named estimator), `*_bytes`
(opaque, not estimated) and `reported_usage` (from a provider).

| Field | Contents |
| --- | --- |
| `identity` | `session_id`, `journal_sequence`, `head`, `model`, configuration digest, `builder_version`, `projection_version` |
| `fingerprint` | The projection's fingerprint |
| `instructions` | `absent`, `empty` or `included` |
| `included` | One record per projection item, in order: `entry_id` (`null` for instructions), `kind`, `estimated_tokens`, `opaque_bytes`, for assistant items each included `block`/`continuation` part, and for tool results their `provenance` (omitted for other items) |
| `excluded` | Per entry, block or continuation, in branch order: `entry_id`, `part` (`entry`, `block` with `index`, or `continuation`), `reason`, `summary_id`, and `effective_summary` under nested summaries |
| `summaries` | Each summary that no later one supersedes: covered entry IDs, superseded IDs, `included`, estimate |
| `totals` | `estimator` (method, version, parameters); estimated instructions, user text, assistant text, tool calls, tool results, summaries, media and total; `opaque_provider_state_bytes` |
| `budget` | `known` (limits, each deduction, usable, estimated, headroom, overflow, trigger, threshold) or `unknown` (reason) |
| `reported_usage` | Latest completed response's provider usage, or `null` |
| `portability` | Provider state and continuations excluded as another model's, with owner, schema and size; never the payload |
| `preview` | With a model override: `model_at_head`, `preview_model`, `would_stop_being_portable`, `would_become_portable` and `budget_at_head`; otherwise `null` |
| `flags` | `budget_unknown`, `overflow`, `compaction_recommended`, `preview` |

**Checks.** After every build, in every build profile, the builder verifies:
the report describes exactly the projection's items, including each tool
result's provenance, and no `assistant` item is empty; every branch entry is decided exactly once, by one entry-level
decision or, for a response with no included part, by its part exclusions
alone; every block and continuation of a response decided by parts has exactly
one part decision; nothing from another branch appears; only summary reasons
carry a `summary_id` or `effective_summary`, each names a summary on the
branch, and they differ;
every included call has exactly one included result directly after its
response, and every result has its call. A violation is
`ContextError::Invariant(code)`, a builder bug. The core faults on it.

## Fingerprint

`fingerprint = SHA-256` of the canonical JSON of
`{"items": [...], "model": {...}, "tools": [...], "options": {...}, "projection_version": N}`. Canonical means the
compact `serde_json` serialization of these types: struct fields in declared
order, enum tags first, map keys sorted bytewise, no whitespace. Map keys (including
tool-call arguments, tool schemas and provider options) are sorted because `serde_json`'s `preserve_order` feature
is off; a test fails if it is ever enabled. Items carry entry IDs, so identical
text on different branches has different fingerprints.

Only the logical projection is hashed, never the report, response IDs,
continuation or deadline. Tools and inference options are hashed. The same snapshot branch, head, model and configuration give the same
fingerprint on any machine and after any reopen. Entries are immutable; rebuilding an earlier head gives the same fingerprint
when current instructions, tools and options are unchanged.

**`PROJECTION_VERSION` policy.** It is hashed into every fingerprint. Bump it
whenever an inclusion rule, an item type or its serialization changes, so old
and new fingerprints can never collide. `BUILDER_VERSION` versions the report
and checks; bump it when report fields change. Both are 4. Version 2 omits
responses with nothing to project, and adds `empty_response`,
`effective_summary` and the `budget_unknown` flag. Version 3 adds
`provenance` to `tool_result` items and to their report records. Version 4
adds tools/options to the fingerprint and selects a single validated continuation
outside assistant items; builder version 4 checks/accounts for that selection.

## Integration

**Core.** The core builds whenever a turn enters `Preparing`: after C1 for the
first inference and after `continuing>preparing` for later ones. A pending
cancel or the inference limit is handled first.

1. It emits `ContextBuilt { turn_id, head, fingerprint, totals, flags }`.
2. With an unknown budget it commits `turn_finished(failed,
   context_budget_unknown)` from `Preparing`; on overflow,
   `turn_finished(failed, context_budget_exceeded)`. Either way there is no C2 and no
   provider call.
3. Otherwise it keeps the projection and commits C2. After C2's ack it emits
   `StartInference { head, input, fingerprint, ... }`. C2 adds no entries, so
   the projection is still valid; the core checks it was built for the same
   head and is not a preview.
4. Every response the inference persists, whatever its outcome, records
   that fingerprint as `projection_fingerprint`, so an inspector can rebuild
   exactly the context that produced it.

`Runtime::new` takes the decoded `SessionConfiguration`. `Runtime::inspect_context`
builds a report from the published snapshot.

**Handle.** `SessionHandle::inspect_context(head: Option<EntryId>,
model_override: Option<ModelRef>) -> Result<ContextReport, InspectError>`.
`None` means the current head. The driver answers between steps, like `view()`.
Any entry can be the head, so a client can inspect another branch without
navigating. Nothing is persisted and the head never moves. An unknown head is
`InspectError::Context(ContextError::UnknownHead)`; a stopped driver is
`InspectError::Closed`.

**CLI.** `harness context <data-root> <session-id> [--head ID] [--model
PROVIDER/MODEL] [--context-tokens N]` prints the report as pretty JSON. It
uses `JournalStore::inspect` under a shared lock; if a running engine holds the
session it exits at once with a message saying so. `--model` builds a preview.
`--context-tokens` sets the limit of the projected model; without it the budget
is reported as unknown, which is a report, not an error. A journal with an invalid suffix is reported from its valid prefix,
with a warning on stderr. See [development](development.md#context-inspection).

## Compaction previews

`context::compaction::select_span(snapshot, head, config)` is pure. It selects
whole completed turns before at least the two newest completed turns and the
current turn. A rolling span includes prior active summary coverage and explicitly
supersedes applicable ancestor summaries. Sibling summaries never apply.
`NotEligible` explains empty history, only recent turns, unresolved work, no new
dialogue since a summary, a non-completed turn barrier, an unknown head or invalid
policy. Pins are not implemented and have no exclusion policy yet.

The optional session configuration object is independently shape-checked:

```json
{"compaction":{"summary_model":{"provider_instance":"summary","model":"cheap"},"max_summary_tokens":2048,"retain_recent_turns":2,"automatic":true}}
```

Without `summary_model`, selection still works but generation returns
`summary_model_unconfigured`; the session model is never used by default.
`max_summary_tokens` defaults to 2048 and must be positive; retention defaults
to 2 and cannot be lower. Malformed compaction configuration fails loading as
`invalid_compaction`. `automatic` defaults to true when `summary_model` is
configured and false otherwise; explicitly enabling it without a summary model
is invalid. `automatic: false` keeps manual preview/commit available, but refuses
overflowing starts rather than silently generating a summary.

`RollingSummary::new(Arc<dyn ModelProvider>)` implements `CompactionStrategy`.
`propose(CompactionInput, OperationControl)` uses the existing builder's source
projection and prior summaries, fixed structured-section instructions, no tools
and no session-owned native state or continuation. Cancellation and deadlines
return `Cancelled` without a preview. Provider failures, refusals, empty,
over-budget or malformed summaries are coded errors, not truncated results.

The result is data only: the unchanged proposed compaction entry, exact manifest
artifact bytes and reference, session-model before/after reports, and bindings
for head, journal sequence, session model, source and strategy/configuration
policy fingerprints. `session::compaction_manifest` shares provenance construction
with storage validation. The hypothetical after snapshot uses the existing
builder, not a second summary placement algorithm. Its journal sequence stays
at the base sequence until a future real commit allocates one. Generating a
preview publishes nothing and cannot write a journal or run a tool.

### Runtime commitment, inspection diff and recovery (m4-06)

Generation is exclusively `Effect::ProposeCompaction` in the pure runtime and
`RollingSummary::propose` in the driver, bounded by an independent cancellation
token and `RuntimeConfig.compaction_deadline` (300 seconds by default). Manual
`PreviewCompaction` replies with a fresh ephemeral `preview_id` and the preview.
`CommitCompaction { preview_id }` rebuilds the manifest from the current snapshot
and rechecks all five bindings before any publication. `stale_preview` names
the differing field. Success publishes/syncs the manifest before appending the
exact preview entry, while idle. `CancelCompaction` or an accepted `StartTurn`
cancels a running generation; late answers cannot restore it.

`CompactionDiff::between(before, after)` reports newly covered entries,
newly superseded summaries, complete per-category `Totals` before/after, and
`fits_budget` (false for unknown budgets or overflow). The diff may show a token
increase: no saving is fabricated. It is included in manual preview replies,
automatic preview/commit events and `inspect_context_preview(preview_id)` on the
runtime/handle. These inspection APIs and `prepare` never call the strategy.
The deterministic m4-05 scenario has a separate
`context/golden/compaction-preview-diff.json` golden alongside its two reports.

Previews are single-use and memory-only; any session commit, fault or restart
discards them. A crash during generation leaves no artifacts or journal changes.
A crash after manifest publication but before append leaves only an unreferenced
content-addressed artifact. After summary append but before reply, reopen sees
one summary and is idle; retrying the old preview ID is stale, never another
append. The ID-only commit command has no durable command receipt, whereas the
subsequent `StartTurn` retains its normal C1 receipt and retry behavior. Automatic
generation never runs merely because a session was reopened or inspected.

See [runtime compaction commands](runtime.md#compaction-commands-and-automatic-triggers)
for pre-C1 budget checking, pending-start deduplication, events and failure codes.

## Not implemented

- Real tokenizers (estimates remain explicitly heuristic).
- Pinning and alternative strategies (Milestone 6).

## Tests

`context::tests` checks golden reports for the three RFC-001 fixtures in
`crates/core/src/context/golden/`, built as `harness context` builds them with
`--context-tokens 128000`. The CLI smoke test in `scripts/check.sh` must print
the compaction golden file byte for byte. Journals appended through
`JournalStore` cover partial responses with orphan calls, answered and
unanswered calls, provider state under previews and switches, superseded and
nested summaries, instructions, budgets, reported usage, media and the
invariant checks themselves. Runtime scenarios, driver tests and the
randomized test are described in [runtime](runtime.md#tests).
