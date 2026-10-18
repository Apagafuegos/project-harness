# 0004: Pure runtime core with a driver

Status: Accepted for Milestone 2, step 2.

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
Tokio nor any client. A driver (step 3) performs the effects and turns their
answers into inputs. The core reads storage's facts from the published
snapshot: pending calls, the model at a head, receipts, used identities and the
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

**Resync by view and sequence.** A snapshot cannot resync a client because it
omits the in-flight response. `RuntimeView` pairs the published snapshot with
phase, active turn and the bounded partial response at an event sequence. Every
event carries `seq`, contiguous per runtime. The runtime builds its own view by
applying each emitted event with the client reducer, `ClientView`, so the view
reflects exactly the events up to `event_seq`. The step-3 driver must subscribe,
take the view, and then drop buffered events at or before `view.event_seq`.

## Consequences

- The lifecycle is tested without Tokio or a provider. Determinism is exact:
  replaying recorded inputs into a fresh runtime and environment reproduces the
  effects byte for byte.
- Storage shapes the commits. A refusal fails its turn with code `refusal`,
  because storage completes turns only after `stop`. RFC-001 7.2 expected
  `completed` with that code. Tool calls are answered `denied` with the text
  `tools_unavailable` until tools exist, because storage results carry no code.
- One commit in flight serializes a turn's durability latency. Every commit
  waits for its sync before the next decision, as RFC-001 5.3 requires.
- The core cannot verify artifacts, so `StartTurn` accepts text only for now.
- `SelectHead` and `SwitchModel` carry no command ID and are not deduplicated.
- Adding storage-side rules needs no core change unless the core must produce
  new shapes. If it does, the brief's constraint applies: report rather than
  change `state.rs`.

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
