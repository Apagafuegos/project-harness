#!/usr/bin/env bash
# Opt-in dev measurement; spends real provider quota. NEVER part of check.sh.
# Host Command/cargo oracles are intentional here, not product tool execution.
# Run with the normal host toolchain, no build-env overrides. Raw evidence is
# memory-only and private scratch roots live under this checkout's target/.
# Missing DeepSeek credentials skip its cells and exit 77. No provider retries.
#
# Usage (every run spends provider quota; one run per cell, no retries):
#   scripts/dogfood.sh --provider all --task all --out DIR
#   scripts/dogfood.sh --provider openai --task T3 --out DIR \
#     --config /absolute/private/defaults.toml
#
# Without --config the driver sends `--no-config`, reproducing the m8-01
# method. With `--config FILE` it sends `harness --config FILE ask …`, so a run
# can measure a user's own TOML (`approve_read_only`, `[sandbox]`). The file is
# the user's: never commit it, and no value from it belongs in evidence except
# key names and sanitized mount counts.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --locked --quiet -p harness --bin harness --example dogfood
exec target/debug/examples/dogfood "$@"
