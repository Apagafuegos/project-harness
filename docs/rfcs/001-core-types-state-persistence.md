# RFC-001: Core Types, State Transitions & Persistence Format

Status: Storage details accepted and implemented under
[decision 0003](../decisions/0003-jsonl-storage.md), with J01–J03 behavioral tests.
The pure runtime core is implemented under
[decision 0004](../decisions/0004-pure-runtime-core.md), which records the runtime turn commits. Other subsystem details
remain proposed; no complete agent runtime is implemented.

Date: 2026-10-16

Architecture baseline: [decision 0002](../decisions/0002-v01-contracts.md).

## 1. Scope and normative language

This RFC refines the six provisionally agreed contracts into an implementation
target. MUST, MUST NOT, and SHOULD describe requirements of the future
implementation. Implemented storage behavior and remaining scope are distinguished
in decision 0003 and the storage usage guide.

The Rust shapes below specify ownership and behavior. Serialization is defined
by the accompanying JSON Schemas, one per record version
([v1](001/schema/session-v1.schema.json), [v2](001/schema/session-v2.schema.json)),
and the semantic rules in this RFC. A schema-valid record is not necessarily a valid
session: ancestry, state transitions, authorization, and call/result associations
also require semantic validation.

v0.1 is Linux-first, one process, one executable, a headless library, and a
Ratatui client. There is no IPC protocol, MCP client, plugin runtime, subagent,
remote workspace, or filesystem rollback mechanism in this specification.

## 2. Identity, ownership, and common content

Session, record, entry, turn, response, call, invocation, approval, decision, and
command identifiers are distinct Rust newtypes over UUIDv7 values. New IDs come
from the trusted engine, never from the model. Provider-native identifiers are
separate strings. UUID order and wall clocks are not journal order.

Use `u64` for counters, milliseconds, sizes, and token counts; serialized values
MUST be in `0..=9_007_199_254_740_991`. Exhaustion is an error, never wraparound.
Journal timestamps are UTC RFC 3339 strings ending in `Z`; use a monotonic clock
for live deadlines. All textual JSON strings are UTF-8. Workspace-relative tool
paths are UTF-8 in v0.1; unsupported non-UTF-8 names produce an explicit error.

```rust,ignore
pub struct ModelRef {
    pub provider_instance: String,
    pub model: String,
}

pub struct ArtifactRef {
    pub sha256: String,
    pub bytes: u64,
}

pub enum ContentBlock {
    Text { text: String },
    Image { artifact: ArtifactRef, mime_type: String },
    Audio { artifact: ArtifactRef, mime_type: String },
    File { artifact: ArtifactRef, mime_type: String, name: String },
    ReasoningSummary { text: String },
    ToolCall {
        call_id: CallId,
        name: String,
        arguments: serde_json::Map<String, serde_json::Value>,
        provider_call_id: Option<String>,
    },
    ProviderState {
        owner: ModelRef,
        schema: String,
        artifact: ArtifactRef,
    },
}

pub struct CallRef {
    pub response_entry_id: EntryId,
    pub call_id: CallId,
}

pub enum ToolStatus {
    Succeeded, Failed, InvalidArguments, Denied, TimedOut,
    Cancelled, InfrastructureFailure, Uncertain,
}

pub struct ToolResult {
    pub call: CallRef,
    pub invocation_id: Option<InvocationId>,
    pub status: ToolStatus,
    pub content: Vec<ContentBlock>,
    pub metadata: ToolMetadata,
}
```

`ToolMetadata` has nullable `exit_code` and `duration_ms`, `output_truncated`, and
`provenance` (`executor`, `runtime`, `user_observed`, or
`user_acknowledged_unknown`), and `artifacts: Vec<ArtifactRef>`. The schema defines
their exact wire fields. Full text captures belong in metadata artifacts; the
model gets bounded inline text, not an automatic multimodal file attachment.

Content order is significant. `ReasoningSummary` is only provider-designated
displayable content, never a promise to expose hidden reasoning. Opaque native
state remains an owner-tagged artifact; inspection reports may name its owner
and size but MUST NOT disclose its payload. It is not ordinary prompt text.

User messages accept text, image, audio, and file blocks. Assistant responses
may additionally contain reasoning summaries, tool proposals, and native state.
Tool results accept text/media/file blocks, not nested calls or native state.
Role validation MUST enforce these subsets even where the shared block schema
permits a broader union. Unsupported modalities produce a capability error;
the data model supports them before adapters necessarily do.

## 3. Contract 1: model provider

### 3.1 Input and stream ownership

```rust,ignore
pub type ModelEventStream = std::pin::Pin<
    Box<dyn futures_core::Stream<Item = ModelEvent> + Send + 'static>
>;

pub struct OperationControl {
    pub cancel: tokio_util::sync::CancellationToken,
    pub deadline: Option<tokio::time::Instant>,
}

pub struct ModelRequest {
    pub response_id: ResponseId,
    pub input: ModelInput,
}

pub struct ModelInput {
    pub model: ModelRef,
    pub instructions: Vec<Instruction>,
    pub items: Vec<ConversationItem>,
    pub tools: Vec<ToolDefinition>,
    pub options: InferenceOptions,
    pub continuation: Option<ProviderContinuation>,
}

pub trait ModelProvider: Send + Sync {
    fn stream(&self, request: ModelRequest, control: OperationControl)
        -> ModelEventStream;
}
```

The stream is owned and starts work on polling. It does not borrow the provider;
an adapter may capture an `Arc` to its configured client. No separate start
future is needed. Setup failures are terminal events, possibly before a start.
The caller supplies response identity and cancellation/deadline controls.
The runtime wraps the pure projected `ModelInput` in a new `ModelRequest` for
one inference. Response IDs/deadlines are not generated by the context builder
and are excluded from logical-input fingerprints.
Dropping the stream requests transport cancellation and releases any producer;
it does not guarantee a terminal event to a caller that stopped polling.

`Instruction` is ordered `{ level: System | Developer, text: String }`.
`ConversationItem` is an ordered message or tool result projected from the tree;
local lifecycle/audit entries are never serialized as model dialogue.
`InferenceOptions` has required `max_output_tokens`, optional reasoning effort,
optional sampling controls, and a provider-namespaced JSON options map.
Unknown/unsupported required options are errors, not silently ignored settings.

`ProviderContinuation` has `owner: ModelRef`, `schema: String`,
`artifact: ArtifactRef`, and `input_fingerprint: String`. Its fingerprint binds
the logical prefix it replaces. A changed branch, compaction, instructions,
tool set, or incompatible model invalidates it. Per-item `ProviderState` is
retained separately; a response handle is not a substitute for replay blocks.

Capabilities come from explicitly configured model metadata, with an adapter
validation pass. Context/output limits are optional facts; unknown context
limits require an explicit configured limit before inference. Network discovery
and guessed model-name-based limits are not required in v0.1.

### 3.2 Normalized events and terminal outcomes

> **Implementation note.** [Decision 0005](../decisions/0005-provider-stream.md)
> implements this section with the core's `ProviderEvent` rather than
> `ModelEvent`. Events carry no `response_id`, because a stream belongs to one
> request, and no engine `call_id`, because the core issues `CallId`s at
> `ToolCallStarted`. `Finished` is a flat `{ outcome, finish_reason, error }`.
> The proposal below is kept for reference.

`ModelEvent` is one of: `Started`, `TextDelta`, `ReasoningSummaryDelta`,
`ToolCallStarted`, `ToolArgumentsDelta`, `ToolCallReady`, `UsageUpdated`, or
`Finished`. Every event carries `response_id`; block deltas carry a stable
response-local `block_index`, and tool events carry the engine-issued `call_id`.
An adapter maps provider-native item/call IDs to these identities.

`Finished` has an outcome of `Completed { finish_reason, usage, continuation }`,
`Failed { error, usage }`, or `Cancelled { usage }`. Finish reasons are `stop`,
`tool_calls`, `output_limit`, or `refusal`. Usage is a nullable snapshot with
nullable input/output/cache/reasoning counts; normalized updates are cumulative
snapshots, not increments. Replace known values, never sum snapshots or invent
zero for missing values. Adapter conversion handles native incremental reports.

Model error codes are `invalid_request`, `unsupported_capability`,
`authentication`, `rate_limit`, `transport`, `provider_failure`,
`malformed_stream`, `limit_exceeded`, or `context_overflow`, with a safe message,
optional provider code, and optional retry-after milliseconds. Raw requests/headers
are not errors.

There is at most one start and exactly one observed terminal event while a
consumer continues polling. After a terminal event, the next poll is EOF.
Unannounced EOF becomes an engine-observed interrupted response. Deltas follow
their block/call start; each call has at most one ready event. Arguments are
fully assembled JSON objects before ready, and schema validation still follows.

The engine MUST buffer proposals until a successful `tool_calls` finish. No
proposal from failed, cancelled, truncated, refused, or interrupted inference is
executable. An inconsistent finish/call combination is `malformed_stream`.
Text from such responses can be retained, explicitly marked partial.

The stream SHOULD be pull-driven. Any internal queue is bounded by both count
and bytes (defaults: 256 events, 1 MiB queued data). An individual event is at
most 256 KiB; adapters split text deltas and reject oversized indivisible tool
arguments/native payloads or store native payloads as artifacts. Backpressure
waits cancellably; essential deltas are never silently dropped. Deadlines also
apply while waiting. Stream drop cancels tasks; no detached unbounded producers.
Combined inline response content/arguments is bounded to 4 MiB. Overflow ends
the response with `limit_exceeded`, retains only bounded partial text, and makes
all its calls ineligible; it cannot force an oversized journal commit.

Adapters do not restart after publishing content. v0.1 has no automatic inference
retry; a user may start a new turn after failure. This never replays tools.

### 3.3 Model switching

Switch only while idle and not recovery-blocked. Build a preview under the target
capabilities before committing `model_changed`. Default policy rejects
incompatible required blocks. An explicit user-selected conversion/omission
policy may remove optional native state, convert supported displayable content,
and report every affected entry. No silent conversion to text and no invented
translation of opaque state. Historical data remains unchanged. A model switch
never carries a stale provider continuation into the next request.

## 4. Contracts 2 and 3: tools, execution, and security

### 4.1 Registry, validation, and authorization

`ToolDefinition` has name, description, JSON Schema, and a declared effect set:
`read`, `filesystem_mutation`, `process`, `network`, or `external_side_effect`.
The registry, not model arguments, selects the trusted implementation and
backend. From record version 2, each `execution_intent` records its plan's
effect set in that canonical order, and each tool result carries a
machine-readable `code`, `null` exactly when an executor or runtime result
succeeded or a user supplied the result. Reserved built-in names cannot be replaced within a running session.

The initial argument objects reject unknown keys:

- `read`: `path`, optional `offset_bytes` (default 0), optional `max_bytes`
  (default/max 1 MiB). Only regular UTF-8 text files initially; binary/media
  support is represented in common types but explicitly unsupported by this tool.
- `write`: `path`, `text`, `overwrite` (default false). UTF-8 contents, at most
  1 MiB inline. Existing parent directories required; no implicit recursive creation.
- `edit`: `path`, nonempty `old_text`, and `new_text`. Exactly one literal match
  is required, counting overlapping occurrences too. Zero/multiple matches fail
  without mutation; no regex replacement.
- `bash`: nonempty `command` (at most 64 KiB UTF-8 bytes), optional
  workspace-relative `cwd` (default `.`), optional
  `timeout_ms` (default 120,000; permitted range 1..=600,000). No argument can
  request elevation, host environment inheritance, or a sandbox bypass.

All tools have a runtime deadline; `read`/`write`/`edit` default to 30 seconds.
Structural, semantic, environmental, and authorization validation all precede
execution. `read` output exposes byte offsets and truncation through text/result
metadata; a range splitting a UTF-8 character is adjusted to a valid boundary
with the actual consumed offsets reported, never silently decoded lossily.
Read planning uses inspection facts only, with no content read or whole-file
size cap; range UTF-8 handling belongs to the step 5 executor. Edit planning
reads at most 1 MiB and checks UTF-8 and literal occurrence uniqueness.

```rust,ignore
pub trait Tool: Send + Sync {
    fn definition(&self) -> &ToolDefinition;
    fn validate(&self, arguments: &serde_json::Value)
        -> Result<ValidatedOperation, Refusal>;
    fn plan(&self, operation: &ValidatedOperation, call: &CallRef,
            workspace: &dyn WorkspaceView, policy: &PolicySnapshot)
        -> Result<Plan, Refusal>;
}

pub enum Authorization {
    Allow(Box<Plan>),
    Ask(ApprovalRequest),
    Deny(Refusal),
}

pub fn decide(plan: &Plan, facts: &PlanFacts, policy: &PolicySnapshot)
    -> Authorization;

pub enum ExecutionOutcome {
    Observed(ToolResult),
    Uncertain { invocation_id: InvocationId, diagnostic: String },
}

pub trait Executor: Send + Sync {
    fn execute(&self, operation: AuthorizedOperation, control: OperationControl)
        -> BoxFuture<'_, ExecutionOutcome>;
}
```

The validate/plan split, fixed registry and pure `decide` are implemented in
Milestone 3 step 3; see [decision 0009](../decisions/0009-tool-policy.md) for
the exact policy table and API boundaries. `WorkspaceView` supplies trusted
facts and bounded UTF-8 reads, not direct filesystem access. `Plan` and
`ValidatedOperation` have private fields and are constructed only by tools.
`Refusal` carries `ResultCode`, `ToolStatus` and short content-free detail;
`text()` renders `<code>: <detail>`. Unknown/disabled names are refused before
validation. Schemas reject unknown keys; semantic argument failures use
`invalid_arguments`, with a field/key and rule diagnostic capped at 200 bytes
and no echoed value. Configuration decoding instead returns session-level
`PolicyError`, not a tool refusal; view contract failures such as a missing
regular-file digest use `infrastructure_failure`. `.git` is read-only by default, but a configured protected
path is fully denied and takes precedence over read-only or any allowance.

The executor sketch and `AuthorizedOperation` below remain planned for step 4.
`BoxFuture<'a, T>` means a pinned, boxed, `Send` future with that lifetime.
`AuthorizedOperation` has a private constructor.
The latter contains the exact validated operation, invocation/call identity,
plan digest, decision ID, policy revision, workspace capability, approved
mounts/network, and expiry. Executors recheck binding/expiry immediately before dispatch;
model-supplied JSON cannot manufacture this type. An uncertain executor outcome
records uncertainty and blocks recovery, not a terminal successful result;
`ToolStatus::Uncertain` is only a user-acknowledged recovery result. Cancellation
never implies rollback of already performed mutations. If a trusted replacement
completed and its success is verified, retain that success even when the enclosing
turn was cancelled.

The plan digest is SHA-256 over exactly `Plan::bytes()` (`Digest::of`); the same
canonical compact JSON bytes are used for approval and, in step 4, persistence
and execution. Plan format **version 2** fixes this field order: `version`,
`call`, `name`, `arguments`, `workspace`, `cwd`, `effects`, `backend`, `mounts`,
`network`, `timeout_ms`, `policy_revision`, `target`. Arguments are validated,
normalized and defaulted, never raw model JSON. A file target has ordered
`path`, `pre_image` (current digest or `"absent"`), and, for write only,
`content_sha256`. Bash has null target, a workspace `.` mount (`ro` when
workspace_mutation is deny, otherwise `rw`), followed by sorted/deduplicated
normalized configured protected paths as `masked` mounts, including absent
paths. A separate `.git` read-only mount is added only when it exists and no
masked path covers it; a covered `.git` is masked instead. Step 7 enforces these
approved mounts. Network is always `deny`. File deadlines are fixed at
30,000 ms. `Plan::effects()` alone supplies intent effects: read `[read]`,
write/edit `[filesystem_mutation]`, bash `[process]`. The historical example
artifact remains plan v1; v2 golden artifacts are in `crates/core/src/tools/golden/`.
Re-resolve filesystem access through capabilities and recheck the pre-image at
execution in step 5 (`stale_plan` without mutation on mismatch). A digest binds
intent; it is not a filesystem-race defense.

Initial workspace selection is explicit user authorization, outside model text.
The default policy allows authorized workspace reads, asks once per write/edit
or process invocation, and denies protected paths. Users may explicitly configure
session-scoped routine-write or sandbox-process allowances. Allowances live in
the configuration artifact, not new journal events. Write/edit allowances bind
a tool and exact normalized relative path, a component-wise directory prefix
ending in `/` (descendants only), or `.` for the workspace. Bash allowances bind
exact command bytes plus normalized cwd, never a command prefix or glob.
`Allowance::remember` grants the narrowest matching allowance. `grant`/`revoke`
produce a compact configuration artifact changing only `policy_revision` and
`allowances`, preserving unknown values and existing allowances. Step 4 will
publish it through `configuration_changed`; a revision invalidates pending
approvals but carries allowances forward. Deny policy and protection always win.
A configured allowance never bypasses the sandbox. Approval requests
expire after five minutes and are single-use for the bound call/plan. User denial,
expiry, cancellation, or policy revision invalidates the ticket.

One response may contain multiple calls. Execute strictly in block order,
sequentially, with no overlapping tool activity in v0.1. Invalid/denied calls get
structured non-executed results and do not prevent validating later calls.
Cancellation records results for remaining calls as cancelled/not started.

### 4.2 Trusted filesystem operations

Trusted built-ins do not spawn executables and are not OS-sandboxed. Their
correctness and authorization enforce containment. Pin a workspace directory
descriptor; accept relative paths, reject absolute paths, `..`, NULs, symlinks,
magic links, and mount crossings in v0.1. Use descriptor-relative resolution
with `openat2` beneath that descriptor and explicit no-symlink/no-mount controls.
Do not canonicalize a string and then reopen it by path. An unsupported required
resolution primitive is a capability failure, not permission to use a weaker
path-prefix check. [Linux documents these resolution controls.](https://man7.org/linux/man-pages/man2/openat2.2.html)

Pin the parent descriptor for mutation, reject nonregular files and multiply
linked files, and operate on verified descriptors. Write/edit use a new temporary
file in that directory, restrictive mode, write+sync, then descriptor-relative
rename and parent-directory sync. For `overwrite=false`, use no-replace rename.
Edit verifies the matched source's identity/content before replacement and
fails on detected concurrent change; it does not promise compare-and-swap
against unrelated host processes. Never write in place through a symlink or
hard link. Cancellation before rename leaves the original; cancellation after
rename records a mutation that may already have succeeded, not a rollback.

Protected capabilities include the journal/artifact store, provider credentials,
and trusted harness policy/configuration; the policy may add workspace paths.
Git metadata is read-only by default. Built-ins enforce exclusions, and process
mounts must mask protected data or mount it read-only as appropriate.

v0.1 assumes a local workspace and no hostile concurrent host process running
as the same Unix user. Isolation does not defend against a compromised kernel,
the user editing the journal directly, or all resource-exhaustion attacks.

### 4.3 Bubblewrap process backend

All external executables, including `ls`, Git, tests, and dependency installers,
use Bubblewrap. The backend constructs the argument vector itself; it never
accepts raw sandbox flags from model input. `bash` invokes a configured absolute
`/bin/bash --noprofile --norc -c` inside that environment; startup files and
`BASH_ENV` are not inherited.

Use an empty mount view with explicit read-only runtime/toolchain mounts, a
workspace mount with authorized write scopes, private temporary/home directories,
private PID/IPC/UTS/user/network namespaces, dropped capabilities, a new session,
and supervisor-linked lifetime. Do not bind the host root/home wholesale or
expose host sockets, engine descriptors, or session/credential storage.
`workspace-read` has no workspace writes; `workspace-write` enables only approved
write paths plus private temporary/build paths. Protected nested paths remain
masked/read-only even beneath a writable mount.

Construct the environment from an allowlist: sandbox `PATH`, `HOME`, `TMPDIR`,
locale, and explicit nonsecret toolchain settings. Do not copy the engine's
environment and remove only known API-key names. Provider secrets remain in
the trusted process. Capture stdout/stderr through pipes, not the controlling TTY.

`elevated` is planned beyond Milestone 3: no M3 argument, command or policy
outcome can request it. It would be a one-operation, explicitly approved
expansion of this same sandbox, not unrestricted execution. Exact extra mounts/effects appear in the plan.
Networking remains denied unless an enforcement backend can provide the exact
approved restriction. Initial v0.1 supports deny-network only; a requested
network exception fails with `network_unsupported` until a constrained egress
backend exists. Network exceptions are not in Milestone 3.
Do not equate `--share-net` with a host/domain allowlist. Offline build/test
caches may instead be explicitly mounted under the authorized plan.

Timeout/cancellation sends graceful termination to the supervised process scope,
then forced termination after two seconds; wait/reap before another tool runs.
Use PID namespace/supervisor lifetime to contain descendants, including detached
processes; a successful foreground exit must not leave a background workload.
If cleanup cannot be established, block execution/recovery rather than reporting
a clean cancellation. Default capture limit is 64 MiB per call; stop the workload
on overflow, preserve bounded output, and set truncation metadata. Model-facing
text defaults to at most 32 KiB; larger permitted captures are artifact references.
Limits are explicit session configuration recorded with policy, not model choices.

Bubblewrap availability and required namespace/mount controls are checked before
the effect begins. Failure is `infrastructure_failure`, with no unrestricted
retry. Bubblewrap requires a correctly configured policy; it is not a complete
sandbox policy by itself. [See upstream's security guidance.](https://github.com/containers/bubblewrap/blob/main/README.md)

## 5. Contract 4: authoritative JSONL journal

### 5.1 Layout, framing, and version

An explicitly selected private data root holds `sessions/<session-id>.jsonl` and
`artifacts/<sha256>`. The initial default is
`$XDG_STATE_HOME/project-harness`, or `$HOME/.local/state/project-harness` when
unset. Directories are mode 0700; files 0600. Refuse unsafe ownership/symlinks;
keep this root outside authorized workspace/process mounts. No secrets are
embedded in workspace paths or configuration records.

There is exactly one authoritative journal per session. Artifact bytes are
sidecars, not a second session database. In-memory indexes are rebuildable.
No separate mutable `HEAD` file is authoritative.

Each physical line is a UTF-8 JSON envelope followed by exactly one LF:

```json
{"record":{"version":1,"seq":0,"record_id":"019a0000-0000-7000-8000-000000000001","session_id":"019a0000-0000-7000-8000-000000000002","at":"2026-10-16T12:00:00Z","previous_checksum":null,"payload":{"kind":"session_created","workspace":"/project","initial_model":{"provider_instance":"openai-default","model":"configured-model"},"policy_revision":"initial","configuration":{"sha256":"<configuration digest>","bytes":123}}},"sha256":"<digest of exact record JSON bytes>"}
```

This framing sketch contains placeholder digests. The linked
[example journals](001/examples/README.md) have real checksums and valid records.

SHA-256 covers the exact UTF-8 bytes of the `record` value, excluding the envelope
and LF. A reader captures that raw value (for example with [Serde `RawValue`](https://docs.rs/serde_json/latest/serde_json/value/struct.RawValue.html));
it MUST NOT parse/reserialize before checking the checksum. No canonical JSON
dependency is required. Writers use compact JSON, no BOM/blank lines, no duplicate
object keys, and the envelope field order `record`, `sha256`. Readers reject
duplicate keys at all depths and verify byte checksums before semantic replay.
Every record carries the previous record's SHA-256 value (null only at seq 0).
This detects accidental alteration, not a malicious writer able to recompute hashes.

Every record carries its own `version`. Readers support versions 1 and 2 and
validate each record against that version's schema; writers write the latest
version, 2, including when appending to a session created under version 1.
Versions never decrease within a journal, and an unknown version fails closed.
Records also carry monotonic contiguous `seq` starting at zero,
unique record identity, immutable session identity, and a timestamp. The first
record is `session_created`; every later record is a `commit`. Maximum physical
line size is 8 MiB. Reject oversized input before unbounded allocation. Store
large outputs/media/native state in artifacts rather than expanding the journal.

Unknown versions/record variants, duplicate IDs, checksum errors, invalid
references, malformed complete lines, and illegal state transitions fail closed.
Do not skip unknown records. Future migrations produce a separately validated
export/new journal with provenance; normal opening never rewrites the source.

### 5.2 Entries, audit events, and the active head

A `commit` payload contains:

```rust,ignore
pub struct Commit {
    pub command_id: Option<CommandId>,
    pub command_fingerprint: Option<String>,
    pub head: HeadChange,
    pub entries: Vec<Entry>,
    pub events: Vec<JournalEvent>,
}

pub struct HeadChange {
    pub from: Option<EntryId>,
    pub to: Option<EntryId>,
    pub reason: HeadReason, // advance | navigate | retain
}

pub struct Entry {
    pub id: EntryId,
    pub parent_id: Option<EntryId>,
    pub turn_id: Option<TurnId>,
    pub payload: EntryPayload,
}

pub trait SessionStore: Send {
    fn snapshot(&self) -> SessionSnapshot;
    fn append(&mut self, commit: Commit) -> BoxFuture<'_, Result<CommitAck, StoreError>>;
}
```

`CommitAck` contains the durable sequence, record checksum, and resulting head.
`StoreError` distinguishes locked, unsupported version, corrupt history,
recovery required, stale head/command, missing artifact, limit, and I/O failures.
The engine actor owns one store; `append` takes exclusive mutable access and
must finish sync before returning an acknowledgement. The immutable snapshot
contains the validated head/tree/indexes, current configuration, and unresolved
execution state at a specific sequence; it is not a mutable message array.

`EntryPayload` variants are `user_message`, `assistant_response`, `tool_result`,
`model_changed`, and `compaction`. Assistant response content includes ordered
tool-call blocks, preserving their position relative to text. Call proposals
are not duplicated as unrelated dialogue entries. Each assistant response
records its model, response ID, outcome, finish reason, usage, continuation,
and optional structured error (category/message/provider code/retry delay).
Completed/cancelled responses have no error; failed responses require one.
Audit events store approvals, execution intent, uncertainty, state transitions,
turn outcomes, and explicit recovery; they are not tree parents/model dialogue.

The header and every `turn_started` include a `configuration` artifact containing
the actual policy, tool definitions, instructions, model metadata, and context
settings, with their schema/implementation versions. Fingerprints without
reconstructable inputs are insufficient. An idle-only `configuration_changed`
audit event records later session-wide policy changes; navigation cannot undo
security policy. Approval/authorization events reference the stored plan artifact
whose digest binds the decision. Their exact fields are in the schema.

`head.from` MUST equal the current reconstructed head. `advance` appends a single
chain: the first new entry's parent equals `from`, each next parent equals the
previous new ID, and `to` equals the last new ID. `navigate` has no entries and
selects an existing entry or null root while idle. The target must be a safe
conversation boundary with no eligible tool-proposal group missing results;
otherwise return `head_inside_tool_exchange`. Read-only inspection may still
view such intermediate entries. Navigation never re-executes their calls.
`retain` has no entries and equal `from`/`to`, for audit-only changes. A record with neither entries nor
events nor a head change is invalid. Parent/head references must resolve to
earlier history or earlier entries in this commit; forward ancestry references
and cycles fail. A user entry may refer to a turn declared by `turn_started` in
that same atomic commit. Later entries use that existing turn identity, with
recovery-only results permitted after its interrupted terminal outcome.

Consequently, one commit atomically publishes entries, related audit events,
and their head change. A crash cannot expose an entry while committing an
unrelated head update. Reconstruct the active head by replaying complete valid
commits, not by taking the physically last tree entry. Navigation never deletes
sibling branches or changes files. New work after navigation creates a sibling.

Model selection is branch-derived: the nearest ancestral `model_changed`, or the
header's initial model. `model_changed` also records the explicit portability
policy. Audit ordering is global by record sequence and event index; entry order
within a record is array order. Times and UUID sorting are informational only.
Authorization/intent call references must identify a response in an earlier
durable record. A result with an invocation ID must refer to an intent in an
earlier durable record; intent and observed execution result cannot be fabricated
as one atomic commit. These stricter rules override the general allowance for
references to earlier entries in the same commit.

### 5.3 Writer ownership, append, and durability

Acquire a nonblocking exclusive `flock` on the actual journal descriptor before
validation/recovery or writing, and hold it for the session's engine lifetime.
Never unlink/replace that inode during normal use. Opening a second writer
returns `session_locked`. Locks are advisory: direct out-of-band edits are
outside the supported writer protocol. [See Linux locking semantics.](https://man7.org/linux/man-pages/man2/flock.2.html)

Use local filesystems with working locks and file/directory synchronization;
network filesystems and hostile concurrent file replacement are unsupported.
Creation uses create-new/no-follow, writes and syncs the header, and syncs the
parent directory before acknowledging session creation.

For every commit: validate against a staged copy of reconstructed state, serialize
the complete bounded line, `write_all` through the exclusive append descriptor,
flush any userspace buffer, and `sync_all` before publishing the committed state,
acknowledging the initiating command, or starting its authorized effect.
No batched delayed durability in v0.1. `O_APPEND` alone does not make a multiline
transaction or a short write durable. After any write/flush/sync failure, poison
the writer, stop the turn, and prohibit further effects/appends until reopen and
recovery. Do not attempt to append the same bytes again in that process.

After reopen, a complete valid LF-terminated record is replayable even if the
prior process died before acknowledging it. Validate and sync the accepted
prefix before resuming effects. Persisted `command_id` deduplicates accepted
commands: repeating the same ID/content returns the recorded acceptance/outcome;
reusing it for different content is an error. Exactly-once arbitrary external
effects are not promised. Only commits directly initiated by a command carry
its ID; subsequent internal commits carry null. `command_fingerprint` is nullable
exactly when the ID is null, and otherwise binds the submitted command. Store
its compact normalized JSON as a content-addressed artifact before committing;
the fingerprint is that artifact's SHA-256. Accepted-command receipts are the
committed sequence plus their turn/approval/recovery identifiers, reconstructable
from that commit; they do not depend on remembering an in-memory reply.

Artifacts are written to a private temporary file, hashed, file-synced, renamed
without overwriting existing differing bytes, and parent-directory-synced before
a journal record can reference them. Verify hash and length on read; never trust
an artifact-supplied path. Failed append may leave an unreferenced artifact,
which can be cleaned separately. Missing referenced artifacts are reported and
block dependent projections/replay; never substitute empty content.

File synchronization alone does not ensure a new directory entry is durable;
creation/rename also needs directory synchronization. These guarantees depend
on the storage honoring successful synchronization. [Linux fsync semantics](https://man7.org/linux/man-pages/man2/fsync.2.html)
are the baseline.

### 5.4 Torn tails and corruption

Opening reports the longest checksum/schema/semantically valid LF-terminated
prefix and the exact failing offset. A final suffix without LF is an uncommitted
tail, including a valid JSON object missing its terminator. Standard write-open
returns `recovery_required`; it never silently truncates or continues appending.

An explicit repair operation, under the same writer lock and with an expected
file fingerprint/length, copies and syncs the suffix into a private quarantine
artifact, truncates only that uncommitted suffix, syncs the journal, then appends
and syncs `tail_repaired` with the original size, valid boundary, and artifact.
This is a documented exception for uncommitted bytes, not rewriting historical
entries. Read-only inspection can examine the valid prefix and diagnostic first.
If repair crashes, the retained prefix is still authoritative; a quarantined
artifact may be orphaned. Never claim a repair audit committed before it did.

An LF-terminated bad checksum, invalid record, or corruption inside the prefix
is not a recoverable tail. Refuse writable open; expose diagnostics and require
an explicit, preserved-source salvage/export outside ordinary startup. No
automatic rollback to an older valid prefix and no guessing missing entries.

## 6. Contract 5: context and compaction

> **Implementation note.** [Decision 0006](../decisions/0006-context-builder.md)
> implements projection and its report as `ContextBuilder::build(snapshot,
> head, model_override)`, without `ProjectionInput` or a `ContextBuilder`
> trait. Instructions come from the configuration artifact; tools, options and
> continuation selection are deferred. As C01 requires, execution rejects an
> unknown context limit (`context_budget_unknown`); inspection reports it as
> unknown. Sibling entries are not listed as
> `other_branch`, and the exclusion reasons are the closed set in
> [context](../context.md#inclusion-rules). Summary generation (6.1) is not
> implemented. The proposal below is kept for reference.

```rust,ignore
pub struct ProjectionInput {
    pub session: SessionSnapshot,
    pub head: Option<EntryId>,
    pub model: ModelRef,
    pub instructions: Vec<Instruction>,
    pub tools: Vec<ToolDefinition>,
    pub budget: ContextBudget,
    pub policy: ContextPolicy,
}

pub struct ContextProjection {
    pub input: ModelInput,
    pub report: ContextReport,
}

pub trait ContextBuilder: Send + Sync {
    fn project(&self, input: ProjectionInput)
        -> Result<ContextProjection, ContextError>;
}
```

`SessionSnapshot` is immutable validated state at a journal sequence. The builder
cannot append, call a model, execute a tool, or regenerate a summary. Report
fields are session/head, journal sequence, model, policy/estimator versions,
logical-input fingerprint, included/excluded entry IDs with reason codes,
summary/source mappings, portability diagnostics, category token estimates,
configured reserves/limits, and separately labeled actual usage from prior
responses. Never present previous usage as measured tokens for a new request.
The engine exposes `inspect_context` with this report before there is a TUI.

Projection selects only ancestors of the chosen head, then applies compatible
compaction records, portability rules, instructions, pins, tools, and output
bounding. Reason codes include `recent`, `pinned`, `summary`, `covered_by_summary`,
`audit_only`, `other_branch`, `partial_response`, and `incompatible_provider`.
The report includes omitted sibling entries as `other_branch`; the engine may
page this report without losing its stable snapshot identity.

Budget satisfies `estimated_input + reserved_output + tool_headroom + safety`
`<= configured_context_limit`. Output reserve is at least requested max output;
headroom and safety are explicit nonzero policy values. Use an exact compatible
tokenizer where available; otherwise label the estimator/version and safety
margin. Estimates cannot guarantee provider token counts. A provider overflow
returns a visible diagnostic and may trigger a new recorded compaction/projection,
never re-execution of already completed tools. If protected/recent content still
cannot fit, return `context_budget_exceeded`, not silent truncation.

### 6.1 Rolling summaries

Automatic preparation requests compaction at 85% of usable estimated input
budget; a user may also request it while idle. Keep at least the two most recent
completed turns plus the current turn uncompressed. If no eligible older span
exists, explain why compaction cannot help. The strategy interface accepts a
validated completed span, target budget, prior summaries, and policy; generation
is asynchronous and separately cancellable from pure projection.

```rust,ignore
pub trait CompactionStrategy: Send + Sync {
    fn propose(&self, input: CompactionInput, control: OperationControl)
        -> BoxFuture<'_, Result<CompactionPreview, ContextError>>;
}
```

`CompactionInput` binds the snapshot/head, exact source span, prior summaries,
model/configuration, and target budget. `CompactionPreview` contains the proposed
entry, before/after reports, and those source fingerprints. It grants no journal
write or tool-execution capability; the engine validates and commits it.

A compaction entry contains `strategy`, `strategy_version`, `covered_entry_ids`
in ancestral order, `source_fingerprint`, `source_manifest`, `supersedes` summary
entry IDs, `summary` text, `model`, and a configuration fingerprint. Coverage is an exact
contiguous ancestral slice before the retained recent window; endpoint timestamps
alone are insufficient. No unresolved call, pending approval, or unfinished turn
may intersect it. Required pinned items remain outside coverage (or compaction
fails); pins cannot be silently absorbed into lossy text.

For rolling replacement, a new summary covers the old source span plus the next
eligible span, and explicitly supersedes the applicable ancestor summary IDs.
The new source fingerprint binds exact source IDs/payloads and the summaries
used to generate it. Its manifest artifact lists ordered
`{ entry_id, record_checksum, entry_index }` references for covered entries and
superseded summaries, plus the configuration fingerprint; the fingerprint is
the manifest artifact's SHA-256. Superseded summaries remain in history. Active
summaries cannot have overlapping coverage; a child summary never applies to a
sibling branch. Ignore summaries outside the selected ancestry.

Summaries use structured sections for task/constraints, decisions, files/actions,
outcomes, and unresolved work. Preserve tool identity/status information where
relevant, but do not pretend lossy generation guarantees perfect recall. The
builder checks relationships and budgets; evaluations measure retention quality.

Preview generation does not change session/workspace state or execute tools;
it may call a provider and stage private artifacts. Return an inspection diff and a
preview bound to head, sequence, model, source, and policy fingerprints. Commit
only if those inputs still match; otherwise return `stale_preview`. Manual
compaction commits explicitly on request; automatic compaction may generate and
commit under the configured policy, with before/after reports available.

### 6.2 Valid tool exchanges and partial responses

Treat a completed assistant tool-proposal set plus its results as an indivisible
projection group. All proposals get a result before another inference, including
denied/invalid/not-started cancellations. Preserve native call IDs when required.
Never move a result ahead of its proposal, include an orphan, or cut a group at
a budget boundary. A ready proposal in a partial response is not a committed
executable exchange; project any retained partial text with an interruption
diagnostic and omit ineligible call blocks. An uncertainty block requires
explicit recovery before a new inference, even after branch navigation.

## 7. Contract 6: runtime commands, events, and states

### 7.1 Application interface

`EngineHandle::submit(command_id, command)` is async and returns a correlated
acceptance/result. Commands are `StartTurn { prompt }`, `CancelTurn { turn_id }`,
`SubmitApproval { approval_id, plan_digest, decision }`, `SelectHead { entry_id }`,
`SwitchModel { model, portability }`, `PreviewCompaction`, `CommitCompaction`,
`UpdateConfiguration { snapshot }`, and `ResolveExecution { invocation_id, resolution }`. Session identity is bound
to the handle; no command changes the workspace of a running session.

`EngineHandle::snapshot_and_subscribe()` atomically returns state and an event
cursor/subscription, avoiding a snapshot/subscription race. Read APIs expose
session/tree inspection, context reports, and protected artifact access.
The TUI only submits commands and displays events; it does not make policy
decisions independently or run tools to service an approval.

Every `EngineEvent` has session ID, optional turn ID, a process-instance stream
ID, increasing event sequence, optional durable journal sequence, and a payload.
Payloads include state changes, text/reasoning deltas, context reports,
approval requests, tool started/output/completed, diagnostics, and turn finished.
`tool_started` is emitted only after durable execution intent. `tool_completed`
and `turn_finished` are emitted only after their durable commits. Live deltas
are transient, not individually journaled. Errors/timeouts before commitment
must not be displayed as durable success.

Each subscriber is independently bounded (256 events / 1 MiB). A slow subscriber
is closed with a `lagged` subscription outcome and must fetch a new snapshot;
the engine never silently drops essential events into an apparently complete
stream. A closed subscriber does not cancel the engine. Control/cancellation
uses a separate bounded priority path, not the event queue. Reconnection cannot
replay lost transient deltas; the snapshot distinguishes committed and in-flight
content. Command responses remain authoritative when event delivery is lost.

### 7.2 State transitions

The engine actor owns session state; one active turn per session. `StartTurn`
when busy returns `session_busy`, rather than silently queueing another turn.
Default limits are 32 inference responses and 128 tool proposals per turn;
exceeding a configured limit terminates visibly after persisting completed work.

The normal path is:

1. `Idle -> Preparing`: durably append the user entry and `turn_started`; then
   acknowledge the start command. Capture policy/model/configuration snapshots.
2. `Preparing -> Inferring`: build/inspect context, committing any automatic
   compaction first. Then start one model stream.
3. `Inferring -> Processing`: persist the assembled response and its outcome.
   On successful `tool_calls`, enqueue proposals in block order. On a complete
   answer/refusal/output limit, finish with a corresponding visible reason.
   A normal stop or refusal is `completed` (no failure code);
   output limit is `failed` with code `output_limit`. Provider failure is
   `failed`, and cancelled inference is `cancelled`; none execute proposals.
4. `Processing -> Authorizing`: resolve, validate, and authorize the next call.
   Invalid calls commit a result and move to the next call without an executor.
5. `Authorizing -> Executing`: an allow decision (or a matching user approval)
   plus execution intent is durably committed before dispatch. Denial commits
   a denied result. Waiting for approval remains cancellable in `Authorizing`.
6. `Executing -> Continuing`: wait for process cleanup, store/sync permitted
   artifacts, and commit the result. If more proposals remain, return to
   `Processing`; otherwise prepare the next inference using all committed results.
7. `Continuing -> Preparing`: rebuild context. There is no inference between two
   pending tools from the same response.
8. A terminal transition persists `turn_finished` with `completed`, `failed`,
   `cancelled`, or `interrupted`, emits the terminal event once, then returns
   to `Idle`. Terminal outcome is retained in snapshots, not lost when idle.

`state_changed` audit events record operational transitions. Terminal outcome
comes from `turn_finished`; applying that event clears the active turn and
sets idle unless unresolved execution/cleanup requires recovering. Such a block
has priority over idle even for an already interrupted turn. Recovery may append
results for that interrupted turn without creating another active turn. The
semantic validator checks these rules and legal transitions.
Cancellation from any active state enters `Cancelling`; recovery after process
restart enters `Recovering`. Head navigation, model switching, configuration
changes, and manual compaction commits are idle-only. Inspection and cancellation remain available
while busy. `ResolveExecution` is recovery-only.

### 7.3 Tool write-ahead ordering and crash recovery

For a dispatched call, the required order is:

1. Commit the complete assistant response containing its call proposal.
2. Validate and obtain a bound authorization decision. Commit approval request
   before presenting it; commit user decision before relying on it.
3. Commit `execution_intent` with invocation/call, decision, plan digest, backend,
   and timeout. Only after sync may the executor run.
4. Execute, fully clean up, finalize artifacts, and commit exactly one terminal
   `tool_result` referencing the proposal and invocation.
5. Only then may a subsequent inference consume that result.

If an intent is durable but no result is durable, the outcome is uncertain even
when the process may never have spawned. On restart, revoke pending approvals,
mark unfinished turns interrupted, append `execution_uncertain` for unresolved
intents, and enter `Recovering`. There is no automatic replay, including reads.
Workspace side effects do not belong only to one conversational branch, so
choosing a different head MUST NOT bypass unresolved execution recovery.

The preceding recovery behavior is the current implementation. Settled M3 D9
will, in step 6, automatically close unresolved intents whose effects are
exactly `{read}` with `cancelled`/`process_restart`, without replaying them.
Missing v1 effects never qualify. Step 3 changes no recovery behavior.

Resolution is explicit and auditable: `ObservedResult` supplies a user-observed
outcome, or `AcknowledgeUnknown { note }` records an uncertain result the user
accepts as unresolved knowledge. Append exactly one result plus
`recovery_resolved`; never rewrite an old result or create a fake success.
Only then may the engine become idle. A requested rerun is a new invocation,
new call/turn, and fresh authorization; resolution itself never executes it.
If a result already committed but the client missed it, replay that result and
do not run the operation again.

Queued proposals without execution intent are known not started. When terminating
an interrupted/cancelled turn, append their cancelled/not-started results before
allowing another inference. A cancelled provider response's proposals are
ineligible, so it needs no synthetic executed-tool results. Persistence or
cleanup failure holds the session in a blocked/recovery condition, not idle.

## 8. Conformance requirements

These are implementation gates. J01–J03 have storage tests; other cases remain
future gates. Give each
case a deterministic test with a fake clock, scripted provider, counted executor,
fault-injected journal, and temporary local storage. Ordinary CI is offline.

- **M01** Interleave two tool argument streams; reconstruct correct IDs/JSON in
  block order, and do not dispatch before terminal successful `tool_calls`.
- **M02** Setup failure, malformed JSON, EOF without terminal, cancellation,
  output limit, and refusal never authorize partial proposals; usage stays nullable.
- **M03** Stream drop/deadline and full internal buffers stop producers; cumulative
  usage updates are not added together. A second native adapter needs no runtime fork.
- **M04** Round-trip multimodal/opaque state; switch providers with reject and
  explicit omit policies; report every portability loss and invalidate continuation.
- **T01** Disabled/unknown tools, bad schemas, semantic failures, protected paths,
  and denied/expired/mismatched approvals yield zero executor invocations.
- **T02** Multiple valid calls execute sequentially; result association survives
  earlier failures/cancellation. A proposal has at most one result on its branch.
- **S01** Traverse `..`, absolute paths, symlink/magic-link/mount and hard-link
  cases, plus source replacement races. Writes remain atomic or report uncertainty.
- **S02** Real Linux sandbox tests prove blocked host/secret/socket access, denied
  network, read-only/write-scoped mounts, cleared environment, and descendant
  cleanup on normal completion/cancel/timeout. Missing Bubblewrap fails closed.
  These platform tests are required in a suitably provisioned security job before
  enabling `bash`; an unavailable prerequisite is not a successful security test.
- **J01** Round-trip the schema and checksums; reject duplicate IDs/keys, invalid
  parent/head/call references, unsupported versions, reordered sequences, invalid
  final complete records, and tampered final records as well as middle records.
- **J02** Reconstruct siblings and selected heads; reject a second writer. Inject
  failures after every write/flush/sync and before acknowledgement; recovery
  replays complete records, reports tails, deduplicates command IDs, and never
  publishes unsynced state to an executor.
- **J03** Repair only an unterminated suffix with quarantined evidence; malformed
  terminated records require preserved-source salvage. Missing/hash-mismatched
  artifacts block dependent work and cannot become empty success output.
- **R01** Crash before intent, after intent/before spawn, after mutation/before
  result, and after result/before notification. Only the last has a committed
  result; uncertainty never causes a repeat invocation. Navigation cannot evade it.
- **R02** Reject concurrent turns; accept cancellation while waiting for approval,
  backpressured inference, and execution. Emit one durable terminal outcome;
  revoked approvals cannot later trigger work.
- **C01** Reproject the same immutable inputs byte/logically identically with a
  stable report; other branches, usage estimates, and actual prior usage remain
  distinguishable. Reject unknown budgets and preserve complete tool groups.
- **C02** Manual preview leaves session/workspace unchanged; stale commit fails. Automatic rolling
  summaries retain recent turns, reference exact source IDs, supersede only
  ancestor summaries, respect pins, preserve original entries, and fit the budget
  or explicitly fail. No summary is regenerated during pure projection.
- **E01** Subscriber lag/reconnect obtains a coherent snapshot and explicit gap;
  transient deltas cannot stall cancellation or fabricate committed completion.

The [valid example journals](001/examples/README.md) are serialization fixtures,
not substitutes for crash injection, semantic validators, or sandbox tests.

## 9. Review and implementation order

Detailed defaults and record shapes are proposed by this RFC. Review changes here
before freezing the first runtime API; implementation must not quietly choose a
different persistence or security model. Dependency versions and provider wire
formats are implementation details to verify against current primary docs.

Implement common types/semantic validators and journal fixtures first; then the
headless mock lifecycle and inspection API; then safe tools, write-ahead recovery,
and the enforced process backend. Add live native providers and rolling summary
generation after these foundations, followed by the Ratatui client. Keep each
slice independently testable; no delegation or runtime implementation occurs as
part of accepting this specification document.
