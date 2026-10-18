//! One scenario per lifecycle rule. Each also runs through `scenario`, which
//! repeats it, replays it and checks resync, reopening and turn outcomes.

use super::*;
use crate::runtime::tests::scenario;

fn started(d: &mut Driver) {
    assert!(d.provider(ProviderEvent::Started).is_empty());
}
fn say(d: &mut Driver, block_index: u32, text: &str) -> Vec<Effect> {
    d.provider(ProviderEvent::TextDelta {
        block_index,
        text: text.into(),
    })
}
fn finish(d: &mut Driver, outcome: ProviderOutcome, reason: Option<FinishReason>) -> Vec<Effect> {
    d.provider(ProviderEvent::Finished {
        outcome,
        finish_reason: reason,
        error: None,
    })
}
fn stop(d: &mut Driver) -> Vec<Effect> {
    finish(d, ProviderOutcome::Completed, Some(FinishReason::Stop))
}
fn call(d: &mut Driver, block_index: u32, name: &str) {
    d.provider(ProviderEvent::ToolCallStarted {
        block_index,
        name: name.into(),
        provider_call_id: Some(format!("native-{block_index}")),
    });
}
fn args(d: &mut Driver, block_index: u32, fragment: &str) {
    d.provider(ProviderEvent::ToolArgumentsDelta {
        block_index,
        fragment: fragment.into(),
    });
}
fn ready(d: &mut Driver, block_index: u32) -> Vec<Effect> {
    d.provider(ProviderEvent::ToolCallReady { block_index })
}
fn usage(input: Option<u64>, output: Option<u64>) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: None,
        reasoning_tokens: None,
    }
}

/// Start a turn and settle until its inference runs.
fn begin(d: &mut Driver, prompt: &str) -> RequestId {
    let (request, _) = d.start(prompt);
    d.settle();
    assert!(matches!(d.accepted(request), Accepted::TurnStarted { .. }));
    started(d);
    request
}

/// A whole turn that answers `reply` and stops.
fn plain_turn(d: &mut Driver, prompt: &str, reply: &str) {
    begin(d, prompt);
    say(d, 0, reply);
    stop(d);
    d.settle();
}

/// Two interleaved tool calls, finished with `tool_calls`.
fn two_calls(d: &mut Driver) {
    call(d, 1, "read");
    call(d, 3, "write");
    args(d, 3, r#"{"path":"b","#);
    args(d, 1, r#"{"path""#);
    args(d, 3, r#""text":"x"}"#);
    args(d, 1, r#":"a"}"#);
    ready(d, 3);
    ready(d, 1);
}

fn persists(effects: &[Effect]) -> usize {
    effects
        .iter()
        .filter(|e| matches!(e, Effect::Persist { .. }))
        .count()
}

fn has(effects: &[Effect], f: impl Fn(&Effect) -> bool) -> bool {
    effects.iter().any(f)
}

/// The assistant responses on the current branch, oldest first.
fn responses(d: &Driver) -> Vec<AssistantResponse> {
    let snapshot = d.runtime.snapshot();
    snapshot
        .ancestry(snapshot.head())
        .unwrap()
        .into_iter()
        .filter_map(|stored| match &stored.entry.payload {
            EntryPayload::AssistantResponse(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

fn results(d: &Driver) -> Vec<ToolResult> {
    let snapshot = d.runtime.snapshot();
    snapshot
        .ancestry(snapshot.head())
        .unwrap()
        .into_iter()
        .filter_map(|stored| match &stored.entry.payload {
            EntryPayload::ToolResult(r) => Some(r.result.clone()),
            _ => None,
        })
        .collect()
}

fn outcome(d: &Driver, turn: &TurnId) -> Option<TurnOutcome> {
    d.runtime.snapshot().turn(turn).unwrap().outcome
}

fn turn_of(d: &Driver, request: RequestId) -> TurnId {
    match d.accepted(request) {
        Accepted::TurnStarted { turn_id, .. } => turn_id.clone(),
        other => panic!("not a turn start: {other:?}"),
    }
}

fn texts(response: &AssistantResponse) -> Vec<String> {
    response
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

const OPEN: &str = "user turn_started idle>preparing";
const INFER: &str = "preparing>inferring";
const STOP: &str = "assistant(completed/stop) inferring>processing finished(completed)";

macro_rules! scenarios {
    ($($name:ident $(with $config:expr)?),+ $(,)?) => {$(
        #[test]
        fn $name() {
            let config = [RuntimeConfig::default() $(, $config)?].into_iter().last().unwrap();
            scenario(config, bodies::$name);
        }
    )+};
}

fn limited(max_inferences_per_turn: u32, max_partial_bytes: usize) -> RuntimeConfig {
    RuntimeConfig {
        max_inferences_per_turn,
        max_partial_bytes,
        ..RuntimeConfig::default()
    }
}

scenarios!(
    plain_turn_ends_in_stop,
    tool_calls_answered_unavailable_then_second_inference_stops,
    inference_limit_fails_the_turn with limited(3, 4096),
    cancel_during_inference_stream_finishes_first,
    cancel_during_inference_grace_timer_fires_first,
    cancel_during_inference_late_events_are_ignored,
    cancel_during_in_flight_commit_acts_after_ack,
    cancel_before_inference_finishes_cancelled,
    start_turn_is_rejected_while_busy_and_cancel_needs_the_active_turn,
    commands_are_rejected_while_recovery_is_required,
    command_retry_after_ack_returns_the_original_receipt,
    command_retry_while_in_flight_attaches_to_the_opening_commit,
    turn_id_differs_from_command_id_and_the_receipt_links_them,
    stream_ending_without_finished_is_interrupted,
    partial_response_limit_interrupts_the_stream with limited(32, 1000),
    malformed_or_incomplete_tool_calls_are_never_answered,
    io_failure_before_write_faults_with_only_rejections_after,
    io_failure_after_sync_leaves_the_unacknowledged_commit_durable,
    rejected_persist_faults_the_runtime,
    select_head_and_switch_model_only_while_idle,
    provider_outcomes_map_to_turn_outcomes,
    stale_streams_and_timers_are_dropped_with_diagnostics,
);

mod bodies {
    use super::*;

    pub fn plain_turn_ends_in_stop(d: &mut Driver) {
        let request = begin(d, "hi");
        let effects = say(d, 0, "Hel");
        assert!(has(&effects, |e| matches!(
            e,
            Effect::Emit(RuntimeEvent {
                kind: EventKind::TextDelta { .. },
                ..
            })
        )));
        assert_eq!(persists(&effects), 0, "deltas are never persisted");
        say(d, 0, "lo");
        d.provider(ProviderEvent::UsageUpdated {
            usage: usage(Some(10), Some(1)),
        });
        d.provider(ProviderEvent::UsageUpdated {
            usage: usage(Some(10), Some(2)),
        });
        let partial = d.runtime.view().partial.unwrap();
        assert_eq!(partial.blocks[&0], PartialBlock::Text("Hello".into()));
        let effects = stop(d);
        assert_eq!(persists(&effects), 1);
        // The terminal event follows only the ack.
        assert!(!has(&effects, |e| matches!(
            e,
            Effect::Emit(RuntimeEvent {
                kind: EventKind::TurnFinished { .. },
                ..
            })
        )));
        d.settle();
        assert_eq!(shapes(d), [OPEN, INFER, STOP]);
        let response = &responses(d)[0];
        assert_eq!(texts(response), ["Hello"]);
        // Usage replaces earlier figures; it is never summed.
        assert_eq!(response.usage, Some(usage(Some(10), Some(2))));
        let turn = turn_of(d, request);
        assert_eq!(outcome(d, &turn), Some(TurnOutcome::Completed));
        assert_eq!(d.runtime.phase(), Phase::Durable(RuntimeState::Idle));
        assert!(d.runtime.view().partial.is_none());
    }

    pub fn tool_calls_answered_unavailable_then_second_inference_stops(d: &mut Driver) {
        begin(d, "use tools");
        say(d, 0, "Let me look.");
        two_calls(d);
        // Nothing is answered before a successful `tool_calls` finish.
        assert_eq!(shapes(d), [OPEN, INFER]);
        let first = d.inference;
        finish(d, ProviderOutcome::Completed, Some(FinishReason::ToolCalls));
        d.settle();
        assert_ne!(d.inference, first, "a second inference started");
        started(d);
        say(d, 0, "Tools are unavailable.");
        stop(d);
        d.settle();
        assert_eq!(
            shapes(d),
            [
                OPEN,
                INFER,
                "assistant(completed/toolcalls) inferring>processing",
                "result(denied) result(denied) processing>continuing continuing>preparing",
                INFER,
                STOP,
            ]
        );
        let proposal = &responses(d)[0];
        let calls: Vec<_> = proposal
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall {
                    call_id,
                    name,
                    arguments,
                    provider_call_id,
                } => Some((
                    call_id.clone(),
                    name.clone(),
                    arguments.clone(),
                    provider_call_id.clone(),
                )),
                _ => None,
            })
            .collect();
        // Block order, reassembled arguments and native IDs survive interleaving.
        assert_eq!(calls[0].1, "read");
        assert_eq!(
            calls[0].2,
            serde_json::json!({"path":"a"}).as_object().unwrap().clone()
        );
        assert_eq!(calls[0].3.as_deref(), Some("native-1"));
        assert_eq!(calls[1].1, "write");
        assert_eq!(
            calls[1].2,
            serde_json::json!({"path":"b","text":"x"})
                .as_object()
                .unwrap()
                .clone()
        );
        let results = results(d);
        assert_eq!(results.len(), 2);
        for (result, call) in results.iter().zip(&calls) {
            assert_eq!(result.call.call_id, call.0, "answered in branch order");
            assert_eq!(result.status, ToolStatus::Denied);
            assert_eq!(result.invocation_id, None);
            assert_eq!(result.metadata.provenance, Provenance::Runtime);
            assert!(
                matches!(&result.content[0], ContentBlock::Text { text } if text.starts_with("tools_unavailable"))
            );
        }
    }

    pub fn inference_limit_fails_the_turn(d: &mut Driver) {
        let max = d.runtime.config().max_inferences_per_turn;
        let request = begin(d, "loop");
        for round in 0..max {
            if round > 0 {
                started(d);
            }
            call(d, 0, "read");
            args(d, 0, "{}");
            ready(d, 0);
            finish(d, ProviderOutcome::Completed, Some(FinishReason::ToolCalls));
            d.settle();
        }
        let shapes = shapes(d);
        assert_eq!(
            shapes.last().unwrap(),
            "result(denied) processing>continuing finished(failed, inference_limit)"
        );
        assert_eq!(d.started.len(), max as usize);
        let turn = turn_of(d, request);
        assert_eq!(outcome(d, &turn), Some(TurnOutcome::Failed));
    }

    pub fn cancel_during_inference_stream_finishes_first(d: &mut Driver) {
        let request = begin(d, "long answer");
        say(d, 0, "partial");
        let cancel = d.cancel();
        let effects = d.segment.effects.clone();
        let tail = &effects[effects.len() - 3..];
        assert!(
            matches!(tail[0], Effect::CancelInference { .. }),
            "cancel the stream at once"
        );
        assert!(matches!(tail[1], Effect::SetTimer { .. }));
        assert!(
            matches!(&tail[2], Effect::Persist { commit, .. } if shape(commit) == "inferring>cancelling")
        );
        assert!(d.reply(cancel).is_none(), "replied after the ack");
        // The provider confirms while the cancelling commit is still in flight.
        let effects = stop(d);
        assert_eq!(persists(&effects), 0, "one commit in flight");
        d.settle();
        assert!(matches!(d.accepted(cancel), Accepted::Cancelling { .. }));
        assert!(has(&d.segment.effects, |e| matches!(
            e,
            Effect::CancelTimer { .. }
        )));
        assert_eq!(
            shapes(d),
            [
                OPEN,
                INFER,
                "inferring>cancelling",
                "assistant(cancelled) finished(cancelled)"
            ]
        );
        assert_eq!(texts(&responses(d)[0]), ["partial"]);
        assert_eq!(
            outcome(d, &turn_of(d, request)),
            Some(TurnOutcome::Cancelled)
        );
    }

    pub fn cancel_during_inference_grace_timer_fires_first(d: &mut Driver) {
        let request = begin(d, "long answer");
        say(d, 0, "so far");
        let cancel = d.cancel();
        d.settle();
        assert!(matches!(d.accepted(cancel), Accepted::Cancelling { .. }));
        assert_eq!(d.runtime.phase(), Phase::Durable(RuntimeState::Cancelling));
        let timer = *d.timers.iter().next().unwrap();
        let effects = d.fire(timer);
        assert_eq!(persists(&effects), 1);
        d.settle();
        assert_eq!(
            shapes(d).last().unwrap(),
            "assistant(cancelled) finished(cancelled)"
        );
        assert_eq!(texts(&responses(d)[0]), ["so far"]);
        assert_eq!(
            outcome(d, &turn_of(d, request)),
            Some(TurnOutcome::Cancelled)
        );
        // The stream finishes after the grace period: ignored.
        let effects = stop(d);
        assert!(matches!(
            &effects[..],
            [Effect::Emit(RuntimeEvent {
                kind: EventKind::Diagnostic(RuntimeDiagnostic::StaleProviderEvent { .. }),
                ..
            })]
        ));
        assert!(d.ended().is_empty());
    }

    pub fn cancel_during_inference_late_events_are_ignored(d: &mut Driver) {
        begin(d, "long answer");
        say(d, 0, "kept");
        // A ready proposal in a cancelled response is never answered.
        call(d, 1, "read");
        args(d, 1, "{}");
        ready(d, 1);
        d.cancel();
        // Deltas after `CancelInference` are dropped, with or without a commit
        // in flight, and never reach the persisted response.
        let effects = say(d, 0, " dropped");
        assert!(has(&effects, |e| matches!(
            e,
            Effect::Emit(RuntimeEvent {
                kind: EventKind::Diagnostic(_),
                ..
            })
        )));
        d.settle();
        let effects = say(d, 1, "dropped too");
        assert_eq!(persists(&effects), 0);
        d.provider(ProviderEvent::UsageUpdated {
            usage: usage(Some(1), Some(1)),
        });
        let effects = d.ended();
        assert_eq!(persists(&effects), 1);
        d.settle();
        assert_eq!(responses(d)[0].content, text("kept"));
        assert_eq!(responses(d)[0].usage, None);
        assert!(results(d).is_empty());
        let before = d.segment.effects.len();
        stop(d);
        say(d, 0, "late");
        d.ended();
        for timer in [TimerId(1), TimerId(99)] {
            let effects = d.fire(timer);
            assert!(matches!(
                &effects[..],
                [Effect::Emit(RuntimeEvent {
                    kind: EventKind::Diagnostic(RuntimeDiagnostic::StaleTimer { .. }),
                    ..
                })]
            ));
        }
        assert!(d.segment.effects[before..].iter().all(|e| matches!(
            e,
            Effect::Emit(RuntimeEvent {
                kind: EventKind::Diagnostic(_),
                ..
            })
        )));
        assert_eq!(d.runtime.phase(), Phase::Durable(RuntimeState::Idle));
    }

    pub fn cancel_during_in_flight_commit_acts_after_ack(d: &mut Driver) {
        // A cancel while the tool-call response commit is in flight answers
        // the unstarted calls as cancelled after the ack.
        let request = begin(d, "use tools");
        two_calls(d);
        finish(d, ProviderOutcome::Completed, Some(FinishReason::ToolCalls));
        let cancel = d.cancel();
        assert!(d.reply(cancel).is_none());
        assert!(!has(&d.segment.effects, |e| matches!(
            e,
            Effect::CancelInference { .. }
        )));
        d.settle();
        assert!(matches!(d.accepted(cancel), Accepted::Cancelling { .. }));
        assert_eq!(
            shapes(d)[2..],
            [
                "assistant(completed/toolcalls) inferring>processing",
                "result(cancelled) result(cancelled) processing>cancelling finished(cancelled)",
            ]
        );
        assert_eq!(
            outcome(d, &turn_of(d, request)),
            Some(TurnOutcome::Cancelled)
        );

        // A cancel while the finishing commit is in flight is too late.
        begin(d, "answer");
        stop(d);
        let late = d.cancel();
        assert!(d.reply(late).is_none());
        d.settle();
        assert_eq!(d.rejected(late), Rejection::NotActive);
        assert_eq!(shapes(d).last().unwrap(), STOP);
    }

    pub fn cancel_before_inference_finishes_cancelled(d: &mut Driver) {
        // Cancelled while `Preparing -> Inferring` is in flight: no stream starts.
        let (request, _) = d.start("first");
        d.complete();
        assert!(d.pending.is_some(), "preparing>inferring in flight");
        let cancel = d.cancel();
        d.settle();
        assert!(matches!(d.accepted(cancel), Accepted::Cancelling { .. }));
        assert!(d.started.is_empty(), "no inference started");
        assert_eq!(
            shapes(d)[1..],
            [INFER, "inferring>cancelling finished(cancelled)"]
        );
        assert_eq!(
            outcome(d, &turn_of(d, request)),
            Some(TurnOutcome::Cancelled)
        );

        // Cancelled while results return the turn to `Preparing`.
        let request = begin(d, "second");
        call(d, 0, "read");
        args(d, 0, "{}");
        ready(d, 0);
        finish(d, ProviderOutcome::Completed, Some(FinishReason::ToolCalls));
        d.complete();
        assert!(d.pending.is_some(), "results commit in flight");
        let cancel = d.cancel();
        d.settle();
        assert!(matches!(d.accepted(cancel), Accepted::Cancelling { .. }));
        assert_eq!(
            shapes(d)[6..],
            [
                "result(denied) processing>continuing continuing>preparing",
                "preparing>cancelling finished(cancelled)",
            ]
        );
        assert_eq!(
            outcome(d, &turn_of(d, request)),
            Some(TurnOutcome::Cancelled)
        );
    }

    pub fn start_turn_is_rejected_while_busy_and_cancel_needs_the_active_turn(d: &mut Driver) {
        let idle = d.command(Command::Cancel {
            turn_id: TurnId::new(),
        });
        assert_eq!(d.rejected(idle), Rejection::NotActive);
        let (first, _) = d.start("first");
        // While the opening commit is in flight.
        let (second, _) = d.start("second");
        assert_eq!(d.rejected(second), Rejection::Busy);
        d.settle();
        let turn = turn_of(d, first);
        // While the turn is active.
        let (third, _) = d.start("third");
        assert_eq!(d.rejected(third), Rejection::Busy);
        let other = d.command(Command::Cancel {
            turn_id: TurnId::new(),
        });
        assert_eq!(d.rejected(other), Rejection::NotActive);
        started(d);
        stop(d);
        d.settle();
        let finished = d.command(Command::Cancel { turn_id: turn });
        assert_eq!(d.rejected(finished), Rejection::NotActive);
        // Rejections never queue work.
        assert_eq!(shapes(d), [OPEN, INFER, STOP]);
    }

    pub fn commands_are_rejected_while_recovery_is_required(d: &mut Driver) {
        let (request, id) = d.start("interrupted");
        d.settle();
        let turn = turn_of(d, request);
        started(d);
        say(d, 0, "half");
        d.reopen();
        assert_eq!(d.runtime.phase(), Phase::RecoveryRequired);
        assert_eq!(d.runtime.view().event_seq, 0, "construction emits nothing");
        // A retry of the accepted command still learns its durable outcome.
        let retry = d.command(Command::StartTurn {
            command_id: id,
            content: text("interrupted"),
        });
        assert!(
            matches!(d.accepted(retry), Accepted::TurnStarted { turn_id, .. } if *turn_id == turn)
        );
        for command in [
            Command::StartTurn {
                command_id: d.new_command_id(),
                content: text("new"),
            },
            Command::Cancel { turn_id: turn },
            Command::SelectHead { head: None },
            Command::SwitchModel {
                model: ModelRef {
                    provider_instance: "mock".into(),
                    model: "other".into(),
                },
            },
        ] {
            let request = d.command(command);
            assert_eq!(d.rejected(request), Rejection::RecoveryRequired);
        }
        let stale = d.step(Input::Provider {
            inference: InferenceId(1),
            event: ProviderEvent::Started,
        });
        assert_eq!(persists(&stale), 0);
        assert_eq!(shapes(d), [OPEN, INFER]);
    }

    pub fn command_retry_after_ack_returns_the_original_receipt(d: &mut Driver) {
        let (first, id) = d.start("once");
        d.settle();
        let original = d.accepted(first).clone();
        started(d);
        stop(d);
        d.settle();
        let before = shapes(d).len();
        let retry = d.command(Command::StartTurn {
            command_id: id.clone(),
            content: text("once"),
        });
        assert_eq!(d.accepted(retry), &original, "same ack and turn ID");
        let stale = d.command(Command::StartTurn {
            command_id: id,
            content: text("different"),
        });
        assert_eq!(d.rejected(stale), Rejection::StaleCommand);
        assert_eq!(shapes(d).len(), before, "no new commit");
    }

    pub fn command_retry_while_in_flight_attaches_to_the_opening_commit(d: &mut Driver) {
        let (first, id) = d.start("once");
        let retry = d.command(Command::StartTurn {
            command_id: id.clone(),
            content: text("once"),
        });
        assert!(d.reply(retry).is_none(), "attached, not answered");
        let stale = d.command(Command::StartTurn {
            command_id: id,
            content: text("other"),
        });
        assert_eq!(d.rejected(stale), Rejection::StaleCommand);
        d.settle();
        assert_eq!(
            d.accepted(first),
            d.accepted(retry),
            "both answered by the one ack"
        );
        assert_eq!(shapes(d), [OPEN, INFER]);
        started(d);
        stop(d);
        d.settle();
    }

    pub fn turn_id_differs_from_command_id_and_the_receipt_links_them(d: &mut Driver) {
        let (request, id) = d.start("linked");
        d.settle();
        let turn = turn_of(d, request);
        assert_ne!(turn.as_str(), id.as_str());
        let snapshot = d.runtime.snapshot().clone();
        let receipt = snapshot.command_receipt(&id).expect("durable receipt");
        assert!(
            receipt
                .commit
                .events
                .iter()
                .any(|e| matches!(e, JournalEvent::TurnStarted(s) if s.turn_id == turn))
        );
        // The core fingerprints with the function `bind_command` stores.
        let command = serde_json::json!({ "kind": "start_turn", "content": text("linked") });
        let bytes = crate::session::store::normalized_command(&command).unwrap();
        assert_eq!(receipt.fingerprint, Digest::of(&bytes));
        assert!(snapshot.turn(&turn).is_some());
        assert!(snapshot.turn(&id.as_str().parse().unwrap()).is_none());
        started(d);
        stop(d);
        d.settle();
    }

    pub fn stream_ending_without_finished_is_interrupted(d: &mut Driver) {
        let request = begin(d, "hi");
        say(d, 0, "cut");
        d.ended();
        d.settle();
        assert_eq!(
            shapes(d)[2],
            "assistant(interrupted) finished(failed, provider_eof)"
        );
        assert_eq!(texts(&responses(d)[0]), ["cut"]);
        assert_eq!(outcome(d, &turn_of(d, request)), Some(TurnOutcome::Failed));
    }

    pub fn partial_response_limit_interrupts_the_stream(d: &mut Driver) {
        let max = d.runtime.config().max_partial_bytes;
        begin(d, "big");
        say(d, 0, &"a".repeat(max / 2));
        let effects = say(d, 0, &"b".repeat(max / 2));
        assert!(has(&effects, |e| matches!(
            e,
            Effect::CancelInference { .. }
        )));
        assert_eq!(persists(&effects), 1);
        // The partial stays bounded: the overflowing delta is not applied.
        let partial = d.runtime.view().partial.unwrap();
        assert_eq!(partial.blocks[&0], PartialBlock::Text("a".repeat(max / 2)));
        d.settle();
        assert_eq!(
            shapes(d)[2],
            "assistant(interrupted) finished(failed, response_too_large)"
        );
        assert_eq!(texts(&responses(d)[0]), ["a".repeat(max / 2)]);
    }

    pub fn malformed_or_incomplete_tool_calls_are_never_answered(d: &mut Driver) {
        let malformed = "assistant(failed) finished(failed, malformed_stream)";
        // Invalid JSON at `ready` ends the stream immediately.
        begin(d, "bad json");
        call(d, 0, "read");
        args(d, 0, r#"{"path": "#);
        let effects = ready(d, 0);
        assert!(has(&effects, |e| matches!(
            e,
            Effect::CancelInference { .. }
        )));
        d.settle();
        // Duplicate keys are malformed, as storage would reject them.
        begin(d, "duplicate keys");
        call(d, 0, "read");
        args(d, 0, r#"{"a":1,"a":2}"#);
        ready(d, 0);
        d.settle();
        // `tool_calls` with a call that never became ready.
        begin(d, "incomplete");
        call(d, 0, "read");
        call(d, 1, "write");
        args(d, 0, "{}");
        ready(d, 0);
        finish(d, ProviderOutcome::Completed, Some(FinishReason::ToolCalls));
        d.settle();
        // `stop` with a ready call is inconsistent.
        begin(d, "inconsistent");
        call(d, 0, "read");
        args(d, 0, "{}");
        ready(d, 0);
        stop(d);
        d.settle();
        // Truncation drops an incomplete call instead of promoting it.
        begin(d, "truncated");
        say(d, 0, "text");
        call(d, 1, "read");
        args(d, 1, r#"{"pa"#);
        finish(
            d,
            ProviderOutcome::Completed,
            Some(FinishReason::OutputLimit),
        );
        d.settle();
        let shapes = shapes(d);
        let responses: Vec<_> = shapes
            .iter()
            .filter(|s| s.starts_with("assistant"))
            .collect();
        assert_eq!(
            responses,
            [
                malformed,
                malformed,
                malformed,
                malformed,
                "assistant(completed/outputlimit) inferring>processing finished(failed, output_limit)",
            ]
        );
        assert!(
            shapes.iter().all(|s| !s.contains("result(")),
            "no call answered"
        );
        let snapshot = d.runtime.snapshot();
        let truncated = match &snapshot
            .entry(snapshot.head().unwrap())
            .unwrap()
            .entry
            .payload
        {
            EntryPayload::AssistantResponse(r) => r.clone(),
            _ => unreachable!(),
        };
        assert_eq!(truncated.content, text("text"));
        for (_, stored) in snapshot.entries() {
            if let EntryPayload::AssistantResponse(r) = &stored.entry.payload
                && r.outcome == ResponseOutcome::Failed
            {
                assert_eq!(
                    r.error.as_ref().unwrap().code,
                    ModelErrorCode::MalformedStream
                );
                assert!(
                    r.content
                        .iter()
                        .all(|b| !matches!(b, ContentBlock::ToolCall { .. }))
                );
            }
        }
    }

    pub fn io_failure_before_write_faults_with_only_rejections_after(d: &mut Driver) {
        let (request, _) = d.start("doomed");
        d.settle();
        let turn = turn_of(d, request);
        started(d);
        say(d, 0, "partial");
        stop(d);
        d.fail = Some(Point::BeforeWrite);
        d.complete();
        assert_eq!(d.runtime.phase(), Phase::Faulted);
        assert!(d.runtime.view().partial.is_none());
        let before = d.segment.effects.len();
        let cancel = d.command(Command::Cancel { turn_id: turn });
        let command_id = d.new_command_id();
        let start = d.command(Command::StartTurn {
            command_id,
            content: text("again"),
        });
        assert_eq!(d.rejected(cancel), Rejection::Faulted);
        assert_eq!(d.rejected(start), Rejection::Faulted);
        say(d, 0, "late");
        d.ended();
        d.fire(TimerId(1));
        assert!(
            d.segment.effects[before..].iter().all(|e| matches!(
                e,
                Effect::Reply {
                    result: Err(Rejection::Faulted),
                    ..
                }
            )),
            "only rejections after a fault"
        );
    }

    pub fn io_failure_after_sync_leaves_the_unacknowledged_commit_durable(d: &mut Driver) {
        let (request, _) = d.start("durable but unacknowledged");
        d.fail = Some(Point::Synced);
        d.complete();
        assert_eq!(d.rejected(request), Rejection::Faulted);
        assert_eq!(d.runtime.phase(), Phase::Faulted);
        // `finish` checks that the reopened journal holds exactly this commit.
    }

    pub fn rejected_persist_faults_the_runtime(d: &mut Driver) {
        let (request, _) = d.start("refused");
        d.fail_pending(PersistError::Rejected {
            code: "illegal_state_transition".into(),
        });
        assert_eq!(d.rejected(request), Rejection::Faulted);
        let fault = d.segment.events().find_map(|e| match &e.kind {
            EventKind::Fault(cause) => Some(cause.clone()),
            _ => None,
        });
        assert!(matches!(
            fault,
            Some(FaultCause::Persist(PersistError::Rejected { .. }))
        ));
        // An answer for a commit that is not in flight is also a fault, so it
        // is reported once and ignored afterwards.
        let effects = d.step(Input::PersistFailed {
            token: CommitToken(42),
            error: PersistError::Io {
                message: "late".into(),
            },
        });
        assert!(effects.is_empty());
    }

    pub fn select_head_and_switch_model_only_while_idle(d: &mut Driver) {
        let other = ModelRef {
            provider_instance: "mock".into(),
            model: "other".into(),
        };
        plain_turn(d, "first", "one");
        let first_head = d.runtime.snapshot().head().cloned();
        let switch = d.command(Command::SwitchModel {
            model: other.clone(),
        });
        d.settle();
        assert!(matches!(d.accepted(switch), Accepted::ModelSwitched { .. }));
        let empty = d.command(Command::SwitchModel {
            model: ModelRef {
                provider_instance: "mock".into(),
                model: String::new(),
            },
        });
        assert_eq!(d.rejected(empty), Rejection::Invalid("empty_model"));
        // The next inference uses the branch's new model.
        begin(d, "second");
        let model = d.segment.effects.iter().rev().find_map(|e| match e {
            Effect::StartInference { model, .. } => Some(model.clone()),
            _ => None,
        });
        assert_eq!(model, Some(other.clone()));
        // Busy during the turn.
        let busy_head = d.command(Command::SelectHead { head: None });
        let busy_model = d.command(Command::SwitchModel {
            model: other.clone(),
        });
        assert_eq!(d.rejected(busy_head), Rejection::Busy);
        assert_eq!(d.rejected(busy_model), Rejection::Busy);
        call(d, 0, "read");
        args(d, 0, "{}");
        ready(d, 0);
        finish(d, ProviderOutcome::Completed, Some(FinishReason::ToolCalls));
        d.settle();
        started(d);
        stop(d);
        d.settle();
        // A head inside a tool exchange, unknown heads and the current head are invalid.
        let proposal =
            d.runtime
                .snapshot()
                .entries()
                .find_map(|(id, stored)| match &stored.entry.payload {
                    EntryPayload::AssistantResponse(r)
                        if r.finish_reason == Some(FinishReason::ToolCalls) =>
                    {
                        Some(id.clone())
                    }
                    _ => None,
                });
        let inside = d.command(Command::SelectHead { head: proposal });
        assert_eq!(
            d.rejected(inside),
            Rejection::Invalid("head_inside_tool_exchange")
        );
        let unknown = d.command(Command::SelectHead {
            head: Some(EntryId::new()),
        });
        assert_eq!(d.rejected(unknown), Rejection::Invalid("unknown_head"));
        let same = d.command(Command::SelectHead {
            head: d.runtime.snapshot().head().cloned(),
        });
        assert_eq!(d.rejected(same), Rejection::Invalid("head_unchanged"));
        // Navigating back restores the model selected on that branch.
        let back = d.command(Command::SelectHead {
            head: first_head.clone(),
        });
        d.settle();
        assert!(matches!(d.accepted(back), Accepted::HeadSelected { .. }));
        assert_eq!(d.runtime.snapshot().head(), first_head.as_ref());
        assert_eq!(
            d.runtime
                .snapshot()
                .model_at(first_head.as_ref())
                .unwrap()
                .model,
            "scripted"
        );
        let root = d.command(Command::SelectHead { head: None });
        d.settle();
        assert!(matches!(d.accepted(root), Accepted::HeadSelected { .. }));
        assert_eq!(shapes(d).last().unwrap(), "navigate");
    }

    pub fn provider_outcomes_map_to_turn_outcomes(d: &mut Driver) {
        // Setup failure before `Started`, with a provider error.
        let (request, _) = d.start("fails");
        d.settle();
        d.provider(ProviderEvent::Finished {
            outcome: ProviderOutcome::Failed,
            finish_reason: None,
            error: Some(ModelError {
                code: ModelErrorCode::RateLimit,
                message: "slow down".into(),
                provider_code: Some("429".into()),
                retry_after_ms: Some(1000),
            }),
        });
        d.settle();
        assert_eq!(outcome(d, &turn_of(d, request)), Some(TurnOutcome::Failed));
        // A failure without an error is malformed.
        begin(d, "bad failure");
        finish(d, ProviderOutcome::Failed, None);
        d.settle();
        begin(d, "provider cancelled");
        say(d, 0, "x");
        finish(d, ProviderOutcome::Cancelled, None);
        d.settle();
        begin(d, "refused");
        finish(d, ProviderOutcome::Completed, Some(FinishReason::Refusal));
        d.settle();
        // Content before `Started` is malformed.
        let (request, _) = d.start("early");
        d.settle();
        say(d, 0, "too early");
        d.settle();
        assert_eq!(outcome(d, &turn_of(d, request)), Some(TurnOutcome::Failed));
        let responses: Vec<_> = shapes(d)
            .into_iter()
            .filter(|s| s.starts_with("assistant"))
            .collect();
        assert_eq!(
            responses,
            [
                "assistant(failed) finished(failed, rate_limit)",
                "assistant(failed) finished(failed, malformed_stream)",
                "assistant(cancelled) finished(cancelled, provider_cancelled)",
                "assistant(completed/refusal) inferring>processing finished(completed)",
                "assistant(failed) finished(failed, malformed_stream)",
            ]
        );
        let snapshot = d.runtime.snapshot();
        let usages: Vec<_> = snapshot
            .entries()
            .filter_map(|(_, s)| match &s.entry.payload {
                EntryPayload::AssistantResponse(r) => Some(r.usage.clone()),
                _ => None,
            })
            .collect();
        assert!(
            usages.iter().all(Option::is_none),
            "missing usage stays null"
        );
    }

    pub fn stale_streams_and_timers_are_dropped_with_diagnostics(d: &mut Driver) {
        plain_turn(d, "first", "one");
        let old = d.inference.unwrap();
        begin(d, "second");
        let effects = d.step(Input::Provider {
            inference: old,
            event: ProviderEvent::TextDelta {
                block_index: 0,
                text: "stale".into(),
            },
        });
        let dropped: Vec<_> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Emit(RuntimeEvent {
                    kind:
                        EventKind::Diagnostic(RuntimeDiagnostic::StaleProviderEvent {
                            inference,
                            dropped,
                        }),
                    ..
                }) => Some((*inference, *dropped)),
                _ => None,
            })
            .collect();
        assert_eq!(dropped, [(old, 1)]);
        let effects = d.step(Input::Provider {
            inference: InferenceId(77),
            event: ProviderEvent::Started,
        });
        assert!(matches!(
            &effects[..],
            [Effect::Emit(RuntimeEvent {
                kind: EventKind::Diagnostic(RuntimeDiagnostic::StaleProviderEvent {
                    dropped: 2,
                    ..
                }),
                ..
            })]
        ));
        assert!(d.step(Input::ProviderEnded { inference: old }).is_empty());
        say(d, 0, "fresh");
        stop(d);
        d.settle();
        assert_eq!(texts(&responses(d)[1]), ["fresh"]);
    }
}
