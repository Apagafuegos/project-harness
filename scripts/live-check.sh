#!/usr/bin/env bash
# Opt-in live check for a real provider. Never part of check.sh or CI.
#
#   scripts/live-check.sh openai [--auth codex|api-key]
#   scripts/live-check.sh anthropic
#
# For OpenAI, default auth is `codex`, the user's ChatGPT subscription through
# the read-only Codex CLI login. `--auth api-key` uses OPENAI_API_KEY instead.
# The model comes from HARNESS_LIVE_OPENAI_MODEL, defaulting to the adapter's
# documented live-check model.
#
# Anthropic uses ANTHROPIC_API_KEY only (M4 decision 13: no subscription
# login). Without that variable the check is skipped and exits 77, so a caller
# can tell "skipped" from "passed" (0) and "failed" (1). The model comes from
# HARNESS_LIVE_ANTHROPIC_MODEL, defaulting to claude-haiku-5-5.
#
# This spends real quota, so it is only ever run on request.
set -euo pipefail

provider="${1:-}"
if [ "$provider" != "openai" ] && [ "$provider" != "anthropic" ]; then
  echo "usage: scripts/live-check.sh openai [--auth codex|api-key]" >&2
  echo "       scripts/live-check.sh anthropic" >&2
  exit 2
fi
shift

cd "$(dirname "$0")/.."

if [ "$provider" = "anthropic" ]; then
  if [ "$#" -gt 0 ]; then
    echo "anthropic takes no arguments" >&2
    exit 2
  fi
  if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
    echo "live anthropic: skipped (no ANTHROPIC_API_KEY)"
    exit 77
  fi
  exec cargo run --locked --quiet -p harness-providers --example live_anthropic
fi

auth="${HARNESS_LIVE_OPENAI_AUTH:-codex}"
while [ "$#" -gt 0 ]; do
  case "$1" in
  --auth)
    [ "$#" -ge 2 ] || {
      echo "--auth needs a value" >&2
      exit 2
    }
    auth="$2"
    shift 2
    ;;
  *)
    echo "unknown argument: $1" >&2
    exit 2
    ;;
  esac
done

case "$auth" in
codex | api-key) ;;
*)
  echo "unknown auth: $auth (use codex or api-key)" >&2
  exit 2
  ;;
esac

# Set HARNESS_LIVE_OPENAI_RECORDING to a new path to retain the first completed
# stream as a sanitized fixture (never raw IDs, prose or encrypted payloads).
exec cargo run --locked --quiet -p harness-providers --features live-recording --example live_openai -- "$auth"
