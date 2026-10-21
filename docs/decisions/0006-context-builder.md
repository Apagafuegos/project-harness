# 0006: Pure context builder and report

Status: Accepted for Milestone 2, step 5.

Date: 2026-10-21

## Context

Contract 5 and RFC-001 section 6 propose a context builder that projects one
branch of history for one model and explains the result. Until step 5,
`StartInference` named only the head and model, and `ModelInput` held only the
model. Context inspection is meant to set this harness apart, so the report is
a first-class output that clients, a future TUI inspector and tests all read,
not a debug print. Storage already validates everything the builder relies on:
ancestry, the model at each head, call/result order, and compaction coverage
that never splits a turn.

## Decision

**A pure function, called inside `step`.** `ContextBuilder::build(snapshot,
head, model_override)` reads only the snapshot, head, `ContextConfig`, decoded
`SessionConfiguration` and a `TokenEstimator`. It performs no I/O and reads no
clock, so the core can call it synchronously whenever a turn enters
`Preparing`. No separate builder task, cache or effect is needed, and replaying
a runtime's inputs reproduces its projections byte for byte.

**The report comes from the same walk.** One pass over `ancestry(head)` finds
summaries and answered calls; a second makes every decision and produces both
the projection and the report. They cannot disagree, and checks after every
build prove it: every branch entry has exactly one decision, every block and
continuation of an included response exactly one part decision, nothing from
another branch appears, and calls and results are paired. The checks run in
every build profile; a violation is `ContextError::Invariant` and faults the
core, which never guesses.

**The projection is never persisted.** Nor is the report. History is the only
durable fact; both can always be rebuilt. The fingerprint is SHA-256 of the
compact canonical JSON of the items, the model and `PROJECTION_VERSION`. It is
the identity of a projection, stable on any machine and after any reopen.
Changing an inclusion rule or item shape bumps `PROJECTION_VERSION`.

**Closed exclusion reasons.** `ExclusionReason` is an enum: `orphan_tool_call`,
`failed_response`, `reasoning_display_only`, `provider_state_not_portable`,
`model_change_marker`, `superseded_summary` and `covered_by_summary`. A report
cannot invent free text, and a silent drop fails the completeness check.

**Honest budgets.** Limits come only from a configured `ModelCatalog`; the
provider is not consulted. An unknown context limit is reported as unknown,
with no overflow check, and the turn proceeds. Estimates name their method;
opaque provider state is counted in bytes, never tokens; provider-reported
usage is labelled, attributed to the response it describes, and never added to
an estimate. An estimate above the usable budget fails the turn from
`Preparing` with `context_overflow`, before C2 and without a provider call.

**Instructions come from the configuration artifact.** Its optional
`instructions` array becomes the first projection item. The pure core cannot
read artifacts, so the storage worker decodes the artifact once when a session
opens and `Runtime::new` takes it. The builder refuses one decoded from another
artifact.

**Reasoning summaries are display-only.** They are never replayed to any
provider. Native reasoning replay, if it ever comes, belongs with opaque
provider state.

## Consequences

- `Effect::StartInference` carries `input: ModelInput` and `fingerprint`
  instead of `model`, and `ModelInput` gains `items`. `ModelProvider`
  implementations receive the projection.
- `Runtime::new` takes a `SessionConfiguration`; `RuntimeConfig` gains
  `context`. `StorageWorker` fails to open a session whose configuration
  artifact does not decode.
- `ContextBuilt` is a new event. It does not change client views.
- The builder's cost is linear in branch length, once per inference. On a
  10,000-entry branch a release build takes about 60 ms, of which about 18 ms
  is hashing a 2 MB projection and 15 ms the checks. Only storage validation is
  logarithmic.
- A preview (`model_override`) is a report, never a request: the core refuses
  to start an inference from one.
- `ModelRef` derives `Eq`, `Hash` and `Ord` so it can key the catalog. The
  journal format, schema and validation rules are unchanged.

### Departures from RFC-001

- **Unknown budgets are reported, not rejected.** RFC-001 section 3.1 and C01
  require an explicit limit before inference. This decision reports the budget
  as unknown and lets the turn proceed; an operator who wants enforcement
  configures a limit. Rejecting would make every session without a catalog
  entry unusable before native adapters can supply limits.
- **No `other_branch` entries.** RFC-001 proposes listing sibling entries as
  `other_branch`. Reports cover exactly the selected branch, which keeps them
  linear in that branch; another branch is inspected by passing its head.
- **Reason codes.** The RFC's `recent`, `pinned`, `summary`, `audit_only`,
  `partial_response` and `incompatible_provider` are not used. Pins and the
  retained-recent window arrive with compaction; audit events are never tree
  entries; partial responses are included with their `outcome`;
  `provider_state_not_portable` names the incompatibility.
- **Instructions, not `tools` and `options`.** `ModelInput` has `model` and
  `items`. Tool definitions, inference options and continuation selection are
  added with tools and native adapters.

## Alternatives considered

- **Build in the driver, or in a task.** It would have to be fed back as an
  input, duplicating the snapshot the core already holds, and replay would no
  longer reproduce projections. The builder is cheap enough to run in `step`.
- **Persist the projection or fingerprint.** A projection is derived data; a
  stored copy could disagree with history. Recording the fingerprint on each
  response is useful but needs a schema version, and is deferred to Milestone 3.
- **Debug-only checks.** A broken invariant could send an unanswered call to a
  provider, which rejects it. The checks cost about a quarter of a build and
  run everywhere.
- **Canonicalize through `serde_json::Value`.** Sorting every object's keys made
  hashing a 10,000-entry projection take 60 ms instead of 18 ms. Struct field
  order is fixed by the code and versioned by `PROJECTION_VERSION`, and map
  order is enforced by a test, so the compact serialization is already canonical.
