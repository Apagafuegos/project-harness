# Runtime core

Status: Milestone 2, step 2 is implemented as `harness_core::runtime`: a pure,
synchronous state machine tested against real storage. There is no driver,
provider, context builder or recovery yet, and nothing here runs an agent end
to end. [Decision 0004](decisions/0004-pure-runtime-core.md) records the design.

## Shape

```rust,ignore
let mut runtime = Runtime::new(snapshot, RuntimeConfig::default()); // emits nothing
let effects: Vec<Effect> = runtime.step(input, &mut env);
```

`step` does nothing except what it returns. It performs no file, network or
channel access, never reads the clock, and creates identities only through
`Env`: `now()` plus generators for entry, turn, response and call IDs.
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
| `PersistFailed` | `CommitToken`, `PersistError::{Rejected{code}, Io{message}}` | Storage worker |
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
| `Persist` | `CommitToken`, `Commit`, optional `(CommandId, Value)`, `at` | Bind the command if present, append, answer |
| `StartInference` | `InferenceId`, `TurnId`, head, `ModelRef`, `ResponseId` | Open a stream; context projection arrives in step 5 |
| `CancelInference` | `InferenceId` | Stop the stream; its later events are dropped |
| `SetTimer` / `CancelTimer` | `TimerId`, duration | Driver timer |
| `Emit` | `RuntimeEvent` | Broadcast |
| `Reply` | `RequestId`, `Result<Accepted, Rejection>` | Exactly one per `Command` |

`Persist.at` is the record timestamp from `Env::now`, because
`JournalStore::append` takes one. `StartInference.response_id` is the identity
the assistant entry will carry, which RFC-001 gives the provider request.

`Accepted` is `TurnStarted { turn_id, ack }`, `Cancelling { turn_id, ack }`,
`HeadSelected { ack }` or `ModelSwitched { ack }`. `Rejection` is `Busy`,
`RecoveryRequired`, `Faulted`, `NotActive`, `StaleCommand` or `Invalid(code)`.

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

Storage determined two of these sequences:

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
3. An opening commit in flight with this command ID: the same fingerprint attaches
   the request to that commit, and both requests are answered on its ack. A
   different fingerprint replies `StaleCommand`.
4. `RecoveryRequired`, then `Busy` (an active turn or any commit in flight).
   Nothing is queued.
5. `Invalid`: `empty_content`, `unsupported_content`, `content_too_large`,
   `command_id_in_use` (the UUID already names another journal identity) or
   `head_inside_tool_exchange`.

The `TurnId` comes from `Env` and is never the command's UUID, because storage
registers both in one identity set. `receipt.commit` links them.

Milestone 2 accepts text blocks only. Media blocks reference artifacts the pure
core cannot verify, and a missing artifact would make storage refuse the commit
and fault the session.

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
  once, and persist `inferring>cancelling`. The first of `Finished`,
  `ProviderEnded` or the timer ends the stream. Then the core persists the
  partial response as `cancelled` with `turn_finished(cancelled)`, and cancels
  the timer if it is still armed. Deltas after `CancelInference` are dropped as
  stale and never reach the response.
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

`PersistFailed` of either kind, an answer for a token not in flight, or an
acknowledgement that does not extend the snapshot moves the core to `Faulted`.
It then cancels the open stream and armed timer, clears the partial response,
emits `Fault`, and rejects pending requests and every later command with
`Faulted`. It never retries. Other inputs then produce no effects. Only a new
`Runtime` over a reopened store leaves `Faulted`. A `Rejected` answer is a core
bug; tests fail on it.

A snapshot that requires recovery, or is not `idle`, starts the core in
`RecoveryRequired`. Every command is rejected, except that a `StartTurn` retry
still learns its durable receipt.

## Events and resync

`RuntimeEvent { seq, kind }`. `seq` starts at 1 for each `Runtime`, increases by
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

`Runtime::view()` returns `RuntimeView { event_seq, snapshot, phase,
active_turn, partial }`. It reflects exactly the events with
`seq <= event_seq`. The runtime maintains its own view by applying each event
it emits to a `ClientView`, the same reducer clients use, so the invariant holds
by construction and is also tested. `partial` is bounded like the response.

`ClientView::from(view)` and `apply(&event)` return `Applied`, `Duplicate` for
`seq <= event_seq`, or `NeedsResync` for a gap, which leaves the view unchanged.
Views compare snapshots by session, sequence and record checksum.

**Requirement on the step-3 driver:**

1. Subscribe to the broadcast.
2. Take the view.
3. Drop buffered events with `seq <= view.event_seq`.

On `Lagged`, repeat all three. The driver owns the core in one task and takes
views only between steps.

## Not implemented yet

- **Step 3:** the storage worker thread, the Tokio driver and broadcast, and
  subscriber lag handling. The worker must switch off the test-build oracle on
  its own thread in benchmarks.
- **Step 4:** the provider `Stream` trait and a scripted mock that expresses
  mid-stream failure and cancellation without wall-clock time.
- **Step 5:** context projection and its report. `StartInference` names the head
  and model only.
- **Step 6:** startup recovery. `RecoveryRequired` only rejects commands today.
- Also later: approvals, compaction, configuration changes, real tools and
  providers.

## Tests

`runtime::tests` drives the core synchronously against a real `JournalStore` in
a temporary directory, with the oracle on. Every scenario runs twice with fresh
environments and is compared with storage-chosen bytes abstracted. It is also
replayed from its recorded inputs and must give byte-identical effects. Each run
checks resync from every recorded view, and checks that reopening the journal
gives the published snapshot. A randomized test runs 512 seeds. See
[development](development.md#runtime-tests).
