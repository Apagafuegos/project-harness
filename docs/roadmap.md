# Roadmap

The scaffold, specification artifacts, headless storage (Milestone 1) and the
headless mock lifecycle with context inspection (Milestone 2) exist.
Later stages have acceptance criteria; they are not implemented features.

## 0. Foundation and proposed specification — present

Two-crate Rust workspace, pinned toolchain, dependency lockfile, CLI help/version,
shared local/CI checks, six provisionally agreed contracts, and the Rust-only
architecture decisions. RFC-001 proposes concrete types, JSONL transactions,
state transitions, recovery, and conformance cases, with schema/example journals.
The foundation itself implements no agent runtime, sandbox, or TUI.

## 1. Review RFC-001 and implement common types/storage — implemented

Storage review and implementation decisions are recorded in
[decision 0003](decisions/0003-jsonl-storage.md). Review the remaining proposed
details before implementing runtime work.
Tree-based JSONL history, selective sandboxing, six independent contracts, and
sequential tools are already architecture constraints, not choices to reopen
silently in code.

Implemented common IDs/content (including multimodal variants), validated entries,
journal records, immutable snapshots, and semantic validators. Tests cover checksums,
head transactions, single-writer locking, sync ordering, artifact publication,
torn-tail diagnostics/repair, and corruption rejection with fault injection.
RFC cases J01–J03 and all serialization fixtures run offline. No tool effects
exist. See [usage and limitations](session-storage.md): blocking storage API,
explicit recovery commits, local Linux storage, no salvage/export command.

## 2. Headless mock lifecycle and context inspection — complete

Step 2, the pure runtime core, is implemented: typed commands, inputs, effects
and events, the one-turn-per-session state machine, provider-event validation,
cancellation, command deduplication and the view/`seq` resync protocol. It is
tested against real storage with scenarios, replay and 512 randomized seeds
([runtime](runtime.md), [decision 0004](decisions/0004-pure-runtime-core.md)).
Step 3 is implemented: a dedicated worker owns blocking storage, a Tokio driver
owns the pure core, and `SessionHandle` exposes commands, media, shutdown,
reopen and subscriptions. Epochs and view resets cover lag and runtime replacement.
Step 4 is implemented: the public `ModelProvider` stream seam, cancel-then-release
stream lifecycle and a deterministic `ScriptedProvider`
([decision 0005](decisions/0005-provider-stream.md)).
Step 5 is implemented: the pure context builder and its report, run by the
core in `Preparing` (an unknown budget or an overflow fails the turn with
`context_budget_unknown` or `context_overflow` before any stream), `SessionHandle::inspect_context` and the read-only `harness context`
command, with golden reports for the three fixtures and C01 coverage
([context](context.md), [decision 0006](decisions/0006-context-builder.md)).
Step 6 is implemented: startup recovery and tail repair
([decision 0007](decisions/0007-startup-recovery.md)). After open, create or
reopen the core closes an interrupted turn on its own, answering calls that
never started as cancelled, and records an execution intent without a result
as uncertain, then waits until the user resolves it. Nothing is retried or
invented. A torn tail is repaired only through `SessionHandle::repair_tail`,
`reopen_repairing_tail` or `harness journal repair`, bound to the inspected
fingerprint and size. Every started turn now ends with exactly one
`turn_finished`, including after faults and crashes.

The milestone goal runs in one place as `cargo run --example scripted_session`.
[Milestone 2 acceptance](milestone-2-acceptance.md) maps every criterion and
M01–M04, C01, R02 and E01 to tests. It has no gap rows. Three cases are partial
because their remainder needs later milestones:

- **M03:** per-operation deadlines, queue bounds inside a native adapter and a
  second native adapter (Milestone 4).
- **M04:** the explicit `omit` switch policy and native replay state
  (Milestone 4).
- **R02:** cancellation while waiting for approval or during execution
  (Milestone 3).

## 3. Tools, selective security, and recovery

Implement registry validation, sequential scheduling, policy, approvals, and
opaque authorized operations. Add trusted `read`, `write`, and `edit` with
secure descriptor-relative access and safe replacement semantics. Persist intent
before side effects, results before inference, and recover uncertainty explicitly.

Add `bash` only behind the configured Bubblewrap backend with tested mounts,
network denial, environment filtering, deadlines, bounded output, and descendant
cleanup. Missing enforcement must fail closed. Run T01–T02, S01–S02, and R01;
actual Linux security tests are a gate, not an optional successful skip.

Implemented so far: `harness doctor` probes the host's sandbox capabilities
([decision 0008](decisions/0008-host-sandbox.md)), and journal schema v2 is in
place ([session storage](session-storage.md#record-versions)). Records are
versioned one by one; v1 journals stay readable and continue as v2. Results
carry a machine-readable code, intents their declared effects, and responses
the fingerprint of the projection their inference was sent. The pure tool
layer now provides four fixed definitions, strict validation, canonical v2
plans, configuration allowances and deterministic policy ([decision 0009](decisions/0009-tool-policy.md)).
No tool executes yet; runtime integration and closing read-only intents after
a crash remain planned.

## 4. Native providers and rolling compaction

Integrate OpenAI Responses first against current official documentation, with
native-event fixtures and opt-in live checks. Then add native Anthropic Messages
without provider-specific branches in orchestration. Prove portable diagnostics,
opaque-state retention, and safe switching within existing sessions.

Implement automatic/manual rolling summaries behind the strategy interface,
previews, source manifests, retained recent turns, and inspection diffs. Prove
C02 and budget failures without rewriting history. Ordinary CI stays offline;
live inference/summarization checks are separate and explicit.

## 5. Ratatui client

Implement rendering, navigation, streaming, cancellation, approvals, and a
context inspector over the same headless engine API. Restore terminal state on
errors/shutdown; handle subscriber lag through coherent snapshots. The TUI must
not implement provider request building, tool authorization, or execution.

## 6. Context experiments

Expand pin/exclusion controls and evaluate summary strategies on fixed coding
tasks. Measure requirement retention, modified-file recall, and completion after
compaction with recorded configuration/input provenance. Do not confuse better
summaries with lossless memory or workspace rollback.

## Outside v0.1

Subagents, MCP client, plugin runtime, semantic memory, remote workspaces,
multi-agent orchestration, separate server/JSON-RPC, and filesystem checkpoints.
Restricted subprocess networking also needs an actual constrained-egress backend
before the proposed deny-network-only implementation can grant exceptions.
Pick a license before publishing packages or claiming reuse terms.
