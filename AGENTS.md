# Working on project-harness

## Project direction

- Rust only. Use Cargo; do not add a Go or JavaScript toolchain.
- Keep `harness-core` independent of the CLI, Ratatui, terminal input, and rendering.
- Tokio is the selected asynchronous runtime and Ratatui is the selected TUI.
  Add their dependencies when implementing code that uses them.
- Start with in-process typed commands and events. A separate engine process,
  JSON-RPC transport, or remote client needs a concrete use case and a decision record.
- Session history is append-only; model context is a computed projection.
- v0.1 sessions use trees in one JSONL journal; tool execution is sequential.
- Trusted Rust filesystem tools require workspace authorization. Every external
  executable uses Bubblewrap, with no unrestricted fallback and network denied
  by default. Opt-in user toolchains mount read-only with a fixed environment;
  a mount is never writable. See `docs/contracts/execution-security.md`.
- The current contract documents are drafts. When implementing them, resolve and
  record relevant open decisions and add behavioral tests alongside the code.
- The six high-level contracts are provisionally agreed. Review RFC-001 before
  implementing its proposed concrete types, persistence, or recovery details.
- Add crates when stable boundaries justify them, rather than creating one for
  every planned subsystem.

## Workflow

- Read `docs/README.md` and the relevant contract before making architectural changes.
- Use the pinned toolchain and keep `Cargo.lock` in version control.
- Run `./scripts/check.sh` before handing off Rust changes.
- Tests must work without network access or API keys once dependencies are fetched.
  Live-provider checks must be explicit, opt-in, and separate from ordinary CI.
- Update documentation when behavior or scope changes. Describe implemented
  behavior separately from planned behavior.
- Do not log secrets or silently retry uncertain side effects. Authorization
  policy and OS sandbox enforcement are separate responsibilities.

## Cloudflare

When interacting with Cloudflare, use the `cf` CLI unless the project has a
Wrangler configuration file.
