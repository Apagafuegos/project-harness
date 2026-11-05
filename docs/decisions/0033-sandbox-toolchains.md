# 0033: Project toolchains in the bash sandbox

Status: Accepted and implemented for Milestone 8 step 07.

Date: 2026-11-05

## Context

The M8-01 dogfood baseline found that four of five tasks could not run a
project's tests: the bash sandbox binds only system paths, with
`PATH=/usr/bin:/bin` and `HOME=/home/sandbox`, so a rustup toolchain in the
user's home (`~/.rustup`, `~/.cargo`) is unreachable and `cargo test` fails
with a bare "command not found".

Widening the sandbox by default would contradict decision 0002's fixed mount
set. This decision adds **opt-in, read-only toolchain mounts**, a prepended
executable search path and a small explicit environment, configured only in
the user's TOML file (ADR 0028), plus a documented and tested Rust recipe on
this host.

## Configuration schema

One new table, `[sandbox]`, in the M7 user TOML only
(`$XDG_CONFIG_HOME/harness/config.toml`, `--config PATH`, or nothing with
`--no-config`). There is never a project or workspace config. Unknown keys
inside the table are refused like any other unknown key.

```toml
[sandbox]
# Absolute directories, bound read-only at the same path in every bash sandbox.
toolchain_paths = ["/home/you/.rustup/toolchains/1.99.1-aarch64-unknown-linux-gnu",
                   "/home/you/.cargo/registry", "/home/you/.cargo/git"]
# Absolute directories prepended before /usr/bin:/bin, in this order.
path_prepend = ["/home/you/.rustup/toolchains/1.99.1-aarch64-unknown-linux-gnu/bin"]

[sandbox.env]
RUSTUP_TOOLCHAIN = "1.99.1"
CARGO_NET_OFFLINE = "true"
```

- `toolchain_paths`: duplicates are refused; a relative path, a trailing slash
  that is not `/`, `.`/`..` components, or a non-normalized spelling is refused
  naming the value.
- `path_prepend`: each entry must be inside a `toolchain_paths` entry or
  `/usr`; a colon in a path is refused because it would split `PATH`. `PATH`
  itself is not settable and is always
  `<path_prepend…>:/usr/bin:/bin`.
- `env`: a string map. Names are refused (naming the key, never the value) if
  they are credential-shaped under ADR 0028's rule (`api_key`, `*_key`,
  `token`, `secret`, `password`, case-insensitive) or owned by the harness:
  `PATH`, `HOME`, `TMPDIR`, `LANG`, `TERM`, `BASH_ENV`, `ENV`, `SHELLOPTS`,
  `BASHOPTS`, `CARGO_*`, `LD_*`, `OPENAI_*`, `ANTHROPIC_*`, `DEEPSEEK_*`,
  `CODEX_*`. A name must match `[A-Za-z_][A-Za-z0-9_]*` and a value may not
  contain NUL.
- The sandbox can be configured with the flag-provided workspace: `--config`
  is validated against the effective workspace and data root, so a `[sandbox]`
  table that is valid in one workspace may be refused in another. A refusal
  exits 2 and names the path.

With no `[sandbox]` table the behaviour is exactly v0.1's: the same plan bytes
(`crates/core/src/tools/golden/bash.json`), the same argv, `PATH=/usr/bin:/bin`
and no extra mounts. An empty table is the same as no table
(`tools::tests::omitted_or_empty_sandbox_keeps_the_v01_plan_bytes`).

### Credential masking (M8 decision 13)

For each mount root the fixed leaves `credentials`, `credentials.toml` and
`.git-credentials` are masked with the existing mechanism when they **exist at
sandbox setup**: a regular file becomes a read-only `/dev/null` bind, a
directory becomes an empty read-only tmpfs. Nothing is discovered
recursively, and the list is fixed.

An **absent** credential leaf is not masked and does not fail the sandbox.
Bubblewrap cannot create a mount point beneath a read-only bind
(`Can't create file … Read-only file system`; ADR 0012's documented
absent-path limitation and audit L04). The mount stays read-only, so a
sandboxed process cannot create the file, and the test proves that a write to
that path fails with nothing appearing on the host. Creating the file on the
host while a command runs is the existing L04 race, unchanged. No host
placeholder is ever created and no mount is ever made writable to mask.

### Writable cache state

A mount is never writable. When the sandbox is configured, the backend adds two
private, empty writable directories inside the sandbox:
`/home/sandbox/.cargo` (`CARGO_HOME`, mode 0700, owned by the sandbox user on
a private tmpfs) and `/tmp/cargo-target` (`CARGO_TARGET_DIR`, mode 0700 on the
existing private `/tmp` tmpfs).

This is the **per-command** choice, not a per-session directory under the data
root. Reasons:

- the directories are already private mounts of the sandbox namespace, so
  "private, 0700, owned by the sandbox user" holds by construction, with no
  host-side ownership, mode or traversal to enforce;
- nothing outlives the command, so there is no garbage to collect, no
  cross-session leak of build state or cache contents, and no new path under
  the data root for GC (ADR 0030) to reason about;
- a per-session cache would make the second command's behaviour depend on an
  earlier command's writes — an unreviewed state channel between tool calls —
  and would put attacker-writable files under the harness's own storage root.

The cost is that every command rebuilds what it needs. That is acceptable for
the one documented recipe: an offline registry is mounted read-only, so
dependency metadata is read from the mount and only per-command artifacts and
cargo lock files are written.

`CARGO_HOME` and `CARGO_TARGET_DIR` are set by the backend itself when the
sandbox is configured; that is why `CARGO_*` is a refused `[sandbox.env]`
namespace.

## Refusals at config load

Each `toolchain_paths` entry (and each resolved `path_prepend`) must be an
absolute, canonical (no symlink component), existing directory. It is refused
with exit 2, naming the path, when it is:

- `/`;
- `$HOME` itself, or any ancestor of `$HOME` (which would expose it anyway);
- the workspace, the data root, any ancestor of either, or anything inside
  either;
- the harness config directory, `~/.ssh`, `~/.gnupg`, `~/.codex`, `~/.aws`,
  `~/.config` as a whole, or anything inside them;
- `/tmp`, and anything overlapping `/workspace`, `/home/sandbox`,
  `/tmp/cargo-target`, `/proc` or `/dev` (the sandbox's own private mounts);
- missing, or not a directory (fail closed; no silent skip).

`path_prepend` entries must also resolve inside a mounted toolchain or `/usr`;
that is checked after resolution so a symlink cannot redirect the executable
search path out of the approved mounts. The sandbox independently repeats the
structural and existence checks before every spawn (`validate_config`), so a
mount that disappears after config load fails closed instead of producing a
different sandbox.

## Plan, session record and continuation

The effective settings are recorded in the existing session configuration
artifact (the header's `configuration` artifact, i.e. the policy snapshot
object), under a new additive `sandbox` field:

```json
"sandbox": {"toolchain_paths": ["…"], "path_prepend": ["…"], "env": {"K": "V"}}
```

The field is omitted when nothing is configured, so v0.1 journals are
byte-identical and old readers see what they saw before. **No journal schema or
record-version change is needed**: the artifact is an opaque content-addressed
configuration document whose unknown fields are already preserved, and the
field is only read by the new code.

`bash` plans carry the same object in a new trailing `sandbox` field, omitted
when empty, so the exact approved bytes bind the mount set, the search path and
the environment. `Sandbox::build` refuses a plan whose `sandbox` differs from
the configured one (`plan sandbox configuration mismatch`) before spawning. The
`read`, `write`, `edit`, `list`, `glob` and `grep` plans are unchanged.

The continuation rule is the existing one from ADR 0028/0012:
`wiring::open_session` compares the live configuration's `workspace_read`,
`workspace_mutation`, `process`, `network`, `allowances` **and now `sandbox`**
against the recorded artifact field by field, and refuses a difference with
`session_policy_mismatch` before the journal is opened for writing. Nothing is
written, so re-running with the recorded settings continues normally. This is
the same rule remembered grants and strategies already follow; no new
comparison, code or journal field was introduced.

## Environment and arguments

`PATH` becomes `<prepends…>:/usr/bin:/bin`, still fixed, never inherited.
Configured variables are added with `--setenv` after the fixed allowlist; the
host environment is still cleared twice (`--clearenv` and `Command::env_clear`).
Environment values appear in argv, so the approval preview and `doctor` render
**key names only**, and no diagnostic echoes a value.

Golden argv for a configured sandbox (see
`crates/core/src/tools/golden/toolchains-argv.json`, checked exactly by
`tools::sandbox::args::tests::toolchain_arguments_golden_and_configuration_binding`):

```text
--info-fd 10 --json-status-fd 11 --unshare-all --unshare-user --die-with-parent
--new-session --clearenv
--setenv PATH <mount>/bin:/usr/bin:/bin --setenv HOME /home/sandbox
--setenv TMPDIR /tmp --setenv LANG C.UTF-8 --setenv TERM dumb
--setenv RUSTUP_TOOLCHAIN 1.99.1
--setenv CARGO_HOME /home/sandbox/.cargo --setenv CARGO_TARGET_DIR /tmp/cargo-target
--ro-bind /usr /usr --proc /proc --dev /dev --tmpfs /tmp --tmpfs /home/sandbox
--perms 0700 --dir /home/sandbox/.cargo --perms 0700 --dir /tmp/cargo-target
--ro-bind <toolchain> <toolchain> --ro-bind /dev/null <toolchain>/credentials
--bind <workspace> /workspace --chdir /workspace /bin/bash --noprofile --norc -c :
```

The unconfigured case is byte-for-byte the v0.1 golden: no extra `--setenv`,
no `--perms`/`--dir`, no toolchain binds, `PATH=/usr/bin:/bin`.

Ordering rule: every toolchain bind is installed **before** the credential
masks, and before the workspace's own ancestor binds and protected-path masks,
so a mask can never be covered again by a later mount. Overlapping mounts are
sorted by component count, as the workspace mounts already are.

## The Rust recipe for this host

Tested on Ubuntu ARM64, Bubblewrap 0.11.1, rustup toolchain
`1.99.1-aarch64-unknown-linux-gnu`:

```toml
[sandbox]
toolchain_paths = ["/home/you/.rustup/toolchains/1.99.1-aarch64-unknown-linux-gnu",
                   "/home/you/.cargo/registry"]
path_prepend = ["/home/you/.rustup/toolchains/1.99.1-aarch64-unknown-linux-gnu/bin"]

[sandbox.env]
RUSTUP_TOOLCHAIN = "1.99.1"
CARGO_NET_OFFLINE = "true"
```

and, once per command, the registry is linked into the writable
`CARGO_HOME` because cargo resolves the registry through `CARGO_HOME`:

```sh
ln -s /home/you/.cargo/registry "$CARGO_HOME/registry"
cargo test --offline
```

Mount the **toolchain's** `bin` (the real `cargo`/`rustc` binaries), not
`~/.cargo/bin` (rustup proxy shims that need `~/.rustup` settings and would
require mounting the whole home). Mount `~/.cargo/registry` (and optionally
`~/.cargo/git`) read-only instead of `~/.cargo` itself: it is the cache
subdirectory, it holds no credential file, and it avoids the
`~/.cargo/credentials` masking case entirely. Cargo writes only
`$CARGO_HOME/.package-cache`, `$CARGO_HOME/registry/cache` and
`$CARGO_TARGET_DIR`, all inside the private writable directories.

The dependency must already be in the mounted offline registry. The S02 test
uses `itoa` from this repository's own dependency tree and asserts that
`cargo test --offline` really ran the test binary (`SANDBOX_RUST_TEST_BINARY_RAN`
and `1 passed`), that the workspace gained only `Cargo.lock` (no `target/`),
that `$CARGO_HOME` is 0700 and owned by the sandbox user, and that an external
TCP connection is still refused. The suite skips, with a printed `SKIPPED:`
line, when no 1.99.1 toolchain or offline registry is present on the host,
rather than pretending.

## Doctor and clients

`harness doctor` gains a `sandbox` section:

```json
{"toolchain_paths":[{"path":"…","mode":"ro","exists":true,"readable":true,
  "credential_masks":[{"path":"…/credentials","status":"masked"},
                      {"path":"…/credentials.toml","status":"absent"}]}],
 "path_prepend":["…"],"env_keys":["RUSTUP_TOOLCHAIN"]}
```

It reports existence and readability per mount, `masked` versus `absent` per
fixed credential leaf, the prepends and environment **key names**. It never
reports an environment value or a credential's content, and it does not create
anything.

`harness config show` labels the effective `[sandbox]` values with
`[config]` or `[default]` (flags cannot set them) and prints environment key
names only. Both clients build their `SandboxConfig` through the same
`Settings::sandbox_config()`, so ask and the TUI apply identical settings, and
the recorded session configuration comes from the same `wiring` helper.

The approval preview gains the sandbox scope from the **exact bound plan
bytes**: the read-only toolchain mounts, the sandbox `PATH` and the environment
key names, next to the existing `network: denied` line. In `harness ask` the
same lines are appended to the approval prompt summary.

## Unchanged

Network denial (still `--unshare-all`, no exception), protected paths and
`.git` masking, per-call approval, descriptor hygiene and the fail-closed
probe/assembly are untouched. A configured toolchain cannot widen workspace
access, cannot make anything writable, and cannot set a harness-owned variable.
A failed or missing toolchain mount fails the probe (bash unavailable), never a
host fallback.

## Explicitly not attempted

- No network for tools, with or without a mounted toolchain.
- No writable toolchain mounts, and no `rw` option in the schema.
- No project, workspace or per-session config file; user TOML only.
- No automatic discovery of toolchains, caches or credential files.
- No per-session cache and no cross-session cache sharing.
- No new dependency, no new crates, no journal schema or record-version change.
- No `elevated` mode, no extra capabilities, no seccomp filter.

## Residual limits

- **A mounted toolchain is trusted user code.** `cargo`/`rustc` read the
  workspace and every mount, and a malicious toolchain can exfiltrate what it
  can read into tool output. Mounting a directory trusts it.
- **Every mount is readable in full** by the sandboxed command except the fixed
  masked leaves; `.git` still protects the workspace's `.git`, not a mounted
  toolchain's files.
- **Absent credential leaves are not pre-masked** (decision 13); the read-only
  mount prevents creation from inside, and host-side creation during a command
  is the L04 race.
- **Hard-link aliases** of a masked file inside the mount are not protected,
  exactly like the workspace masks (ADR 0012).
- **Caches are not shared across commands or sessions**, so a mounted toolchain
  cannot reuse build artifacts between turns. `CARGO_HOME`/`CARGO_TARGET_DIR`
  are fixed by the backend and cannot be redirected.
- **Host toolchain drift:** the recipe names an exact toolchain directory and
  exact paths. A rustup update, a moved home or a missing offline registry
  makes the mount refused at load (missing) or the build fail; the harness
  never guesses a replacement path.
- The mounted registry must contain every dependency; `--offline` is required
  because the network is denied, and a missing crate is a normal build error.
- Toolchains whose executables are proxy shims (rustup proxies in
  `~/.cargo/bin`, or wrapper scripts that exec a host path) do not work unless
  their real targets are mounted too; the harness does not rewrite or wrap
  commands.

## Evidence

- `tools::sandbox::settings::tests`: environment-name refusals (including that
  the value is never echoed), path spelling/duplicate/prepend refusals, the
  exact `PATH` string, and the full refusal matrix for `/`, `$HOME`, home
  ancestors, workspace/data-root ancestors and descendants, the config
  directory, `~/.ssh`, `~/.gnupg`, `~/.codex`, `~/.aws`, `~/.config`, a
  file, a missing path and a symlinked mount.
- `tools::sandbox::args::tests::toolchain_arguments_golden_and_configuration_binding`:
  the configured argv above, plus refusal of a plan bound to different
  settings.
- `tools::tests::omitted_or_empty_sandbox_keeps_the_v01_plan_bytes`: the
  unchanged v0.1 golden plan, empty-equals-omitted, configured bash plans
  differ, and trusted file plans stay identical.
- `crates/cli/src/config_sandbox_tests.rs`: strict table types and unknown
  keys, credential-shaped and harness-owned environment refusals without value
  echo, `config show` labels and key-name-only rendering, load-time path
  refusals, and both clients' recorded `sandbox` field with continuation
  refusal (and an untouched journal) for a changed mount, prepend, environment
  or removal.
- `crates/core/tests/sandbox/toolchains.rs` (real S02, exact-head CI): masked
  credentials cannot be read (present), absent leaves cannot be created inside
  and leave no host file, the mount stays read-only, private protected paths
  and `.git` stay protected, network stays denied, the private caches are 0700
  and owned by the sandbox user and start empty, and the plan-mismatch refusal.
  The second test runs `cargo test --offline` through the production sandbox
  with a read-only toolchain and registry and asserts the test binary ran.
- `crates/cli/tests/doctor.rs`: per-mount existence/readability and
  masked/absent credentials, prepends, environment key names, and no value in
  the rendered report.
- `crates/cli/src/tui/tools.rs`: the approval preview shows the bound mounts,
  sandbox `PATH` and environment key names and no values, and non-bash plans
  render nothing.

### Break-on-purpose

Each defense was removed on its own, the named test failed, and the file was
restored before the final runs:

| Mutation | Regression test | Observed failure |
| --- | --- | --- |
| Toolchain bind made writable (`--bind`) | `sandbox/toolchains.rs` (S02) | the in-sandbox write to the mount succeeded: `nonzero_exit: exit status 82` |
| Same, against the golden argv | `toolchain_arguments_golden_and_configuration_binding` | argv mismatch, exit 101 |
| `path_prepend` check removed | `sandbox_paths_refuse_invalid_spellings_duplicates_and_unmounted_prepends` | an outside-`/usr` prepend was accepted |
| Credential-name rule removed from `env` | `sandbox_env_names_are_refused_without_values` | `api_key` was accepted |
| Protected/refused mount paths skipped | `sandbox_location_refusals_cover_roots_ancestors_descendants_and_aliases` | `~/.ssh` and a config-dir mount were accepted |
| Fixed credential mask list emptied | `sandbox/toolchains.rs` (S02) | the present credential was readable: `nonzero_exit: exit status 81` |
| Same, against the golden argv | `toolchain_arguments_golden_and_configuration_binding` | the `--ro-bind /dev/null` mask disappeared |
| `sandbox` dropped from the continuation comparison | `sandbox_both_clients_record_settings_and_refuse_continuation_changes` | a changed mount continued instead of refusing |
