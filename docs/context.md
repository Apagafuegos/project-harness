# Context builder

Status: Milestone 2, step 5 is implemented as `harness_core::context`: a pure
builder, its report and fingerprint, the core's use of it in `Preparing`,
`SessionHandle::inspect_context` and the read-only `harness context` command.
[Decision 0006](decisions/0006-context-builder.md) records the design.
Summary generation, compaction commands, tool definitions, inference options
and native wire formats are not implemented; see [Not implemented](#not-implemented).

## Three layers

| Layer | What it is | Where it lives |
| --- | --- | --- |
| History | The append-only journal. Never changes. | `JournalStore`, `SessionSnapshot` |
| Projection | What the model should see, derived purely from one branch of history | `Projection`, `ModelInput { model, items }` |
| Provider request | The projection in one provider's wire format | Native adapters, Milestone 4 |

`ContextBuilder::build` is the only path from history to anything a provider
sees. It returns the projection and a `ContextReport` from the same walk, so the
report cannot disagree with the projection. Neither is persisted: both can
always be rebuilt from history.

```rust,ignore
let builder = ContextBuilder::new(&config, &session_configuration);
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
| Provider-state block or continuation owned by the projection's model | Included, attached to its response | — |
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
  excluded as `empty_response`. An `assistant` item always has content or a
  continuation.
- **Provenance.** A `tool_result` item carries the result's `provenance`:
  `executor`, `runtime`, `user_observed` or `user_acknowledged_unknown`. A
  result the user supplied while resolving an uncertain invocation (see
  [recovery](runtime.md#recovery)) is shown to the model in later turns, so the
  model must be able to tell it from real tool output. The same value is in
  `result.metadata.provenance`; the top-level field makes it explicit, and a
  check rejects a mismatch. How each provider labels it is left to the
  Milestone 4 adapters.
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
keys; other fields are ignored here. `SessionConfiguration::load` reads and
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
  The core fails the turn with `context_overflow`.
- **Compaction recommended:** estimated total `>=`
  `ceil(usable * compaction_trigger_permille / 1000)`. Nothing acts on it until
  summaries are generated in Milestone 4.

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
`{"items": [...], "model": {...}, "projection_version": N}`. Canonical means the
compact `serde_json` serialization of these types: struct fields in declared
order, enum tags first, map keys sorted bytewise, no whitespace. Map keys (only
tool-call arguments) are sorted because `serde_json`'s `preserve_order` feature
is off; a test fails if it is ever enabled. Items carry entry IDs, so identical
text on different branches has different fingerprints.

Only the projection is hashed, never the report, response IDs or request
options. The same snapshot branch, head, model and configuration give the same
fingerprint on any machine and after any reopen. Entries are immutable, so a
later snapshot rebuilds the same fingerprint for an earlier head.

**`PROJECTION_VERSION` policy.** It is hashed into every fingerprint. Bump it
whenever an inclusion rule, an item type or its serialization changes, so old
and new fingerprints can never collide. `BUILDER_VERSION` versions the report
and checks; bump it when report fields change. Both are 3. Version 2 omits
responses with nothing to project, and adds `empty_response`,
`effective_summary` and the `budget_unknown` flag. Version 3 adds
`provenance` to `tool_result` items and to their report records.

## Integration

**Core.** The core builds whenever a turn enters `Preparing`: after C1 for the
first inference and after `continuing>preparing` for later ones. A pending
cancel or the inference limit is handled first.

1. It emits `ContextBuilt { turn_id, head, fingerprint, totals, flags }`.
2. With an unknown budget it commits `turn_finished(failed,
   context_budget_unknown)` from `Preparing`; on overflow,
   `turn_finished(failed, context_overflow)`. Either way there is no C2 and no
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

## Not implemented

- Summary generation and manual or automatic compaction (Milestone 4). The
  builder only honors summaries already in the journal.
- Tool definitions, inference options and continuation selection in
  `ModelInput`. Each response's own continuation is attached to its item;
  choosing which one a request uses belongs to native adapters.
- Native wire formats (Milestone 4) and real tokenizers.
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
