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

## Record versions

Each record carries its own `version`. Readers accept versions 1 and 2
(`SUPPORTED_VERSIONS`) and validate every record against its own version's
bundled schema. Writers write only version 2 (`CURRENT_VERSION`), including
when they append to a session created under v1. A v1 journal therefore
continues as a v1 prefix and a v2 suffix, one unbroken checksum chain; the
tail-repair commit is an ordinary v2 commit. Opening never rewrites a record.

| Rule | Error code |
| --- | --- |
| `version` is not the integer 1 or 2 (`0`, `3`, `"2"`, `2.0`, missing) | `unsupported_version` |
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

The schema accepts any code matching the pattern, so readers keep codes they do
not know. `ResultCode` names the ones this build writes: `tools_unavailable`,
`cancelled` and `process_restart`. An intent's `effects` of `None` mean it was
recorded before v2; it must never be treated as read-only. Whether policy
allows an effect is not a storage rule, and storage does not compare `effects`
with the plan artifact it does not read. The context projection leaves out
`code` and the fingerprint, so a v1 and a v2 copy of the same entries project
identically. A v1 record carrying a v2 field, or a v2 record missing one, is a
`schema_violation`.

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
recent turns, supersession and exact manifest provenance. Generation, pin
selection, portability previews and context budgets are not implemented.

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
