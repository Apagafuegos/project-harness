# 0009: Pure tool contracts and configuration policy

Status: Accepted and implemented for Milestone 3 step 3. Execution, approvals in
runtime state, descriptor resolution, and recovery changes remain planned.

Date: 2026-10-27

## Decision

`harness_core::tools` is a module, not a crate. Its fixed registry contains
`read`, `write`, `edit`, and `bash`, with no registration/replacement API.
Resolve the name and enabled-tools policy before validation. Compile each strict
JSON Schema once. Validation is pure in arguments and produces a private,
defaulted `ValidatedOperation`; planning reads only a supplied `WorkspaceView`
and produces a private immutable `Plan`. Neither type can be deserialized from
model JSON. The definitions alone do not make an executor available. Runtime's
existing `tools_unavailable` behavior is unchanged.

`WorkspacePath` validates its only public constructor, limits input to 4096
UTF-8 bytes, rejects absolute paths, traversal components and NUL, and normalizes
redundant separators and dot components. `.` is the workspace root, including
for a default shell cwd. String/component comparisons here implement policy
patterns, not filesystem containment or descriptor protection. The trusted view
supplies kind, size, link count, content digest, and inherited protection. Tests
use only an in-memory fake workspace. The module performs no runtime I/O.

File planning requires regular files with at most one link, existing mutation
parents, UTF-8 reads/edits bounded to 1 MiB, and one literal non-overlapping edit
match. Replacement contents are also bounded to 1 MiB. Refusals contain codes,
status, and short text without file contents. File deadlines are 30 seconds;
shell deadlines default to 120 seconds and range from 1 to 600 seconds. JSON
Schema integral numbers such as `1.0` are normalized to integer plan arguments;
present numbers must never accidentally select an omitted argument's default.

## Plan format version 2

Canonical compact JSON uses struct field order, not arbitrary object iteration:

`version`, `call`, `name`, `arguments`, `workspace`, `cwd`, `effects`, `backend`,
`mounts`, `network`, `timeout_ms`, `policy_revision`, `target`.

The version is 2. Arguments include validated defaults and normalized paths.
File targets contain ordered `path`, `pre_image` (SHA-256 or `"absent"`), and,
for write only, `content_sha256`. Shell targets are null. Shell mounts contain
workspace `.` (`rw` unless workspace mutation is denied, otherwise `ro`), then
`.git` as a separate `ro` mount when present. Network is always `deny`.

`Plan::digest()` is `Digest::of(Plan::bytes())`. Approvals carry exactly call,
plan digest, plan bytes and policy revision. `Plan::effects()` is the only
source for a future intent: read has `[read]`, write/edit
`[filesystem_mutation]`, bash `[process]`. Tests pass these values through the
real storage state validator in memory without changing storage rules. This
format version does not change journal schema v2 or existing example artifacts.
Golden v2 plans are in `crates/core/src/tools/golden/`.

## Policy table (first matching row)

| Case | Decision |
| --- | --- |
| Target or cwd has Denied protection | Deny `protected_path` |
| Write/edit target has ReadOnly protection | Deny `protected_path` |
| Read; workspace_read allow / ask / deny | Allow / Ask / Deny `policy_denied` |
| Write/edit; workspace_mutation deny | Deny `policy_denied` |
| Write/edit; matching allowance | Allow |
| Write/edit otherwise | Ask |
| Bash; process deny | Deny `policy_denied` |
| Bash; exact matching allowance | Allow |
| Bash otherwise | Ask |

Planning refuses protected access before fetching text; `decide` also checks
the protection rows. `.git` is built-in read-only; configuring `.git` in
`protected_paths` instead denies all accesses beneath it. Descriptor protection
and configuration protection combine by taking the stricter value. A plan is
bound internally to the exact configuration artifact as well as its revision
and protection facts; stale policy, disabled tools or substituted facts fail
closed with `policy_denied`.

## Allowances are configuration (review point 2)

Allowances are not journal events. Write/edit allowances are tool plus exact
normalized path, directory prefix ending in `/` (strict descendants, not the
directory itself), or `.` for the whole workspace. Matching is component-aware:
`src/` never covers `src2/a`. Shell allowances match exact command bytes and
normalized cwd, with no prefixes/globs. They never override a deny policy or
protected path. Reads never use allowances.

`Allowance::remember` derives only the exact file path or exact shell command
and cwd; it returns none for reads. `grant` appends if not already present,
`revoke` removes matches, and both replace the policy revision while preserving
all other configuration values, including unknown fields and the original
values/order of existing allowances. Serialization is compact JSON; source
whitespace and object-key ordering are not preserved. The caller publishes the
new artifact and records `configuration_changed` in step 4. A new revision
invalidates pending approvals; existing allowances survive because they are
carried forward in configuration.

## Explicitly deferred

- Step 4: runtime integration, `SubmitApproval`, ticket IDs and five-minute
  expiry, remember/configuration commits, executor threading and cancellation.
- Step 5 (D6): descriptor-relative `openat2` with `RESOLVE_BENEATH |
  RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV`, no symlinks in
  any component and no fallback if unavailable. Descriptor/ancestor `(dev, ino)`
  protection, real content hashing, and pre-image rechecking are enforced there.
- Step 6: read-only intent recovery (D9); no recovery behavior changes here.
- Step 7: Bubblewrap enforcement, protected mount masking, startup probe and
  security suite. Merely planning bash does not register it for execution.
- `elevated` and subprocess network exceptions are **not in Milestone 3**.
  No argument or policy can request them. Non-deny network configuration fails
  with `network_unsupported`. Seccomp and unavailable TasksMax remain ADR 0008's
  recorded gaps.

## Verification

The tools unit tests cover strict schemas and byte boundaries, path validation,
four golden plans and digests, every planning refusal and policy row, exact
approval binding, protections, allowance preservation and matching, seeded
SplitMix64 no-widening properties, real v2 storage effects validation, all
ResultCode spellings against the schema, and a source check forbidding runtime
filesystem/process/asynchronous I/O. The baseline runtime/session tests remain
unchanged. Mutation checks are recorded in the handoff report.
