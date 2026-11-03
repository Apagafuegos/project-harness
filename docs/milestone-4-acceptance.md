# Milestone 4 acceptance

Milestone 4, “native providers and rolling compaction”, is complete within its
agreed scope as of **2026-10-30**. This maps every sentence-level criterion in
[roadmap §4](roadmap.md#4-native-providers-and-rolling-compaction--complete),
RFC-001 **M01–M04, C01 and C02**, and the behavioral settled decisions to gate
evidence. Anthropic live compatibility remains partial, explicitly below.

Status: **met** means covered within the agreed milestone scope; **partial**
means a recorded limitation with its owning decision or later milestone named;
**gap** means an uncovered in-scope requirement. There are no gap rows.

Test names are Rust functions unless a script/example is named. Adapter unit
evidence lives in `crates/providers/src/{openai,anthropic}/tests.rs`; durable
cross-adapter evidence lives in `crates/providers/tests/switch.rs`. Core runtime
scenarios use real `JournalStore` with the storage oracle, replay/resync and
reopen checks. Provider integration tests drive real storage through
`SessionHandle`, not fabricated journal responses. Native HTTP/SSE fixtures use
attributed loopback servers (random per-listener path and run header); unrelated
host probes cannot change evidence. No ordinary test uses real credentials.

## Acceptance matrix

| Criterion | Evidence running in the gate | Status | Note |
| --- | --- | --- | --- |
| OpenAI Responses first, current official sources | `platform_passes_every_conformance_case`, `chatgpt_codex_passes_every_conformance_case`, `platform_request_is_one_correct_stateless_body`, `chatgpt_request_moves_instructions_into_input` | met | ADR 0017 records Responses references and the pinned Codex source; both profiles run the complete public manifest. |
| Native-event fixtures and explicit OpenAI live checks | Both OpenAI conformance tests; `sanitized_recorded_codex_stream_maps_to_completed_text_and_usage`; Live checks below | met | Handwritten fixtures plus a pre-write sanitized recording. Live checks are supporting evidence, never the sole gate. |
| Native Anthropic Messages; no provider branches in orchestration | `every_conformance_case_passes_on_native_fixtures`, `request_body_is_one_correct_messages_body`, `native_tool_call_ids_stay_paired_across_a_switch_in_both_directions`, `provider_session` | met | ADR 0018: no core edit was needed for Anthropic. Router dispatches by provider instance; core owns only the typed seam, state ownership and policy. Full native fixture coverage is not an Anthropic live claim. |
| Portable diagnostics | All three native conformance manifests (`ProviderError`, `RateLimit`, `SetupFailure`, `MalformedToolJson`, `UnsupportedOption`); `non_http_transport_errors_keep_their_original_code_and_message`, `error_events_are_classified_without_echoing_provider_text` | met | Structured categories, nullable fields, retry guidance; no provider-body echo. |
| Opaque retention and safe switching inside existing sessions | `switching_back_replays_only_the_owning_providers_state_and_no_continuation`, `openai_to_anthropic_is_rejected_by_default_and_omits_every_entry`, `anthropic_to_openai_is_rejected_by_default_for_signed_thinking`, both `m04_*_multimodal_and_native_replay_survive_reopen` tests | met | Reject writes nothing; explicit omit reports every optional loss; required media cannot be omitted. Switch-back restores only the owner's state through reopen. |
| Automatic and manual rolling summaries behind the strategy seam | `c02_automatic_retains_configured_recent_turns`, `exact_request_has_independent_model_no_tools_or_native_state`, `a_summary_from_the_other_provider_is_plain_text_for_the_next_one`, `provider_session` | met | `CompactionStrategy` / `RollingSummary`; independent summary model; summaries are plain text to either adapter. |
| Read-only previews, source manifests, recent turns, inspection diffs | `c02_manual_preview_leaves_journal_and_workspace_unchanged`, `c02_automatic_references_exact_source_ids`, `c02_automatic_retains_configured_recent_turns`, `c02_stale_source_manifest_substitution`, `golden_preview_reports_and_storage_acceptance` | met | Exact source IDs/checksums, retention three tested, hypothetical after-report uses the same builder. Diff is also pinned by `compaction-preview-diff.json` in the fixture preview test. |
| C02 and explicit budget failure without rewriting history | `c02_original_entries_remain_readable_byte_identical`, `c02_budget_fits_or_failure_is_explicit`, `oversized_protected_content_rejects_after_one_compaction_attempt`, `mid_turn_overflow_fails_without_compaction_inside_turn` | met | Original journal prefix/entries/checksums unchanged. Oversized protected content explicitly fails; no truncation or compaction within an active turn. |
| Ordinary CI offline; live inference separate and explicit | `scripts/test-offline-gate.sh`, `sentinel_credential_never_printed_on_real_auth_failure`, complete gate inside `scripts/with-offline-network.sh` | met | External network denied during CI validation; Cargo offline. Provisioning is explicitly separate, not falsely claimed offline. See exact boundary below. |
| **M01** interleaved call IDs/JSON in block order; dispatch only after terminal successful tool calls | Three native conformance manifests, `InterleavedTools` and `OneTool`; `m01_openai_ready_call_waits_for_terminal_success`, `m01_anthropic_ready_call_waits_for_terminal_success` | met | A wrapper pauses each real adapter's terminal event after ready calls. No durable proposal/result or dependent inference before release; the valid read really executes afterwards. Interleaved reconstruction independently checks IDs, block indices and parsed JSON. |
| **M02** setup/malformed/EOF/cancel/output-limit/refusal do not authorize partial proposals; nullable usage | Three native manifests (`SetupFailure`, `MalformedToolJson`, `CancelSetup`, `CancelStreaming`, `OutputLimit`, `Refusal`, `Text`); `out_of_order_and_invalid_data_are_malformed_streams`, `structural_violations_are_malformed_streams`, core `malformed_or_incomplete_tool_calls_are_never_answered` | met | Native mappers reject EOF without terminal; core independently discards ineligible calls. Text fixtures omit usage. |
| **M03** drop/deadline/backpressure cleanup; cumulative snapshots replace; second adapter without runtime fork | Three native manifests (`Drop`, `Deadline`, cancellation, `Usage`); `backpressured_cancel_deadline_drop_release`, `response_bound_and_conservative_queue_backpressure`, `m03_openai_cumulative_usage_is_not_summed`, `m03_anthropic_cumulative_usage_is_not_summed` | met | Native manifests verify snapshots, durable tests verify final stored counts (output 9, not 11/10). Shared transport bounds all producers and closes while backpressured. ADR 0018 records unchanged core. |
| **M04** multimodal/opaque round trip, reject/omit, every portability loss, continuation invalidation | Three native manifests (`NativeState`, `Request`); both `m04_*_multimodal_and_native_replay_survive_reopen`; `switching_back_replays_only_the_owning_providers_state_and_no_continuation`; core `native_replay_survives_crash_reopen_and_is_omitted_after_model_switch` | met | Image artifact bytes and actual native image encoding survive reopen with exact native replay. Both directions refuse then omit; foreign payload absent on wire. Neither real adapter emits continuations (decision 15); core test additionally exercises a nonempty continuation through crash/reopen/switch. |
| **C01** immutable reproducible projection/report, branch isolation, estimates vs actual usage, unknown budgets, whole tool groups | `fixture_reports_are_identical_after_reopening_and_across_data_roots`, `golden_branching_reports_only_the_selected_branch`, `reported_usage_is_labelled_and_never_added_to_estimates`, `budget_is_unknown_without_a_limit_and_flags_overflow_and_compaction`, `answered_calls_are_included_with_their_results_directly_after`, CLI `budget_failure_is_1_without_opening_provider` | met | All fixture goldens and context CLI smoke run in the gate; provider usage is not a token estimate. |
| **C02** read-only preview and stale refusal; rolling exact sources, recent turns, ancestor summaries, original entries, budget, no projection regeneration | `c02_manual_preview_leaves_journal_and_workspace_unchanged`, `c02_stale_head`, `c02_stale_sequence`, `c02_stale_model`, `c02_stale_source_manifest_substitution`, `c02_stale_policy`, `c02_automatic_references_exact_source_ids`, `c02_automatic_retains_configured_recent_turns`, `c02_automatic_supersedes_only_ancestor_summaries`, `c02_original_entries_remain_readable_byte_identical`, `c02_budget_fits_or_failure_is_explicit`, `c02_no_summary_regenerated_during_pure_projection_or_prepare` | met | ADR 0019 defines the implemented scope; crash/epoch/cancellation and randomized coverage remain in the gate. Pin clause separately partial below. |
| Decision **8**: no automatic inference retry | Both adapters' `a_rate_limit_is_reported_once_without_a_retry`; all three manifests' `RateLimit`; `automatic_single_attempt_per_trigger_and_failure_never_blocks` | met | HTTP request count stays one; retry delay is diagnostic, not permission to replay. Summary failure never creates a retry loop. |
| Decision **13**: credentials never leak | `http_error_taxonomy_retry_and_redaction`, `captured_secret_outputs`, `codex_read_only_rereads_expiry_and_redaction`, OpenAI `no_secret_reaches_errors_debug_or_fixtures`, Anthropic `the_api_key_is_sent_as_x_api_key_and_never_rendered`, CLI `authentication_failure_is_3_with_codex_hint_and_secret_free_storage`, `sentinel_credential_never_printed_on_real_auth_failure` | met | Sentinel checks cover output, Debug, diagnostics and session/artifact bytes. Real login is read-only only in opt-in runs; no Anthropic subscription source. |
| Decision **14**: stateless OpenAI | `platform_request_is_one_correct_stateless_body`, `chatgpt_request_moves_instructions_into_input`, `same_owner_reasoning_state_is_replayed_and_a_foreign_owner_is_not`, both OpenAI manifests | met | `store:false`, no `previous_response_id`, encrypted per-item replay; no response continuation on either profile. |
| Decision **16**: native payload Debug redaction | `provider_state_debug_redacts_payload_and_keeps_other_fields`, `native_continuation_debug_redacts_payload_and_keeps_other_fields`, `finished_debug_redacts_a_nested_continuation` | met | Sentinel absent, byte length present, including nested continuation. |
| `ask` approval policy | `tool_proposal_approved_via_fake_terminal`, `tool_proposal_denied_via_fake_terminal`, `no_terminal_denies_every_call_even_approve_read_only`, `approve_read_only_uses_registry_but_never_auto_approves_mutation`, `stdin_dash_is_prompt_not_approval_input`, `argument_summary_is_bounded_single_line_and_terminal_errors_deny` | met | Only literal `y`; no terminal always denies. Auto-read requires terminal and exact trusted-filesystem read effects. CLI submits approvals, never executes or remembers grants. |
| Anthropic live streaming/usage/tool/error/cancellation | `every_conformance_case_passes_on_native_fixtures`; recorded live skip below | partial | Missing one run with `ANTHROPIC_API_KEY` (ADR 0018); no consumer OAuth substitution. |
| Cross-provider live switching | All real-adapter switch integration tests; recorded same-provider live pass below | partial | Live OpenAI → Anthropic → OpenAI needs `ANTHROPIC_API_KEY` (ADR 0020 amendment). |
| Continuation conversion | `switch_required_media_rejected_under_both_policies`, `switch_required_tools_rejected_and_clean_switch_policies` | implemented in M7 | ADR 0015 amendment: explicit ConvertSupported converts displayable content in the projection only; opaque state is never translated. |
| C02 pins | `c02_pins_not_applicable_until_pins_exist` (historical) | partial | **Closed by [Milestone 6](milestone-6-acceptance.md).** Pins did not exist in M4, so pin exclusion was not applicable; M6 implements whole-turn pins/exclusions and maps the pin clause to `c02_automatic_summaries_respect_pins`, `storage_refuses_a_compaction_covering_a_pinned_turn` and `projection_order_is_summary_then_pinned_turn_then_retained`. This row records M4's past validation claim; it is not rewritten. |
| Exact token counts and unsupported native features | Catalog/request capability tests; `budget_is_unknown_without_a_limit_and_flags_overflow_and_compaction` | partial | Heuristic estimates, not real tokenizers. Hosted tools, audio/files and other adapter-specific exclusions remain in ADRs 0017/0018; extensions require later decisions. Codex chooses its own wire output budget. |
| Preserved-thinking transformation inspection | `preserved_thinking_is_requested_behind_a_summary_and_a_switch`, `preserved_thinking_beta_header_follows_the_body_predicate` | partial | ADR 0018 amendment requests `drop_block` when replaying; API transformations are not parsed/reported. State remains immutable in the journal; richer inspection is a later decision/M5 UI. |

## Running the evidence

```sh
cargo fetch --locked                 # provisioning, may use network
CARGO_NET_OFFLINE=true ./scripts/check.sh
# Stronger external-network enforcement, used for the acceptance run and CI:
./scripts/with-offline-network.sh ./scripts/with-clean-fds.sh \
  env CARGO_NET_OFFLINE=true ./scripts/check.sh
cargo run --offline --locked --example provider_session
```

The complete Rust **1.99.1** acceptance gate passed in a fresh network-only
namespace with loopback up and **no external routes**, using a **3,600,000 ms**
timeout. No sandbox opt-out was set. Formatting was explicitly run with
`cargo fmt --all`. `rustfmt.toml` was the first commit, pinning style edition 2024.

Against base **0d8e2e6**, this step adds **six Rust tests**, all ordinary provider
`--test switch` integration tests: two M01, two M03 and two M04 tests named
above. That target grows **6 → 12**. Ordinary workspace tests plus doctests grow
**532 → 538**; required security tests stay **39**, giving **571 → 577 distinct
passing gate tests**. The pre-existing `scripted_session` example unit test adds
one further passing example test (572 → 578 if counted). Re-running core with
`conformance` is not counted twice, nor is nested tool-host crash-child output.
The new offline guard and the two provider-example runs/diff are additional
script checks. The existing switch-back test is strengthened with two reopens;
the CLI auth test is corrected without adding/removing a test.

Each native conformance entry point runs all **17 CaseKind** scenarios: OpenAI
Platform, OpenAI ChatGPT Codex, and Anthropic Messages, **51 fixture cases**
within three Rust tests. Every M-row uses those real-adapter suites; integration
and core evidence adds durability, dispatch and nonempty continuation semantics
which a stateless single adapter fixture cannot establish alone.

`provider_session` uses the real Router and both adapters against attributed
loopback SSE servers. It runs an OpenAI tool turn (honestly denied
`tools_unavailable`, because the tour installs no workspace authority), then two
turns needed by the minimum recent-turn window, manual preview/commit on the
independent Anthropic summary model, a rejected switch, an explicit omit switch
with portability metadata, an Anthropic turn and final context report
membership/provenance/estimates. Random entry IDs, host paths and the
identity-dependent fingerprint are intentionally omitted from printed reports;
actual owner/schema/block/byte counts remain. The gate builds outside smoke
bounds and runs the binary twice under separate 300-second timeouts, diffing
bytes (`scripts/check.sh` lines 28–30, 72–79), following the direct-binary pattern.

## Offline boundary and regression found

“Offline” means **no external network after provisioning**, not no loopback
sockets. It cannot mean that a fresh CI machine installs Rust/apt/dependencies
without downloads. `.github/workflows/ci.yml` lines 20–32 and 44–52 explicitly
separate checkout/toolchain/apt/locked dependency fetch from validation. Global
`CARGO_NET_OFFLINE` is true (lines 10–12); only the two bootstrap `cargo fetch`
steps override it. `scripts/check.sh` lines 7–10 exports offline mode and runs
`scripts/test-offline-gate.sh`; tests, Clippy, docs and example builds then use
locked cached dependencies. No credentials are needed.

Workspace validation enters `with-offline-network.sh` (workflow line 35); all
three S02 targets in the second job do too (lines 66–68). The launcher (lines
5–16) uses noninteractive sudo only to create a network namespace and bring up
`lo`, then `setpriv` restores the caller's UID/GID/groups **before** validation.
It intentionally preserves the host user namespace: a Bubblewrap outer user
namespace changes UID maps and invalidates a doctor's host-map-negative test.
There is no network fallback. The direct `check.sh` command enforces Cargo
and entry-point separation; the CI/acceptance launcher additionally enforces the
OS network boundary. CI provisioning, including checkout/actions/artifact
upload, is explicitly outside that boundary.

The first isolated run exposed an existing accidental live test:
`sentinel_credential_never_printed_on_real_auth_failure` supplied a valid-looking
sentinel key to the real platform endpoint and expected HTTP authentication
failure. Isolated, it returned `transport` (exit 1), not authentication (exit 3).
The test now uses a forbidden header byte so real adapter construction fails
locally, and removes any ambient Anthropic key. The sentinel still must not
appear in either output stream or any session file. Fix commit **233c69a** is
test-only; no production defect or retry was introduced.

Repository search confirms the literal `scripts/live-check.sh` reference occurs
only in documentation (including example doc-comments and a manifest comment)
and that script's own usage text, never an executable gate/workflow invocation. The offline guard rejects live entry points and downloader commands;
the namespace is the stronger defense even against an unrecognized future
network call. The break-on-purpose live insertion was detected **before** any
inference, so it did not consume an extra live run.

## Live checks — 2026-10-30, opt-in only

Each requested command was invoked **once**, sequentially, with no retries.
These are the complete sanitized check summaries, not raw provider output.
No quota/rate-limit failure occurred. The source script prints no credentials,
account/request/response IDs or session content.

### `scripts/live-check.sh openai` — exit 0

```text
live openai: auth=codex model=gpt-6.1-sol context=500000 max_output=unknown
text streaming: pass (4 chars)
usage: pass (input=31 output=5 cached=0 reasoning=0)
tool round trip: pass
invalid model: pass (InvalidRequest)
cancellation: pass
elapsed: 6s
summary: pass
```

### `scripts/live-check.sh ask` — exit 0

```text
live ask: auth=codex
exit: 0
stdout contains pong: pass
summary: pass
```

### `scripts/live-check.sh switch` — exit 0, cross-provider part partial

```text
openai turn 1: pass (completed)
cross-provider: skipped (no ANTHROPIC_API_KEY)
same provider -> gpt-6-astra: pass (omitted 0 entries)
alternate model turn: pass (completed)
same provider back: pass (omitted 0 entries)
original model turn: pass (completed)
openai tool round trip: pass (completed)
openai tool round trip: tool result pass
same-provider: pass
summary: pass
```

### `scripts/live-check.sh anthropic` — exit 77, partial

```text
live anthropic: skipped (no ANTHROPIC_API_KEY)
```

This is neither a live pass nor a product failure. A sanctioned API key and one
explicit live run remain necessary; fixtures cannot establish live compatibility.

## Break-on-purpose verification

All mutations compiled or ran as shell, produced the intended assertion/failure,
and were individually reverted before the complete passing gate. None is a
production fix. The core product files have no remaining changes.

| Temporary violation | Named evidence that failed | Result |
| --- | --- | --- |
| Promote ready call to completed/tool_calls before native terminal | `m01_openai_ready_call_waits_for_terminal_success`, `m01_anthropic_ready_call_waits_for_terminal_success` | Cargo 101; durable proposal appeared before terminal success. |
| Sum cumulative output snapshots | `m03_openai_cumulative_usage_is_not_summed`, `m03_anthropic_cumulative_usage_is_not_summed` | Cargo 101; stored 11/10 instead of 9. |
| Carry latest historical continuation despite owner/model switch | `native_replay_survives_crash_reopen_and_is_omitted_after_model_switch` | Cargo 101; expected portability-loss classification failed. The builder's invariants also defend the projection. |
| Rewrite a covered original journal entry after compaction | `c02_original_entries_remain_readable_byte_identical` | Cargo 101; byte-identical original-prefix assertion failed. Test-local disk rewrite models the forbidden persistence effect, without weakening the append-only API. |
| Insert live inference into check.sh | `scripts/test-offline-gate.sh` | Exit 1; rejected the inserted live entry point, without executing it. |

## Other known limits

ADRs 0014–0020 and amendments are linked from the documentation index. Native
continuation is optional in conformance; neither stateless adapter manufactures
one. Opaque artifacts remain sensitive on disk, not encrypted by the harness.
No automatic retry, refresh-token rotation, hidden fallback, history rewrite or
provider-specific core fork is authorized by this milestone. Preserved-thinking
controls may cause the API to drop replayed blocks after a prefix edit; errors
remain explicit and are never retried. CLI recovery UI and Ratatui are M5;
pins and context experiments are M6. Previously recorded M3 enforcement limits
are unchanged. No crate, dependency feature, public API or storage rule changed
in this acceptance step.
