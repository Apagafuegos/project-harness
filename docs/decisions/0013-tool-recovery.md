# 0013: Read-only restart closure and advisory file-image hints

Status: Implemented in Milestone 3 step 6.

Date: 2026-10-28

## Decision

Recovery remains pure core logic over durable snapshots. No invocation is
replayed. Under D9, the single unresolved intent qualifies for automatic
closure only if its effects are exactly `Some({read})` and it has no recorded
`execution_uncertain`. Recovery first commits the transition to Recovering,
then a runtime-provenance result with its invocation ID, status `cancelled`
and code `process_restart`. It closes later not-started calls in branch order
and finishes the turn interrupted. There is no `execution_uncertain` or
`recovery_resolved` for that read. A v1 intent (`effects: None`) is unknown,
not read-only; an already uncertain read remains subject to user resolution.

D30 resolves the original step-6 ordering ambiguity: storage permits at most
one unresolved intent (`overlapping_execution` and next-call validation).
A read after an uncertain write is a not-started call. The user resolves the
write first; the existing path answers the trailing read with a runtime
`cancelled/process_restart` result **without** invocation ID. Tests use only
histories the real journal accepts, including read-result / write-intent /
not-started-read in one turn. No synthetic multiple-intent state is used.

## The one storage exception

Only the ToolResult rule changes. Runtime provenance with an invocation ID is
accepted only in Recovering (or for an interrupted turn), for an existing
matching earlier intent without a result or uncertainty, effects exactly
`Some({read})`, status cancelled, code process_restart. Existing call order,
identity, content and result-code validation still applies. Executor and
user-observed/acknowledged-unknown paths retain their existing rules.

Entries are validated before events, so the Recovering transition cannot
share the D9 result's first commit. A crash between transition and result is
safe: another recovery pass derives the same closure, never dispatches it.
There are no new journal entry/event kinds, schema changes, projection changes,
result codes, crates or dependency feature changes.

## Advisory evidence, not facts

For uncertain write/edit the core emits `InspectUncertain` with a fresh local
HintToken and the plan artifact from the intent's authorization. The driver
loads exact verified artifact bytes on the storage worker, bounded by the
existing artifact limit. Its dedicated executor thread calls pure
`tools::recovery_hint(bytes, workspace)`. Private deserialization extracts only
needed fields; it cannot deserialize or manufacture a Plan or authorization.

The five serialized hints are matches_post_image, matches_pre_image,
changed_since, missing and unavailable (with a content-free reason). Write
compares post-image first so a no-op write reports post. Edit has no post digest:
check pre first, then read up to 1 MiB and reverse-apply exactly one occurrence
of new_text to old_text. Count overlapping byte offsets, per decision 12;
ambiguous reversal is changed_since even if one possible reversal hashes to
pre. Empty replacement text is unique only for empty current text. Refusals
retain their machine code; no bash hint is requested.

Hints arrive as Input::Hints and nonpersisted EventKind::RecoveryHints. The
shared client reducer updates UncertainInvocation::hint, preserving it across
resync. No journal fact or model-facing tool result is inferred from a hint;
the user still resolves every uncertain invocation. Current-file equality is
not proof that this invocation caused it, and evidence may become stale
immediately after inspection.

## Driver lifecycle

Artifact reads have their own asynchronous future, not the exclusive commit
slot. Hint computation shares the sequential executor job queue with planning
and execution. Jobs/answers carry epoch and token. A real tool job arriving
while an inspection runs is deferred, with cancellation retained directly on
its token; no second job is dispatched concurrently. Pending hint work never
holds a recovery commit or ResolveUncertain. Resolving invalidates the hint
token, and stale/resolved answers are discarded. Missing host, unreadable or
oversized artifact, and inspection panic become unavailable advice rather
than session faults. Planning/execution failure semantics remain unchanged.

On shutdown/replacement, advisory artifact futures must be dropped before
waiting for storage exit: they retain worker senders even while unpolled.
The executor thread is still joined on shutdown, and outstanding effects are
cleaned up before recovery. Hint work is not an untracked detached filesystem
operation.

## Verification

- Storage accepts only the D9 exception and rejects mutation/process/mixed
  effects, v1 unknown effects, uncertain read, outside recovery, wrong status
  and wrong code. Reopen replays accepted D9 records.
- Core scenarios repeat/replay and check resync against real storage: D9
  read plus later calls, D30 write/read and read/write/read, already uncertain
  read, v1 intent, authorized artifact binding, stale/late hint inputs and
  resolution before hints. R01 Lost/Durable/Torn gates and seeded read-intent
  crashes preserve the storage oracle.
- Pure fake-workspace hint tests cover both image rules, absence/change,
  refusals, no-op write ordering, overlapping reverse matching, empty text,
  canonical plan goldens and serialized names.
- Production driver reopen tests use FsWorkspace in temporary directories
  for write pre/post/changed/deleted and edit reverse-post evidence. They
  verify unchanged execution counts, view resync, no journal hints and user
  resolution. Blocked inspection leaves views and resolution responsive;
  panic, missing host, artifact limit and stale epoch are tested.
- Break-on-purpose checks cover v1 D9 widening, uncertain-read storage
  widening, no-op ordering, nonoverlapping edit counting and waiting for hints.

CLI installation of real executors and recovery UI remain later steps.
