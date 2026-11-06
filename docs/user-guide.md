# User guide — v0.2

A Linux coding-agent harness with a terminal client and a scriptable `ask`
command. Session history is append-only; model context is a separate projection.
Branching and compaction never undo workspace changes.

## Install a prebuilt binary

The repository is public and licensed `MIT OR Apache-2.0`. v0.2.1 assets are
built and published from the `v0.2.1` tag by the release workflow; see the
[GitHub Release](https://github.com/Apagafuegos/project-harness/releases). A
missing archive means that release run has not finished yet — `v0.1.0` is the
latest published release as this guide is written. No Rust toolchain is needed
to run a prebuilt binary.

Choose `x86_64-unknown-linux-musl` for Linux x86_64 (`uname -m`: `x86_64`), or
`aarch64-unknown-linux-musl` for Linux ARM64 (`aarch64`). Download the matching
`harness-0.2.1-TARGET.tar.gz` and `SHA256SUMS` from the **same release**. In the
download directory, for example on ARM64:

```sh
grep '  harness-0.2.1-aarch64-unknown-linux-musl.tar.gz$' SHA256SUMS > selected.sha256
test -s selected.sha256
sha256sum -c selected.sha256
tar -tzf harness-0.2.1-aarch64-unknown-linux-musl.tar.gz
tar -xzf harness-0.2.1-aarch64-unknown-linux-musl.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 755 harness-0.2.1-aarch64-unknown-linux-musl/harness "$HOME/.local/bin/harness"
export PATH="$HOME/.local/bin:$PATH"
harness --version
harness --help
harness ask --help
```

Substitute the x86_64 target in every filename on that architecture. Keep the
archive's licences, changelog and `third-party-notices/` when redistributing.
Checksums detect changed bytes, not the authenticity of a compromised release;
signatures/attestations and independently reproducible binary builds are not
provided. Reproducible archive packaging is a narrower guarantee.

## Host requirements and doctor

Static linking does not supply host isolation. Trusted file tools require Linux
`openat2` with beneath/no-symlink/no-mount resolution. External programs require
Bubblewrap at `/usr/bin/bwrap`, `/bin/bash` and usable user/mount/PID/network
namespaces. A systemd user manager can additionally enforce TasksMax; without
it the process-count limit is unavailable. A real terminal on stdin **and**
stdout is required for the TUI (minimum 20×5); pipes use `ask`.

```sh
harness doctor
harness doctor --workspace "$PWD" --data-root "$HOME/.local/share/harness"
harness doctor --help
```

Doctor reports JSON; exit zero means the report printed, **not** that every
capability is available. Inspect file/sandbox availability and reason codes.
Install the distribution's Bubblewrap package, then consult the administrator
about user-namespace or AppArmor restrictions. The reviewed CI AppArmor profile
in the repository is CI provisioning, not an instruction to disable host policy.
`--bwrap /absolute/path` lets doctor probe another installed binary; it does not
change the clients' default backend. Never fix unavailable isolation with root
execution, a host shell fallback, disabling security, or a network bypass.
Unavailable tools are not offered; missing enforcement fails closed. All shell
network access is denied, including dependency downloads. Provider HTTP runs in
the trusted harness, not in a tool sandbox.

Use a local filesystem with functioning locks and file/directory sync. Keep the
private data root outside the authorized workspace. No network filesystem or
hostile same-user process protection is promised. See the
[security audit](https://github.com/Apagafuegos/project-harness/blob/main/docs/security/v0.1-audit.md)
for the full threat model and residual risks.

## Provider and authentication

The default is OpenAI with an **existing Codex login**, read-only. If you use
Codex, authenticate with its own `codex login` command first. Harness reads the
Codex login at `$HOME/.codex/auth.json` read-only; it does not consult
`CODEX_HOME` and does not refresh or rotate that login. An expired login requires
logging in again outside harness.

For OpenAI API-key access choose `--provider openai --auth api-key` and set
`OPENAI_API_KEY` in your shell's private environment. Anthropic uses
`--provider anthropic` with `ANTHROPIC_API_KEY`; DeepSeek uses
`--provider deepseek` with `DEEPSEEK_API_KEY` (its Messages-compatible endpoint).
Do **not** supply `--auth` for Anthropic/DeepSeek. Defaults are `gpt-6.1-sol`
(Codex), `gpt-5.5` (OpenAI API key), `claude-sonnet-5-5` and `deepseek-flash`.
`--model MODEL` overrides the choice. Provider availability depends on your
account. Anthropic live compatibility and live cross-provider switching remain
partial (no Anthropic key was supplied for v0.2 either); offline fixtures are
not live proof.

Do not put credentials in TOML, AGENTS.md, prompts, command examples or exports.
Harness does not load `.env` files and has no credential UI. Provider diagnostic
codes/messages are filtered, but ordinary user/model/tool text is **not** a
secret scrubber. Inspect anything before publishing it.

## First turn and terminal session

These examples make real, potentially billed requests when you run them:

```sh
harness ask --workspace "$PWD" "Explain this project's build; do not change files"
harness ask --workspace "$PWD" --provider deepseek "Summarize the project"
# From the workspace directory, with terminal stdin/stdout:
harness
```

`ask` writes assistant text only to stdout and diagnostics/session identity to
stderr. If the answer does not end with a newline, `ask` writes exactly one
newline to stdout before its first diagnostic (the `usage:` summary, an error or
exit), so a merged terminal never glues both streams onto one line. `-` reads the prompt from stdin, never an approval. Exit statuses:
0 completed, 1 turn failure, 2 usage/configuration/refused switch,
3 authentication, 130 cancelled. There is no automatic inference retry.

Storage defaults to `$XDG_DATA_HOME/harness`, otherwise
`~/.local/share/harness`; `--data-root /absolute/path` selects another private
root. Journals live in `sessions/`, shared content-addressed bytes in
`artifacts/`. Keep both for a complete backup: opaque native state is sensitive
and not encrypted by harness. Only one engine can write a session at a time.

New sessions also offer three trusted, in-process read-only tools: `list` for one
directory, `glob` for paths matching a gitignore-style pattern, and `grep` for a
pure-Rust regular-expression content search with an optional glob filter and
optional context. They never follow symlinks, omit `.git` and protected paths,
honour root and nested `.gitignore` files, state truncation explicitly, and can
be auto-approved with `--approve-read-only`. Older sessions keep the tool set
recorded when they were created; start a new session to use them.

## User TOML defaults

`$XDG_CONFIG_HOME/harness/config.toml`, falling back to
`~/.config/harness/config.toml`, is the only automatically loaded config.
There is **no project/workspace config**. Precedence is explicit command-line
flag, then user TOML, then built-in default. No tilde or environment expansion
occurs inside values. A harmless example:

```toml
provider = "openai"
auth = "codex"
compaction_strategy = "observation_masking"
approve_read_only = false
switch_policy = "reject"
```

Other supported keys are `model`, `summary_model` and `data_root` (strings).
`approve_read_only` is boolean and applies to **both clients**:
`--approve-read-only` (or the TOML key) auto-approves only the registry's
trusted read-only tools (`read`, `list`, `glob`, `grep`) in `ask` and in the TUI.
Startup `switch_policy` stays **ask-only**; it may coexist in a shared file but
does not replace the TUI's reject-first model dialog. The other keys apply to
both clients. Auth still must be applicable to the selected provider.

```sh
harness config path
harness config show
harness config show --model gpt-6.1-sol
harness --config /absolute/path/defaults.toml config show
harness --no-config config show
harness --no-config ask --workspace "$PWD" "Explain the project"
```

`config path` reports the chosen path and whether it exists, and validates any
file it would load; `show` labels each effective value with `flag`, `config` or
`default` (booleans are quoted for display, while TOML uses `true`/`false`).
Neither command loads credentials or opens a session.
An absent default file is normal. An explicit missing file, unknown key, wrong
type, unreadable/non-UTF-8 file or file over 64 KiB exits 2. Credential-like keys
are specifically refused without echoing their values. `--no-config` ignores
TOML, including its errors; it does not disable user instructions.

### Project toolchains in the bash sandbox

By default the bash sandbox binds only system paths, with `PATH=/usr/bin:/bin`,
so a rustup toolchain or another project toolchain outside `/usr` is
unreachable and `cargo test` fails with exit 127. A `[sandbox]` table in the
**user** TOML only (never a project config) adds opt-in, read-only mounts:

```toml
[sandbox]
toolchain_paths = ["/home/you/.rustup/toolchains/1.99.1-aarch64-unknown-linux-gnu"]
path_prepend = ["/home/you/.rustup/toolchains/1.99.1-aarch64-unknown-linux-gnu/bin"]
cargo_registry = "/home/you/.cargo/registry"

[sandbox.env]
RUSTUP_TOOLCHAIN = "1.99.1"
```

With no table, the sandbox is exactly v0.1's. Each `toolchain_paths` entry is a
read-only bind at the same absolute path; `path_prepend` entries come before
`/usr/bin:/bin` and must resolve inside a mounted toolchain or `/usr`;
`cargo_registry` is bound read-only at `$CARGO_HOME/registry`. Nothing is ever
writable, and nothing is discovered automatically. Mount the toolchain's own
`bin` (the real `cargo`/`rustc`), not `~/.cargo/bin` (rustup proxy shims that
need the rest of your home), and give the registry as `cargo_registry` rather
than mounting `~/.cargo`, so no credential file is inside a mount.

`[sandbox.env]` accepts ordinary names; it refuses credential-shaped names —
the whole name or its last `_`-separated segment is `token`, `secret`,
`password`, `passwd`, `credential`, `credentials` or `key`, so `GITHUB_TOKEN`,
`DB_PASSWORD` and `AWS_SECRET_ACCESS_KEY` are refused — and names the harness
owns, including **all `CARGO_*` names** (the harness sets `CARGO_HOME` to a
private per-command directory). Pass `cargo test --offline` instead of setting
`CARGO_NET_OFFLINE`; the sandbox denies network anyway. **These values are not
secrets:** they are recorded in the session, appear in the approved plan and are
passed to Bubblewrap on its command line, so anything you would not print in a
terminal does not belong there.

Every mount and prepend is checked when the config loads, and a refusal exits 2
naming the path: `/`, `$HOME` or any of its ancestors, the workspace, the data
root, anything inside either, the harness config directory, `~/.ssh`, `~/.gnupg`,
`~/.codex`, `~/.aws`, `~/.config`, the sandbox's own private mounts, and a
missing or non-directory value. A changed `[sandbox]` configuration is a policy
change, so continuing a session with one is refused with
`session_policy_mismatch`; re-run with the recorded settings.

Present credential leaves inside a mount (`credentials`, `credentials.toml`,
`.git-credentials`) are masked. An absent one cannot be created from inside the
read-only mount. `harness doctor` shows each mount, its existence and
readability, its credential-mask statuses, the prepends and the environment
**key names only**; the bash approval preview shows the same scope. Build output
is not redirected: cargo writes the workspace's own `target/`, so repeated
commands reuse it.

One practical consequence: running a project's tests inside a dependency-free
crate without a committed `Cargo.lock` creates one, because cargo writes a lock
file. That is ordinary build output in the workspace, not a harness change.

## Continue and recorded instructions

```sh
harness sessions list
harness ask --workspace "$PWD" --session SESSION_ID "Now explain the tests"
printf '%s\n' 'Continue the explanation' | harness ask --workspace "$PWD" --session SESSION_ID -
harness --session SESSION_ID
```

Replace `SESSION_ID` with the UUID from the catalog. Continue from the same
workspace/data root. Omitted model/provider selections retain the branch model;
explicit values (including TOML values) request a switch. Recorded strategy is
retained unless explicitly selected; a differing continuation strategy is
refused, not installed. Continue with the same summary-model/automatic policy
and auth/provider availability. `session_policy_mismatch` refuses any other
changed policy field. Remembered grants are client-specific: `ask` never uses
them, so a session that records one refuses an `ask` continuation with
`session_has_remembered_grants` (exit 2) and says to continue it in the TUI.
The TUI records those grants and resumes exactly them, and a grant never widens
past the protected paths or its recorded tool/path or exact command/cwd binding.
When it opens a session with recorded grants it lists every resumed one at the
top of the transcript, before any turn starts — `tool · path: "…"` or
`bash · command: "…" · cwd: "…"`, escaped and bounded — and says they came from
this session's journal. A journal copied from elsewhere carries its grants with
it, and that list is how to notice them; nothing is dropped or changed by it. A
session with no grants lists nothing. Do not edit history or weaken checks; the
read-only catalog/inspection/export still provide access.

New sessions record, in order: built-in system instructions (workspace, actual
tools, one call at a time with approval, no unchanged retry of a denied call, and
no repeat of a call whose identical result is already known); optional user
`$XDG_CONFIG_HOME/harness/AGENTS.md` (fallback `~/.config/harness/AGENTS.md`);
root project AGENTS.md; and nested project AGENTS.md files in lexicographic path
order, framed with their directory scope. All but the base are developer
instructions. Project discovery is descriptor-rooted and never follows symlinks.
It skips dot directories and `target`, `node_modules`, `dist`, `build`, `vendor`;
it visits depths 0–8 and at most 4,000 directories including root. Files have a
64 KiB cap; user plus project content has a 256 KiB total cap. Unsafe/oversized
files are skipped whole with content-free warnings; reaching walk/total limits
warns. The user-owned instruction file can follow symlinks. Nested scope is
instruction framing for the model, not an execution permission.

Instructions are recorded **once at creation**. Edits affect new sessions only;
continuation never re-reads them. Treat instruction text as recorded context,
not trusted policy, and never put secrets there.

## Approvals, recovery and keys

Every proposed tool is validated and authorized; calls execute sequentially.
The clients start with workspace reads, mutations and processes asking approval.
In `ask`, approvals use `/dev/tty` and only literal `y` allows; no terminal means
deny every call. `--approve-read-only` requires a terminal and only auto-approves
registry-declared trusted filesystem reads, not shell commands or mutations.
It is not a secret-content filter and can include readable `.git` metadata.

In the TUI an approval captures keys: `a` allow once, `d` deny, `r` allow and
remember a narrow tool/path or exact command/cwd grant, Esc **denies**. `v`
shows raw arguments; scrolling/paste never answers. `harness` also accepts
`--approve-read-only` (or `approve_read_only = true` in the user TOML): only
registry-declared trusted read-only tools (`read`, `list`, `glob`, `grep`) are
then answered automatically, with an ordinary allow bound to the same plan that
is never remembered, and the transcript says the call was auto-approved as
read-only. `bash`, `write`, `edit`, unknown and non-registry tools always open
the dialog, and the setting never creates a grant or changes the recorded
policy. Inspect command/cwd/timeout,
network denial, file previews/diffs and plan digest before approving. Remember
never overrides protected paths or sandbox enforcement and has the continuation
limitation above. Tickets bind an exact plan/revision and expire after five
minutes.

Every `bash` call runs in a Bubblewrap sandbox with network denied. When a
sandboxed command exits **127**, its result text ends with one fixed line:
`note: exit 127 — the command was not found inside the sandbox (PATH=…);
repeating the same command will fail the same way.` The PATH shown is the
sandbox's own, including any `[sandbox] path_prepend` entries, so an absent
toolchain is visible instead of guessed. Only exit 127 gets the note; the
status, result code and exit status are unchanged.

Execution intent is durable before dispatch and results before dependent
inference. An interrupted mutation with no durable result is uncertain, not
silently retried. The recovery dialog requires your observed result or explicit
acknowledgement of unknown effects; file-image hints are advisory, not proof.
For an interrupted `write` or `edit`, whose pre-image was captured, the
observation choices state what happened to the **file** — the change was applied
or it was not — because only you can say that; the recorded result carries your
statement and the undo rules follow from it. For any other call the choices
state the tool's status. Nothing is guessed from the evidence hints.
Choosing another branch cannot bypass uncertainty. Resolution never reruns a
tool; any later rerun is a fresh authorized operation. Read-only restart closure
is a narrow special case, not an observed success. Torn journal tails require
explicit `harness journal inspect` then `harness journal repair` with the
reported fingerprint/size; complete corruption is never silently repaired.

| Key | Action |
| --- | --- |
| F1 / `?` with empty draft | Help; F1/Esc closes back to the underlying dialog |
| Enter / Shift-Enter or Alt-Enter | Send while idle / newline |
| Arrows, Home/End; Ctrl-Left/Right | Edit by character/line or word |
| Up at first line / Down at last line | Prompt history / restore saved draft |
| Ctrl-W / Ctrl-U | Delete previous word / to line start |
| PgUp/PgDn; Ctrl-Up/Down | Scroll page / line |
| End on empty draft / Ctrl-End | Follow output / follow while preserving draft |
| Esc | Cancel active turn; idle dismiss error; close dialog (approval denies) |
| Ctrl-C | Cancel; second press during the same busy turn quits; idle asks |
| Ctrl-D | Quit on an empty prompt, draining cancellation/storage |
| Ctrl-R / Ctrl-E | Expand reasoning summaries / tool output |
| Ctrl-O / Ctrl-T / Ctrl-L / Ctrl-K | Sessions / branches / models / context |
| Ctrl-Z | Preview the undo of the newest agent turn's file edits (y confirms, Enter/n/Esc cancels) |

Prompt history is memory-only, seeded from the current branch and reset on
session replacement. Bracketed paste is atomic editable text, never a send or
approval. Mouse capture is off for terminal selection. `NO_COLOR` disables
colour (bold/dim remain); `TERM=dumb` selects ASCII decorations. Markdown/GFM
and fenced-code highlighting use bundled grammars and ANSI colours. Unknown
languages, disabled colour, or blocks above 2,000 lines/256 KiB render plain.
Tables/code can hard-wrap; raw HTML is inert. Terminal state is restored on
ordinary exit, errors, panics and handled signals; SIGKILL cannot run cleanup.

## Sessions, branches and models

Ctrl-O lists sessions; Up/Down selects, Enter opens, `n` creates. Handover is
idle-only and restricted to the process's workspace; it durably releases the
old writer. Other workspace sessions can be listed but not opened here.
`e` exports Markdown to a new path; `d` asks y/N before permanent journal-only
deletion. The current open session cannot be deleted.

Ctrl-T shows branches. Enter selects a safe head; `e` edits a user prompt and
sends it as a sibling, preserving the original. This changes conversation, not
filesystem state. `p` pins the selected finished whole turn, `x` excludes it;
press the same key again to reset. Controls are idle-only append-only entries,
apply on that ancestry, and cannot target already covered turns. Pins remain
verbatim; exclusions remain readable in history but never reach inference or
a summarizer. Neither grants unlimited context: oversized protected content
fails explicitly.

Ctrl-L selects a model with Reject first. A portability report names affected
parts without opaque payloads. `o` explicitly omits **optional** state only;
`c` explicitly converts supported incompatible media/tool dialogue to bounded
deterministic text when offered. Opaque state is never translated. Conversion
is projection-only: switching back to a capable model restores originals.
Required incompatible content cannot be omitted under `omit-optional`.
For `ask`, an explicit continuation switch uses:

```sh
harness ask --workspace "$PWD" --session SESSION_ID --model gpt-6-astra --switch-policy reject "Continue"
```

Choose `omit-optional` or `convert-supported` only after understanding the loss.
No stale continuation is carried across a switch. Not all catalog models support
all media/options, and unknown budgets block inference rather than guessing.

## Inspect and compact context

Ctrl-K shows estimates, reserves, included/excluded reasons, controls, conversion
markers and fingerprints. `m` previews another model without switching.
Standalone read-only inspection (after closing the writer) is:

```sh
harness context /absolute/data/root SESSION_ID
harness context /absolute/data/root SESSION_ID --context-tokens 500000
```

An optional `--head ENTRY_ID` previews ancestry and `--model PROVIDER/MODEL`
overrides the target. Without `--context-tokens` this standalone command reports
an unknown model limit; it is not the live client's merged catalog. Estimates
are not actual tokenizer counts; prior provider usage is separately labelled.

New sessions explicitly record `observation_masking`: deterministic, model-free,
keeping dialogue/tool identities while replacing tool-result bodies. Older
journals with an omitted strategy still decode as `rolling_summary`.
`rolling_summary` produces a model narrative; `structured_state` adds bounded
deterministic file/command/requirement extracts before that narrative. Both need
an independently configured summary model. Choose on **creation**, for example:

```sh
harness --compaction-strategy rolling_summary --summary-model gpt-6.1-sol
harness ask --workspace "$PWD" --compaction-strategy structured_state --summary-model gpt-6.1-sol "Explain the project"
```

There is no CLI/TUI strategy-switch shortcut. The headless typed
`SetCompactionStrategy` command changes it only while idle through a recorded
configuration change, invalidates previews, and requires a summary model for
model-based strategies. Continuation flags are not that command.

Manual compaction: Ctrl-K, `c` generates a preview; inspect the exact summary,
source IDs/manifests, before/after estimates and fit result. Enter commits only
an adequately rendered preview; Esc cancels/discards it without a journal write.
A stale preview must be regenerated explicitly. Model-based generation is a
separate potentially billed request; masking makes none.

Automatic compaction is enabled by client `--summary-model` configuration, not
by the masking default alone. It triggers after a completed turn at 85% of
usable estimated input budget, or while idle before a new message would
overflow. It never compacts inside a tool-loop turn. At least two recent
completed turns remain; controlled turns are skipped. Each trigger gets one
attempt, with explicit failure if no eligible span can help. Reopen and failure
do not create retry loops. All strategies retain originals and exact source
provenance. Masking is lossy and may grow prose-heavy context; the small recorded
experiments do not establish a general quality winner or any workspace rollback.

## Undo the agent's file edits

The agent's trusted `write` and `edit` effects can be reverted, one turn at a
time. Pre-image bytes are captured before each mutation, so undoing restores a
file the agent overwrote and removes a file it created. Bash and every other
effect are never reversed, and the report lists them.

In the TUI press **Ctrl-Z** while idle: a modal names every file it would
restore or remove and every call it will not reverse. `y` confirms; **Enter
defaults to no**, and `n`/Esc cancel without recording anything. The command
path is the same as `harness sessions undo`:

```sh
harness sessions undo SESSION_ID           # asks y/N on a terminal
harness sessions undo SESSION_ID --yes     # required without a terminal
```

Undo is all-or-nothing on preflight: if any target's current content differs
from what the agent left, or a protected path or workspace check fails, nothing
changes and every conflicting path is named (`undo_conflict`, exit 2). The
report names every effect it does not reverse: the target turn's bash calls and
those of newer turns skipped on the way to it. Nothing is offered when the
branch has no unconsumed turn with a trusted file edit (`undo_nothing`), when
the turn predates pre-image capture (`undo_unavailable_legacy`), or when an
effect's application is unknown (`undo_uncertain_effect`). A newest turn whose
every mutation is proven `not_applied` changed nothing, so confirming it
consumes that turn without touching a file and the next Ctrl-Z reaches the turn
before it.

An undo is one durable intent and one durable result, so an interrupted undo is
reported as uncertain rather than guessed at. The command still answers:
`harness sessions undo` exits 2 with `undo_interrupted` and says the undo is
resolved in the TUI, where you state per-file observations and a note or
acknowledge that the outcome is unknown. That dialog has two panes: the note
takes every printable character, and **Tab** switches to the file list, where
`r` restored, `x` removed, `o` other and `u` unknown set the selected row, `a`
accepts that all are unknown, and `←`/`→` switch between observing and accepting
unknown. Typing a note therefore never changes an observation.

A resolution that storage would reject
is refused in the dialog instead of faulting the session — a note over 64 KiB
(`resolution_note_too_large`) or a row that is not an observation
(`undo_rows_mismatch`) — and the dialog keeps your note. An undo whose records
would exceed the 8 MiB journal limit is refused before anything is staged
(`undo_too_large`). Nothing is retried. Until an interrupted undo is resolved
the session refuses new turns. History is append-only, and the model's context
records the undo through a user note, so it never assumes the reverted edits are
present. There is no redo.

**Do not downgrade after using undo.** v0.2.1 records the corrected list of calls
an undo does not reverse, and a v0.2.0 binary refuses that intent with
`undo_binding`. A session in which v0.2.1 recorded an undo may not open in
v0.2.0; every other v0.2.1 journal still does. Upgrade to v0.2.1 and stay there
once you have used `Ctrl-Z` or `harness sessions undo`.

## Export, delete and reclaim space

Close running session engines first; maintenance uses nonblocking journal locks.
All commands resolve `--data-root`, user TOML, then the normal storage default.

```sh
harness sessions list
harness sessions --data-root /absolute/data/root list
harness sessions export SESSION_ID --format markdown --output conversation.md
harness sessions export SESSION_ID --format markdown --all-branches --output branches.md
harness sessions export SESSION_ID --format journal --output /absolute/new-backup
harness sessions delete SESSION_ID --yes
harness sessions gc --dry-run
harness sessions gc --yes
```

Export is read-only and refuses existing outputs. Markdown defaults to the
active branch; `--all-branches` names every branch and shared history. Text is
verbatim, media is referenced, opaque state omitted. Journal export creates a
new private data root with the verified journal and all referenced artifacts,
checking the actual copied bytes; use it for replayable backups. It contains
sensitive native/session data. A failed journal export may leave an empty new
directory. Never publish an export without reviewing its contents.

Deletion is irreversible and removes **only the journal**, never shared
artifacts, under the exclusive writer lock (`session_locked` if open). Captured
pre-images are artifacts too: they are kept while any journal references them,
including after an undo and on every branch, and are removed only by `gc`.
Without a terminal, deletion and real GC need `--yes`; dry-run does not.
GC reclaims only unreferenced private regular artifacts older than the **24-hour
grace window**. Dedupe reuse refreshes mtime, and a shared publication/collection
lock serializes reuse with selection/unlink. **Any unreadable, locked, corrupt,
torn or unsupported journal refuses the whole GC run** (`gc_journal_unreadable`),
rather than guessing references. Symlink/unsafe artifacts are left alone.
Dry-run lists candidates and byte totals without removing anything. There is
no automatic retention policy.

## Boundaries and further reading

No seccomp is installed; TasksMax is host-dependent; uninterruptible kernel I/O
can delay cleanup. Shell mounts do not defend all hard-link aliases or hostile
host races; absent protected paths cannot be masked in advance and detection is
not rollback (including an initially absent `.git`). No unrestricted fallback
is safe. See the full audit linked above, the
[documentation index](https://github.com/Apagafuegos/project-harness/blob/main/docs/README.md)
and [M7 acceptance](https://github.com/Apagafuegos/project-harness/blob/main/docs/milestone-7-acceptance.md)
and [M8 acceptance](https://github.com/Apagafuegos/project-harness/blob/main/docs/milestone-8-acceptance.md).
Subagents, MCP/plugins, remote workspaces, semantic memory, filesystem checkpoints
and constrained shell networking are planned beyond v0.2. A parallel read-only
tool executor is deferred, not planned.
