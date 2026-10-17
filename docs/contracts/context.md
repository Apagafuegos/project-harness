# Contract 5: Context Management

Status: Independent context projection, rolling summaries, automatic/manual
compaction, and headless observability are provisionally agreed. Concrete types
and algorithms are proposed in [RFC-001](../rfcs/001-core-types-state-persistence.md).

## Inputs and output

The builder consumes an immutable session snapshot/head, model capabilities,
instructions, tool definitions, token budget, recorded summaries, and policy.
It returns a provider-independent request and a construction report. It cannot
append history, execute tools, or make a new inference as part of projection.
Provider-native serialization remains the adapter's responsibility.

The report exposes active branch/head, included/excluded entries, reasons,
summary provenance, token estimates/estimator version, reserves, portability
diagnostics, and configuration identity. Actual provider usage remains separately
labeled and does not pretend to measure a request not sent yet. The engine
inspection API is part of the first headless slice; the Ratatui inspector comes later.

## Compaction

Start with rolling summarization of an older completed span while retaining
recent history. Both automatic overflow protection and user-requested compaction
are required. A strategy interface separates selection/generation from projection.

Each committed summary identifies exact covered ancestors, strategy/version,
generation inputs, and superseded summaries. Originals remain in the JSONL tree.
Summary generation is a separate asynchronous operation; reusing its recorded
result makes projection repeatable. A preview reports the before/after projection
and must be rejected if the source head/configuration changes before commitment.

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
