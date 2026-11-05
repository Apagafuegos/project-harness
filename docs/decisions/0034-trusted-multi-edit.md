# 0034: Trusted multi-file exact edits

Status: Accepted for implementation (M9 decision B1; B2 feedback is a separate
runtime change).

Date: 2026-11-05

## Context

The M8 dogfood T4 used one broad `bash` call and a Python heredoc to rename and
edit several files. That behavior was not undoable under ADR 0032 and did not
show a per-file approval preview. The underlying need is a single authorized
operation for a mechanical set of literal replacements, not a more capable
shell or filesystem checkpoint.

Existing `write` and `edit` bind one path in one plan, capture one pre-image,
and record one `FileEffect` for the invocation. ADR 0032 groups those images by
path to create an all-target undo. A batch therefore needs a plan and captured
image that describe all target paths while retaining descriptor-rooted path
resolution, ancestor protection, hard-link rejection, symlink refusal, and
the existing approval and undo intent boundaries.

## Decision

Add a trusted `multi_edit` tool. Its argument is a bounded, nonempty ordered
list of `{path, old_text, new_text}` edits. Every old string is nonempty and
literal; each edit must find exactly one occurrence in the bytes produced by
the preceding edit to that same path. Repeated paths are grouped by their
canonical workspace-relative path, sorted lexically for deterministic target
order, and their edits retain input order. No regex, create, delete, rename,
or binary operation is added. The tool remains subject to the existing
workspace mutation policy and one approval decision binds the entire plan.

The plan records each grouped file's pre-image digest and the ordered edit
sequence. The executor re-resolves and preflights every target and computes all
replacement bytes before changing any target. It then stages and syncs one
same-directory temporary per target and rechecks the complete target set. A
preflight or staging refusal changes no workspace target. After that barrier,
files are committed by individually atomic renames in lexical path order.
There is deliberately no claim of OS-atomic multi-file commit: a failure after
one or more renames reports the exact committed paths and the remaining
outcomes; an uncertain rename is marked unknown, never retried.

Each file receives an exact approval preview showing the workspace-relative
path and its literal replacements. This is a preview of the bound arguments,
not an assertion that the file still matches; execution rechecks the bound
pre-image before staging and committing.

## Undo and persistence

Reuse ADR 0032's pre-image artifacts, durable execution intent, post-image
digests, and all-target undo preparation. Extend a v5 `FileImage` to carry one
or more path images, preserving the existing singleton fields so v4 records
remain readable. Record per-path execution effects with the result so undo
selects only paths known to have committed; interrupted/ambiguous batches stay
on the existing explicit recovery path and must identify each path from its
captured images. Undo continues to preflight and stage every selected path
before its first restore. A failure during its subsequent rename sequence is
reported honestly as partial/unknown; it is not an atomic multi-file rollback.

New records use version 5 and readers continue accepting versions 1–4. Older
plans and goldens stay unchanged. A v5 schema is added rather than relaxing
any historical schema. The new `multi_edit` operation is an effect-classified
trusted filesystem mutation and needs no new executor backend or policy
capability.

## Bash feedback

When a shell invocation has reliable evidence that it changed workspace files,
the result should say that shell changes are outside undo and recommend
`edit`/`multi_edit` for reversible changes. Do not infer changes merely from a
zero exit code, command text, or output. If the current executor cannot provide
safe evidence, omit the feedback; workspace checkpoints remain deferred.

## Consequences

- One model call and one approval cover the deterministic batch, with a
  per-file review surface.
- All targets are validated before workspace mutation, and a failed preflight
  leaves every target untouched.
- The commit phase can partially succeed because independent paths cannot be
  committed atomically. Result metadata and undo/recovery must preserve that
  fact per path.
- No directory creation, rename, delete, or general shell mutation becomes
  undoable through this tool.
- A new journal version is required to persist multi-path images and outcomes;
  existing v1–v4 records remain supported.

## Verification

Core tests cover validation, deterministic grouping, exact single-match
semantics, protected/symlink/hard-link rejection, all-target preflight, staging
before mutation, successful per-path effects, partial/unknown commit results,
durable pre-images, and undo of the batch. CLI tests cover the exact per-file
approval preview. Existing write/edit plans, goldens, and v1–v4 journal fixtures
must remain unchanged.
