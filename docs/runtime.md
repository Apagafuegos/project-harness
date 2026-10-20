# Runtime core

Status: Milestone 2, steps 2 to 4 are implemented as `harness_core::runtime`
and `harness_core::model`: a pure synchronous state machine, a dedicated storage
worker, a Tokio driver/handle with broadcast subscriptions, and the provider
stream seam with a scripted mock. Context building and startup recovery remain
future work. Decisions [0004](decisions/0004-pure-runtime-core.md) and
[0005](decisions/0005-provider-stream.md) record the design.

## Shape

```rust,ignore
let mut runtime = Runtime::new(snapshot, RuntimeConfig::default(), &mut env); // emits nothing
let effects: Vec<Effect> = runtime.step(input, &mut env);
```

`step` does nothing except what it returns. It performs no file, network or
channel access, never reads the clock, and creates identities only through
`Env`: `now()` plus generators for runtime epochs, entry, turn, response and
call IDs.
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
| `Provider` | `InferenceId`, `ProviderEvent` | Provider stream |
| `ProviderEnded` | `InferenceId` | Stream closed, with or without `Finished` |
| `TimerFired` | `TimerId` | Driver timer |

Commands are `StartTurn { command_id, content }`, `Cancel { turn_id }`,
`SelectHead { head }` and `SwitchModel { model }`. `RequestId`, `CommitToken`,
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
| `Persist` | `CommitToken`, `Commit`, optional `(CommandId, Value)`, `at` | Bind the command if present, append, answer |
| `StartInference` | `InferenceId`, `TurnId`, head, `ModelRef`, `ResponseId` | Open a stream; context projection arrives in step 5 |
| `CancelInference` | `InferenceId` | Cancel the stream's token; keep delivering it for a confirmation |
| `ReleaseInference` | `InferenceId` | Stop listening and drop the stream; once per `StartInference` |
| `SetTimer` / `CancelTimer` | `TimerId`, duration | Driver timer |
| `Emit` | `RuntimeEvent` | Broadcast |
| `Reply` | `RequestId`, `Result<Accepted, Rejection>` | Exactly one per `Command` |

`Persist.at` is the record timestamp from `Env::now`, because
`JournalStore::append` takes one. `StartInference.response_id` is the identity
the assistant entry will carry, which RFC-001 gives the provider request.

`Accepted` is `TurnStarted { turn_id, ack }`, `Cancelling { turn_id, ack }`,
`HeadSelected { ack }` or `ModelSwitched { ack }`. `Rejection` is `Busy`,
`RecoveryRequired`, `Faulted`, `NotActive`, `StaleCommand` or `Invalid(code)`.
The handle can also return `Overloaded` before submission, and the driver
returns `ShuttingDown` for commands it receives while draining; neither reaches
the core.

`RuntimeConfig` has `max_inferences_per_turn` (32), `max_partial_bytes`
(4 MiB), `max_user_bytes` (1 MiB) and `cancel_grace` (2 s).

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
| C3 stop | assistant `completed/stop` | `inferring>processing`, `turn_finished(completed)` |
| C3 tool calls | assistant `completed/tool_calls` | `inferring>processing` |
| C4 answers | one runtime result per pending call | `processing>continuing`, `continuing>preparing`, then C2 again |
| C4 at limit | one runtime result per pending call | `processing>continuing`, `turn_finished(failed, inference_limit)` |

Other stream endings record the response and finish the turn in one commit:

| Stream ending | Assistant entry | Events |
| --- | --- | --- |
| `completed/refusal` | `completed/refusal` | `inferring>processing`, `turn_finished(completed)` |
| `completed/output_limit` | `completed/output_limit`, incomplete calls dropped | `inferring>processing`, `turn_finished(failed, output_limit)` |
| `failed` with a valid error | `failed`, that error | `turn_finished(failed, <error code>)` |
| `cancelled` by the provider | `cancelled` | `turn_finished(cancelled, provider_cancelled)` |
| EOF without `Finished` | `interrupted` | `turn_finished(failed, provider_eof)` |
| over `max_partial_bytes` | `interrupted` | `turn_finished(failed, response_too_large)` |
| protocol violation | `failed`, `malformed_stream` error | `turn_finished(failed, malformed_stream)` |

Storage determines the tool-answer sequence:

- Tools are unavailable in Milestone 2. Each pending call gets a `ToolResult`
  with status `denied`, no invocation, `runtime` provenance and the text
  `tools_unavailable: …`. Storage has no result code field, so the code leads the
  text. The proposal is durable in C3 before any result. C4 uses storage's
  `processing>continuing` path for calls that were rejected without
  authorization, then `continuing>preparing`.

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
4. `RecoveryRequired`, then `Busy` (an active turn or any commit in flight).
   Nothing is queued.
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
flight). Otherwise they get `Busy`, `RecoveryRequired` or `Faulted`. Each is one
commit, a `navigate` or a `model_changed` entry, and the reply follows its ack.
Invalid requests (`head_unchanged`, `unknown_head`,
`head_inside_tool_exchange`, `empty_model`) are rejected before persisting.
`model_changed` records the `reject` portability policy; the portability preview
arrives with the context builder.

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

Cancellation never claims rollback.

## Provider stream

`harness_core::model` is the seam. `ModelProvider::stream(&self, ModelRequest,
OperationControl) -> ModelEventStream` returns an owned, pull-driven
`futures_core::Stream` of `ProviderEvent`s that starts work when polled. Setup
failures are terminal events. `OperationControl` carries a `CancellationToken`;
deadlines are deferred. `ModelInput` names only the model until step 5.

`model::scripted::ScriptedProvider` replays one `Script` per stream. Its steps
are `Event`, `Wait` (until a test signals), `AwaitCancel` and `Stall` (until
dropped); the end of the script is EOF. Events are yielded as written, even
invalid ones. It reads no clock and reports `provider_failure`/`no_script` when
no script is left. `streams()` records each request, the events yielded, and
whether the stream was cancelled and dropped.

- The core emits `ReleaseInference` exactly once per started stream, when it
  stops listening: at any end, at grace expiry, for a malformed or oversized
  stream, and on fault. Malformed and oversized streams are released at once,
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
`RecoveryRequired`. Every command is rejected, except that a `StartTurn` retry
still learns its durable receipt.

## Events and resync

`RuntimeEvent { epoch, seq, kind }`. `epoch: RuntimeEpoch` is the UUIDv7
incarnation generated through `Env` when constructing a runtime. `seq` starts at 1 for each `Runtime`, increases by
exactly one per event, and is unrelated to journal sequences. Kinds:

- `Committed { ack, snapshot }`: one per `Persisted`, with the new snapshot.
- `PhaseChanged { phase, active_turn }`: `Phase` is `Durable(RuntimeState)`,
  `RecoveryRequired` or `Faulted`.
- `TurnFinished { turn_id, outcome, code }`: after the commit holding it.
- `InferenceStarted`, `TextDelta`, `ReasoningSummaryDelta`, `ToolCallStarted`
  (with the issued `CallId`), `ToolArgumentsDelta`, `ToolCallReady`,
  `UsageUpdated`, and `PartialCleared`. `PartialCleared` comes when the response
  is durable or abandoned by a fault.
- `Diagnostic` and `Fault`.

`Runtime::view()` returns `RuntimeView { epoch, event_seq, snapshot, phase,
active_turn, partial }`. It reflects exactly the events with
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
- `subscribe()` immediately registers a broadcast receiver. `next().await`
  yields `Some(SubscriptionItem::Reset(ClientView))` first, then contiguous
  `Event(RuntimeEvent)` items, or `None` after driver shutdown.
- `put_artifact(Vec<u8>).await -> Result<ArtifactRef, PersistError>` rejects
  payloads above `max_artifact_bytes` before any I/O. The worker publishes the
  bytes and replies after artifact file and directory sync. Artifact I/O failure
  faults the runtime even without a pending append.
- `shutdown().await` asks the core to cancel any active inference, waits for its
  timer/terminal commits and all in-flight storage work, drains artifacts, drops
  worker senders, and awaits worker exit. The lock is released before it
  returns, including for simultaneous shutdown callers. Immediate open succeeds.
  During the drain the mailbox is still read: commands get
  `Rejection::ShuttingDown`, uploads and reopen get
  `Rejected { code: "shutting_down" }`, and views are still answered. Messages
  queued after the loop ends are answered the same way once the worker exits.
  After the driver has exited, `command` reports `Faulted` because the mailbox
  is closed.
- `reopen().await -> Result<RuntimeView, PersistError>` requires `Faulted`.
  It drains and stops the old worker, opens the same journal on a new one, and
  constructs a new runtime/epoch. Unfinished work stays `RecoveryRequired`.
  Torn-tail errors are returned without repair; explicit storage repair remains
  required. No command or uncertain write is automatically retried.

`DriverConfig` defaults: 16 command slots, 64 broadcast slots, 8 worker slots
(and at most 8 concurrent artifact tasks), 16 MiB artifact uploads, and the
existing `RuntimeConfig` limits. Channel capacities of zero are clamped to one.
Artifact task saturation returns `PersistError::Rejected { code: "overloaded" }`.
Reopen outside `Faulted` returns `Rejected { code: "not_faulted" }`.

The driver uses biased `select!`: persistence/preflight replies first, provider
events next, timers next, commands last. For `StartInference` it calls
`ModelProvider::stream` with a `ModelRequest { response_id, input: ModelInput
{ model } }` and a fresh cancellation token, and polls the returned stream one
event at a time; nothing is queued between the stream and the core.
`CancelInference` cancels the token, and `ReleaseInference` drops the stream.
Every drop also cancels the token. Worker exit and artifact failure are
also observed before commands. It maps effects to worker requests, provider
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

`StorageWorker::open` and `create` open on a named `std::thread`. Only that
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
Reopen feeds `PublishView` to the new core, emitting its phase with the new
epoch even when recovery prevents a new command.

## Not implemented yet

- **Step 5:** context projection and its report. `StartInference` names the head
  and model only.
- **Step 6:** startup recovery. `RecoveryRequired` only rejects commands today.
  Explicit torn-tail repair through the handle also belongs here.
- Also later: approvals, compaction, configuration changes, real tools and
  providers.

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
`ScriptedProvider` scripts cover confirmed cancellation without virtual time,
a stalled stream released at grace end, setup and mid-stream failure,
interruption and a malformed stream released at once. A randomized test runs 512 seeds. See
[development](development.md#runtime-tests).
