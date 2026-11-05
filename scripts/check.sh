#!/bin/sh
# Keep local validation and CI on the same path. Run from any working directory.
set -eu

cd "$(dirname "$0")/.."

# Provision the pinned toolchain and locked dependencies before validation.
export CARGO_NET_OFFLINE=true
# Debug level and incremental mode come from the workspace profile in
# Cargo.toml, shared with ad-hoc cargo commands so they reuse these artifacts.
./scripts/test-offline-gate.sh

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
# Debug-only real-binary PTY fixture, isolated from the other lock/spawn targets.
cargo clippy -p harness --all-targets --features test-provider --locked -- -D warnings
cargo test -p harness --features test-provider --test tui_pty --test m9_tui_pty --locked -- --nocapture
# Public adapter fixture contract must also build and run for downstream users.
cargo clippy -p harness-core --all-targets --features conformance --locked -- -D warnings
cargo test -p harness-core --features conformance --locked
RUSTDOCFLAGS="-D warnings" cargo doc -p harness-core --features conformance --no-deps --locked
if [ "${HARNESS_ALLOW_NO_SANDBOX:-}" = 1 ]; then
    echo "SKIPPED: shell security suite"
else
    cargo test --locked -p harness-core --test sandbox -- --ignored --test-threads=1
    cargo test --locked -p harness-core --test tool_host -- --ignored --test-threads=1
    cargo test --locked -p harness --test doctor -- --ignored --test-threads=1
    # M6-05: the offline evaluation pipeline, including the hidden check run in
    # the same Bubblewrap sandbox. It fails, never skips, without a backend.
    cargo test --locked -p harness-eval --test offline_gate -- --ignored --test-threads=1
fi
./scripts/test-security-gate.sh
cargo build --workspace --locked
# Compile outside the smoke bounds: cold builds can be slow under load.
cargo build --locked -q -p harness-core --examples
cargo build --locked -q -p harness-providers --example provider_session
cargo test --locked -q -p harness-core --example scripted_session --no-run
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
# Run binaries directly so timeout signals the smoke process, not cargo.
bin="${CARGO_TARGET_DIR:-target}/debug"
timeout 300 "$bin/harness" --help
timeout 300 "$bin/harness" --version

# Smoke-test `harness context` on the RFC-001 compaction fixture: a private
# data root built with plain tools must print the checked-in golden report.
fixtures=docs/rfcs/001/examples
session=019a0000-0000-7000-8000-000000000003
smoke=$(mktemp -d)
trap 'rm -rf "$smoke"' EXIT
mkdir -m 700 "$smoke/state" "$smoke/state/sessions" "$smoke/state/artifacts"
for artifact in "$fixtures"/artifacts/*; do
    install -m 600 "$artifact" "$smoke/state/artifacts/"
done
install -m 600 "$fixtures/compaction.jsonl" "$smoke/state/sessions/$session.jsonl"
timeout 300 "$bin/harness" context "$smoke/state" "$session" \
    --context-tokens 128000 >"$smoke/report.json"
diff -u crates/core/src/context/golden/compaction.json "$smoke/report.json"
echo "harness context smoke test: report matches the compaction golden file"

# The Milestone 2 end-to-end example: two turns, a crash during a third,
# automatic recovery and the final context report. It is offline and
# deterministic, so two runs must print the same bytes.
timeout 300 cargo test --locked -q -p harness-core --example scripted_session
timeout 300 "$bin/examples/scripted_session" >"$smoke/example-1.txt"
timeout 300 "$bin/examples/scripted_session" >"$smoke/example-2.txt"
diff -u "$smoke/example-1.txt" "$smoke/example-2.txt"
grep -q "streams opened by recovery: 0" "$smoke/example-1.txt"
echo "scripted_session example: deterministic, recovered without a stream"

# M3: real descriptor-rooted read/edit, approval, process death during write
# result append, explicit tail repair and user resolution. No sandbox backend.
timeout 300 "$bin/examples/tool_session" >"$smoke/tool-example-1.txt"
timeout 300 "$bin/examples/tool_session" >"$smoke/tool-example-2.txt"
diff -u "$smoke/tool-example-1.txt" "$smoke/tool-example-2.txt"
grep -q 'advisory hint matches_post_image' "$smoke/tool-example-1.txt"
grep -q 'ToolResult Some(UserObserved)' "$smoke/tool-example-1.txt"
echo "tool_session example: deterministic, uncertain write resolved by the user"

# M4: real native adapters behind the router, entirely on attributed loopback.
timeout 300 "$bin/examples/provider_session" >"$smoke/provider-example-1.txt"
timeout 300 "$bin/examples/provider_session" >"$smoke/provider-example-2.txt"
diff -u "$smoke/provider-example-1.txt" "$smoke/provider-example-2.txt"
grep -q 'switch Reject: refused' "$smoke/provider-example-1.txt"
grep -q 'switch OmitOptional: accepted' "$smoke/provider-example-1.txt"
grep -q 'Anthropic turn: completed' "$smoke/provider-example-1.txt"
echo "provider_session example: deterministic, native adapters and compaction"
