# 0019: Preview-only rolling compaction

Status: Implemented for Milestone 4, step 05. Commit commands and automatic
triggering are not implemented here.

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
enter the slice. Pins do not exist: pin exclusion is **not applicable until
pins exist**, not approximated here.

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

## Scope of m4-06

Explicit preview commitment, stale-preview checks, commands, automatic 85%
preparation/triggering, inspection diffs and full C02 integration are m4-06.
There is no new runtime command, automatic behavior or journal write here.
An active-turn preview protects the current turn but cannot be committed until
storage's idle requirement and all preview bindings hold. Navigating to a
historical head likewise requires selecting that head before a future commit.

## Evidence

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

