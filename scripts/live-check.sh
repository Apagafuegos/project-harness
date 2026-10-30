#!/usr/bin/env bash
# Opt-in live check for a real provider. Never part of check.sh or CI.
#
#   scripts/live-check.sh openai [--auth codex|api-key]
#
# Default auth is `codex`, the user's ChatGPT subscription through the read-only
# Codex CLI login. `--auth api-key` uses OPENAI_API_KEY instead. The model comes
# from HARNESS_LIVE_OPENAI_MODEL, defaulting to the adapter's documented
# live-check model. This spends real quota, so it is only ever run on request.
set -euo pipefail

provider="${1:-}"
if [ "$provider" != "openai" ]; then
  echo "usage: scripts/live-check.sh openai [--auth codex|api-key]" >&2
  exit 2
fi
shift

auth="${HARNESS_LIVE_OPENAI_AUTH:-codex}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --auth)
      [ "$#" -ge 2 ] || { echo "--auth needs a value" >&2; exit 2; }
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
  codex|api-key) ;;
  *)
    echo "unknown auth: $auth (use codex or api-key)" >&2
    exit 2
    ;;
esac

cd "$(dirname "$0")/.."
exec cargo run --locked --quiet -p harness-providers --example live_openai -- "$auth"
