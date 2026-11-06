# Session storage

`harness_core::session` implements the first headless storage milestone on Linux.
It stores one checksummed JSONL journal per session and content-addressed artifact
sidecars. It does not run an agent, project context or execute workspace tools.
The runtime, the context builder and the `harness journal` commands build on it.

## Rust API

Use an explicit private root whose parent already exists, outside the workspace.
`DataRoot::default_path()` computes the XDG default without creating it. Opening
an existing root checks its ownership, permissions and symlink-free traversal.

```rust
use std::sync::Arc;
use harness_core::session::*;
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let temporary_parent = tempfile::tempdir()?;
# let private_root = temporary_parent.path().join("state");
// In an application, private_root is an explicitly chosen absolute path.
let root = Arc::new(DataRoot::open(private_root, true)?);
let configuration = root.put_artifact(br#"{"policy_revision":"initial"}"#)?;
let session_id = SessionId::new();
let mut store = JournalStore::create(root.clone(), session_id.clone(), SessionHeader {
    workspace: "/workspace".into(),
    initial_model: ModelRef {
        provider_instance: "mock-default".into(),
        model: "scripted".into(),
    },
    policy_revision: "initial".into(),
    configuration,
}, Timestamp::now())?;

// A head change and its entries/events are one durable transaction.
let entry_id = EntryId::new();
let before = store.snapshot();
let mut transaction = Commit {
    command_id: None,
    command_fingerprint: None,
    head: HeadChange {
        from: before.head().cloned(),
        to: Some(entry_id.clone()),
        reason: HeadReason::Advance,
    },
    entries: vec![Entry {
        id: entry_id.clone(),
        parent_id: before.head().cloned(),
        turn_id: None,
        payload: EntryPayload::ModelChanged(ModelChanged {
            model: ModelRef { provider_instance: "mock-default".into(), model: "other".into() },
            portability: Portability::Reject,
        }),
    }],
    events: vec![],
};
let command_id = CommandId::new();
store.bind_command(&mut transaction, command_id.clone(),
    &serde_json::json!({"kind":"switch_model","model":"other"}))?;
let acknowledgement = store.append(transaction.clone(), Timestamp::now())?;
assert_eq!(before.entries().len(), 0); // old snapshots remain immutable
assert_eq!(store.snapshot().entries().len(), 1);
assert!(store.snapshot().entry(&entry_id).is_some());
assert_eq!(store.append(transaction, Timestamp::now())?, acknowledgement);
drop(store); // releases the exclusive journal-descriptor lock

let reopened = JournalStore::open(root, &session_id)?;
assert_eq!(reopened.snapshot().sequence(), acknowledgement.sequence);
assert!(reopened.snapshot().command_receipt(&command_id).is_some());
# Ok(())
# }
```

`advance` appends a single chain starting at the selected head. `navigate` appends
no entries and selects an existing safe conversation boundary (or the null root)
while idle. `retain` appends only audit events. `head.from` is compare-and-set.
Navigation preserves sibling entries and never changes workspace files or the
session-wide policy. `ancestry(head)` returns entries in conversation order;
`model_at(head)` selects the nearest model change on that branch.

Snapshots are cheap to clone and share unchanged history across commits.
`entry(id)` and `turn(id)` look up individual records; `entries()` and `turns()`
iterate in identity order, while `events()` iterates in journal order. All three
iterators report their exact remaining length and support reverse iteration.

Wire structs are construction inputs; schema validation and semantic replay run
before every append. User entries require a matching `turn_started` and
`idle -> preparing` transition. Assistant entries belong to the active inference;
complete tool groups require ordered associated results. Persisted audit types
are available now so the later runtime can use the same representation.

## Read-only session catalog

`DataRoot::sessions()` discovers every `sessions/<UUIDv7>.jsonl` name through the
pinned directory descriptor; other names are ignored. It returns serializable
entries containing `session_id`, `workspace`, `created_at`, `last_activity`,
`model`, durable `state`, `title`, `status` and `reason`. Model is evaluated at
the current head; activity is the last committed record timestamp, not mtime.
The title is the first user message's concatenated text, first line, bounded to
120 Unicode scalar values (no ellipsis); no user messages means null.

Inspection takes only a nonblocking shared lock, never writes or repairs, and
never silently drops a matching file. Status is `ok`, `busy` (writer lock),
`needs_repair` (torn tail after a valid header), or `unreadable` (corrupt,
unsupported, unsafe or I/O failure), with a short reason. Available metadata
comes only from the validated prefix; busy/headerless entries have null
metadata. Order is activity descending, session ID ascending for ties, unknown
activity last. Work is linear in total journal size plus existing validation.
See [decision 0021](decisions/0021-shared-wiring-session-catalog.md).

```sh
harness sessions [--data-root DIR]
```

The command prints a pretty JSON array. Its default root matches `ask`:
`$XDG_DATA_HOME/harness`, otherwise `$HOME/.local/share/harness`. It creates
nothing; a missing root prints `[]`, an unsafe root fails. Use the explicit
journal commands below for repair; catalog discovery never initiates recovery.

## Export, deletion and artifact collection

```sh
harness sessions [list] [--data-root DIR]
harness sessions export <ID> --format markdown|journal --output PATH [--all-branches]
harness sessions delete <ID> [--yes]
harness sessions gc [--dry-run] [--yes]
```

The bare command and `list` print the read-only catalog above, byte for byte.
Every subcommand takes the same `--data-root`, and falls back to the user
config's `data_root` and then the `ask` default when the flag is absent. Exit
status is 0 on success, 2 for a usage error, refusal or a confirmation that
cannot be asked for, and 1 for a storage failure. A refusal prints one stable
code on stderr: `session_not_found`, `session_locked`, `confirmation_required`,
`export_exists`, `export_artifact_invalid: <sha256>`, `journal_invalid: <code>`,
`gc_journal_unreadable: <session-id>` or `invalid_output_path`.

### Markdown export

`export --format markdown` is read-only: it holds the same non-blocking shared
lock as the catalog for the whole read, refuses a session a running engine hold
as `session_locked`, and never repairs or writes the journal. The output path
must not exist (`export_exists`); the bytes are staged beside it and published
by a no-replace rename, so a failed export leaves nothing behind.

The default content is the active branch from the root to the head: a title with
the session ID, workspace, creation time, initial model and the model at the
head, then one section per entry in conversation order:

| Entry | Rendering |
| --- | --- |
| user message | `## You`, then each text block verbatim |
| assistant response | `## Assistant (<provider>/<model>)`, text verbatim, reasoning summaries as block quotes, tool calls as fenced `json` blocks naming the tool, a note when the outcome is not `completed` |
| tool result | `## Tool result (<tool>)`, status and code, exit code, duration and truncation as bullets, referenced artifacts as digest lines, and the full content in fenced blocks (never truncated) |
| compaction | one note naming the strategy and version, its covered-entry count and its model when it has one |
| context control | one note naming the turn and the effective mode |
| model change | one note naming the model and the portability policy |

Media blocks become reference lines naming the kind, MIME type, byte count and
the first twelve characters of the SHA-256; media is never embedded. Opaque
provider state is omitted with a note naming its owner, schema and size, and its
bytes are never printed. Fences grow past any run of backticks in the content,
so a block can never close early. The same journal always produces the same
bytes.

`--all-branches` names every branch head instead of following one branch: the
leaves of the entry tree plus the durable head when it is not a leaf, in ID
order. The tree is walked in journal order and each entry appears exactly once.
A `## Branch <head-entry-id>` heading introduces the run of entries only one
head has, `## Shared history` introduces the run every head has in common, and
`## Branches <id>, <id>` introduces a run several heads share, so a shared
prefix is never repeated. A session with a single branch is one
`## Branch <head-entry-id>` run. Entry sections then use `###` instead of `##`.

### Journal export

`export --format journal` is read-only too. It creates `<PATH>`, which must not
exist (`export_exists`), containing `sessions/<id>.jsonl` as an exact copy and
`artifacts/<sha256>` for every artifact the journal references. Each artifact is
verified before and while it is copied, and an absent or altered artifact fails
the export (`export_artifact_invalid`). Created partial children are removed;
an empty newly created destination directory may remain. Pre-existing output
is never removed or overwritten.
References include configuration, command and manifest digests, media, provider
state, plan artifacts and a quarantine artifact, exactly as storage validation
sees them.

The result is itself a data root: `DataRoot::open` accepts it and the journal
replays to the same head. `--all-branches` applies to Markdown only.

### Deletion

`delete <ID>` takes the exclusive writer lock without waiting, so a session a
running engine holds is refused as `session_locked`. Outside a terminal it
requires `--yes` (`confirmation_required`); with a terminal it asks
`Delete session <title> (<id>)? [y/N]`. It removes exactly one journal, and a
lock file if one ever existed separately, and nothing else; a missing ID is
`session_not_found`.

**Deletion is irreversible, and it never removes an artifact.** Artifacts are
shared and content-addressed across sessions, so a session's bytes may still be
another session's bytes; the command prints
`run 'harness sessions gc' to reclaim artifact space`.

### Artifact collection

`gc` removes artifact bytes that no readable journal references **and** whose
modification time is older than a 24-hour grace window. It scans every journal
under a non-blocking shared lock and refuses the whole run as
`gc_journal_unreadable: <id>` if one cannot be locked or validated, because a
damaged journal might reference anything. A journal deleted after discovery is
skipped.

The grace window is what makes this safe for shared storage: a writer stores an
artifact *before* the journal commit that references it, so an artifact is
unreferenced for a moment; and `DataRoot::put_artifact*` refreshes an existing
artifact's modification time on a dedupe hit with `utimensat`, so an in-flight
reuse is never old. To prevent reuse between the mtime check and unlink,
collection holds an exclusive artifact-directory `flock` from before candidate
selection through the last unlink and directory sync. Artifact publication holds
the same lock shared across rename, dedupe verification and mtime refresh. Each
operation opens a separate `NOFOLLOW | CLOEXEC` directory description: reuse
either finishes first and is young when collection checks, or waits until
collection finishes and publishes fresh bytes. No lock file or journal format
change is involved. Collection stats each candidate under that lock without
following a symlink and deletes only private regular files it verified, then
syncs the artifacts directory.

`--dry-run` lists the candidates and their total size and changes nothing. A real
run without a terminal needs `--yes`. Both print the artifact count, the total
bytes and the number of journals scanned.

See [ADR 0030](decisions/0030-session-export-deletion-and-gc.md) for the race
analysis and the reasons deletion and reclamation are separate commands.

## Record versions

Each record carries its own `version`. Readers accept versions 1 through 5
(`SUPPORTED_VERSIONS`) and validate every record against its own version's
bundled schema. Writers write only version 5 (`CURRENT_VERSION`), including
when they append to a session created under an earlier version. A v1 journal
therefore continues as a v1 prefix and a v5 suffix, one unbroken checksum
chain; the tail-repair commit is an ordinary v5 commit. An older binary refuses
at the first v5 record with `unsupported_version`; it never ignores an undo or
appends past one. Opening never rewrites or migrates a record.

| Rule | Error code |
| --- | --- |
| `version` is not an integer from 1 through 5 (`0`, `6`, `"2"`, `2.0`, missing) | `unsupported_version` |
| A record's shape is not that of its own version | `schema_violation` |
| A record's version is lower than the previous record's | `version_regression` |
| A writer is handed a record whose version is not `CURRENT_VERSION` | `write_version` |

v2 adds three fields to v1, each a required key in v2 and absent in v1:

| Field | v2 value | Read from v1 as |
| --- | --- | --- |
| `tool_metadata.code` | `null` or a code matching `^[a-z][a-z0-9_]{0,63}$` | `None` |
| `execution_intent.effects` | a nonempty set from `read`, `filesystem_mutation`, `process`, `network`, `external_side_effect` | `None`: effects unknown |
| `assistant_response.projection_fingerprint` | `null` or the SHA-256 of the context projection the inference was sent | `None` |

The Rust types serve both versions: the fields are `Option`s that read as
`None` from v1 bytes and always serialize. Storage checks these rules on
v2 records:

| Rule | Error code |
| --- | --- |
| An executor or runtime result that succeeded carries a code | `code_on_success` |
| An executor or runtime result that did not succeed has no code | `code_required` |
| A `user_observed` or `user_acknowledged_unknown` result carries a code | `code_on_user_result` |
| `effects` are not in the order listed above, so one set has two byte forms | `effects_not_canonical` |
| A `trusted_fs` intent declares more than `read` and `filesystem_mutation`, or a `bubblewrap` intent lacks `process` | `effects_backend_mismatch` |

v3 adds one entry kind and relaxes one v2 field; it adds no required key to a
v2 payload:

| Change | v3 value | Read from v1/v2 as |
| --- | --- | --- |
| `entry.payload.kind` | gains `context_control` | no such entry exists |
| `compaction.model` | optional; **absent** means no model was used | always present |

`null` is not a spelling of "no model": a v3 compaction either names its model
or omits the field. The model-based strategies always name theirs; the
model-free `observation_masking` (implemented in m6-04) omits it.

The schema accepts any code matching the pattern, so readers keep codes they do
not know. `ResultCode` names the ones this build writes: `tools_unavailable`,
`cancelled` and `process_restart`. An intent's `effects` of `None` mean it was
recorded before v2; it must never be treated as read-only. Whether policy
allows an effect is not a storage rule, and storage does not compare `effects`
with the plan artifact it does not read. The context projection leaves out
`code` and the fingerprint, so a v1 and a v2 copy of the same entries project
identically. A v1 record carrying a v2 field, or a v2 record missing one, is a
`schema_violation`.

v4 adds the pre-image binding a durable undo needs. Its two new required keys
are present-and-nullable, so one spelling exists per v4 object:

| Change | v4 value | Read from v1-v3 as |
| --- | --- | --- |
| `tool_metadata.file_effect` | `not_applied`, `applied`, `unknown` or `null` | `None`: unavailable, not "absent" |
| `execution_intent.file_image` | an image object, or `null` | `None`: no stored pre-image |
| `journal_event.kind` | gains `undo_intent`, `undo_uncertain`, `undo_result` | no such event exists |
| `entry.payload.kind` | gains `undo_note` | no such entry exists |

A `file_image` is `{path, pre_image, pre_mode, post_image}`: the canonical
workspace path, the pre-image bytes as an optional artifact reference, the
ordinary permission bits or `null` exactly when the target was absent, and the
SHA-256 of the exact replacement bytes. A trusted `write`/`edit` intent in a v4
record always carries one, and every other call always carries `null`; its
result carries a `file_effect` classification exactly for those calls. The
classification is the executor's observation of the rename commit point, never
an inference from the result's status or text.

v5 extends those images for ADR 0034's trusted `multi_edit`. The original
`path`, `pre_image`, `pre_mode` and `post_image` fields remain the first target;
optional `additional_targets` carries the remaining sorted path images. A v5
tool result may include sorted `file_paths` rows (`path`, `effect`) when a batch
has per-target outcomes. The top-level `file_effect` is their aggregate. Undo
selects only paths known applied; an unknown path still requires explicit
recovery, and independent rename commits are not described as one atomic
filesystem transaction. The independent v1-v4 schemas remain unchanged.

An `undo_intent` is `{undo_id, target_turn_id, anchor_head, configuration,
policy_revision, targets, omitted_calls}`. A target is
`{path, invocation_ids, pre_image, pre_mode, post_image}` with the mutations in
execution order, the first pre-image and the last post-image, and targets are
unique by path and sorted bytewise. `undo_uncertain` is `{undo_id, reason}`
with a reason bounded to 1 KiB. `undo_result` is `{undo_id, outcome,
file_outcomes, note_entry_id, resolution_note}` with one row per target in
target order, and `undo_note` is `{undo_id, target_turn_id, outcome, text}`.
The outcome set is `succeeded`, `not_applied`, `observed` and
`acknowledged_unknown`; a resolution note is required exactly for the last
two and bounded to 64 KiB.

Undo records have exact commit shapes:

| Commit | Shape | Error code |
| --- | --- | --- |
| intent | retained head, no entries, exactly one `undo_intent` | `undo_commit_shape` |
| uncertainty | retained head, no entries, exactly one `undo_uncertain` | `undo_commit_shape` |
| terminal | one advancing `undo_note` at the intent's anchor, exactly one `undo_result` naming it | `undo_commit_shape` |

Storage additionally checks, before any entry of the commit is applied:

| Rule | Error code |
| --- | --- |
| The intent is not exactly the selection derived from durable facts (turn, targets, omitted calls), or its anchor/configuration/revision differ | `undo_binding` |
| The image does not agree with the bound plan's path, pre-image or write post-image | `undo_image_binding` |
| The note, rows, text or result do not match the intent, or the note id is wrong | `undo_result_binding` |
| The result arrives in the wrong uncertainty phase, or duplicates one | `undo_recovery_state` |
| Targets are unsorted, duplicated or empty of mutations | `undo_binding` |

The terminal note text is deterministic: storage recomputes it from the intent,
the rows and the omitted calls' recorded tool names, so a hand-written note
cannot say something the journal does not support. `session/validation.rs`
verifies every pre-image artifact, the intent's configuration and the
plan/image agreement during append *and* replay, and
`session/maintenance.rs::record_references` lists the same digests for export
and collection.

## Undo of one turn's file edits

Undo reverts the trusted `write`/`edit` effects of one finished turn: a file the
turn created is removed, a file it overwrote gets its captured pre-image bytes
back, and nothing else is ever reversed. An `undo_note` entry is the durable
record the model reads; it is not dialogue, has no turn, and sits at the
intent's anchor head. A note consumes its target turn on its own ancestry only,
so a sibling branch is offered the same turn again.

An unresolved undo is unfinished work: it blocks turn starts, navigation,
configuration changes, a second undo, and every ordinary tool exchange until
its result is committed. Recording uncertainty sets the operational state to
`recovering`, and the terminal commit sets it back to `idle`. A not-applied
undo consumes nothing, so the user may ask again. Storage admits exactly two
undo shapes through a recovery barrier: an `undo_uncertain`, and the terminal
note/result pair. It never admits an `undo_intent`, so recovery can neither
start nor restart an undo.

## Context controls

A `context_control` entry records one finished turn's effective mode on the
branch: `default`, `pinned` or `excluded`. Its payload names the target turn;
the entry's own `turn_id` is `null`, because a control is a branch marker rather
than dialogue, exactly like `model_changed`. The last control for a turn on the
selected ancestry wins, controls never alter history, and a control that would
not change the effective mode is refused instead of recorded. See
[decision 0025](decisions/0025-context-controls.md).

| Rule | Error code |
| --- | --- |
| The session is not idle, or has unresolved work | `session_not_idle` |
| The entry has a `turn_id` of its own, or its commit starts a turn | `context_control_inside_turn` |
| The head is inside a tool exchange | `head_inside_tool_exchange` |
| The target turn is unknown or still unfinished | `context_control_unfinished_turn` |
| No entry of the target turn is on the control's parent ancestry | `context_control_not_ancestral` |
| The control does not change the target turn's effective mode | `context_control_noop` |
| A `pinned` or `excluded` control targets a turn an active summary covers | `context_control_covered` |
| A compaction's coverage contains a turn pinned at its parent | `compaction_covers_pinned_turn` |
| A compaction's coverage contains a turn excluded at its parent | `compaction_covers_excluded_turn` |
| A coverage gap is not a whole turn controlled at the compaction's parent | `compaction_not_contiguous` |

`session_not_idle` covers the tool-exchange case in practice: a pending exchange
belongs to an open turn. The `head_inside_tool_exchange` check in the branch
walk stays as the same defense `model_changed` has. Resetting to `default` is
always accepted, because it is how a control is undone; pinning or excluding a
turn whose content an active summary already absorbed is refused, because the
summary text derived from it stays in the projection.

**Coverage shape.** Since m6-02 a compaction's `covered_entry_ids` is the
selected span minus every turn pinned or excluded at the compaction's parent, so
it is an ancestral, ordered subsequence of the branch. The entries between its
first and last covered entry are either covered or belong to a controlled turn;
anything else is `compaction_not_contiguous`. A covered entry whose turn is not
covered whole is `compaction_splits_turn`, so every gap is a whole turn. The
rule lives in `session::coverage` and the builder validates a recorded coverage
with it too, folding the same controls at the same parent, so storage and the
builder cannot disagree; `session::compaction_manifest` remains the single
source of the manifest and its fingerprint.

Effective modes are derived state: `BranchFacts` carries them per entry, replay
rebuilds them, and the test-build oracle compares them with a scanning walk
after every applied record.

## Durability and recovery

The API blocks through file/directory synchronization. The async runtime owns
the store on a dedicated worker thread and awaits its acknowledgement before
effects. See [runtime](runtime.md) for the async handle and subscriptions.
Never dispatch a tool from a staged or failed transaction. A journal write/flush/
sync failure poisons that store: discard it and reopen, without retrying its bytes.
A complete valid record can replay after a lost acknowledgement. Its command
receipt returns the original sequence/checksum/head and accepted commit.

`JournalStore::inspect` acquires a shared nonblocking lock and reports the exact
valid byte boundary, first failing offset/reason, total length and whole-file
SHA-256. It retains an immutable valid-prefix snapshot. An engine already holding
the writer lock supplies its own snapshot for live inspection.

An unterminated final suffix, even a complete JSON object lacking LF, blocks
normal writable open. Pass the inspection to `JournalStore::repair_tail`, or its
fingerprint and size to `repair_inspected_tail`, only when repair is explicitly
requested. Under the exclusive lock, it checks the
expected size and digest, durably quarantines the exact suffix, truncates only
those uncommitted bytes, syncs the journal, then appends/syncs `tail_repaired`.
A file without a valid header cannot be repaired into an invented session.

A complete malformed/checksum-invalid/semantically-invalid line is corruption.
Opening and tail repair preserve it and refuse writable access. A separate
preserved-source salvage/export workflow is required and is not implemented.
Missing, unsafe or altered referenced artifacts block loading; they never become
empty content. Artifacts are checked for exact byte length and SHA-256, and are
synchronized before a journal can reference them. Publication uses no-replace
rename; an existing artifact is accepted only after verification.

On reopen, unfinished turns and intents lacking results require recovery.
Opening does not append synthetic outcomes, revoke history, or retry tools.
Only explicit recovery commits are accepted until the block clears. Known
not-started proposals receive runtime cancelled results before turn termination;
uncertain intents require `execution_uncertain`, an interrupted turn, and an
explicit observed or acknowledged-unknown result plus `recovery_resolved`.
Pending approval/authorization records cannot be used to resume dispatch after
restart. The runtime core makes these commits after open: it closes an
interrupted turn on its own and waits for the user to resolve each uncertain
intent. See [runtime recovery](runtime.md#recovery).

### Journal commands

Two CLI commands expose inspection and repair without an engine. Both take the
data root and session ID, and both refuse at once a session a running engine
holds.

```sh
cargo run --locked -p harness -- journal inspect <data-root> <session-id>
cargo run --locked -p harness -- journal repair <data-root> <session-id> \
  --fingerprint <sha256> --size <bytes>
```

`journal inspect` prints `JournalStore::inspect` as JSON: `file_bytes`,
`valid_bytes`, the whole-file `fingerprint`, the count of valid records per
format version (`versions`, such as `{"1": 812, "2": 40}`), the first `diagnostic` (offset,
kind such as `torn_tail` or `corrupt`, and code), whether it is `repairable`
(a torn tail after a valid header), the `quarantine_bytes` repair would move,
and the `valid_prefix`: sequence, head, state, active turn, whether it requires
recovery, and its unresolved invocations. It writes nothing.

`journal repair` calls `JournalStore::repair_inspected_tail` with the
fingerprint and size the user copied from that inspection. Neither is ever
chosen for the user. A changed file is refused as `stale_repair`, and a
complete corrupt record, a missing header or a journal without a torn tail as
`tail_not_repairable`; nothing is changed. On success it prints the
`quarantine_artifact` reference, the original and valid sizes, the new
sequence and whether the session still requires recovery. That recovery
happens when an engine next opens the session; through a `SessionHandle` the
same flow is `PersistError::TornTail` and `SessionHandle::repair_tail` (see
[tail repair](runtime.md#tail-repair)).

## Validation and limits

The bundled schemas are used locally with network resolution disabled. Required
nullable fields, unknown variants/fields, UUIDv7 format, UTC timestamps and safe
integer bounds are checked. Every object rejects duplicate keys, including tool
arguments. Checksums cover the original raw record bytes, never reserialized JSON.
The hash chain, sequences, IDs, ancestry, heads, turn transitions, call/result
bindings, authorization references and artifact references are validated.
Compaction replay checks ancestry, whole completed source turns, the two retained
recent turns, the coverage shape above, supersession and exact manifest
provenance, and that no covered turn is pinned or excluded. Context-control
replay checks the effective-mode rules above. Summary
generation, selection, target-model portability previews and context
budgets are not storage questions and are not implemented here.

Physical records, including LF, are limited to 8 MiB with bounded reading and
serialization. Snapshot indexes reside in memory; transaction validation stages
a cloned state. Replay currently clones indexes per commit and rechecks referenced
artifacts. Validation derives per-entry branch facts and live tool-state indexes,
rebuilt by replay and never persisted, so validating one appended or replayed commit
costs O(log n) in history length; compaction validation and `ancestry()` remain
linear in branch length.
Artifacts can be streamed; `read_artifact` requires a caller-supplied allocation
limit. Opaque provider bytes must stay out of logs and ordinary prompt text.

The root and its `sessions`/`artifacts` children require effective-user ownership
and 0700 permissions; files require 0600, regular-file type and one hard link.
Permissions are never silently relaxed. Root path traversal and file opens use
pinned directory descriptors and no-follow operations. This protects the harness's
own storage; it is not a workspace tool implementation or a process sandbox.

Use local Linux filesystems honoring `flock`, `fsync` and directory sync. Tests
inject failures around real storage operations; they cannot prove power-loss
behavior on hardware that lies about successful synchronization. Network
filesystems and hostile same-user replacement remain unsupported.

## Checks

Run `./scripts/check.sh`, or `CARGO_NET_OFFLINE=true ./scripts/check.sh` after
fetching dependencies. J01–J03 tests exercise all three specification journals,
semantic rejection, final/middle-record tampering, locks, durable branch/model/
policy reconstruction, command deduplication, write/flush/sync/acknowledgement
failures, quarantine/truncation crashes and artifact failures. Ordinary tests
require neither network access, API keys, Bubblewrap nor provider services.
