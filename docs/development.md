# Development

## Environment

Linux is the initial development and validation target. This foundation is
checked in Linux CI; other operating systems are not yet supported targets.

Install Git, a C compiler/linker and standard development headers, and
[rustup](https://www.rust-lang.org/tools/install). On Debian/Ubuntu the system
prerequisites are `git`, `curl`, `ca-certificates`, and `build-essential`.

The repository pins Rust 1.99.1 with the minimal profile plus rustfmt and Clippy.
Inside the checkout, `rustup toolchain install` installs the toolchain from
`rust-toolchain.toml`, which also selects it for Cargo. If rustup was just
installed, open a new shell or source `"$HOME/.cargo/env"` so Cargo is on `PATH`.

There is no separate bootstrap script, container requirement, or mandatory editor.
An editor with rust-analyzer is optional; let its extension manage its language
server or install it separately through rustup.

## Commands

Run these from the repository root:

```sh
cargo build --workspace --locked
cargo run --locked -p harness -- --help
cargo run --locked -p harness -- --version
./scripts/check.sh
```

The check script verifies formatting, Clippy with warnings denied, workspace
tests and doctests, a development build, documentation with warnings denied,
and CLI help/version smoke checks. It can also be invoked from another working
directory. CI runs the same script with the repository toolchain.

The checks exercise J01–J03 storage behavior and the Rust API usage example,
including the specification journals and injected durability failures. They do
not prove provider compatibility, workspace tools or sandbox enforcement.

For narrower checks while working:

```sh
cargo fmt --all
cargo check --workspace --locked
cargo test -p harness-core --locked
cargo doc --workspace --no-deps --locked --open
```

## Dependency and toolchain changes

The workspace uses Rust 2024 and Cargo resolver 3. Shared package metadata,
dependencies, and lints live in the root manifest. Each crate opts into them.
First-party unsafe code is forbidden; this does not audit dependencies or create
an execution sandbox.

Clap supplies CLI help, version, and argument validation. Storage uses Serde and
Serde JSON, the bundled JSON Schema validator with remote resolution disabled,
SHA-256, UUIDv7, UTC timestamps, Rustix filesystem/locking primitives, typed
errors and imbl persistent collections for sharing session snapshots. Tempfile
is test-only. Tokio, Ratatui and HTTP clients are added when their implementations
need them. Enable only required features.

Commit `Cargo.lock`. Use `--locked` in routine builds and CI. To intentionally
change dependencies, edit the manifest or run a targeted `cargo update -p NAME`,
review the manifest and lockfile changes, and run the checks. When changing Rust,
update `rust-toolchain.toml` and the workspace `rust-version` together, and update
the version mentioned in these docs. CI reads the toolchain file directly.

Network access is needed to install Rust and fetch uncached dependencies. Once
fetched, checks must not contact model providers or other services. To verify
this locally, set `CARGO_NET_OFFLINE=true` when running the check script.

## Secrets and runtime data

The current library/CLI do not read credentials or `.env` files. No API keys are required.
When providers are implemented, document their configuration and opt-in live
checks. Keep credentials out of Git, fixtures, logs, session entries, context
snapshots, and tool subprocess environments. Environment inheritance must be
explicit when process execution is introduced.

The storage library can create a private state root, checksummed journals and
content-addressed artifacts when explicitly called. The CLI creates no runtime
storage. See [session storage](session-storage.md) for API usage and limits.
Retention/deletion UI remains future work.

## Specification artifacts

[RFC-001](rfcs/001-core-types-state-persistence.md) has a bundled JSON Schema,
checksummed example journals, and conformance cases. Storage tests validate all
three examples and their artifacts and cover J01–J03. Runtime, security, model
and context gates remain future work.

Bubblewrap and Linux namespace support will be prerequisites for external tool
execution, not for the current storage/CLI checks. Do not install or
claim a working sandbox merely because it is the chosen backend.

## References

- [Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html)
- [Cargo dependency resolver](https://doc.rust-lang.org/cargo/reference/resolver.html)
- [rustup profiles](https://rust-lang.github.io/rustup/concepts/profiles.html)
- [Tokio setup and feature selection](https://tokio.rs/tokio/tutorial/hello-tokio)
- [Ratatui installation](https://ratatui.rs/installation/)
