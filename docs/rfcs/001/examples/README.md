# RFC-001 serialization examples

These JSONL journals use real exact-byte SHA-256 checksums. Storage tests decode,
round-trip and semantically replay all four with their artifacts. They remain
specification fixtures, not proof of an agent runtime or sandbox.

The first three stay v1 and remain valid: readers accept v1 to v3 records, and
nothing rewrites an older record. Appending to one of those journals continues
it with v3 records on the same checksum chain; the storage and runtime tests do
that rather than keeping a mixed example here. The fourth is v3 and exercises
the v3-only entry kind.

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
- [Context controls](context-controls.jsonl): three completed turns, then four
  `context_control` v3 entries: turn 1 pinned, turn 2 excluded, turn 3 pinned
  and then reset to `default`. Every turn stays in history; only projection
  changes, and the control entries are not dialogue.
- [v0.2.0 undo intent](undo-v0.2.0-omitted.jsonl): v4. A completed captured
  write, a newer bash-only turn, and an `undo_intent` written by a v0.2.0
  binary, whose `omitted_calls` lists only the target turn's own calls. v0.2.1
  writers report the newer turn's bash call too, and replay accepts either list
  and no other (decision 17). Opening it appends nothing; the storage tests
  resolve it and continue the same chain.

The [v1](../schema/session-v1.schema.json), [v2](../schema/session-v2.schema.json)
and [v3](../schema/session-v3.schema.json) schemas each check one record
version's envelope and payload shapes.
The RFC adds checksum, role, ancestry, head, state, authorization, and recovery
semantics. JSON Schema alone cannot validate those relationships.

The `artifacts/` directory contains the referenced configuration, normalized
commands, plan, and source manifest, named by the SHA-256 of their exact bytes.
They contain no credentials. Example filenames are descriptive; runtime files
will be named by session ID under the private data root.

Malformed/torn variants should be derived in implementation tests (J01–J03),
not accepted as successful histories. A draft schema/fixture check was run during
authoring; no Python or JavaScript tooling is required by the Rust project.
