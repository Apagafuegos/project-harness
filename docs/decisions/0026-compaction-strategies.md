# 0026: Compaction strategy selection and alternative strategies

Status: Implemented for Milestone 6, step 04 (m6-04), amended by
m6-side-summary-request (decision 16: one explicit fresh request for both
model-based strategies, versions 2 and 1) and by m6-07 (decision 18: the chosen
new-session default is `observation_masking`, while decoding an omitted strategy
still means `rolling_summary`). Milestone 6 settled decisions 3, 4, 12 and 18.

Date: 2026-10-31

## Decision

### One recorded strategy, one registry

`CompactionConfig` gains `strategy: StrategyId`, a **closed enum** persisted in
the session configuration object:

```json
{"compaction":{"strategy":"rolling_summary","summary_model":{...},"max_summary_tokens":2048,"retain_recent_turns":2,"automatic":true}}
```

`StrategyId` is `rolling_summary` (the default), `observation_masking` or
`structured_state`. An absent field means `rolling_summary`, so every
configuration written before this step keeps its meaning and no record version
changes; an unknown name is `invalid_compaction`, never a silent fallback. The
name is the journal's `Compaction.strategy` value and the version is
`Compaction.strategy_version` (`"1"` for `observation_masking`, `"2"` for the two
model-based strategies since decision 16's prompt change), so a committed entry
states exactly which strategy wrote it.

The harness records the strategy **at session creation**, in the same
configuration artifact as the tool policy; `--compaction-strategy` selects it.
Changing it later is one recorded, idle-only `ConfigurationChanged`
(`Command::SetCompactionStrategy`), never a rewrite:

- the exact configuration bytes are rewritten by
  `context::with_compaction_strategy` — only `compaction.strategy` and
  `policy_revision` change — and the result must decode through
  `SessionConfiguration::parse` before it is recorded, so an unusable
  configuration can never be installed;
- the new artifact is published before the append and installed after the
  acknowledgement, exactly like a remembered grant;
- a strategy that needs a model is refused (`summary_model_unconfigured`) when
  the configuration has none, so the recorded policy can always generate;
- repeating the current strategy is `strategy_unchanged`; anything while a turn
  is open is `Busy`. Nothing is written on refusal.

`context::compaction::strategy(id, provider)` is the **single registry**: the one
place an id becomes an implementation. The driver's `ProposeCompaction` effect
resolves the id from the input's configuration, so the runtime core has no
strategy-specific branch at all: preview, commit, staleness, cancellation,
automatic triggering and pins/exclusions are the same code path for all three.
The session provider is offered to the registry, but only the model-based
strategies keep it: `ObservationMasking` has no provider field, so its
model-free promise is a type-level fact and not a convention.

`policy_fingerprint` hashes `(strategy name, strategy version, compaction
configuration)`. Because the strategy is part of it, a recorded strategy change
makes every outstanding preview stale, exactly like a summary-model change.
`validate_preview` compares the policy before the journal sequence: a recorded
change advances the sequence too, and the policy is the reason the preview died.

### Shared preparation and assembly

`prepare` and `assemble` in `context::compaction` hold everything the three
strategies have in common: configuration/snapshot binding, `select_span`
(decision 14: the span minus every pinned or excluded turn), unique-identity
checks, the session-model builder report, the narrowed source (covered entries
and superseded summaries only, no continuation, no session-owned native state,
no tools) and, at the end, the manifest, the entry, the hypothetical after
report and the bindings. A strategy therefore supplies only its summary text and
its model: `assemble(input, prepared, summary, model)`.

### The explicit fresh summary request (decision 16, version 2)

`summary_input` — shared by `rolling_summary` and `structured_state`, and used by
no other strategy — appends **one fixed, request-only prompt** after the narrowed
source and the fixed instructions. Its text is the constant
`SUMMARY_REQUEST_INSTRUCTION`, which asks for the configured summary of the
preceding completed source and restates the fixed system instruction's
untrusted-data rule; it interpolates nothing, so it cannot quote a source entry,
include a pinned or excluded turn, or name an artifact.

The prompt is a new provider-independent item variant, `ContextItem::Request
{ text }`. It has no entry identity, because there is no source entry behind it
and no identity is reserved for a prompt: a fixed UUID could collide with an
imported journal's entries, and a generated one would fabricate a source
identity. It carries no provenance, no artifact reference and no capability, and
both adapters map it to one plain user turn, so no wire format, transport or
profile changed. The builder never creates one, so existing projected item bytes,
`PROJECTION_VERSION`/`BUILDER_VERSION`, projection fingerprints, old summary
replay and the journal schema are all unchanged. Every consumer handles it
without access: artifact enumeration adds nothing, masking renders nothing, the
builder's report check rejects it as `request_item_in_projection`, and the eval
renderer prints its literal text.

Why: a completed source can end with an assistant response, and a Messages
request whose final turn is the assistant's is assistant prefill, which the
provider rejects. The first live matrix's eight model-based cells failed exactly
that way; the four masking cells completed. The request therefore has to be
asked for by a user turn, and both model-based strategies ask the same way.
Because the prompt changed, their `StrategyId::version` is **2**; masking stays
**1** because neither its selection nor its rendering changed. A recorded version
is never rewritten: older summaries project and inspect exactly as recorded, and
an outstanding preview of either model-based strategy becomes stale through the
policy fingerprint rather than being silently reused.

### `observation_masking`: deterministic, model-free

Masking keeps the conversation's shape and drops tool result bodies, which are
usually the largest and least durable part of a coding session's context
(observation masking; see Sources). `ObservationMasking::propose` renders the
covered items in order and makes **zero provider calls**; `Compaction.model` is
absent.

Exact output format (version 1, one section header per item, one line per block):

```text
--- user ---
<text blocks verbatim>
[image <mime> <bytes> B sha256:<digest>]
[audio <mime> <bytes> B sha256:<digest>]
[file <name> <mime> <bytes> B sha256:<digest>]
--- assistant (<completed|failed|cancelled|interrupted>) ---
<text blocks verbatim>
[call <name> <compact JSON arguments>]
--- tool result ---
[tool result: <tool> <status> <body bytes> B[ digest sha256:<digest>]]
--- summary ---
<the superseded summary text verbatim>
```

- A tool call is one line: its name and its compact-JSON arguments, bounded to
  512 bytes with an explicit `… (+N B)` marker and the omitted byte count.
- Each tool result body is replaced by the placeholder, which names the tool
  (from the call block that proposed it, `unknown` if that block is not in the
  span), the persisted status, the body's byte size and — only when one was
  recorded — the first result artifact's digest. Artifact references and tool
  status survive; the body does not.
- Generated reasoning summaries and provider state are not summary text and are
  omitted. Entry ids are not part of the rendering, because the summary is
  prose for the model and the manifest already binds provenance.
- **Bounded output.** After rendering, the estimate must be within
  `max_summary_tokens`; otherwise generation fails with
  `masked_span_too_long`. Nothing is truncated silently. Only individual argument
  lines are bounded, and that elision is explicit and counted. An empty
  rendering is `empty_summary`.
- **Determinism.** The output depends only on the covered items: no clock, no
  identities, no I/O. The same span renders byte-identically, which
  `crates/core/src/context/golden/observation-masking.txt` pins.

The strategy always checks the limit, but it can only fail where the rendering
is genuinely over budget: masking is lossy for tool bodies and lossless for
dialogue, so a span of long user messages may not shrink. `structured_state`
exists for exactly that case.

### `structured_state`: deterministic extracts, then a model narrative

`StructuredState` computes its extracts from the same narrowed source and the
covered journal entries/events, with no artifact I/O:

- **Files touched** — every `read`, `write` or `edit` call in the span names a
  workspace path. Per path, keyed and ordered by path, the extract records the
  last operation, the status of the last result for it, and the **last known
  digest**: a result artifact digest when the executor recorded one, else the
  digest of the plan that authorized the last call for that path (the recorded
  `AuthorizationDecided.plan_digest`/`ExecutionIntent.plan_digest`, which binds
  the pre-image state it was planned against), else `-`.
- **Commands** — every `bash` call in span order with its command, cwd, the
  status of its result and the executor's exit status (`-` when absent).
  Results are matched by call identity, including when one assistant response
  proposes several commands with different outcomes.
- **Errors** — every call whose result is not `succeeded` (or `cancelled`) in
  span order: tool, path or command, status and the stored code. A failed or
  interrupted *turn* is recorded the same way, though selection only ever covers
  completed turns, so that arm cannot fire today.
- **Requirements** — the first user message of each covered turn, in turn order,
  whitespace-collapsed and bounded to 240 bytes with an explicit marker. This is
  the safe fallback the settled decision allows: the harness does not try to
  classify which sentences are requirements.

Exact committed text for `structured_state` (this block did not change with the
version bump): the deterministic extracts block, verbatim, then the model
narrative, whose request now ends with the shared request-only prompt:

```text
## Deterministic extracts
### Files touched
- `<path>` | last: <read|write|edit> | <status|-> | digest <sha256:…|->
### Commands
- `<command>` (cwd `<cwd>`) | exit <n|-> | <status|->
### Errors
- <tool> `<path|command|turn id>`: <status>[ (<code>)]
### Requirements
- turn <n>: "<first user message>"
## Model narrative
## Task and constraints
…
## Decisions
…
## Files and actions
…
## Outcomes
…
## Unresolved work
…
```

- Empty sections say `- none`. More than 16 entries in one section list the
  first 16 and then `- … (N more, 16 of M listed)`.
- Single-line fields are bounded to 160 bytes with an explicit `… (+N B)`.
- The whole block is bounded to 4096 bytes; when it would exceed that, whole
  lines are dropped and the block ends with
  `- … (extracts truncated at 4096 B; N lines omitted)`.
- The narrative is the rolling summary's fixed five-section prompt over the same
  narrowed source, with the rendered extracts supplied as **quoted JSON data in a
  separate system instruction**. Its output budget is `max_summary_tokens` minus
  the extracts' estimate, and the combined text is re-checked against the limit
  (`summary_too_long`). The request itself ends with the fixed request-only user
  instruction described above, not with the source's last assistant turn.
- **The extracts are inserted by the strategy, never echoed by the model**, so
  the narrative cannot alter, drop or reorder them; a model that restates them
  only adds text after the block. `Compaction.model` is set.

### Clients

`--compaction-strategy rolling_summary|observation_masking|structured_state`
selects the strategy for `ask` and for the TUI on a **new** session, through the
shared wiring's `SessionOptions`, and it is recorded in the created
configuration.

**Continuation rule.** The flag follows `--summary-model`'s explicit-value rule:
on continuation the recorded configuration is compared with the requested one
*after decoding*, and an explicitly supplied strategy that differs from the
recorded one is refused with `session_policy_mismatch` and writes nothing. An
absent flag keeps the recorded strategy (the flag's default value is not
treated as an override), and naming the recorded strategy is not a change. A
mid-session change is the recorded `SetCompactionStrategy` command, not a client
flag; the clients do not send it in this step (m6-03 owns the TUI controls).

The status bar and inspector were left untouched: m6-03 runs in parallel in
`crates/cli/src/tui/`.

## m6-07 amendment: the chosen new-session default (decision 18)

On 2026-11-02 the user chose `observation_masking` as the M6 product default
after reviewing the second bounded matrix. The implementation keeps two defaults
that are deliberately different:

- **Creation.** `harness::wiring::creation_compaction_strategy()` is the single
  client creation default (`ObservationMasking`). Both parsers — `ask` and the
  plain-`harness` TUI — resolve their flag through `ask_cli::strategy_selection`,
  so there is no separate TUI policy, and the existing shared wiring records the
  strategy in the new session's configuration artifact. `--help` shows it as the
  flag's default.
- **Decoding.** `StrategyId::default()` and `CompactionConfig`'s serde default
  stay `rolling_summary`. That is the legacy reading of a configuration that
  omits the strategy — a missing `compaction` object, an empty one, or an old
  summary-model policy. Changing it would silently reinterpret every journal
  written before M6-04, so it must not follow the product default.
- **Continuation.** Without an explicit selection, the recorded strategy wins
  (`requested.strategy = recorded.strategy`), including the legacy
  `rolling_summary` reading. No configuration entry is appended and earlier
  journal bytes are unchanged. An explicit differing selection is still refused
  with `session_policy_mismatch`, and a refusal writes nothing.
- **Unchanged rules.** Versions stay `2`/`1`/`2`; no schema, artifact or record
  is rewritten. Masking is model-free and leaves `automatic: false` when no
  summary model is configured; explicit model-based strategies still require a
  configured summary model. The default alone neither enables nor disables the
  automatic trigger.

This is the user's measured provisional choice, not an inferred winner. The
second matrix cannot rank the strategies generally: four prose-heavy tasks, one
repetition, lexical matchers and one probe turn. Masking's distinguishing
measured properties are determinism, zero summary inference and the smallest
post-compaction estimate, not better memory than a summary; it stays lossy,
does not guarantee a shorter projection and provides no workspace rollback. See
[context experiments](../context-experiments.md) and
[Milestone 6 acceptance](../milestone-6-acceptance.md).

## Consequences

- Three strategies share one selection/commit path, so a new strategy only has
  to produce text (and say whether it used a model).
- The recorded configuration is the only place a strategy lives; a branch's
  compactions carry the name and version they were written with, so a later
  change never rewrites or reinterprets them.
- Masking trades detail for determinism and zero cost. Structured state spends
  one model call but keeps an exact, machine-checkable account of files,
  commands, errors and requirements at the top of the summary. Neither is
  better in general: that is what m6-05 measures.
- `automatic` no longer requires a summary model when the selected strategy does
  not use one (`observation_masking`); it still does for the other two.

## Sources

Read on 2026-10-31:

- "The Complexity Trap: Simple Observation Masking Is as Efficient as LLM
  Summarization for Agent Context Management" (arXiv:2508.21433, first submitted
  29 August 2025) — the systematic comparison with SWE-agent on SWE-bench
  Verified that motivates masking simple, stale observations instead of
  summarizing them.
- Anthropic engineering, "Effective context engineering for AI agents" —
  context as a finite resource; compaction and structured note-taking as the two
  complementary strategies, with structured notes kept outside the model's own
  words.
- Anthropic engineering, "Effective harnesses for long-running agents" — the
  progress-file pattern: an agent-maintained, human-readable state block that
  survives context loss, which is what the deterministic extracts formalize.

## Evidence

- Selection and formats: `context::tests::compaction_tests` —
  `strategy_identity_is_closed_and_recorded` (versions 2, 1, 2),
  `model_based_requests_end_with_one_fixed_request_only_prompt` (decision 16:
  the explicit fresh request on completed source ending in an assistant turn),
  `observation_masking_is_a_deterministic_zero_stream_golden` (checked-in
  golden, zero streams, `model` absent), `observation_masking_over_budget_fails_explicitly`
  (`masked_span_too_long`),
  `structured_state_extracts_are_ordered_and_untouchable`,
  `structured_state_extracts_are_bounded_with_explicit_elision`, and
  `every_strategy_skips_pinned_and_excluded_turns`.
- Adapter: `harness_providers::anthropic::tests::deepseek::a_summary_request_ends_in_a_user_turn_after_an_assistant_response`
  submits the shared request through the real DeepSeek Messages adapter over
  attributed loopback: a final user turn, a complete tool pair, no credential.
- Runtime: `runtime::tests::compaction` —
  `c02_manual_preview_leaves_journal_and_workspace_unchanged`,
  `c02_stale_*`, `c02_automatic_retains_configured_recent_turns`,
  `c02_automatic_references_exact_source_ids`,
  `c02_automatic_supersedes_only_ancestor_summaries`,
  `c02_original_entries_remain_readable_byte_identical`,
  `c02_budget_fits_or_failure_is_explicit`,
  `c02_automatic_summaries_respect_pins` and
  `c02_no_summary_regenerated_during_pure_projection_or_prepare` run over
  `StrategyId::ALL`; `structured_state_extracts_reads_writes_a_failed_command_and_a_denial`
  is the executor-shaped extract fixture; `a_strategy_change_is_recorded_idle_only_and_invalidates_previews`,
  `a_strategy_change_makes_an_outstanding_preview_stale_policy`,
  `a_strategy_change_needs_an_idle_session_and_a_usable_model` cover the
  recorded change, reopen and refusals.
- Clients: `harness` integration tests
  `compaction_strategy_flag_is_recorded_on_a_new_session` and
  `continuation_refuses_a_compaction_strategy_change`, plus the `--help`
  assertions and the plain-`harness` flag ratchet.

### Break-on-purpose results (m6-04)

Each mutation was applied individually, the named test failed, and it was
reverted:

| Mutation | Failing test and observation |
| --- | --- |
| Let `observation_masking` call the provider | `observation_masking_is_a_deterministic_zero_stream_golden`: the recording provider saw one stream |
| Let the model's text replace the extracts block | `structured_state_extracts_are_ordered_and_untouchable`: the committed text did not start with the extracts |
| Leave the strategy out of the policy fingerprint | `a_strategy_change_is_recorded_idle_only_and_invalidates_previews`: the preview's binding fingerprint still equaled the recorded policy's after the change |
| Silently truncate an oversized masked rendering | `observation_masking_over_budget_fails_explicitly`: a preview was returned instead of `masked_span_too_long` |
