# 0005: Provider stream seam and scripted mock

Status: Accepted for Milestone 2, step 4. Amended the same day with the loop
order and the provider idle timeout, before step 5.

Date: 2026-10-20

## Context

Contract 1 and RFC-001 section 3 propose an owned, pull-driven event stream per
inference, with cancellation through a token. Step 3 drove the runtime through
a private mpsc stand-in: the public handle could only report
`provider_unavailable`, and the driver dropped the receiver on
`CancelInference`. The core already waits for the first of `Finished`,
`ProviderEnded` or its grace timer after a cancel. Because the stand-in dropped
the stream, a confirmation could never arrive, so every cancellation waited out
the full `cancel_grace`.

## Decision

**A public `harness_core::model` seam.** `ModelProvider::stream(&self,
ModelRequest, OperationControl) -> ModelEventStream`, where the stream is
`Pin<Box<dyn futures_core::Stream<Item = ProviderEvent> + Send + 'static>>`, as
RFC-001 3.1 proposes. The stream is owned, does not borrow the provider, and
starts work only when polled. Setup failures are terminal events, possibly
before `Started`. The trait is object safe. `SessionHandle::open` and `create`
take an `Arc<dyn ModelProvider>`, and the private seam and `UnavailableProvider`
are gone.

**The core's `ProviderEvent` is the normalized event.** It moves to `model`
and is re-exported from `runtime`. RFC-001 proposes a `ModelEvent` carrying
`response_id` on every event and an engine-issued `call_id` on tool events.
Neither is adopted. A stream belongs to exactly one request, so a per-event
response ID tells the driver nothing it does not already know. Decision 0004
has the core issue each `CallId` at `ToolCallStarted`, so a provider cannot
choose journal identities, and native IDs travel in `provider_call_id`.
`Finished` stays the flat `{ outcome, finish_reason, error }` that the core
validates. Usage arrives through `UsageUpdated` snapshots. Continuation
arrives with native providers and the context builder.

**The request names only what exists.** `ModelRequest { response_id, input }`
keeps RFC-001's shape, but `ModelInput` holds only `model` until step 5 adds the
projected instructions, items, tools, options and continuation. The runtime
chooses the response ID. The provider never sees the session head or turn.

**`OperationControl` carries only the cancellation token.** RFC-001 also
proposes an optional deadline. No policy sets one yet, and a field nothing
produces would fix its semantics before they exist. It is deferred.

**Cancel, then release.** `CancelInference` now means: cancel the stream's
`tokio_util` `CancellationToken` and keep polling. The core ignores events
other than a confirmation (`Finished` or end of stream) until `cancel_grace`
ends. A new `ReleaseInference` effect means the core has stopped listening.
The driver drops the stream, and dropping always cancels the token as well.
The core emits it exactly once per `StartInference`: when a stream ends for any
reason, at grace expiry, on a malformed or oversized stream, and on fault.
Malformed and oversized streams are released, not cancelled, because nothing
remains to confirm. The release decision belongs to the core; the driver only
executes it. Core tests assert the once-per-inference invariant at every step.

**Pull-driven, with no queue in the driver.** The driver polls the current
stream in its `select!` loop and feeds each event to the core. Nothing is
buffered between them; the core's `max_partial_bytes` bounds retained content.
RFC-001's per-adapter queue bounds (256 events, 1 MiB, 256 KiB per event)
apply to native adapters, which buffer network input. The mock has no queue.

**`ScriptedProvider` is the deterministic mock.** It is public, in
`model::scripted`. Each stream takes the next `Script`, a list of `Step`s:

- `Event`: yield one event.
- `Wait(oneshot::Receiver)`: hold until a test releases it. Cancellation does
  not release it.
- `AwaitCancel`: hold until the token is cancelled.
- `Stall`: never continue; only dropping ends the stream.

The end of the script is EOF. Events are yielded exactly as written, including
invalid sequences, so validation can be tested through it. It reads no clock.
With no script left, a stream reports a setup failure (`provider_failure`,
`no_script`). `streams()` records each stream's request, events yielded, and
whether it was cancelled and dropped.

**Provider events are polled last.** The driver's biased loop polls storage
replies, then worker exit, timers, artifact failures and the mailbox, and only
then the stream. Each provider event spends Tokio's cooperative budget
(`tokio::task::coop::poll_proceed`), so a stream that is always ready still
makes the driver task yield. Commands therefore arrive between events, and a
`Cancel` takes effect at the next event boundary. In the test, with 20,000
deltas ready, no delta was applied between sending the cancel and its effect.

**An idle timeout belongs to the core.** `RuntimeConfig::provider_idle_timeout`
(default 300 s) bounds the time an open stream may go without any event. The
core arms a timer with `StartInference`, which also covers setup, and re-arms
it after every event that leaves the stream open, even one it rejects. A
cancellation replaces it with the grace timer. On expiry the core releases the
stream, records the response as `interrupted` and finishes the turn `failed`
with code `provider_stalled`. Nothing remains to confirm, so it releases rather
than cancels. The default is generous because reasoning models may stay silent
for minutes before their first visible event.

**Dependencies.** Both are added in the workspace manifest and used only by
`harness-core`:

- `futures-core` 0.3, with `default-features = false` and only `std`, for the
  `Stream` trait. Tokio already depended on it.
- `tokio-util` 0.7 (0.7.20 locked), with `default-features = false` and no
  features enabled, for `sync::CancellationToken` and its owned
  `WaitForCancellationFutureOwned`. That module is unconditional; the codec,
  io, net, rt and time feature modules stay disabled. It adds `bytes` and
  `futures-sink` to the lockfile; `pin-project-lite` and `futures-core` were
  already present. Enable a feature only together with code that needs it,
  and record it here.

## Consequences

- A confirmed cancellation finishes without waiting for the grace period. An
  adapter that ignores cancellation costs `cancel_grace` before it is dropped.
- Driver tests express cancellation, interruption, setup failure, mid-stream
  failure and malformed output through scripts, without virtual time unless
  the grace period itself is under test.
- The loop serves timers and commands before provider events. A `View` or
  `Cancel` is never delayed by a fast stream, and a deadline that passes while
  events are ready still fires.
- Each event re-arms the idle timer, which costs a `CancelTimer` and a
  `SetTimer`: in the driver, an aborted and a spawned sleep task. A cheaper
  timer wheel can replace the per-timer tasks if streams make this measurable.
- A provider silent for longer than `provider_idle_timeout` fails its turn,
  even if it would later have answered.
- Step 5 extends `ModelInput`; providers must not assume it stays model-only.
- Not yet covered from Contract 1's validation gates: native replay state and
  its rejection (M04), deadlines, adapter queue bounds and a native conformance
  suite. These arrive with native providers in Milestone 4.

## Alternatives considered

- **RFC-001's `ModelEvent` with per-event response and call IDs.** This would
  duplicate identities that the driver and core already own.
- **Dropping the stream on cancel.** This is the step 3 behavior: simple, but
  confirmation can never arrive, so the grace period becomes a fixed delay.
- **Letting the driver infer when to drop.** This puts a lifecycle decision in
  the plumbing, and the pure core tests could not see it.
- **A push-based provider (channel or callback).** This needs an unbounded or
  separately bounded queue and detached producer tasks, which Contract 1
  forbids by default.
