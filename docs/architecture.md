# Architecture

Status: Six high-level contracts provisionally agreed. Session storage is
implemented under [decision 0003](decisions/0003-jsonl-storage.md). The pure
runtime core, async driver and recovery are implemented under decisions
0004–0007. Milestones 3–4 add real tools/sandboxing, native providers, rolling
compaction and headless ask (ADRs 0009–0020). Remaining proposals in
[RFC-001](rfcs/001-core-types-state-persistence.md) are not implementation claims.

## Engine and client

`harness` owns CLI parsing, terminal lifecycle, navigation, input, approval
presentation, and future Ratatui rendering. It owns the Tokio runtime and talks
to `harness-core` through in-process typed commands and events. It does not
assemble provider requests, authorize actions independently, or execute tools.

`harness-core` is a headless library containing six logical boundaries. These
are responsibilities, not six new crates. One process and Cargo workspace remain
the baseline; another client/process needs a transport decision, not a rewrite
of orchestration.

## Six contracts

1. **Model provider:** native OpenAI first, native Anthropic second; a common
   streaming interface and hybrid multimodal content preserve necessary native
   state. Capabilities are explicit, and model switches report incompatibility.
   Adapters never execute tools. See [model](contracts/model.md).
2. **Tools:** a registry exposes validated `read`, `write`, `edit`, and `bash`.
   Every call is authorized and returns structured, call-associated results.
   Multiple proposals are supported but execute sequentially in v0.1. Future
   MCP/extensions must integrate through the registry. See [tools](contracts/tools.md).
3. **Execution and security:** trusted built-in Rust file operations may run
   directly under descriptor-based workspace policies. Every external executable
   uses Linux Bubblewrap. Default network denial and explicit, auditable elevation
   are enforced separately from user authorization. No unrestricted fallback.
   See [execution and security](contracts/execution-security.md).
4. **Session storage:** one append-only JSONL journal per session contains tree
   entries and ordered audit events, with a durable active head. One writer,
   validated reconstruction, explicit corruption diagnostics, and conservative
   execution recovery are required. See [sessions](contracts/sessions.md).
5. **Context management:** a pure projection of the selected ancestry, recorded
   summaries, instructions, tool definitions, capabilities, and policy. Rolling
   compaction retains recent history and never deletes originals. A headless
   report explains inclusion, omission, provenance, and budgets from the first
   runtime slice. See [context](contracts/context.md).
6. **Agent runtime:** an asynchronous state machine orchestrates the other five.
   One active turn per session; typed commands, ordered observations, cancellation,
   durable tool outcomes before further inference, and no blind execution replay.
   See [runtime](contracts/runtime.md).

## History is distinct from context and workspace state

JSONL is authoritative history. Entries have parent links; siblings retain
alternative conversations. Model context is a temporary projection, not the
stored session object. Native formatting belongs to adapters after projection.

Compaction appends a summary with exact source references. Original messages,
tool results, and prior summaries remain inspectable. Given the same validated
snapshot/configuration, projection reuses recorded summaries deterministically;
LLM summary generation is a separate recorded operation.

Changing heads changes conversation ancestry, not files. Uncertain side effects
block execution recovery across branches; navigation cannot make them safe.
Git worktrees/checkpoints are outside v0.1.

## Persistence and execution ordering

The implemented RFC-001 persistence contract uses a checksummed JSONL commit that publishes entries and their
active-head update together. Audit-only records retain the head. Branch
navigation is itself a durable commit, without deleting sibling entries.

Before an executor runs, persist the proposal, authorization, and execution
intent. Before the next inference, persist the result and referenced artifacts.
A crash after intent without a result means uncertainty, not permission to retry.
Torn uncommitted tails require explicit repair; corrupted complete records are
never silently ignored. See the RFC for synchronization and recovery details.

## Security boundary

Routine means a trusted Rust implementation operating entirely within explicitly
authorized workspace capabilities, without spawning an executable. It does not
mean a shell command that appears harmless. Built-ins still pass policy checks;
their safety depends on correct implementation because they are not OS-sandboxed.

Bubblewrap is the selected enforcement backend, with explicit mounts, namespaces,
network denial, environment filtering, and process supervision. Before its tests
pass, the project must not claim to provide safe process execution. Required
S02 gate tests now exercise these boundaries. Execution is deny-network only;
restricted network exceptions need a
separately enforced backend and cannot silently become host networking.

## Implementation status

The three-crate workspace includes a blocking headless session store and J01–J03
behavioral tests: exact-byte checksums, atomic commits/head selection, semantic
replay, locking, artifact integrity, writer poisoning and explicit tail repair.
The schema and all three example journals are exercised by tests. See
[session storage usage](session-storage.md) for API limits.

`harness_core::runtime` is the synchronous core of the agent runtime: one input
in, effects out, no I/O. It reads lifecycle facts from storage snapshots and is
tested by appending every commit it emits to a real store. See
[runtime](runtime.md). A Tokio task is its only production caller and owns the
broadcast/handle; a dedicated thread owns the blocking store. `harness_core::model`
defines the provider stream seam and a scripted mock. `harness_core::context`
is the pure context builder: the core calls it in `Preparing`, and its report
is available through `SessionHandle::inspect_context` and the read-only
`harness context` command. See [context](context.md). `harness-providers` owns
both native adapters, shared HTTP/SSE/credentials and the instance Router, never
core orchestration. The runtime runs real authorized tools and independently
configured summary generation through the same provider seam. The headless
`harness ask` client uses typed commands/events and terminal approvals, and the
Ratatui client (M5) drives the same typed commands and subscriptions with a pure
model and a thin terminal shell. See [M5 acceptance](milestone-5-acceptance.md),
[M4 acceptance](milestone-4-acceptance.md) and the [roadmap](roadmap.md).
