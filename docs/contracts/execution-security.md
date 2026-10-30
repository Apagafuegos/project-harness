# Contract 3: Execution & Security

Status: Selective sandboxing and Linux Bubblewrap are provisionally agreed;
detailed policy/defaults proposed in
[RFC-001](../rfcs/001-core-types-state-persistence.md).

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
- `workspace-read` grants read-only workspace access. `workspace-write` grants
  authorized write scopes and private temporary/build space. `elevated` is a
  user-approved capability expansion for one exact operation, not a bypass.
- Subprocess network access is denied by default. Any exception must be both
  explicitly authorized and enforceable at the requested scope.
- Provider credentials, trusted policy, and session storage are not exposed to
  arbitrary tool programs. Environment construction is an allowlist.
- Missing required isolation fails closed. A safe-looking command never earns
  an implicit unrestricted fallback.
- Timeout/cancellation cleans up descendants before subsequent tools execute.
- Permission decisions, elevation, uncertainty, and outcomes are auditable.

The RFC proposes strict no-symlink/no-mount workspace resolution, approval defaults,
and deny-network-only process support initially. Restricted egress needs a
separate enforcement backend before exceptions can work; approval alone cannot
create it. Conformance S01–S02 must use real Linux isolation tests before `bash`
is enabled. Kernel compromise and hostile same-user host processes are outside
the stated v0.1 threat model.

S02 loopback checks attribute connections using a fresh random nonce for each
run, on both 127.0.0.1 and 127.0.0.2. Host services may probe new listeners, so
an accept alone is not evidence of sandbox egress. After execution, tests drain
pending connections with bounded reads, reject any received nonce, and count/log
unrelated traffic. Tool-host checks also require a failed tool result with a
connect-style error; the in-sandbox Rust payload still requires connection
refusal and checks DNS, external TCP and the loopback-only interface list.
`./scripts/test-loopback-probes.sh` explicitly enables repeated registry-driven
side probes; it is not part of the default gate (see `docs/providers.md`).
