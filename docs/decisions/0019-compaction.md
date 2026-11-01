# 0019: Rolling compaction

Status: Implemented for Milestone 4, steps 05–06, amended for Milestone 6,
step 02 (m6-02: coverage skips controlled turns) and by m6-side-summary-request
(decision 16: an explicit fresh summary request and strategy versions 2). The
original preview-only scope is preserved below; the m4-06 amendment adds runtime
commitment and triggers, and the m6-02 amendment replaces decision 9's interim
refusal.

Date: 2026-10-29

## Decision

### Configuration and selection

The optional session configuration `compaction` object has independently
configured `summary_model: ModelRef`, `max_summary_tokens` (default **2048**,
positive), and `retain_recent_turns` (default **2**, minimum **2**). Missing or
null `summary_model` is unconfigured, never an implicit session model. Empty
model/provider names, unknown fields, non-objects and invalid field shapes or
bounds fail loading with `invalid_compaction`; duplicate keys retain the existing
`configuration_not_an_object` error. Selection does not require a summary model.
Generation refuses without it using `summary_model_unconfigured`.

`select_span` is pure over a validated snapshot, explicit head and policy. It
walks only that ancestry. Starting at the root (or the newest active summary's
original coverage start for rolling replacement), it takes the longest exact
contiguous slice before the protected window. The window begins at the earliest
of the retained recent completed turns and any unfinished turn. All covered
turns must have outcome `Completed`; failed/cancelled/interrupted turns are
barriers, not silently skipped. Whole-turn grouping follows storage's branch
ancestry. Unfinished calls and approvals belong to unfinished turns and cannot
enter the slice. Pins did not exist at this step: the m6-02 amendment below
replaces "pin exclusion is not applicable until pins exist" with skipping the
controlled turns, so the slice stays contiguous only between them.

For rolling replacement the original coverage is included together with the
newly eligible completed dialogue. All active ancestor summaries overlapping
that slice are explicitly superseded. Off-branch summaries are ignored, even
when they cover shared ancestors. Markers or summaries alone do not count as
new dialogue. No inference or artifact read happens during selection.

The closed `NotEligible` reasons are:

- `empty`: no ancestors at the selected root.
- `only_recent_turns`: no dialogue before the protected window.
- `unresolved_work`: unfinished work blocks the first source turn.
- `nothing_new_since_summary`: no eligible new dialogue beyond prior coverage.
- `non_completed_turn`: a terminal non-completed turn blocks contiguous coverage.
- `unknown_head`: head is not in this snapshot.
- `invalid_compaction`: a programmatically supplied policy violates bounds.

The m6-02 amendment adds `only_controlled_turns` and removes m6-01's interim
`context_controls_in_span`.

### Source identity

`session::compaction_manifest` shares its implementation with storage artifact
validation. Compact JSON contains `covered`, `supersedes` and
`configuration_fingerprint`; each array retains supplied ancestral order and
contains `{entry_id, record_checksum, entry_index}` references. Object keys use
serde_json's sorted map ordering, as existing storage fixtures do. The manifest
artifact SHA-256 is `source_fingerprint`. Storage's rules and interpretation are
unchanged; this only factors out its expected manifest construction.

### Generation

`CompactionStrategy::propose` accepts immutable `CompactionInput` and
`OperationControl`, returning a future of preview data. The input contains a
snapshot, explicit head, current decoded session configuration, session context
policy/tools, caller-allocated unused entry and response identities, and a
read-only artifact source. It has no journal writer, workspace or tool host.
`RollingSummary` takes only `Arc<dyn ModelProvider>` as its I/O capability.

The existing builder projects the selected branch for the **session model**.
From that projection the strategy keeps selected source items and applicable
superseded summary text, in projection order. Prior summaries therefore take
the place of already summarized dialogue exactly as the builder placed them.
Fixed system instructions require these exact Markdown headings in order:

1. `## Task and constraints`
2. `## Decisions`
3. `## Files and actions`
4. `## Outcomes`
5. `## Unresolved work`

The request explicitly names `summary_model`, has no tool definitions, resets
inference options to the configured summary output budget and no reasoning or
provider controls, and never sends a session-owned native block or continuation,
even if both models were explicitly configured equal. Historical tool calls and
results remain paired data with identity, status and provenance, not capabilities.
Artifact access is narrowed to the sanitized input. Session instructions are not
allowed to replace the fixed summarizer instructions; they are supplied as quoted
JSON source data alongside them. This is a fresh inference, not a continuation
of the session's model operation.

Streaming is pull-driven, with bounded estimated text checked during assembly
using the existing `bytes4-flat-media-v1` heuristic. Multiple text blocks assemble
in block-index order. Generated reasoning/native state and continuation handles
are discarded, never published as summary text or reused. Ignored streamed data
is bounded at 4 MiB, native payloads at 256 KiB each, and block metadata at 4096
indices; kind collisions or unrequested tools are malformed streams. Oversized
manifest artifacts or entries are errors, not unpersistable previews.
Cancellation and the absolute
Tokio deadline independently abort the future and drop its stream, even for a
provider that ignores control; deadline expiry also cancels its token. Both
return `Cancelled`, never a partial preview. Errors are coded:
`provider_failure`, `summary_refused`, `empty_summary`, `malformed_summary`,
`summary_too_long`, `malformed_summary_stream`, plus configuration/projection,
identity and manifest errors. No truncation, fabrication or automatic retry.

### Preview and binding

`CompactionPreview` contains the exact proposed `Entry::Compaction` envelope,
manifest bytes and the before/after `ContextReport`s for the session model. It
binds the original head, journal sequence, session model, source fingerprint and
policy fingerprint. The latter is SHA-256 of compact JSON
`[strategy_name, strategy_version, compaction_config]`, including defaults.
The proposed entry contains strategy/version, ordered coverage, manifest ref and
digest, superseded IDs, exact summary text, summary model and current configuration
fingerprint.

The after report uses the unchanged builder over a private hypothetical snapshot
with the proposed entry appended to its selected ancestry. Its sequence remains
the **base** sequence, not a reserved future record sequence. Tests publishing
manifest bytes and appending the unchanged entry through `JournalStore` compare
the real after report, adjusting only the newly allocated sequence. No artifacts
are published and no head/sequence changes during generation. The preview need
not save tokens: it reports actual estimates, including increases, honestly.

## Original scope of m4-06 (implemented by the amendment)

Explicit preview commitment, stale-preview checks, commands, automatic 85%
preparation/triggering, inspection diffs and full C02 integration are m4-06.
There is no new runtime command, automatic behavior or journal write here.
An active-turn preview protects the current turn but cannot be committed until
storage's idle requirement and all preview bindings hold. Navigating to a
historical head likewise requires selecting that head before a future commit.

## m4-06 amendment

### Runtime and policy

The pure core emits `ProposeCompaction { token, input }`; only the driver runs
`RollingSummary::propose`, using the session's supplied provider and the
independent configured summary model. It owns one generation future, fresh
cancellation token and absolute deadline (`compaction_deadline`, 300 seconds).
Generation yields cooperatively even for an always-ready provider. Cancellation,
shutdown, fault and replacement cancel/drop it. Answers carry runtime epochs and
fresh local tokens; stale answers are ignored. The builder, `prepare` and both
inspection APIs remain pure and never generate summaries.

`compaction.automatic` is a resolved boolean, defaulting to true with a summary
model and false without one. Enabling it without a summary model is invalid.
Its effective value joins the other defaulted fields in the policy fingerprint.
`automatic: false` preserves manual generation and commitment, but does not
silently compact on an overflowing start. No storage rule or journal format changes.

`PreviewCompaction` is idle-only and recovery-blocked. It checks eligibility
before emitting any provider effect, then returns
`CompactionPreviewed { preview_id, preview }`. IDs come from `Env`, are fresh
across runtimes and are never journaled. `CommitCompaction { preview_id }`
rechecks **head, sequence, session model, source and policy**. Selection and
manifest construction run again over the current snapshot; cached manifest
bytes alone cannot authorize a commit. A mismatch is `StalePreview { field }`
(`stale_preview`), consumes the proposal and publishes nothing. Success passes
the exact entry and manifest to existing `Persist`, which publishes/syncs the
artifact before append. Commitment is always idle-only.

Any session commit, fault or restart discards the cached proposal. A bounded
single tombstone may retain only its ID and invalidating field. Unknown or
consumed IDs reject as stale. `CancelCompaction` cancels running generation;
the pending preview/start request receives `cancelled`. `StartTurn` cancels
manual or post-turn generation before proceeding. Generation failure is an
ephemeral coded event, not a recovery block and never a retry loop.

After a terminal turn acknowledgement, the idle head projection is compared
with `compaction_trigger_permille` (850 by default). An eligible span generates
and commits automatically, with preview and durable commit events containing
both reports and the diff. A result still over budget fails explicitly instead
of committing an ineffective post-turn summary. Each trigger gets one attempt;
neither summary acknowledgement, reopen, inspection nor failure retriggers it.

Before C1, the core projects a private staged opening commit through the same
builder. If the new message overflows and automatic compaction is eligible, it
holds the start request, compacts **while idle**, then checks the prospective
context again before C1. Identical pending-start retries attach during generation
and summary persistence; changed content is `StaleCommand`, and another start
is `Busy`. No user entry or turn is written when no span can help or the
post-compaction projection still exceeds budget. The rejection is
`ContextBudgetExceeded { reason: Option<NotEligible> }`
(`context_budget_exceeded`). A later tool-loop overflow fails the already-open
turn with that code; it never triggers compaction inside the turn. This replaces
the earlier `context_overflow` execution code in decision 0006.

### Inspection and crash semantics

`CompactionDiff` is computed only from the before/after reports. It lists newly
covered IDs and superseded summaries in report order, complete category token
estimates before/after, and whether the known budget fits. It does not fabricate
a saving. The preview reply and automatic preview/commit events carry it.
`Runtime::inspect_context_preview(&id)` and the handle's async counterpart
return that live preview (or stale rejection), without regenerating anything.

A crash during generation leaves no persistent state. After publication but
before append, only an unreferenced manifest remains, and reopen is idle.
After append but before reply, the summary appears once on reopen, still idle.
There is no `command_id` in `CommitCompaction`: its ephemeral single-use preview
ID is not a durable command identity. Replaying the old ID after restart rejects
it, so no receipt or second append is needed. The held `StartTurn` uses its
ordinary C1 receipt once C1 commits; a summary alone does not claim that a turn
started. Tests cover Lost/Durable/Torn boundaries, with no two in-flight intents.

### C02 evidence

Named tests are in `runtime::tests::compaction` unless qualified otherwise:

| Clause | Evidence |
| --- | --- |
| Preview changes neither session nor workspace | `c02_manual_preview_leaves_journal_and_workspace_unchanged` compares journal, all data-root files and workspace bytes |
| Every stale binding | `c02_stale_head`, `c02_stale_sequence`, `c02_stale_model`, `c02_stale_source_manifest_substitution`, `c02_stale_policy`; matching substituted digest/manifest still cannot pass |
| Configured recent turns remain | `c02_automatic_retains_configured_recent_turns` uses retention three |
| Exact source IDs/provenance | `c02_automatic_references_exact_source_ids` compares the stored artifact with a reconstructed manifest |
| Only ancestor summaries superseded | `c02_automatic_supersedes_only_ancestor_summaries` includes rolling replacement and a sibling branch |
| Original entries preserved | `c02_original_entries_remain_readable_byte_identical` checks the journal prefix, entry bytes and checksums |
| Fits or explicit failure | `c02_budget_fits_or_failure_is_explicit`, `oversized_protected_content_rejects_after_one_compaction_attempt`, `mid_turn_overflow_fails_without_compaction_inside_turn` |
| Pins | `c02_automatic_summaries_respect_pins` (m6-02): an automatic compaction never covers, sends or loses a pinned turn |
| No regeneration during projection | `c02_no_summary_regenerated_during_pure_projection_or_prepare`; driver test `driver_shutdown_cancels_generation_and_reopen_has_no_preview_or_summary` records zero streams across full branch inspection |

The m4-05 deterministic fixture additionally has
`compaction-preview-diff.json`, alongside its unchanged before/after goldens.
Driver tests exercise actual summary streams, cancellation/start preemption,
absolute deadline, epoch/token reuse, inspection and post-ack events. The main
randomized test calls compaction-specific seeded interleavings on **every seed**:
manual preview, commit, stale commit, automatic generation, cancel/failure and
Lost/Durable/Torn crashes, all with the storage oracle, replay and resync checks.

### Break-on-purpose results (m4-06)

Each mutation was applied individually, the named test failed (exit 101), then
the mutation was reverted:

| Mutation | Failing test and observation |
| --- | --- |
| Skip sequence validation | `c02_stale_sequence`: the request was not rejected |
| Trust preview manifest bytes without reconstruction | `c02_stale_source_manifest_substitution`: substituted provenance was not rejected |
| Start the summary strategy during context inspection | driver `driver_shutdown_cancels_generation_and_reopen_has_no_preview_or_summary`: zero-stream assertion failed |
| Emit compaction while Preparing on a tool-loop overflow | `mid_turn_overflow_fails_without_compaction_inside_turn`: observed Preparing instead of Idle; independent `storage_idle_rule_rejects_compaction_inside_turn` still rejects with `session_not_idle` |
| Retry automatic generation after failure | `automatic_single_attempt_per_trigger_and_failure_never_blocks`: another generation was pending |

## m6-02 amendment

Decision 14 replaces m6-01's interim refusal with skipping. A compaction's
coverage is the selected span minus every turn pinned or excluded at the
selection point, so the summarizer never receives a controlled entry and no
summary ever stands for one. RFC-001 6.1's "required pinned items remain outside
coverage" is kept; its "exact contiguous ancestral slice" becomes *ancestral
order, contiguous except controlled turns*.

### Selection

`select_span` still takes the longest eligible span of completed turns before the
retained window and rolls the active summary's source in. It then drops every
entry of a turn whose effective mode, folded over the selected ancestry, is
`pinned` or `excluded`. The window stays counted in turns, so a controlled turn
inside it changes nothing. The new closed reason is `only_controlled_turns`
(`NotEligible::OnlyControlledTurns`): coverage would be empty, or no eligible new
dialogue remains after the active summary. m6-01's `context_controls_in_span`
(`NotEligible::ContextControlsInSpan`) is **removed**; it existed only for that
one step and no journal or report ever carried it. Marker entries that carry no
turn are not dialogue and stay covered, so a summary's manifest binds a control
entry exactly as it stood.

### Projection and report

Nothing in the projection changed. A summary still stands at the position of its
first covered entry, so the order after a compaction that skipped controlled
turns is: instructions, the summary, the skipped **pinned** turns verbatim in
original order with their full tool groups, then the retained turns. A skipped
turn that precedes every covered entry necessarily stays before the summary,
because coverage records only the covered IDs and the summary cannot be placed
before the entry it names. Excluded turns remain `user_excluded` and are never
covered. Covered entries are `covered_by_summary`, and a pinned turn's items are
included with `pinned: true`. Rolling supersession across several compactions
keeps the same invariants. `check::check` gained a branch-order invariant
(`items_out_of_branch_order`, `summary_without_coverage`) so a pinned turn
projected after the retained window, or a summary out of place, is a builder
bug. A control entry an active summary covers is reported
`covered_by_summary`/`superseded_summary` instead of `context_control_marker`;
m6-01's marker-only rule and its `undecided_control_entry` invariant are relaxed
accordingly. `PROJECTION_VERSION` stays 4 and `BUILDER_VERSION` stays 5: no item
or report field changed.

Pins are never truncated. If pinned turns plus retained turns plus the summary
still do not fit, the existing explicit `context_budget_exceeded` stands:
automatic compaction does not commit an ineffective summary and `StartTurn` is
refused. A manual preview reports `fits_budget: false` in the `CompactionDiff`.

### Storage rule and codes

Coverage must be an ancestral, ordered subsequence of the branch between its
first and last covered entry, and every entry it skips must belong to a whole
turn whose effective mode at the compaction's parent is `pinned` or `excluded`.
The rule lives in `session::coverage`, a pure function both storage and the
builder call: storage folds the parent's controls from `BranchFacts`, and the
builder folds the same controls while walking to each compaction entry. The
manifest and its fingerprint still come from `session::compaction_manifest`.

| Rule | Error code |
| --- | --- |
| A covered ID is not on the ancestry | `compaction_not_ancestral` |
| Covered IDs are out of order, or a gap is not a whole controlled turn | `compaction_not_contiguous` |
| A covered entry's turn is not covered whole | `compaction_splits_turn` |
| Coverage contains a turn pinned at the parent | `compaction_covers_pinned_turn` |
| Coverage contains a turn excluded at the parent | `compaction_covers_excluded_turn` |

`compaction_not_contiguous` keeps its m4-05 name and its old cases: reversed or
interleaved coverage still fails it. Pinning or excluding an already covered turn
is still `context_control_covered`, so a recorded coverage is never retroactively
invalidated by a later control; that is also why the builder folds controls at
each compaction's own parent rather than at the head.

### Controls after compaction

Resetting an excluded turn that a compaction skipped brings it back verbatim in
its original position relative to the pinned turns, because it was never covered.
A reset is always accepted, and the projection simply includes its entries again.
A compaction is appended at the head, so it cannot change the rebuilt projection
of any earlier entry: an existing continuation stays valid across it. A control
applied afterwards does change the head's fold used for prefix rebuilds, so it can
still invalidate a later continuation through `continuation_prefix_mismatch`.

### C02 pin clause

`c02_automatic_summaries_respect_pins` closes RFC-001 C02's pin clause: an
automatic compaction triggered by an overflowing `StartTurn` does not cover the
pinned turn, its summary request contains no entry of it, the committed summary
stands for the other span turn, and the pinned turn is projected verbatim and
marked `pinned`. `a_pinned_turn_that_cannot_fit_fails_explicitly` covers the
budget half: the preview overflows, automatic compaction emits
`context_budget_exceeded` and commits nothing, and the pinned message is still
projected byte for byte.

### Evidence (m6-02)

- Selection and projection: `context::tests::compaction_tests` covers
every `NotEligible` reason including `only_controlled_turns`, skipped pinned and
excluded turns, two compactions with a pin, the projection order
golden, excluded turns staying excluded, the summarizer's exact input, resetting
a skipped excluded turn, a skipped pinned turn with a later control, and the
budget failure with a 40 KB pinned turn.
- Storage: `session::tests::controls` accepts a controlled gap and refuses a gap
of an uncontrolled turn, a partially covered turn, a covered excluded turn and
out-of-order coverage, next to the existing pinned refusal.
- Runtime: `c02_automatic_summaries_respect_pins`,
`a_pinned_turn_that_cannot_fit_fails_explicitly`, and the randomized test's new
controls-then-compaction-then-controls interleaving on every seed, with the
storage oracle on.

### Break-on-purpose results (m6-02)

Each mutation was applied individually, the named test failed, and it was
reverted:

| Mutation | Failing test and observation |
| --- | --- |
| Feed a pinned entry to the summarizer | `summarizer_never_receives_a_controlled_entry` and `c02_automatic_summaries_respect_pins`: a pinned entry reached the request |
| Accept a gap containing an uncontrolled turn in storage | `coverage_refuses_a_gap_of_an_uncontrolled_turn`: the commit was accepted |
| Project skipped pinned turns after the retained turns | `projection_order_is_summary_then_pinned_turn_then_retained` and `items_out_of_branch_order` |
| Truncate a pinned turn to fit | `a_large_pinned_turn_is_never_truncated_and_overflows_the_budget` and `a_pinned_turn_that_cannot_fit_fails_explicitly` |
| Cover an excluded turn | `coverage_refuses_an_excluded_turn`: the commit was accepted |

## Evidence (m4-05)

`context::tests::compaction_tests` exercises every eligibility reason, configured
retention/current-turn protection, source provenance, real storage acceptance,
rolling supersession, sibling isolation, independent model selection and native
state removal, scripted generation failures/cancellation/deadline, and exact
requests. Deterministic RFC fixture previews have byte-for-byte before/after
golden reports. Storage-oracle negative tests separately reject recent coverage,
out-of-order manifest references and omitted rolling supersession. Mutation
checks temporarily disable each corresponding producer guard and require the
named acceptance test to fail before reverting it.

### Break-on-purpose results

All mutations were applied individually, observed failing, and reverted:

| Mutation | Failing evidence |
| --- | --- |
| Retain zero completed turns, including the newest in coverage | `selection_closed_reasons_and_retained_window`; storage acceptance rejects `compaction_unfinished_or_recent_turn` |
| Reverse manifest covered references only | `storage_acceptance_normal_and_rolling`: `source_manifest_mismatch` |
| Omit rolling `supersedes` | `storage_acceptance_normal_and_rolling`: builder first rejects `overlapping_active_summaries`; independent storage-oracle test rejects the same missing-supersession entry |
| Leave session native blocks in summary input | `exact_request_has_independent_model_no_tools_or_native_state`: request artifact allowlist assertion fails |
| Disable heading validation | `generation_errors_are_coded_and_never_produce_previews`: malformed-summary case incorrectly returns a preview |

## m6-side amendment

M6 decision 16, implemented in m6-side-summary-request. The first live matrix
(m6-06, experiment source `cd14762`) had its eight model-based compaction cells
fail at the provider: the completed-dialogue request ended with an **assistant**
turn, which the Messages API treats as assistant prefill and rejected with HTTP
400, while the same body with an appended user request was accepted. Observation
masking, the only model-free arm, was unaffected. That evidence is not changed or
rerun; this amendment implements and verifies the fix offline, with no paid
request.

### Explicit fresh request

`summary_input` is the one shared composition both model-based strategies use,
and it now ends the request with **one fixed, request-only prompt** after the
narrowed source and their instructions. The text is the constant
`SUMMARY_REQUEST_INSTRUCTION`: it asks for the configured summary of the
preceding completed source and restates the fixed system instruction's
untrusted-data rule, and it interpolates nothing, so it can never quote a source
entry, a controlled turn or an artifact name.

The prompt is a new provider-independent item variant, `ContextItem::Request
{ text }`, and deliberately **not** a `ContextItem::User` with some made-up
identity. `ContextItem::User.entry_id` means "the source journal entry this item
comes from"; a request-only prompt comes from no entry, and no UUID is reserved
by storage, so any fixed value could collide with entries of an imported
journal, and any generated value would be a fabricated source identity.
`ContextItem::Request` carries no entry identity at all (`entry_id()` returns
`None`), no provenance, no artifact reference and no capability. Both adapters
map it to one plain user turn, so no wire format, transport or provider profile
changed.

Adding a variant does not change any projection: the builder never creates a
request item, so existing serialized items keep their exact bytes,
`PROJECTION_VERSION`/`BUILDER_VERSION` are unchanged, projection fingerprints
are unchanged, the journal schema is untouched, and recorded summaries replay
exactly as before. Consumers account for the item without giving it provenance
or access: artifact enumeration adds no reference for it, masking renders nothing
for it (a request prompt is never covered source), the builder's report check
rejects it as `request_item_in_projection` because a projection can never contain
one, the eval renderer reads its literal text, and the driver's test helper labels
it as a request.

Everything else the request guarantees is unchanged: ancestral source order,
paired calls and results with identity/status/provenance, the independent summary
model, the caller's output budget, no tools, no reasoning or provider controls,
no native continuation or session-owned native state, cancellation and the
absolute deadline, pinned/excluded turns never sent, and artifact access narrowed
to exactly the sanitized source. Selection, coverage validation, the manifest,
the journal schema, control rules and the preview binding are untouched.

### Strategy versions

The prompt changed, so `StrategyId::version` is now **2** for `rolling_summary`
and `structured_state` and stays **1** for `observation_masking`, whose selection
and output are unchanged. A committed compaction records the version it was
written with, and readers never reinterpret it: a summary recorded under version
1 still projects, inspects and reprojects as originally recorded. Because the
version joins `policy_fingerprint`, an outstanding preview of either model-based
strategy made before the upgrade is stale (`stale_preview`, field
`policy_fingerprint`) rather than silently reusable.

### Evidence (m6-side)

- `context::tests::compaction_tests::model_based_requests_end_with_one_fixed_request_only_prompt`:
  for both model-based strategies over a completed source that **ends with an
  assistant response**, the recorded request's last item is exactly the fixed
  request-only prompt, `entry_id()` on it is `None`, every other item names a real
  journal entry, and the prompt text is absent from coverage, the manifest, the
  committed entry and the rebuilt projection; generation appends nothing; the
  artifact allowlist is empty and tools/continuation are empty; the entry records
  version 2.
- `strategy_identity_is_closed_and_recorded` pins the three versions (2, 1, 2).
- `providers::anthropic::tests::deepseek::a_summary_request_ends_in_a_user_turn_after_an_assistant_response`
  builds the body from the real shared item, then submits it through the real
  DeepSeek Messages adapter on attributed loopback. The source is user, a paired
  tool call and result, and a **completed final assistant response**: without the
  request the body ends with that assistant turn (prefill), with it the body ends
  with one standalone user turn carrying only the fixed text, and the earlier
  pair and the assistant response survive in order. A second case keeps a source
  ending at a tool result, where the request joins that existing user turn. The
  stream's last event is asserted to be `Finished`/`Completed`/`Stop` (not merely
  a poll count), the submitted body equals the built one, and no credential or
  network is used.
- Existing request/report goldens changed only where the version is recorded
  (`compaction-preview-after.json`). The eval record's per-strategy version is
  asserted per arm (`1` for masking, `2` for the model-based arms) and the
  checked-in markdown row follows the record.

### Break-on-purpose results (m6-side)

Each mutation was applied individually, the named test failed, and it was
reverted:

| Mutation | Failing test and observation |
| --- | --- |
| Remove the fixed request item from `summary_input` | `model_based_requests_end_with_one_fixed_request_only_prompt`: `rolling_summary: the request does not end with a request-only prompt` |
| Keep the item but drop it in the adapter's message builder | `deepseek::a_summary_request_ends_in_a_user_turn_after_an_assistant_response`: the built body's last turn was the assistant response again, so the final-role assertion failed |
