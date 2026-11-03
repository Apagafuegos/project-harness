# Contract 4: Session Storage

Status: Tree-based append-only JSONL storage is implemented in `harness-core`
under [decision 0003](../decisions/0003-jsonl-storage.md), with J01–J03 tests.
[RFC-001](../rfcs/001-core-types-state-persistence.md) defines its framing/schema.
The runtime performs recovery commits after open
([decision 0007](../decisions/0007-startup-recovery.md)); sequential tool intent,
results and explicit uncertainty recovery are implemented under
[0010](../decisions/0010-tool-pipeline.md) and
[0013](../decisions/0013-tool-recovery.md).

## Inputs and output

One authoritative JSONL file per session records immutable tree entries and
ordered audit events. A reader reconstructs validated entries, ancestry, active
head, branch-derived model selection, turn state, and unresolved execution.
Artifacts hold large output, media, and protected native state by content hash.
In-memory indexes and caches are rebuildable; a second database is not authority.

Tree entries hold user/assistant content (including tool-call blocks), tool
results, model changes, compactions, and whole-turn context controls. Audit
events hold approvals, execution intent, state transitions, uncertainty, and
explicit recovery. Token-by-token
deltas remain transient in v0.1; completed/partial response snapshots are stored.

## Guarantees

- Entries are immutable and parent-linked; siblings preserve different histories.
- The active head is durably recorded. Navigation does not assume the last
  physically appended entry is selected, and does not undo filesystem effects.
- One writer is enforced by a lock; timestamps are not the ordering key.
- Records are versioned, ordered, uniquely identified, and validated on load.
  Parent/head/call references and state transitions must be valid.
- Important intent is synced before a risky operation begins. A durable result
  precedes its use in subsequent inference.
- Incomplete trailing bytes are detected and reported. Committed corruption is
  never silently skipped or truncated away.
- Recovery is deterministic for the same validated history and never blindly
  repeats an action whose external effects may already have happened.

## Implemented storage design

The store implements one checksummed transaction per JSONL line. Each commit
includes new entries, audit events, and a compare-and-set head update so reconstruction
cannot publish them independently. A header selects schema/session/workspace
identity and initial model/configuration. Schema validation is necessary but
not sufficient for semantic consistency.

The implementation validates exact-byte checksums, file/directory sync ordering,
writer poisoning on I/O failure, explicit quarantined-tail repair, artifact publication,
command deduplication, and persisted execution uncertainty. J01–J03 exercise
these storage behaviors, including injected I/O failures. R01 is covered by the
runtime/driver/tool-host crash tests; the store itself never executes a tool.
Readers accept record versions 1–3; writers emit v3, without rewriting old
records ([ADR 0025](../decisions/0025-context-controls.md)). See the
[usage guide](../session-storage.md) for explicit recovery and platform limits.

Context projection and summarization are a separate [contract 5](context.md).
SQLite is not an alternative initial store under the agreed v0.1 architecture.

## Maintenance and limits

[ADR 0030](../decisions/0030-session-export-deletion-and-gc.md) implements
read-only catalog/Markdown/journal exports under shared journal locks and
irreversible **journal-only** deletion under the exclusive writer lock.
Journal export verifies the actual copied artifact hashes and lengths and
refuses existing destinations. An unsuccessful export may leave an empty new
directory. Text is faithful, not scrubbed; opaque state is omitted from Markdown
but preserved in journal backups. Private storage is not encryption.

Explicit artifact GC validates every journal, refuses the whole run if any
cannot be read/locked/validated, and removes only unreferenced safe artifacts
older than the 24-hour grace window. Dedupe refreshes mtime. An independent
open-description artifact-directory flock serializes publication/reuse with
GC selection/unlink. Deletion never reclaims artifacts directly. See
`crates/cli/tests/session_maintenance.rs` and
`session::tests::artifact_gc::artifact_gc_serializes_selection_and_unlink_with_dedupe_reuse`.

The supported boundary is a local Linux filesystem honoring locks and sync;
network filesystems and hostile same-user mutation are unsupported. Torn-tail
repair remains explicit and fingerprint-bound; there is no complete-corruption
salvage command or automatic retention policy.
