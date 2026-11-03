# Milestone 7 acceptance — v0.1 candidate

M7's pre-release features and documentation are implemented. **Tagging,
publication and verification of the final release assets remain pending the
orchestrator**, not completed by this step. Workspace version is **0.1.0**;
`publish = false`, toolchain, dependencies and Cargo.lock are unchanged in step 09.
The repository is public; decision 17 supersedes the plan's private-release
premise. Security audit accepted at
`5b89aa2f0f3b038b73993d4c1d137da494c6027f` is this step's baseline.

**Met** means implemented with cited source tests/recorded evidence; **partial**
means an explicit limitation, not a pass inferred from fixtures. No new runtime,
product behavior or journal format is introduced by documentation reconciliation.
The only tooling changes make the release guide mandatory in packaging and
assert its presence for each target in the release workflow. Final exact-SHA CI
results belong in the step-09 handoff; historical runs below do not substitute
for them. No paid/live request, tag or release was made in step 09.

## Plan goals and implementation evidence

The approved plan called for config, nested instructions, export/deletion,
highlighting, ConvertSupported, better TUI UX, reachable live evidence, an audit,
and reconciled contracts/docs before the orchestrator tags v0.1.0.

| Plan step / goal | Implementation and gate evidence | Status |
| --- | --- | --- |
| 01: close reachable live-evidence partials | Sanitized M7-01 logs in [M5 acceptance](milestone-5-acceptance.md#evidence-log--m7-01-2026-11-03) and [M6 acceptance](milestone-6-acceptance.md#evidence-log--m7-01-2026-11-03); `crates/cli/examples/tui_smoke.rs`, `scripts/live-check.sh`; details below | Met for Codex/DeepSeek answer/restoration and DeepSeek scripted checks; Anthropic/cross-provider live partial |
| 02: explicit ConvertSupported | [ADR 0015 amendment](decisions/0015-switching-model-metadata.md); `crates/core/src/context/{builder,projection,convert}.rs`, `runtime/core.rs`; `convert_supported_policy_matrix_is_projection_only`, `convert_supported_preserves_multiple_tool_pairs_and_original_parts` in `context/tests.rs`; `convert_supported_switch_back_restores_original_media`, `convert_supported_opaque_state_is_omitted_never_textualized` in `runtime/driver/model_tests.rs`; `tests/ask_cli.rs::convert_supported_flag_is_accepted_before_authentication`; production-loop `tui_navigation.rs::required_incompatibility_is_not_omittable` (renders `c convert`, then asserts the converted metadata reference and no raw image item in the next real recorded request) | Met, projection only |
| 03: user TOML / nested instructions | [ADR 0028](decisions/0028-user-config.md), [ADR 0022 amendment](decisions/0022-recorded-model-instructions.md); `crates/cli/src/{config,config_tests,wiring,wiring_instructions_tests}.rs`, `tests/config.rs`, `tests/ask.rs`; `precedence_every_applicable_key_both_clients_and_ask_only_scope`, `user_root_nested_order_scope_and_symlinks_and_exclusions`, `total_cap_skips_whole_files_and_continues_to_smaller_files` | Met |
| 04: export / delete / GC | [ADR 0030](decisions/0030-session-export-deletion-and-gc.md); `crates/core/src/session/{catalog,maintenance,artifacts}.rs`, `crates/cli/src/{sessions_cli,export}.rs`, `tests/session_maintenance.rs`, `tests/tui_maintenance.rs`; artifact GC lock and copied-byte tests in `crates/core/src/session/tests/artifact_gc.rs` | Met; deletion journal-only |
| 05: TUI UX foundations | [ADR 0023 Part 6](decisions/0023-tui-architecture.md#part-6-ux-foundations); pure `tui/{theme,keys,render,mod}.rs`; TestBackend and loop tests in `tests/tui.rs`, `tui_runtime.rs`; `word_wrap_keeps_words_and_hanging_indent`, `no_color_covers_transcript_prompt_errors_status_and_all_dialog_frames`, `prompt_history_seeds_current_branch_restores_draft_and_keeps_sent_prompts`, injected-clock activity and help-table checks | Met |
| 06: Markdown / highlighting / tools / approvals | [ADR 0023 Part 7](decisions/0023-tui-architecture.md#part-7-content-rendering-m7-06); `tui/{markdown,highlight,tools,transcript}.rs`; `kitchen_sink_markdown_renders_the_same_projection_at_80_and_120_columns`, `no_color_has_no_colour_and_ascii_mode_has_no_wide_glyphs`, `completed_entries_render_once_and_only_the_partial_is_reparsed`, user-text preservation, fence-cap and approval-preview tests in `tests/tui.rs` | Met |
| 07: licence / binary release pipeline | [ADR 0029](decisions/0029-release-pipeline.md), [release evidence](development.md#accepted-dry-run-evidence); workspace metadata, LICENSE-MIT/APACHE, `.github/workflows/release.yml`, `scripts/{release-version,package-release,test-release,third-party-notices}.sh` | Met for tooling and historical two-target dry run; publication pending |
| 08: security audit after features | [Accepted report](security/v0.1-audit.md), [classified history inventory](security/history-hits.tsv), `tests/security_audit.rs`, provider error-code/probe-env/file/maintenance regressions, `scripts/test-history-scan.sh` | Met within stated threat model, not absence-of-vulnerabilities proof |
| 09: reconcile / guide / acceptance / changelog | All six [contracts](README.md), [user guide](user-guide.md), README/index/roadmap and relevant architecture/context/storage/release notes; CHANGELOG.md; unconditional required-guide manifest/cmp and isolated refusal fixture in `scripts/test-release.sh` | Met for changes and focused local checks; exact-head full CI required in handoff |
| Orchestrator: tag v0.1.0 / verify public assets | Final candidate dispatch/tag/version/static smoke/archive/checksum/publication verification | Pending, deliberately not performed |

### Concrete semantics covered

- **Config:** only user TOML at XDG/HOME or `--config`; `--no-config` bypasses
  it. Explicit flag > config > built-in default. Strict keys/types, 64 KiB cap,
  missing-default tolerance, explicit-missing refusal and content-free credential
  key refusal. `approve_read_only` and startup `switch_policy` are ask-only;
  TUI approvals still require explicit keys. Config is not a new journal record.
- **Instructions:** base system, user developer, root developer, sorted nested
  developer text. Descriptor-rooted project reads skip symlinks, dot and known
  build/vendor directories; depths 0–8, 4,000 directories including root,
  64 KiB/file and 256 KiB aggregate content. Whole-file skips warn. User-owned
  instructions use std fs and can follow symlinks. Creation-only recording;
  continuation never rereads instructions. Text scope is not execution policy.
- **Conversion:** selected explicitly; unsupported user media becomes stored
  metadata references, calls/results become bounded text only for tool-less
  targets. Converted parts appear in preview/context reports, estimates and
  fingerprints. Optional opaque state follows omission/owner/prefix rules,
  never translation. Capable switch-back restores original blocks. Existing
  `ModelChanged.portability` persists the choice without schema/history changes.
- **Maintenance:** shared read locks and full validation for exports, refusal to
  overwrite, actual copied hash/length checks. Exclusive journal-only deletion.
  GC refuses the whole run on any unreadable/locked/invalid journal; retains
  referenced artifacts and those within 24 hours. Dedupe refreshes mtime.
  Independent open-description artifact-directory flocks serialize publication/
  reuse with selection/unlink. Dry-run lists bytes without unlinking.
- **TUI:** pure model/typed Action boundary remains guarded by
  `tests/tui_boundary.rs`; terminal restoration by `tests/tui_pty.rs`, uncertain
  effect recovery by `tests/tui_recovery.rs`. 20×5 minimum, ANSI palette,
  NO_COLOR, ASCII decorations, no mouse capture, bracketed paste, history/help,
  word/grapheme wrapping/editor, active-only injected activity and scrollable
  dialogs. GFM, inert HTML, code-only bundled highlighting capped at 2,000 lines/
  256 KiB, compact/expandable tool bodies and scrollable approval previews.
- **Context:** original append-only trees remain authority. Whole-turn controls,
  all three strategies, preview binding, explicit budget failures and sequential
  tools retain [M6's coverage](milestone-6-acceptance.md). New sessions explicitly
  record observation_masking; omitted legacy strategy is rolling_summary.
  Continuations retain recorded settings; differing explicit strategy is refused.
  The idle typed SetCompactionStrategy command is not a CLI/TUI shortcut.

## Settled decisions 1–17

| Decision | Resolution / evidence | Status |
| --- | --- | --- |
| 1: MIT OR Apache-2.0 | Both root licence files and inherited Cargo metadata; notices/licence review in audit. Effective now the repository is public | Met |
| 2: prebuilt x86_64/aarch64 musl, not crates.io | Native release matrix and dry runs below; `publish = false`. Public distribution supersedes original private premise | Tooling met; tag/publication pending |
| 3: all requested gaps in v0.1 | Plan steps 02/03/04/06 above implement config, nested instructions, export/delete, highlighting and conversion | Met |
| 4: TUI UX in scope | ADR 0023 Parts 6–7, pure-model/loop/render/PTY evidence above | Met |
| 5: limited disk | Step 09 uses only this worktree's target, one serial targeted debug build, no local full check.sh or extra target roots; shell fixture traps clean temporary directories | Met |
| 6: no Anthropic key | No new live attempt; fixture evidence explicitly separate | Partial: live Anthropic and live cross-provider switching |
| 7: user TOML | ADR 0028 config/process tests and source labels; existing persisted fields only, no secrets or project policy in TOML | Met |
| 8: bounded recorded instructions | ADR 0022 amendment, instruction ordering/cap/walk/symlink tests and first-request/continuation checks | Met |
| 9: export/delete and TUI keys | ADR 0030 maintenance/UI tests; decision 16 explicitly supersedes artifact deletion portion | Met as amended |
| 10: pure-Rust bounded highlighting | Syntect bundled grammars + fancy-regex, ANSI roles, NO_COLOR/plain fallback, cap/cache tests; ADR 0023 records 18,745,256 → 19,705,032 bytes (+959,776 / +5.1%) in the measured release profile | Met; historical measurement, not final musl size |
| 11: explicit projection-only conversion | Context/driver policy matrix, tool pairing, native-state, fingerprint and restored-original tests; no storage schema change | Met |
| 12: TUI UX / boundary | Parts 6–7, generated key allowlist/help, no mouse capture, paste, editor/modal/activity/render tests; boundary guard and PTY suite retained | Met |
| 13: release workflow / guide | `v*` trigger, nonpublishing dispatch, native musl builds/smoke, sorted checksums, mandatory guide/licences/README/changelog/notices in archives | Tooling met; final publication pending |
| 14: config applicability | Ask-only approve_read_only/startup switch_policy in help/show/tests; TUI dialog/explicit actions unchanged | Met |
| 15: one-build budget / registration | M7-07's single local ARM musl build: 266s, 10,427,912 bytes, stripped static ELF; version/help/doctor-help passed. Before-profile/ARM GNU baselines unavailable. Registration followed by two successful two-target dispatches | Met; no invented comparison |
| 16: journal-only delete / explicit GC | `artifact_gc_serializes_selection_and_unlink_with_dedupe_reuse`, maintenance tests, mtime refresh and exclusive selection/unlink lock; UI says GC reclaims bytes | Met |
| 17: public repository | `d80cb9a` policy/SECURITY/Dependabot record; full reachable-history audit and exact-head required CI, SHA-pinned Actions, restricted tag publishing | Met within accepted audit/policy scope; no settings changed here |

## Offline CI and recorded live evidence

### Accepted baseline and final-head requirement

Accepted audit SHA: `5b89aa2f0f3b038b73993d4c1d137da494c6027f`.
[CI 41716846672](https://github.com/Apagafuegos/project-harness/actions/runs/41716846672)
ran on that exact SHA: **Rust workspace — success; Host sandbox security (S02) —
success**. Step 09 must also pass both jobs on its exact final pushed SHA;
its handoff supplies that run link and all job results, avoiding a circular
attempt to embed a commit's own hash in its contents.

The current `.github/workflows/ci.yml` provisions pinned Rust/apt/locked
sources separately. Validation runs Cargo offline inside a loopback-only network
namespace, without live credentials or the sandbox opt-out. Rust workspace:

- Generates dependency notices, tests the history scanner and release tooling.
- Runs the full `scripts/check.sh`: fmt, warning-denying workspace Clippy/tests,
  test-provider PTY, core conformance feature + docs, required sandbox/tool_host/
  doctor/offline-evaluation ignored targets, offline/security control-flow guards,
  workspace/example/docs builds, help/version/context golden smoke and twice-run
  deterministic scripted/tool/provider examples.

The independent **Host sandbox security (S02)** job records doctor before/after
loading the committed runner-only AppArmor profile and runs `sandbox`, `tool_host`,
`doctor` and `offline_gate` ignored suites serially under the same network/FD
wrappers. Unavailable isolation is failure, never a successful skip.
This is full offline validation **after provisioning**, not a claim that Rust,
apt, checkout or dependency downloads are offline.

Historical M7-01 full gate (recorded in M6): 44 test-result groups, **1,257
passing executions, zero failures**, exit 0 within 3,900 seconds. That count is
not presented as the current final test count and does not double-count into a
new distinct-test claim.

### Two-target release dry runs (M7-07)

[41625756606](https://github.com/Apagafuegos/project-harness/actions/runs/41625756606)
at `9223c52c0ea1fd98d26f006009fb32b7d78e0adc`, then
[41628110546](https://github.com/Apagafuegos/project-harness/actions/runs/41628110546)
at `305305b35601f41200c93dc2be3dfd8a43b20981` both succeeded after workflow
registration. The latter's results, rechecked through Actions metadata:

| Job | Result |
| --- | --- |
| version | success |
| verify / Rust workspace | success |
| verify / Host sandbox security (S02) | success |
| build (aarch64-unknown-linux-musl, ubuntu-24.04-arm) | success |
| build (x86_64-unknown-linux-musl, ubuntu-24.04) | success |
| package | success |
| publish | skipped (correct for workflow_dispatch) |

Both native binaries passed static linking and version/help/doctor-help checks;
each archive was packaged twice and byte-compared; package aggregation verified
both targets and generated sorted SHA256SUMS. See [release evidence](development.md#accepted-dry-run-evidence).
These historical runs predate audit notices and the step-09 guide, so do not prove
final-asset contents or publication. No extra release build/dispatch ran here.
Archive reproducibility is **not** independently reproducible binary builds.

### M7-01 sanitized live checks — retained, not rerun

At source `8724ff6`, 2026-11-03, each paid check ran once without retry:

- Codex `gpt-6.1-sol` and DeepSeek `deepseek-flash` TUI smokes: exit 0,
  completed pong answer, identical termios before/after and required terminal
  enter/paste/leave/cursor restoration sequences. These prove live answer/quit/
  restoration, not a live coding/tool workflow. M5's earlier quota failure remains
  recorded as historical evidence, not erased or counted as a pass.
- `scripts/live-check.sh deepseek`: exit 0, streamed tool turn, tool round trip,
  usage **input 172 / output 45**, thinking replay **2 blocks** and completed
  continuation, summary pass. The initial missing-key skip (exit 77, no request)
  is retained separately from the supplied-key pass.
- Anthropic/live OpenAI→Anthropic→OpenAI checks were not attempted: **partial**,
  no Anthropic key supplied. Native fixture conformance is complete but cannot
  establish live compatibility.

Only sanitized summaries in the linked M5/M6 logs are evidence. No raw transcript,
credential, account identifier or copied login file is added. The two DeepSeek
strategy matrices remain a bounded experiment (four prose-heavy tasks, one
repetition, lexical matchers, one probe turn); they do not rank general quality.
Masking is lossy, need not shorten context, and cannot roll back files.

## Security audit disposition and residual risks

The [accepted audit](security/v0.1-audit.md) is authoritative for detail. All four
medium findings are fixed: A01 clears the TasksMax probe environment; A02
allowlists provider code/type strings; A03 packages dependency notices and
versioned corresponding-source links; A04 hashes actual copied export bytes.
Regression evidence includes `scope_probe_environment_is_an_allowlist`, provider
unknown-code tests, `crates/cli/tests/security_audit.rs` and
`journal_export_checks_the_copied_bytes_not_only_the_verified_source`.
L02 overlong paths is fixed; L01 noncredential identities, L03 bincode maintenance,
L04 host/resource limits, L05 prompt injection and L06 remembered continuation
remain documented, not silently removed.

The historical scan reviewed **605 commits / 1,790 locations**: 34 noncredential
true positives, 190 sanitized fixtures, 1,566 false positives and **zero identified
credentials requiring rotation**. This was heuristic discovery plus manual
classification, not exhaustive proof across possible encodings or future commits.
Dependency review at DB `965bf9ad083b58e6340d65829d85217b18be9290` reported zero
known vulnerabilities; bincode 1.3.3 has informational unmaintained advisory
RUSTSEC-2025-0141, no patched version, and a reasoned narrow exception. Seven
transitive duplicate families remain warnings. Reviewed imbl MPL components are
unmodified with full notices/source links; no broad copyleft exemption exists.

Retained limits (none is weakened by documentation):

- No seccomp; TasksMax needs a usable systemd user manager. Uninterruptible kernel
  I/O can delay cleanup indefinitely; early success would falsely claim cleanup.
- Shell mounts protect names, not every hard-link alias or hostile same-user host
  race. Root renames can diverge pinned file and path-bound shell views. Kernel
  compromise and hostile same-user host processes are outside the threat model.
- Absent protected paths cannot be masked; post-execution detection is not
  rollback and can miss transient creation. Initially absent `.git` is writable
  during its creation command; an existing `.git` is read-only.
- Probe/doctor diagnostics and session headers can contain user-selected host
  paths. User-owned instruction symlinks can be followed. Read authority is not
  content redaction; prompt injection influences proposals, not trusted policy.
- Private artifacts are not encrypted; ordinary user/model/tool text is verbatim,
  not a secret scrubber. Inspect exports/diagnostics before publishing them.
- Remembered grants can cause `session_policy_mismatch` on client continuation;
  inspection/export remains available. Failed journal export can leave an empty
  newly created directory. Neither justifies weakening checks.
- Fork/exec can briefly inherit another thread's journal lock until CLOEXEC;
  transient `session_locked` is a refusal, never authorization to replay effects.
- No signed provenance/attestations or independent reproducible-binary proof.
  Security review does not establish absence of vulnerabilities.

## Step-09 focused verification and handoff boundary

Local, serial and offline: one `cargo build --locked --offline -p harness --bin
harness`; the built binary's version and 14 help entry points; `config show`
default/flag/config source labels plus credential-key, `--no-config` and
explicit-missing-file refusals; `list`/`export`/`delete`/`gc --dry-run`/`context`
forms against a throwaway empty data root (`session_not_found`,
`[]`, `would remove 0 artifacts`); `third-party-notices.sh` before
`test-release.sh`; synthetic `test-history-scan.sh`, `test-offline-gate.sh` and
`test-security-gate.sh`; `cargo fmt --all -- --check` (no Rust changes).
No local full check.sh, release build or live endpoint request.

Release regression packages twice under different umasks, compares archive bytes,
checks notice/MPL contents and exact guide bytes, then runs a copied packaging
script in a controlled fixture with missing and empty guide. Both refuse with
exit 2 and `required release document missing or empty: docs/user-guide.md`,
without creating output. The generated archive contains
`harness-0.1.0-<target>/docs/user-guide.md` byte-identical to the source, and the
release workflow additionally asserts that member for each target. The real
guide is never removed; fixture traps clean all temporary children. Wrong tag
and invalid epoch refusals remain covered.

Changed local Markdown links/anchors and guide help syntax are checked before
handoff. Source tests are behavioral, not tests mirroring documentation prose.
The handoff reports exact final SHA, CI run and both required job results.
Remaining release actions: orchestrator review/integration, final-candidate
workflow verification, protected tag, and verification of public assets/checksums.
No release date, published archive or final binary reproducibility is claimed.
