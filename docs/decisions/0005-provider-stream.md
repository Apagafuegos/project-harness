# 0005: Provider stream seam and scripted mock

Status: Accepted for Milestone 2, step 4.

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

**Dependencies.** `futures-core` supplies `Stream` and `tokio-util` supplies
`CancellationToken`, both without default features.

## Consequences

- A confirmed cancellation finishes without waiting for the grace period. An
  adapter that ignores cancellation costs `cancel_grace` before it is dropped.
- Driver tests express cancellation, interruption, setup failure, mid-stream
  failure and malformed output through scripts, without virtual time unless
  the grace period itself is under test.
- The biased loop still polls provider events before timers and commands, as
  step 3 chose. A stream that is always ready is consumed before the next
  command is read, bounded by the core's partial limit. Native adapters wait
  on I/O between chunks.
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
