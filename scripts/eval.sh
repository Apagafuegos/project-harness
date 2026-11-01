#!/bin/sh
# Run the evaluation harness. Live mode spends money, is opt-in and is never in
# CI: it needs DEEPSEEK_API_KEY in the environment and refuses to start without
# --yes after printing a coarse estimate.
set -eu
cd "$(dirname "$0")/.."

if [ "${1:-}" = "--live" ] && [ -z "${DEEPSEEK_API_KEY:-}" ]; then
    echo "scripts/eval.sh: DEEPSEEK_API_KEY is not set" >&2
    exit 2
fi

exec cargo run --locked -q -p harness-eval --bin harness-eval -- "$@"
