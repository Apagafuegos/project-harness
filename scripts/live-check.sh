#!/usr/bin/env bash
# Opt-in live check for a real provider. Never part of check.sh or CI.
#
#   scripts/live-check.sh openai [--auth codex|api-key]
#   scripts/live-check.sh anthropic
#   scripts/live-check.sh deepseek
#   scripts/live-check.sh ask
#   scripts/live-check.sh switch [--auth codex|api-key]
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
# DeepSeek uses DEEPSEEK_API_KEY on its Anthropic-compatible endpoint (M6
# decision 10) and follows the same skip rule: without the variable it exits 77.
# The model comes from HARNESS_LIVE_DEEPSEEK_MODEL, defaulting to deepseek-flash.
# It runs one streamed turn with a tool round trip and one follow-up turn that
# replays the stored thinking block.
#
# `switch` proves mid-session switching in one durable session. With
# ANTHROPIC_API_KEY it runs OpenAI -> Anthropic -> OpenAI with a tool round trip;
# without it (the case on this machine) it runs the same-provider variant,
# openai::LIVE_CHECK_MODEL -> another model the account serves -> back, and
# reports the cross-provider part as skipped. Auth defaults to `codex`; override
# with HARNESS_LIVE_SWITCH_AUTH=api-key. It emits only pass/fail lines, model
# names and stable turn codes.
#
# This spends real quota, so it is only ever run on request.
set -euo pipefail

provider="${1:-}"
if [ "$provider" = "tui" ]; then
  [ "$#" -eq 1 ] || { echo "usage: scripts/live-check.sh tui" >&2; exit 2; }
  cd "$(dirname "$0")/.."
  cargo build --locked --quiet -p harness --bin harness
  bin="${CARGO_TARGET_DIR:-target}/debug/harness"
  case "$bin" in /*) ;; *) bin="$(pwd)/$bin" ;; esac
  exec cargo run --locked --quiet -p harness --example tui_smoke -- "$bin"
fi
if [ "$provider" = "switch" ]; then
  shift
  auth="${HARNESS_LIVE_SWITCH_AUTH:-codex}"
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
  cd "$(dirname "$0")/.."
  exec cargo run --locked --quiet -p harness-providers --example live_switch -- "$auth"
fi
if [ "$provider" = "ask" ]; then
  [ "$#" -eq 1 ] || {
    echo "usage: scripts/live-check.sh ask" >&2
    exit 2
  }
  cd "$(dirname "$0")/.."
  scratch="$(mktemp -d)"
  trap 'rm -rf "$scratch"' EXIT
  mkdir "$scratch/workspace"
  cargo build --locked --quiet -p harness
  set +e
  "${CARGO_TARGET_DIR:-target}/debug/harness" ask --workspace "$scratch/workspace" --data-root "$scratch/state" \
    --auth codex "Reply with the single word: pong" >"$scratch/stdout" 2>"$scratch/stderr"
  code=$?
  set -e
  # Never echo raw diagnostics, session IDs, credentials or artifacts. The only
  # failure evidence is the client's stable turn code, which is not free text.
  echo "live ask: auth=codex"
  echo "exit: $code"
  if [ "$code" -eq 0 ] && grep -q 'pong' "$scratch/stdout"; then
    echo "stdout contains pong: pass"
    echo "summary: pass"
  else
    stable="$(grep -m1 '^turn: ' "$scratch/stderr" || true)"
    [ -z "$stable" ] || echo "stable code: ${stable#turn: }"
    echo "stdout contains pong: fail"
    echo "summary: fail"
    exit 1
  fi
  exit 0
fi
if [ "$provider" != "openai" ] && [ "$provider" != "anthropic" ] && [ "$provider" != "deepseek" ]; then
  echo "usage: scripts/live-check.sh openai [--auth codex|api-key]" >&2
  echo "       scripts/live-check.sh anthropic" >&2
  echo "       scripts/live-check.sh deepseek" >&2
  echo "       scripts/live-check.sh ask" >&2
  echo "       scripts/live-check.sh tui" >&2
  echo "       scripts/live-check.sh switch [--auth codex|api-key]" >&2
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

if [ "$provider" = "deepseek" ]; then
  if [ "$#" -gt 0 ]; then
    echo "deepseek takes no arguments" >&2
    exit 2
  fi
  if [ -z "${DEEPSEEK_API_KEY:-}" ]; then
    echo "live deepseek: skipped (no DEEPSEEK_API_KEY)"
    exit 77
  fi
  exec cargo run --locked --quiet -p harness-providers --example live_deepseek
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
