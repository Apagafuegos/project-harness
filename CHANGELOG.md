# Changelog

Notable changes follow [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and [Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

Release candidate; tag, publication and public binary assets remain pending.

### Added

- Linux-first Rust harness: a headless library, Tokio driver and Ratatui terminal
  client, plus scriptable `ask`, context/journal inspection and host `doctor`.
- Checksummed append-only JSONL session trees, durable head navigation, private
  content-addressed artifacts, writer locks and explicit torn-tail repair.
- Native OpenAI Responses (API key or existing read-only Codex login), Anthropic
  Messages and DeepSeek's Messages-compatible profile; bounded HTTP/SSE streams,
  nullable usage, owner-scoped native replay and fixture conformance suites.
- Sequential `read`, `write`, `edit` and sandboxed `bash`, exact-plan approvals,
  durable execution intent/results and explicit uncertain-effect recovery.
- Read-only context reports, whole-turn pins/exclusions, preview-first and
  automatic compaction, rolling summaries, model-free observation masking and
  structured state with an independent summary model.
- Strict user TOML defaults and bounded recorded user/root/nested AGENTS.md
  instructions; no workspace configuration or credentials in TOML.
- Session catalog, Markdown/all-branches and verified journal exports,
  journal-only deletion and explicit delayed artifact garbage collection.
- TUI help, history, word/grapheme editing and wrapping, atomic bracketed paste,
  follow mode, activity/status, scrollable modals, GFM Markdown, bounded bundled
  syntax highlighting, compact tool blocks and approval diffs/previews.
- MIT OR Apache-2.0 licensing, third-party licence/source notices, two-target
  Linux musl release workflow, checksums and reproducible archive packaging with
  a mandatory user guide. Dispatch dry runs never publish.
- Offline evaluation and security gates, sanitized opt-in live evidence,
  user guide and milestone acceptance/audit documentation.

### Changed

- New sessions explicitly record `observation_masking`; old omitted strategies
  remain `rolling_summary` and continuation retains recorded policy.
- Explicit `ConvertSupported` switching converts incompatible displayable media
  and tools only in model context, with per-part reports and converted-input
  fingerprints; it never translates opaque state or rewrites history.
- Context controls and model-free compactions use record version 3; versions 1
  and 2 remain readable and continue as v3 without migration.
- Release builds use stripped thin-LTO musl binaries with unwinding retained for
  terminal restoration; `NO_COLOR` and ASCII terminal decorations are supported.

### Fixed

- Coherent TUI reset replaces stale inspector identity/report across subscriber
  lag and runtime replacement; late replies cannot repopulate old panels.
- Summary requests end with an explicit fresh user request rather than accidental
  assistant prefill on Messages endpoints.
- Artifact collection selection/unlink is serialized with publication/dedupe
  reuse; dedupe refreshes mtime and GC retains the 24-hour grace window.
- Journal export checks copied artifact hashes, not only previously verified
  sources and lengths; existing output paths are never overwritten.
- Overlong trusted-file paths are bounded refusals rather than infrastructure
  failures. Ordinary test authentication failures cannot call a live endpoint.

### Security

- Descriptor-rooted openat2 file containment, preimage revalidation and atomic
  replacement; Bubblewrap for every model-spawned executable, network denied,
  explicit mounts, environment allowlists and supervised descendant cleanup.
  Missing enforcement always fails closed, without a host fallback.
- Fixed the TasksMax capability probe's inherited environment and arbitrary
  provider error-code/type strings leaking into diagnostics or durable state.
- Accepted pre-release audit reviewed public reachable history, credentials,
  policy, filesystem/sandbox boundaries, session maintenance and release supply
  chain; all four medium findings were fixed with behavioral regressions.
- Dependency audit found zero known vulnerabilities at its recorded database
  revision; bincode's unmaintained advisory remains a documented low maintenance
  risk. This is not a guarantee of no vulnerabilities.
- Residual limits include no seccomp, host-dependent TasksMax, shell hard-link/
  host-race and absent-path limits, unencrypted private artifacts, remembered-
  grant continuation refusal and no signed provenance/reproducible binary proof.
  See the full security audit in the public repository.
