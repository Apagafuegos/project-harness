# Milestone 6 acceptance

Milestone 6, “context experiments”, is complete within its agreed scope as of
**2026-11-02**. This maps every sentence-level criterion in
[roadmap §6](roadmap.md#6-context-experiments--complete), the relevant RFC-001
**C01/C02** clauses and section 6.1, the M6 settled decisions 1–18, and the
implemented DeepSeek profile to evidence that runs in the gate. The two live
matrices are recorded separately and are not gate evidence; one strategy default
was chosen by the user from them.

Status: **met** means covered within the agreed milestone scope; **partial**
means a recorded limitation with its owning decision or later work named; **gap**
means an uncovered in-scope requirement. There are no gap rows.

Test names are Rust functions unless a target, script or example is named. The
live provider evidence is DeepSeek-only and is described under
[Live checks](#live-checks--opt-in-outside-the-gate).

## Scope

| Area | Recorded decision |
| --- | --- |
| DeepSeek provider profile | [ADR 0024](decisions/0024-deepseek-profile.md) |
| Whole-turn pins/exclusions, schema v3 | [ADR 0025](decisions/0025-context-controls.md), RFC-001 6.1, [ADR 0019 m6-02 amendment](decisions/0019-compaction.md#m6-02-amendment) |
| Strategy selection and the two alternatives | [ADR 0026](decisions/0026-compaction-strategies.md), and its m6-07 amendment |
| Offline evaluation harness | [ADR 0027](decisions/0027-evaluation-harness.md) |
| The two live matrices | [context experiments](context-experiments.md) |

Record version 3 is the only version writers produce; v1 and v2 remain
readable. Version numbers stayed `2`/`1`/`2` for `rolling_summary` /
`observation_masking` / `structured_state` in this step. No schema bump,
migration or artifact rewrite was added for the chosen default.

## Acceptance matrix

### Context controls and compaction (roadmap §6, C02, RFC 6.1)

| Criterion | Evidence running in the gate | Status | Note |
| --- | --- | --- | --- |
| Record version 3: a new `context_control` branch entry and optional `Compaction.model` | `shipped_schema_bytes_are_pinned`, `only_integer_versions_one_to_three_select_a_schema`, `each_version_requires_exactly_its_own_fields`, `context_control_is_a_v3_entry_kind_with_a_closed_mode_set`, `a_v3_compaction_may_omit_its_model_but_never_write_null`, `writers_refuse_any_version_but_the_current_one`, `new_sessions_write_every_record_as_v3`, `a_lower_version_after_a_higher_one_is_corrupt`, `a_v2_prefix_continues_as_v3`, `a_repaired_v1_tail_continues_as_v3` | met | v1–v3 read, v3 written. Older journals continue as v3 without migration. The `Compaction.model` field is absent, never `null`, for a model-free record. |
| Controls are append-only branch entries, replayable and undoable only by another control | `controls_are_accepted_while_idle_and_are_idempotent`, `set_context_control_is_accepted_while_idle`, `set_context_control_is_refused_when_not_idle`, `a_storage_rejection_is_typed_and_writes_nothing`, `a_control_survives_a_reopen`, `replay_rebuilds_effective_controls_exactly`, `a_control_on_a_sibling_branch_does_not_apply_to_this_one`, `a_control_target_must_be_a_finished_turn_on_this_ancestry`, `a_control_is_refused_inside_a_turn_and_in_a_turn_opening_commit`, `a_control_is_refused_inside_a_tool_exchange` | met | A control is a tree entry like `model_changed`, never dialogue. A sibling forked before it does not inherit it. A no-op is `context_control_noop`; a covered target is `context_control_covered`. Nothing is written on refusal. Undoing is `unpin`/`include`, never a rewrite. |
| Whole-turn granularity: one finished turn, user message + responses + tool exchanges, never split | `golden_context_controls_pin_and_exclude_whole_turns`, `controls_apply_to_whole_turns_of_the_selected_ancestry`, `excluding_a_turn_never_splits_a_tool_group`, `a_pin_marks_its_turn_without_changing_the_projection`, `golden_compaction_places_the_summary_at_its_first_covered_entry`, `tree_shows_each_turns_effective_mode_at_80x24_and_20x5` | met | The fold applies to every entry of the target turn on the selected ancestry. The report and transcripts show the mode; a pinned turn is included exactly as it was. |
| Pinned turns are verbatim, outside coverage, in original order before the retained window | `c02_automatic_summaries_respect_pins`, `projection_order_is_summary_then_pinned_turn_then_retained`, `rolling_compaction_skips_pins_and_keeps_the_order`, `a_pinned_turn_that_cannot_fit_fails_explicitly`, `a_large_pinned_turn_is_never_truncated_and_overflows_the_budget`, `storage_refuses_a_compaction_covering_a_pinned_turn`, `coverage_may_skip_a_controlled_turn`, `coverage_refuses_a_gap_of_an_uncontrolled_turn`, `coverage_refuses_a_partially_covered_turn`, `coverage_refuses_an_excluded_turn`, `coverage_refuses_a_wrong_order` | met | Coverage is the selected span minus every controlled turn, validated identically in storage and the builder. RFC-001 6.1's “required pinned items remain outside coverage” holds. A pin that cannot fit fails explicitly instead of being truncated. |
| Excluded turns stay in history/transcript/report and never reach a summary model | `summarizer_never_receives_a_controlled_entry`, `excluded_turn_stays_excluded_after_compaction`, `resetting_a_skipped_excluded_turn_restores_it_in_place`, `every_strategy_skips_pinned_and_excluded_turns`, `loop_exclude_shows_user_excluded_and_the_request_omits_the_turn`, `excluded_turn_is_tagged_and_dimmed_but_still_readable` | met | `user_excluded` is a projection reason only. The message, response and tool results stay readable and the recorded request omits them. Resetting restores the turn verbatim, never inside a summary. |
| Budget failure is explicit; nothing truncates or silently retries | `c02_budget_fits_or_failure_is_explicit`, `a_large_pinned_turn_is_never_truncated_and_overflows_the_budget`, `oversized_protected_content_rejects_after_one_compaction_attempt`, `mid_turn_overflow_fails_without_compaction_inside_turn`, `loop_budget_failure_preserves_reason_and_suggests_eligible_compaction`, `observation_masking_over_budget_fails_explicitly`, `masked_span_too_long` path | met | `context_budget_exceeded`, `context_budget_unknown`, `masked_span_too_long` and `summary_too_long` are stable codes; no path truncates a pinned turn or fabricates a saving. |
| Preview leaves history/workspace unchanged; a stale preview/commit is refused | `c02_manual_preview_leaves_journal_and_workspace_unchanged`, `c02_stale_head`, `c02_stale_sequence`, `c02_stale_model`, `c02_stale_policy`, `c02_stale_source_manifest_substitution`, `c02_preview_never_revives_after_commit_navigation_restart_or_fault`, `a_strategy_change_makes_an_outstanding_preview_stale_policy`, `loop_preview_cancel_leaves_journal_bytes_identical`, `loop_stale_preview_names_the_changed_field_and_offers_retry`, `preview_first_requires_a_render_and_explicit_enter` | met | Previews are ephemeral, single-use and read-only. A strategy change is part of the policy fingerprint, so it stales an outstanding preview. Retrying is explicit, never automatic. |
| Automatic compaction keeps its own enabling rules and recent window | `c02_automatic_retains_configured_recent_turns`, `c02_automatic_references_exact_source_ids`, `c02_automatic_supersedes_only_ancestor_summaries`, `c02_automatic_summaries_respect_pins`, `post_turn_trigger_is_exactly_850_permille_and_never_inside_turn`, `automatic_single_attempt_per_trigger_and_failure_never_blocks`, `idle_and_recovery_guards_refuse_compaction_without_provider_work`, `loop_automatic_compaction_visible_and_refreshes_open_panel` | met | Choosing `observation_masking` as the new-session default does **not** enable or disable `automatic`: it still follows the configured summary model and explicit field. No automatic trigger runs inside a turn, on reopen or after a failure retry. |
| Original entries remain readable and byte-identical | `c02_original_entries_remain_readable_byte_identical`, `entries_written_before_v3_project_identically`, `a_strategy_change_is_recorded_idle_only_and_invalidates_previews`, `a_strategy_change_needs_an_idle_session_and_a_usable_model` | met | Compaction and strategy changes are appends. Old records, summaries and coverage keep their meaning; a recorded version is never rewritten. |
| C01: reproducible projection/report, branch isolation, estimates vs reported usage, unknown budgets, whole tool groups | `fixture_reports_are_identical_after_reopening_and_across_data_roots`, `golden_branching_reports_only_the_selected_branch`, `reported_usage_is_labelled_and_never_added_to_estimates`, `budget_is_unknown_without_a_limit_and_flags_overflow_and_compaction`, `answered_calls_are_included_with_their_results_directly_after`, `golden_context_controls_pin_and_exclude_whole_turns`, `excluding_a_turn_invalidates_a_continuation_bound_to_the_old_prefix` | met | C01's row was already closed in M4; M6 adds the control fold and v3 projection to the same invariants. Reports stay distinguishable and reproducible. |

### Strategies, model-free records and the chosen default

| Criterion | Evidence running in the gate | Status | Note |
| --- | --- | --- | --- |
| `observation_masking` is deterministic and model-free; it records no model | `observation_masking_is_a_deterministic_zero_stream_golden`, `strategy_identity_is_closed_and_recorded`, `a_v3_compaction_may_omit_its_model` | met | Zero provider streams, `Compaction.model == None`, checked-in golden text. The strategy stores no provider, so the promise is a type-level fact. |
| `structured_state` and `rolling_summary` keep an explicit model-based path | `structured_state_extracts_are_ordered_and_untouchable`, `structured_state_extracts_are_bounded_with_explicit_elision`, `structured_state_matches_batched_commands_to_their_own_results`, `exact_request_has_independent_model_no_tools_or_native_state`, `model_based_requests_end_with_one_fixed_request_only_prompt`, `unconfigured_summary_model_refuses_but_selection_works`, `a_summary_request_ends_in_a_user_turn_after_an_assistant_response` | met | Model-based generation requires a configured summary model and never guesses the session model. The request-only prompt is not a journal entry and changes no projection bytes or versions. |
| Explicit strategy selection keeps working, including a recorded idle switch | `explicit_compaction_strategy_is_recorded_and_the_new_session_default_is_masking`, `both_parsers_detect_every_explicit_strategy_selection`, `a_strategy_change_is_recorded_idle_only_and_invalidates_previews`, `a_strategy_change_needs_an_idle_session_and_a_usable_model`, `continuation_refuses_a_compaction_strategy_change` | met | All three selections are recorded on creation. A later switch is one `ConfigurationChanged` via `SetCompactionStrategy`; it is idle-only and its previews/continuations follow the recorded policy. |
| A new ask/TUI session defaults to `observation_masking` and records it | `ask_parser_defaults_a_new_session_to_observation_masking`, `plain_harness_parser_defaults_a_new_session_to_observation_masking`, `plain_tui_new_session_records_the_shared_masking_default`, `explicit_compaction_strategy_is_recorded_and_the_new_session_default_is_masking` | met | One definition, `wiring::creation_compaction_strategy()`, feeds both parsers through `ask_cli::strategy_selection`; both create through the same wiring, so the strategy is in the creation configuration artifact. |
| Legacy omitted strategies keep their `rolling_summary` meaning; continuations are not rewritten | `creation_default_and_legacy_decoding_are_distinct`, `omitted_strategy_session_continues_as_rolling_summary_untouched`, `recorded_legacy_session_continues_without_an_implicit_strategy_change`, `strategy_identity_is_closed_and_recorded`, `configuration_defaults_and_invalid_shapes` | met | `StrategyId::default()` and `CompactionConfig` decoding stay `rolling_summary`. A continuation with an omitted flag keeps the recorded strategy and appends no configuration change; journal bytes before the new turn are unchanged. Explicit differing selections remain refused by the existing mismatch policy. |
| The default does not change automatic enablement, model selection or summary-model handling | `summary_model_opt_in_is_independent_and_persisted`, `continuation_refuses_a_summary_policy_change`, `c02_automatic_retains_configured_recent_turns`, `explicit_compaction_strategy_is_recorded_and_the_new_session_default_is_masking` | met | Masking+no summary model records `automatic: false`; a configured summary model still enables it exactly as before. Continuing still requires the recorded summary model. Versions remain 2/1/2. |

### Client controls, inspector and terminal boundary

| Criterion | Evidence running in the gate | Status | Note |
| --- | --- | --- | --- |
| TUI keys select a whole turn and reset it; refusals are the engine's | `tree_keys_toggle_each_turns_effective_mode`, `loop_pin_journals_the_control_and_the_inspector_shows_pinned`, `loop_exclude_shows_user_excluded_and_the_request_omits_the_turn`, `loop_reset_returns_the_turn_to_default`, `loop_covered_turn_refusal_is_shown_verbatim_and_writes_nothing`, `loop_refusal_on_the_active_turn_sends_nothing`, `loop_pinned_turn_becomes_excluded_with_one_control` | met | `p`/`x` send exactly one `SetContextControl` for the whole turn; a second press resets. The client never decides a control itself. |
| Inspector and tree show modes and reasons | `pure_panel_budget_order_exclusions_configuration_and_preview_label`, `loop_panel_report_refreshes_after_committed_turn`, `tree_shows_each_turns_effective_mode_at_80x24_and_20x5`, `excluded_turn_is_tagged_and_dimmed_but_still_readable`, `opaque_schema_payload_and_portability_sentinel_never_render_in_panel` | met | `[pin]`/`[excl]` markers, `pinned` and `user_excluded` reasons, and a `Controls` section. Excluded content stays readable; no native payload is rendered. |
| The TUI boundary test stays green | `tui_never_builds_requests_runs_tools_or_approves_without_a_key`, `boundary_guard_rejects_provider_execution_and_timer_approval_mutations` | met | `crates/cli/tests/tui_boundary.rs` runs in the gate. M6 added no provider request, tool authorization/execution or approval decision to `crates/cli/src/tui/`. |
| `harness context` and the read-only catalog expose controls without writes | `prints_the_golden_report_of_the_context_controls_fixture`, `catalog_json_and_context_include_new_session_instructions_without_modification`, `refuses_a_locked_session_at_once_and_reports_bad_input` | met | The CLI report includes the effective control modes; inspection takes a shared lock and writes nothing. |

### DeepSeek, evaluation and live evidence

| Criterion | Evidence running in the gate | Status | Note |
| --- | --- | --- | --- |
| DeepSeek is a Messages-adapter profile with its own instance, key, catalog and native-state owner | `every_conformance_case_passes_on_deepseek_fixtures`, `a_plain_deepseek_turn_is_one_messages_body`, `images_follow_the_deepseek_catalog`, `an_unsigned_thinking_block_maps_only_on_deepseek`, `an_unsigned_thinking_payload_replays_only_on_deepseek`, `redacted_thinking_is_outside_the_deepseek_vocabulary`, `the_deepseek_key_is_sent_as_x_api_key_and_never_rendered`, `a_missing_deepseek_credential_fails_before_submission`, `env_deepseek_key_selects_the_api_key_header`, `catalog_rows_match_the_deepseek_models_page`, `deepseek_is_registered_with_its_key_and_skipped_without_it`, `deepseek_provider_requires_its_own_key_and_no_openai_auth_flag` | met | No new wire format, no core branch: profile data plus mapping hooks. `deepseek-flash` and `deepseek-v4-pro` are sourced catalog rows. Missing key is skipped unless selected, then exit 3 with no secret. The live endpoint itself is exercised by both matrices below, not by `scripts/live-check.sh deepseek` (which had no key at m6-00). |
| Evaluation is a dev-only crate with no core/product types and no binary linkage | `crates/eval` (`publish = false`), `offline_pipeline_is_deterministic_complete_and_does_not_leak_the_check`, `scripts/check.sh` runs `cargo test -p harness-eval --test offline_gate -- --ignored` | met | `harness` does not depend on `harness-eval`; core gains no evaluation type. |
| Fixed tasks record configuration/input provenance, and metrics are deterministic where they can be | `every_task_declares_exactly_the_files_its_history_changed`, `a_summary_that_drops_a_requirement_lowers_projection_retention`, `a_continuation_that_does_not_fix_the_task_completes_false`, `completion_rejects_a_change_to_the_forbidden_math_file`, `completion_rejects_an_extra_file_in_the_no_new_files_task`, `byte_identical_output_includes_the_final_newline`, `aggregate_halves_are_derived_from_the_records`, `cli_records_a_real_commit_and_refuses_a_malformed_one`, `a_non_compaction_configuration_change_changes_source_provenance`, `live_plan_replays_the_fixed_history_before_the_first_live_request`, `live_and_scripted_plans_replay_identical_history_and_workspaces` | met | Requirement retention is literal-substring matching against each task's stated requirements; file recall is from the recorded tool results; completion is the task's hidden check in Bubblewrap. `session_configuration` and `compaction_policy` are separate identities (decision 15). |
| The offline matrix proves mechanics and is separate from explicit live checks | `offline_pipeline_is_deterministic_complete_and_does_not_leak_the_check`, `the_hidden_check_runs_inside_the_sandbox_and_never_on_the_host`, `hidden_check_publication_refuses_links_and_existing_paths`, `a_failed_compaction_skips_the_probe_and_the_hidden_check`, `a_probe_failure_is_attributed_and_skips_the_hidden_check`, `a_stalled_probe_times_out_and_the_continuation_never_runs`, `a_successful_live_plan_reaches_completion_without_offline_reply_leakage`, `hidden_check_cleanup_preserves_a_replaced_regular_file` | met | The offline target runs the full pipeline with `ScriptedProvider` and is required in both CI jobs. Live mode is `scripts/eval.sh --live --yes`, opt-in, bounded, never in CI, and needs `DEEPSEEK_API_KEY`. |
| Both live matrices are recorded with their exact source SHAs, commands and limitations | [context experiments](context-experiments.md) and the committed result files under `eval/results/m6-06*` | met | First run: 4/12 measured, 8 `provider_failure`. Second run: 12/12 measured, 0 mechanical errors. All raw values, earlier failures, fingerprints, estimates and limits are preserved; no raw result was rewritten. |

## Running the evidence

```sh
cargo fetch --locked                       # provisioning, may use network
CARGO_NET_OFFLINE=true ./scripts/check.sh  # full offline gate
# Stronger external-network enforcement, as used for the acceptance run and CI:
./scripts/with-offline-network.sh ./scripts/with-clean-fds.sh \
  env CARGO_NET_OFFLINE=true ./scripts/check.sh
```

`check.sh` builds every target; runs the workspace tests (including the bin
parser tests), the `test-provider` PTY target, the conformance-feature core
tests, the mandatory S02/security suites and the offline evaluation pipeline;
then runs both examples, the `harness context` goldens and the binary smoke
tests. `cargo fmt --all` is run before the gate.

The complete Rust **1.99.1** acceptance gate was run on 2026-11-02 with a
**3,900,000 ms** timeout under
`./scripts/with-offline-network.sh ./scripts/with-clean-fds.sh env
CARGO_NET_OFFLINE=true ./scripts/check.sh`. It exited **0**: 44 test-result
groups, **1,257 tests passed, 0 failed**, including the required `sandbox`,
`tool_host`, doctor and offline-evaluation `--ignored` targets and the
`test-provider` PTY target. No paid provider call, live matrix or credential was
used.

Targeted suites for this step:

```sh
cargo test -p harness --bin harness strategy_selection
cargo test -p harness --test ask
cargo test -p harness --test ask_cli
cargo test -p harness --test tui_runtime
cargo test -p harness --test tui_controls
cargo test -p harness --test tui_inspector
cargo test -p harness --test tui_boundary
cargo test -p harness-core context::tests::compaction_tests
cargo test -p harness-core runtime::tests::compaction
```

## Break-on-purpose verification

Each mutation was applied alone to the committed tree, the named evidence was
observed failing, and the tree was restored before the gate.

| Temporary violation | Named evidence that failed | Result |
| --- | --- | --- |
| Return the shared parser default to `rolling_summary` | `ask_parser_defaults_a_new_session_to_observation_masking`, `plain_harness_parser_defaults_a_new_session_to_observation_masking` | The parser no longer resolves the chosen default. |
| Flip `StrategyId::default()`/`CompactionConfig` decoding to `observation_masking` | `creation_default_and_legacy_decoding_are_distinct`, `strategy_identity_is_closed_and_recorded`, and the legacy TUI inspector fixtures whose configuration omits a strategy | Old journals would be silently reinterpreted. |
| Treat an omitted continuation flag as an explicit selection | `recorded_legacy_session_continues_without_an_implicit_strategy_change`, `omitted_strategy_session_continues_as_rolling_summary_untouched`, `continuation_refuses_a_compaction_strategy_change` | A continuation would be refused or rewritten. |
| Let the client creation default also rewrite an existing configuration | `recorded_legacy_session_continues_without_an_implicit_strategy_change` (journal prefix and zero `configuration_changed` assertions) | Historical bytes changed. |
| Let `observation_masking` call the provider | `observation_masking_is_a_deterministic_zero_stream_golden` | One stream was recorded instead of zero. |
| Let a summary cover a pinned turn | `c02_automatic_summaries_respect_pins`, `storage_refuses_a_compaction_covering_a_pinned_turn` | Coverage validation accepted a controlled turn. |
| Give an excluded turn's entries to the summarizer | `summarizer_never_receives_a_controlled_entry` | The narrowed source contained a controlled entry. |

## Partial rows

| Item | What is missing | Where it belongs |
| --- | --- | --- |
| A general quality ranking of the three strategies | Four fixed, prose-heavy tasks × one repetition per matrix; lexical matchers; one probe turn; no LLM judge. This cannot estimate variation or significance, and `multi-file-change` scores 0/3 on the probe in every arm. | Future evaluation with a larger suite and repetitions. Not inferred from M6. |
| `observation_masking` as a memory product | Masking is lossy for observations, guarantees neither a shorter projection nor workspace rollback, and grew the estimated projection (+6.0%) on this suite. The default is the user's measured provisional choice (2026-11-02, decision 18), not a universal result. | [Context experiments: Limits](context-experiments.md#limits) and ADR 0026's m6-07 amendment. |
| Live Anthropic compatibility and cross-provider live switching | One live run with `ANTHROPIC_API_KEY` | Carried from M4/M5; ADR 0018/0020. Outside M6. |
| Live TUI end-to-end | One completed live answer in the TUI; the M5 smoke hit `rate_limit` | Carried from M5; opt-in procedure. M6's TUI controls are covered offline through the production loop. |
| `scripts/live-check.sh deepseek` | The m6-00 step had no key, so that script's pass path was unrun; the live DeepSeek endpoint is proven by the two matrices instead | ADR 0024's opt-in check remains available. |
| Exact token counts and workspace rollback | Estimator-only byte counts; no tokenizer and no filesystem checkpointing | ADR 0026 / roadmap “Outside v0.1”. |

## Known limits recorded by this milestone

- **The chosen default is separate from decoding.** `StrategyId::default()` and
  an omitted `compaction.strategy` remain `rolling_summary` forever; only the
  clients' creation default is `observation_masking`. This is deliberate so old
  journals are never reinterpreted (ADR 0026 m6-07 amendment).
- **Masking is not compression by guarantee.** It removes tool-result bodies and
  keeps dialogue, so a prose-heavy span can grow. It makes no model call and
  provides no rollback.
- **Pins are verbatim, not unlimited.** A pinned turn that cannot fit fails the
  turn with the existing budget codes.
- **Controls are whole turns.** There is no per-entry pin, no control of an
  unfinished turn, and no rewrite: undo is another control.
- **Live evidence is DeepSeek Flash only** (plus the catalog row for Pro), with a
  small fixed suite. No Anthropic or OpenAI live comparison was run for M6.
- **No paid request is part of CI.** `scripts/eval.sh --live` is opt-in, bounded
  and requires an explicit `--yes` and a key; ordinary tests are offline.

## Handoff

- Milestone 6 implementation is complete in the recorded scope; the next
  milestone is not started here.
- The product default for new ask/TUI sessions is `observation_masking`, chosen
  by the user on 2026-11-02 from the second matrix, and recorded in the creation
  configuration of each new session.
- Legacy `rolling_summary` decoding, recorded strategies, explicit selections,
  recorded idle switching and the automatic-compaction enablement rules are
  unchanged.
