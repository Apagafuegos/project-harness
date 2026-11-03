# 0030: Session export, deletion and artifact collection

Status: Accepted and implemented for Milestone 7 step 04.

Date: 2026-11-03

## Context

Artifacts are content-addressed files shared by every session in one private data
root (`<data_root>/artifacts/<sha256>`). A writer publishes an artifact, syncs it
and only then appends the journal commit that references it, so between those two
steps the bytes exist while no journal mentions them. Nothing in the journal
records ownership, because the same bytes are deliberately reused by dedupe.

Milestone 7 asks for session export and deletion (decision 9) and for an explicit
artifact collection that never races another session's writes (decision 16). The
two must be separate commands: deletion that also removed artifacts could delete
bytes another session's commit is about to reference, and no journal could prove
otherwise after the fact.

## Decision

### Markdown export is a read-only projection

`DataRoot::export_markdown` holds the same non-blocking shared lock the catalog
uses for the whole read, validates the journal with the existing scanner, and
renders from the validated snapshot. A diagnostic refuses the export
(`journal_invalid`) instead of exporting a prefix, because a truncated transcript
would look complete. The renderer is deterministic: the same journal always
produces the same bytes, and no map iteration, wall clock or path leaks into the
output.

`SessionSnapshot::created_at` carries the header record's timestamp as derived
state (never persisted in a new field), so a live snapshot renders the same
header as a journal read.

Text is verbatim; tool calls are fenced JSON naming the tool; tool results carry
status, code, exit code, duration, truncation and full content; media becomes a
reference line with type, bytes and a digest prefix; opaque provider state is
omitted with a note. Fences grow past any run of backticks in the content. The
`--all-branches` layout walks the entry tree in journal order, emits each entry
once, and labels the run by the set of branch heads that still share it
(`## Shared history`, `## Branch <head>`, `## Branches <a>, <b>`).

### Journal export is a new data root

`export --format journal` copies the verified journal and every artifact it
references into a directory that must not exist, verifying each artifact before
and while copying. The tree is created with the private modes `DataRoot` requires
(0700 directories, 0600 files), so the result opens as a data root and replays to
the same head. A failure removes exactly the tree it created, entry by entry
through the descriptors it holds, and never follows a link.

### Deletion removes one journal and nothing else

`delete` takes the exclusive writer lock non-blocking (so a running engine is
refused as `session_locked`), reads the title for its confirmation question under
the shared lock, and then unlinks `sessions/<id>.jsonl` from the pinned directory
descriptor and syncs it. There is no separate lock file in this design, and a
lock file would be removed with the journal if one existed. Deletion is
irreversible and refuses a missing ID with `session_not_found`.

### Collection is explicit, journal-verified and delayed

`gc` reads every journal under a non-blocking shared lock. If any journal cannot
be locked or validated it refuses the whole run with `gc_journal_unreadable:
<id>`; a damaged journal might reference anything, so a partial reference set is
never good enough. The reference set mirrors storage validation exactly: the
header configuration, tool-started and configuration-changed artifacts, plan
artifacts, quarantine artifacts, media and provider-state blocks, continuation
artifacts, tool-result metadata artifacts, compaction manifests and the bare
command and configuration fingerprints.

An artifact is removed only if no readable journal references it **and** its
modification time is at least 24 hours old. Candidates are stat'ed without
following a symlink and removed only when they are private regular files, then
the artifacts directory is synced.

### A dedupe hit refreshes the modification time

`DataRoot::put_artifact*` now calls `utimensat(..., UTIME_NOW,
AT_SYMLINK_NOFOLLOW)` after accepting an existing artifact. A reuse therefore
looks exactly as young as a fresh write, which is what the grace window needs.

### The TUI only names the work

The sessions screen gains `d` and `e`. The pure model emits
`Action::DeleteSession` and `Action::ExportSession`, opens its own y/N and path
dialogs in the m7-05 frame, and touches no storage; the shell performs the work
under the same serialization permit as commands and handover, and reports the
written path or the refusal code. Deleting the session this client owns is
refused twice: the dialog says so before offering a choice, and the shell rejects
the action if it arrives anyway, because an engine cannot remove the journal it is
writing. Exporting the current session reads the client's own durable snapshot,
since its own writer lock excludes the shared read; every other session is read
from its journal. Both keys are in the generated key reference.

## Race analysis

Decision 16 names the window this design has to survive: another session writes
an artifact, and commits a journal that references it, some time later.

| Situation | Outcome |
| --- | --- |
| Writer publishes bytes, has not committed yet | No journal references them, but their mtime is seconds old, so the 24-hour window keeps them. |
| Writer dedupe-hits an existing artifact, then commits | The hit refreshed the mtime, so the artifact is young again; a collection that had already decided it was a candidate still requires the artifact to be older than the window. |
| Writer dedupe-hits an artifact exactly as a collection unlinks it | The writer's `verify_artifact` fails with `artifact_missing_or_invalid` and the commit fails loudly; the engine reports a storage fault rather than committing a record whose bytes are gone. The window is microseconds and no silent partial commit is possible. |
| Collection runs while a journal is locked by its writer | The run refuses with `gc_journal_unreadable: <id>`, so it never computes a reference set from a partially readable journal. |
| Deletion runs while another session's writer holds shared artifacts | Deletion removes only a journal; no artifact is touched, so a shared artifact survives. |
| Deletion runs while the same session's engine is writing | The exclusive writer lock refuses it with `session_locked`; nothing waits. |
| A journal is torn, corrupt or unsupported | Export and collection both refuse; neither repairs, invents entries or removes bytes. |
| A path in `sessions/` or `artifacts/` is a symlink | Every open uses `NOFOLLOW` on pinned directory descriptors; a symlinked journal refuses the collection run, and a symlinked artifact is left alone. |
| A forked child (a shell tool, or the test harness) inherits the journal descriptor between `fork` and `exec` | The descriptor is `CLOEXEC`, so `exec` closes it; during the window a *second process* can transiently observe `session_locked`. Every command refuses rather than waits, so a retry is always safe and nothing is corrupted. |

Deletion and collection never touch anything outside `<data_root>/sessions` and
`<data_root>/artifacts`, and neither changes the journal format: both are
file-level maintenance around an append-only journal.

## Evidence and consequences

`crates/cli/tests/session_maintenance.rs` covers the Markdown golden for the
active branch and for `--all-branches`, determinism, `export_exists`, a journal
export that reopens as a data root and replays to the same head, a corrupt
artifact that fails and cleans up, deletion refusing a lock and requiring
`--yes`, deletion leaving every artifact and then reporting
`session_not_found`, collection keeping referenced and young artifacts and
removing an aged orphan set with `set_modified`, a dry run that changes nothing,
refusal on an unreadable or locked journal, the dedupe mtime refresh, a symlinked
journal and artifact left alone, and byte-for-byte proof that export changes
nothing in the data root. `crates/cli/tests/tui_maintenance.rs` covers the delete
dialog and cancel, the current-session refusal in both layers, `session_locked`
in the banner, the export prompt writing Markdown and then refusing
`export_exists`, and the help coverage of `d` and `e`. `tui/keys.rs` keeps its
generated reference exhaustive.

The client/engine boundary (ADR 0023) is unchanged: the model emits typed
actions, the shell reads and unlinks. Deletion stays journal-only, so a future
per-artifact reference index could refine collection without changing this
contract; nothing here assumes an index exists.
