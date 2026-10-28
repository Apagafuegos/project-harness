# Runtime core

Status: Milestone 2 is implemented as `harness_core::runtime`,
`harness_core::model` and `harness_core::context`: a pure synchronous state
machine, a dedicated storage worker, a Tokio driver/handle with broadcast
subscriptions, the provider stream seam with a scripted mock, the context
builder that the core runs in `Preparing`, and startup recovery with explicit
tail repair. Milestone 3 steps 4a–4b add the pure-core tool pipeline and async
tool-host dispatch on a dedicated executor thread.
[Decision 0010](decisions/0010-tool-pipeline.md) records that boundary. The
trusted filesystem executor is implemented separately in
[step 5](decisions/0011-trusted-file-tools.md); sandbox execution and CLI
installation of a tool host remain planned.
Decisions [0004](decisions/0004-pure-runtime-core.md),
[0005](decisions/0005-provider-stream.md),
[0006](decisions/0006-context-builder.md) and
[0007](decisions/0007-startup-recovery.md) record the design; the builder itself
is described in [context](context.md).

## Shape

```rust,ignore
let mut runtime = Runtime::new(snapshot, configuration, RuntimeConfig::default(), &mut env); // emits nothing
let effects: Vec<Effect> = runtime.step(input, &mut env);
```

`configuration` is the session's decoded configuration artifact
(`SessionConfiguration`), read by the caller because the core performs no I/O.
`step` does nothing except what it returns. It performs no file, network or
channel access, never reads the clock, and creates identities only through
`Env`: `now()` plus generators for runtime epochs, entry, turn, response and
call IDs, approval/decision/invocation IDs and new policy revisions.
`SystemEnv` uses the wall clock and random UUIDv7s. `DeterministicEnv` has a
fixed time and counter-derived UUIDv7s, with a 12-bit stream number per runtime
so identities stay unique across a reopen. Taking `&mut self` is equivalent to
`(state, input) -> (state, effects)`.

Storage is the authority on lifecycle rules. The core reads pending calls, the
model at a head, receipts, used identities and the recovery requirement from the
published snapshot. It keeps only what storage cannot know: the commit in
flight, the current stream and its validation state, the inference count of the
active turn, timers, and a requested cancel.

### Inputs

| Input | Carries | From |
| --- | --- | --- |
| `Command` | `RequestId`, `Command` | Client, through the driver |
| `Persisted` | `CommitToken`, `CommitAck`, new `SessionSnapshot` | Storage worker, after sync |
| `PersistFailed` | `CommitToken`, `PersistError::{Rejected{code}, Io{message}, Poisoned, WorkerLost}` | Storage worker |
| `ArtifactsVerified` | `CommitToken`, missing/invalid references | Storage worker, before C1 |
| `StorageFailed` | `PersistError` | Worker exit or failed artifact I/O |
| `DriverInvariant` | stable `code` | Driver refused an effect that would break its own invariant |
| `PublishView` | — | Driver attaching a replacement runtime; emits the current phase |
| `Start` | — | Driver, right after constructing a runtime; plans any recovery |
| `Provider` | `InferenceId`, `ProviderEvent` | Provider stream |
| `ProviderEnded` | `InferenceId` | Stream closed, with or without `Finished` |
| `TimerFired` | `TimerId` | Driver timer (inference or approval) |
| `Planned` | `PlanToken`, `Result<Plan, Refusal>` | Driver workspace planning |
| `ToolFinished` | `ExecutionToken`, `ExecutionOutcome` | Executor, after cleanup |
| `Hints` | `HintToken`, invocation/hint pairs | Executor-thread advisory inspection |

Commands are `StartTurn { command_id, content }`, `Cancel { turn_id }`,
`SelectHead { head }`, `SwitchModel { model }` and
`ResolveUncertain { invocation_id, resolution }` (see [Recovery](#recovery)), and
`SubmitApproval { approval_id, plan_digest, decision, remember }`. `RequestId`, `CommitToken`,
`InferenceId` and `TimerId` are `u64` values local to one runtime and never
journaled.

Provider events follow Contract 1: `Started`, `TextDelta`,
`ReasoningSummaryDelta`, `ToolCallStarted`, `ToolArgumentsDelta`,
`ToolCallReady`, `UsageUpdated` and `Finished { outcome, finish_reason, error }`.
Blocks are named by a response-local `block_index`. The core issues each call's
`CallId` when it sees `ToolCallStarted`, so a provider cannot choose journal
identities. `ToolCallReady` carries no value; the core parses the concatenated
fragments itself.

### Effects

| Effect | Carries | Meaning |
| --- | --- | --- |
| `VerifyArtifacts` | `CommitToken`, artifact references | Verify before building C1 |
| `Persist` | `CommitToken`, `Commit`, optional `(CommandId, Value)`, artifacts, `at` | Publish artifacts, bind command, append, answer |
| `StartInference` | `InferenceId`, `TurnId`, head, `ModelInput`, tools, fingerprint, `ResponseId` | Open a stream for the projection built in `Preparing` |
| `PlanCall` | `PlanToken`, call, tool, validated operation | Plan against the trusted workspace and current policy |
| `Execute` | `ExecutionToken`, `AuthorizedOperation` | Run after intent acknowledgement |
| `CancelExecution` | `ExecutionToken` | Request cancellation; still await the executor answer |
| `InspectUncertain` | `HintToken`, invocation/authorized-plan artifact pairs | Request nonblocking advisory evidence |
| `CancelInference` | `InferenceId` | Cancel the stream's token; keep delivering it for a confirmation |
| `ReleaseInference` | `InferenceId` | Stop listening and drop the stream; once per `StartInference` |
| `SetTimer` / `CancelTimer` | `TimerId`, duration | Driver timer |
| `Emit` | `RuntimeEvent` | Broadcast |
| `Reply` | `RequestId`, `Result<Accepted, Rejection>` | Exactly one per `Command` |

`Persist.at` is the record timestamp from `Env::now`, because
`JournalStore::append` takes one. `StartInference.response_id` is the identity
the assistant entry will carry, which RFC-001 gives the provider request.

`Accepted` is `TurnStarted { turn_id, ack }`, `Cancelling { turn_id, ack }`,
`Resolved { invocation_id, ack }`, `HeadSelected { ack }` or
`ModelSwitched { ack }` or `ApprovalSubmitted { approval_id, ack }` (decision durable).
`Rejection` is `Busy`, `RecoveryRequired`,
`ResolutionRequired`, `Faulted`, `NotActive`, `StaleCommand` or `Invalid(code)`.
The handle can also return `Overloaded` before submission, and the driver
returns `ShuttingDown` for commands it receives while draining; neither reaches
the core.

`RuntimeConfig` has `max_inferences_per_turn` (32), `max_partial_bytes`
(4 MiB), `max_user_bytes` (1 MiB), `cancel_grace` (2 s),
`provider_idle_timeout` (300 s) and `context`, the builder's `ContextConfig`
(model catalog, budget deductions, compaction trigger, estimator).

## Durability

- At most one `Persist` is in flight. Nothing else is persisted until its token
  is answered.
- The published snapshot changes only on `Persisted`, after the core checks that
  the acknowledgement extends the previous snapshot by exactly one record.
  Events describing durable facts follow that check. Stream deltas are emitted
  at once and never persisted individually.
- Transitions share a commit only when nothing happens between them.
  `Preparing -> Inferring` is always its own commit, because context building
  belongs in `Preparing`.

## Turn commits

These are the commits the tests assert. Entries come first, then events in order.

| Commit | Entries | Events |
| --- | --- | --- |
| C1 open | user message | `turn_started`, `idle>preparing` (carries the command) |
| C2 infer | — | `preparing>inferring`; `StartInference` follows its ack |
| Unknown budget, instead of C2 | — | `turn_finished(failed, context_budget_unknown)`; no stream |
| Overflow, instead of C2 | — | `turn_finished(failed, context_overflow)`; no stream |
| C3 stop | assistant `completed/stop` | `inferring>processing`, `turn_finished(completed)` |
| C3 tool calls | assistant `completed/tool_calls` | `inferring>processing` |
| C4 answers (no executors) | one runtime result per pending call | `processing>continuing`, `continuing>preparing`, then C2 again |
| C4 at limit (no executors) | one runtime result per pending call | `processing>continuing`, `turn_finished(failed, inference_limit)` |

Entering `Preparing` (after C1, or after C4's `continuing>preparing`) builds
the context, unless a cancel is pending or the inference limit is reached. The
core emits `ContextBuilt` first. If the model has no configured context limit,
the turn fails from `Preparing` with `context_budget_unknown` (RFC-001 C01);
if the projection overflows the budget, with `context_overflow`. Either way the
provider is never asked for a stream. Otherwise the core keeps the projection,
commits C2 and sends that projection with `StartInference` after the ack. C2
adds no entries, so the projection is still the head's; the core checks the
head matches and that it is not a preview. `RuntimeConfig::default()` has an
empty model catalog, so a runtime that infers must list its models'
limits in `context.catalog`. Inspection still reports an unknown budget. A
builder invariant failure faults the core with its code.

Other stream endings record the response and finish the turn in one commit:

| Stream ending | Assistant entry | Events |
| --- | --- | --- |
| `completed/refusal` | `completed/refusal` | `inferring>processing`, `turn_finished(completed)` |
| `completed/output_limit` | `completed/output_limit`, incomplete calls dropped | `inferring>processing`, `turn_finished(failed, output_limit)` |
| `failed` with a valid error | `failed`, that error | `turn_finished(failed, <error code>)` |
| `cancelled` by the provider | `cancelled` | `turn_finished(cancelled, provider_cancelled)` |
| EOF without `Finished` | `interrupted` | `turn_finished(failed, provider_eof)` |
| No event within `provider_idle_timeout` | `interrupted` | `turn_finished(failed, provider_stalled)` |
| over `max_partial_bytes` | `interrupted` | `turn_finished(failed, response_too_large)` |
| protocol violation | `failed`, `malformed_stream` error | `turn_finished(failed, malformed_stream)` |

Storage determines the tool-answer sequence:

- With no executors (`Runtime::new`), each pending call gets a `ToolResult`
  with status `denied`, no invocation, `runtime` provenance and the text
  `tools_unavailable: …`, and the code `tools_unavailable`. Results keep their
  code as the text's prefix too. The proposal is durable in C3 before any result. C4 uses storage's
  `processing>continuing` path for calls that were rejected without
  authorization, then `continuing>preparing`.

### Tool pipeline

Construct `Runtime::with_tools` with a fixed `ToolSet` (registry plus available
backends). For each proposal, in block order: resolve, validate, commit
`processing>authorizing`, emit `PlanCall`, then `decide` on `Planned`.
A resolve/validation/planning refusal records one nonexecuted result. Unknown
and disabled tools never plan; an unavailable backend returns `tools_unavailable`.

| Policy outcome | Durable steps |
| --- | --- |
| Allow | authorization + intent + `authorizing>executing`; after ack, `ToolStarted` and `Execute` |
| Ask | publish plan, commit request, emit `ApprovalRequested`, arm five-minute timer |
| Deny | authorization with reason equal to code; after ack, result and onward transitions |
| Executor answer | publish captures, commit result with executor provenance; after ack, `ToolFinished` |

Denial and result are **separate commits** (decision 24). Storage applies
entries before events and cannot validate authorization against a call already
answered in that commit. A crash between these commits is known not started:
existing recovery records `cancelled/process_restart`, never uncertainty.
Allow and intent remain one commit. The intent's effects and artifact bytes are
exactly the plan's. No `Execute` precedes its acknowledgement.

Results move `authorizing>processing` or `executing>continuing>processing` when
calls remain. With none remaining, go through Continuing to Preparing (or fail
at the inference limit). No inference occurs inside an unfinished exchange.

`SubmitApproval` checks the durable pending ID, digest, wall-clock deadline and
policy revision. A wrong digest rejects `approval_mismatch` without a commit;
a used or absent ticket rejects `NotActive`. Denial, expiry or invalidation
uses `approval_denied`, `approval_expired` or `approval_invalidated`. The timer
also enforces expiry. `ApprovalResolved` follows the decision's sync and clears
`RuntimeView.pending_approval`; resync exposes the full pending plan and expiry.

Remember on approved write/edit/bash publishes `PolicySnapshot::grant` using
`Allowance::remember` and a fresh revision. One commit orders authorization,
intent, configuration change and Executing. This consumes the old revision's
authorization before installing the grant. The sole storage exception (decision
16) permits a non-idle configuration change only after a same-commit allow
using a pending ticket, with a different revision. The core reloads policy on
acknowledgement; the next identical call uses the allowance without asking.
Reads ignore remember. Broken policy fails the turn `policy_invalid`, cancels
unstarted proposals without a model-facing policy error, and leaves the runtime
usable. Policy and instructions decode the same configuration bytes.

Every `StartInference.tools` carries enabled, available definitions in registry
order. These definitions are **outside** `ModelInput` and its fingerprint;
projection behavior, versions and goldens do not change. The async driver uses
`Runtime::with_tools` with the configured host's available backends, including
after reopen or in-place repair. The default host preserves no-tools behavior.

Every terminal path persists exactly one `turn_finished`, which storage applies
once. After its ack the core emits `TurnFinished` and resets the turn's
inference count.

## Commands

**StartTurn.** The command JSON is
`{"kind":"start_turn","content":[...]}`. The core fingerprints it as
SHA-256 of `session::store::normalized_command`. `bind_command` stores exactly
those bytes, so the two cannot drift. The order of checks is:

1. `Faulted` rejects.
2. A receipt with this command ID: the same fingerprint replies `Accepted` with
   the original ack and the turn ID from the receipt's `turn_started`. A different
   fingerprint replies `StaleCommand`. This also applies while recovery is
   required.
3. An opening commit or media verification in flight with this command ID: the same fingerprint attaches
   the request to that commit, and both requests are answered on its ack. A
   different fingerprint replies `StaleCommand`.
4. `RecoveryRequired` or `ResolutionRequired`, then `Busy` (an active turn or
   any commit in flight). Nothing is queued.
5. `Invalid`: `empty_content`, `unsupported_content`, `invalid_media`, `content_too_large`,
   `command_id_in_use` (the UUID already names another journal identity) or
   `head_inside_tool_exchange`.

The `TurnId` comes from `Env` and is never the command's UUID, because storage
registers both in one identity set. `receipt.commit` links them.

Text, image, audio and file blocks are accepted. Media references are checked
through `VerifyArtifacts { token, refs }` before C1 is built. `ArtifactsVerified`
with anything missing or invalid replies `Invalid("artifact_missing")` to every
attached retry and writes nothing; an empty missing list continues to C1.
Verification counts as a commit in flight for busy checks and deduplication.
Empty media MIME types or file names reject with `Invalid("invalid_media")`
before verification; these fields must satisfy the existing storage schema.
Reasoning, tool-call and provider-state blocks remain invalid user input.
Storage verifies media again on append. Artifacts are never deleted by the
harness; disagreement between preflight and append faults the runtime.

**SelectHead / SwitchModel** need an idle session (no active turn, nothing in
flight). Otherwise they get `Busy`, `RecoveryRequired`, `ResolutionRequired` or
`Faulted`. Each is one
commit, a `navigate` or a `model_changed` entry, and the reply follows its ack.
Invalid requests (`head_unchanged`, `unknown_head`,
`head_inside_tool_exchange`, `empty_model`) are rejected before persisting.
`model_changed` records the `reject` portability policy. Before switching, a
client can preview the switch with `inspect_context(None, Some(model))`: the
report lists provider state that would stop being portable and the budget
under the new model's limits. The command itself does not run the preview.

## Cancellation

`Cancel { turn_id }` for anything but the durable active turn replies
`NotActive`. Otherwise the request waits for a durable outcome:

- **During inference:** emit `CancelInference` and `SetTimer(cancel_grace)` at
  once, and persist `inferring>cancelling`. The driver cancels the stream's
  token and keeps polling it. The first of `Finished`, `ProviderEnded` or the
  timer ends the stream, and the core emits `ReleaseInference`. Then the core
  persists the partial response as `cancelled` with `turn_finished(cancelled)`,
  and cancels the timer if it is still armed. Deltas after `CancelInference`
  are dropped as stale and never reach the response. A provider that confirms
  ends the turn at once; one that ignores the token is dropped at the grace end.
- **During a commit in flight:** recorded and acted on after the ack, depending
  on the state that commit produced:
  - `inferring` before any stream: `inferring>cancelling` and
    `turn_finished(cancelled)`, with no `StartInference`.
  - `processing` with calls pending: `cancelled` runtime results for each call,
    `processing>cancelling` and `turn_finished(cancelled)`.
  - `preparing`: `preparing>cancelling` and `turn_finished(cancelled)`.
  - The turn already finished another way: the reply is `NotActive`.
- The reply is `Accepted::Cancelling` with the ack of the first commit after
  which the turn is durably `cancelling` or finished as cancelled.

- **While authorizing:** a pending approval is durably denied with reason
  `cancelled`, then that call and every later call receive `cancelled` results
  and the turn finishes cancelled. Without a ticket, cancel directly answers
  the unstarted calls; a late planning token cannot revive them.
- **While executing:** emit `CancelExecution`, commit Cancelling and wait for
  `ToolFinished`, including when its answer races the cancellation commit.
  Preserve the executor's actual outcome (`cancelled` or a winning `succeeded`)
  and cancel later calls. An intent already committed during cancellation is
  dispatched with an immediate cancellation request, never silently orphaned.

Cancellation never claims rollback.

## Provider stream

`harness_core::model` is the seam. `ModelProvider::stream(&self, ModelRequest,
OperationControl) -> ModelEventStream` returns an owned, pull-driven
`futures_core::Stream` of `ProviderEvent`s that starts work when polled. Setup
failures are terminal events. `OperationControl` carries a `CancellationToken`;
deadlines are deferred. `ModelInput` is the projection: the model and its
ordered `ContextItem`s (instructions, user, assistant, tool result, summary).

`model::scripted::ScriptedProvider` replays one `Script` per stream. Its steps
are `Event`, `Wait` (until a test signals), `AwaitCancel` and `Stall` (until
dropped); the end of the script is EOF. Events are yielded as written, even
invalid ones. It reads no clock and reports `provider_failure`/`no_script` when
no script is left. `streams()` records each request, the events yielded, and
whether the stream was cancelled and dropped.

- An idle timer of `provider_idle_timeout` is armed with `StartInference` and
  re-armed after every event that leaves the stream open. A cancellation
  replaces it with the grace timer. On expiry the stream is released, the
  response is recorded `interrupted`, and the turn fails with
  `provider_stalled`.
- The core emits `ReleaseInference` exactly once per started stream, when it
  stops listening: at any end, at grace expiry, for a malformed or oversized
  stream, on idle timeout and on fault. Malformed and oversized streams are released at once,
  without a cancellation to confirm.
- Events for a stream that is not current, or that is cancelled or already
  ended, are dropped. Each drop emits a `StaleProviderEvent` diagnostic with
  the runtime's running count. A stale timer emits `StaleTimer`.
- Deltas accumulate into the partial response. `UsageUpdated` replaces usage.
- `max_partial_bytes` bounds the JSON-escaped size of text, reasoning summaries,
  tool names and provider call IDs, plus raw argument fragments. Each new block
  costs 160 bytes. At `ToolCallReady` the core also charges any growth from
  reserializing the arguments. A delta that would exceed the bound is not
  applied: the core cancels the stream and records it as `interrupted`.
- Protocol violations cancel the stream, emit a `MalformedStream` diagnostic,
  and record the response as `failed/malformed_stream`. Violations include a
  duplicate `Started`, content before `Started`, mismatched or duplicate blocks,
  empty tool names, arguments for an unknown or ready call, duplicate or invalid
  ready, and usage above 2^53−1. Invalid ready covers fragments that are not one
  JSON object, or that repeat a key.
- `Finished` must be consistent:
  - `completed` needs a finish reason and no error.
  - `tool_calls` needs at least one call, every one ready.
  - `stop` and `refusal` must have no calls.
  - `failed` needs a valid error and no finish reason.
  - `cancelled` has neither.
  - `completed` before `Started` is malformed.
  Anything else is malformed.
- Only `completed/tool_calls` with every call ready persists `ToolCall` blocks.
  Other responses keep their text and reasoning summaries and drop call blocks.
  Nothing incomplete is ever answered as a call.

## Faults

`PersistFailed`, `StorageFailed` or `DriverInvariant`, an answer for a token
not in flight, or an acknowledgement that does not extend the snapshot moves
the core to `Faulted`. `DriverInvariant { code }` faults with
`FaultCause::Internal(code)`.
It then cancels the open stream and armed timer, clears the partial response,
emits `Fault`, and rejects pending requests and every later command with
`Faulted`. It never retries. Other inputs then produce no effects. Only a new
`Runtime` over a reopened store leaves `Faulted`. A `Rejected` answer is a core
bug; tests fail on it.

A snapshot that requires recovery, or is not `idle`, starts the core in
`RecoveryRequired`; `Input::Start` then recovers it, as described next.

## Recovery

After a crash or a fault, a session returns to `Idle` on its own when that is
safe, and waits for the user when it is not. It never replays a side effect or
invents history: no stream opens, no assistant entry is written and no result
claims `executor` provenance during recovery. Opening still writes nothing;
recovery is ordinary commits that storage validates. See
[decision 0007](decisions/0007-startup-recovery.md).

`Runtime::new` emits nothing. The driver feeds `Input::Start` right after
constructing a runtime on open, create, reopen and repair, before it serves any
message. A runtime with nothing to recover emits nothing for it. Otherwise the
core plans one recovery step, and each recovery commit's acknowledgement plans
the next, until the session is `Idle` or waits for the user. While a step is
planned or in flight the phase is `RecoveryRequired` and commands get
`RecoveryRequired`, except that a `StartTurn` retry still learns its durable
receipt. A failed recovery commit faults the runtime as usual; after
`reopen()`, `Start` plans again from the new snapshot. Commits are atomic, so
a half-applied step is impossible and nothing is written twice.

**An interrupted turn with nothing uncertain** closes in one commit:

| Entries | Events |
| --- | --- |
| `cancelled` runtime result per pending call without an intent, text `process_restart: …` | `<state>>recovering` unless already `recovering`, `turn_finished(interrupted, process_restart)` |

After its ack the core emits `TurnFinished` and `Recovered { turn_id,
answered, code }`, where `answered` lists those calls, and the session is
`Idle`. A partial response that was streaming at the crash was never
persisted, so nothing is written for it.

**D9 read-only closure:** an unresolved intent with effects exactly `{read}`
and no recorded `execution_uncertain` closes automatically. First commit the
transition to Recovering (entries are validated before their own events), then
commit its runtime result with invocation ID, status `cancelled` and code
`process_restart`, along with results for later not-started calls and the
interrupted finish. No uncertainty or resolution event is written; the read is
never replayed. `effects: None` (v1) and already uncertain reads do not qualify.
Storage permits only one unresolved intent (D30), so a read following an
uncertain write is a not-started **call**, not another intent. It receives a
runtime `cancelled/process_restart` result without invocation ID only after
the user resolves the write. See [ADR 0013](decisions/0013-tool-recovery.md).

**An uncertain invocation**, every other execution intent without a result, may or may
not have run. `Start` commits `execution_uncertain` (reason
`process_restart: …`) for each one not yet recorded; storage moves to
`recovering`. If no not-started call follows on the branch, the same commit
finishes the turn `interrupted` and `Recovered` follows its ack. Otherwise the
turn stays open: storage answers calls in branch order, and the uncertain call
is first. The core then enters `AwaitingResolution`. `RuntimeView::awaiting`
and a `ResolutionRequired { invocations }` event list each one as an
`UncertainInvocation { invocation_id, turn_id, call, tool, plan_digest,
intent_at, hint: Option<RecoveryHint> }`. Every command is rejected with `ResolutionRequired` except a
`StartTurn` retry with a receipt and:

`ResolveUncertain { invocation_id, resolution }`, where `resolution` is

- `Observed { status, text }`: the user checked what happened. It becomes a
  tool result with `user_observed` provenance and that status. `uncertain` is
  refused (`observed_status_uncertain`), and `text` must be non-empty and at
  most `max_user_bytes` (`empty_content`, `content_too_large`).
- `AcknowledgedUnknown`: a result with status `uncertain` and
  `user_acknowledged_unknown` provenance.

The result and `recovery_resolved` are one commit, and the reply
`Accepted::Resolved` follows its ack. When it resolves the last uncertain
invocation of a still-open turn, the same commit also answers the remaining
not-started calls as above and finishes the turn `interrupted`; storage
accepts the four records together. An unknown ID is `Invalid("unknown_invocation")`;
outside `AwaitingResolution` the command is `Invalid("nothing_to_resolve")`,
`RecoveryRequired` or `Faulted`, and a resolution already in flight makes a
second one `Busy`. No uncertain invocation is resolved automatically and the
tool is never run again; the user starts a new turn afterwards.

**Advisory hints:** on entering AwaitingResolution the core requests inspection
of uncertain `write`/`edit` authorized plan artifacts, never current proposals.
The storage worker reads verified bytes bounded by `max_artifact_bytes`; the
executor thread computes `RecoveryHint`. A write compares the current digest
with post-image first (including no-op writes), then pre-image/absence. An edit
checks pre-image first, then reverse-applies a unique overlapping-aware
`new_text` occurrence to compare its SHA-256 with the pre-image. Reads of edit
text are bounded to 1 MiB. Evidence is `matches_post_image`, `matches_pre_image`,
`changed_since`, `missing`, or `unavailable { reason }` (refusal code, or
`tools_unavailable`, `artifact_unavailable`, `infrastructure_failure`). No hint
is requested for `bash`. Inspection panic is advice unavailable, not a fault.

`Input::Hints` produces nonpersisted `RecoveryHints`, applied by ClientView to
`awaiting[].hint` so resync preserves it. Hints never enter journal facts or
model context. Tokens and epochs reject stale answers; resolution works before
hints arrive, invalidates their token, and drops any late answer. Artifact loads
have their own future, not the exclusive persistence slot; inspection shares
the one-job executor queue. Neither blocks recovery commits or user resolution.

The commits storage accepted for each shape:

| Shape | Commits after `Start` |
| --- | --- |
| Unfinished turn | `<state>>recovering finished(interrupted, process_restart)` |
| Unanswered calls | `result(cancelled)… processing>recovering finished(…)` |
| D9 read intent, then not-started calls | `<state>>recovering`; `result(cancelled, invocation)… result(cancelled)… finished(…)` |
| Uncertain call alone | `execution_uncertain finished(…)`; on resolution `result(<status>) recovery_resolved` |
| Uncertain, then not-started | `execution_uncertain`; on resolution `result(<status>) result(cancelled) recovery_resolved finished(…)` |

A torn tail is repaired only on request, before any of this; see
[repair](#tail-repair).

## Events and resync

`RuntimeEvent { epoch, seq, kind }`. `epoch: RuntimeEpoch` is the UUIDv7
incarnation generated through `Env` when constructing a runtime. `seq` starts at 1 for each `Runtime`, increases by
exactly one per event, and is unrelated to journal sequences. Kinds:

- `Committed { ack, snapshot }`: one per `Persisted`, with the new snapshot.
- `ContextBuilt { turn_id, head, fingerprint, totals, flags }`: a turn's
  context was built in `Preparing`. It carries the estimate totals and the
  `overflow`, `compaction_recommended` and `preview` flags, not the report, and
  changes no view. Clients that want the report call `inspect_context`.
- `PhaseChanged { phase, active_turn }`: `Phase` is `Durable(RuntimeState)`,
  `RecoveryRequired`, `AwaitingResolution` or `Faulted`. A phase other than
  `AwaitingResolution` clears the view's `awaiting`.
- `TurnFinished { turn_id, outcome, code }`: after the commit holding it.
- `Recovered { turn_id, answered, code }`: recovery closed this turn, after
  its `TurnFinished`.
- `ResolutionRequired { invocations }`: the invocations waiting for the user;
  it replaces the view's `awaiting`.
- `RecoveryHints { hints }`: ephemeral evidence for invocations still awaiting
  resolution; updates only matching `awaiting[].hint` fields.
- `InferenceStarted`, `TextDelta`, `ReasoningSummaryDelta`, `ToolCallStarted`
  (with the issued `CallId`), `ToolArgumentsDelta`, `ToolCallReady`,
  `UsageUpdated`, and `PartialCleared`. `PartialCleared` comes when the response
  is durable or abandoned by a fault.
- `ApprovalRequested { approval_id, call, plan_digest, plan, expires_at }`,
  `ApprovalResolved { approval_id, outcome }`, `ToolStarted { call, invocation_id }`
  and `ToolFinished { call, status, code }`. Pending approval state uses the same
  client reducer, so resync cannot lose a ticket. Started and finished tool
  events follow intent/result durability, respectively.
- `Diagnostic` and `Fault`.

`Runtime::view()` returns `RuntimeView { epoch, event_seq, snapshot, phase,
active_turn, partial, awaiting, pending_approval }`. It reflects exactly the events with
`seq <= event_seq`. The runtime maintains its own view by applying each event
it emits to a `ClientView`, the same reducer clients use, so the invariant holds
by construction and is also tested. `partial` is bounded like the response.

`ClientView::from(view)` and `apply(&event)` return `Applied`, `Duplicate` for
`seq <= event_seq`, or `NeedsResync` for a gap or different epoch, which leaves
the view unchanged. Epoch is checked before sequence, so a new runtime's seq 1
cannot be dropped as an old event.
Views compare snapshots by session, sequence and record checksum.

## Async handle and worker

`SessionHandle::open(root, session_id, provider, DriverConfig)` opens the
journal on its worker and spawns a driver task on the caller's Tokio runtime.
`SessionHandle::create(root, session_id, header, provider, config)` creates and
syncs a new session instead. `provider` is an `Arc<dyn ModelProvider>` that
streams every inference of the session. `root` is an `Arc<DataRoot>` prepared by the caller outside
async execution. Header configuration artifacts must already be published.
Cloning a handle is cheap. Dropping every handle also initiates shutdown;
subscriptions hold only weak command senders and do not keep the driver alive.

- `command(Command).await -> Result<Accepted, Rejection>` uses `try_send` on
  the bounded command mailbox. Full returns `Overloaded` at once. A second turn
  still gets the core's `Busy`; turns are never queued for later execution.
- `view().await` returns a coherent view taken between steps.
- `inspect_context(head, model_override).await -> Result<ContextReport,
  InspectError>` builds a report between steps from the published snapshot.
  `head: None` is the current head; any entry can be named, so another branch
  can be inspected without navigating to it. `model_override` makes it a
  preview. Nothing is persisted and the head never moves. An unknown head is
  `InspectError::Context(ContextError::UnknownHead)`; a stopped driver is
  `InspectError::Closed`. It is answered while draining for shutdown, like views.
- `subscribe()` immediately registers a broadcast receiver. `next().await`
  yields `Some(SubscriptionItem::Reset(ClientView))` first, then contiguous
  `Event(RuntimeEvent)` items, or `None` after driver shutdown.
- `put_artifact(Vec<u8>).await -> Result<ArtifactRef, PersistError>` rejects
  payloads above `max_artifact_bytes` before any I/O. The worker publishes the
  bytes and replies after artifact file and directory sync. Artifact I/O failure
  faults the runtime even without a pending append.
- `shutdown().await` asks the core to cancel any active turn, waits for its
  timer/terminal commits and outstanding tool cleanup, drains artifacts, closes
  the executor queue and joins its thread through `spawn_blocking`, then drops
  storage worker senders and awaits worker exit. The lock is released before it
  returns, including for simultaneous shutdown callers. Immediate open succeeds.
  During the drain the mailbox is still read: commands get
  `Rejection::ShuttingDown`, uploads and reopen get
  `Rejected { code: "shutting_down" }`, and views are still answered. Messages
  queued after the loop ends are answered the same way once the worker exits.
  After the driver has exited, `command` reports `Faulted` because the mailbox
  is closed.
- `reopen().await -> Result<RuntimeView, PersistError>` requires `Faulted`.
  It awaits outstanding planning/execution asynchronously, then drains and stops
  the old storage worker, opens the same journal on a new one,
  constructs a new runtime/epoch, feeds `PublishView` and then `Start`, which
  recovers the session as on open. A torn tail is returned as
  `PersistError::TornTail` without repair. No command or uncertain write is
  automatically retried.
- `open_with_env` and `create_with_env` take an `Env` for time and
  identities, so a handle can run deterministically; `open` and `create` use
  `SystemEnv`.

### Tail repair

A torn tail is the end of a record that was never fully written. Cutting it
off is safe only for the exact file the user looked at, so repair always takes
the inspected fingerprint and size and never runs on its own.

1. `SessionHandle::open` or `reopen()` fails with `PersistError::TornTail`.
   Its `TornTail` carries the full `Inspection`: valid bytes, file bytes,
   whole-file fingerprint and the valid prefix's snapshot. `tail_bytes()` is
   what would be quarantined.
2. The client shows the user that byte count and where the valid prefix ends
   (its sequence, head, state and any turn in progress).
3. On confirmation it calls `SessionHandle::repair_tail(root, session_id,
   expected_fingerprint, expected_size, provider, config)`.
4. On the worker thread, `JournalStore::repair_inspected_tail` rescans under
   the writer lock, checks the fingerprint and size, quarantines the tail into
   an artifact, truncates and syncs, and commits `tail_repaired`.
5. The session opens normally and `Start` runs any recovery.

A faulted handle does steps 3 to 5 in place with
`reopen_repairing_tail(expected_fingerprint, expected_size)`, getting a new
epoch; existing subscriptions reset. `RepairError` is `StaleRepair` (the file
changed since inspection), `NotRepairable` (a complete corrupt record, a
missing header, or no torn tail), `Locked` (another engine holds the session),
`NotFaulted` (the in-place form on a healthy handle) or `Persist(error)`.
Nothing is ever skipped or rewritten. `harness journal inspect` and
`harness journal repair` do the same without an engine; see
[session storage](session-storage.md#durability-and-recovery).

`DriverConfig.tools` is a `ToolHost { workspace: Arc<dyn WorkspaceView + Send +
Sync>, executors: Vec<Arc<dyn Executor>> }`. Its default, `ToolHost::none()`,
installs no tools and starts no executor thread. Applications explicitly supply
a trusted workspace view and the executors they support. Duplicate backend
executors make open/create fail with `duplicate_executor_backend` before any
journal I/O. Debug output lists backends only, not workspace data.

With at least one executor, a single named `harness-tools` OS thread runs both
workspace planning and execution. Standard-channel jobs carry epoch, token and
immutable inputs; planning takes a clone of `Runtime::policy()`. Tokio-channel
answers are selected by the driver, so neither hashing nor execution blocks
views, subscriptions or commands. Only one job may be outstanding; overlap
faults `executor_overlap`. An answer from another epoch is dropped silently.
Each execution gets a fresh `tools::CancelToken`, retained by execution token in
the driver; cancellation sets it directly, not via the busy thread's queue.

Each job runs under `catch_unwind`. A planning panic is a nonexecuted
`infrastructure_failure` refusal. Execution panic, a missing matching executor
or thread loss faults `executor_lost` without inventing a `ToolFinished` input:
the intent has no result, so reopen reports it uncertain and never replays it.
A fault may leave cancellation/cleanup outstanding. Reopen and repair wait for
that answer before restarting storage; shutdown waits before joining the thread
and stopping storage. Executors must honor their deadlines to bound these waits.

`DriverConfig` defaults: 16 command slots, 64 broadcast slots, 8 worker slots
(and at most 8 concurrent artifact tasks), 16 MiB artifact uploads, and the
existing `RuntimeConfig` limits. Channel capacities of zero are clamped to one.
Artifact task saturation returns `PersistError::Rejected { code: "overloaded" }`.
Reopen outside `Faulted` returns `Rejected { code: "not_faulted" }`.

The driver uses biased `select!`, in this order: persistence and preflight
replies, worker exit, executor answers, timers, artifact failures, commands, and provider events
last. Each provider event spends Tokio's cooperative budget, so even a stream
that is always ready makes the task yield, and commands such as `Cancel` are
served between events. For `StartInference` it calls
`ModelProvider::stream` with a `ModelRequest { response_id, input }` and a
fresh cancellation token, and polls the returned stream one
event at a time; nothing is queued between the stream and the core.
`ModelRequest.input` is exactly the projection the core built.
`CancelInference` cancels the token, and `ReleaseInference` drops the stream.
Every drop also cancels the token. It maps effects to worker requests, provider
receivers, sleep tasks and broadcast events; it makes no turn decisions.
Timers use Tokio deadlines and keep abortable task handles by `TimerId`.
Each fire carries the epoch of the runtime that set it. A new runtime restarts
`TimerId`s, so the driver drops fires from another epoch instead of letting a
stale fire end a new runtime's grace period; reopen also aborts leftover timers.

The driver holds one storage slot for `Persist` and `VerifyArtifacts`. The core
never has two outstanding, but the driver checks the slot in every build. If an
effect would replace an occupied slot, the driver keeps the existing operation,
which may already be written, skips the new one and feeds
`DriverInvariant { code: "storage_overlap" }` to the core.

`StorageWorker::open` and `create` open on a named `std::thread`. After opening
the journal, the worker reads and decodes the session's configuration artifact
for the context builder; an artifact that does not decode fails the open with
its code, such as `invalid_instructions`. Only that
thread calls `JournalStore`; it owns the store until drop. Requests arrive by
bounded mpsc and `blocking_recv`, with oneshot replies after sync. No
`spawn_blocking` or Tokio runtime is created. I/O errors poison later appends,
which still receive `Poisoned`; worker panics and dropped replies become
`WorkerLost`. Closing all senders drains queued work, drops the store, releases
the lock and signals exit. Failed opens also await exit before returning.

## Subscription protocol

1. Subscribe to broadcast before asking the driver for its view.
2. Ask for a view between steps and yield `Reset(ClientView::from(view))`.
3. Ignore buffered events already reflected by that epoch and sequence.
4. Apply contiguous events. On broadcast `Lagged`, a different epoch or a gap,
   resubscribe **before** asking for another view and repeat from step 2.

`Subscription` runs this protocol internally using the shared reducer. The
partial response is in each reset, so joining or resyncing mid-stream retains
all text observed so far. Delta history itself is transient; durable session
history and the current partial response are preserved through resync.
Reopen and in-place repair feed `PublishView` to the new core, emitting its
phase with the new epoch, and then `Start`, so subscribers see the recovery
events that follow.

## Not implemented yet

- Real sandbox executors, CLI tool-host wiring and native providers. The
  library's trusted filesystem executor is available but not installed by CLI.
- General configuration-update commands, summary generation and compaction.
  Remember is the implemented configuration-change path.
- Automatic reconciliation of uncertain side effects. Advisory file-image
  hints are implemented; resolution remains the user's statement.
- Recovery UI beyond the handle, events and CLI (Milestone 5), and retrying a
  turn after recovery: the user starts a new turn.

## Tests

`runtime::tests` drives the core synchronously against a real `JournalStore` in
a temporary directory, with the oracle on. Every scenario runs twice with fresh
environments and is compared with storage-chosen bytes abstracted. It is also
replayed from its recorded inputs and must give byte-identical effects. Each run
checks resync from every recorded view, and checks that reopening the journal
gives the published snapshot. The async driver's tests use a current-thread
Tokio runtime with paused time, real channels and a dedicated worker. Only
explicit virtual-time advances fire timers; I/O gates hold durability boundaries
without real sleeps. They cover lag, mid-stream joins, epoch replacement,
shutdown and its drain rejections, uncertain writes, panic, overload, media,
an injected overlapping storage effect, and a stale timer fire after reopen.
Tool-host tests use the fake workspace, scripted provider and per-invocation
counted trusted-fs/bubblewrap stubs. They cover approval/remember/denial/expiry,
durable ordered events and late subscriptions, planning/execution nonblocking,
cancellation races, planning panic, execution loss and uncertainty, stale epochs,
tools after reopen, and executor cleanup before reopen/shutdown.
`ScriptedProvider` scripts cover confirmed cancellation without virtual time,
a stalled stream released at grace end, setup and mid-stream failure,
interruption, a malformed stream released at once, a cancel that preempts
an always-ready stream, and an idle timeout that restarts on each event.
Context scenarios check that `ContextBuilt` precedes C2 and `StartInference`
follows its ack with the same fingerprint, that overflow fails the first or a
later inference from `Preparing` with no stream, and that every projection is
rebuilt identically after reopening. Driver tests check that a scripted
provider is never opened on overflow, that requests carry the configured
instructions, and that `inspect_context` reads a sibling branch without moving
the head.

Recovery scenarios (`runtime::tests::recovery`) kill the process after a
chosen commit, without shutdown, and restart it: in `Preparing`, `Inferring`,
`Processing`, `Authorizing`, `Continuing` and `Cancelling`, with unanswered
calls, with an uncertain call alone or followed by a not-started call, during
recovery itself (the commit lost, durable but unacknowledged, or torn), while
waiting for a resolution, and during a resolution. Records a tool runtime would
write (authorization and intent) are appended through the dying process's
writer. The interrupted-execution fixture waits in `AwaitingResolution` and
returns to `Idle` when resolved either way. While recovering, the test driver
fails if the core starts a stream or persists an assistant entry or an
executor result, and every run ends with every started turn finished exactly
once; `finish` restarts any run left faulted or unfinished. Driver tests cover
recovery through `SessionHandle::open` with a scripted provider that records
no stream, resolution through the handle, repair on a faulted handle (stale
fingerprint and size refused, new epoch, subscription reset) and the static
`repair_tail` refusals. A randomized test runs 512 seeds, with crashes. See
[development](development.md#runtime-tests).
