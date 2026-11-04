#!/usr/bin/env bash
# Opt-in dev measurement; spends real provider quota. NEVER part of check.sh.
# Host Command/cargo oracles are intentional here, not product tool execution.
# Run with the normal host toolchain, no build-env overrides. Raw evidence is
# memory-only and private scratch roots live under this checkout's target/.
# Missing DeepSeek credentials skip its cells and exit 77. No provider retries.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --locked --quiet -p harness --bin harness --example dogfood
exec target/debug/examples/dogfood "$@"
