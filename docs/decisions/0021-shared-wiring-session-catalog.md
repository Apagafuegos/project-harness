# 0021: Shared client wiring and a read-only session catalog

Status: Accepted and implemented for Milestone 5 step 01.

Date: 2026-10-31

## Decision

`harness::wiring` owns provider/router construction, sourced model catalogs,
authentication-profile owners, new-session model defaults, data-root defaults,
session configuration, policy matching, create/open, `ToolHostConfig`, and the
explicit continuation switch. `SessionOptions` has no prompt, terminal, rendering
or approval-loop state. `open_session` accepts `Arc<dyn ModelProvider>`; offline
clients/tests inject `ScriptedProvider`. `Providers` is the optional real-adapter
factory: credential failures of unselected instances are skipped, while the
selected failure has the same sanitized classification and Codex login hint.
No adapter message or credential enters configuration or diagnostics.

`ask` retains prompt/I/O handling, recovery observation, switch-report rendering,
approval presentation and the streaming turn loop. Existing flags, help, output,
exit classifications and switch semantics are unchanged, except for the required
root-AGENTS warnings. Both clients can call shared assembly without copying it.
There is no terminal UI, transport, new crate or dependency in this step.

`DataRoot::sessions()` is the catalog API: discovery belongs to the existing
private descriptor-rooted storage capability, not another index or database.
It returns serializable `SessionCatalogEntry` values. Enumerating the pinned
`sessions` descriptor ignores names other than valid UUIDv7 stems plus `.jsonl`.
Every matching name produces an entry, including unsafe/inaccessible files.
Entries are inspected using the existing journal scanner under a nonblocking
**shared** descriptor lock. The lock stays held while timestamps are read from
the validated prefix; neither an exclusive lock, repair nor writable open occurs.
`scan` gains only sibling-module visibility; existing storage types/formats and
validation are unchanged.

The status is `ok`, `busy`, `needs_repair` (only a torn tail after a valid header),
or `unreadable` (including corruption, unsupported versions, unsafe files and
I/O errors), with a short content-free reason. Metadata comes from the validated
prefix only. Busy/headerless entries have null metadata. Creation is the header
record time, activity is the last committed record time, not filesystem mtime.
Model is derived at the durable current head and state is the durable runtime
state; listing performs no runtime recovery. The title is the first user message
in journal order, across branches: concatenate its text blocks, take the first
line, then at most **120 Unicode scalar values**, without an added ellipsis.
No user messages means null; a first message without text has an empty title.

Order is last-activity descending (parsed instants), then session ID ascending;
unknown activity sorts last. Discovery failure for the directory itself fails
the listing; a single broken session never does. There is no cache. Work is
linear in total journal size for v0.1, plus existing replay/artifact-validation
costs and a bounded-per-record timestamp pass.

`harness sessions [--data-root DIR]` prints a pretty JSON array, uses the same
XDG_DATA_HOME/HOME default as ask, and never creates storage. A missing root is
an empty catalog; an unsafe root is an error. This does not change the older
`DataRoot::default_path()` helper or positional arguments of inspection commands.

## Evidence and consequences

Core catalog tests cover empty/stray files, committed order and Unicode titles,
model/state, live writers, coexistence with another shared reader, torn/corrupt/
unsupported/unsafe files, and byte-for-byte preservation of the complete data
root. CLI integration tests cover JSON, busy status, missing roots, usage and
context instruction accounting. Existing ask assertions are retained unchanged.
A busy entry cannot show a live writer's metadata; a future UI can use its own
live handle view for that session, without weakening catalog locking.
