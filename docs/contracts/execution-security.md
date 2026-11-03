# Contract 3: Execution & Security

Status: Implemented Linux v0.1 boundary. Accepted
[ADR 0009](../decisions/0009-tool-policy.md),
[0011](../decisions/0011-trusted-file-tools.md) and
[0012](../decisions/0012-sandbox-backend.md) refine
[RFC-001](../rfcs/001-core-types-state-persistence.md).
Real S01/S02 tests and the [accepted audit](../security/v0.1-audit.md) provide
bounded evidence, not a proof that no vulnerabilities exist.

## Inputs and ownership

The authorization layer accepts a validated operation and trusted policy,
returning allow, ask, or deny. The executor accepts an opaque authorized
operation, workspace capabilities, limits, and cancellation. The model cannot
choose its own capabilities or manufacture approval.

Routine means trusted in-process Rust code operating entirely within explicitly
authorized workspace boundaries without spawning any executable. Authorized
file reads/listing/search/writes/edits can qualify. `ls`, Git, builds, tests,
shells, installers, and all other external programs always use the process backend.

## Guarantees

- Built-ins enforce workspace/protected-path policies using descriptor-relative
  filesystem operations, not path-prefix checks or check-then-open strings.
- Prevent traversal/symlink escapes; write/edit use safe replacement procedures.
- Built-ins still require authorization. They are not OS-sandboxed, so their
  security depends on implementation correctness and policy.
- External programs run through configured Bubblewrap isolation with explicit
  mounts, namespaces, a filtered environment, process cleanup, and output limits.
- File plans bind exact targets; process plans bind read-only or writable
  workspace mounts plus protected-path masks/read-only `.git` and private
  temporary/home space. A writable shell workspace is not per-file containment.
  `elevated` is not offered in v0.1; any future expansion needs a new decision.
- Subprocess network access is always denied in v0.1. A requested exception is
  `network_unsupported`; approval cannot supply a missing enforcement backend.
- Provider credentials, trusted policy, and session storage are not exposed to
  arbitrary tool programs. Environment construction is an allowlist.
- Missing required isolation fails closed. A safe-looking command never earns
  an implicit unrestricted fallback.
- Timeout/cancellation cleans up descendants before subsequent tools execute.
- Permission decisions, uncertainty, and outcomes are auditable; elevation is
  not offered in v0.1.

The implementation uses strict no-symlink/no-mount workspace resolution,
exact-plan approvals and deny-network-only process support. Restricted egress
needs a separate enforcement backend before exceptions can work; approval alone
cannot create it. S01–S02 run real Linux isolation tests in CI, and `bash` is
offered only when both the file plan and sandbox probes pass. Kernel compromise
and hostile same-user host processes are outside the stated v0.1 threat model.

No seccomp filter is installed. TasksMax requires a usable systemd user manager;
uninterruptible kernel I/O can delay cleanup. Shell path binds do not protect
all hard-link aliases or hostile host races; root renames can diverge file-tool
and shell views. Absent protected paths cannot be masked in advance: creation
is detected after execution without rollback; initially absent `.git` can be
created by the first shell command. Trusted built-ins revalidate descriptors
but do not promise atomic compare-and-swap against unrelated host mutation.
User config and user instructions use a separate user-owned std-fs boundary;
project instructions never become authority. See audit residual risks before
claiming stronger isolation or redaction.

`crates/core/tests/sandbox.rs`, `crates/core/tests/tool_host.rs` and
`crates/cli/tests/doctor.rs` run the required real host suites in CI;
`crates/core/src/tools/fs/tests.rs` covers trusted-file containment.
The audit's probe-environment and provider-code regressions close separate
secret channels; transcript text and exports are not general secret scrubbers.

S02 loopback checks attribute connections using a fresh random nonce for each
run, on both 127.0.0.1 and 127.0.0.2. Host services may probe new listeners, so
an accept alone is not evidence of sandbox egress. After execution, tests drain
pending connections with bounded reads, reject any received nonce, and count/log
unrelated traffic. Tool-host checks also require a failed tool result with a
connect-style error; the in-sandbox Rust payload still requires connection
refusal and checks DNS, external TCP and the loopback-only interface list.
`./scripts/test-loopback-probes.sh` explicitly enables repeated registry-driven
side probes; it is not part of the default gate (see `docs/providers.md`).
