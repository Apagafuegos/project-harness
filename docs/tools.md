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

Runtime effect wiring, recovery integration, shell execution in Bubblewrap, and
native provider adapters are not implemented by this backend. Trusted file
operations still require workspace authorization; they are not OS-sandboxed.
`elevated` is not a Milestone 3 capability.
