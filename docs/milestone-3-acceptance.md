# Milestone 3 acceptance

Milestone 3, "tools, selective security, and recovery", is complete as of
2026-10-29. This maps every criterion of [roadmap section 3](roadmap.md#3-tools-selective-security-and-recovery--complete)
and RFC-001 T01, T02, S01, S02, R01, R02 and J03's tool-output artifact clause
to executable gate evidence. Test names below are Rust functions, unless a
script or example is named explicitly.

Status: **met** means covered within the agreed milestone scope; **partial**
means a recorded limitation with its owning decision or later milestone named;
**gap** means an uncovered in-scope requirement. There are no gap rows.

Core scenarios use real `JournalStore` storage with its test oracle enabled,
run twice, replay recorded inputs and clock samples, check event/view resync,
and reopen the journal. Counted executors test orchestration only. S01 uses
real `FsWorkspace`; S02 uses real Linux `Sandbox` and the assembled host, not
counted substitutes. The fixed registry exposes no model-selected executor or
public constructor for `AuthorizedOperation`.

| Criterion | Evidence | Status | Note |
| --- | --- | --- | --- |
| Registry validation | `t01_resolve_validate_plan_and_policy_refusals_never_execute`, `provider_definitions_filter_enabled_available_in_registry_order`, `enabled_tool_without_its_backend_refuses_before_validation_or_planning` | met | Resolve enabled/available names before strict schema and semantic validation. Incomplete model responses remain ineligible (`malformed_or_incomplete_tool_calls_are_never_answered`). |
| Sequential scheduling | `t02_three_valid_calls_execute_one_at_a_time_even_after_executor_failure`, `t02_valid_calls_wait_for_terminal_result_ack_and_keep_association_after_failure_and_cancel`, `t02_results_and_execution_stay_in_block_order_after_invalid_call` | met | No second planning/executor effect while an execution or its result commit is outstanding. Three valid calls, earlier executed failure, and cancellation retain association and one result per call on the branch. |
| Policy | `t01_resolve_validate_plan_and_policy_refusals_never_execute`, `remember_reloads_policy_and_second_identical_call_never_asks`, `broken_policy_fails_turn_not_tool_and_runtime_remains_usable` | met | Allow/ask/deny and remembered configuration grants; invalid configuration fails the turn, not as a model-facing refusal. |
| Approval binding and revocation | `approvals_mismatch_is_commit_free_then_approve_or_deny_or_expire`, `approval_wall_clock_expiry_cannot_be_bypassed_by_delaying_the_timer`, `policy_revision_invalidates_pending_approval_with_durable_denial_and_no_execution`, `r02_cancel_approval_answers_every_pending_call_and_revokes_ticket` | met | Exact pending call/plan/revision/expiry; a wrong digest is a command rejection, not a tool result. Expiry decision and timestamp share one instant. |
| Opaque authorized operations | `write_ahead_exact_plan_and_executor_failed_result_are_durable`, `fs_executor_trait_runs_authorized_plans_and_shared_cancellation`, `s02_bash_approval_publishes_verified_capture` | met | Only the core constructs the opaque operation after authorization and durable intent; executors consume its exact typed plan, not model JSON. |
| Descriptor-relative file access | `s01_invalid_paths_never_reach_openat2`, `s01_symlink_final_intermediate_and_outside`, `s01_proc_root_and_fd_magic_links`, `s01_mount_crossing_dev_shm_is_required_not_skipped`, `s01_hard_links_to_outside_refused_during_planning`, `s01_directory_relocated_outside_root_during_resolution_or_reopen`, `s01_protected_paths_direct_and_different_spelling_by_inode` | met | Strict `openat2` resolution, no symlinks/mount crossings/fallback; descriptor and ancestor identity protection. |
| Safe write/edit replacement | `fs_write_and_edit_are_atomic_preserve_mode_and_report_post_image`, `fs_write_and_edit_strip_special_mode_bits`, `s01_changed_content_while_preparing_replacement_is_stale`, `s01_created_target_at_rename_requires_noreplace`, `s01_cancel_at_every_precommit_step_leaves_original_and_no_temp`, `s01_cancel_after_rename_reports_success` | met | Exclusive temporary, data/mode sync, rename, parent sync; stale pre-images refused. Post-rename cancellation preserves success, never claims rollback. |
| Intent before side effects | `write_ahead_exact_plan_and_executor_failed_result_are_durable`, `artifact_publication_failure_never_dispatches_an_uncommitted_intent`, `r01_four_mutation_crash_points_never_reinvoke_and_navigation_cannot_evade_uncertainty`, `randomized_interleavings_keep_storage_and_events_consistent` | met | Every observed Execute must have an acknowledged intent and verified exact plan artifact. |
| Results before dependent inference and notification | `write_ahead_exact_plan_and_executor_failed_result_are_durable`, `t02_three_valid_calls_execute_one_at_a_time_even_after_executor_failure`, `s02_multimegabyte_capture_uses_append_path_not_upload_limit` | met | Captures are published before result append; next call/inference and ToolFinished follow result acknowledgement. |
| Explicit recovery of uncertainty | `r01_four_mutation_crash_points_never_reinvoke_and_navigation_cannot_evade_uncertainty`, `d30_write_then_read_and_read_write_read_wait_for_the_user`, `d9_read_intent_then_not_started_calls_closes_in_branch_order`, `s02_process_crash_kills_sandbox_and_never_replays_intent` | met | Mutations require user resolution. Exactly-read unrecorded uncertainty closes cancelled/process_restart (D9); v1 unknown effects do not qualify. At most one unresolved intent (D30). |
| Bash only behind Bubblewrap; mounts | `s02_probe_required`, `s02_host_secrets_and_data_root_hidden`, `s02_outside_workspace_writes_denied`, `s02_readonly_workspace_write_denied`, `s02_git_write_denied`, `s02_masked_directory_and_file_denied`, `s02_protected_descendants_stay_masked_beneath_git_ro_bind` | met | Configured read-only runtime mounts, private home/tmp/dev/proc, plan-bound workspace/masks. |
| Bash network denial | `s02_network_dns_external_tcp_host_loopback_and_only_lo`, `s02_bash_network_is_denied` | met | DNS, external TCP and host loopback inaccessible. |
| Bash environment/descriptor filtering | `s02_secret_environment_not_inherited`, `s02_harness_descriptors_not_inherited`, `s02_unknown_non_cloexec_descriptor_fails_before_execution`, `s02_inheritable_descriptor_disables_shell_at_startup` | met | Clearenv plus host env_clear, allowlist only; unexpected inheritable descriptors fail closed. |
| Bash deadlines and bounded output | `s02_timeout_cleans_grandchildren_and_term_ignoring_process`, `s02_exit_status_signal_and_bounded_output`, `s02_bash_timeout_cleans_namespace`, `s02_multimegabyte_capture_uses_append_path_not_upload_limit` | met | Bounded inline text and raw capture artifacts, timeout and overflow terminate the workload. |
| Bash descendant cleanup | `s02_background_grandchild_cleaned_after_success`, `s02_cancel_cleans_grandchildren`, `s02_timeout_cleans_grandchildren_and_term_ignoring_process`, `s02_cancel_cleans_namespace_and_accepts_next_turn`, `s02_drop_is_graceful_cancellation_without_uncertainty` | met | Kill namespace init, reap and wait before returning; normal completion also cleans detached descendants. |
| Missing enforcement fails closed | `fs_unavailable_enosys_and_seccomp_eperm_have_no_fallback`, `s02_fail_closed_missing_relative_and_fake_bwrap`, `s02_setup_failures_never_fall_back_to_host`, `s02_broken_bwrap_offers_only_files_and_refuses_bash`, `s02_file_layer_unavailable_disables_working_shell` | met | Bash requires usable file planning and sandbox backends; no unrestricted fallback. |
| Security tests are a required gate | `scripts/test-security-gate.sh`, `s02_probe_required`, `s02_fail_closed_missing_relative_and_fake_bwrap` | met | Both pinned CI jobs run sandbox, tool_host and doctor security targets without the opt-out. Missing bwrap is a failed prerequisite, not successful security evidence; see workflow citations below. |
| **T01** every refusal path yields zero executor calls | `t01_resolve_validate_plan_and_policy_refusals_never_execute`, `approvals_mismatch_is_commit_free_then_approve_or_deny_or_expire`, `approval_wall_clock_expiry_cannot_be_bypassed_by_delaying_the_timer` | met | Counted executor assertions cover disabled, unknown, bad schema, semantic/environment failure, protected path, policy/approval denial, expired and mismatched approval. |
| **T02** multiple valid calls sequential; association after failure/cancel; at most one branch result | `t02_three_valid_calls_execute_one_at_a_time_even_after_executor_failure`, `t02_valid_calls_wait_for_terminal_result_ack_and_keep_association_after_failure_and_cancel`, `t02_results_and_execution_stay_in_block_order_after_invalid_call` | met | Real storage oracle and shared scenario uniqueness checks; no concurrent executor effect is accepted. |
| **S01** traversal, absolute/symlink/magic/mount/hard-link paths and source replacement races; atomic writes | `s01_invalid_paths_never_reach_openat2`, `s01_symlink_final_intermediate_and_outside`, `s01_proc_root_and_fd_magic_links`, `s01_mount_crossing_dev_shm_is_required_not_skipped`, `s01_hard_links_introduced_after_planning_refused_for_all_tools`, `s01_directory_symlink_swap_between_plan_and_execution`, `s01_changed_content_is_stale_for_read_write_and_edit`, `fs_write_and_edit_are_atomic_preserve_mode_and_report_post_image` | met | Real filesystem descriptors, required separate-device /dev/shm provisioning; no successful mount-test skip. |
| **S02** real Linux isolation, host secrets/sockets/network, mount scope, environment, cleanup, missing bwrap | `s02_host_secrets_and_data_root_hidden`, `s02_network_dns_external_tcp_host_loopback_and_only_lo`, `s02_readonly_workspace_write_denied`, `s02_secret_environment_not_inherited`, `s02_background_grandchild_cleaned_after_success`, `s02_cancel_cleans_grandchildren`, `s02_timeout_cleans_grandchildren_and_term_ignoring_process`, `s02_fail_closed_missing_relative_and_fake_bwrap`, `s02_bash_approval_publishes_verified_capture` | met | Actual Sandbox and assembled SessionHandle targets run serially in the required security gate. |
| **R01** four crash points; only last has committed executor result; no re-invocation; navigation cannot evade uncertainty | `r01_four_mutation_crash_points_never_reinvoke_and_navigation_cannot_evade_uncertainty`, `r01_crashes_in_every_new_phase_never_replay_and_revoke_approvals`, `s02_process_crash_kills_sandbox_and_never_replays_intent` | met | Before intent: known not started. After intent/before spawn and after mutation/before result: uncertain mutation, SelectHead rejected ResolutionRequired. After result/before ack/notification: replay result only. Counted crash matrix plus real host SIGKILL, not handle drop. |
| **R02** concurrent turns; cancel approval, backpressured inference and execution; one durable terminal outcome; revoked approvals inert | `start_turn_is_rejected_while_busy_and_cancel_needs_the_active_turn`, `cancel_preempts_an_always_ready_stream_within_a_few_events`, `r02_cancel_approval_answers_every_pending_call_and_revokes_ticket`, `r02_executing_cancel_waits_for_executor_and_preserves_winning_success`, `executor_completion_during_cancel_commit_is_preserved_and_stale_answers_ignored` | met | Closes M2's remainder through the real policy/approval/executor-effect pipeline. Every scenario and seed checks exactly one turn_finished per started turn. Late approval rejected with zero executor calls. |
| **J03 tool-output artifact clause** missing/digest-mismatched artifacts block dependent work, never empty success | `j03_missing_or_digest_mismatched_tool_capture_blocks_reopen_not_empty_success`, `write_ahead_exact_plan_and_executor_failed_result_are_durable`, `s02_bash_approval_publishes_verified_capture`, `s02_multimegabyte_capture_uses_append_path_not_upload_limit` | met | New test damages an actual committed executor capture (absent or same length/wrong digest). Inspection reports Artifact and open refuses; diagnostic valid prefixes are not runnable sessions. |
| Elevated capability expansion | `t01_resolve_validate_plan_and_policy_refusals_never_execute` | partial | Not offered by any M3 argument, policy outcome or command. Protected access stays denied; ADR 0009, future capability decision required. |
| Subprocess networking exceptions | `s02_network_dns_external_tcp_host_loopback_and_only_lo` | partial | Always denied; constrained-egress backend and a later decision required (ADRs 0008/0012), not an M3 promise. |
| Seccomp filtering | `s02_probe_required` | partial | No seccomp filter, recorded in ADR 0008/0012. Namespace/mount enforcement is tested, not a substitute seccomp claim. |
| Process-count enforcement | `s02_fork_bomb_deadline_cleanup_and_tasksmax_when_available`, `s02_exact_shell_bytes_cwd_and_recorded_tasksmax_fallback` | partial | TasksMax only with a usable systemd user scope; otherwise explicitly reported gap. Deadline and full cleanup still required (ADR 0008). |
| Concurrent lock release/reopen during spawn | `s02_process_crash_kills_sandbox_and_never_replays_intent`, `s02_drop_is_graceful_cancellation_without_uncertainty` | partial | ADR 0012 amendment: fork/exec may briefly inherit a concurrent journal lock and cause session_locked. No lock bypass or silent effect retry; spawn tests stay in separate integration binaries. |

## Running the evidence

```sh
CARGO_NET_OFFLINE=true ./scripts/check.sh
RUNTIME_SEEDS=5000 cargo test --release --locked -p harness-core randomized -- --nocapture
cargo run --locked -p harness-core --example tool_session
```

Against base fbef55f, this step adds four ordinary core tests (301 → 305
passing core unit tests; the three pre-existing ignored manual tests remain
outside the ordinary run). Ordinary workspace tests plus doctests increase
327 → 331; the explicitly required security tests remain 39, for 366 → 370
distinct passing gate tests. Nested crash-child harness output is not counted
again. The security control-flow script and two tool_session runs are additional
gate checks. The offline Rust 1.99.1 gate and local S02/tool_host targets passed.

The gate runs ordinary tests (including all core scenarios, S01 and 512 seeds),
then explicitly runs the ignored sandbox/tool_host/doctor security targets
serially. An ignored S02 test is evidence here **because** the gate runs it.
The clean-fd launcher is CI launch hygiene, not relaxed product enforcement.

Workflow citations: `.github/workflows/ci.yml` lines 14–28 pin Rust workspace
to Ubuntu 24.04, install bwrap/load the committed profile, and invoke check.sh;
lines 30–60 pin Host sandbox security (S02), install bwrap/load the same profile,
and explicitly execute all three targets. Neither job sets
HARNESS_ALLOW_NO_SANDBOX. `scripts/check.sh` lines 10–16 has only that explicit
local opt-out. `scripts/test-security-gate.sh` runs check.sh with bwrap absent
from PATH and a counted Cargo stand-in, requiring each target's failing exit
status to propagate, with no SKIPPED output. Actual missing-backend refusal is
separately tested by `s02_fail_closed_missing_relative_and_fake_bwrap`.

`tool_session` creates a std-managed scratch workspace, uses
create_with_tools/open_with_tools with sandbox None, and runs a scripted read,
approved edit and write. A re-executed child applies an example-local file-size
resource limit based on the prior edit's intent record: the write mutation fits,
but its result append is killed by SIGXFSZ. This deterministic OS crash runs no
Rust destructors. The parent verifies the death, repairs only the inspected torn
tail, reopens the real host, verifies matches_post_image advice, and records the
user's observed result. Recovery opens no provider stream. Its final context
report prints membership/provenance and flags, deliberately omitting random
identities, host paths, elapsed times and fingerprint. The gate diffs two runs.

### Randomized coverage

Before this step random.rs already drove the real pure-core pipeline, not
synthetic tool-result commits: tool_interleavings uses registry validation,
PlanCall against fake facts, policy, durable approval/remember/deny/expiry,
counted Executor dispatch, cancellation, and Lost/Durable/Torn crash choices.
It remains at 512 seeds in the gate. Every Execute is checked against the
acknowledged intent and exact persisted plan; duplicate invocation IDs fail;
all started turns have one terminal outcome, with storage oracle and context
rebuild/replay/resync checks enabled. No extension to simulate what it already
drives was needed. Real FsWorkspace/Sandbox enforcement remains S01/S02 work.

The 5,000-seed release run passed: 2,545 planning effects, 1,928 approval
requests, 1,175 executor dispatches, 250 execution-cancellation requests,
229 restarts from Executing, 75 uncertain invocations and 187 remembered grants.
It prints these tool paths with --nocapture and requires nonzero tool-path
coverage at 512 seeds or more.

### Break-on-purpose verification

All mutations were reverted before the final gate. These are deliberate test
failures, not product defects.

| Mutation | Evidence that failed | Result |
| --- | --- | --- |
| Dispatch before intent acknowledgement | `r01_four_mutation_crash_points_never_reinvoke_and_navigation_cannot_evade_uncertainty`, `randomized_interleavings_keep_storage_and_events_consistent` | Both exit 101: Execute without durable intent. |
| Accept ended-turn approval | `r02_cancel_approval_answers_every_pending_call_and_revokes_ticket` | Exit 101: expected NotActive, got ApprovalSubmitted. |
| Dispatch overlapping executor work | `t02_valid_calls_wait_for_terminal_result_ack_and_keep_association_after_failure_and_cancel` | Exit 101: existing execution/planning slot occupied. Mutation duplicated the dispatch with a second token; it does not manufacture a second durable intent (D30). |
| Replay unresolved work on reopen | `r01_four_mutation_crash_points_never_reinvoke_and_navigation_cannot_evade_uncertainty` | Exit 101: invocation count changed from 0 to 1. Injected forbidden replay in the test driver's restart dispatcher, without widening the opaque product operation API. |
| Skip security when bwrap absent, without opt-out | `scripts/test-security-gate.sh` | Exit 1: security gate bypassed sandbox, rather than propagating required target failure. |

## Other known limits

ADR 0011: planning hashes large files without a deadline; execution checks a
cooperative deadline between kernel operations. Parent fsync failure after
rename can be infrastructure failure after mutation, not rollback.
ADR 0012: absent protected paths cannot be overmounted; post-run detection
cannot prevent transient creation/removal. Masks protect approved names, not
all hard-link aliases. Signal-like shell exits are ambiguous with 128+signal.
Hostile same-user host processes and kernel compromise are outside the threat
model. These limits are not silently expanded security claims.

Native adapters (M4), CLI chat and recovery UI (M5) remain later work.
