# Documentation

This repository begins with environment setup and design contracts. A document
describing a future component is not evidence that it is implemented.

- [Development](development.md): toolchain, commands, CI, dependencies, and checks.
- [Architecture](architecture.md): component ownership and the session/context boundary.
- [Rust-only foundation decision](decisions/0001-rust-only-foundation.md): accepted
  stack, crate layout, and client boundary.
- [v0.1 constraints](decisions/0002-v01-contracts.md): six contracts and the
  provisionally agreed JSONL, tree, tool, sandbox, and context choices.
- [RFC-001](rfcs/001-core-types-state-persistence.md): proposed concrete types,
  state transitions, atomic journal commits, recovery, and conformance cases.
- [Storage implementation decision](decisions/0003-jsonl-storage.md): accepted
  storage details, API decisions, and boundaries of the first milestone.
- [Session storage usage](session-storage.md): implemented Rust API and recovery
  behavior, with an executable example and conformance-test coverage.
- [Runtime core decision](decisions/0004-pure-runtime-core.md): accepted pure
  core with a driver, one commit in flight, command dedup and view resync.
- [Runtime core](runtime.md): implemented inputs, effects, turn commits,
  cancellation, worker/driver, media and resync; what steps 4–6 add.
- [Model provider](contracts/model.md): native APIs, hybrid multimodal content,
  streaming, capabilities, and model switching.
- [Tools](contracts/tools.md): registry, validation, authorization, sequential calls,
  and structured results.
- [Execution and security](contracts/execution-security.md): trusted built-ins,
  workspace capabilities, Bubblewrap, and fail-closed enforcement.
- [Session storage](contracts/sessions.md): one JSONL journal, trees, durable heads,
  writer ownership, and crash recovery.
- [Context management](contracts/context.md): independent projection, rolling
  summaries, budgets, and the headless inspection API.
- [Agent runtime](contracts/runtime.md): commands/events, one turn per session,
  state transitions, cancellation, and write-ahead execution intent.
- [Roadmap](roadmap.md): small vertical slices with observable acceptance criteria.

## Document status

**Accepted decisions** describe the chosen direction. Change them by recording
the reason and consequences in a decision record.

**Provisionally agreed contracts** establish the six behavioral boundaries and
the architectural constraints in decision 0002. These choices supersede earlier
options such as SQLite or parallel initial tool execution.

**RFC details** specify how to implement those boundaries. Storage details are
accepted and tested under decision 0003; the pure runtime core's turn commits
and async worker/driver under decision 0004. Other details, including approval
defaults, the provider Stream trait and recovery orchestration, remain proposed. Schema/examples alone are
not evidence of implementation.

**Implementation status** is recorded in the README and roadmap. A contract is
validated only when its behavioral requirements have executable tests; live
provider compatibility needs additional opt-in verification.
