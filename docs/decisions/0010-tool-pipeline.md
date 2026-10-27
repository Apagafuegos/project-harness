# 0010: Pure-core sequential tool pipeline

Status: Implemented through Milestone 3 step 4b in the pure core and async
session driver. The trusted filesystem executor is implemented separately in
step 5 ([0011](0011-trusted-file-tools.md)); sandbox execution and CLI host
installation remain deferred.

Date: 2026-10-27

## Decision

The runtime is still a deterministic `step(Input, Env) -> Vec<Effect>` machine.
Its immutable `ToolSet` pairs the fixed registry with available executor
backends. `Runtime::new` installs none and preserves Milestone 2's
`tools_unavailable` results. `Runtime::with_tools` enables the pipeline.
Definitions on each `StartInference` are filtered by enabled configuration and
backend availability, in registry order. They are separate from `ModelInput`:
projection bytes, builder/projection versions, fingerprints and golden files
are unchanged. Native provider adapters will decide how to send definitions.

For each durable proposal, in block order, the core resolves the name and
validates arguments. `Processing -> Authorizing` is committed before
`PlanCall { token, call, tool, operation }`. The driver plans against the fake
workspace and current policy, then returns `Planned`. Planning is an effect
because workspace inspection is I/O; `decide` is pure. A refusal answers only
that call, then the next call is validated. There is no overlapping planning
or execution. Local planning/execution tokens are fresh; stale answers do
nothing. All journal identities, revisions and timestamps come from `Env`.

**Write ahead.** Allow commits `authorization_decided(allow)` and
`execution_intent`, then `authorizing>executing`. Only its acknowledgement
produces `ToolStarted` and `Execute`. Intent effects come only from
`Plan::effects()`. Plan bytes are published exactly, not reserialized.
`AuthorizedOperation` has private fields and no public constructor or
serialization ingress. Only the core constructs it: plan, invocation, call,
decision and authorizing revision. Sharing immutable plan internals keeps the
by-value effects small without changing canonical bytes.

`Executor` is synchronous, `Send + Sync`, reports its backend, and takes an
`AuthorizedOperation` reference and an `Arc<AtomicBool>`-backed `CancelToken`.
`ExecutionOutcome` contains explicit status, optional closed `ResultCode`,
inline content, exit code, elapsed milliseconds, truncation and capture bytes.
Success has no code; other executor statuses require one. Planning refuses
with `invalid_arguments`, `denied`, or `infrastructure_failure` (decision 22),
never `failed`. An executor sets `failed` explicitly, for example `stale_plan`.
Results have executor provenance only if an intent was durable and the driver
returned an executor outcome. `ToolFinished` is emitted only after result sync.

**Artifacts.** `Persist.artifacts` travels in the same worker job as the
append. The existing artifact store publishes/syncs all bytes before the
commit is bound/appended. This covers plans, remembered configurations and
executor captures. Artifact or append failure faults the runtime, with no
retry or dispatch. The async driver only has the mechanical new-field and
exhaustiveness adaptations here; it installs no tools and rejects an
unexpected tool effect rather than silently ignoring one.

## Approval lifecycle

Ask commits `approval_requested` with the canonical plan artifact/digest,
policy revision and `Env::now() + five minutes`. After acknowledgement,
`ApprovalRequested` updates the shared client reducer and `pending_approval`
in resync views, and the core arms a timer for the remaining lifetime.

`SubmitApproval { approval_id, plan_digest, decision, remember }` is accepted
only for that durable, pending ticket. Wrong digest rejects with
`approval_mismatch`, without a commit. Wrong ID, used ticket, or ticket from
another runtime replies `NotActive`. Both the wall clock at submission and the
timer enforce expiry; delaying timer delivery cannot extend authorization.
Policy-revision mismatch invalidates the request. Durable resolution emits
`ApprovalResolved`, clearing the resync view. Codes are `approval_denied`,
`approval_expired`, `approval_invalidated`, or `cancelled`.

**Decision 24: denials require two commits.** First commit
`authorization_decided(deny, reason=<code>)`, then its acknowledgement permits
the nonexecuted result and onward transitions in a second commit. Storage
applies entries before events and checks authorization against the next pending
call, so combining denial and result would fail `call_not_next_on_branch`.
There is no change to that rule. Approval submission is acknowledged when its
decision is durable; result notification follows the result's own sync.

**Remember (decision 16).** On an approved write/edit/bash call, derive the
narrowest allowance with `Allowance::remember` and generate a fresh revision
through `Env`. Publish the bytes from `PolicySnapshot::grant`, preserving all
prior allowances and other configuration values. One atomic commit orders:

1. `authorization_decided(allow, approval_id)` under the original revision;
2. `execution_intent`, consuming that original authorization;
3. `configuration_changed`, installing the grant and different revision;
4. `authorizing>executing`.

The intent precedes the grant because storage checks the authorization against
the revision current at that event. The core reloads policy from the new
configuration bytes only after acknowledgement. The original invocation keeps
its original plan/revision; the next identical call sees the grant and does not
ask again. Reads ignore remember. Configuration source bytes are retained by
`SessionConfiguration`; context projection semantics do not change.

The sole storage exception is: outside Idle, a configuration change requires
a preceding, validated allow in the **same commit**, consuming a pending
approval under the current revision; the new revision must differ. Idle rules
are unchanged. Tests reject no allow, wrong order, deny, missing ticket,
unchanged revision and an allow in an earlier commit.

A malformed policy fails the turn with `policy_invalid`, not a model-facing
configuration refusal. Already proposed, unstarted calls receive ordinary
`cancelled` results so storage's complete-exchange invariant is preserved.
The session remains usable for later turns. With no executors the old behavior
is preserved, including legacy configurations without tool policy fields.

## Cancellation and recovery

Cancelling in Authorizing revokes a pending ticket with a durable deny first
(reason `cancelled`), then answers that call and all later calls `cancelled`
and finishes the turn. A late planning answer cannot execute anything.
Cancelling after a durable intent emits `CancelExecution`, commits Cancelling,
and waits for the executor's terminal answer. If cancellation wins, record
cancelled; if the mutation already succeeded, retain succeeded. Later calls
are cancelled. There is no rollback claim, and no inference before cleanup.

Recovery is unchanged: a durable intent without a result is uncertain,
including reads in this step, and is never replayed. A decision without an
intent (including the new denial/result crash gap) is known not started and
recovery answers it `cancelled/process_restart`, with no uncertainty. A durable
result is replayed from storage, never re-executed. Pending approvals do not
survive runtime replacement. The defensive revision-invalidation test injects
a decoded policy revision while waiting; there is no new public concurrent
configuration-update command in this sequential slice.

## Verification and deferred work

Storage-backed scenarios cover T01/T02, exact write-ahead binding, approval
expiry/mismatch/denial/invalidation, remembered grants, executor failures,
artifact publication failures, stale tokens, R02 cancellation races and R01
Lost/Durable/Torn crashes at every new durability gate. Scenarios replay their
inputs and fake-clock samples and resync from recorded views. Randomized runs
exercise tools, approvals, remember, executor outcomes, crashes and cancels;
context rebuild checks use each immutable snapshot's own configuration.

Step 4b supplies dedicated executor-thread and async handle plumbing. No real
workspace or subprocess executor is enabled here. Elevated authorization and
subprocess networking are not in Milestone 3; protected paths stay denied.
Read-only recovery and evidence-based hints remain step 6. Seccomp remains the
recorded ADR 0008 gap.

## Amendment (step 4b)

`DriverConfig.tools: ToolHost` supplies a trusted workspace and one executor per
backend. Duplicate backends are rejected before storage I/O. `ToolHost::none()`
is the default: no executor thread and unchanged `tools_unavailable` behavior.
Both initial launch and replacement construct `Runtime::with_tools` with the
host's backends. The host's Debug output lists only backends.

One `harness-tools` OS thread per tool-enabled session receives sequential Plan
and Execute jobs on a standard channel. Planning resolves the built-in against a
cloned current policy and reads the workspace on that thread; execution receives
the already-authorized operation. Answers return over a Tokio channel selected
by the driver. There is at most one outstanding job; overlap faults with
`executor_overlap`. Jobs and answers carry the runtime epoch and local token.
Answers from other epochs are silently dropped before touching outstanding work.

Each execution has a fresh cooperative `CancelToken`, retained by the driver.
`CancelExecution` cancels it directly, never through the busy thread's queue.
The executor's actual terminal outcome wins, including success after a mutation;
cancellation does not claim rollback. Planning and execution cannot block views,
subscriptions or cancellation on the driver task.

Each job is unwind-protected. Planning panic yields an honest nonexecuted
`infrastructure_failure` refusal. Execution panic, missing backend or thread loss
faults `executor_lost`, with **no** synthetic execution result. The durable intent
remains uncertain on recovery and is never dispatched again.

Fault cancellation can outlive the runtime. Reopen and in-place repair await the
outstanding answer asynchronously **before** restarting storage or recording
uncertainty. Shutdown cancels, awaits cleanup and durability, closes the job
channel and joins the executor through `spawn_blocking`, before stopping storage.
Thus no live effect can race user resolution, and shutdown releases the writer
only after executor cleanup. Wait bounds rely on executor deadline compliance.

An expiry comparison and the record authorizing against it now share one lazily
sampled clock instant per core step (decision 28). `ResultCode::ALL` and `parse`
centralize the closed code set with exhaustive compile-time coverage; an invalid
stored denial code faults `invalid_stored_result_code` rather than panicking.
