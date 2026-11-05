# Contract 6: Agent Runtime

Status: Implemented v0.1 boundary. Accepted ADRs refine the original
[RFC-001](../rfcs/001-core-types-state-persistence.md) design. The pure
core and async driver are implemented; see [runtime](../runtime.md) and
[decision 0004](../decisions/0004-pure-runtime-core.md).

## Inputs and observations

The engine accepts typed in-process commands for turns, cancellation, approvals,
head selection, model switching, compaction, context controls, strategy changes,
undo preview/undo and execution recovery. There is no separate server or JSON-RPC transport.
The synchronous core emits effects; the Tokio driver and dedicated storage/tool
workers own I/O. One commit is in flight; side effects wait for durable intent.
Tokio drives inference, process supervision, cancellation, and bounded event
delivery.

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
- Recovery marks unresolved mutating/process effects uncertain. No automatic
  replay occurs, and explicit reconciliation itself never reruns the operation.
  D9 closes an unambiguously read-only intent as cancelled/process_restart,
  never as observed success; legacy/missing effects and already uncertain reads
  still require explicit resolution.
- Branch navigation cannot bypass uncertainty about workspace effects.
- The engine runs without Ratatui. The TUI owns presentation and submits user
  decisions but contains no core agent logic or independent executor.

RFC cases R01–R02 and E01 cover recovery ordering, turn exclusion, cancellation,
event lag, and coherent client snapshots. The core's tests cover turn exclusion,
cancellation during inference and in-flight commits, and coherent views with
explicit gaps. The driver also tests subscriber lag in a real broadcast, runtime
replacement, worker faults and shutdown. Startup recovery is implemented under
[decision 0007](../decisions/0007-startup-recovery.md): interrupted turns close
after open, and uncertain intents wait for the user's resolution without being
rerun. Approval cancellation, execution and tool reconciliation (R01 and R02)
are implemented under [0010](../decisions/0010-tool-pipeline.md) and
[0013](../decisions/0013-tool-recovery.md); see
[Milestone 3 acceptance](../milestone-3-acceptance.md).

`crates/core/src/runtime/tests/` and `crates/core/tests/tool_host.rs` test
write-ahead dispatch, real-storage crash boundaries, cancellation, recovery and
randomized durable interleavings. `crates/cli/tests/tui_boundary.rs` guards the
thin client; `tui_pty.rs` tests restoration before panic diagnostics and after
errors/signals. Subscribers receive an authoritative Reset after lag/epoch/seq
gaps, not a fabricated replay of missed transient deltas. Cancellation is not
filesystem rollback or a guarantee that remote computation/billing stopped.

No automatic inference retry, parallel tool execution, workspace switching,
subagents or remote client protocol is implemented. Persistence or unconfirmed
process cleanup keeps the session blocked, never silently idle. The
[audit](../security/v0.1-audit.md) records host cleanup/resource limits; a
remembered-grant continuation is refused by `ask` with a specific code and
resumed by the TUI that recorded it.
