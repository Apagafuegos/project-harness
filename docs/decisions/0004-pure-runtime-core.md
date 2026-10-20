# 0004: Pure runtime core with a driver

Status: Accepted for Milestone 2, steps 2 and 3.

Date: 2026-10-18

## Context

Contract 6 and RFC-001 section 7 describe an asynchronous runtime that owns one
active turn per session. Storage (decision 0003) already enforces the lifecycle:
legal transitions, the next call on a branch, the inference rules and the
recovery barrier. A second state machine that tracks the same rules could drift
from storage. Storage would then refuse a commit the runtime thought was legal,
at runtime rather than in tests.

## Decision

**A pure core and a driver.** `harness_core::runtime::Runtime` is a synchronous
state machine with the signature
`step(&mut self, Input, &mut impl Env) -> Vec<Effect>`. It performs no I/O and
reads no clock. It creates identities only through `Env`, and depends on neither
Tokio nor any client. One Tokio driver task is the only production caller of
`step`; it performs effects and turns their answers into inputs. It runs on the
caller’s runtime; the library creates no Tokio runtime. A named storage worker
thread owns the blocking writer, including opening and dropping it. The core
reads storage's facts from the published snapshot: pending calls, the model at a head, receipts, used identities and the
recovery requirement. It never re-derives them. Tests run every commit through
real storage with its oracle on, so drift fails in tests.

**One commit in flight.** The core issues one `Persist` and waits for
`Persisted` or `PersistFailed` before persisting anything else. It publishes a
snapshot only from `Persisted`, after checking that the ack extends the previous
snapshot by one record. Any persistence failure is terminal (`Faulted`) and is
never retried. This keeps journal order equal to decision order, and an
uncertain write cannot be repeated.

**Turn command IDs are journal dedup keys, distinct from `TurnId`.** Storage
keeps all identities in one set, so the command's UUID cannot also be the turn's.
The opening commit binds the command. Its receipt's commit holds the
`turn_started` that names the turn. The core fingerprints the command with
`session::store::normalized_command`, the function `bind_command` uses to store
it, and checks receipts before busy and recovery. A retry therefore learns the
durable outcome, including after a lost ack or a restart. A retry while the
opening commit is in flight attaches to that commit.

**Resync by epoch, view and sequence.** A snapshot cannot resync a client
because it omits the in-flight response. `RuntimeView` pairs the published snapshot with
phase, active turn and the bounded partial response at an event sequence. Every
event carries `seq`, contiguous per runtime, and an `epoch` from
`Env::runtime_epoch()` in `Runtime::new`. Each runtime incarnation has a fresh
UUIDv7 epoch, even over the same journal. The reducer checks epoch before seq,
so replacement events cannot be mistaken for old duplicates. The runtime builds
its own view by applying each emitted event with the client reducer, `ClientView`, so the view
reflects exactly the events up to `event_seq`. The driver subscribes before
requesting a view, then discards buffered events already reflected in that view.
Lag, a sequence gap or an epoch change restarts this protocol. The first item
and each resync is `Reset(ClientView)`, including the partial response.
`Input::PublishView` emits the core's current phase after a replacement, even
when recovery is required; subscribers need not wait for another command.

**Media preflight shares the exclusive slot.** Before C1 the core emits
`VerifyArtifacts`, waits for `ArtifactsVerified`, and attaches identical retries.
Other turns remain busy. Missing/invalid media rejects with `artifact_missing`
without writing. Storage still rechecks artifacts on append. A failure there
faults the runtime; no uncertain write is retried.

**Shutdown and faults.** Shutdown requests core cancellation, waits for in-flight
durability and artifact work, drops worker senders, then awaits exit before
replying. Commands arriving during the drain are answered `ShuttingDown` by the
driver without reaching the core. A worker panic or lost reply becomes `WorkerLost`. Infrastructure
failures outside an append enter the core through `Input::StorageFailed` and
fault there too. A driver invariant failure, such as a second storage effect
while one is outstanding, enters as `Input::DriverInvariant` and faults with
`FaultCause::Internal`; the outstanding operation is kept, never dropped.
Timer fires carry their runtime's epoch, and fires from a replaced runtime are
dropped. Reopen is allowed only in `Faulted`, drains the old worker and
opens the same journal on a new worker. It never repairs a torn tail implicitly.
An unfinished reopened snapshot stays `RecoveryRequired` until step 6.

## Consequences

- The lifecycle is tested without Tokio or a provider. Determinism is exact:
  replaying recorded inputs into a fresh runtime and environment reproduces the
  effects byte for byte.
- A refusal completes its turn; an output limit fails it with `output_limit`.
  Only recovery records interrupted turns. Tool calls are answered `denied`
  with the text `tools_unavailable` until tools exist, because storage results carry no code.
- One commit in flight serializes a turn's durability latency. Every commit
  waits for its sync before the next decision, as RFC-001 5.3 requires.
- The core delegates media verification to the worker before building C1.
- `SelectHead` and `SwitchModel` carry no command ID and are not deduplicated.
- Adding storage-side rules needs no core change unless the core must produce
  new shapes. If it does, the brief's constraint applies: report rather than
  change `state.rs` without explicitly reviewing its validation contract (as
  the separately committed refusal rule did).

## Alternatives considered

- **Async actor with inline awaits.** Simpler to write first, but it mixes
  scheduling with decisions, and testing the decisions needs a runtime and fake
  time.
- **Pipelined commits.** Lower latency, but a failure would leave later
  decisions built on an unacknowledged state.
- **Reusing the command ID as the `TurnId`.** Storage rejects it with
  `duplicate_id`.
- **Snapshot-only resync.** Loses the in-flight response, and cannot order deltas
  against durable state.
