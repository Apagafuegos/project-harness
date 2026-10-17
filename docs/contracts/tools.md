# Contract 2: Tools

Status: High-level behavior provisionally agreed; concrete interface and result
format proposed in [RFC-001](../rfcs/001-core-types-state-persistence.md).

## Inputs and ownership

The registry binds a tool name, description, JSON Schema, effect classification,
and trusted implementation. Session configuration selects enabled tools.
The model proposes a name/arguments; the registry does not accept a model-selected
executor, permission mode, or replacement implementation.

Initial tools are `read`, `write`, and `edit` as trusted built-in Rust operations,
and `bash` as a sandboxed subprocess. Listing/search tools may follow later.
Future MCP and custom extensions register capabilities through this boundary;
their infrastructure is outside v0.1.

## Pipeline and output

Resolve registered/enabled name, validate schema, validate semantics and
environment, authorize, bind the exact authorized operation, dispatch, execute,
capture, and durably record a structured result. Authorize every tool, including
reads and trusted mutations. Execution is sequential in model block order even
when one response proposes multiple calls.

Results retain call/response/invocation identity, status, content, duration/exit
metadata where available, truncation information, and artifact references.
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

The RFC specifies argument objects, literal single-match editing, write defaults,
opaque authorized-operation types, scheduling, and limits. Execution restrictions
are owned by [contract 3](execution-security.md); persistence is owned by
[contract 4](sessions.md). Conformance cases T01–T02 and S01–S02 gate implementation.
