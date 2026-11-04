# Built-in tools

Milestone 3 is complete; [acceptance](milestone-3-acceptance.md) maps the
implemented pipeline, file/shell security and recovery guarantees to gate tests.
`cargo run --example tool_session` is an offline deterministic walkthrough using
real files, approval, a process crash, recovery advice and user resolution.

## Implemented: validation, plans, and policy

The fixed `harness_core::tools::Registry` validates and plans `read`, `write`,
`edit`, and `bash`. See [ADR 0009](decisions/0009-tool-policy.md) for canonical
plans, policy, allowances, and approval bindings. Validation and policy are pure;
planning uses a trusted `WorkspaceView`.

## Implemented: trusted file backend

`harness_core::tools::fs` supplies a real Linux `WorkspaceView` and a low-level
executor for authorized read/write/edit plans. See
[ADR 0011](decisions/0011-trusted-file-tools.md) for security guarantees, costs,
known limits, and S01 coverage. Step 7b assembles and dispatches this backend
through `SessionHandle`; bash is not executed by the file backend.

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

## Implemented: search tools

`list`, `glob` and `grep` are trusted, in-process, read-only tools on the same
descriptor-rooted workspace (ADR 0031). They are appended after `bash` in the
registry, have effects `[read]` and backend `trusted_fs`, and therefore follow
the `workspace_read` policy and `--approve-read-only` with no new approval
classification. They never use allowances. Their canonical plans bind every
defaulted argument.

- **`list`:** one directory (default `.`), at most 1,000 entries, byte-sorted
  `file`/`dir`/`symlink` rows with file sizes.
- **`glob`:** one gitignore-style pattern relative to `path` (default `.`), at
  most 1,000 file paths and 20,000 visited directories including the start.
- **`grep`:** pure-Rust `regex` syntax over UTF-8 files, optional gitignore-style
  `glob` filter, `matches` (path, line, text, optional before/after context) or
  `files`, 200 results by default and at most 2,000, context 0–5 lines.
  Files over 1 MiB, binary files (NUL in the first 8 KiB) and non-UTF-8 files
  are skipped and counted; at most 64 MiB is scanned. Line text is cut to 500
  UTF-8 bytes with a marker.

Walks never follow symlinks and skip FIFOs/device nodes. `.git`, configured
protected paths and the pinned harness data root are omitted before listing or
matching, even with `include_ignored`, and are never counted as skips. Root and
nested `.gitignore` files are read through the workspace API under the 64 KiB
cap; an unreadable one warns without revealing content and is ignored. Dot
directories are skipped in recursive walks unless a literal pattern component
names them. `include_ignored: true` disables ignore rules only.

Results are deterministic compact JSON: rows, counts, `truncated`, the exact
limit names reached, warnings, visited directories, scanned bytes and skip
counts by reason. Inline text is at most 32 KiB; a larger result publishes its
complete JSON as an artifact and keeps a bounded prefix inline. The shared
30-second deadline and cancellation propagate between entries, files and lines.
Interrupted search intents use the existing read-only restart closure.

## Planned separately

Native provider adapters and a CLI chat command remain separate work.
Trusted file operations still require workspace authorization;
they are not OS-sandboxed. `elevated` is not a Milestone 3 capability.

## Implemented: shell backend (step 7a)

`harness_core::tools::sandbox` provides a blocking Bubblewrap backend.
`Sandbox` implements the shared `Executor` trait: `execute` runs the authorized
plan against `SandboxConfig::workspace_root`. Step 7b registers bash only after
both the file layer and sandbox probe succeed; the driver authorizes the exact
plan and publishes the returned artifact bytes before committing its result.
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
./scripts/with-clean-fds.sh cargo test --locked -p harness-core --test sandbox -- --ignored --test-threads=1
./scripts/with-clean-fds.sh cargo test --locked -p harness-core --test tool_host -- --ignored --test-threads=1
./scripts/with-clean-fds.sh cargo test --locked -p harness --test doctor -- --ignored --test-threads=1
```

The suite uses Rust payloads plus ordinary sandboxed workloads. The TIOCSTI
workload uses Ubuntu's `/usr/bin/python3` standard-library ioctl support;
there is no added project toolchain or dependency. Pinned Ubuntu 24.04 CI loads
the committed AppArmor profile and runs the suite without the opt-out.

## Implemented: real tool-host assembly (step 7b)

`runtime::ToolHost::assemble(&ToolHostConfig, &PolicySnapshot)` returns a host
and a `ToolAvailability` report. The config supplies absolute `workspace_root`
and `data_root`, plus `sandbox: Option<SandboxConfig>`; `None` disables bash.
The sandbox root is always overwritten with the configured workspace. The
policy alone supplies the exclusions for both descriptor-pinned file protection
and shell plan masks. File startup failure disables shell planning too
(`planning_view_unavailable` plus the file reason), even if the sandbox probe
passes; the probe still runs to retain its own diagnostics. There is no
shell-only mode, weakened resolver, or unsandboxed fallback.

An in-workspace data root must equal or lie beneath a configured protected path,
or both backends report `data_root_unprotected`. Physical aliases and component
boundaries are checked. This is defense in depth for direct assembly/doctor:
session storage already refuses workspace/data-root overlap with
`storage_overlaps_workspace`, before a host can be assembled.

Use `SessionHandle::open_with_tools` / `create_with_tools` to assemble from the
session's actual current configuration after storage opens. The handle exposes
`tool_availability()` (legacy caller-supplied hosts return `None`). An invalid
policy installs no tools and fails a turn with `policy_invalid` before inference.
Supplying `DriverConfig.tools` as well is rejected (`tool_host_conflict`). A
mismatched storage root or session workspace is rejected too. Remember carries
the exclusions forward; any installed policy with changed exclusions faults
`tool_host_stale` before planning/execution can use stale capabilities.

For a read-only report without creating storage:

```sh
harness doctor --workspace /absolute/project --data-root /absolute/private-state
```

Both flags are required together and must be absolute. Doctor uses an empty
protected list and the default sandbox settings (with its existing executable
path overrides). It adds only `tools`; without these flags, the original report
is unchanged. The stable JSON shape is:

```json
{"file_tools":{"status":"available"},"shell":{"status":"unavailable","reasons":["shell_disabled"]}}
```

`tool_host` S02 tests cover real approvals, file post-images, shell captures,
network/protected/git denial, deadlines, Cancel, graceful handle-drop shutdown,
and a SIGKILL of a re-executed host process. SIGKILL leaves the durable intent
uncertain, never replayed; Bubblewrap's `--die-with-parent` ends the namespace.
Handle drop instead cancels and drains a durable result, leaving no uncertainty.
The capture test publishes 3 MiB through the append worker even with a 1 KiB
upload limit. Executor capture publication is not `put_artifact` upload: the
worker publishes the full configured capture (64 MiB by default) before the
result, without the upload cap or silent truncation. Artifact storage's numeric
bound is the journal's safe-integer bound, not a smaller capture-size limit.

The ADR 0012 amendment also records the in-process fork/exec lock window:
a concurrent sandbox/probe spawn can briefly retain another thread's journal
lock after that writer closes, so immediate reopen may report `session_locked`.
This transient refusal does not justify bypassing the lock or retrying effects.
Tests that spawn run in separate integration targets, not the core unit binary.
The single probe unit test that spawns takes the test-only spawn gate that also
guards every journal lock acquisition, so a child cannot hold an inherited
journal lock across another test's reopen (ADR 0012).
