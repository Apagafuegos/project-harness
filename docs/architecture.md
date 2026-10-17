# Architecture

Status: Six high-level contracts provisionally agreed. Runtime unimplemented;
concrete details are proposed in [RFC-001](rfcs/001-core-types-state-persistence.md).

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

RFC-001 proposes a checksummed JSONL commit that publishes entries and their
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
pass, the project must not claim to provide safe process execution. The RFC
proposes deny-network only initially; restricted network exceptions need a
separately enforced backend and cannot silently become host networking.

## Implementation status

Only the two-crate scaffold, CLI help/version, tooling, and documentation exist.
RFC-001 includes machine-readable schema/examples and future conformance cases,
not an implementation of these guarantees. See the [roadmap](roadmap.md).
