# Documentation

v0.2.0 is the current published release, with Milestones 1–8 implemented; the
`CHANGELOG.md` entry for v0.2.1 is prepared and unreleased. Start with the
[user guide](user-guide.md) for
installation and daily use. A design document alone is not evidence of
implementation; acceptance documents separate behavioral tests, historical live
runs and remaining partials.

Key current documents:

- [User guide](user-guide.md): installing a prebuilt binary, first turn, config,
  instructions, approvals, keys, sessions, undo, toolchains, context and
  maintenance.
- [M8 acceptance](milestone-8-acceptance.md): decisions 1–16, the daily-driver
  features, their tests, offline/S02 evidence, the dogfood baseline and rerun,
  and v0.2.0 readiness with its partials and deferred item.
- [M7 acceptance](milestone-7-acceptance.md): decisions 1–17, the v0.1 features,
  full offline/S02 evidence, release dry runs, audit and the release record.
- [Security audit](security/v0.1-audit.md): accepted findings, fixes and residual risks.
- [User config](decisions/0028-user-config.md): strict user-only TOML precedence
  and read-only auto-approval scope.
- [Release pipeline](decisions/0029-release-pipeline.md): public dual-target musl
  archives, dependency notices, mandatory guide and non-publishing dry runs.
- [Session maintenance](decisions/0030-session-export-deletion-and-gc.md): verified
  exports, journal-only deletion and GC publication/reuse locking.
- [Search tools](decisions/0031-search-and-navigation-tools.md): trusted
  in-process `list`/`glob`/`grep`, walk rules, limits and read-only policy.
- [Undo](decisions/0032-undo-agent-edits.md): turn-scoped, all-or-nothing undo of
  trusted `write`/`edit` effects and record version 4.
- [Sandbox toolchains](decisions/0033-sandbox-toolchains.md): opt-in read-only
  toolchain mounts, a fixed sandbox environment, masking and the offline Rust
  recipe.
- [Dogfood baseline](dogfood/m8-baseline.md) and [rerun](dogfood/m8-rerun.md):
  the same five tasks on v0.1.0 and on v0.2.0 as a user would configure it, with
  sanitized per-cell evidence under `docs/dogfood/`.

The full index:

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
- [Shared wiring and session catalog](decisions/0021-shared-wiring-session-catalog.md):
  provider, policy and instruction assembly shared by both clients, plus the
  read-only `DataRoot::sessions()` catalog.
- [Recorded instructions](decisions/0022-recorded-model-instructions.md): the
  built-in, user, root and bounded nested `AGENTS.md` instructions, recorded once
  at creation (M7 amendment).
- [TUI architecture](decisions/0023-tui-architecture.md): the pure model, the
  `Action` boundary, the terminal lifecycle, key map and navigation.
- [DeepSeek profile](decisions/0024-deepseek-profile.md): the second Messages
  endpoint profile, its credential, catalog and native-state owner.
- [Context controls](decisions/0025-context-controls.md): whole-turn pins and
  exclusions, record version 3 and branch-scoped replay.
- [Compaction strategies](decisions/0026-compaction-strategies.md): the strategy
  registry, masking and structured state, and the chosen new-session default.
- [Evaluation harness](decisions/0027-evaluation-harness.md): the dev-only
  offline pipeline, fixed tasks, provenance and opt-in live mode.
- [Search tools](decisions/0031-search-and-navigation-tools.md): the M8
  `list`/`glob`/`grep` tools, walk rules, limits and policy.
- [Undo of agent edits](decisions/0032-undo-agent-edits.md): the M8 undo records,
  pre-images, surfaces, recovery and record version 4.
- [Sandbox toolchains](decisions/0033-sandbox-toolchains.md): the M8 `[sandbox]`
  table, mounts, masking, writable state and the host recipe.
- [Providers](providers.md): implemented adapters, router, credentials and opt-in checks.
- [Built-in tools usage](tools.md): implemented file/shell/search backends, host
  assembly and opt-in toolchains.
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
- [Milestone 5 acceptance](milestone-5-acceptance.md): the Ratatui client's
  rendering, navigation, streaming, cancellation, approvals and inspector, the
  terminal-lifecycle and lag-coherence evidence, and the client/engine boundary
  guard.
- [Context experiments](context-experiments.md): both bounded DeepSeek Flash
  matrices, every raw value and limit, and the resolved default.
- [Milestone 6 acceptance](milestone-6-acceptance.md): pins/exclusions, schema
  v3, strategies, DeepSeek, the offline evaluation harness, the chosen default
  and the live evidence, with partial rows stated.
- [Milestone 7 acceptance](milestone-7-acceptance.md): configuration,
  instructions, conversion, maintenance, TUI polish, release tooling, the audit
  and the published v0.1.0 release record.
- [Milestone 8 acceptance](milestone-8-acceptance.md): search tools, undo and
  record version 4, sandbox toolchains, read-only auto-approval, the dogfood
  baseline and rerun, and the v0.2.0 readiness record with its partials.

## Document status

**Accepted decisions** describe the chosen direction. Change them by recording
the reason and consequences in a decision record.

**Implemented contracts** describe the six v0.1 behavioral boundaries and the
architectural constraints in decision 0002. These choices supersede earlier
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
(0017/0018), compaction generation/runtime (0019) and headless ask (0020). M5
implements the shared wiring and read-only session catalog (0021), recorded
instructions (0022) and the Ratatui client with its terminal lifecycle (0023).
M6 implements the DeepSeek profile (0024), whole-turn context controls and
record version 3 (0025), compaction strategy selection (0026) and the offline
evaluation harness (0027); the live matrices and the user's chosen
default are recorded in [context experiments](context-experiments.md).
M7 implements projection-only conversion (0015 amendment), user TOML (0028),
user/nested instructions (0022 amendment), TUI UX/GFM/highlighting (0023 Parts
6–7), public release tooling (0029) and session maintenance (0030). The accepted
security audit baseline is `5b89aa2f0f3b038b73993d4c1d137da494c6027f`.
M8 implements trusted search tools (0031), durable undo of trusted file edits
and record version 4 (0032), and opt-in read-only sandbox toolchains (0033).
Workspace version is 0.2.0, published as the public
[`v0.2.0` release](https://github.com/Apagafuegos/project-harness/releases/tag/v0.2.0)
with both musl archives and their checksums. A v0.2.0 binary reads v1–v4
journals and writes v4, which v0.1.0 refuses. The
[`v0.1.0` release](https://github.com/Apagafuegos/project-harness/releases/tag/v0.1.0)
remains available.
Schema/examples alone are not evidence of implementation.

**Implementation status** is recorded in the README and roadmap. A contract is
validated only when its behavioral requirements have executable tests; live
provider compatibility needs additional opt-in verification.
