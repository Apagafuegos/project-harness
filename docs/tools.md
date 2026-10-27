# Built-in tools

## Implemented: validation, plans, and policy

The fixed `harness_core::tools::Registry` validates and plans `read`, `write`,
`edit`, and `bash`. See [ADR 0009](decisions/0009-tool-policy.md) for canonical
plans, policy, allowances, and approval bindings. Validation and policy are pure;
planning uses a trusted `WorkspaceView`.

## Implemented: trusted file backend

`harness_core::tools::fs` supplies a real Linux `WorkspaceView` and a low-level
executor for authorized read/write/edit plans. See
[ADR 0011](decisions/0011-trusted-file-tools.md) for security guarantees, costs,
known limits, and S01 coverage. Runtime registration/dispatch is **not** wired by
this step, and bash is not executed by this backend.

```rust,no_run
use harness_core::tools::{FsProtectedPaths, FsWorkspace, WorkspacePath};

let protected = FsProtectedPaths {
    configured: vec![WorkspacePath::new("private/policy").unwrap()],
    // Trusted session owner supplies this when its data root is inside the
    // authorized workspace. Use None only when the data root is outside it.
    harness_data_root: Some(WorkspacePath::new(".harness-state").unwrap()),
};
let workspace = FsWorkspace::open("/authorized/project", &protected)?;
# Ok::<(), harness_core::tools::FsUnavailable>(())
```

Opening pins the workspace root and probes strict `openat2`. Unsupported or
blocked `openat2` makes the backend unavailable, never less restrictive. Paths
are normalized relative `WorkspacePath`s; no traversal, symlinks, magic links,
or mount crossings are allowed. Existing configured exclusions and the supplied
in-workspace data root are denied by object/ancestor `(dev, ino)` identity;
`.git` is read-only. Protection follows renamed objects, and exclusions are
pinned for the lifetime of the workspace.

Plan through the registry with this same workspace and trusted policy, obtain
authorization with `decide`, then let the runtime construct an
`AuthorizedOperation` after its write-ahead intent commits. `FsWorkspace`
implements `tools::Executor` with backend `TrustedFs`; the trait executes that
operation using the shared `tools::CancelToken` and returns
`tools::ExecutionOutcome` directly. Its fields are `status`, `code`, `content`,
`exit_code`, `duration_ms: Some(..)`, `output_truncated`, and `artifacts`.
Execution statuses remain explicitly independent of planning refusals. The
low-level `workspace.execute(&plan, &cancel_token)` entry point also remains;
its caller owns authorization and must never dispatch a denied or unapproved
plan. Bash and other non-`TrustedFs` plans fail with `infrastructure_failure`.

- **Read:** Recheck regular-file/link-count/protection facts and SHA-256, then
  return the requested byte range adjusted inward to valid UTF-8 boundaries.
  Text begins with `bytes START..END; more=BOOL; output_truncated=BOOL`, then a
  newline and file text. `END` is the next continuation offset. Aggregate
  model-facing text is at most 32 KiB; no lossy decoding or file artifacts.
  Empty/split ranges may contain no complete characters. Inspection hashes
  without a whole-file cap; execution checks its 30-second deadline between
  chunks and blocking operations.
- **Write:** The parent must already exist. Recheck the planned digest or
  absence, then replace through an exclusive random temporary file, mode
  restoration, fsync, rename, and parent fsync. New files are 0644; replacement
  preserves only the original ordinary permission bits (`mode & 0o777`), stripping
  setuid, setgid, and sticky bits. A concurrently created absent target cannot be
  overwritten (`RENAME_NOREPLACE`).
- **Edit:** Recheck the pre-image and reapply exactly one literal occurrence,
  counting overlaps. Source and resulting text are bounded to 1 MiB and must be
  UTF-8. Replacement uses the same atomic procedure as write.

Write/edit success text reports `sha256=POST_IMAGE; bytes=N`. A stale pre-image
returns `failed`/`stale_plan`. Other execution-time access errors are `failed`
with their code; cancellation is `cancelled`; deadline expiry is `timed_out`;
unexpected I/O errors are `infrastructure_failure` with an errno diagnostic.
Before rename, cancellation or failure cleans up the temporary and leaves the
original untouched. After rename, cancellation does not undo the mutation or
replace success. Parent-fsync failure can report an infrastructure failure
following a committed replacement; never assume rollback or retry it silently.

## Planned separately

Runtime effect wiring, recovery integration, and native provider adapters are
separate work. Trusted file operations still require workspace authorization;
they are not OS-sandboxed. `elevated` is not a Milestone 3 capability.

## Implemented: shell backend (step 7a)

`harness_core::tools::sandbox` provides a blocking Bubblewrap backend. Bash is
**not yet registered** with the driver. `Sandbox` implements the shared
`Executor` trait: `execute` runs the authorized plan against
`SandboxConfig::workspace_root`. Installation still belongs to step 7b; the
driver must authorize the exact plan and publish the returned artifact bytes.
The shared `tools::CancelToken` and `ExecutionOutcome` are used directly.

```rust,no_run
use harness_core::tools::{CancelToken, Sandbox, SandboxConfig};

let config = SandboxConfig {
    workspace_root: "/authorized/project".into(),
    ..SandboxConfig::default()
};
let sandbox = Sandbox::probe(config)?;
if let Some(reason) = sandbox.process_limit_gap() {
    eprintln!("{reason}");
}
// After authorization:
// let outcome = sandbox.run(&plan, workspace_root, &CancelToken::new());
# let _ = CancelToken::new();
# Ok::<(), Box<dyn std::error::Error>>(())
```

The backend takes command/cwd/deadline and workspace mount modes from the plan,
never raw model sandbox flags. It denies network access, clears inherited
variables, recreates private home/tmp/proc/dev, binds configured runtime paths
read-only, and applies `.git` read-only protection and protected-path masks.
A failed probe or setup returns an infrastructure failure, never a host retry.
`harness doctor` uses the same probe and explains host availability without
changing host configuration.

Defaults are a 64 MiB aggregate capture, 32 KiB model content, two-second kill
grace and TasksMax=64. stdout and stderr are separately captured; artifacts
contain the full bounded raw bytes (stdout first, stderr second), and
`duration_ms` is `Some(elapsed_ms)`. Model text uses head/tail truncation with
a marker; its compact JSON, including escaping, is bounded to 32 KiB. Timeout
and cancellation kill the PID namespace init and wait for cleanup; successful
foreground exit leaves no background descendants either. Executed statuses
are chosen explicitly, not taken from planning refusals.

When a systemd user manager is unavailable, the backend records the lack of a
TasksMax process limit; deadlines and namespace cleanup still apply. Missing
required isolation always fails closed. There is no seccomp filter, network
exception or elevated mode in M3. An absent protected path cannot be mounted:
post-run detection returns `failed` / `protected_path` if it was created, and
leaves the object in place. See [ADR 0012](decisions/0012-sandbox-backend.md)
for the full security boundary and remaining gaps.

### Shell security tests

`./scripts/check.sh` requires the real-Linux S02 suite by default. The only
local opt-out is `HARNESS_ALLOW_NO_SANDBOX=1`, which prints
`SKIPPED: shell security suite`. This does not enable bash in the product.
To run the suite alone:

```sh
cargo test --locked -p harness-core --test sandbox -- --ignored --test-threads=1
```

The suite uses Rust payloads plus ordinary sandboxed workloads. The TIOCSTI
workload uses Ubuntu's `/usr/bin/python3` standard-library ioctl support;
there is no added project toolchain or dependency. Pinned Ubuntu 24.04 CI loads
the committed AppArmor profile and runs the suite without the opt-out.
