# Roadmap

The scaffold, specification artifacts and first headless storage milestone exist.
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

## 2. Headless mock lifecycle and context inspection — in progress

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
Remaining: startup recovery (step 6).

Step 6 also adds torn-tail repair as an explicit `SessionHandle` command. Today
`reopen()` returns the torn-tail diagnostic unchanged, so such a session is
unreachable through the handle. The command must be bound to the fingerprint and
size of the inspected journal, as `JournalStore::repair_tail` already is, and
refuse a stale inspection. It is never run implicitly. Unfinished-turn recovery
and torn-tail repair then share one inspect-then-confirm recovery flow.

Add the public provider stream lifecycle and a deterministic scripted provider.
Implement initial branch projection and the inspection API before Ratatui.
Prove context/report reproducibility, ordered events, usage, partial-response
handling, bounded buffering, and no execution from incomplete calls.

Use M01–M04, C01, R02, and E01 with mocks/fixtures, without keys or network.
Native provider differences are represented in types rather than erased.

## 3. Tools, selective security, and recovery

Implement registry validation, sequential scheduling, policy, approvals, and
opaque authorized operations. Add trusted `read`, `write`, and `edit` with
secure descriptor-relative access and safe replacement semantics. Persist intent
before side effects, results before inference, and recover uncertainty explicitly.

Add `bash` only behind the configured Bubblewrap backend with tested mounts,
network denial, environment filtering, deadlines, bounded output, and descendant
cleanup. Missing enforcement must fail closed. Run T01–T02, S01–S02, and R01;
actual Linux security tests are a gate, not an optional successful skip.

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
