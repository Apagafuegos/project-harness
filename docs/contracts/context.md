# Contract 5: Context Management

Status: Implemented v0.1 boundary. Accepted ADRs below refine the original
[RFC-001](../rfcs/001-core-types-state-persistence.md) design.
Projection of recorded history, the report, budgets and the inspection API are
implemented under [decision 0006](../decisions/0006-context-builder.md). Summary
generation, preview/commit, automatic triggering, whole-turn pins/exclusions and
the three compaction strategies are implemented under
[decisions 0019](../decisions/0019-compaction.md),
[0025](../decisions/0025-context-controls.md) and
[0026](../decisions/0026-compaction-strategies.md); see [context](../context.md)
and [Milestone 6 acceptance](../milestone-6-acceptance.md).

## Inputs and output

The builder consumes an immutable session snapshot/head, model capabilities,
instructions, tool definitions, token budget, recorded summaries, and policy.
It returns a provider-independent request and a construction report. It cannot
append history, execute tools, or make a new inference as part of projection.
Provider-native serialization remains the adapter's responsibility.

The report exposes active branch/head, included/excluded entries, reasons,
summary provenance, token estimates/estimator version, reserves, portability
diagnostics, configuration identity, effective whole-turn controls, and the
recorded undo notes on the branch (`undos`, omitted when there are none). Actual provider usage remains separately
labeled and does not pretend to measure a request not sent yet. The engine
inspection API is part of the first headless slice; the shipped Ratatui inspector
uses that same headless API.

### Recorded undo notes

A recorded undo is projected as one `ContextItem::User` block carrying the
note's entry id; it is a user action, never executor or model output, and never
a new turn. Its text names paths and call identities only: no pre-image bytes,
artifact reference or command text ever reaches the projection or an export.
Undo notes are never covered by a compaction: the latest one is a coverage
barrier, so a new span starts after it and no pre-existing summary is
superseded to reach back over it. `PROJECTION_VERSION` (6) and
`BUILDER_VERSION` (7) record these inclusion and report changes.

## Compaction

Three strategies use one selection/preview/commit path: model-based
`rolling_summary`, deterministic model-free `observation_masking`, and
`structured_state` (bounded deterministic extracts plus model narrative).
New clients record `observation_masking`; decoding an omitted legacy strategy
still means `rolling_summary`. Continuations retain the recorded strategy;
explicit differing creation flags/config defaults are refused on continuation.
An idle typed `SetCompactionStrategy` appends a configuration change and stales
previews; no CLI/TUI switching shortcut exists.

Selection retains at least two recent completed turns, skips whole pinned and
excluded turns, and stops at non-completed/unfinished barriers. Pins remain
verbatim and exclusions never reach summarization. Automatic compaction is a
separate recorded policy (clients enable it with `summary_model`), not implied
by the masking default. It runs at the 85% usable-input threshold after a turn,
or while idle before a prospective start overflows; never within a turn or as
a reopen/failure retry. Each trigger has one attempt. Model-based strategies
require an independent summary model; masking needs none. Manual generation
is preview-first and commitment is explicit. Source/head/sequence/model/policy
changes refuse stale previews, rather than regenerating silently.

Each committed summary identifies exact covered ancestors, strategy/version,
generation inputs, and superseded summaries. Originals remain in the JSONL tree.
Model-based generation is a separate asynchronous operation; masking is a
pure deterministic rendering. Reusing recorded results makes projection
repeatable. A preview reports the before/after projection and must be rejected
if the source head/configuration changes before commitment.

## Guarantees

- Only selected ancestry becomes dialogue; sibling history stays inspectable.
- Context and history are separate objects. Compaction never deletes originals.
- Tool proposal/result groups remain valid and complete; unfinished operations
  cannot be absorbed by a summary or bypassed through another branch.
- Enforce the configured budget with explicit output/tool headroom and clearly
  labeled estimates; unknown limits or irreducible overflow produce errors.
- Native state remains owner-scoped. Switching models converts, explicitly omits,
  or rejects incompatible content; it never silently treats it as portable.
- Every inclusion, omission, or replacement has inspectable provenance.
- Summary quality is inherently lossy and must be evaluated; an interface cannot
  guarantee that an LLM retained every important requirement.

RFC cases C01–C02 specify reproducibility, exact source spans, branch isolation,
tool grouping, stale previews, retained recent turns, pins, and budget failures.
`crates/core/src/context/tests.rs`, `compaction_tests.rs` and
`runtime/tests/compaction.rs` exercise those gates; client evidence is in
`crates/cli/tests/tui_inspector.rs` and `tui_controls.rs`.

[ADR 0015's M7 amendment](../decisions/0015-switching-model-metadata.md)
implements projection-only ConvertSupported, with per-part conversion reports
and fingerprints over converted text. Opaque state is never textualized and
originals return for a capable model. Token budgets remain labelled heuristics,
not exact provider tokenizer counts. Masking can increase estimated context,
all summaries are lossy, and the bounded [experiments](../context-experiments.md)
do not establish a general quality ranking or any filesystem rollback.
