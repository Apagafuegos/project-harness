# 0012: Bubblewrap backend and mandatory S02 security gate

Status: Implemented for Milestone 3 steps 7a–7b. The amendment below records
real host assembly, runtime registration and end-to-end verification.

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
pre_exec callback.

The Ubuntu 24.04 GitHub runner supplied an inherited nonblocking pipe at fd 142
(flags 2048, lacking CLOEXEC). CI run 40752712939 identified the failure in the
**descriptor audit before spawn**, not namespace acquisition. Startup failures
now preserve their stage and underlying reason, including fd flags and target
metadata (never descriptor contents). CI invokes `scripts/with-clean-fds.sh`
before Cargo: it closes inherited non-CLOEXEC descriptors above stderr in that
launch child, preserving the runner parent and standard streams. It does not
relax the backend's audit or change AppArmor. S02 reproduces the condition with
a real nonblocking pipe duplicated to fd 142 or above, verifies launcher cleanup
and standard-stream preservation, and separately still requires the product to
reject an unexpected inheritable descriptor before executing a command. The
PID namespace identity check is unchanged; the optional-field hypothesis was
not the cause of these CI failures. An isolated subprocess test verifies refusal of a deliberately
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

## Amendment: fast-exit namespace acquisition (S02 flake)

The original CI failure's cause cannot be identified without its result text.
The sandbox and tool-host status assertions now share a helper that prints code
and model-facing text. Local Bubblewrap **0.11.1** experiments nevertheless proved
a `/proc/<init>/ns/pid` teardown race: after a successful `pidfd_open`, lookup
can fail with EACCES or ESRCH, not only ENOENT. In 3,000 immediate `exit 7`
runs with acquisition delayed until launcher reaping, 76 EACCES and 5 ESRCH
lookups were immediately followed by pidfd fdinfo `Pid: -1`. Only these two
additional lookup errors are treated as disappearance, and only with that
positive pidfd-retirement evidence; an unreadable fdinfo, live pidfd, identity
mismatch or unrelated error still fails closed. No unverified pidfd is installed
or signalled. A retired pidfd proves the process bearing the reported init PID
was reaped; even if that PID was reused, the original init necessarily died
first. Launcher reaping, capture draining and existing cleanup waits remain.
Pure injected-facts tests cover both errors with and without retirement and
retain refusal of mismatched or missing namespace identities.

Do **not** use `exit-code` alone as proof that init exited: the 0.11.1
[manual](https://github.com/containers/bubblewrap/blob/v0.11.1/bwrap.xml)
describes the child command's exit, while
[`do_init` and `monitor_child`](https://github.com/containers/bubblewrap/blob/v0.11.1/bubblewrap.c)
show PID 1 writing the foreground command's status to an eventfd after reaping
it, then continuing to reap descendants. The outer monitor writes JSON status
from that eventfd without first reaping PID 1 (or, on the SIGCHLD path, after
reaping PID 1). With the production namespace/session/die-with-parent flags,
`(sleep 30 &) ; exit 7` reported `exit-code: 7` while init's pidfd was still
live in 100/100 trials. A direct bwrap exec of a nonexistent executable emitted
init information but no exit-code, as the setup-finished pipe requires a
successful exec. Immediate foreground failure cannot itself omit either
control message or cause a socket, waitpid, capture or spawn error. PID reuse
remains a theoretical identity-mismatch cause, not an observed one (local
`pid_max=4194304`). In a separate 1,000-run delayed-acquisition experiment,
447 pidfd opens returned ESRCH, 172 namespace lookups returned ENOENT, 363
matched, 17 returned EACCES and one returned ESRCH.

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
- Step 7a alone implemented Executor; step 7b now installs it through the
  runtime host assembly below. A native provider and CLI chat remain separate.

## Amendment: session-owned host assembly (step 7b)

`runtime::ToolHostConfig` contains absolute workspace and data-root paths and
optional `SandboxConfig`. `ToolHost::assemble` pins `FsWorkspace` using exactly
`PolicySnapshot.protected_paths`, overrides the sandbox workspace, and probes
Bubblewrap. Bash requires **both** backends to be usable: its planner inspects
cwd through `FsWorkspace` (decision 31). A failed file layer still runs the
sandbox probe and adds `planning_view_unavailable` and the file reason. No
shell-only mode, fabricated directory facts or planner changes were introduced.

An in-workspace data root must equal or descend from a configured exclusion,
otherwise both backends are `data_root_unprotected`. Root comparisons resolve
existing ancestors, handle component boundaries and aliases, and refuse `..`
or unresolvable symlink roots rather than lexically guessing. Session storage
already rejects overlap before assembly; that earlier refusal is asserted at
the handle while assembly and doctor exercise the defense-in-depth check
(decision 34). Storage rules are unchanged.

`SessionHandle::open_with_tools` and `create_with_tools` open storage before
assembly. A preinstalled host, wrong storage root or different session workspace
is rejected, never merged. Startup assembly runs on a blocking task. Invalid
policy assembles nothing and fails a turn `policy_invalid` before inference;
legacy no-tool runtime behavior is unchanged. The handle's optional
`tool_availability()` describes startup, not a concurrent policy mutation.

The driver remembers the assembled exclusions and faults `tool_host_stale`
after a changed policy is installed, before dispatching its effects. Already
applied client events keep their sequence. Remember carries exclusions forward.
Replacement keeps this binding rather than silently rebuilding capabilities.
No general configuration-update command is added.

`ToolAvailability` serializes `file_tools` and `shell`, each with `status`
(`available` or `unavailable`) and unavailable `reasons`. Probe errors may
contain arbitrary executable output, so assembly emits content-free reason
categories instead of raw captures/environment values. Descriptor-audit
failure is reported at startup as `descriptor_audit_failed`. The existing
host-capability report is unchanged.

`harness doctor --workspace DIR --data-root DIR` adds this `tools` report,
with an empty protected list and default sandbox config plus the existing
executable overrides. Both absolute paths are required together; doctor creates
no storage. Without these flags it takes the original report path (decision 33).

The required S02 gate and both pinned CI jobs run `tests/tool_host.rs` and the
forwarding-fake doctor test, through the existing clean-fd launcher in CI. They
exercise actual file edits, remembered grants, shell output/capture digests,
.git/protected/network denial, timeout/Cancel cleanup and subsequent turns.
A protected path with a symlink ancestor triggers the real file opener failure.
An isolated inheritable-fd child proves shell refusal during assembly.

**Known in-process lock/spawn gap:** a sandbox/probe spawn briefly inherits
other threads' journal lock descriptors between fork and exec, despite those
descriptors being close-on-exec. If another thread releases and immediately
reopens such a journal during that window, the open can report `session_locked`
until the child execs. This is a transient refusal, not permission to bypass the
lock or silently retry an uncertain effect. Step 7b does not change the lock or
spawn implementation. Probe regressions run in the separate S02/CLI integration
binaries; core unit tests use a disabled shell so subprocesses cannot interfere
with unrelated immediate-release/reopen assertions. Failed file startup with a
failed probe is retained in the CLI tools-report tests; failed file startup
with a working probe and broken-bwrap refusal remain in `tests/tool_host.rs`.

A SIGKILL test re-executes the test binary and kills the host only after the
shell appends a counter line following its durable intent. No Rust destructor
runs; Bubblewrap's `--die-with-parent` tears down the namespace. Reopen records
uncertainty, accepts user resolution, and never replays the counter command,
including after a later turn. Separate handle-drop coverage proves graceful
cancel/drain, no live namespace and no uncertainty (decision 32).

The append worker publishes executor artifacts directly via the artifact store,
not the handle's 16 MiB upload path. The store's safe-integer size bound exceeds
64 MiB; no upload limit is raised. An end-to-end 3 MiB capture test uses a 1 KiB
upload cap and verifies every captured byte and its digest.
