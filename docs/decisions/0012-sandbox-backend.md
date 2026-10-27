# 0012: Bubblewrap backend and mandatory S02 security gate

Status: Implemented for Milestone 3 step 7a. Bash is **not registered** with the
runtime; authorization/dispatch adaptation remains step 7b.

Date: 2026-10-27

## Decision

`tools::sandbox` owns the blocking process boundary and implements the shared
`tools::Executor` trait from step 4a. `Sandbox::execute` uses the authorized
operation's exact plan and `SandboxConfig::workspace_root`; a non-bash plan
returns infrastructure_failure. The workspace root defaults to the current
directory and callers should set it explicitly to their authorized workspace.
`Sandbox::run` uses the shared `tools::CancelToken` and returns the shared
`tools::ExecutionOutcome`, with `Some(duration_ms)` and raw artifact vectors.
There is no duplicate token or outcome type. A private `Sandbox` can
only be obtained by `Sandbox::probe(SandboxConfig)`. It shares the doctor's
absolute-path, minimum-version (0.5.0), in-sandbox and **host-side** UID/GID-map
checks. The CLI delegates its entire existing report to core; its JSON shape
and existing tests are unchanged. The backend additionally probes the actual
configured runtime mounts, bash, control-fd protocol and optional user scope.
No failing probe or setup permits an unrestricted fallback.

Validated immutable `Plan::bytes()` supply command, cwd, timeout, network denial
and every workspace mount. The plan's recorded workspace identity must match
its execution root; substitution fails before spawn. No raw model JSON or
sandbox flags are accepted.
Configured system paths are read-only; merged-/usr symlinks are recreated.
Defaults include `/usr`, `/bin`, `/lib`, `/lib64`, `/sbin`,
`/etc/alternatives`, `/etc/ld.so.cache`, `/etc/ssl/certs`, when present on the
architecture. An explicitly configured missing mount fails closed. Never bind
host root, all of etc, or host home. `/proc`, `/dev`, `/tmp` and
`/home/sandbox` are private. Environment construction uses both `env_clear()`
and `--clearenv`, setting only PATH, HOME, TMPDIR, LANG and TERM. Bash itself
may add PWD, SHLVL and `_`; BASH_ENV and startup files are not inherited.

The workspace mount follows the plan's `.` mode. Ancestor binds precede
child masks, regardless of canonical plan serialization order: `.git`'s
read-only bind must not cover and expose `.git/private`. Existing directory
masks are empty tmpfs mounts remounted read-only; file masks are read-only
`/dev/null` binds. Outside `/dev`, Bubblewrap's nodev bind makes these device
masks reject reads and writes with EACCES; directory/write-only protections
reject writes with EROFS. Mount/cwd symlink components fail setup.

Both standalone bwrap and an available `systemd-run --user --scope` launch
have explicit stdin/stdout/stderr configuration and a cleared host environment.
The scope launcher receives only the bus connection variables it needs.
`--expand-environment=no` is essential: otherwise systemd-run changes shell
command bytes (including `$$`). When available, the scope uses configured
`TasksMax` (64 by default). Otherwise `process_limit_gap()` and the doctor's
cgroup report explicitly record its absence. RLIMIT_NPROC is not used.

## Descriptors and lifetime

Rust-owned descriptors are close-on-exec. Before spawn, `/proc/self/fdinfo`
is audited: an unknown non-CLOEXEC harness descriptor makes setup fail closed.
Safe Rust cannot borrow and change arbitrary raw inherited descriptors without
unsafe code. The only temporary exceptions are backend-owned info/status
sockets, serialized under a spawn mutex, restored/closed in the parent
immediately after spawn and consumed/closed by bwrap before bash exec.
Embedders must create new descriptors close-on-exec too. There is no unsafe
pre_exec callback. An isolated subprocess test verifies refusal of a deliberately
non-CLOEXEC foreign descriptor; a separate in-sandbox test enumerates actual fds.

`--info-fd` reports the namespace init's host pid; `--json-status-fd` supplies
exit status. A pidfd, checked against the reported PID-namespace inode, binds
signals without PID-reuse races. Timeout, cancellation and output overflow send
SIGTERM to init, then SIGKILL after the configured grace (default two seconds).
Bwrap is reaped using rustix. Completion waits for init's pidfd retirement;
that retirement may lag launcher reaping briefly. Normal foreground completion
also tears down the namespace and its detached descendants. A precancelled plan
never starts. Cleanup uncertainty blocks completion rather than claiming a clean
cancellation. Probe processes use bounded nonblocking capture and a private
process group; an inherited output writer cannot cause an unbounded thread join.

## Output and results

Separate stdout/stderr reader threads retain a total of at most 64 MiB by
default. Overflow stops the workload; the retained raw stream bytes are
artifacts for the core to publish, stdout first and stderr second. Combined
model-facing output uses UTF-8-safe head/tail elision and a marker, at most
32 KiB of compact JSON including block overhead, escaping and diagnostics. Limits
come from trusted configuration, not shell arguments.

The outcome has decision 19's exact fields. Executed statuses are explicitly
selected here, never copied from planning `Refusal::status` (decision 22):
succeeded/null, failed/nonzero_exit, failed/output_limit,
failed/protected_path, timed_out/timeout, cancelled/cancelled, or
infrastructure_failure/sandbox_unavailable. No new session result-code variants
are added on this branch (decision 25).

## Verification and mutation checks

`crates/core/tests/sandbox.rs` is the real-Linux S02 suite. `check.sh` runs its
ignored security tests explicitly and serially after ordinary tests. A failed
probe fails the check. ADR 0008's sole opt-out is
`HARNESS_ALLOW_NO_SANDBOX=1`, printing exactly `SKIPPED: shell security suite`.
Both CI jobs are pinned to `ubuntu-24.04`, load the unchanged reviewed
`ci/apparmor/harness-ci-bwrap`, and run S02 without an opt-out. CI preserves
before/after doctor artifacts and prints the tested bwrap version.

Tested versions: local **0.11.1** on Ubuntu ARM64; pinned Ubuntu 24.04 CI
**0.9.0** on AMD64 (also present in base run 40533407916's doctor artifacts).
The doctor's established version floor is preserved; the configured-backend
probe also requires its actual control-fd protocol to work.

Decision 26 resolves the original single-defense mutation contradiction:
flag presence is checked by exact golden argument vectors; environment denial
is mutated by dropping both environment-clearing defenses. A fake bwrap that
lies about in-sandbox facts but retains host id maps is refused by the host-side
map check. The other mutations deliberately allow git writes, omit masks,
omit init termination, and leak a harness descriptor. Each is tested, then
reverted before the full final check. Protected descendants below `.git`,
non-CLOEXEC setup refusal, configured-shell failure, exact command bytes/cwd,
and a missing TasksMax manager have additional regressions.

## Known gaps and limits of scope

- An absent protected path cannot be overmounted. The backend checks after the
  run; if the command leaves it created, the result is failed/protected_path.
  This post-run protection failure takes precedence over interruption results
  (but not infrastructure failure). The created object stays: there is no
  rollback. This does not prevent a transient creation removed again before
  the final check.
- Without a reachable usable systemd user manager there is no TasksMax limit;
  deadlines, PID namespaces and full cleanup remain enforced. The fork-bomb
  test uses immediate forced cleanup as trusted test configuration, and also
  requires task-limit denial whenever a scope is available.
- No seccomp filter. Network exceptions and elevated capabilities are not
  supported in M3. Network is always denied.
- Bwrap encodes signal death as `128 + signal` in both its exit status and JSON
  status. This is indistinguishable from an explicit shell exit with the same
  number; diagnostics name the possible signal and retain the exact exit code,
  rather than claiming an observation the protocol cannot provide.
- Mount masks protect approved path names, not every possible hard-link alias
  of their contents. Mount setup rejects symlinks but does not provide the
  trusted file tools' descriptor-relative filesystem guarantees against hostile
  concurrent host mutations. Hostile same-user host processes and kernel
  compromise remain outside the v0.1 threat model.
- This step implements Executor but does not install it in the driver, publish
  artifacts or register/authorize shell calls. Step 7b must wire that installation
  once step 4b exists; the driver must authorize the exact plan and publish bytes.
