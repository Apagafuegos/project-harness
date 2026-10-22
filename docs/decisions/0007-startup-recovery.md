# 0007: Startup recovery and tail repair

Status: Accepted for Milestone 2, step 6.

Date: 2026-10-22

## Context

A crash or a fault can stop a session at any step: mid-turn, with tool calls
proposed but unanswered, after an execution intent with no result, or halfway
through writing a record. Storage already refuses to guess. Opening never
appends history, a reopened writer accepts only recovery commits until nothing
is unfinished, and a torn tail blocks a writable open. Until step 6 nothing
performed those recovery commits: a reopened runtime sat in
`RecoveryRequired` and refused every command, and no `SessionHandle` path
reached `JournalStore::repair_tail`.

The rule to keep is RFC-001 section 7: never replay a side effect and never
invent history. An unfinished turn is a fact that can be recorded. Whether a
tool ran is a fact only the user can supply.

## Decision

**Recovery is ordinary core commits after open.** Storage still never
synthesizes history. A new input, `Input::Start`, is fed by the driver right
after it constructs a runtime: on open, create and reopen, and after a repair,
before any message is served. If the snapshot needs recovery, the core plans
it in that step and each recovery commit's acknowledgement plans the next. The
commits are built from the published snapshot like any other, validated by
storage, and atomic, so a crash during recovery leaves either nothing or one
whole step. The next `Start` resumes from the new snapshot.

**A turn with nothing uncertain closes without asking.** One commit answers
every pending call that has no execution intent with a `cancelled` runtime
result whose text leads with `process_restart`, moves the turn to
`recovering` unless it is already there, and finishes it `interrupted` with
code `process_restart`. The session returns to `Idle` and a `Recovered`
event names the turn and the answered calls. A partial response that was
streaming at the crash was never persisted, so nothing is written for it.

**Uncertainty is recorded, then the user resolves it.** An execution intent
without a result may or may not have run. `Start` commits `execution_uncertain`
for it. If no not-started call follows on the branch, the same commit
finishes the turn `interrupted`; otherwise the turn stays open, because
storage answers calls in branch order and the uncertain call is first. The
core then enters `AwaitingResolution`: the view and a `ResolutionRequired`
event list each invocation with its call, tool name, plan digest and intent
time. Every command except `ResolveUncertain`, and the receipt of an already
accepted `StartTurn`, is rejected with `ResolutionRequired`.
`ResolveUncertain { invocation_id, resolution }` takes either
`Observed { status, text }`, recorded with `user_observed` provenance, or
`AcknowledgedUnknown`, recorded with status `uncertain` and
`user_acknowledged_unknown` provenance. The result and `recovery_resolved`
are one commit. When it resolves the last uncertain invocation of a still-open
turn, the same commit answers the remaining not-started calls and finishes
the turn. Nothing is resolved, retried or reconciled automatically.

**The model can tell a user's statement from tool output.** Tool-result
projection items carry their `provenance`, and so do the report's included
records. `PROJECTION_VERSION` and `BUILDER_VERSION` become 3. How a provider
labels a user-resolved result is left to the Milestone 4 adapters.

**Repair is bound to the inspected file.** A torn tail makes open and
`reopen()` fail with `PersistError::TornTail`, which carries the full
inspection: valid and total bytes, whole-file fingerprint and the valid
prefix's snapshot. The user confirms; the client then calls
`SessionHandle::repair_tail(root, session, fingerprint, size, provider,
config)`, or `reopen_repairing_tail(fingerprint, size)` on a faulted handle.
The worker runs storage's repair under the writer lock: it rescans, refuses a
changed file (`StaleRepair`), a complete corrupt record or missing header
(`NotRepairable`) and a session another engine holds (`Locked`), quarantines
the tail into an artifact, truncates, syncs and commits `tail_repaired`. The
session then opens and `Start` recovers it. `harness journal inspect` and
`harness journal repair --fingerprint F --size N` offer the same flow without
an engine; neither picks the values for the user.

## Storage sequences

Storage accepted every sequence this decision describes; no rule changed.

| Shape | Commits |
| --- | --- |
| Unfinished turn, no calls | `<state>>recovering`, `turn_finished(interrupted, process_restart)` |
| Unanswered calls | `cancelled` runtime result per call, `<state>>recovering`, `turn_finished(...)` |
| Uncertain call alone | `execution_uncertain`, `turn_finished(...)`; then the user's result and `recovery_resolved` |
| Uncertain call, then a not-started call | `execution_uncertain`; then the user's result, a `cancelled` runtime result, `recovery_resolved`, `turn_finished(...)` |

`execution_uncertain` moves storage to `recovering` itself, so the uncertain
shapes carry no state change.

## Consequences

- `Phase::RecoveryRequired` now means automatic recovery is planned or in
  flight; it lasts one commit. `Phase::AwaitingResolution` is new, and so are
  `Rejection::ResolutionRequired`, `Accepted::Resolved`, the `Recovered` and
  `ResolutionRequired` events and `RuntimeView::awaiting`.
- Every turn ever started ends with exactly one `turn_finished` once its
  uncertain effects are resolved, including after faults and crashes. The
  randomized test now requires it of every run.
- `UnresolvedInvocation` gains `at`, the intent's record time, kept in memory
  by replay. The journal format, schema and validation rules are unchanged.
- `JournalStore::repair_inspected_tail` takes the fingerprint and size alone;
  `repair_tail` delegates to it.
- `SessionHandle::open_with_env` and `create_with_env` take an `Env`, so a
  handle can run with deterministic identities, as the example does.
- Golden reports change only in their fingerprints and versions.

## Alternatives considered

- **Let storage close interrupted turns while opening.** Opening would write
  history the user never sees planned, and storage would make lifecycle
  decisions that belong to the core.
- **Resolve uncertainty from what the tool would most likely have done.** A
  read-only tool looks safe to assume, but the fixture shows why not: nothing
  proves whether it ran. Milestone 3's tools may offer a reconciled
  suggestion; the user still decides.
- **Repair a torn tail on open.** Cutting bytes is safe only for the exact
  file the user inspected; another process could append in between.
- **Two commits for the final resolution.** Storage accepts one, so the
  answers and the turn's end are atomic with the resolution.
