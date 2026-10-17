# 0003: First JSONL session implementation

Status: Accepted for the storage milestone; other RFC-001 subsystems remain proposed.

Date: 2026-10-17

## Review and scope

Review of RFC-001 sections 2, 5, the persisted parts of 6/7, the v1 schema, and
J01–J03 found the transaction/checksum/head model suitable for implementation.
Keep its one-journal tree and exact-byte checksums. Do not weaken durability to
get an initial runtime working. Implement in `harness-core`; another crate is
not justified yet. The executable remains help/version only.

This accepts the storage representation and validation rules, not a claim that
providers, context projection, approval policy, tools, or recovery orchestration
are implemented. Persisting their types/events does not execute those systems.

## Decisions resolved during implementation

- The first concrete API is blocking `JournalStore`, not the RFC's illustrative
  boxed-future trait. Its transaction is not cancellable halfway through sync.
  A future Tokio actor must own it on a blocking worker and await durability
  before acknowledging commands or dispatching effects. No Tokio dependency is
  added until asynchronous runtime code exists.
- Public wire structs are construction inputs. The store checks the bundled
  v1 JSON Schema on every serialized transaction and loaded line, including
  required nullable fields, safe-integer bounds and unknown fields. A separate
  duplicate-key pass covers every JSON object, including tool arguments.
  UUIDv7 identities, SHA-256 digests and UTC timestamps also validate on decode.
- Each physical line, including LF, is limited to 8 MiB. Both serialization and
  reading are bounded. An oversized unterminated suffix is still a torn tail;
  quarantine streams it without retaining it in memory. Oversized complete
  records are never repairable tails.
- Operational transitions also allow `Processing -> Continuing` for rejected
  calls and `Authorizing -> Processing/Continuing` for nonexecuted outcomes.
  Terminal outcomes are separate events. These fill the RFC's unspecified
  negative-call transitions without allowing inference before tool results.
- Approval decisions and intents may share a transaction, in event order, as in
  the interrupted-execution fixture. Their proposal must be in an earlier
  durable record. An invocation's result must be in a later durable record.
- Opening never synthesizes audit history. A snapshot identifies unfinished
  turns and unresolved intents, and a reopened writer accepts only explicit
  recovery transactions until the block is cleared. Recovery commits terminate
  interrupted turns, mark intents uncertain, record known-not-started cancelled
  proposals, or record an explicit observed/acknowledged-unknown result. This
  storage API performs no automatic execution or recovery orchestration.
- Command deduplication returns the original sequence/checksum/head and retains
  the accepted commit for reconstructing correlated identifiers. The submitted
  command is a compact JSON object artifact; `bind_command` computes its digest.
  JSON object key order is normalized by Serde's ordered map. The schema remains
  the wire authority; existing compact fixtures need no rewriting.
- Read-only file inspection takes a nonblocking shared lock, so it cannot race a
  supported writer. A running engine serves inspection from its immutable
  snapshot. Tail repair reacquires the exclusive journal lock and rechecks the
  whole-file SHA-256 and length supplied by inspection.
- Only the explicit repair operation may append `tail_repaired`. Quarantine is
  synced before truncation, then the journal is synced before its repair audit.
  A crash between truncation and the audit may leave a valid prefix and orphaned
  quarantine; opening must not pretend the missing audit committed.
- Data roots are explicit absolute paths. Their parent must exist; this API does
  not create arbitrary ancestor directories. The XDG helper only computes a
  default path. Root/children require owner-only 0700; files require owner-only
  0600 and one hard link. Descriptor-relative, no-follow traversal refuses
  symlink components. Existing permissions are verified, never silently changed.
  The recorded workspace and private storage must not overlap. Supported storage
  remains a local filesystem with working flock and file/directory sync; hostile
  same-user replacement and network filesystems remain outside the guarantee.
- Compaction records are replayed and their ancestry, complete source turns,
  retained two-turn window, configuration and exact source manifests are checked.
  Summary generation, pin policy, context budgets and projection remain future
  work; accepting a stored summary does not validate the quality of its text.

## Consequences

The public API can create/open a session, append validated atomic transactions,
select heads via navigation commits, inspect all branches, verify/read/publish
artifacts, and explicitly repair torn tails. Failed journal I/O poisons that
writer; a fully written record may be recovered even when its acknowledgement
was lost. Tests inject failures at durability boundaries using real temporary
files; they do not simulate a kernel or storage device falsely reporting sync.

The append-only exception applies only to uncommitted suffix bytes. Complete
corruption requires a separate preserved-source salvage/export workflow, which
is not implemented by ordinary opening or repair. No workspace tool or process
sandbox is implemented or implied by private storage filesystem operations.
