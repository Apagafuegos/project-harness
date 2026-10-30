# 0020: Minimal headless durable `harness ask`

Status: Implemented for Milestone 4 step 08. Ratatui remains Milestone 5.

Date: 2026-10-30

## Decision and boundary

`harness ask [--data-root DIR] --workspace DIR [--provider openai]
[--auth codex|api-key] [--model MODEL] [--summary-model MODEL]
[--session ID] [--approve-read-only] PROMPT` runs one real runtime turn.
`PROMPT=-` reads bounded UTF-8 from stdin. The library entry point
`harness::ask::run` takes `Arc<dyn ModelProvider>`, options and injected stdin,
stdout, stderr, terminal and interrupt handles. The binary alone constructs
`OpenAiResponses` and installs Unix SIGINT handling. Core and providers are
unchanged. Provider selection is an explicit clap choice and a wiring boundary;
adding an adapter later needs another choice, metadata and constructor, not a
change to the client loop or core. No engine process/transport is added.

The CLI adds only `harness-providers` and Tokio. Tokio inherits workspace `rt`,
`sync`, `time`, `macros` and adds `signal`: a current-thread runtime handles
subscription/command selection, nonblocking terminal polling and Unix Ctrl-C.
No multithread runtime, new executor, HTTP stack or additional production crate
is needed. Enabling `signal` adds the locked `signal-hook-registry` dependency.
Existing rustix supplies safe nonblocking/CLOEXEC `/dev/tty` open; no unsafe code.

## Provider, model and credentials

Default auth is `codex` (the user's subscription), through
`OpenAiResponses::codex_login` / `CodexLogin::from_default_path`. Platform auth
uses `platform_from_env` / `OPENAI_API_KEY`. No credential is copied into
configuration, instructions, logical requests or diagnostics. The CLI prints
only authentication classification and the exact hint `run \`codex login\``;
it never formats an arbitrary adapter message. No retries or refreshes occur.

New session defaults follow ADR 0017: `gpt-6.1-sol` on Codex, `gpt-5.5` on the
platform. `openai::catalog` supplies the runtime's sourced context metadata;
unknown context limits remain explicit `context_budget_unknown`, not guesses.
Auth profiles use distinct provider instances (`openai-codex`,
`openai-api-key`) so encrypted owner-scoped state cannot cross endpoints.

`--summary-model` stores an independent model on that same provider instance
and enables automatic compaction. Without it the stored configuration explicitly
disables automatic compaction; budget overflow fails instead. The caller must
repeat the same summary choice on continuation, avoiding implicit opt-in.
Runtime defaults own timeouts: 300s idle, 2s cancel grace, absolute inference
deadline `None`, 300s compaction deadline. There is no CLI-specific inference
retry or timer. Shutdown after a terminal turn cancels remaining idle summary
work, as defined by the handle; overflow-triggered pre-C1 compaction completes
before the user turn opens. This headless process does not remain alive for
background maintenance.

## Storage and continuation

The existing CLI requires explicit absolute private data roots. `ask` preserves
those safety checks while providing an XDG default for interactive usability:
`$XDG_DATA_HOME/harness`, otherwise `$HOME/.local/share/harness`. No cwd-relative
storage is chosen: it must stay outside the authorized workspace so tools cannot
read credentials/policy/journal or accidentally edit session data. Missing parent
directories are created, but `DataRoot` still refuses unsafe paths/permissions.
Explicit relative roots and invalid XDG defaults fail configuration validation.

New sessions use `SessionHandle::create_with_tools` and print the ID to stderr;
`--session` uses `open_with_tools`. Relative workspace inputs are canonicalized
once, and the host must match the stored workspace. Continuation uses the saved
model, refusing an explicit mismatch or another auth owner. This command does
not switch models, change policy, resolve uncertainty or repair journals.
Automatic runtime recovery is awaited before starting a new turn; an uncertain
side effect fails explicitly, never a silent approval or replay.

## Output and approval contract

Only assistant text goes to stdout, flushed on each observed update, with no
inserted newline. Status/proposals/results, displayable reasoning, final usage
(per response, preserving unknown fields) and errors are single-line stderr
records. Control characters in diagnostics are neutralized; tool names and JSON
arguments are bounded to 80 and 320 characters. Reasoning summaries are dimmed
only when stderr is a TTY. Raw reasoning and encrypted/native state are never
rendered or formatted. stdout pipes behave exactly like a terminal.

The gap-free subscription's reducer is used, including resets. Output offsets
are tracked by response, text kind and ordinal; committed content supplies any
missing tail after lag, without reprinting already-written bytes. Initial session
history is excluded by its baseline journal sequence. Tool results/proposals are
deduplicated by call identity. Opaque blocks are ignored, not rendered.

All new policies ask for reads, mutations and processes, deny network, contain
no allowances, and enable the four built-ins. Continuation refuses changed
approval defaults, remembered allowances or compaction choices. Every eligible
call goes through runtime `SubmitApproval`, with `remember: false`. The CLI
never executes a tool. Without `/dev/tty` all approval requests are denied, even
with `--approve-read-only`. With a terminal that flag checks exact registry
effects `{read}` and trusted filesystem backend; unknown, mixed/mutating effects
and shell are never auto-approved. Otherwise `/dev/tty` shows the bounded tool
proposal then `approve? [y/N]`. Only a literal lowercase `y` line allows; EOF,
terminal errors and other input deny. Nonblocking reads keep SIGINT responsive.

Host assembly authorizes the workspace through the existing policy-owned tool
host. Missing sandbox is reported once with its sanitized reason categories;
shell is unavailable but file tools can still run. There is no fallback.

## Cancellation and exit codes

The first SIGINT sends runtime `Cancel` for the active turn and waits for its
durable terminal finish, then shuts down and exits 130. A signal during pending
start cancels compaction and cancels any turn that wins the opening race. The
second SIGINT exits the process immediately (no destructors); the next open owns
normal recovery. Cancellation never claims rollback.

| Code | Meaning |
| --- | --- |
| 0 | Turn completed |
| 1 | Failed turn: provider, context budget, runtime/tool infrastructure or client output failure |
| 2 | Usage/configuration, unsafe/missing/locked storage, mismatched continuation policy or unresolved recovery |
| 3 | Authentication; Codex prints `run \`codex login\`` |
| 130 | Cancelled |

## Verification

Offline integration tests use core's `ScriptedProvider` with real storage/file
tools and injected terminal/I/O. They check exact output, streaming before finish,
reasoning/native isolation, approval and denial, no-terminal refusal even with
auto-read, read-only registry gating, continued history, stdin prompts,
durable cancellation, authentication and budget classification, bounded proposals
and terminal failure. Process-spawning CLI tests are a separate integration
target to avoid ADR 0012's known fork/exec lock-inheritance gap with concurrent
journal reopen assertions. A real adapter setup failure uses a missing temporary
Codex login and a sentinel API key, checking both output streams and all
journal/artifact bytes without network or real credentials.

`scripts/live-check.sh ask` is opt-in only and spends Codex quota once per
invocation. It uses a temporary workspace/private root, asks for `pong`, checks
exit 0 and stdout, and prints only a sanitized pass/fail summary. It is absent
from `check.sh` and CI. The step's handoff records the one live run and the four
reverted mutation checks (no-terminal allow, stdout status, ignored cancellation,
credential disclosure on authentication failure).

## Amendment (m4-07): switching across providers

Date: 2026-10-30

`harness ask` gains `--provider openai|anthropic` (default `openai`) and
`--switch-policy reject|omit-optional` (default `reject`). The client loop, the
storage rules, the approval contract and the exit codes are unchanged; only the
wiring and the continuation decision grew.

**Wiring.** The binary now builds `harness_providers::Router` and hands it to
`ask::run` as the one injected provider. Every instance whose credential exists
is registered — `openai-codex` (read-only Codex login), `openai-api-key`
(`OPENAI_API_KEY`) and `anthropic` (`ANTHROPIC_API_KEY`) — so one process can
serve a session that switches between them. Only the selected instance is
required: `--provider anthropic` without a key exits 3 with
`error: authentication`, and a failing unselected source is skipped silently.
`--auth` describes the OpenAI profile only; passing it explicitly with
`--provider anthropic` exits 2 with `auth_not_applicable`. The runtime catalog
is the union of `openai::extend_catalog`/`anthropic::extend_catalog` for the
registered instances, so a switched-to model has sourced limits instead of
`context_budget_unknown`. New-session defaults: `gpt-6.1-sol` (codex),
`gpt-5.5` (api-key), `claude-sonnet-5-5` (anthropic). `--summary-model` is taken
on the same instance as `--model`, and continuing a session still requires the
same summary choice (otherwise `session_policy_mismatch`).

**Continuation and the switch.** `--session` keeps the branch's saved model
unless the user passes `--provider` or `--model`. A default alone is never
applied silently. When either is passed, the client performs an explicit
`Command::SwitchModel { model, policy }` while the session is idle and before the
turn opens. The target instance is checked against the registered ones first, so
an unservable instance is `provider_not_configured` (exit 2) before anything is
opened. The previous "refuse any other provider instance" rule is gone: choosing
another provider is the feature, and the policy decides what may be lost.

The portability report is printed on stderr in both outcomes, one line per
affected entry, never a payload:

```text
switch: refused from openai-codex/gpt-6.1-sol to anthropic/claude-sonnet-5-5
switch: omits entry=019a… part=block[1] owner=openai-codex/gpt-6.1-sol schema=openai.responses.reasoning.v1 bytes=36
switch: requires entry=019a… part=block[1] capability=tools
```

A refusal prints it and exits 2 (`error: portability_loss`) having written
nothing; an accepted switch prints the same report and then runs the turn.

**Live check.** `scripts/live-check.sh switch [--auth codex|api-key]` runs
`examples/live_switch.rs` once: OpenAI → Anthropic → OpenAI with one short turn
each and a tool round trip, or, without `ANTHROPIC_API_KEY`, the same-provider
variant (the live-check model → another model from the account's
`~/.codex/models_cache.json` → back) with the cross-provider part reported as
`skipped (no ANTHROPIC_API_KEY)`. It prints pass/fail lines, model names and
stable turn codes only. It is absent from `check.sh` and CI.

**Verification.** `crates/cli/tests/ask.rs` covers, offline with the scripted
provider: rejection by default with the report and no inference opened or
written, the same switch under `omit-optional` (the turn runs on the target and
the next request carries no state and no continuation), a continuation with no
override keeping the saved model, a required tool incompatibility refused under
both policies, and an unregistered target refused before opening.
`crates/cli/tests/ask_cli.rs` covers the new flags, the help text, the provider
choices, and the Anthropic authentication path without a key (no secret in
either stream, nothing opened).
