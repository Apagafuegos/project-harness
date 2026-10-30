#!/bin/sh
# Explicit opt-in stress check; not part of check.sh or ordinary CI.
set -eu
cd "$(dirname "$0")/.."
export HARNESS_TEST_LOOPBACK_PROBE=1 CARGO_NET_OFFLINE=true
runs=${1:-20}
logs=$(mktemp -d)
echo "loopback probe logs: $logs"
i=1
while [ "$i" -le "$runs" ]; do
    if ! cargo test --locked -p harness-providers >"$logs/providers-$i.log" 2>&1; then
        echo "FAIL providers iteration $i: $logs/providers-$i.log"
        exit 1
    fi
    if ! cargo test --locked -p harness-core --test tool_host -- --ignored --test-threads=1 >"$logs/tool-host-$i.log" 2>&1; then
        echo "FAIL tool_host iteration $i: $logs/tool-host-$i.log"
        exit 1
    fi
    echo "PASS probe iteration $i/$runs: providers and S02 tool_host"
    i=$((i + 1))
done
cargo test --locked -p harness-core --test sandbox -- --ignored --test-threads=1 >"$logs/sandbox.log" 2>&1 || {
    echo "FAIL sandbox: $logs/sandbox.log"
    exit 1
}
echo "PASS probe-enabled S02 sandbox"
