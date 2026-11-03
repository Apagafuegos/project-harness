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
or core instruction changes were added in M5.

## M7 step 03 amendment: user and nested instructions

The earlier root-only scope is superseded. At creation, record the base system
instruction, then the optional user file at developer level, then the root
project file and nested project files, also at developer level. The user file
is `$XDG_CONFIG_HOME/harness/AGENTS.md`, falling back to
`$HOME/.config/harness/AGENTS.md`, independent of `--config`/`--no-config`.
It uses std fs with a 64 KiB UTF-8/regular-file bound (nonblocking open), framed
`User instructions from <path>:` with a HOME prefix abbreviated to `~`.
Failures produce one content-free warning; absence is normal.

Project reads still use `FsWorkspace::read_text`. The small read-only
`FsWorkspace::list_directories` primitive enumerates immediate real directory
entries through a strict descriptor-rooted reopen, verifies pinned identities
and ancestors, rejects symlinks, and retains a caller-bounded lexicographic set.
No ambient workspace path is reopened. The CLI owns discovery policy: skip dot
directories and `target`, `node_modules`, `dist`, `build`, `vendor`; visit at most
4,000 directories **including the root**, at depths 0 through 8. Files at depth
8 are included; deeper directories are not entered. Each reached bound warns
once, naming its limit. Unsafe directory enumeration produces one content-free
warning. Discovered relative paths are sorted before reading files.

The root framing is unchanged. Nested files use
`Project instructions from <rel>/AGENTS.md (apply to files under <rel>/):`.
Each file remains capped at 64 KiB, and user plus project **content bytes**
(excluding generated framing and base prose) are capped at 256 KiB. A file that
would exceed the total is skipped whole, never truncated. One warning names
the total cap; later smaller files can still fit. No instruction discovery,
preflight, configuration rewrite or warning happens on continuation.
`tool_availability_changed` and the two-host assembly guard are unchanged.

Evidence: `wiring::instruction_tests` covers order/framing, symlinked files and
directories, excluded names, both walk bounds, user read errors, exact per-file
cap, and whole-file total-cap skipping. Ask tests assert nested first-request
instructions and edited nested continuation immutability. The sessions/context
process test isolates HOME, asserts user/root/nested ordering, and verifies the
context report counts all four recorded instructions.
