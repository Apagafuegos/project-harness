# 0031: Search and navigation tools

Status: Accepted and implemented for Milestone 8 step 02.

Date: 2026-11-04

## Context

The four built-ins (`read`, `write`, `edit`, `bash`) gave the model no way to
find files or text without guessing paths or starting a sandboxed shell. Bash
requires approval on every call, and each call pays for a Bubblewrap startup.
Milestone 8 decision 5 therefore adds three trusted, in-process, read-only
tools: `list`, `glob` and `grep`.

The existing trusted file layer already owns the security boundary: a pinned
workspace descriptor, strict `openat2` resolution (`BENEATH`, `NO_SYMLINKS`,
`NO_MAGICLINKS`, `NO_XDEV`), ancestor identity rechecks, and protection by
pinned `(dev, ino)` identity (ADR 0011). Search must extend that layer rather
than introduce a second resolver or an ambient path walker.

## Decision

### Registry and plans

The registry becomes seven fixed entries. `read`, `write`, `edit` and `bash`
keep their indices, definitions, schemas and golden plans byte for byte; the
new definitions are appended after `bash` as `list`, `glob`, `grep`. Their
effects are exactly `[read]` and their backend is `TrustedFs`, so
`harness ask --approve-read-only` covers them through the existing registry
classifier without a change to `ask.rs`.

Plans remain canonical version 2 JSON. Every new argument is validated,
defaulted, normalized and bound into `arguments`; `target` is `null` because a
search plan has no single pre-image. Consequently a change to any argument,
including a defaulted one, changes the plan digest and therefore the approval
ticket. A grep pattern is compiled during validation with `RegexBuilder` and a
1 MiB `size_limit`; an invalid, too-large, look-around or backreference pattern
is an `invalid_arguments` refusal before planning. Glob and ignore patterns use
the same gitignore-style syntax and a 16 KiB UTF-8 bound.

The new session policy enables all seven tools but keeps `policy_revision`
`ask-v1`. ADR 0009 requires a new revision when policy meaning or matching
changes; this step adds tools that use the existing `workspace_read` row
verbatim. Existing journals keep their recorded `enabled_tools`, so a
continuation does not gain the new tools and an unknown/disabled name is
refused with `tool_disabled`.

### Policy, protection and recovery

`list`, `glob` and `grep` follow the `workspace_read` decision only: allow,
ask or deny. They never consume or extend allowances; `Allowance::remember`
returns `None` for all three. A `path` naming `.git` or a configured protected
path is refused during planning with `protected_path`, the same code `read`
uses. Planning also refuses a target whose inherited descriptor protection is
not `None`.

During a walk, `.git`, configured protected paths and the pinned harness data
root are omitted before they can be listed, matched or counted. A protected
`.gitignore` is silently absent; it produces neither a warning nor an unreadable
skip. Renamed protected objects stay hidden because the filter is by pinned
identity and configured spelling, not by a path-prefix guess.

Search intents have effects `[read]`, so an interrupted call is closed at
restart by the existing ADR 0013 read-only closure: `cancelled` /
`process_restart` with runtime provenance, never replayed. No journal format or
record version changed.

### Walking

All traversal is descriptor-rooted through the existing `FsWorkspace`
resolution and a shared directory scanner. The M7 `list_directories` primitive
and nested-instruction discovery use the same scanner, so there is no second
resolver, no `std::fs` path walk and no `ignore`-crate walker.

- `list` enumerates one directory, reports `file`, `dir` and `symlink` entries
  in byte order, and includes a file's size. It never follows a symlink.
  Directory entries are filtered through the ignore rules; dot-directories are
  ordinary entries in a single-directory listing. Invalid UTF-8 names are
  escaped byte by byte (`\xNN`, with a literal backslash doubled).
- `glob` and recursive `grep` walk a byte-sorted frontier. Symlinks and special
  files (FIFOs, device nodes) are skipped without blocking. Dot-directories are
  skipped unless a literal component of the glob pattern names them.
- `.gitignore` rules are read through the workspace API, root and nested, with
  the M7 64 KiB cap. An oversized, non-UTF-8 or unsafe file yields one
  content-free warning and is ignored. Nested rules override parent rules and
  negations such as `!keep.rs` work. `include_ignored: true` disables these
  rules, never the protection rules. Global/user excludes are not read.
- Each directory is resolved again immediately before entry, and identities are
  rechecked after enumeration, so a directory replaced by a symlink mid-walk
  cannot be entered. A failed recheck truncates the walk with
  `walk_unavailable` rather than reading outside the workspace.

### Limits and result shape

Every limit is stated in the tool description and reported explicitly. A
result is compact JSON with a per-tool row array (`entries`, `paths` or
`matches`), `count`, `match_count`, `truncated`, `limits`, `warnings`,
`directories_visited`, `scanned_bytes`, and `skipped` by reason. Reaching any
limit sets `truncated: true` and adds a limit name; it never truncates silently.
Rows are sorted by byte order (and grep rows by path then line), with no
timestamps, so the same tree yields the same bytes.

| Tool | Bound |
| --- | --- |
| `list` | 1,000 entries |
| `glob` | 1,000 paths; 20,000 visited directories including the start |
| `grep` | shared 20,000-directory walk; 200 matches by default, at most 2,000; 1 MiB per file; 64 MiB scanned in total |
| all | shared 30-second tool deadline; cancellation checked between entries, files and lines |

Grep skips files over 1 MiB, binary files (a NUL in the first 8 KiB) and
non-UTF-8 files, counting each reason separately. Line text is cut to 500
UTF-8 bytes with a `[truncated]` marker and sets `line_bytes:500`. Binary,
oversized and non-UTF-8 paths are skipped and not exposed as text.

### Model-facing text and TUI

Inline model-facing text, including the JSON wrapper and escaping, is at most
32 KiB. Larger results publish the complete JSON as one artifact and keep a
bounded prefix inline with `captured: true` and an omitted-row count. This
reuses the executor capture/publication path from ADR 0010 and ADR 0011.

The TUI summarises `list <path>`, `glob <pattern>` and `grep /<pattern>/ <path>`
plus the result count and `truncated`, renders decoded search rows instead of
raw JSON, and expands with the existing global Ctrl-E switch. Approval previews
show every argument with its default. The base instruction prefers the new
tools over shell commands for finding files and text; it is recorded at
creation, so it changes new sessions only (ADR 0022).

### Matchers and dependencies

`regex` supplies grep. Its pure-Rust engine has no backreferences or
look-around, so a catastrophic-looking pattern cannot backtrack exponentially;
a bounded test confirms `(a+)+$` finishes quickly on hostile input. `globset`
is not added directly: the `ignore` crate's `gitignore` matcher (with the
`Gitignore`/`GitignoreBuilder` API) provides gitignore semantics for both
globs and ignore files. Only that matcher is used; its walker is never used.
`regex` was already in `Cargo.lock`; this step adds `ignore`, `globset`, `bstr`, `crossbeam-*` and their support crates. No
runtime process is spawned and no new crate owns filesystem traversal.

### Binary size

Measured on the repository release profile, Rust 1.99.1, aarch64 Linux, with
`cargo build --release -p harness`:

| Tree | `target/release/harness` |
| --- | --- |
| base `7b0c958`, before this step | 12,067,424 bytes |
| this step | 12,329,728 bytes |

The increase is 262,304 bytes (+2.17%).

## Alternatives rejected

- **The `ignore` crate's parallel walker** would have saved traversal code but
  bypassed the descriptor/ancestor/protection rules and reintroduced ambient
  path walking.
- **A path-prefix filter for `.git` and protected paths** would reveal renamed
  protected objects and miss pinned identities; the existing descriptor
  protection is reused instead.
- **`std::regex`-free hand-written matching, or a shell-out to `grep`/`rg`**
  would either add backtracking/security risk or spawn a process, contrary to
  the trusted in-process requirement.
- **Bumping `policy_revision` for new sessions** was unnecessary because the
  read rule and its matching semantics did not change. Existing sessions keep
  `ask-v1` and their recorded tool set.

## Evidence

Core unit tests cover strict schemas and defaults, new golden plans and digests,
all-argument digest binding, every `workspace_read` row, refusal of `.git` and
protected paths, allowance non-use, list/glob/grep limits and determinism, nested
ignore overrides and negation, `include_ignored`, dot-directories, symlink escape
and loops, a directory-to-symlink swap mid-walk, FIFOs and device nodes, long and
non-UTF-8 names, binary/oversized/non-UTF-8 skip counts, context at file edges,
the 64 MiB scan bound, cancellation/deadline reporting, large-output capture,
and the D9 read-only recovery closure for grep.

Client tests prove `--approve-read-only` auto-approves the new tools without
touching the classifier, old sessions continue without gaining them, and the
80-column TUI blocks and approval previews for `NO_COLOR` and ASCII mode. The
break-on-purpose results are recorded in the M8 step 02 handoff.
