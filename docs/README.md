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
- [Provider stream decision](decisions/0005-provider-stream.md): accepted
  `ModelProvider` seam, cancel-then-release lifecycle and scripted mock.
- [Context builder decision](decisions/0006-context-builder.md): accepted pure
  builder in `step`, unpersisted projection, closed reasons and honest budgets.
- [Startup recovery decision](decisions/0007-startup-recovery.md): accepted
  recovery as core commits after open, explicit resolution and bound tail repair.
- [Host sandbox decision](decisions/0008-host-sandbox.md): accepted shell
  security suite placement, the CI AppArmor profile and the opt-out.
- [Tool policy decision](decisions/0009-tool-policy.md): implemented pure tool
  validation/planning, canonical v2 plans, configuration allowances and policy.
- [Tool pipeline decision](decisions/0010-tool-pipeline.md): pure-core approvals,
  write-ahead execution, sequential executor threading and cancellation.
- [Trusted file decision](decisions/0011-trusted-file-tools.md): descriptor-rooted
  file operations, atomic replacement and real-filesystem S01 tests.
- [Sandbox decision](decisions/0012-sandbox-backend.md): Bubblewrap enforcement,
  mandatory S02 gate, session-owned host assembly and lock/spawn limitation.
- [Tool recovery decision](decisions/0013-tool-recovery.md): D9 read-only closure,
  advisory file-image hints and explicit user resolution.
- [Native model seam](decisions/0014-model-seam.md): complete requests, native
  replay, deadlines, conformance and redaction amendments.
- [Switching and metadata](decisions/0015-switching-model-metadata.md): reject,
  explicit omission and cross-provider routing amendment.
- [Provider transport](decisions/0016-provider-transport.md): HTTP/SSE bounds,
  cleanup, credentials and headerless-SSE amendment.
- [OpenAI Responses](decisions/0017-openai-adapter.md): native mappings, stateless
  profiles, official sources, catalog and sanitized live recording.
- [Anthropic Messages](decisions/0018-anthropic-adapter.md): native mappings,
  signed state and preserved-thinking amendment.
- [Rolling compaction](decisions/0019-compaction.md): selection, generation,
  previews and runtime/automatic-trigger amendment.
- [Headless ask](decisions/0020-headless-ask.md): durable CLI, approvals and
  cross-provider switching amendment.
- [Providers](providers.md): implemented adapters, router, credentials and opt-in checks.
- [Built-in tools usage](tools.md): implemented file/shell backends and host assembly.
- [Runtime core](runtime.md): implemented inputs, effects, turn commits,
  cancellation, worker/driver, provider stream, media, resync, recovery and
  tail repair.
- [Context builder](context.md): implemented inclusion rules, budget, report,
  fingerprint, `inspect_context` and `harness context`.
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
- [Milestone 2 acceptance](milestone-2-acceptance.md): each criterion and RFC
  case mapped to tests, with partial rows stated.
- [Milestone 3 acceptance](milestone-3-acceptance.md): tools, security, recovery,
  full R02, output artifacts, randomized coverage and mutation checks.
- [Milestone 4 acceptance](milestone-4-acceptance.md): both native adapters,
  compaction, headless ask, offline validation and sanitized live evidence.

## Document status

**Accepted decisions** describe the chosen direction. Change them by recording
the reason and consequences in a decision record.

**Provisionally agreed contracts** establish the six behavioral boundaries and
the architectural constraints in decision 0002. These choices supersede earlier
options such as SQLite or parallel initial tool execution.

**RFC details** specify how to implement those boundaries. Storage details are
accepted and tested under decision 0003; the pure runtime core's turn commits
and async worker/driver under decision 0004; the provider stream seam under
decision 0005; the context builder and report under decision 0006; startup
recovery and tail repair under decision 0007; the pure tool layer and policy
under decision 0009; approval/execution integration under decision 0010;
trusted files under decision 0011; sandbox and host assembly under decision 0012;
and tool recovery under decision 0013. M4 implements native model replay and
switching (0014/0015), transport/credentials (0016), both native adapters
(0017/0018), compaction generation/runtime (0019) and headless ask (0020).
Schema/examples alone are not evidence of implementation.

**Implementation status** is recorded in the README and roadmap. A contract is
validated only when its behavioral requirements have executable tests; live
provider compatibility needs additional opt-in verification.
