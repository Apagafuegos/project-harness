#!/bin/sh
# Regression guard for the offline validation boundary (after provisioning).
set -eu
cd "$(dirname "$0")/.."
# Explicit live entry points and common download commands cannot join either
# gate. Bootstrap downloads in CI are reviewed separately below.
if grep -En 'live[-_](check|openai|anthropic|switch)|\b(curl|wget)\b|https?://' scripts/check.sh; then
    echo 'offline gate: network/live entry point in check script' >&2
    exit 1
fi
if grep -En 'live[-_](check|openai|anthropic|switch)|\b(curl|wget)\b' .github/workflows/*.yml; then
    echo 'offline gate: live entry point in CI' >&2
    exit 1
fi
grep -q '^export CARGO_NET_OFFLINE=true$' scripts/check.sh
grep -q '^  CARGO_NET_OFFLINE: "true"$' .github/workflows/ci.yml
# Every CI job must explicitly fetch the locked dependencies during bootstrap;
# all later Cargo calls inherit offline mode.
[ "$(grep -c 'CARGO_NET_OFFLINE=false cargo fetch --locked' .github/workflows/ci.yml)" -eq 2 ]
echo 'offline gate: validation offline; live entry points excluded'
