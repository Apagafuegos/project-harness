# 0035: Bounded line-addressed and batched reads

Status: Accepted for Milestone 9 step 3; integration with the shared read plan
and executor is pending.

## Decision

Keep one trusted `read` tool and one `Read` authorization under
`workspace_read`. Extend the existing `path` form with line ranges and add a
mutually exclusive `paths` form for up to eight distinct workspace paths.
Every path in a batch carries its own byte or line range. The registry and
existing descriptor-rooted filesystem executor remain the only execution path;
there is no second reader, extra permission, or per-file authorization.

The legacy single-path byte form remains canonical and byte-compatible when
`start_line` and `line_count` are absent. Its existing byte-offset golden plans
and output format remain unchanged. Its byte defaults remain offset zero and a
1 MiB `max_bytes`.

The strict schema accepts exactly one of:

- `path`, optionally with `offset_bytes`, `max_bytes`, or a line range; or
- `paths`, an array of 1–8 objects with `path` and optional per-path range
  fields. Duplicate normalized paths are invalid.

Byte ranges use `offset_bytes >= 0` and `max_bytes` from 0 through 1 MiB. A line
range requires both `start_line >= 1` and `line_count` from 1 through 10,000;
`max_bytes` is also an optional output bound up to 1 MiB. An `offset_bytes`
field cannot appear with either line field. `start_line` and `line_count` are a
pair. For `paths`, all range fields are inside each array item, not on the root
object. Unknown keys, mixed forms, and out-of-bound values are invalid
arguments.

Line numbers are one-based and split on LF. CRLF bytes are preserved, including
the carriage return. A final non-newline-terminated line is addressable; a
trailing LF does not create a phantom line, and a start line past EOF yields an
empty range. A long line is scanned in bounded chunks. Returned text is UTF-8;
the byte ceiling clips only at a codepoint boundary, and malformed selected
bytes fail rather than being replaced.

## Aggregate output and truncation

The full result remains at most 32 KiB, including per-file headers. For a batch,
reserve up to 192 bytes for each ordered result header before allocating the
remaining payload budget. Allocate payload sequentially in input order; this
makes truncation deterministic. Each item has a zero-based index matching the
input `paths` order, its returned byte interval, `more`, and an explicit
`output_truncated` boolean. The call-level truncation flag is true if any
per-file result is truncated. The index maps to the path already present in
the call arguments, avoiding a result-size dependency on up to eight 4096-byte
path strings.

`more` means bytes remain in the file after the returned interval. In byte mode,
the caller's `max_bytes` defines the requested range, so reaching it is not by
itself tool truncation. In line mode, the selected line range is the request;
reaching `max_bytes` or the aggregate output budget before its end sets that
item's `output_truncated`. UTF-8 boundary adjustment is also reported as
truncation when it omits selected bytes.

## Policy and filesystem invariants

Planning first resolves metadata and evaluates configured and filesystem
protection, regular-file kind, and single-link status for every requested path.
Only after all paths pass does planning bind their content digests. Execution
again resolves and opens every target through the existing workspace-rooted
`openat2` path, rejecting protected objects, hard links, symlinks, and mount
crossings before hashing or returning content from any member. It then verifies
all plan-bound digests before collecting ranges. Descriptor and ancestor checks,
the existing deadline and cancellation control, and the existing 32 KiB result
boundary continue to apply.

`workspace_read` continues to decide the whole read call. The tool has no
allowance kind, grants no remembered permission, does not read outside the
authorized workspace, and does not weaken configured protected-path or `.git`
rules. Batch paths are bound in input order into the canonical plan; changing a
path, mode, range, or range bound changes the approval digest. The executor
remains sequential and in-process, preserving D9 closure.

## Evidence required for integration

The new argument parser and bounded line/output helpers have focused unit
tests. Integration tests must exercise the registered schema, policy and plan
binding, the original byte golden unchanged, line ranges over UTF-8/CRLF/EOF and
long lines, eight-path output limits and deterministic per-item truncation, and
protected/symlink/hard-link refusals. The integrated core check remains the
acceptance gate; this staged module is not compiled until the parent adds its
registry, plan and executor wiring.
