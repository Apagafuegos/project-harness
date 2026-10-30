# 0014: Complete logical model requests and native replay

Status: Implemented for Milestone 4, step 01a. No native HTTP adapter is
implemented by this decision.

Date: 2026-10-29

## Decision

`ModelInput` contains model, ordered items, enabled/available `ToolDefinition`s,
`InferenceOptions` and one optional `ProviderContinuation`. Definitions move
from `Effect::StartInference` into the input; the builder takes them through
`with_tools`. The runtime uses its current policy and backend availability for
both inference and inspection. Adapters receive no policy or workspace authority.

`InferenceOptions.max_output_tokens` is exactly `ContextConfig.output_reserve`.
The session configuration's optional `inference` object accepts
`reasoning_effort` (`low`, `medium`, `high`, or absent/null) and
`provider_options`, an object mapping nonempty provider namespaces to JSON
objects. Unknown inference fields and malformed shapes fail configuration
loading with `invalid_inference`. Adapters must validate their namespaces and
reject unsupported required values with `unsupported_capability`, not drop them.
There is no automatic inference retry.

### Fingerprint and continuation validity

Projection version **4** hashes compact canonical JSON in field order:
`items`, `model`, `tools`, `options`, `projection_version`. Struct order is
fixed; JSON object keys are sorted. Response identity, deadline and continuation
are excluded. This supersedes decision 0010's M3 decision 18, which kept tool
definitions outside the projection and its fingerprint, and completes the
native-state deferrals in decisions 0005 and 0006. Builder version **4** adds
closed continuation exclusion reasons and the single-request continuation check.

Per-item continuations are removed from `ContextItem::Assistant`; only the
request input carries the selected continuation. Owner-scoped per-block native
state remains in assistant content. A continuation is selected only from the
last included assistant entry, only for the projection model, and only when a
pure rebuild at that response's parent, using **current** instructions, tools,
options and model, matches its `input_fingerprint`. Prefix rebuilding reproduces the full projection, including earlier
continuation-only assistants that are valid at that prefix. Fingerprints are
memoized oldest-first per build, without recursion; continuation values remain
excluded from the hash. Each required parent is rebuilt at most once per
build, rather than recursively redoing prefixes, with a linear-sized digest
cache. Long native-continuation histories still incur repeated prefix walks. A later assistant
without a continuation does not fall back to an older handle.

Exclusions remain source-ordered and complete:

- `provider_state_not_portable`: another owner;
- `continuation_not_latest`: not the last included assistant (also used by
  historical continuations superseded by a later included assistant);
- `continuation_prefix_mismatch`: the current rebuilt prefix differs.

Covered, superseded and failed responses retain their entry-level exclusions.
An otherwise content-empty assistant remains only when its selected
request-level continuation is valid and associated with its report. This
preserves decision 0006's content-or-continuation rule after migrating the
continuation out of the item. Invalid continuation-only responses are omitted. Branch navigation never
imports another branch's entries or handles. Payloads are never converted to
text, and reported usage remains optional, never invented as zero.

### Native payload publication

The normalized events add:

```rust,ignore
ProviderState { block_index: u32, schema: String, payload: Vec<u8> }
Finished { outcome, finish_reason, error,
           continuation: Option<NativeContinuation { schema, payload }> }
```

Milestone 4 decision 12 resolves the brief's original “unknown block” wording:
`ProviderState` introduces its own complete block at a fresh index, with no
separate start event. Any used index is `malformed_stream/duplicate_block`.
Text/reasoning deltas to a state block are `block_kind_mismatch`. Empty schemas
are malformed, matching storage's schema requirement. Continuations on a
non-completed finish are malformed. The provider cannot supply artifact identity,
owner, or continuation fingerprint: those fields do not exist on the events.

The core privately retains payloads. `PartialBlock::ProviderState` and its live
event expose only `{ schema, bytes }`, never payload bytes or a text rendering.
In content/block-index order, the response references artifacts owned by the
request model. A completed continuation is similarly owned and bound to the
producing request's fingerprint. `Persist.artifacts` publishes/syncs exact
payload bytes before appending the same response commit. Publication failure
faults without retry; ordinary recovery never generates or replays inference.
Storage's existing `InferenceResponse` rules are unchanged.

Each native payload is at most **256 KiB**, with **4 MiB** total native state per
response, including its continuation. An excess fails with `limit_exceeded` and
removes all call eligibility. Accepted bounded state may remain in that failed
response, but failed responses are excluded from context. Payload bytes do not
consume `max_partial_bytes`; their journal envelopes (schema and fixed block
overhead) do, preventing unbounded empty native blocks or oversized schemas.

### Read-only replay artifacts

`ModelRequest.artifacts: ArtifactSource` is cloneable and testable. Its exact
allowlist comes from the projection's state, continuation, media and referenced
tool-result artifacts. `read(&ArtifactRef).await` refuses an unreferenced or
size-modified reference before I/O, verifies both length and SHA-256, and returns
only verified bytes. The driver's private resolver bounds the actual storage
read by the referenced length on a blocking worker. Providers receive neither
`DataRoot` nor a write capability. Debug/equality expose/compare the allowlist,
not resolver internals or artifact bytes; equality is logical request equality,
not storage-handle identity.

### Per-inference deadline

`RuntimeConfig.inference_deadline: Option<Duration>` defaults to `None`.
At stream start the driver derives `OperationControl.deadline` using Tokio's
monotonic clock. The pure core independently arms an existing `SetTimer` effect
for that duration. Progress restarts only the idle timer, never the deadline.
Setup is included. Expiry cancels the token, ignores subsequent content, waits
for finish/EOF or `cancel_grace`, then releases once and records an interrupted
partial response and `turn_finished(failed, inference_deadline)`.

This implements the brief's explicit cancel/grace requirement; unlike the
existing idle timeout's immediate release (`provider_stalled`), a deadline
allows cleanup confirmation. User cancellation still wins as user cancellation
when requested durably. All terminal paths/faults disarm the deadline. There is
no clock in the builder or core.

## Amendment (m4-03)

M4 decision 15 makes continuation optional in the public `NativeState`
conformance case: native blocks and a completed outcome are required, but a
response-level continuation is not. Fixtures with continuations retain the
existing metadata, bounds and no-continuation-on-failure checks. OpenAI's
stateless adapter uses per-item state without a continuation.

M4 decision 16 replaces the derived `Debug` on `ProviderEvent` and
`NativeContinuation` with hand-written impls that print opaque payloads as
`[redacted; N bytes]`; types, fields and other impls are unchanged. Adapter
steps may make small fixes inside `model/` that change no public type.

## Evidence

- `context::tests::continuation_*`: branch/prefix, instructions, tools, options,
  model and latest-response invalidation; canonical bytes/fingerprint tests.
- `runtime::tests::model`: real-storage native bounds/ownership, malformed
  sequences, metadata-only views, deadline scenarios with replay/resync.
- `runtime::driver::tests::model_tests`: worker crash/reopen replay, same-owner
  reads, switched-owner omission, media reads and virtual-time deadline tests,
  including setup and an adapter that ignores cancellation.
- `model::artifacts::tests`: exact allowlist, no unreferenced I/O, size/hash checks.
- The seeded randomized runtime suite now emits native blocks/continuations,
  schedules deadlines, and checks stored/request owner scoping and fingerprint
  binding. The provider-event compile-fail doctest pins that an adapter cannot
  name another owner.
