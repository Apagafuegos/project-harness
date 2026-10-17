# Session storage

`harness_core::session` implements the first headless storage milestone on Linux.
It stores one checksummed JSONL journal per session and content-addressed artifact
sidecars. It does not run an agent, project context, execute workspace tools, or
supply CLI session commands.

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
        id: entry_id,
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

Wire structs are construction inputs; schema validation and semantic replay run
before every append. User entries require a matching `turn_started` and
`idle -> preparing` transition. Assistant entries belong to the active inference;
complete tool groups require ordered associated results. Persisted audit types
are available now so the later runtime can use the same representation.

## Durability and recovery

The API blocks through file/directory synchronization. A future Tokio engine must
own the store on a blocking worker, awaiting its acknowledgement before effects.
Never dispatch a tool from a staged or failed transaction. A journal write/flush/
sync failure poisons that store: discard it and reopen, without retrying its bytes.
A complete valid record can replay after a lost acknowledgement. Its command
receipt returns the original sequence/checksum/head and accepted commit.

`JournalStore::inspect` acquires a shared nonblocking lock and reports the exact
valid byte boundary, first failing offset/reason, total length and whole-file
SHA-256. It retains an immutable valid-prefix snapshot. An engine already holding
the writer lock supplies its own snapshot for live inspection.

An unterminated final suffix, even a complete JSON object lacking LF, blocks
normal writable open. Pass the inspection to `JournalStore::repair_tail` only
when repair is explicitly requested. Under the exclusive lock, it checks the
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
restart. Automatic recovery orchestration belongs to the next runtime slices.

## Validation and limits

The bundled v1 schema is used locally with network resolution disabled. Required
nullable fields, unknown variants/fields, UUIDv7 format, UTC timestamps and safe
integer bounds are checked. Every object rejects duplicate keys, including tool
arguments. Checksums cover the original raw record bytes, never reserialized JSON.
The hash chain, sequences, IDs, ancestry, heads, turn transitions, call/result
bindings, authorization references and artifact references are validated.
Compaction replay checks ancestry, whole completed source turns, the two retained
recent turns, supersession and exact manifest provenance. Generation, pin
selection, portability previews and context budgets are not implemented.

Physical records, including LF, are limited to 8 MiB with bounded reading and
serialization. Snapshot indexes reside in memory; transaction validation stages
a cloned state. Replay currently clones indexes per commit and rechecks referenced
artifacts. This favors a clear first implementation over large-history performance.
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
