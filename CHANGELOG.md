# Changelog

Notable changes follow [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and [Semantic Versioning](https://semver.org/).

## [0.2.1] - 2026-11-06

Fixes from the v0.2.0 post-release review, plus the two measurement defects it
found. No new capability and no journal migration: every v1–v4 journal,
including one a v0.2.0 binary wrote, still opens.

**Do not downgrade after using undo.** v0.2.1 writes the corrected
`omitted_calls` on an undo intent, which v0.2.0's stricter equality check
refuses with `undo_binding`. A journal where v0.2.1 recorded an undo may
therefore not open in v0.2.0. Upgrade first and stay on v0.2.1 once you have
used `Ctrl-Z` or `harness sessions undo` in a session.

### Fixed

- `harness sessions undo` no longer hangs when a restoration stops partway. An
  undo that becomes uncertain now answers its command with `undo_interrupted`,
  which names the undo and says it is resolved in the TUI; the command exits 2.
  Every undo command is answered when its commit fails or the runtime faults.
- A resolution that storage would reject is refused in the runtime instead of
  reaching the journal and faulting the session: a note over 64 KiB
  (`resolution_note_too_large`) and an `Observed` row that is not an observation
  (`undo_rows_mismatch`). One shared constant defines the note limit for the
  refusal and for storage. The session stays awaiting resolution and the TUI
  keeps the dialog and its draft.
- A terminal commit whose one entry is not `undo_note` is an invalid record
  (`undo_commit_shape`), not a panic; the same check holds through journal
  replay from a file. The undo selection no longer indexes or `expect`s on
  journal-controlled data.
- Undoing an earlier turn reports the `bash` calls of newer unconsumed turns
  skipped on the way, in the preview, the note, the result and both clients, as
  ADR 0032 requires. Writers emit the corrected list; replay accepts that list
  or the v0.2.0 one and nothing else, so journals stay on record version 4 and
  no history is rewritten.
- A TUI session whose newest turn is all `not_applied` reaches the confirmation
  modal instead of "nothing to undo", so the turn can be consumed and the turn
  before it reached, as the CLI already allowed.
- A recheck refusal on the first undo target, before any mutation, commits
  `not_applied` instead of uncertainty. Once a rename or unlink was attempted,
  uncertainty stands.
- The bash approval preview and the `ask` prompt list the configured
  `cargo_registry` as a read-only mount, so every bound mount appears in both
  clients.
- The TUI lists every remembered grant it resumed when it opens a session, at
  the top of the transcript before any turn starts, escaped and bounded. The
  recorded grants are never changed or dropped.
- An undo whose intent or worst-case terminal commit would exceed the 8 MiB
  journal record limit is refused before anything is staged or changed
  (`undo_too_large`), reserving every target, every omitted call, the maximum
  resolution note and worst-case JSON escaping.
- Dogfood measurement: a created `Cargo.lock` no longer fails the T1/T5
  no-change oracle (T2–T4 already exempted it), and a leftover scratch
  directory no longer turns a finished run into `infrastructure_failure`.

### Security

- `[sandbox.env]` refuses credential-shaped names by their last `_`-separated
  segment as well as the bare words, case-insensitively: `GITHUB_TOKEN`,
  `NPM_TOKEN`, `DB_PASSWORD`, `CLIENT_SECRET` and `AWS_SECRET_ACCESS_KEY` are
  refused by name, and a value is never echoed. No existing top-level user-config
  key matches the rule.
- ADR 0033, `execution-security.md` and the user guide now state that
  `[sandbox.env]` values are not secrets: they are recorded in the session,
  appear in the approved plan and are passed to Bubblewrap on its command line.

## [0.2.0] - 2026-11-05

Milestone 8 makes harness a daily driver: search tools, durable undo of the
agent's file edits, project toolchains in the bash sandbox, opt-in read-only
auto-approval and the friction fixes from the first dogfood measurement. Released
from the public `v0.2.0` tag; the GitHub Release carries the prebuilt archives.

### Added

- Trusted, in-process, read-only search and navigation tools: `list` (one
directory), `glob` (workspace paths matching a gitignore-style pattern) and
`grep` (pure-Rust regex content search, optional glob filter and context). They
never follow symlinks, never match or descend into `.git` or a protected path,
honour root and nested `.gitignore` files (with an explicit `include_ignored`
opt-out), state every truncation explicitly, and are ordinary `Read` plans under
the existing `workspace_read` policy (ADR 0031).
- Durable undo of the newest agent turn's trusted `write`/`edit` effects: Ctrl-Z
in the TUI and `harness sessions undo <ID> [--yes]`. Pre-image bytes are stored
before the mutation; an undo is all-or-nothing on preflight, is one durable
intent plus one durable result, refuses when any target's current content
differs, names its conflicting paths, never reverses bash or other effects
(they are reported), and records a context note so the model stops assuming the
reverted edits (ADR 0032).
- Opt-in, read-only project toolchains for `bash` through a `[sandbox]` table in
the user TOML: `toolchain_paths`, `path_prepend`, an optional `cargo_registry`
bound read-only at `$CARGO_HOME/registry`, and `[sandbox.env]` settings. Each
mount is a read-only bind at the same path, `PATH` is fixed to
the prepends plus `/usr/bin:/bin`, `CARGO_HOME` is a private per-command
directory, build output stays in the workspace's own `target/`, present
credential leaves are masked, and `doctor` plus the approval preview report
mounts and environment key names only (ADR 0033).
- `approve_read_only` now reaches the TUI as well as `ask`: with the flag or
`approve_read_only = true`, registry-declared read-only tools (`read`, `list`,
`glob`, `grep`) are answered automatically with an ordinary, non-remembered
allow bound to the same plan, and the transcript says the call was
auto-approved. `bash`, `write`, `edit` and unknown tools still open the dialog.
- The dogfood measurement harness (`crates/eval/dogfood/`, `crates/cli/examples/dogfood.rs`,
`scripts/dogfood.sh`): a dependency-free fixture, five tasks with mechanical
oracles, a PTY driver that answers approvals and enforces the deadline, journal-
derived metrics including the decision-7 read-only share, an offline
`--self-test`, sanitized evidence, an opt-in `--config FILE` and approval
metrics split into read-only and other.

### Changed

- New sessions enable `list`, `glob` and `grep`; existing sessions keep the tool
set recorded at their creation. The four original tool definitions, their
indices and their golden plans are unchanged.
- Record version is **4**. Writers emit v4; readers accept v1-v4. A v0.2.0
binary reads every v1-v3 journal unchanged, and continuation keeps writing v4
without rewriting history. A **v0.1.0 binary refuses a v4 journal**
(`unsupported_version`), so upgrade before opening a session a v0.2.0 binary has
written to.
- A sandboxed `bash` result with exit status 127 now ends with one fixed
model-facing note that the command was not found inside the sandbox, gives the
sandbox `PATH`, and says a repeat will fail the same way. Status, result code
and exit status are unchanged, and no other exit code gets the note.
- A `harness ask` continuation of a session with remembered grants still
refuses (ask never inherits grants) but now says so with
`session_has_remembered_grants` and directs the user to the TUI, which resumes
exactly the recorded allowances; a grant keeps its recorded tool/path or exact
command/cwd binding and never overrides protected paths or the sandbox.
- `harness ask` ends assistant stdout with one newline before it writes any
diagnostic, when the text did not already end with one, so merged
stdout/stderr no longer glue an answer to the `usage:` line.
- The base instruction for new sessions names the search tools and adds one
sentence against repeating an identical call whose result is already known.
- Bash plans carry the effective `[sandbox]` object, so the approved bytes bind
the mount set, search path, registry and environment; a plan bound to different
settings is refused before spawn, and a changed `[sandbox]` configuration is
refused on continuation by the existing `session_policy_mismatch` rule.

- An auto-approved read-only call submits an ordinary, non-remembered allow
bound to the same plan, journaled exactly like a manual allow, so recovery,
export and context see no special case.

### Fixed

- A failed `harness sessions export` removes the destination directory it
created when it is left empty. It never removes a pre-existing directory and
never removes anything it did not create.

### Security

- Toolchain mounts are validated at config load with the v0.1 refusal set (exit
2, naming the path): `/`, `$HOME` and its ancestors, the workspace and data
root and their ancestors or descendants, the harness config directory,
`~/.ssh`, `~/.gnupg`, `~/.codex`, `~/.aws`, `~/.config`, the sandbox's own
private mounts, and missing or non-directory values. A mount is never writable,
there is no project or workspace config, and nothing is auto-discovered.
- `[sandbox.env]` refuses credential-shaped names and every name the harness
owns, including all `CARGO_*`, `LD_*`, `OPENAI_*`, `ANTHROPIC_*`, `DEEPSEEK_*`
and `CODEX_*` names, without ever echoing a value. Network denial, the protected
paths, `.git` protection, per-call approval and descriptor hygiene are
unchanged, and a missing or failed toolchain mount fails the sandbox probe
instead of falling back to the host.
- Present credential leaves inside a mount (`credentials`, `credentials.toml`,
`.git-credentials`) are masked with the existing mechanism. An absent leaf is
not masked, cannot be created from inside the read-only mount, and leaves no
host file; creating it on the host during a command remains the documented
host-race limit (ADR 0033, decision 13).
- Undo restores only trusted `write`/`edit` effects, re-checks every target and
the workspace root before changing anything, stores pre-images as private
content-addressed artifacts that GC keeps while referenced, and never rewrites
or deletes journal history.

## [0.1.0] - 2026-11-03

Released from the public `v0.1.0` tag; the tagged source tree is the release
artifact source, and the GitHub Release carries the prebuilt archives.

### Added

- Linux-first Rust harness: a headless library, Tokio driver and Ratatui terminal
  client, plus scriptable `ask`, context/journal inspection and host `doctor`.
- Checksummed append-only JSONL session trees, durable head navigation, private
  content-addressed artifacts, writer locks and explicit torn-tail repair.
- Native OpenAI Responses (API key or existing read-only Codex login), Anthropic
  Messages and DeepSeek's Messages-compatible profile; bounded HTTP/SSE streams,
  nullable usage, owner-scoped native replay and fixture conformance suites.
- Sequential `read`, `write`, `edit` and sandboxed `bash`, exact-plan approvals,
  durable execution intent/results and explicit uncertain-effect recovery.
- Read-only context reports, whole-turn pins/exclusions, preview-first and
  automatic compaction, rolling summaries, model-free observation masking and
  structured state with an independent summary model.
- Strict user TOML defaults and bounded recorded user/root/nested AGENTS.md
  instructions; no workspace configuration or credentials in TOML.
- Session catalog, Markdown/all-branches and verified journal exports,
  journal-only deletion and explicit delayed artifact garbage collection.
- TUI help, history, word/grapheme editing and wrapping, atomic bracketed paste,
  follow mode, activity/status, scrollable modals, GFM Markdown, bounded bundled
  syntax highlighting, compact tool blocks and approval diffs/previews.
- MIT OR Apache-2.0 licensing, third-party licence/source notices, two-target
  Linux musl release workflow, checksums and reproducible archive packaging with
  a mandatory user guide. Dispatch dry runs never publish.
- Offline evaluation and security gates, sanitized opt-in live evidence,
  user guide and milestone acceptance/audit documentation.

### Changed

- New sessions explicitly record `observation_masking`; old omitted strategies
  remain `rolling_summary` and continuation retains recorded policy.
- Explicit `ConvertSupported` switching converts incompatible displayable media
  and tools only in model context, with per-part reports and converted-input
  fingerprints; it never translates opaque state or rewrites history.
- Context controls and model-free compactions use record version 3; versions 1
  and 2 remain readable and continue as v3 without migration.
- Release builds use stripped thin-LTO musl binaries with unwinding retained for
  terminal restoration; `NO_COLOR` and ASCII terminal decorations are supported.

### Fixed

- Coherent TUI reset replaces stale inspector identity/report across subscriber
  lag and runtime replacement; late replies cannot repopulate old panels.
- Summary requests end with an explicit fresh user request rather than accidental
  assistant prefill on Messages endpoints.
- Artifact collection selection/unlink is serialized with publication/dedupe
  reuse; dedupe refreshes mtime and GC retains the 24-hour grace window.
- Journal export checks copied artifact hashes, not only previously verified
  sources and lengths; existing output paths are never overwritten.
- Overlong trusted-file paths are bounded refusals rather than infrastructure
  failures. Ordinary test authentication failures cannot call a live endpoint.

### Security

- Descriptor-rooted openat2 file containment, preimage revalidation and atomic
  replacement; Bubblewrap for every model-spawned executable, network denied,
  explicit mounts, environment allowlists and supervised descendant cleanup.
  Missing enforcement always fails closed, without a host fallback.
- Fixed the TasksMax capability probe's inherited environment and arbitrary
  provider error-code/type strings leaking into diagnostics or durable state.
- Accepted pre-release audit reviewed public reachable history, credentials,
  policy, filesystem/sandbox boundaries, session maintenance and release supply
  chain; all four medium findings were fixed with behavioral regressions.
- Dependency audit found zero known vulnerabilities at its recorded database
  revision; bincode's unmaintained advisory remains a documented low maintenance
  risk. This is not a guarantee of no vulnerabilities.
- Residual limits include no seccomp, host-dependent TasksMax, shell hard-link/
  host-race and absent-path limits, unencrypted private artifacts, remembered-
  grant continuation refusal and no signed provenance/reproducible binary proof.
  See the full security audit in the public repository.
