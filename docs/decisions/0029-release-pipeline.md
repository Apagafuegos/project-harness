# 0029: Release pipeline

Status: Implemented; dual-target workflow-dispatch proof pending default-branch
registration and orchestrator validation (M7 decision 15).

Date: 2026-11-03

## Decision

Distribute `harness` as private GitHub Release assets, not crates.io packages.
The workspace and all four crates declare `MIT OR Apache-2.0`, inherit the
repository URL, and retain `publish = false`.

Build static `x86_64-unknown-linux-musl` on `ubuntu-24.04` and static
`aarch64-unknown-linux-musl` on `ubuntu-24.04-arm`. Native apt `musl-tools`
supplies `musl-gcc` for ring's C/assembly code; Rust supplies the target's
self-contained musl link libraries. No linker overrides, Docker, Zig, cross
wrapper or extra language toolchain is needed. The locked normal/build tree
contains ring 0.17.14 and cc 1.10.0, but no OpenSSL, aws-lc or cmake. Local ARM
compilation and static-link smoke passed. Hosted ARM availability is not yet
proven: if unavailable or queued over 20 minutes, the orchestrator must arrange
and verify the apt-only cross-compile fallback before acceptance, including
execution of both binaries' smoke checks. Do not silently drop ARM.

The release profile uses thin LTO, one codegen unit, symbol stripping, and
**unwind**, not abort. ADR 0023's panic hook restores the terminal before the
previous diagnostic hook; `TerminalGuard::drop` also restores on unwinding.
Engine/storage cleanup must not be replaced by abort. The existing offline
feature-enabled PTY panic test remains part of the shared check gate.

## Workflow and authority

A `v*` tag starts version validation, the reusable ordinary CI workflow
(`ci.yml`), a native target matrix, smoke/package checks, checksum aggregation,
and tag-only publishing. A mismatched tag fails before any Cargo build.
`workflow_dispatch` has `dry_run: true` by default. **All dispatches are
non-publishing**, even on tags or with `dry_run: false`; this input cannot grant
publish authority. Publish requires a tag push and successful dependencies.
Only the publish job has `contents: write`.

Reuse all ordinary CI jobs, including S02 and its reviewed AppArmor setup,
rather than copying provisioning or weakening verification. Release builds do
not run sandbox tests or need AppArmor. Only Rust/apt/dependency provisioning
uses build-time external network; build and smoke execute through the existing
loopback-only network wrapper. Cargo remains offline outside fetch. GitHub
checkout, artifact transfers and publish necessarily use GitHub's network/API;
these are orchestration, not dependency resolution or validation.

Pin every third-party action in both workflows to a full commit SHA with a
major-version comment. Use the runner's preinstalled `gh` CLI for publishing:
this avoids another third-party action and `--verify-tag` refuses to create a
missing tag. There is no retry or asset clobber for uncertain publishing side
effects. No release or tag is created during registration/dry-run validation.

A new workflow may not dispatch before default-branch registration. Under M7
decision 15, the implementer hands off the exact reviewed branch SHA with local
and ordinary exact-head CI evidence. The orchestrator integrates that reviewed
SHA for registration and proves both target builds, smoke checks and artifacts
with `dry_run=true` before accepting M7-07. The implementer does not change main.

## Packaging

`scripts/package-release.sh` takes version, target, binary and output directory.
It requires `SOURCE_DATE_EPOCH`, taken from the checked-out commit's timestamp
(the tag commit for release runs). GNU tar sorts paths, fixes timestamps and
numeric UID/GID to zero, normalizes permissions, and uses GNU format. `gzip -n`
omits original names/timestamps. The independent archive manifest and content
comparisons fail if either license or another mandatory member is omitted.
An absent future user guide is explicitly noted, not silently assumed present.
Each CI target packages twice and compares bytes. Aggregation generates sorted
`SHA256SUMS` in a short-lived artifact even for dry runs. Publish verifies and
regenerates that manifest before attaching the two archives and checksums.
Tarball reproducibility means identical inputs/tools/epoch produce identical
archives; it is not a claim of bit-identical binaries across compiler versions
or checkout paths. Build provenance/attestations are not implemented here.

## Evidence and limits

The single permitted local ARM musl build took **266 seconds**, producing a
**10,427,912-byte** stripped, statically linked ELF; `--version`, `--help` and
`doctor --help` passed. Before-profile and aarch64-gnu measurements are
unavailable under the one-release-build budget; no size delta is claimed.
See [development](../development.md#releases) for operations. Hosted dual-target
proof is a separate acceptance requirement, not implied by this local result.
