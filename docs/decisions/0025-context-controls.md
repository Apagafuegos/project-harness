# 0025: Context controls and record version 3

Status: Implemented for Milestone 6, step 01 (m6-01). The interim compaction
refusal described below was replaced by skipping in m6-02 (see the m6-02
amendment to [decision 0019](0019-compaction.md)); the client controls arrive in
m6-03.

Date: 2026-10-31

## Decision

### One durable entry per control

A finished turn's effective mode is a durable, append-only statement on the
branch: a new entry kind `context_control` in record version 3.

```json
{"kind":"context_control","turn_id":"<finished TurnId>","mode":"default"|"pinned"|"excluded"}
```

The payload's `turn_id` names the **target** turn. The entry's own `turn_id`
field stays `null`: a control is a branch marker, not dialogue, exactly like
`model_changed`. `ContextMode` is a closed enum with `default` as its default
value, and it is persisted with `snake_case` spellings.

Controls never rewrite or delete history. Undo is another control entry, so a
turn pinned and then reset has three entries and keeps both. The last control
for a turn on the selected ancestry wins; controls on sibling branches never
apply. A control that would not change the effective mode is refused rather
than recorded, so a control entry is always evidence of an effective change.

### Storage rules

Storage is the oracle. A `context_control` entry is accepted only when every
rule below holds, and each refusal is a stable `StoreError::Invalid` code with
nothing written:

| Rule | Error code |
| --- | --- |
| The session is idle, with no active turn and no unresolved work | `session_not_idle` |
| The entry has no `turn_id` of its own and its commit starts no turn | `context_control_inside_turn` |
| The head is not inside a tool exchange | `head_inside_tool_exchange` |
| The target turn exists and has a terminal outcome | `context_control_unfinished_turn` |
| At least one entry of the target turn is on the control's parent ancestry | `context_control_not_ancestral` |
| The control changes the target turn's effective mode on this ancestry | `context_control_noop` |
| A non-`default` mode does not touch a turn an active summary already covers | `context_control_covered` |
| A compaction's coverage contains no turn pinned at its parent | `compaction_covers_pinned_turn` |
| A compaction's coverage contains no turn excluded at its parent | `compaction_covers_excluded_turn` |

`context_control_inside_turn` is needed because entries are validated before
the commit's events install the active turn; without it, a control in a
turn-opening commit would slip past the idle check. The tool-exchange rule
(`head_inside_tool_exchange`) is the same branch-walk defense `model_changed`
has; a pending exchange always belongs to an open turn, so `session_not_idle`
is the observable refusal for a real journal. It cannot be reached through the
runtime, which checks idle first.

**Covered turns.** Pinning a turn an active summary already absorbed would
promise that the turn is kept verbatim, which the summary text already
contradicts, so it is refused with `context_control_covered`. Excluding such a
turn is refused with the same code for the same reason: the summary text
derived from it stays in the projection, so the promise could not be kept.
Resetting to `default` is always accepted, because it is how a control is
undone and it promises nothing. "Active summary" means a summary on this
ancestry that no other summary on it supersedes, exactly as the builder's
`Plan` counts them.

**Pinned coverage.** A compaction whose coverage includes a turn pinned at the
compaction's parent is refused (`compaction_covers_pinned_turn`), and since
m6-02 the same holds for an excluded turn (`compaction_covers_excluded_turn`).
This is defense in depth for RFC-001 6.1: storage must not depend on one caller.
Since m6-02 the runtime selects coverage as the span minus every controlled turn
instead of refusing a span that contains one, so these refusals catch a caller
that would cover a controlled turn rather than a span that merely holds one. The
coverage shape itself is validated by `session::coverage`, shared with the
builder.

### Derived state

`BranchFacts` gained `controls: OrdMap<TurnId, ContextMode>`, computed once per
entry from the parent's facts and shared when unchanged, like `pending` and
`model`. The `#[cfg(test)]` scanning oracle has a matching `walked_controls`
and compares it after every applied record in test builds. Reading the fold is
O(log n); appending a control is O(log n) plus the linear ancestry scans the
coverage rules already need.

### Projection and report

The builder folds the controls of the **selected ancestry** once. Excluded
turns are decided before summary replacement: every entry of such a turn gets
one entry-level exclusion with `ExclusionReason::UserExcluded`
(`user_excluded`), including tool proposals and their results, so a tool group
is never split. If a turn is both excluded and covered by an active summary,
`user_excluded` wins for its entries, because that is the user's explicit
instruction; `summaries[].covered_entry_ids` still records the coverage. A
pinned turn is included exactly as before. `IncludedItem` gained
`pinned: bool`, omitted when false, so a pin is visible in the report without
changing the projection.

The control entry itself is excluded from dialogue with the new
`ExclusionReason::ContextControlMarker` (`context_control_marker`). A distinct
reason rather than `model_change_marker` keeps the report's account precise:
the two entry kinds make different statements, and a reader should not have to
infer which from the entry payload. Since m6-02 an active summary may cover the
control entry itself, which is not dialogue and carries no turn; it is then
reported `covered_by_summary` (or `superseded_summary`), because the summary
stands for every covered source entry.

`ContextReport` gained `controls: [{turn_id, mode, control_entry_id}]`, omitted
when empty and ordered by the branch position of the effective control entry.
Only non-`default` turns appear, so a pin followed by a reset leaves no
summary entry.

**Fingerprint.** Items, and therefore fingerprints, change only when a control
changes what is sent: excluding a turn removes items, and pinning adds none.
`PROJECTION_VERSION` stays 4. `BUILDER_VERSION` becomes 5 because the report
gained fields, two reasons and the controls summary; the three older goldens
change by that one line and nothing else.

**Continuations.** The effective controls are folded over the *selected head's*
ancestry and that fold is used for every prefix rebuilt for a continuation.
This matters because a control is appended after the response it affects: the
rebuilt prefix at the response's parent must reflect the modes in force at the
head, or a provider would be told to continue native state for items that are
no longer sent. With that fold the existing `ContinuationPrefixMismatch` rule
handles exclusion with no control-specific case: the prefix fingerprint
changes and the continuation is dropped. A prefix build's report can only name
control entries it contains, so a mode set by a later control is applied but
not listed there; the check compares the report and the applied modes
accordingly. The head build always contains every setter.

### Interim compaction refusal (superseded by m6-02)

**Superseded.** m6-01 made `select_span` return
`NotEligible::ContextControlsInSpan` (`context_controls_in_span`) when the span
it would choose contained a turn whose effective mode was `pinned` or
`excluded`, so automatic compaction behaved as it does for every other
`NotEligible` reason. **[Decision 0019](0019-compaction.md#m6-02-amendment)'s
m6-02 amendment replaces this refusal with skipping:** coverage is the span minus
every controlled turn, `ContextControlsInSpan` is gone, and the new reason
`only_controlled_turns` covers the case where nothing eligible would remain.
The text below records what m6-01 shipped.

### Runtime command

`Command::SetContextControl { turn_id, mode }` is idle-only through
`idle_check`, appends one `context_control` entry at the current head through
`advancing`, and replies `Accepted::ContextControlSet { ack, turn_id, mode }`.
It emits `EventKind::ContextControlSet` after the acknowledgement, like
`ModelSwitched`. It has no durable command receipt, exactly like `SwitchModel`;
a retry after success is refused as `context_control_noop` and writes nothing,
which is the deduplication the journal needs. The core validates through
`SessionSnapshot::control_error`, the same function storage uses, so every
semantic refusal is a typed `Rejection::Invalid(code)` and a storage rejection
of a core-built control would be a core bug.

### Record version 3

One schema bump covers Milestone 6. `session-v3.schema.json` equals v2 plus the
`context_control` entry and an optional `Compaction.model`.

`Compaction.model` is **absent** to mean "no model was used"; `null` is a
schema violation in every version. Absent was chosen over null because v1 and
v2 require a model and nothing else in the journal spells an absent optional
value as `null`, so one spelling keeps "was there a model?" a structural
question. `Compaction.model` is `Option<ModelRef>` with
`skip_serializing_if = "Option::is_none"`; every writer in this step still sets
`Some`, and model-free strategies arrive in m6-04. Storage accepts a v3
compaction without a model and still requires every other provenance field.

Readers accept versions 1 to 3, each validated against its own bundled schema;
writers produce version 3 only. A v1 or v2 journal continues as v1/v2 prefix
and a v3 suffix on one checksum chain, exactly as v1 continued as v2 in
Milestone 3. `docs/rfcs/001/examples/context-controls.jsonl` is a v3 example
with four controls; the existing examples stay v1.

## Evidence

- Storage: `session::tests::controls` covers every rule and code, both
  compaction rules (including the same coverage accepted without a pin), a v3
  compaction without a model (field absent, never null), a v2 prefix continuing
  as v3, and replay equality through the oracle. `session::tests::versions`
  pins the v2 schema bytes, adds the v3 compaction and `context_control` schema
  tests, and covers versions 1 to 3.
- Projection: `context::tests` covers pin, exclude, reset, sibling controls, an
  excluded tool group, an excluded prefix invalidating a continuation,
  fingerprint stability under a pin, and the `context-controls` golden.
- Compaction: `compaction_tests::controlled_turns_are_skipped_and_ignored_in_the_retained_window`
  (m6-02 replaced the m6-01 refusal test of the same fixture).
- Runtime: `runtime::tests::controls` covers acceptance, every refusal state,
  typed storage codes, reopening and the no-op retry; the randomized test picks
  a control command on every seed.
- Clients: the TUI renders `Pinned turn …`, `Excluded turn …` and
  `Reset turn …`, the inspector lists `user_excluded`, and `harness context`
  prints the new golden.
