#!/usr/bin/env bash
# Explicit networked advisory/licence/source review; never part of check.sh.
set -euo pipefail
cd -- "$(dirname -- "$0")/.."
if [[ ${1:-} == --install && $# == 1 ]]; then
    mkdir -p target
    scratch=$(mktemp -d "$PWD/target/supply-chain.XXXXXXXX")
    trap 'rm -rf -- "$scratch"; rm -f target/debug/cargo-audit target/debug/cargo-deny' EXIT
    export CARGO_HOME="$scratch/home"
    mkdir -p "$CARGO_HOME"
    # Use only the worktree target, and never install into the user's Cargo home.
    cargo install --debug --locked --root "$scratch/tools" --target-dir "$PWD/target" cargo-audit@0.22.2
    cargo install --debug --locked --root "$scratch/tools" --target-dir "$PWD/target" cargo-deny@0.20.2
    export PATH="$scratch/tools/bin:$PATH"
elif [[ $# != 0 ]]; then
    echo 'usage: scripts/supply-chain.sh [--install]' >&2
    exit 2
fi
command -v cargo-audit >/dev/null || { echo 'cargo-audit missing; use --install' >&2; exit 2; }
command -v cargo-deny >/dev/null || { echo 'cargo-deny missing; use --install' >&2; exit 2; }
cargo audit --file Cargo.lock
cargo deny --locked check advisories licenses bans sources
