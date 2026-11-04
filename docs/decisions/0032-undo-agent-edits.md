# 0032: Durable undo of an agent turn's trusted file edits

Status: Accepted and implemented (M8 step 03b, record version 4).

Date: 2026-11-04

## Context

M8 decision 6 requires user-initiated undo, not history navigation or a model
rollback tool. History remains append-only. The oracle for this proposal is
`7b0c958c628d8a6dbf79d2eba700f5e79a3cdd4f`. References below name existing
files and functions; new types, fields, rules and commands are **planned**.

`session/state.rs::State::apply_record` supports retained audit commits and
head-advancing entries outside turns. `State::entry` already admits idle model,
compaction and context-control markers. However, an existing `ExecutionIntent`
requires an active turn, an Authorizing state, an earlier model proposal and a
consumed allow (`State::event`, `State::next_call`). A `UserMessage` requires
`turn_started`. Neither may be fabricated for undo. Add dedicated undo events
and a dedicated note entry, within the existing Commit/checksum/lock mechanism.
This is an additive extension, not a new storage architecture.

Related decisions: [0003](0003-jsonl-storage.md),
[0007](0007-startup-recovery.md), [0010](0010-tool-pipeline.md),
[0011](0011-trusted-file-tools.md), [0013](0013-tool-recovery.md),
[0021](0021-shared-wiring-session-catalog.md),
[0025](0025-context-controls.md), and
[0030](0030-session-export-deletion-and-gc.md).

## Decision

### 1. Unit, selection and branch semantics

Undo selects one **finished agent turn** on `ancestry(snapshot.head())`, in
reverse order of first appearance of its `turn_id`, not UUID or physical append
order. Finished includes completed, failed, cancelled and interrupted turns;
only durably evidenced applied trusted mutations are reversible. The outcome
comes from `SessionSnapshot::turn`, not the last assistant finish reason.
`SessionSnapshot::{ancestry,turn,events}` and `State::ancestry` in
`session/state.rs` supply these facts; `entries()`/`turns()` iterate in identity
order and must not be used as conversation order.

Fold consumed target turns from ancestral `undo_note` entries. Skip consumed
turns and turns with no trusted write/edit invocation (empty, read-only and
bash-only turns); report any skipped bash invocations as not reversed. Select
the newest remaining turn containing a trusted write/edit invocation. Do not
skip a legacy or unclassifiable mutation turn to reach an easier earlier turn.
If there is no candidate, return `undo_nothing`, listing skipped bash calls if
any. A candidate with only proven `not_applied` mutations also has no file
changes: consume it with an empty successful undo, allowing the next request
to reach earlier turns. A successful no-op write counts as applied.

The target's relevant proposals/results must be on the selected ancestry; a
head navigated into a finished turn but before its applied results is refused
as `undo_incomplete_turn`, never treated as the whole turn. Selection verifies
that all recorded entries of that turn are ancestral. Newer non-turn markers
(model changes, controls, compactions, undo notes) are not agent turns.

Repeat undo walks earlier unconsumed candidate turns. A new agent turn becomes
the next candidate. An undo never selects another undo: **no redo**. This avoids
promising re-execution of bash or reversal of a partly observed recovery.
Undo consumes a turn only on success or explicit resolution of an uncertain
undo; a known-not-applied undo leaves it eligible for a fresh user request.

Consumption is branch-derived, like ADR 0025 controls: an inherited note
consumes its target on descendants, a sibling note does not. Navigating to a
sibling never restores files; undo there must pass current filesystem checks.
Even on the same branch, navigating before a note removes its consumption from
that ancestry, not its physical effects: a new attempt normally conflicts.
There is no workspace snapshot per branch and no guarantee that sibling undo
can succeed. `State::apply_record` Navigate and `step` implement the existing
separation between branch facts and session-wide effects/configuration.

### 2. Capture before dispatch, image binding and size

Add a required nullable v4 field `ExecutionIntent.file_image`. Non-write/edit
intents have null. Trusted write/edit intents carry this object (all keys
required, no unknown keys):

| Key | Type and meaning |
| --- | --- |
| `path` | validated canonical `WorkspacePath` string from the bound plan |
| `pre_image` | null for absent; otherwise ordinary `ArtifactRef {sha256, bytes}` |
| `pre_mode` | null iff absent; otherwise integer ordinary permission bits, 0..511 |
| `post_image` | SHA-256 of the exact replacement bytes |

Old records decode a missing field as unavailable, not evidence of an absent
file. Plan identity and workspace are inherited from the intent's bound
`AuthorizationDecided.plan`, not independently selected by model JSON.
`tools/plan.rs::Plan::build` currently binds workspace, target path,
`pre_image` (digest or string `absent`), and for write only,
`content_sha256`. Edit binds literal arguments and a pre-digest, **not a
post-digest**. Keep canonical plan v2 unchanged. New trusted capture computes
edit's exact replacement with the same overlapping-match and UTF-8 rules as
`tools/fs/execute.rs::FsWorkspace::replace`; it hashes those bytes. For write,
it hashes the typed text and requires equality with `content_sha256`.

After authorization is chosen, but **before** `allow_plan` commits the intent,
a new sequential capture effect opens the target through `FsWorkspace`, checks
protection, regular/single-link status, parent identity and plan pre-image,
and publishes the raw pre-image bytes through
`session/artifacts.rs::DataRoot::put_artifact_reader`. Retain only bounded
buffers for streaming. Check stat stability before/after reading and require
published `ArtifactRef.sha256 == plan.target.pre_image`. On absence, verify
absence and store no artifact. Verify mode from that same stable descriptor.
A stale capture refuses without dispatch. A denied call captures nothing.
Revalidate approval expiry and revision after capture; capture cannot extend
an approval's lifetime.

Then publish the existing plan and any grant/configuration artifacts and commit
allow, intent-with-image, optional remembered grant, and transition to Executing
in the existing order. Only its durable acknowledgement dispatches execution.
`runtime/pipeline.rs::Runtime::allow_plan`,
`runtime/core.rs::Runtime::persisted`, and `runtime/worker.rs::run` currently
provide that ordering (worker publishes artifacts before append). Capture needs
new effect/worker plumbing; the existing `Vec<Vec<u8>>` artifact job must not
be used to buffer an arbitrarily large overwritten file. The capture effect
may stream from a verified descriptor to the artifact store, never let the
model supply a descriptor or artifact reference. A pre-intent crash can leave
an orphan artifact, not a mutation.

Execution retains the existing two pre-image checks in
`FsWorkspace::{replace,pre_image}`, including the pinned-root re-resolution
immediately before rename. It additionally verifies that the captured image
binding matches the plan and that computed replacement digest equals
`file_image.post_image`; the current ordinary mode must still equal captured
`pre_mode` before replacement. Thus capturing different bytes, changing an edit's
input, or changing the target between capture and execution cannot succeed.
The intent retains pre-images even if the result is lost.

**Size correction to the brief:** `tools/builtin.rs::Builtin::validate`
limits write text to 1 MiB in bytes; `Plan::build` and `FsWorkspace::replace`
limit edit input and output to 1 MiB. They do **not** cap the old target of an
overwriting write: `FsWorkspace::pre_image` calls streamed `resolve.rs::hash`
without a size cap. Do not silently introduce a new write restriction. Capture
and restoration stream that old target under the artifact store's existing
safe-integer size limit and cooperative deadline. A binary old file is valid
for write capture/restoration; pre-images are raw bytes, not `read_text`.
Write-created files use null pre-image, not an empty-byte artifact.

Add required nullable v4 `ToolMetadata.file_effect`: null for nonmutations;
`not_applied`, `applied`, or `unknown` for trusted write/edit invocations.
`FsWorkspace::replace` must track its rename commit point: before rename is
not_applied, after successful rename is applied **even if parent fsync fails**;
an ambiguous syscall/infrastructure outcome is unknown. Store the result and
this classification together. It is not inferred from status or result text:
today `execute_hooked` can report infrastructure_failure after a replacement.
For recovery results, user observation may classify applied/not_applied only
as an explicit user statement (extend the existing observed tool-resolution
command with required nullable `file_effect`, non-null only for a captured
mutation); acknowledged-unknown stays unknown. Capture is
still required. Unknown effects refuse undo (`undo_uncertain_effect`) rather
than guess. Make that classification during explicit tool recovery, not from
hints. Once a result acknowledges unknown, that immutable classification remains
unavailable for undo; manual filesystem reconciliation does not upgrade it or
rewrite its result. This rule also covers resolved legacy uncertainty lacking
reliable images. Keep file_effect out of projected tool metadata explicitly in
`context/projection.rs::without_code`, like code; it is storage evidence, not a
new model assertion.

### 3. Exact new records

Use record version **4**, retaining the existing `Record`, `Commit`,
`HeadChange`, `ArtifactRef`, checksums and UUIDv7 identity rules. Add `UndoId`
to the shared uniqueness namespace; it is not an InvocationId or TurnId.
All objects below reject unknown fields. Nullable keys are always present;
there is one spelling per v4 object.

An `UndoTarget` is `{path, invocation_ids, pre_image, pre_mode, post_image}`.
`invocation_ids` is a nonempty vector in original execution order, listing all
applied trusted mutations for this path in the selected turn. `pre_image` and
`pre_mode` come from the first; `post_image` from the last. Each adjacent
mutation's pre-digest must equal the previous post-digest; otherwise
`undo_conflict` (untracked intervening effect), before any restoration. Targets
are unique by path and sorted bytewise. No intermediate restoration is done.
A created-then-edited file is removed; an overwritten-then-edited file gets the
first pre-image. File aliases are subject to the existing single-link/no-symlink
rules, not string-based bypasses.

| Record | Exact payload fields, in addition to `kind` |
| --- | --- |
| event `undo_intent` | `undo_id: UndoId`, `target_turn_id: TurnId`, `anchor_head: EntryId or null`, `configuration: ArtifactRef`, `policy_revision: string`, `targets: UndoTarget[]`, `omitted_calls: CallRef[]` |
| event `undo_uncertain` | `undo_id: UndoId`, `reason: string` |
| event `undo_result` | `undo_id: UndoId`, `outcome: succeeded or not_applied or observed or acknowledged_unknown`, `file_outcomes: UndoFileOutcome[]`, `note_entry_id: EntryId`, `resolution_note: string or null` |
| entry `undo_note` | `undo_id: UndoId`, `target_turn_id: TurnId`, `outcome: same enum`, `text: string` |

`UndoFileOutcome` is `{path, outcome}` where outcome is `restored`, `removed`,
`unchanged`, `observed_restored`, `observed_removed`, `observed_other`, or
`unknown`. It has exactly one row per intent target, in target order.
`omitted_calls` lists every bash proposal (including denied/not-started calls
and skipped newer bash-only turns), plus executed non-write/edit calls declaring
mutation/process/network/external effects, in ancestral proposal/block order,
uniquely by CallRef; never store raw command bytes again. Resolve their displayed names/arguments
from the original proposal. Denied/not-started calls are labelled not executed,
not effects. Non-write/edit effects are never candidates for restoration.

Normal success requires every row restored (present pre-image) or removed
(absent pre-image); not_applied requires every row unchanged. Observed is a
user statement with observed_* or unknown rows and a non-null resolution note;
acknowledged_unknown requires every row unknown and a non-null note. The user
may acknowledge unknown even when hints look conclusive. No new model proposal,
approval, authorization, tool result, or turn-start/finish is emitted for undo.

### 4. Commit sequence and state.rs rules

The runtime/CLI uses one shared engine operation, not a CLI-only writer.
Follow `Runtime::{idle_check,set_context_control,persisted}` in
`runtime/core.rs`: serial effects, one commit in flight, immutable published
snapshot, no outward success before acknowledgement. A preview is ephemeral
and bound to head, sequence, configuration and selected targets. Confirmation
never authorizes a different turn after navigation or intervening work.
Add `Command::PreviewUndo` (read-only, ephemeral preview) and
`Command::Undo { command_id, expected_sequence, expected_head,
expected_configuration, target_turn_id }`; the latter recomputes selection and
checks all expected values before commit A. Both clients use these commands,
including --yes. A stale preview never falls back to selecting another turn.

1. Obtain/retain the session's exclusive nonblocking writer lock. Require idle,
   no active turn, no unresolved tool or undo work, no in-flight planning,
   execution, compaction or persistence. Select the turn and build targets.
   Verify all artifacts before any workspace mutation. Preview lists targets
   and omitted calls; ask y/N unless `--yes`.
2. After confirmation, revalidate preview binding and **every target** against
   its final post-digest, plus current policy protected spellings, descriptor
   protections, workspace identity, parents, regular type and single-link
   status. Missing targets conflict, including files undo would remove.
   Collect every conflicting path/reason; return `undo_conflict` and change
   nothing. Stage replacement temporaries and sync their bytes/modes; staging
   is not a target mutation. Recheck the whole target set after staging.
3. Commit A: retained head, no entries, exactly `undo_intent`. All referenced
   pre-images are already durable. Wait for journal sync acknowledgement.
4. On the sequential executor, revalidate the **whole** target set once more
   before the first target mutation. If this fails, restore nothing and commit
   a not_applied result/note. Otherwise restore targets in sorted order.
   Immediately before each rename/unlink re-resolve from the pinned root and
   recheck digest, parent identity, protection and link count. A present
   pre-image is restored with a same-directory random O_EXCL temporary,
   mode `pre_mode & 0o777`, fsync, atomic rename, parent fsync. Special bits,
   ownership, ACLs, xattrs, timestamps and old inode identity are not restored.
   An absent pre-image uses descriptor-relative `unlinkat` of the checked
   regular file, then parent fsync; never remove directories or parents.
5. Commit B on full success: Advance exactly one `undo_note` at the anchor head
   (`Entry.turn_id = null`), with exactly one matching `undo_result(succeeded)`.
   Publish result, consumption and model note only after sync. A proven failure
   before any mutation commits the same shape with not_applied, consumes
   nothing, and reports refusal/failure honestly. Once any target may have
   changed, do **not** claim not_applied: persist `undo_uncertain`, or fault
   leaving the intent unresolved if persistence fails. Await explicit user
   resolution; no automatic rollback or retry.

Planned semantic changes to `session/state.rs`, shared by runtime preflight and
append/replay validation:

| Site / invariant | Exact planned rule |
| --- | --- |
| `State::idle`, `has_unresolved`; `SessionSnapshot::requires_recovery` | Include an unresolved undo. Keep `active_turn = None` and operational state Idle during normal undo; separate derived undo state blocks ordinary work. Do not invent a turn merely to use Executing. |
| `State::event(UndoIntent)` | Require existing idle predicate, no unresolved invocation/undo, completed exchange at anchor, `anchor_head == head`, exact current config/revision, unique UndoId, eligible latest unconsumed ancestral turn, exact canonical target/source list and omitted calls. Intent-only Retain commit; no entries or other events/turn starts. Reject overlap as `overlapping_execution`. Empty targets allowed only for the all-not_applied candidate described in §1. |
| `State::event(ExecutionIntent)` | Preserve Authorizing, active-turn, next-call, allow/revision/expiry checks; extend no-unresolved test to undo. Validate v4 file-image shape only for trusted write/edit and match its path/pre-digest/post-digest to verified plan/capture through artifact validation. |
| `State::entry(ToolResult)` | Keep prior durable invocation, call order, provenance and D9 rules. Require the v4 file_effect classification for a captured mutation; no file_effect on other calls. Unknown is never silently upgraded by status. |
| `State::event(UndoUncertain)` | Exactly one prior durable unresolved UndoId, no result/previous uncertainty; Retain head, no entries, no turn. Set uncertainty flag and operational state Recovering. |
| `State::entry(UndoNote)` | No own turn, no turn-start in commit, parent equals intent anchor, empty pending exchange; intent from an earlier record. Special exemption from idle, **only** for this pending undo's terminal note. Validate deterministic text and matching payload against intent/result in the same commit. |
| `State::event(UndoResult)` | Prior durable unresolved intent; matching same-commit note and target rows. Succeeded/not_applied require no recorded uncertainty and state Idle. Observed/acknowledged_unknown require uncertainty and Recovering. Reject duplicate/mismatched result or note; clear pending undo, set Idle; no active turn or pending tool exchange permitted. |
| `State::apply_record` | Entries precede events today: prevalidate the terminal result/note pairing from the whole Commit before entry insertion; apply note against still-pending intent, then event clears it. Enforce the exact terminal shape (one entry, one event, Advance) and intent/uncertainty shapes. Reject standalone notes/results, co-committed intent/result, mixed ordinary work, and reused identities. |
| `step`, `BranchFacts`, scanning oracle | UndoNote requires empty pending calls, preserves model/controls; adds target to derived consumed set only for succeeded/observed/acknowledged_unknown. Validate this fold against an ancestral scanning oracle; no persisted cursor/cache. |
| `State::event(ConfigurationChanged)` | Preserve the existing non-idle same-commit validated allow/approval exception, in its existing event order. Additionally refuse **all** config changes while undo pending (even though normal undo's operational state is Idle). An undo is never that exception. |

Turn starts, navigation, model/control/compaction changes, inference,
authorization, configuration changes, and a second undo all refuse while an
undo is unresolved. Runtime maps a pending undo/tool uncertainty to
`undo_uncertain_pending`, ordinary active work to `session_not_idle`.
Recovery resolution and explicit tail repair remain permitted. Intent metadata
cannot authorize an arbitrary user path: source images and paths must be
derived from the target turn's durable trusted invocations.

New invalid-record codes are `undo_commit_shape` (mixed/incorrect commit shape),
`undo_binding` (anchor/config/source/selection mismatch), `undo_image_binding`
(image/plan/effect mismatch), `undo_result_binding` (note/rows/text/result
mismatch), and `undo_recovery_state` (wrong uncertainty/result phase).
Use existing `duplicate_id`, `session_not_idle`, `head_inside_tool_exchange`
and `overlapping_execution` for their existing invariant classes. These are
journal validation failures, not forceable user refusals; CLI wraps them as
`journal_invalid: <code>`. Runtime requests receive the specific user refusals
in §6 before trying to append an invalid record.

`session/validation.rs::record_artifacts` must verify every new pre-image and
intent configuration reference, plan/image agreement, and new result source
bindings, during append **and replay**. `State` has no filesystem I/O; structural
source-chain checks live there, artifact-byte checks in validation. Extend
`session/maintenance.rs::record_references` identically for journal export and
GC. A digest hidden only inside a plan artifact is not a GC reference today.

### 5. All-or-nothing boundary and recovery

“All-or-nothing” means a complete preflight refusal changes **no target**, and
normal success restores the whole selected set. It is **not** an atomic
multi-file filesystem transaction. Match the existing ADR 0011 threat boundary:
local Linux filesystem, honest sync, no hostile same-user concurrent mutation.
Digest rechecks are not filesystem compare-and-swap. A race/error after the
first mutation may leave a partial restoration; do not conceal it, and do not
try to reverse the reversal automatically.

Opening is read-only reconstruction (`session/store.rs::JournalStore::open`).
Extend `store.rs::recovery_commit` narrowly to admit undo_uncertain and the
paired terminal recovery note/result; no undo_intent through the recovery
barrier. Extend runtime Start recovery **before ordinary turn recovery**:
an undo intent lacking a result becomes undo_uncertain/process_restart in a
retained commit, even if inspection shows all files restored. No executor is
redispatched. `requires_recovery` keeps new work blocked until the user resolves
it. Journal I/O failures poison the writer as today
(`JournalStore::append_inner`); reopen, never resubmit uncertain effect bytes.
Tail repair stays explicit and fingerprint-bound.

Show ephemeral per-file hints by comparing current content with original
post-image (not restored) and desired pre-image/absence (restored/removed):
`matches_post_image`, `matches_pre_image`, `changed_since`, `missing`, or
`unavailable {reason}`. For equal images prefer matches_post_image, as
`tools/recovery.rs::recovery_hint` does for write; equality proves neither
execution nor attribution. For removal, absence matches desired pre-image.
Hints never become persisted facts or model assertions. Epoch/token checks,
executor cleanup before reopen/resolution, and stale-answer invalidation follow
`runtime/core.rs::Runtime::{recover,resolve_uncertain}` and ADR 0013.

Add typed `Command::ResolveUndo { undo_id, resolution }`: resolution is either
`Observed { files: UndoFileOutcome[], note: string }` or
`AcknowledgedUnknown { note: string }`. It is accepted only for the pending,
uncertain undo after executor cleanup, never while restoration is live. The TUI
recovery dialog and library handle expose it; the CLI refusal directs the user
to open that session in the TUI to resolve it (no provider inference needed).
The user resolves the whole undo with per-file observed states and a note, or
acknowledges unknown. Commit an observed/acknowledged_unknown result and its
note atomically, then consume the target to prevent accidental retry. An
observed partial restoration is **not** called successful all-or-nothing undo.
Earlier undo requests may still conflict with the resulting workspace.
The user must manually reconcile it; no force, automatic completion or redo.
An unresolved tool intent blocks undo globally, not merely on its branch:
`Invocation::unresolved` includes uncertainty without recovery_resolved, and
`State::idle` checks that set. Without resolution there is no reliable final
post-image or complete source set. D9's read-only closure is not applicable to
an undo (filesystem mutation), nor to an unknown v1 effect set.

### 6. Exclusions, legacy and user surfaces

Never reverse bash/process/network/external effects, read-only tools, model
responses, configuration/grants, or effects without captured pre-images.
Untracked bash changes to a target normally make the post-image/chain check
fail; equal content is not proof that bash had no other effects. Repeated undo
never reverses a bash command, even if its output looks like a file edit.

If a selected turn has any trusted mutation intent predating capture, refuse
**the whole turn**, even if some later mutations have images:

`undo_unavailable_legacy: turn <TURN> has no complete stored pre-images; turns recorded before undo support cannot be undone.`

A missing/corrupt artifact is a storage failure, not “legacy” or empty content.
Old journals open and replay without rewriting one byte; no backfill from the
current workspace is allowed.

**TUI:** use **Ctrl-Z**, free in `crates/cli/src/tui/keys.rs::KEYS` and
`documented` at the baseline (Ctrl-U already edits the prompt). Handle Ctrl-Z
inside the existing raw-mode terminal loop, not OS job control; add it to the
generated help and independent key coverage test. The pure UI emits typed
preview/confirm actions; its owning live session handle performs undo through
the same serialized command path. Never reopen its own locked journal.
Show a scrollable modal:

```text
Undo agent turn <TURN>? [y/N]
Restore: "src/a.rs"
Remove (created by agent): "src/b.rs"
Not reversible: bash <CALL> cwd="." command="cargo test" (status …)
Other effects are not reversed. No redo.
```

Paths/commands are escaped and terminal-control-safe, not raw pasted terminal
instructions. No file contents/pre-image bytes are shown. Enter defaults to N;
y/yes confirms, n/Esc cancels without records. Preserve prompt draft. On
conflict list paths and reasons, no force/approval bypass. Result lists restored,
removed, unchanged or uncertain paths and every omitted call; unresolved undo
uses recovery UI, not another confirmation. Bash arguments already recorded in
the journal may be sensitive: show on explicit preview only, never diagnostics.

**CLI:** `harness sessions undo <ID> [--yes] [--data-root DIR]`, using existing
M7 config/root precedence (`crates/cli/src/sessions_cli.rs::{data_root,run}`).
Hold the exclusive nonblocking journal lock from preview through final commit;
a live engine elsewhere yields session_locked. No provider login/inference is
needed. Ask the same y/N question on a terminal; without a terminal require
--yes. --yes suppresses only confirmation, never checks. Print the file report
and omitted calls even with --yes. Cancellation is exit 0 with no records;
success is 0, refusal/usage is 2, storage/I/O failure is 1, matching
`sessions_cli.rs::{Failure,confirm,run}`. A persisted uncertainty is a refusal
(exit 2) requiring resolution; a failure to persist it is storage failure 1.

| Stable refusal | Message/detail |
| --- | --- |
| `undo_conflict` | sorted escaped paths and content-free reasons; no target changed on preflight refusal |
| `undo_nothing` | no unconsumed trusted file-edit turn on the active branch |
| `undo_unavailable_legacy` | exact message above |
| `undo_uncertain_pending` | resolve pending tool/undo intent `<ID>` before undo |
| `undo_uncertain_effect` | turn `<TURN>` contains an effect whose application is unknown; manual reconciliation cannot upgrade the immutable result |
| `undo_incomplete_turn` | selected head does not contain the whole finished target turn |
| `undo_stale_preview` | session/head/configuration changed; preview again |
| `session_not_idle` | finish/cancel current work before undo |
| `session_locked` | another writer owns the session |
| `session_not_found` | no such session |
| `confirmation_required` | use --yes when no terminal is available |
| `recovery_required` | recover/repair the session before undo |
| `journal_invalid: <code>` | existing invalid/unsupported journal refusal; never repair automatically |

An execution error with possible effects is reported as undo_uncertain_pending
with its UndoId and per-file hints, not as a harmless undo_conflict.

### 7. Recorded model note and context report

The success note's exact text is deterministic UTF-8, LF-separated with a final
LF; paths are JSON strings in target order, IDs use their stored spelling:

```text
The user reverted the agent's trusted file edits from turn <TURN> (undo <UNDO>).
Restored: "src/a.rs"
Removed (created by the agent): "src/b.rs"
Not reversed: bash call <CALL> from response <ENTRY>.
Bash and other effects were not reversed. Do not assume the reverted edits are present.
```

One Restored/Removed line per target; one Not reversed line per omitted call,
using its actual tool name, never command text. Empty targets have the line
`No trusted file changes required restoration.` instead of path lines.
For not_applied the first line is
`The user requested undo of turn <TURN> (undo <UNDO>); no files were reverted.`
and each target line is `Unchanged: <JSON_PATH>`.
For observed resolution the first line is
`The user resolved interrupted undo <UNDO> of turn <TURN>; restoration was not confirmed all-or-nothing.`
followed by `User observed <FILE_OUTCOME>: <JSON_PATH>` per row and a
`User resolution note: <JSON_STRING>` line.
For acknowledged_unknown the first line is
`The user acknowledged unknown effects of interrupted undo <UNDO> of turn <TURN>; which files were reverted is unknown.`
followed by `Unknown: <JSON_PATH>` lines and the escaped resolution note.
Non-success notes end with the same Not reversed lines and final warning as
success, but replace its last sentence with
`Do not assume either the original edits or their restoration are present.`
Never call an uncertain operation a confirmed reversion.

Project UndoNote as an existing `ContextItem::User` with one text block and its
note EntryId, **not** as a new turn or system/developer authority. This uses the
provider-neutral user mapping already in `context/projection.rs::ContextItem`;
`context/builder.rs::{ContextBuilder::build_inner,Walk::user}` needs a new entry arm,
and `context/check.rs::{agrees,complete,ordered}` must recognize its source.
The text explicitly says it is a user action, never executor/model output.
Pre-images and commands are not part of the note. Old tool results remain
historical; the later note corrects assumptions rather than rewriting them.

Keep undo notes verbatim on the selected ancestry, independent of controls on
the target turn. Never cover them in a compaction: extend
`session/coverage.rs::error`, `State::compaction`, and
`context/compaction.rs::select_span` to treat the latest undo note as a coverage
barrier (select only an eligible span after it; old summaries remain intact).
In `select_span`, choose the rolling `previous` only from summaries whose
coverage starts after that barrier; if none exists, start fresh after the note,
without superseding a pre-barrier summary.
This deliberately trades compaction opportunities for durable correction;
existing summaries may describe old edits but the later note always follows.
Do not globally apply a sibling note. Prefix rebuilds include only notes actually
in that prefix; an old continuation may replay with the subsequent user note,
but no continuation may omit that note from its request. Test provider input,
not only a fingerprint change.

In the ADR 0025 context report, included contains
`{entry_id: <NOTE>, kind: user, estimated_tokens: <N>, opaque_bytes: 0, blocks: []}`
(with the other current default fields); controls remains about pins/exclusions,
not filesystem undo. Add optional omitted-when-empty
`undos: [{undo_id, target_turn_id, note_entry_id, outcome, paths}]` in note
ancestral order. All note text is counted in user tokens and fingerprints.
Update report completeness/source checks and the TUI inspector/CLI context
renderer to display this table; do not report the note as context_control_marker.
Bump `BUILDER_VERSION` 6 -> 7 and `PROJECTION_VERSION` 5 -> 6 for changed
inclusion/compaction rules (`context/mod.rs`,
`projection.rs::fingerprint`). No-note histories still have the global version
change; tests must not falsely promise identical old fingerprints.

### 8. Versioning, authorization and retention

Add an independent v4 schema; freeze v1-v3 schema bytes. Readers accept 1..=4,
writers emit 4, even when appending to old journals. Missing v4 image/effect
fields decode unavailable for v1-v3 only; v4 requires explicit null or value
as specified. Reject version regression. Opening never migrates records.
An older binary refuses cleanly at the first v4 record with
`unsupported_version`, never ignores an undo or appends past it. These are the
existing rules in `session/codec.rs::{decode,encode}` (`SUPPORTED_VERSIONS`,
`CURRENT_VERSION`), `State::apply_record`, and `store.rs::scan`.
Checksum framing, 8 MiB physical record bound and command dedup stay unchanged.
Preflight serializes and bounds both the intent and every possible terminal
shape before target changes; do not split one undo into multiple intents to
fit. Bound reason strings to 1 KiB and resolution_note to 64 KiB UTF-8; reserve
that maximum and worst-case JSON escaping in the terminal record-size check.
Otherwise commit B could exceed 8 MiB only after files had changed. Use the existing same-CommandId receipt
mechanism (`JournalStore::{bind_command,append_inner}`) for commit A so a lost
acknowledgement cannot select another turn or dispatch twice. A recovered
receipt locates the existing UndoId/result; receipt replay is never permission
to dispatch an intent again. Fresh user requests get fresh command identities.

Undo is a trusted **user action**, with explicit confirmation but no model
approval or remembered grant. It bypasses model approval, **not enforcement**.
Require the recorded workspace identity and available trusted workspace host;
current configured protected spellings and pinned inode/ancestor protections
win over historical allowances. A newly protected target always refuses the
whole set. Reuse `tools/fs/resolve.rs::{open_relative,FsWorkspace::resolve,
FsWorkspace::regular,verify_unchanged}` with RESOLVE_BENEATH, NO_SYMLINKS,
NO_MAGICLINKS and NO_XDEV, plus policy protection in
`tools/plan.rs::refuse_protection`. `tools/fs/mod.rs::FsWorkspace::open` has
no insecure fallback. Revalidate from pinned root before every commit point,
including unlink; a retained parent descriptor alone cannot prove containment
after relocation. No shell, external executable or arbitrary-path restore API.

Pre-image artifacts can retain secrets deleted or overwritten later. They are
private 0600 files under 0700 directories, **not encrypted**, and are never
included in model context, output captures or Markdown exports. Journal backup
exports include them and need the same care as credentials. Update Markdown
export to render the undo note, never image bytes.
`session/maintenance.rs::{record_references,DataRoot::delete_session}` and
`artifacts.rs::DataRoot::{put_artifact_reader,lock_artifacts}` define retention:
all journals/all branches retain their referenced images indefinitely, even
after undo; deleting a session removes only its journal. Explicit GC may reclaim
images only when no journal references them, after the existing 24-hour grace,
under the existing publication/reuse lock. Shared/deduped references keep bytes.
There is no automatic expiry, secure erasure, or per-turn purge. An unsupported,
locked, torn or corrupt journal still refuses the entire GC run. Add direct
image refs to both validation and GC/export enumeration, not just one.

### 9. m8-03b verification plan

No live provider or credentials are required. Add behavioural tests beside
storage, runtime, file executor, context and client tests, using storage-valid
histories and real descriptor-rooted temporary workspaces.

- Selection: active/sibling ancestry, inherited and sibling consumption,
  navigation before notes, shared ancestors, multiple responses in one turn,
  incomplete navigated turn, empty/read/bash-only skips, failed/cancelled turns,
  no-op mutation, zero-applied candidate, repeated undo and new turn after undo;
  never redo an undo. Check skipped bash reports and mixed bash/write turns.
- Capture: overwrite/create/edit, binary old write target, empty file versus
  absence, >1 MiB overwritten write target streamed, edit input/output limits,
  same file repeatedly (including create+edit), discontinuous image chain,
  stale capture/stat/digest/mode, post digest for edit, artifact-publication
  failure, expiry during capture; assert no dispatch before intent ack.
- Storage: exact v4 required/null fields, impossible image/effect combinations,
  wrong/nonancestral/previously consumed/non-latest target, wrong config/revision,
  source/invocation/order/path mismatches, duplicate identities/results/notes,
  missing prior durable intent, orphan note, mixed commits, same-record
  intent/result, invalid result rows/text/provenance. Test all idle/overlap and
  configuration exception cases, including turn start in undo intent commit.
  Reopen and compare derived state with scanning oracle after every commit.
- Restoration: full multi-file success, created-file unlink and parent sync,
  ordinary modes and special-bit stripping, no parent deletion, conflict on any
  one of many targets changes none; deleted/modified target, symlink, hard link,
  nonregular file, workspace mismatch, protected path added since original edit,
  renamed protected inode/ancestor and relocation outside root. Recheck races
  after staging/intent and between two target operations; classify honestly.
- Unknown effects: parent-sync failure after original write and after undo,
  missing executor outcome, unknown versus user-observed file effect, unresolved
  tool intent including another branch. Hints pre/post/changed/missing/unavailable,
  equal-image preference, user observed partial and acknowledged unknown;
  hints never resolve, consume or retry. Late epoch/token answers do nothing.
- Crash gates: before capture, after artifact sync/before intent (no mutation,
  orphan protected by grace); torn/lost commit-A ack (no dispatch; explicit
  tail repair where needed); after intent before first restore (uncertain);
  after each rename/unlink and before/after parent sync, especially partial
  multi-file restoration (uncertain, per-file hints, never auto-complete);
  after all restores before result (still uncertain); torn result (repair then
  uncertainty); durable result with lost ack (replay note/consumption once,
  no effects). Repeat crashes during uncertainty and resolution commits.
- Context: exact note text/escaping and path order, report included/undos table,
  token counts, deterministic fingerprints, sibling isolation, excluded/pinned
  target and summarized target, compaction barrier, user-observed/unknown notes,
  native continuation plus subsequent note in each provider's submitted input.
  Assert no pre-image bytes/artifact ref enters model text or Markdown.
- Compatibility/maintenance: frozen v1-v3 fixtures replay unchanged; mixed old
  prefix/v4 suffix; legacy and mixed-capture turn refuse as specified; old reader
  rejects v4; version regression; pre-image digest/length corruption blocks load;
  journal export restores a usable undo; GC keeps images from non-active branches
  and unresolved intents, deletion leaves bytes, grace/dedupe locking protects
  capture before intent. Test 8 MiB intent preflight refusal before mutation.
- Surfaces: Ctrl-Z is documented and cannot suspend terminal; y/N default and
  Esc/draft preservation; --yes/nonterminal refusal; exact codes/exit statuses;
  lock contention with live engine; no provider needed; stale preview cannot
  undo a different turn; confirmation and result both enumerate omitted calls.
- Break-on-purpose: remove artifact-before-intent gate, pre-digest equality,
  edit post-digest check, whole-set preflight, per-target root re-resolution,
  protection/link check, parent sync, unresolved-undo barrier, note/result atomic
  pairing, no-retry recovery rule, GC image enumeration, or compaction note
  barrier. Each removal must fail a named behavioural assertion, not merely
  a source-text grep. Mutate mode masking and absence handling separately.

## Consequences

The implementation adds capture I/O, raw-byte artifact retention, one durable
intent/result pair per undo, a branch note, v4 schema support and narrow new
replay rules. It does not add a second journal/database or loosen sequential
execution. It exposes real uncertainty instead of promising multi-file atomicity.
Unchanged context history may still describe the original edits; the retained
user note is authoritative about the user's reversion or its uncertainty.
Old binaries cannot continue a journal after its first v4 append. Large old
write targets increase disk and streaming time; capture failures prevent the
agent mutation rather than permit an unprotected successful edit.

## Alternatives considered

- **Move the head back / delete entries:** loses neither physical effects nor
  bash side effects, and would hide evidence instead of recording restoration.
- **Model tool calls for undo or fabricated turns:** violates proposal/turn/
  authorization rules and misattributes the user's action to the model.
- **Only store pre-images in result metadata:** a crash after mutation before
  result loses the durable reference; an intent must retain them first.
- **Put refs only inside plan JSON:** GC does not recursively trace artifacts.
- **Infer edit post-images by reverse matching:** ADR 0013's heuristic is useful
  advice, not exact proof; deletion/ambiguous replacement defeats it.
- **Best-effort restore or automatic retry/rollback:** violates the preflight
  all-or-nothing refusal and duplicates uncertain side effects after crashes.
- **Force protected/conflicting targets, or use shell/git checkout:** bypasses
  workspace boundaries or overwrites user work; git need not track the files.
- **Redo and full filesystem snapshots:** broader semantics, retention and crash
  handling than required; no credible way to reverse arbitrary bash effects.
- **New universal 1 MiB pre-image cap:** needlessly removes today's ability to
  overwrite a large file; streamed content-addressed artifacts already exist.
