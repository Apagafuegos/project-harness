#!/usr/bin/env bash
# Offline release-tool regression tests; no Rust compilation or external tools fetched.
set -euo pipefail
cd -- "$(dirname -- "$0")/.."
mkdir -p target
scratch=$(mktemp -d target/release-test.XXXXXXXX)
trap 'rm -rf -- "$scratch"' EXIT
export GITHUB_OUTPUT="$scratch/version"
export RELEASE_REF_TYPE=branch RELEASE_REF_NAME=m7-release-pipeline
./scripts/release-version.sh
version=$(awk -F= '$1 == "version" { print $2 }' "$GITHUB_OUTPUT")
export RELEASE_REF_TYPE=tag RELEASE_REF_NAME="v$version"
: >"$GITHUB_OUTPUT"
./scripts/release-version.sh
grep -Fx "version=$version" "$GITHUB_OUTPUT"
export RELEASE_REF_NAME=v999.999.999
: >"$GITHUB_OUTPUT"
if ./scripts/release-version.sh >"$scratch/wrong-tag.log" 2>&1; then
    echo 'wrong tag unexpectedly accepted' >&2
    exit 1
fi
grep 'does not match workspace version' "$scratch/wrong-tag.log"
test ! -s "$GITHUB_OUTPUT"
export SOURCE_DATE_EPOCH=1700000000
binary=${1:-/bin/true}
(
    umask 077
    ./scripts/package-release.sh "$version" aarch64-unknown-linux-musl "$binary" "$scratch/first"
)
(
    umask 022
    ./scripts/package-release.sh "$version" aarch64-unknown-linux-musl "$binary" "$scratch/second"
)
cmp "$scratch/first/harness-$version-aarch64-unknown-linux-musl.tar.gz" \
    "$scratch/second/harness-$version-aarch64-unknown-linux-musl.tar.gz"
if SOURCE_DATE_EPOCH=invalid ./scripts/package-release.sh "$version" aarch64-unknown-linux-musl "$binary" "$scratch/invalid" >"$scratch/invalid.log" 2>&1; then
    echo 'invalid epoch unexpectedly accepted' >&2
    exit 1
fi
grep 'SOURCE_DATE_EPOCH must' "$scratch/invalid.log"
echo 'release tooling tests passed (wrong-tag refusal, packaging, reproducibility, invalid epoch)'
