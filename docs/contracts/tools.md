# Contract 2: Tools

Status: Implemented v0.1 boundary. [RFC-001](../rfcs/001-core-types-state-persistence.md)
is refined by accepted [policy](../decisions/0009-tool-policy.md),
[pipeline](../decisions/0010-tool-pipeline.md),
[file](../decisions/0011-trusted-file-tools.md),
[sandbox](../decisions/0012-sandbox-backend.md) and
[recovery](../decisions/0013-tool-recovery.md) decisions.

## Inputs and ownership

The registry binds a tool name, description, JSON Schema, effect classification,
and trusted implementation. Session configuration selects enabled tools.
The model proposes a name/arguments; the registry does not accept a model-selected
executor, permission mode, or replacement implementation.

Initial tools are `read`, `write`, and `edit` as trusted built-in Rust operations,
and `bash` as a sandboxed subprocess. M8 adds `list`, `glob` and `grep` as
trusted, in-process, read-only built-ins on the descriptor-rooted workspace
([ADR 0031](../decisions/0031-search-and-navigation-tools.md)); they use the
existing `workspace_read` rule, protected-path rules and read-only recovery.
Future MCP and custom extensions register capabilities through this boundary;
their infrastructure is outside v0.1.

## Pipeline and output

Resolve registered/enabled name, validate schema, validate semantics and
environment, authorize, bind the exact authorized operation, dispatch, execute,
capture, and durably record a structured result. Authorize every tool, including
reads and trusted mutations. Execution is sequential in model block order even
when one response proposes multiple calls.

Results retain call/response/invocation identity, status, content, duration/exit
metadata where available, truncation information, artifact references, and — for
a trusted `write`/`edit` — a `file_effect` classification.
Distinguish invalid arguments, denial, expected operation failure, infrastructure
failure, timeout, cancellation, and acknowledged uncertainty. A nonzero command
exit is an operation result, not a provider transport failure.

## Guarantees

- Disabled/unknown tools and invalid arguments never reach an executor.
- Proposals cannot execute until fully assembled and successfully completed by
  the model response; partial inference is not executable.
- Call identity is unique, and each result is associated with its proposal.
- An approval binds the exact plan, policy revision, and call; it is not a
  reusable permission for changed arguments.
- Declared effects (`read`, `filesystem_mutation`, `process`, `network`,
  `external_side_effect`) inform policy but never replace enforcement.
- Deadlines and cancellation propagate through execution and output capture.
- Large permitted output is an artifact; bounded model-facing text cannot fill
  the entire context merely because the command printed a large log.
- Uncertain side effects are reconciled explicitly, never replayed automatically.
- A trusted `write`/`edit` captures its target's pre-image before its execution
  intent is committed, and the executor proves the computed replacement is the
  captured post-image over the captured pre-image and mode. A refused capture
  dispatches nothing, and capture never extends an approval's lifetime.
- A mutation's `file_effect` is the executor's observation of the rename commit
  point: provably not applied before the rename, applied after it even if the
  parent sync fails, and unknown when the syscall outcome is ambiguous. It is
  never inferred from the result status or text and is never shown to the model.
- A user's resolution may classify a captured mutation only as applied or not
  applied; acknowledging unknown records `unknown`, which undo never upgrades.

Shipped argument objects reject unknown keys. `list` defaults to the workspace
root and returns at most 1,000 entries. `glob` defaults to path `.` and
`include_ignored: false`, returns at most 1,000 paths and visits at most 20,000
directories including the start. `grep` defaults to path `.`,
`case_insensitive: false`, `output: matches`, `context_lines: 0`,
`max_results: 200` (at most 2,000) and `include_ignored: false`; it scans at
most 64 MiB, skips and counts files over 1 MiB, binary files and non-UTF-8
files, and cuts line text to 500 UTF-8 bytes with a marker. Every reached limit
is explicit in the result. Search walks never follow symlinks, omit `.git` and
protected paths before matching or counting them, honour root/nested
`.gitignore` under a 64 KiB cap, and are bounded by the shared tool deadline. `read` pages regular UTF-8 files
(up to 1 MiB per range); `write` defaults to no overwrite and requires an existing
parent; `edit` requires exactly one literal match, including overlapping matches.
Writes/edits accept at most 1 MiB. Bash command bytes are bounded to 64 KiB,
with workspace-relative cwd and 1–600,000 ms timeout (default 120,000 ms).
File calls have 30-second deadlines. Output is bounded to 32 KiB inline and
64 MiB captured per call; exceeding capture limits terminates the process.
[Tool usage](../tools.md) records the precise result fields and limitations.

Opaque `AuthorizedOperation` values bind the canonical plan, invocation,
decision and policy revision; model JSON cannot manufacture them. Remembered
grants are narrow configuration allowances, never a sandbox bypass. `ask` never
inherits them and refuses a continuation with a specific
`session_has_remembered_grants` code; the TUI, which recorded them, resumes
exactly the recorded allowances, and any other changed allowance is still
refused; see [the guide](../user-guide.md#continue-and-recorded-instructions).
Execution restrictions
are owned by [contract 3](execution-security.md); persistence is owned by
[contract 4](sessions.md). T01–T02 and S01–S02 are mapped to behavioral tests in
[Milestone 3 acceptance](../milestone-3-acceptance.md):
`crates/core/src/tools/tests.rs`, `runtime/tests/tool_gates.rs`,
`runtime/tests/pipeline.rs`, `tools/fs/tests.rs` and the real sandbox/tool-host
integration suites. Missing enforcement fails, never skips successfully.
