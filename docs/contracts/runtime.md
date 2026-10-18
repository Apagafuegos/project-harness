# Contract 6: Agent Runtime

Status: Asynchronous state machine, one active turn per session, events, and
headless operation are provisionally agreed. Concrete API and transitions are
proposed in [RFC-001](../rfcs/001-core-types-state-persistence.md). The pure
core and async driver of turn handling are implemented; see [runtime](../runtime.md) and
[decision 0004](../decisions/0004-pure-runtime-core.md).

## Inputs and observations

The engine accepts correlated commands for turns, cancellation, approvals, head
selection, model switching, compaction, and execution recovery. Tokio drives
inference, process supervision, cancellation, and bounded event delivery.

Events expose text, state changes, context reports, approval requests, tool
lifecycle/output, diagnostics, and terminal outcomes. Live observations and
durably committed state are distinguishable. A snapshot/subscription API lets
clients recover from lag; cancellation does not wait behind streamed output.

## Lifecycle

Idle, preparing, inferring, processing, authorizing, executing, and continuing
are explicit operational states. Cancellation and recovery have explicit paths.
A turn has one durable terminal outcome before returning to idle. A model response
ending in tool proposals is not itself the end of the turn.

After successful response assembly, validate and authorize each proposed tool,
run calls sequentially, durably record all outcomes, and rebuild context for
the next inference. Expected operational errors may become model-visible results;
persistence failures and unresolved execution cannot masquerade as ordinary success.

## Guarantees

- Every turn has unique identity and one defined terminal outcome.
- One active turn per session; new starts while busy are rejected in v0.1.
- No proposal bypasses registry validation or authorization.
- Persist execution intent before side effects; persist results before inference.
- Cancellation revokes waiting approvals, stops queued calls, cancels streams,
  and cleans up child processes.
- Recovery marks unresolved effects uncertain. No replay occurs until explicit
  reconciliation, and reconciliation itself never reruns the operation.
- Branch navigation cannot bypass uncertainty about workspace effects.
- The engine runs without Ratatui. The TUI owns presentation and submits user
  decisions but contains no core agent logic or independent executor.

RFC cases R01–R02 and E01 cover recovery ordering, turn exclusion, cancellation,
event lag, and coherent client snapshots. The core's tests cover turn exclusion,
cancellation during inference and in-flight commits, and coherent views with
explicit gaps. The driver also tests subscriber lag in a real broadcast, runtime replacement,
worker faults and shutdown. Approval cancellation and recovery orchestration
(R01, the rest of R02 and E01) remain for later steps.
