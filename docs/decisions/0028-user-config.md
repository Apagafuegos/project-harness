# 0028: User-level TOML defaults for the existing clients

Status: Accepted and implemented for M7 step 03.

Date: 2026-11-03

## Decision

This supersedes **M5 decision 3 (flags only)**. The user requested a config
file. Only the user owns configuration: `$XDG_CONFIG_HOME/harness/config.toml`,
or `$HOME/.config/harness/config.toml` when XDG_CONFIG_HOME is unset. There is
no workspace/project config: a cloned repository must not select a provider or
widen tool policy. `--config PATH` selects another user file; `--no-config`
ignores it. These mutually exclusive options are global. Config paths derived
from XDG/HOME must be absolute. An explicit path may be relative to the caller.

The keys are `provider`, `auth`, `model`, `summary_model`,
`compaction_strategy`, `data_root`, `switch_policy` and `approve_read_only`.
All except `approve_read_only` are strings; that key is a boolean. Precedence
is **typed command-line flag > config > built-in default**. The resolver uses
clap's `ValueSource::CommandLine`, so clap defaults never mask config. Config
values are validated with the matching flag's parser, not a second enum list.
The model default depends on the effective provider/auth. An absent summary
model remains absent. No environment interpolation or tilde expansion is added.

### Client applicability (M7 decision 14, amended by M8 decision 12)

The first six keys above apply to both clients. Startup `switch_policy` is
**ask-only**. `approve_read_only` applies to **both clients**: ask's
`--approve-read-only` and the TUI's `--approve-read-only` (or the config key)
use the same flag > config > default precedence and the same classifier —
registry tools whose effects are exactly `[Read]` on the trusted filesystem
(`read`, `list`, `glob`, `grep`). M8 decision 12 supersedes M7 decision 14's
"ask-only" ruling for this one key. In the TUI an eligible pending ticket is
answered with an ordinary, unremembered allow bound to its approval id and plan
digest, the modal never opens, and the transcript says the call was
auto-approved as read-only. Shell commands, mutations, unknown and non-registry
tools always open the modal. The startup `switch_policy` therefore remains
ask-only: these keys may coexist in a shared config used by the TUI without
changing its reject-first startup switching or the model-dialog policy choice.
Help and `config show` label this applicability.

The effective choices enter the same existing client options. Config-supplied
provider/model/strategy count as explicit selections for the existing
continuation checks, exactly like flags. The existing auth applicability,
policy/allowance, summary-model and strategy comparisons and model switching
rules are unchanged. Instructions are not re-read on continuation. There are
**no new persisted settings**, context types, or journal-format fields:
model/header and compaction fields are recorded as before; storage location,
auth selection and client approval defaults are not persisted for observability.
Tests inspect resolved options and behavior and only already-recorded fields.

## File boundary and diagnostics

The user file is read with std fs, not the workspace capability. Reads are
bounded at 65,537 bytes, require a regular UTF-8 file and reject anything above
64 KiB, without truncation. Nonblocking open avoids hanging on special files.
A missing default file is normal; a missing explicit file is an error. Unknown
keys, wrong types, invalid values, malformed TOML and unreadable files exit 2:
`config_invalid: <path>: <key>: <reason>`. Parser diagnostics and source text
are never displayed, since either could contain secrets. Syntax-level errors
name `file`; typed errors name the key.

Credential-like keys (case-insensitive `api_key`, `*_key`, `token`, `secret`,
`password`, including nested tables/arrays) get only
`config_credential_refused: put credentials in environment variables`.
No key value, source line or credential environment value enters diagnostics.
Malformed TOML is a content-free syntax error rather than an attempt to recover
credential fields from invalid syntax. Credentials remain in the existing
environment/Codex authentication sources.

`harness config path` prints the selected path and existence and validates any
file it would load. `harness config show` prints each effective value and source
(`flag`, `config`, `default`), with client-applicability annotations. Flags may follow
`config show` to inspect overrides without opening storage or loading credentials.
Both commands exit 0 or 2. `--no-config` bypasses reading, including validation.

The only new dependency is `toml` (with its transitive dependencies). No project
config search, transport, runtime, secret store or migration is introduced.

## Evidence

`config::tests` covers every applicable key's three-tier precedence in both
production option assemblers, clap-default masking, ask-only isolation, the
read-only auto-approve precedence and TUI plumbing, typed
validation, cap/UTF-8/missing boundaries, credential redaction and source labels.
It drives real ask/session creation with scripted providers and checks existing
header/configuration fields and continuation refusals. `tests/config.rs` covers
real commands, global option placement, HOME/XDG/explicit paths, no-config,
exit 2 errors and actual auth selection without network access. Existing ask
approval and TUI explicit-action tests remain unchanged. Instruction discovery
and its independent bounds are specified by the ADR 0022 amendment.

Required break-on-purpose checks were run independently and reverted. Each
compiled successfully and its named test failed (exit 101):

| Mutation | Regression test | Observed failure |
| --- | --- | --- |
| Treat clap defaults as explicit | `precedence_every_applicable_key_both_clients_and_ask_only_scope` | source `flag` instead of `default` |
| Admit symlink directory entries and remove no-symlink resolution | `user_root_nested_order_scope_and_symlinks_and_exclusions` | extra recorded instructions from an in-workspace alias |
| Truncate at the aggregate cap | `total_cap_skips_whole_files_and_continues_to_smaller_files` | extra partial/empty instructions instead of whole-file skips |
| Include a credential value in refusal | `credential_refusal_never_echoes_value_or_source` | redaction assertion fails without printing the sentinel |

The restored focused suite passes. Complete-path ordering also covers prefix
collisions (`a-b/AGENTS.md`, `a/A/AGENTS.md`, `a/AGENTS.md`); directory enumeration
has a real-filesystem pinned-root, bounded-listing and relocation regression.
