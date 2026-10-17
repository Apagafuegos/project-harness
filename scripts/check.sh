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
