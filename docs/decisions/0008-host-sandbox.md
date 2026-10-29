# 0008: Host sandbox requirements and test placement

Status: Accepted and implemented for Milestone 3. The `harness doctor` probe,
CI profile, real shell security suite and required `check.sh` gate exist.
See [Milestone 3 acceptance](../milestone-3-acceptance.md).

Date: 2026-10-25

## Context

The shell runs only inside Bubblewrap with an explicit user namespace
(`docs/contracts/execution-security.md`, Milestone 3 plan D8). Ubuntu 24.04
sets `kernel.apparmor_restrict_unprivileged_userns=1`. With no AppArmor
profile for `bwrap`, the namespace is created without capabilities and
`bwrap` fails (`loopback: Failed RTM_NEWADDR: Operation not permitted`).
The step 1 spike found exactly that on the GitHub runner, and a working
sandbox there once a profile granting `userns` to `/usr/bin/bwrap` was
loaded. The development machine already had such a profile.

## Decision

**The shell security suite is required by default.** It runs locally through
`scripts/check.sh` and fails the check when the sandbox probe fails. The only
opt-out is `HARNESS_ALLOW_NO_SANDBOX=1`, which makes `check.sh` print
`SKIPPED: shell security suite` instead. CI never sets it.

**CI runs the suite with a committed, reviewed profile.**
`ci/apparmor/harness-ci-bwrap` attaches to `/usr/bin/bwrap` only and grants
what Ubuntu's `usr.bin.bwrap` grants: no confinement plus `userns`. CI loads
it with `apparmor_parser -r`. Lifting the sysctl is not used.

**The product never installs AppArmor profiles** or changes host
configuration. On a host where the probe fails, `bash` is not registered and
`harness doctor` reports why, with a pointer to the documentation.

**The probe checks the namespace from the host.** Besides the checks inside
the sandbox, the doctor reads `uid_map` and `gid_map` of every sandbox
process from outside and requires exactly one line mapping one id to the
invoking effective uid and gid. A map the sandbox reads about itself is
relative to `bwrap`'s own namespaces and cannot show an escape to host root.

**Recorded gaps stay recorded.** Without a reachable systemd user manager
there is no `TasksMax` limit beyond the timeout, and there is no seccomp
filter. Both are reported, never silent. To find the user bus, the doctor
uses `XDG_RUNTIME_DIR`, or derives `/run/user/<euid>` only if that directory
is owned by the effective uid and not group- or world-writable and its `bus`
is a socket owned by the same uid. The derived value goes to the
`systemd-run` child only.

## Consequences

- A contributor on a stock Ubuntu 24.04 host without a `bwrap` profile cannot
  pass `check.sh` without either a profile they choose to
  install or the explicit opt-out.
- CI evidence depends on the committed profile; a change to it is a reviewed
  change to the test environment, not to the product.

## Alternatives considered

- **Set `kernel.apparmor_restrict_unprivileged_userns=0` in CI:** works, but
  disables the restriction for every program on the runner instead of one
  binary.
- **Make the suite opt-in:** it would quietly stop running on hosts that lose
  the capability.
- **Install a profile from the harness:** needs root and changes the host's
  security policy, which is the user's decision.
