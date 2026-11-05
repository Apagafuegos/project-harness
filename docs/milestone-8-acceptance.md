# Milestone 8 acceptance — daily-driver agent, v0.2.0 readiness

Milestone 8 made harness usable for daily coding work: the agent can search the
workspace without a process, take back its own trusted file edits, and run a
project's tests inside the sandbox, and the approval burden the first live
measurement exposed is reduced without weakening a check. All M8 plan steps are
implemented except m8-04, which is **deferred** by decision 11 and again by the
rerun. Workspace version is **0.2.0**; `publish = false`, the toolchain and the
dependency set are unchanged in this step.

The orchestrator has since tagged and published v0.2.0 (see [Release
record](#release-record) below); `CHANGELOG.md`'s `[0.2.0]` section is dated
2026-11-05.

**Met** means implemented with cited source tests or recorded evidence.
**Partial** is an explicit limitation, never a pass inferred from fixtures.
**Deferred** is a recorded decision with numbers. This step changes
documentation, the dogfood driver (an example), `scripts/dogfood.sh`, the
workspace version and the workspace crates' own `Cargo.lock` entries. It
introduces no product behaviour, journal-format or runtime change.

## Plan goals and implementation evidence

| M8 step | Implementation | Tests and recorded evidence | Status |
| --- | --- | --- | --- |
| 01: dogfood baseline (live) | `crates/eval/dogfood/` (fixture `stock-notebook`, five tasks with prompts and mechanical oracles, committed reference patch/answer), `crates/cli/examples/dogfood.rs`, `scripts/dogfood.sh`; accepted at `991f69c2e8570125efba678ba68d8ee6176eb5f0` | `scripts/dogfood.sh --self-test` proves every oracle both ways, the journal metric extractor against a real runtime journal, and the PTY `/dev/tty`; [baseline report](dogfood/m8-baseline.md) and sanitized cells under `docs/dogfood/baseline/`; CI 42010718102 (both jobs success) | Met for the OpenAI half; DeepSeek `not_run` (`key_unavailable`) |
| 02: search and navigation tools | `crates/core/src/tools/{search,builtin}.rs`, `tools/fs/{walk,resolve}.rs`, `crates/cli/src/tui/tools.rs`; `list`/`glob`/`grep` with `Read` effect and trusted-fs backend, walk rules, stated limits, TUI blocks and base instruction ([ADR 0031](decisions/0031-search-and-navigation-tools.md)); accepted at `c4b0e50a15e90fb7e6f3f657449a60ea66702226` | `tools::search::{search_definitions_defaults_strict_validation_and_golden_plans, search_regex_and_glob_validation_bounds_are_pure_refusals, search_policy_reads_ignore_allowances_and_honour_every_read_policy, search_every_argument_is_bound_into_approval_digest}`, `tools::fs::search_tests::{search_list_defaults_types_sizes_byte_order_and_determinism, search_list_and_glob_path_limits_are_explicit_and_exact, search_glob_walk_directory_limit_is_explicit_and_counts_root, search_glob_and_grep_ignore_nested_overrides_negation_and_opt_out, search_dot_directories_require_literal_pattern_components, search_symlink_escape_and_loop_are_never_followed, search_directory_symlink_swap_mid_walk_never_reads_outside, search_protected_paths_and_git_never_appear_or_count_even_with_opt_out, search_ignore_cap_unreadable_and_protected_rules_are_content_free}`, `tools::tests::search_tools_follow_workspace_read_ignore_allowances_and_bind_every_argument`, `runtime::tests::tool_recovery::d9_search_intent_is_closed_read_only_without_reexecution`; original four tools and their golden plans unchanged; CI 41886362604 (both jobs success) | Met |
| 03a: undo design | [ADR 0032](decisions/0032-undo-agent-edits.md) (records, state table, capture, all-or-nothing boundary, recovery, context note, versioning, surfaces, verification plan); accepted at `02e0c93` | Documentation only, reviewed by the orchestrator before 03b; CI 41879615390 | Met |
| 03b: undo of agent file edits | Capture before dispatch, `file_effect` classification, record version 4 (`undo_intent`, `undo_uncertain`, `undo_result`, `undo_note`, `execution_intent.file_image`, `tool_metadata.file_effect`), runtime `PreviewUndo`/`Undo`/`ResolveUndo`, recovery barrier, context note and `undos` report, Ctrl-Z modal, `harness sessions undo <ID> [--yes]`; implemented at `aef721c315cba50dbcde75bfbfda11964ddc2280` (pre-rebase heads `e07c344`, `009920a`, CI 41906109023) plus review `e65e825` (CI 42038415374) | `crates/core/tests/undo.rs` (`undo_restores_an_overwritten_file_and_removes_a_created_one`, `a_preflight_conflict_changes_nothing_and_consumes_nothing`, `a_large_and_a_binary_pre_image_are_streamed_and_restored`, `a_binary_old_target_is_a_valid_pre_image`, `a_turn_with_no_trusted_file_edit_reports_nothing_and_lists_bash`, `a_stale_preview_never_reselects_a_different_turn`, `a_capture_that_stales_refuses_without_dispatching`, `a_pending_undo_blocks_new_work_until_it_is_resolved`, `a_protected_target_never_reaches_capture_or_undo`), `session::tests::undo` (`an_undo_intent_must_be_exactly_the_selection_storage_derives`, `an_intent_whose_targets_disagree_with_the_recorded_images_is_refused`, `the_terminal_note_and_result_are_one_atomic_exact_pair`, `an_unresolved_undo_blocks_turns_configuration_and_overlapping_work`, `a_note_consumes_its_turn_on_its_branch_only`, `a_compaction_may_not_cover_an_undo_note_or_anything_before_it`, `a_missing_pre_image_artifact_blocks_replay`, `a_plan_that_disagrees_with_the_captured_image_is_rejected`, `pre_images_survive_an_undo_and_a_collection`), `tools::fs::undo_tests::temporary_write_failure_is_not_applied_and_other_edits_remain_undoable`, `crates/cli/tests/sessions_undo.rs`, `crates/cli/src/sessions_cli/undo_text_tests.rs::every_undo_path_escapes_terminal_controls`; ADR §9 verification plan and break-on-purpose list | Met |
| 04: parallel read-only tools | Not implemented | Decision 11: 0% against the 10% threshold on the baseline; the m8-06 rerun again measures 0% (no turn with two or more consecutive read-only calls, total tool time 2.72% of turn wall time) | **Deferred** |
| 05: friction fixes | TUI `approve_read_only` (`d15df33`), exit-127 note (`9088ba1`), no-repeat instruction sentence (`56d743f`), stdout ends before diagnostics (`c2bd4db`), specific remembered-grant refusal plus TUI resumption (`f5c928c`), empty export-directory cleanup (`e2971a9`), docs (`d270512`) | `crates/cli/src/config_tests.rs` and `tui/mod.rs` auto-approval tests; `crates/core/src/tools/{tests,sandbox/args}.rs` 127-note and unchanged-status tests; `crates/cli/tests/ask.rs` stdout-newline and `session_has_remembered_grants` tests; `crates/cli/tests/config.rs` TUI-resumption refusal test; `session::tests::artifact_gc::{failed_journal_export_removes_the_empty_directory_it_created, a_failed_export_never_removes_a_preexisting_empty_destination, a_failed_export_never_touches_a_preexisting_nonempty_destination}`; CI at final main head `d270512` 42052880315 and 42046512439 (both jobs success) | Met |
| 06: rerun, comparison, v0.2.0 readiness | Driver `--config FILE` + approval-split metrics + `--self-test` forwarding proof; private user config with `approve_read_only` and a read-only `[sandbox]` toolchain; [rerun report](dogfood/m8-rerun.md) and cells under `docs/dogfood/rerun/`; workspace `0.2.0`; `CHANGELOG.md` `[0.2.0]`; roadmap §8; this document | `scripts/dogfood.sh --self-test` (all oracles both ways, journal metrics with the read-only/other approval split, `--config` forwarding, PTY `/dev/tty`); the rerun itself (five OpenAI cells, five DeepSeek `not_run`); `cargo fmt --all -- --check` and Clippy on the touched crates; final exact-head CI recorded in the step handoff | Met; released afterwards (see [Release record](#release-record)) |
| 07: project toolchains in the bash sandbox | `[sandbox]` table (`toolchain_paths`, `path_prepend`, `cargo_registry`, `env`), load-time refusals, read-only binds, credential masking, private per-command `CARGO_HOME`, workspace `target/`, plan binding, doctor and approval-preview reports ([ADR 0033](decisions/0033-sandbox-toolchains.md)); implemented at `7b645cb` (CI 42027783229) and corrected at `e46fe7d` (CI 42032457831 and 42038365450), both jobs success | `tools::sandbox::settings` and `settings_tests` (env/path/refusal matrix, doctor statuses), `tools::sandbox::args::tests::toolchain_arguments_golden_and_configuration_binding`, `tools::tests::omitted_or_empty_sandbox_keeps_the_v01_plan_bytes`, `crates/cli/src/config_sandbox_tests.rs`, `crates/cli/tests/doctor.rs`, `crates/core/tests/sandbox/toolchains.rs` (`s02_toolchain_credentials_present_and_absent_readonly_and_private_caches`, `s02_toolchain_cargo_test_offline_with_readonly_registry`), `crates/cli/src/tui/tools.rs` preview tests; local run of both S02 toolchain tests on this host: pass | Met |

### Concrete semantics covered

- **Search tools:** identical `read`-class policy (`workspace_read`, protected
  paths, no allowances, `approve_read_only`, D9 read-only restart closure); new
  sessions enable them, existing sessions keep their recorded set; the four
  original tool definitions/indices and golden plans are byte-identical; every
  limit is explicit (`truncated` with counts); `.git` and protected paths are
  neither matched, counted nor revealed; symlinks are never followed; nested
  `.gitignore` overrides and negation work, global excludes are not read.
- **Undo:** one finished turn's `write`/`edit` effects, branch-scoped and
  repeatable; pre-images captured before the mutation and stored as private
  artifacts; all-or-nothing preflight names conflicting paths and changes
  nothing; bash and every other effect are never reversed and are listed; an
  interrupted undo is uncertain and blocks new work until resolved; nothing is
  retried; history stays append-only and the model learns about the undo through
  one recorded user note.
- **Toolchains:** opt-in, user-TOML only, read-only, never auto-discovered; the
  approved bash plan binds the exact mount set, search path, registry and
  environment; a changed configuration is refused on continuation with
  `session_policy_mismatch`; present credential leaves are masked, absent ones
  cannot be created from inside, and no mount is ever made writable.
- **Read-only auto-approval:** covers exactly the registry's `Read`+`TrustedFs`
  tools (`read`, `list`, `glob`, `grep`) in both clients; the approval is
  submitted and journaled as an ordinary allow, never remembered, and the
  transcript says it was auto-approved; `bash`, `write`, `edit`, unknown and
  non-registry tools always ask; `switch_policy` stays ask-only.
- **Compatibility:** readers accept record versions 1–4, writers emit 4, and
  opening never migrates or rewrites. A v0.2.0 binary reads every v0.1 journal;
  a v0.1.0 binary refuses a v4 journal (`unsupported_version`), which the
  changelog states plainly.

## Settled decisions 1–16

| Decision | Resolution and evidence | Status |
| --- | --- | --- |
| 1: M8 is the daily-driver milestone and ends in v0.2.0 | Search tools, undo, sandbox toolchains and the friction fixes are implemented; workspace `0.2.0`, `CHANGELOG.md`, roadmap §8 and this document | Met; tag and assets are the orchestrator's |
| 2: live runs approved (OpenAI Codex, DeepSeek key) | Baseline and rerun use OpenAI `gpt-6.1-sol` through the read-only Codex login, one run per cell; no DeepSeek key was supplied, so its cells are `not_run` / `key_unavailable`; no Anthropic key | **Partial:** DeepSeek dogfood half, Anthropic live and live cross-provider switching |
| 3: disk and compute limits | One debug build per step, serial targeted cargo runs in this worktree's `target/`, no `CARGO_TARGET_DIR` or build-env overrides, scratch directories under `target/` removed; CI is the full gate | Met |
| 4: dogfood measurement design | Dependency-free fixture outside the workspace, five tasks with prompts and mechanical oracles, fresh workspace/data root per cell, PTY driver answering approvals, 10-minute deadline, journal-derived metrics, sanitized evidence, friction log; reused unchanged for the rerun | Met for the OpenAI half; **partial** for DeepSeek |
| 5: search and navigation tools | `list`, `glob`, `grep` as trusted in-process reads with the specified limits, matchers and walk rules, appended after `bash`, TUI blocks, base instruction, ADR 0031 | Met |
| 6: undo of agent edits | ADR 0032 written first and accepted by the orchestrator; implementation meets every listed property, including all-or-nothing preflight, pre-image artifacts, uncertain interrupted undo, the recorded note, append-only history and record version 4; both surfaces exist | Met |
| 7: parallel read-only tools are conditional | Measured 0% against the 10% threshold on the baseline and 0% on the rerun; sequential execution and the one-unresolved-intent rule stay | **Deferred**, with numbers |
| 8: friction fixes (m8-05) | Every candidate is implemented (see step 05, decision 12 and 16); nothing weakens the sandbox, protected paths or approval binding | Met |
| 9: v0.2.0 | Rerun with the same fixture/tasks/providers and a written comparison, version `0.2.0`, changelog section, this acceptance document, roadmap §8 ([rerun](dogfood/m8-rerun.md), [roadmap](roadmap.md)) | Met |
| 10: project toolchains (as amended by 13 and 14) | `[sandbox]` schema, refusals, read-only binds, credential masking, private per-command `CARGO_HOME`, workspace `target/`, optional `cargo_registry`, recorded configuration and continuation comparison, doctor and approval previews, ADR 0033 and the execution-security contract | Met |
| 11: m8-04 deferred | Recorded here and in the roadmap with the baseline and rerun numbers; nothing was implemented | Met as a decision |
| 12: m8-05 scope | F02 TUI auto-approval, F04a 127 note, F04b no-repeat sentence, F05 stdout newline, L06 specific code plus TUI resumption, export cleanup — each with its own behavioural tests and docs; F01 became m8-07; F03 measured by the rerun | Met |
| 13: absent credential files | A present leaf is masked and unreadable (S02 test); an absent leaf is not masked, does not fail the sandbox, and cannot be created from inside with nothing appearing on the host; no placeholder is created and the list stays fixed; doctor reports `masked`/`absent`/`refused` | Met |
| 14: build output and the registry | `CARGO_TARGET_DIR` is not forced; cargo uses the workspace's own `target/`; `CARGO_HOME` stays private and per command; `cargo_registry` is optional, validated with the same refusals, bound read-only at `$CARGO_HOME/registry`, and part of the recorded settings and comparison | Met |
| 15: rerun configuration | Driver `--config FILE` (default `--no-config`), forwarded to `harness --config FILE ask`, proven offline by `--self-test`; the run used `approve_read_only = true` and one read-only toolchain mount, with `RUSTUP_TOOLCHAIN`; approvals reported split into read-only and other with the auto-approved count; DeepSeek absent → `not_run`; every method difference is listed in the report. One deviation: `CARGO_NET_OFFLINE` is refused by `[sandbox.env]`, so it was omitted and the model passed `cargo test --offline` | Met, with the deviation and the documentation correction disclosed |
| 16: the TUI resumes its own remembered grants | The TUI resumes exactly the recorded allowances and keeps their tool/path or exact command/cwd binding; `ask` still refuses with the specific `session_has_remembered_grants` message; protected paths and the sandbox still win (`crates/cli/tests/config.rs`, `crates/cli/tests/ask.rs`) | Met |

## Accepted SHAs, CI and the S02 suites

Each M8 step's implementation was CI-validated. Commits that the orchestrator
rebased before integrating have no run of their own SHA; their pre-rebase heads
do, and the final integrated head ran the full gate again.

| Step | Accepted commit | CI run | Jobs |
| --- | --- | --- | --- |
| 01 baseline + harness | `991f69c` | [42010718102](https://github.com/Apagafuegos/project-harness/actions/runs/42010718102), [42011472853](https://github.com/Apagafuegos/project-harness/actions/runs/42011472853) | Rust workspace success, Host sandbox security (S02) success |
| 02 search tools | `c4b0e50` | [41886362604](https://github.com/Apagafuegos/project-harness/actions/runs/41886362604) | Rust workspace success, S02 success |
| 03a undo design | `02e0c93` | [41879615390](https://github.com/Apagafuegos/project-harness/actions/runs/41879615390) | Rust workspace success, S02 success |
| 03b undo implementation | `aef721c` (pre-rebase `e07c344` → `009920a`) | [41906109023](https://github.com/Apagafuegos/project-harness/actions/runs/41906109023) on `009920a`; review commit `e65e825` [42038415374](https://github.com/Apagafuegos/project-harness/actions/runs/42038415374) | Rust workspace success, S02 success |
| 05 friction fixes + docs | `d270512` | [42052880315](https://github.com/Apagafuegos/project-harness/actions/runs/42052880315), [42046512439](https://github.com/Apagafuegos/project-harness/actions/runs/42046512439) | Rust workspace success, S02 success |
| 07 sandbox toolchains | `7b645cb`, reviewed at `e46fe7d` | [42027783229](https://github.com/Apagafuegos/project-harness/actions/runs/42027783229), [42032457831](https://github.com/Apagafuegos/project-harness/actions/runs/42032457831), [42038365450](https://github.com/Apagafuegos/project-harness/actions/runs/42038365450) | Rust workspace success, S02 success |
| 06 rerun + v0.2.0 readiness | this branch's final commit | exact-head CI runs and both job results are reported in the step handoff, so this document does not embed its own commit's run | Rust workspace, S02 |

The **S02** job is the independent host-security suite: it records doctor before
and after loading the committed runner-only AppArmor profile and runs the
`sandbox`, `tool_host`, `doctor` and `offline_gate` ignored suites serially under
the network/FD wrappers; unavailable isolation is a failure, never a successful
skip. M8 added `crates/core/tests/sandbox/toolchains.rs` to that target, which
this step also ran locally (`cargo test --locked -p harness-core --test sandbox
-- --ignored s02_toolchain`): both tests passed, including the read-only
toolchain, credential masking, absent-leaf creation refusal, private `CARGO_HOME`
and a real `cargo test --offline` through the production sandbox.

## Dogfood evidence

Two measurements with the same fixture, tasks, oracles, deadline and providers:

- [m8-01 baseline](dogfood/m8-baseline.md) (v0.1.0, `73e687b` method) — 5/5
  OpenAI oracles, 20 approval prompts, 25 inferences, 90.0 s wall, 0.92% tool
  time, 0% decision-7 share; DeepSeek `not_run`.
- [m8-06 rerun](dogfood/m8-rerun.md) (v0.2.0 as a user would configure it,
  `d270512` product code) — 4/5 OpenAI oracles (T5's answer was correct but its
  no-change invariant failed on the `Cargo.lock` a real `cargo test` created),
  9 prompts (0 read-only, 9 other, 13 read-only auto-approvals), 27 inferences,
  148.6 s wall, 2.72% tool time, 0% decision-7 share; **cargo ran in the sandbox
  in all 4 attempts and succeeded once (T2) with 16/16 tests passing**; DeepSeek
  `not_run`.

F01–F05 (baseline friction) are resolved or improved and evidenced in the rerun
report: F01 resolved (cargo reachable; T2 and T5 verified with tests), F02
improved (20 → 9 prompts), F03 resolved (13 in-process read calls vs 1), F04
improved (no repeated identical or 127-retry call), F05 resolved (clean stdout
ending). Four new items are open and unfixed by design: F06 (T5's oracle versus
build output), F07 (a bash-based rename is not undoable), F08 (higher token cost
for read-only answers) and F09 (the driver's scratch cleanup). The rerun **found
no product bug**; it found one measurement assumption (F06) and one documentation
defect (`CARGO_NET_OFFLINE`, corrected in ADR 0033's amendment, `docs/tools.md`,
`docs/development.md` and the user guide).

## Offline validation of the changed surface

- `scripts/dogfood.sh --self-test`: all five oracles fail on the pristine
  fixture and pass on the committed reference; T1/T5 reject any file change; the
  real-journal metric extractor reports 2 inferences, 2 reads, 2 approvals and
  one read-only segment and now also proves the read-only/other approval split
  reconstructs the prompts; `--config FILE` is forwarded and the default still
  sends `--no-config`; the PTY exposes `/dev/tty`.
- `cargo fmt --all -- --check` and Clippy on the touched crates; the driver is
  an example, so it is compiled and linted with the workspace.
- The version bump exposed one test that pinned the client's user-agent string to
  `harness/0.1.0` (`crates/providers/src/tests.rs`, the fake server's
  `headers_ok` probe behind `captured_secret_outputs` and
  `http_error_taxonomy_retry_and_redaction`). It now derives
  `harness/<CARGO_PKG_VERSION>` from the crate version, so the assertion stays
exact for the built version and a future bump cannot silently weaken it. This is
a test expectation, not product behaviour; `cargo test -p harness-providers
--lib` passes (90 tests).
- Every changed Markdown link and anchor, and every guide example's CLI syntax,
  was checked against `--help` from the current build.
- No product test, journal format or runtime path changed in this step, so the
  existing suites are the evidence for M8's behaviour; the exact-head CI run is
  the final gate.

## Partials, deferred items and residual risks

Carried forward unchanged; none is weakened by this step.

- **Partial:** Anthropic live compatibility and live cross-provider switching
  (no Anthropic key was ever supplied; fixture conformance is not live proof).
- **Partial at release, since measured:** the DeepSeek half of the dogfood rerun
  was `not_run` when v0.2.0 was accepted, and ran afterwards on the same product
  code (see the [rerun addendum](dogfood/m8-rerun.md#addendum-deepseek-half-added-2026-11-05-after-v020-was-published)).
  There is still no v0.1 DeepSeek baseline.
- **Deferred:** m8-04, parallel read-only tools — 0% of the 10% threshold in both
  measurements; sequential execution and the one-unresolved-intent storage rule
  stay. Not a post-v0.2.0 candidate on this evidence.
- **Undo (ADR 0032):** no redo; bash and other effects are never reversed; turns
  from before pre-image capture cannot be undone (`undo_unavailable_legacy`);
  large overwritten targets cost capture I/O and disk; multi-file restoration is
  not atomic across files, and an interruption is reported as uncertain rather
  than completed; the retained user note is authoritative while older history
  still describes the original edits; a v0.1.0 binary cannot continue a journal
  after its first v4 append.
- **Toolchains (ADR 0033):** a mounted toolchain is trusted user code and can
  read every mount and the workspace; masking covers the fixed credential leaves
  only, not every secret-shaped file, and not hard-link aliases; absent
  credential leaves are not pre-masked (they cannot be created from inside; the
  L04 host race is unchanged); `target/` is shared with the user's own builds;
  `CARGO_HOME` is per command, so registry re-resolution repeats; the recipe
  names exact paths and host toolchain drift makes a mount missing or the build
  fail; the mounted registry must contain every dependency; rustup proxy shims
  do not work without their real targets.
- **v0.1 audit residuals (unchanged):** no seccomp; TasksMax needs a usable
  systemd user manager; shell mounts protect names, not every hard-link alias or
  hostile same-user host race; absent protected paths and an initially absent
  `.git` cannot be masked in advance; private artifacts are not encrypted and
  ordinary text is not scrubbed; no signed provenance or independently
  reproducible binary proof; no claim that vulnerabilities are absent.
- **New, open:** F06 (a no-change oracle conflicts with real build output), F07
  (bash-based mutations are outside undo), F08 (context cost of many small read
  calls above), F09 (driver scratch cleanup). Each is recorded with its evidence
  in the [rerun report](dogfood/m8-rerun.md); none is fixed here.

## Release boundary

Prepared here: workspace `0.2.0`, `Cargo.lock` workspace entries, the
`CHANGELOG.md` `[0.2.0] - Unreleased` section (Added/Changed/Fixed/Security, the
v1–v4 compatibility note and no published-asset claim), roadmap §7's corrected
heading plus §8, README/documentation index updates, the user guide's search,
undo, auto-approval and `[sandbox]` sections, the reconciled contracts, and this
document. Not done here, by instruction: no tag, no release-workflow dispatch, no
`main` change and no asset verification. The orchestrator does those after
acceptance.

## Release record

- `main` integrated the reviewed m8-06 candidate at
  `0c5157f6ce534bceca2b7dc4999a9a33d2424ae5` after CI 42058968787 passed (Rust
  workspace and Host sandbox security (S02)).
- Final-candidate dispatch [42062285842](https://github.com/Apagafuegos/project-harness/actions/runs/42062285842)
  passed on that exact commit: both `verify` jobs, both native musl builds,
  packaging and checksum generation; publish is skipped on dispatch by design.
- Its `release-assets` artifact was downloaded before tagging: `SHA256SUMS`
  verified both archives. Only the **aarch64** archive was opened before
  tagging; it held the binary, both licences, README, `CHANGELOG.md`, `docs/`
  and `third-party-notices/`, and its binary reported `harness 0.2.0`. The
  x86_64 archive's listing was compared **after** publication and holds the
  same files. (Corrected in v0.2.1: the earlier text claimed both archives
  were opened before the tag, which did not happen.)
- Annotated tag `v0.2.0` on that commit started publish run
  [42064489275](https://github.com/Apagafuegos/project-harness/actions/runs/42064489275),
  which passed every job and published the
  [v0.2.0 release](https://github.com/Apagafuegos/project-harness/releases/tag/v0.2.0)
  at 2026-11-05T16:37:49Z (not a draft or prerelease).
- The published `SHA256SUMS` and both archives were re-downloaded, verified
  with `sha256sum -c` and are byte-identical to the dry-run artifact.
- Final binary reproducibility is not claimed; the partials, deferred item and
  residual risks above still apply.
