# 0011: Descriptor-relative trusted file tools

Status: Accepted and implemented for Milestone 3 step 5. The shared executor
interface is implemented; runtime dispatch is implemented in step 4b and
real host installation/shell execution in step 7b (ADR 0012). Descriptions of
runtime wiring below record the step 5 ownership boundary.

Date: 2026-10-27

## Decision

`tools::fs::FsWorkspace` owns an `O_PATH | O_DIRECTORY | O_CLOEXEC` root
capability, selected explicitly by the trusted session owner. Startup probes
`openat2`; `ENOSYS`, seccomp `EPERM`, or any other probe failure makes file tools
unavailable, without a fallback. All workspace path resolution and readable
reopens use full relative prefixes from this pinned root with `RESOLVE_BENEATH |
RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV`. Intermediate
identities are checked again after opening the target. The original root string
is an identity only, never reopened. Temporary creation, rename, cleanup, and
sync use the pinned parent descriptor.

Startup exclusions are configured workspace-relative paths and the harness data
root's workspace-relative location, supplied by the trusted session owner when
it is inside the authorized workspace (including `.` when equal to it). A data
root outside the workspace needs no additional exclusion. Existing exclusions
are `Denied`; existing `.git` is `ReadOnly`, unless a denial takes precedence.
Protection compares `(st_dev, st_ino)` of the object and all resolved ancestors,
not path strings. Protected descriptors remain open so inode numbers cannot be
recycled. Missing exclusions have no inode to pin; configuration policy still
denies their configured spellings. Runtime wiring must supply the data root and
must not treat opening this capability as authorization of a model call.

`inspect` streams SHA-256 in 64 KiB chunks without a whole-file size cap
(decisions 14 and 23), returning kind, size, link count, digest, and inherited
protection. A detected concurrent change fails with `stale_plan`. `read_text`
enforces its caller's byte limit and UTF-8 without lossy decoding. Edit planning
already checks the 1 MiB size limit before calling `read_text`; digest inspection
still necessarily reads the file to bind its pre-image.

`FsWorkspace` implements the shared `tools::Executor` with backend `TrustedFs`.
Its trait entry point consumes an `AuthorizedOperation` and `tools::CancelToken`,
then executes the operation's opaque typed `Plan`, not model JSON. Minimal workspace,
target pre-image, and deadline binding metadata are decoded from that plan's
canonical bytes, without modifying the pure planner's API. File operations come
from its typed internal accessor. A different workspace identity, a bash plan,
or a backend other than `TrustedFs` returns `infrastructure_failure`. The shared
`tools::ExecutionOutcome` is returned directly, with `duration_ms: Some(..)`;
no adapter is required. The low-level plan entry point remains available, but
its caller must perform policy authorization and approval binding first.

Execution explicitly sets `failed`, `timed_out`, `cancelled`, or
`infrastructure_failure` rather than copying planning `Refusal::status`
(decision 22). `code` is absent exactly on success. Outcomes contain the shared
seven fields from decision 19; file tools emit no artifacts or exit code.
Read output includes its actual byte range, remaining-content flag, and
truncation flag. The total model-facing text, including metadata, is at most
32 KiB. Bounded adjacent context validates genuine split UTF-8 characters;
malformed leading bytes and incomplete characters at EOF are `not_utf8`.

## Replacement and cancellation

Read, write, and edit reject multiply linked files, nonregular targets, and
protected access again at execution. Read rehashes the opened file before
reading its requested range. Write/edit compare the pre-image before staging
and once more immediately before committing, including parent identity,
protection, link count, and digest. Edit counts overlapping byte-offset matches
and replaces exactly one, with a 1 MiB output limit.

Replacement creates a UUID-random `O_CREAT | O_EXCL` temporary file in the pinned
parent, initially mode 0600, writes it, copies only ordinary permission bits
(`original_mode & 0o777`, or 0644 for a new file), and fsyncs data and mode before
descriptor-relative `renameat_with`. Setuid, setgid, and sticky bits are never
carried onto replacement contents; both write and edit test `0o4755` becoming
`0o755`, along with the other special-bit combinations.
An absent pre-image uses `RENAME_NOREPLACE`; a concurrent creation is
`stale_plan`. The parent is fsynced after rename. Errors or cancellation before
rename remove the temporary. Cleanup does not obey cancellation, and cleanup
errors are infrastructure failures. A drop guard also attempts cleanup during
unwinding. Cancellation/deadline are checked before every pre-commit blocking
step, including every hash chunk, but never after rename. Post-rename
cancellation therefore preserves success. A parent-fsync I/O failure is an
infrastructure failure after a committed replacement; its durability is unknown
and it must not be silently retried or represented as rollback.

## Costs, limits, and threat model

Hashing very large files costs O(file size) I/O and CPU, even for tiny read
ranges. Planning inspection has no size cap or deadline in `WorkspaceView`;
execution uses the plan's 30-second deadline, checked between blocking steps and
chunks. This is cooperative checking, not interruption of an in-flight kernel
syscall. The local-filesystem assumption and non-hostile same-user host-process
threat model from RFC-001 §4.2 apply. Digest rechecks detect changes; they do not
promise compare-and-swap with unrelated host processes. There is no recovery,
runtime registration, elevation, process execution, or sandbox fallback here.

## Verification and CI mount

`tools::fs::tests` uses real temporary directories and descriptors for the S01
gate: invalid paths never reach `openat2`; final/intermediate/outside and proc
symlinks; hard links; stale content and creations (also precisely at rename);
ancestor symlink swaps and relocation outside the root; renamed protected
objects/directories, in-workspace data root, and `.git`; cancellation at every
observed pre-rename blocking step for absent and existing targets; cleanup on
injected I/O failures; and successful post-rename cancellation. Additional tests
cover unavailable probes, deadlines/statuses, UTF-8 edge corruption, output
paging, file modes, atomic inode replacement, and root-name replacement.

The large-file regression creates a real sparse file larger than the withdrawn
256 MiB cap, reads a real hash chunk, then truncates it deterministically and
asserts `stale_plan`, not `too_large`. A smaller multi-chunk file checks the full
streamed digest and read-plan execution. This avoids a minute-plus software
SHA-256 debug test on ARM without weakening the no-cap check.

The mount test opens `/dev` read-only and resolves `shm`. Linux GitHub-hosted
Ubuntu runners provide `/dev/shm` as a separate tmpfs mount. It first asserts
that `/dev` and `/dev/shm` have different device IDs; a missing or unsuitable
mount fails with an explicit provisioning message, never a skip. No privileged
mount setup or writes to `/dev` are required. Both `scripts/check.sh` and CI's
required Rust workspace job execute these ordinary, non-ignored unit tests.
The source I/O test explicitly classifies `fs` as the sole exception and still
checks every pure tool module. No crates, dependency features, runtime, or
session implementation were changed.
