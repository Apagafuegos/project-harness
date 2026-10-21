#!/bin/sh
# Keep local validation and CI on the same path. Run from any working directory.
set -eu

cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
cargo run --locked -p harness -- --help
cargo run --locked -p harness -- --version

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
cargo run --locked -q -p harness -- context "$smoke/state" "$session" \
    --context-tokens 128000 > "$smoke/report.json"
diff -u crates/core/src/context/golden/compaction.json "$smoke/report.json"
echo "harness context smoke test: report matches the compaction golden file"
