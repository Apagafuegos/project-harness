# 0022: Record base and root project instructions at session creation

Status: Accepted and implemented for Milestone 5 step 01.

Date: 2026-10-31

## Decision

Every new session assembled by `harness::wiring` records the existing
configuration artifact's `instructions` array: built-in base prose at `system`
level, then the root workspace `AGENTS.md` at `developer` level, if readable.
The existing core decoder and context builder supply these first and account
for their estimated tokens; no core/context or journal-format change is needed.

The short factual base lives in `crates/cli/src/base-instruction.txt`, included
at compile time. It identifies the absolute workspace, names actual registry
tools filtered by backend availability (or `none`), states sequential calls and
possible approval, forbids unchanged retries of denied calls, and describes
workspace-relative paths and subprocess network denial. It contains no persona,
provider-specific text, credentials or environment dumps. `bash` is named only
when the sandbox backend is available.

Availability is needed before the header's configuration artifact can be
published. Existing `create_with_tools` assembles the policy-owned host only
after that publication, and its security/protected-set stale guards must remain.
Wiring therefore preflights `ToolHost::assemble`, drops that host, builds the
instructions, and calls the unchanged `create_with_tools` for final assembly.
It compares the actual registry-filtered tool list with preflight. Capability
drift shuts the handle down and fails with `tool_availability_changed`, before
inference. This costs two probes for new sessions and can leave a created idle
journal on refusal; it never bypasses policy-owned assembly or silently infers
with inaccurate capability claims. Continuations do not preflight instructions.

`AGENTS.md` is read through `FsWorkspace::read_text` from a pinned root, with
strict openat2 beneath/no-symlinks/no-magiclinks/no-cross-mount resolution and a
**64 KiB (65,536 byte)** cap. Missing is normal. Oversized files are skipped,
never truncated; malformed UTF-8, symlinks (including escaping ones), and other
unsafe/unavailable reads are skipped with one content-free warning. The warning
names AGENTS.md, and oversize warnings name the cap. A readable file is framed
with `Project instructions from AGENTS.md:` on its own line. Nested AGENTS.md
files are not read in v0.1. No insecure fallback exists if descriptor resolution
is unavailable.

Instructions are immutable creation configuration, not a live filesystem read
on each inference. Editing AGENTS.md affects **new sessions only**. Continuation
checks only the existing approval/allowance/compaction fields, not instructions;
old sessions (including those without instructions) remain openable and keep
recorded text. `ask` emits each creation warning as one stderr line. Both clients
receive the same warning list from shared wiring.

## Evidence

Offline ask tests check exact artifact levels/text, actual request tool names,
first-request ordering, absent/oversized/invalid-UTF-8/escaping-symlink files,
exact cap acceptance, disabled sandbox, nested-file omission and edited-file
continuation. CLI context tests check included instruction count/token estimates.
The existing sentinel-secret check additionally scans the compiled base prose;
its ordinary data-root scan includes any configuration artifacts. No dependency
or core instruction changes were added.
