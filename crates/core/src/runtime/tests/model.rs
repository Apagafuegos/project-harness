//! Native payloads and deadline lifecycle, through real storage and replay.
use super::scenarios::{args, begin, call, ready, responses, say};
use super::*;
use crate::model::NativeContinuation;

fn state(index: u32, payload: Vec<u8>) -> ProviderEvent {
    ProviderEvent::ProviderState {
        block_index: index,
        schema: "mock.state.v1".into(),
        payload,
    }
}
fn completed(native: Option<NativeContinuation>) -> ProviderEvent {
    ProviderEvent::Finished {
        outcome: ProviderOutcome::Completed,
        finish_reason: Some(FinishReason::Stop),
        error: None,
        continuation: native,
    }
}
fn native(bytes: Vec<u8>) -> NativeContinuation {
    NativeContinuation {
        schema: "mock.continuation.v1".into(),
        payload: bytes,
    }
}

#[test]
fn native_state_owner_is_request_model_and_view_contains_only_length() {
    scenario(known(), |d| {
        begin(d, "native");
        say(d, 0, "visible");
        d.provider(state(1, b"secret native bytes".to_vec()));
        let view = d.runtime.view();
        let partial = view.partial.as_ref().unwrap();
        assert_eq!(
            partial.blocks[&1],
            PartialBlock::ProviderState {
                schema: "mock.state.v1".into(),
                bytes: 19
            }
        );
        assert!(!format!("{partial:?}").contains("secret native"));
        let fingerprint = d
            .segment
            .effects
            .iter()
            .find_map(|e| match e {
                Effect::StartInference { fingerprint, .. } => Some(fingerprint.clone()),
                _ => None,
            })
            .unwrap();
        d.provider(completed(Some(native(b"handle".to_vec()))));
        d.settle();
        let response = &responses(d)[0];
        let ContentBlock::ProviderState {
            owner, artifact, ..
        } = &response.content[1]
        else {
            panic!("state in content order")
        };
        assert_eq!(owner, &response.model);
        assert_eq!(
            d.root.read_artifact(artifact, 19).unwrap(),
            b"secret native bytes"
        );
        let continuation = response.continuation.as_ref().unwrap();
        assert_eq!(continuation.owner, response.model);
        assert_eq!(continuation.input_fingerprint, fingerprint);
        assert_eq!(
            d.root.read_artifact(&continuation.artifact, 6).unwrap(),
            b"handle"
        );
    });
}

#[test]
fn native_state_duplicate_indices_and_delta_kind_mismatches_are_malformed() {
    for first in [
        state(0, vec![1]),
        ProviderEvent::TextDelta {
            block_index: 0,
            text: "x".into(),
        },
        ProviderEvent::ReasoningSummaryDelta {
            block_index: 0,
            text: "x".into(),
        },
        ProviderEvent::ToolCallStarted {
            block_index: 0,
            name: "read".into(),
            provider_call_id: None,
        },
    ] {
        scenario_from(Driver::new, |d| {
            begin(d, "duplicate");
            d.provider(first.clone());
            d.provider(state(0, vec![2]));
            d.settle();
            let response = &responses(d)[0];
            assert_eq!(
                response.error.as_ref().unwrap().code,
                ModelErrorCode::MalformedStream
            );
            assert_eq!(response.error.as_ref().unwrap().message, "duplicate_block");
        });
    }
    scenario(known(), |d| {
        begin(d, "state before tool start");
        d.provider(state(0, vec![1]));
        call(d, 0, "read");
        d.settle();
        assert_eq!(
            responses(d)[0].error.as_ref().unwrap().message,
            "duplicate_block"
        );
    });
    for delta in [
        ProviderEvent::ToolArgumentsDelta {
            block_index: 0,
            fragment: "{}".into(),
        },
        ProviderEvent::TextDelta {
            block_index: 0,
            text: "x".into(),
        },
        ProviderEvent::ReasoningSummaryDelta {
            block_index: 0,
            text: "x".into(),
        },
    ] {
        scenario_from(Driver::new, |d| {
            begin(d, "kind mismatch");
            d.provider(state(0, vec![1]));
            d.provider(delta.clone());
            d.settle();
            assert_eq!(
                responses(d)[0].error.as_ref().unwrap().message,
                "block_kind_mismatch"
            );
        });
    }
}

#[test]
fn native_state_and_continuation_payload_limits_pin_storage_and_disable_calls() {
    for mode in 0..4 {
        scenario_from(Driver::new, |d| {
            begin(d, "bounds");
            call(d, 0, "read");
            args(d, 0, "{}");
            ready(d, 0);
            match mode {
                0 => {
                    d.provider(state(1, vec![0; 256 * 1024 + 1]));
                }
                1 => {
                    d.provider(completed(Some(native(vec![0; 256 * 1024 + 1]))));
                }
                _ => {
                    for n in 1..=16 {
                        d.provider(state(n, vec![n as u8; 256 * 1024]));
                    }
                    if mode == 2 {
                        d.provider(state(17, vec![1]));
                    } else {
                        d.provider(completed(Some(native(vec![1]))));
                    }
                }
            }
            d.settle();
            let response = &responses(d)[0];
            assert_eq!(response.outcome, ResponseOutcome::Failed);
            assert_eq!(
                response.error.as_ref().unwrap().code,
                ModelErrorCode::LimitExceeded
            );
            assert!(response.continuation.is_none());
            assert!(
                !response
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
            );
        });
    }
}

#[test]
fn native_payload_bound_is_separate_from_partial_bytes_and_exact_bound_is_accepted() {
    scenario(known(), |d| {
        begin(d, "exact response native bound");
        for n in 0..15 {
            d.provider(state(n, vec![n as u8; 256 * 1024]));
        }
        d.provider(completed(Some(native(vec![15; 256 * 1024]))));
        d.settle();
        assert_eq!(responses(d)[0].outcome, ResponseOutcome::Completed);
        assert!(responses(d)[0].continuation.is_some());
    });
    let mut config = known();
    config.max_partial_bytes = 512;
    scenario(config, |d| {
        begin(d, "bounded");
        d.provider(state(0, vec![0; 256 * 1024]));
        d.provider(completed(Some(native(vec![1; 256 * 1024]))));
        d.settle();
        assert_eq!(responses(d)[0].outcome, ResponseOutcome::Completed);
        assert!(responses(d)[0].continuation.is_some());
    });
}

#[test]
fn native_continuation_noncompleted_finish_and_empty_schema_are_malformed() {
    for outcome in [ProviderOutcome::Failed, ProviderOutcome::Cancelled] {
        scenario_from(Driver::new, |d| {
            begin(d, "partial continuation");
            d.provider(ProviderEvent::Finished {
                outcome,
                finish_reason: None,
                error: None,
                continuation: Some(native(vec![1])),
            });
            d.settle();
            assert_eq!(
                responses(d)[0].error.as_ref().unwrap().message,
                "continuation_on_partial_finish"
            );
        });
    }
    scenario(known(), |d| {
        begin(d, "empty continuation schema");
        d.provider(completed(Some(NativeContinuation {
            schema: String::new(),
            payload: vec![],
        })));
        d.settle();
        assert_eq!(
            responses(d)[0].error.as_ref().unwrap().message,
            "empty_native_schema"
        );
    });
    scenario(known(), |d| {
        begin(d, "empty schema");
        d.provider(ProviderEvent::ProviderState {
            block_index: 0,
            schema: String::new(),
            payload: vec![],
        });
        d.settle();
        assert_eq!(
            responses(d)[0].error.as_ref().unwrap().message,
            "empty_native_schema"
        );
    });
}

#[test]
fn inference_deadline_core_scenario_cancels_then_records_partial_with_replay() {
    for confirm in [false, true] {
        let mut config = known();
        config.inference_deadline = Some(std::time::Duration::from_secs(10));
        scenario_from(
            || Driver::with_config(config.clone()),
            |d| {
                begin(d, "deadline");
                say(d, 0, "so far");
                let deadline = d
                    .segment
                    .effects
                    .iter()
                    .find_map(|e| match e {
                        Effect::SetTimer { timer, duration }
                            if *duration == std::time::Duration::from_secs(10) =>
                        {
                            Some(*timer)
                        }
                        _ => None,
                    })
                    .unwrap();
                let effects = d.fire(deadline);
                assert!(
                    effects
                        .iter()
                        .any(|e| matches!(e, Effect::CancelInference { .. }))
                );
                assert!(
                    !effects
                        .iter()
                        .any(|e| matches!(e, Effect::ReleaseInference { .. }))
                );
                assert_eq!(d.timers.len(), 1);
                if confirm {
                    d.ended();
                } else {
                    let grace = *d.timers.first().unwrap();
                    d.fire(grace);
                }
                d.settle();
                assert_eq!(responses(d)[0].outcome, ResponseOutcome::Interrupted);
                assert_eq!(responses(d)[0].content, text("so far"));
                assert!(d.runtime.snapshot().events().any(|(_, _, event)| matches!(event, JournalEvent::TurnFinished(turn) if turn.code.as_deref() == Some("inference_deadline"))));
                assert!(d.timers.is_empty());
            },
        );
    }
}
