# Roadmap

The scaffold, specification artifacts, headless storage (Milestone 1) and the
headless mock lifecycle with context inspection (Milestone 2), and tools,
selective security and recovery (Milestone 3), and native providers, rolling
compaction and headless ask (Milestone 4), the Ratatui terminal client
(Milestone 5), and context controls, strategies, the DeepSeek profile and the
bounded experiments (Milestone 6), and v0.1 documentation/release readiness
(Milestone 7) are implemented within their recorded scope and v0.1.0 is
released. Anthropic live compatibility and live
cross-provider switching remain partial (no key supplied). M7-01's successful
Codex and DeepSeek TUI answer/restoration evidence supersedes the historical
M5 quota limit; it does not claim a live coding/tool workflow.

Sections 0–6 describe each milestone's historical scope; later closure is
linked rather than changing old measurements. See [M7
acceptance](milestone-7-acceptance.md) and the [user guide](user-guide.md) for
the current product.

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
explicit recovery commits and local Linux storage. No corruption-salvage command;
M7 adds verified exports (ADR 0030).

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
M01–M04, C01, R02 and E01 to tests. It has no gap rows. Its original remainders
are now closed by later milestones:

- **M03:** per-operation deadlines, bounded native queues and both native
  adapters are covered by [Milestone 4 acceptance](milestone-4-acceptance.md).
- **M04:** reject/explicit omit switching and owner-scoped native replay through
  reopen are covered by Milestone 4.
- **R02:** the original approval/execution cancellation remainder is now closed
  by [Milestone 3 acceptance](milestone-3-acceptance.md).

## 3. Tools, selective security, and recovery — complete

Implemented registry validation, sequential scheduling, policy, approvals and
opaque authorized operations. Trusted `read`, `write` and `edit` use strict
secure descriptor-relative access and safe replacement. Intent is acknowledged
before side effects, results before dependent inference, and uncertain mutations
require user resolution; read-only restart closure follows D9.

`bash` is offered only with the configured, probed Bubblewrap backend and a
usable file planning layer. Real Linux tests cover mounts, network denial,
environment filtering, deadlines, bounded output and descendant cleanup.
Missing enforcement fails closed. S02 and assembled tool-host tests are required
in both pinned CI jobs, never a successful skip on an unavailable prerequisite.

Schema v2 was implemented in M3 and is now v3 since M6 ([session
storage](session-storage.md#record-versions)); v1 and v2 records remain readable
and journals continue as v3. The fixed registry,
canonical plans and policy, pure-core/async pipeline, descriptor-rooted files,
sandbox and session-owned assembly, and recovery advice are recorded in
ADRs [0008](decisions/0008-host-sandbox.md),
[0009](decisions/0009-tool-policy.md), [0010](decisions/0010-tool-pipeline.md),
[0011](decisions/0011-trusted-file-tools.md),
[0012](decisions/0012-sandbox-backend.md) and
[0013](decisions/0013-tool-recovery.md).

[Milestone 3 acceptance](milestone-3-acceptance.md) maps every criterion,
T01–T02, S01–S02, R01, full R02 and J03's output-artifact clause to gate tests.
`cargo run --example tool_session` demonstrates real files, approvals, crash,
uncertainty, advice, user resolution and final context membership offline.
There are no gap rows; recorded partial capabilities/limits are:

- **Elevated:** not offered in M3; future capability decision (ADR 0009).
- **Subprocess network:** always denied; constrained egress needs a later backend.
- **Seccomp:** no filter (ADRs 0008/0012).
- **Process count:** TasksMax only with a usable systemd user scope (ADR 0008).
- **Concurrent lock/spawn:** fork/exec can briefly cause session_locked on another
  journal's immediate release/reopen (ADR 0012 amendment); no bypass or effect retry.

Additional filesystem/sandbox threat-model limits are recorded in the acceptance
notes and owning ADRs. Native providers and headless ask were implemented in M4;
the Ratatui client in M5.

## 4. Native providers and rolling compaction — complete

Implemented OpenAI Responses first against the official reference and Codex
source, with native fixtures, a sanitized recording and separate opt-in live
checks. Native Anthropic Messages uses the same seam with no provider-specific
orchestration branch or core change. Both adapters prove portable diagnostics,
owner-scoped opaque-state retention and safe reject/explicit-omit switching
inside durable sessions through the Router.

Automatic and manual rolling summaries use `CompactionStrategy`, an independent
summary model, read-only previews, exact source manifests, retained recent turns
and inspection diffs. C02, stale binding checks and explicit budget failures
preserve original history. Headless `harness ask` streams one turn and handles
terminal approvals. Ordinary validation is offline after dependency/toolchain
provisioning; live checks are separate, explicit and never part of CI.

[Milestone 4 acceptance](milestone-4-acceptance.md) maps every criterion and
M01–M04/C01–C02 to gate tests and records live evidence. The deterministic offline
`cargo run --example provider_session` tour uses both real adapters and is diffed
twice in the gate. Known partials/limits:

- Anthropic live checks and cross-provider live switching need
  `ANTHROPIC_API_KEY`; fixture coverage is complete, not live compatibility proof.
- M4 refused `ConvertSupported`; **closed in M7** by ADR 0015's projection-only
  conversion amendment. Neither native adapter emits response-level continuation
  (ADR 0014).
- Pins did not exist in M4 (C02, ADR 0019). Closed by Milestone 6: whole-turn
  pins/exclusions are implemented and mapped in
  [Milestone 6 acceptance](milestone-6-acceptance.md).
- Estimates are heuristic; provider-specific feature limits and the Codex
  backend-selected output budget are recorded in ADRs 0017/0018.
- Preserved-thinking transformations are requested but not parsed/reported
  (ADR 0018 amendment); a replay can be dropped by the API after prefix edits.

## 5. Ratatui client — complete

Plain `harness` opens a terminal client in the current directory. It is a pure
model plus a thin asynchronous shell over the existing headless engine: the
client renders, navigates, streams, cancels, asks for approvals and inspects
context, and the engine still builds every request, authorizes every call and
runs every tool. Rendering is a width-independent projection of the published
snapshot plus the live partial response. M5 shipped minimal Markdown; M7 adds
GFM and bounded bundled syntax highlighting (ADR 0023 Parts 6–7). Cancellation
and quit are durable: the terminal is restored first, then the turn is cancelled
and storage is drained.

Navigation covers sessions (Ctrl-O), the branch tree (Ctrl-T) and models
(Ctrl-L) over the read-only session catalog and the merged, sourced model
catalog. Handover is idle-only, releases the old writer, retries a raced
`session_locked` once and restores the previous session when an open fails. A
branch edit pre-selects the message's parent and starts a sibling; originals are
never rewritten. Model switches are reject-first, show the classified
portability report, and omit optional state only after an explicit confirmation.

The context inspector (Ctrl-K) is read-only: budget estimates, included and
excluded items, fingerprints and configuration identity, plus model previews
that never switch. Manual compaction is preview-first — Enter commits only the
exact rendered preview, Esc writes nothing — and automatic compaction appears in
the transcript and refreshes an open report.

Terminal state is restored on quit, panics, SIGTERM/SIGHUP and any error the
loop returns; a non-TTY is refused before raw mode is entered. Subscriber lag,
epoch changes and sequence gaps all arrive as `Reset` and replace every panel
coherently, including mid-stream.

`crates/cli/tests/tui_boundary.rs` guards the boundary in the gate: no provider
request building, no tool authorization or execution, no engine workspace or
sandbox type, and no approval decision outside the modal key handler. The shell
(`tui_cli.rs`) may use `wiring`; the model and loop may not.

[Milestone 5 acceptance](milestone-5-acceptance.md) maps every criterion and the
settled decisions to gate tests and records sanitized live evidence. Known
partials/limits:

- The original M5 live smoke hit `rate_limit`; **closed for answer/restoration**
  by M7-01's Codex/DeepSeek passes in M5's Evidence log. Coding/tools remain
  end-to-end offline evidence, not a live coding-workflow claim.
- Anthropic live checks and cross-provider live switching still need
  `ANTHROPIC_API_KEY` (ADR 0018, ADR 0020 amendment).
- M5's flags-only/root-only instructions are **superseded in M7** by user TOML
  (ADR 0028) and recorded user/nested instructions (ADR 0022 amendment).
- M7 adds syntax highlighting and session deletion/export UI (ADRs 0023/0030).
  Workspace switching, automatic retention and credential UI remain unimplemented.
  Torn tails still require `harness journal repair`, never automatic repair.
- Inspector budgets are heuristic estimates, not tokenizer counts. Pins and
  context experiments were M6 and are now implemented; see
  [Milestone 6 acceptance](milestone-6-acceptance.md).

## 6. Context experiments — complete

Implemented whole-turn pins and exclusions as append-only `context_control`
branch entries (record version 3) folded by the projection. A pinned turn stays
verbatim outside compaction coverage; an excluded turn stays in history and the
transcript, is never covered and never reaches a summary model. Compaction skips
controlled turns, and a span that cannot fit fails explicitly instead of being
truncated. Three strategies share one registry — `rolling_summary`, the
model-free `observation_masking` and `structured_state` — selected at creation
and switchable only through a recorded idle `ConfigurationChanged`. A compaction
records its strategy and version and omits `model` when it used none
([ADR 0024](decisions/0024-deepseek-profile.md),
[0025](decisions/0025-context-controls.md),
[0026](decisions/0026-compaction-strategies.md)).

`crates/eval` is a dev-only offline harness over four fixed coding tasks:
requirement retention (from the projection and a probe turn), modified-file
recall and completion after one compaction, with recorded configuration/input
provenance ([ADR 0027](decisions/0027-evaluation-harness.md)). It proves
mechanics, not quality, and is required in the gate. Two bounded DeepSeek Flash
matrices are recorded in [context experiments](context-experiments.md): the
first measured 4/12 cells (eight model-based mechanical failures, fixed by the
explicit summary request); the second measured 12/12 with zero mechanical
errors. No arm reduced the estimated projection on this suite. On 2026-11-02 the
user chose `observation_masking` as the new-session default: both clients record
it explicitly, while decoding an omitted strategy still means `rolling_summary`
and a continuation keeps its recorded strategy.

[Milestone 6 acceptance](milestone-6-acceptance.md) maps every §6 item and the
relevant C01/C02/RFC 6.1 clauses to gate tests. Known partials/limits:

- The comparison cannot establish a general quality winner: four prose-heavy
  tasks, one repetition per matrix, lexical matchers and one probe turn.
- Masking is lossy, guarantees neither a shorter projection nor workspace
  rollback, and the default is the user's measured provisional choice.
- Anthropic live compatibility and live cross-provider switching remain partial.
  The live TUI answer/restoration smoke was closed by M7-01 (M5 Evidence log).
- The m6-00 DeepSeek scripted check was initially unrun; M7-01's single passing
  run now covers streaming/tools/usage/thinking replay (M6 Evidence log).

## 7. v0.1 polish and release readiness — implemented, publication pending

[M7 acceptance](milestone-7-acceptance.md) maps all goals and decisions 1–17:

- Successful sanitized Codex/DeepSeek TUI smokes and DeepSeek scripted check;
  Anthropic/live cross-provider partials remain explicit.
- Explicit projection-only `ConvertSupported`, loss reports and restored originals
  on capable switch-back ([ADR 0015](decisions/0015-switching-model-metadata.md)).
- Strict user-only TOML defaults, path/show/no-config, ask-only settings and
  creation-time bounded user/nested instructions
  ([ADR 0028](decisions/0028-user-config.md),
  [0022 amendment](decisions/0022-recorded-model-instructions.md)).
- Verified Markdown/journal exports, journal-only delete and explicit 24-hour
  artifact GC with publication/dedupe locking
  ([ADR 0030](decisions/0030-session-export-deletion-and-gc.md)).
- Minimal-chrome TUI UX, wrapping/history/help/paste, activity, scrollable dialogs,
  GFM/highlighting, tool blocks and approval previews (ADR 0023 Parts 6–7).
- Effective MIT OR Apache-2.0 licensing in the public repository; native x86_64
  and ARM64 musl builds, smoke checks, reproducible archives, checksums and
  third-party notices ([ADR 0029](decisions/0029-release-pipeline.md)).
- Accepted [security audit](security/v0.1-audit.md), all four medium findings fixed
  with regressions; no claim of vulnerability absence. Residual risks are retained.
- Reconciled six contracts, [user guide](user-guide.md), changelog and the
  required-guide packaging/refusal regression. No runtime or journal-format
  change in step 09.

The two-target release dry run already passed; it is not a tagged release.
Final exact-head offline/S02 CI is required for this documentation step. Release
workflow dispatch/tag/asset verification and publication belong to the orchestrator.

## Outside v0.1

Subagents, MCP client, plugin runtime, semantic memory, remote workspaces,
multi-agent orchestration, separate server/JSON-RPC, and filesystem checkpoints.
Restricted subprocess networking also needs an actual constrained-egress backend
before the proposed deny-network-only implementation can grant exceptions.
No automatic retention, workspace switching or credential UI is provided.
Licensing is settled and effective: MIT OR Apache-2.0.
