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
executable search path, an optional read-only offline registry and a small
explicit environment, configured only in the user's TOML file (ADR 0028), plus
a documented and tested Rust recipe on this host.

## Configuration schema

One new table, `[sandbox]`, in the M7 user TOML only
(`$XDG_CONFIG_HOME/harness/config.toml`, `--config PATH`, or nothing with
`--no-config`). There is never a project or workspace config. Unknown keys
inside the table are refused like any other unknown key.

```toml
[sandbox]
# Absolute directories, bound read-only at the same path in every bash sandbox.
toolchain_paths = ["/home/you/.rustup/toolchains/1.99.1-aarch64-unknown-linux-gnu"]
# Absolute directories prepended before /usr/bin:/bin, in this order.
path_prepend = ["/home/you/.rustup/toolchains/1.99.1-aarch64-unknown-linux-gnu/bin"]
# Optional offline registry, bound read-only at $CARGO_HOME/registry.
cargo_registry = "/home/you/.cargo/registry"

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
- `cargo_registry`: one absolute directory, validated with exactly the
  `toolchain_paths` refusal set (below), and bound read-only at
  `/home/sandbox/.cargo/registry` so cargo finds it through the private
  `CARGO_HOME` with no per-command setup.
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

For each mounted root — every `toolchain_paths` entry and a configured
`cargo_registry` — the fixed leaves `credentials`, `credentials.toml` and
`.git-credentials` are masked with the existing mechanism when they **exist at
sandbox setup**: a regular file becomes a read-only `/dev/null` bind, a
directory becomes an empty read-only tmpfs. The mask is installed at the
sandbox destination (so a registry's leaves are masked under
`$CARGO_HOME/registry`). Nothing is discovered recursively, and the list is
fixed; any other object kind at a leaf fails the sandbox rather than being
exposed.

An **absent** credential leaf is not masked and does not fail the sandbox.
Bubblewrap cannot create a mount point beneath a read-only bind
(`Can't create file … Read-only file system`; ADR 0012's documented
absent-path limitation and audit L04). The mount stays read-only, so a
sandboxed process cannot create the file, and the test proves that a write to
that path fails with nothing appearing on the host. Creating the file on the
host while a command runs is the existing L04 race, unchanged. No host
placeholder is ever created and no mount is ever made writable to mask.

### Writable state: CARGO_HOME only, build output in the workspace

A mount is never writable. When the sandbox is configured, the backend adds one
private, empty writable directory inside the sandbox:
`/home/sandbox/.cargo` (`CARGO_HOME`, mode 0700, owned by the sandbox user on a
private tmpfs).

`cargo`'s build output is **not** redirected. It goes to the default
`<workspace>/target`, which is ordinary workspace output of the same kind the
user's own builds produce, and the sandbox already has a writable workspace.
Forcing a per-command target directory would rebuild every dependency on every
bash call, which a daily coding agent cannot afford (M8 decision 14). The
earlier "no cache in the workspace" rule meant tool state such as
`CARGO_HOME`, not a project's build artifacts.

The choice for `CARGO_HOME` is **per command**, not per session under the data
root:

- it is already a private mount of the sandbox namespace, so "private, 0700,
  owned by the sandbox user" holds by construction, with no host-side
  ownership, mode or traversal to enforce;
- nothing outlives the command, so there is no garbage to collect, no
  cross-session leak of registry caches or credentials, and no new path under
  the data root for GC (ADR 0030) to reason about;
- a per-session `CARGO_HOME` would be an unreviewed state channel between tool
  calls and attacker-writable state inside the harness's own storage root.

The cost is that cargo re-resolves the registry and re-links fingerprints for
each command; the compiled artifacts themselves persist in the workspace
`target/`.

`CARGO_HOME` is set by the backend itself when the sandbox is configured; that
is why `CARGO_*` is a refused `[sandbox.env]` namespace.

## Refusals at config load

Each `toolchain_paths` entry, a configured `cargo_registry`, and each resolved
`path_prepend` must be an absolute, canonical (no symlink component), existing
directory. It is refused with exit 2, naming the path, when it is:

- `/`;
- `$HOME` itself, or any ancestor of `$HOME` (which would expose it anyway);
- the workspace, the data root, any ancestor of either, or anything inside
  either;
- the harness config directory, `~/.ssh`, `~/.gnupg`, `~/.codex`, `~/.aws`,
  `~/.config` as a whole, or anything inside them;
- `/tmp`, and anything overlapping `/workspace`, `/home/sandbox`, `/proc` or
  `/dev` (the sandbox's own private mounts);
- missing, or not a directory (fail closed; no silent skip).

`path_prepend` entries must also resolve inside a mounted toolchain or `/usr`;
that is checked after resolution so a symlink cannot redirect the executable
search path out of the approved mounts. The sandbox independently repeats the
structural and existence checks for every mounted root before every spawn
(`validate_config`), so a mount that disappears after config load fails closed
instead of producing a different sandbox.

## Plan, session record and continuation

The effective settings are recorded in the existing session configuration
artifact (the header's `configuration` artifact, i.e. the policy snapshot
object), under a new additive `sandbox` field:

```json
"sandbox": {"toolchain_paths": ["…"], "path_prepend": ["…"],
            "cargo_registry": "…", "env": {"K": "V"}}
```

The field is omitted when nothing is configured, so v0.1 journals are
byte-identical and old readers see what they saw before. `cargo_registry` is
part of that object and therefore of the comparison. **No journal schema or
record-version change is needed**: the artifact is an opaque content-addressed
configuration document whose unknown fields are already preserved, and the
field is only read by the new code.

`bash` plans carry the same object in a new trailing `sandbox` field, omitted
when empty, so the exact approved bytes bind the mount set, the search path,
the registry and the environment. `Sandbox::build` refuses a plan whose
`sandbox` differs from the configured one (`plan sandbox configuration
mismatch`) before spawning. The `read`, `write`, `edit`, `list`, `glob` and
`grep` plans are unchanged.

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
--setenv PATH <toolchain>/bin:/usr/bin:/bin --setenv HOME /home/sandbox
--setenv TMPDIR /tmp --setenv LANG C.UTF-8 --setenv TERM dumb
--setenv RUSTUP_TOOLCHAIN 1.99.1 --setenv CARGO_HOME /home/sandbox/.cargo
--ro-bind /usr /usr --proc /proc --dev /dev --tmpfs /tmp --tmpfs /home/sandbox
--perms 0700 --dir /home/sandbox/.cargo
--ro-bind <toolchain> <toolchain>
--ro-bind <registry> /home/sandbox/.cargo/registry
--ro-bind /dev/null <toolchain>/credentials
--bind <workspace> /workspace --chdir /workspace /bin/bash --noprofile --norc -c :
```

No `CARGO_TARGET_DIR` is present: cargo writes the workspace's own `target/`.
The unconfigured case is byte-for-byte the v0.1 golden: no extra `--setenv`,
no `--perms`/`--dir`, no toolchain or registry binds, `PATH=/usr/bin:/bin`.

Ordering rule: every read-only bind is installed **before** any credential
mask, and before the workspace's own ancestor binds and protected-path masks,
so a mask can never be covered again by a later mount. Toolchain roots are
sorted by component count, then the registry bind follows (its destination is
already created by the private `CARGO_HOME` directory).

## The Rust recipe for this host

Tested on Ubuntu ARM64, Bubblewrap 0.11.1, rustup toolchain
`1.99.1-aarch64-unknown-linux-gnu`:

```toml
[sandbox]
toolchain_paths = ["/home/you/.rustup/toolchains/1.99.1-aarch64-unknown-linux-gnu"]
path_prepend = ["/home/you/.rustup/toolchains/1.99.1-aarch64-unknown-linux-gnu/bin"]
cargo_registry = "/home/you/.cargo/registry"

[sandbox.env]
RUSTUP_TOOLCHAIN = "1.99.1"
CARGO_NET_OFFLINE = "true"
```

Then, with no preparation step:

```sh
cargo test --offline
```

The configured registry is already visible at `$CARGO_HOME/registry`, cargo
writes its lock files to the private `CARGO_HOME`, and build output lands in
the workspace's `target/` (so repeated commands reuse it). Mount the
**toolchain's** `bin` (the real `cargo`/`rustc` binaries), not `~/.cargo/bin`
(rustup proxy shims that need `~/.rustup` settings and would require mounting
the whole home). The dependency must already be in the mounted offline
registry; the S02 test uses `itoa` from this repository's own dependency tree.
Network is still denied: the same test asserts that an external TCP connection
is refused inside the sandbox.

## Doctor and clients

`harness doctor` gains a `sandbox` section:

```json
{"toolchain_paths":[{"path":"…","sandbox_path":"…","mode":"ro","exists":true,
  "readable":true,
  "credential_masks":[{"path":"…/credentials","sandbox_path":"…","status":"masked"},
                      {"path":"…/credentials.toml","sandbox_path":"…","status":"absent"}]}],
 "cargo_registry":{"path":"…","sandbox_path":"/home/sandbox/.cargo/registry",
  "mode":"ro","exists":true,"readable":true,"credential_masks":[…]},
 "path_prepend":["…"],"env_keys":["RUSTUP_TOOLCHAIN"]}
```

A mask status mirrors exactly what the backend does: a regular file is
`masked`, a directory is `masked` (tmpfs), an absent leaf is `absent`, and any
other kind (which the backend refuses) is `refused`. It reports existence and
readability per mount, prepends and environment **key names**, never a value or
file content, and creates nothing.

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
- No cross-session cache sharing, and no forced build-output redirection.
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
- **Build output is shared with the host.** `target/` is written by the
  sandboxed command and is the same directory the user's builds use, so
  sandboxed artifacts can influence later host builds and stale artifacts
  survive the command. This is normal project output, not isolated cache state.
- **`CARGO_HOME` is per command and not shared across commands or sessions**, so
  registry re-resolution repeats each time even though compiled artifacts
  persist.
- **Host toolchain drift:** the recipe names exact paths. A rustup update, a
  moved home or a missing offline registry makes the mount refused at load
  (missing) or the build fail; the harness never guesses a replacement path.
- The mounted registry must contain every dependency; `--offline` is required
  because the network is denied, and a missing crate is a normal build error.
- Toolchains whose executables are proxy shims (rustup proxies in
  `~/.cargo/bin`, or wrapper scripts that exec a host path) do not work unless
  their real targets are mounted too; the harness does not rewrite or wrap
  commands.

## Evidence

- `tools::sandbox::settings::tests`: environment-name refusals (including that
  the value is never echoed), path spelling/duplicate/prepend refusals, the
  exact `PATH` string, the `cargo_registry` destination and its use of the same
  refusal set, doctor's `masked`/`absent`/`refused` statuses, and the full
  refusal matrix for `/`, `$HOME`, home ancestors, workspace/data-root
  ancestors and descendants, the config directory, `~/.ssh`, `~/.gnupg`,
  `~/.codex`, `~/.aws`, `~/.config`, a file, a missing path and a symlinked
  mount.
- `tools::sandbox::args::tests::toolchain_arguments_golden_and_configuration_binding`:
  the configured argv above (including the registry bind), plus refusal of a
  plan bound to different settings.
- `tools::tests::omitted_or_empty_sandbox_keeps_the_v01_plan_bytes`: the
  unchanged v0.1 golden plan, empty-equals-omitted, configured bash plans
  differ, and trusted file plans stay identical.
- `crates/cli/src/config_sandbox_tests.rs`: strict table types and unknown
  keys, credential-shaped and harness-owned environment refusals without value
  echo, `cargo_registry` parsing/value/refusal, `config show` labels and
  key-name-only rendering, load-time path refusals, and both clients' recorded
  `sandbox` field with continuation refusal (and an untouched journal) for a
  changed mount, prepend, registry, environment or removal.
- `crates/core/tests/sandbox/toolchains.rs` (real S02, exact-head CI): masked
  credentials cannot be read (present), absent leaves cannot be created inside
  and leave no host file, the mount stays read-only, the registry is readable
  read-only at `$CARGO_HOME/registry` and cannot be written, private protected
  paths and `.git` stay protected, network stays denied, `CARGO_HOME` is 0700
  and owned by the sandbox user and starts empty, and the plan-mismatch
  refusal. The second test runs a plain `cargo test --offline` (no link step)
  through the production sandbox with a read-only toolchain and configured
  registry, asserts the test binary ran, that `target/` appears in the
  workspace, and that network is still denied.
- `crates/cli/tests/doctor.rs`: per-mount existence/readability, `masked`,
  `absent` and `refused` credential statuses, the registry mount report,
  prepends, environment key names, and no value in the rendered report.
- `crates/cli/src/tui/tools.rs`: the approval preview shows the bound mounts,
  sandbox `PATH` and environment key names and no values, and non-bash plans
  render nothing.

### Break-on-purpose

Each defense was removed on its own, the named test failed, and the file was
restored before the final runs:

| Mutation | Regression test | Observed failure |
| --- | --- | --- |
| Toolchain bind made writable (`--bind`) | S02 `s02_toolchain_credentials_present_…` | the in-sandbox write to the mount succeeded: `nonzero_exit: exit status 82` |
| Same, against the golden argv | `toolchain_arguments_golden_and_configuration_binding` | argv mismatch, exit 101 |
| `path_prepend` check removed | `sandbox_paths_refuse_invalid_spellings_duplicates_and_unmounted_prepends` | an outside-`/usr` prepend was accepted |
| Credential-name rule removed from `env` | `sandbox_env_names_are_refused_without_values` | `api_key` was accepted |
| Protected/refused mount paths skipped | `sandbox_location_refusals_cover_roots_ancestors_descendants_and_aliases` | `~/.ssh` and a config-dir mount were accepted |
| Fixed credential mask list emptied | S02 `s02_toolchain_credentials_present_…` | the present credential was readable: `nonzero_exit: exit status 81` |
| Same, against the golden argv | `toolchain_arguments_golden_and_configuration_binding` | the `--ro-bind /dev/null` mask disappeared |
| Registry bind removed | S02 `s02_toolchain_cargo_test_offline_with_readonly_registry` | "error: no matching package named `itoa` found"; `nonzero_exit: exit status 101` |
| `sandbox` dropped from the continuation comparison | `sandbox_both_clients_record_settings_and_refuse_continuation_changes` | a changed mount continued instead of refusing |
