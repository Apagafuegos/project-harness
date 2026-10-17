# RFC-001 serialization examples

These proposed v1 JSONL journals use real exact-byte SHA-256 checksums. They are
specification fixtures, not proof of an implemented engine or sandbox.

- [Branching](branching.jsonl): a completed first exchange followed by two sibling
  user branches. The later turns deliberately end before inference. Navigation
  commits retain all entries; the last selected branch is approach B.
- [Interrupted execution](interrupted-execution.jsonl): a complete read proposal,
  authorization, and durable execution intent without a result. A future loader
  must classify the invocation as uncertain, even though the tool was read-only;
  it cannot assume execution either occurred or never began.
- [Compaction](compaction.jsonl): three completed turns and a rolling summary
  covering the first turn while retaining the two recent turns. Its manifest
  binds exact source entries and checksums. The originals remain in the journal.

The [schema](../schema/session-v1.schema.json) checks envelope and payload shapes.
The RFC adds checksum, role, ancestry, head, state, authorization, and recovery
semantics. JSON Schema alone cannot validate those relationships.

The `artifacts/` directory contains the referenced configuration, normalized
commands, plan, and source manifest, named by the SHA-256 of their exact bytes.
They contain no credentials. Example filenames are descriptive; runtime files
will be named by session ID under the private data root.

Malformed/torn variants should be derived in implementation tests (J01–J03),
not accepted as successful histories. A draft schema/fixture check was run during
authoring; no Python or JavaScript tooling is required by the Rust project.
