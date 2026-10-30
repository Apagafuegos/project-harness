use std::sync::Mutex;

use serde_json::json;

use super::*;
use crate::{
    model::{
        ModelInput, NativeContinuation, ReasoningEffort,
        scripted::{Script, ScriptedProvider, Step},
    },
    session::{EffectSet, ModelRef, ResponseId},
};

fn request() -> ModelRequest {
    ModelRequest {
        response_id: ResponseId::try_from("019a0000-0000-7000-8000-000000000001".to_owned())
            .unwrap(),
        artifacts: Default::default(),
        input: ModelInput {
            model: ModelRef {
                provider_instance: "mock".into(),
                model: "fixture".into(),
            },
            items: Vec::new(),
            tools: ["read", "other"]
                .into_iter()
                .map(|name| ToolDefinition {
                    name: name.into(),
                    description: format!("definition of {name}"),
                    input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
                    effects: EffectSet::new([crate::session::EffectClass::Read]).unwrap(),
                    backend: crate::session::ExecutionBackend::TrustedFs,
                })
                .collect(),
            options: InferenceOptions {
                max_output_tokens: 123,
                reasoning_effort: Some(ReasoningEffort::High),
                provider_options: serde_json::from_value(json!({"mock":{"strict":true}})).unwrap(),
            },
            continuation: None,
        },
    }
}
fn usage(output: Option<u64>) -> Usage {
    Usage {
        input_tokens: Some(10),
        output_tokens: output,
        cached_input_tokens: None,
        reasoning_tokens: Some(2),
    }
}
fn error(code: ModelErrorCode) -> ModelError {
    ModelError {
        code,
        message: "safe fixture failure".into(),
        provider_code: Some("fixture_code".into()),
        retry_after_ms: Some(321),
    }
}
fn start() -> Step {
    Step::Event(ProviderEvent::Started)
}
fn call(index: u32) -> Step {
    Step::Event(ProviderEvent::ToolCallStarted {
        block_index: index,
        name: format!("tool{index}"),
        provider_call_id: Some(format!("native{index}")),
    })
}
fn fragment(index: u32, fragment: &str) -> Step {
    Step::Event(ProviderEvent::ToolArgumentsDelta {
        block_index: index,
        fragment: fragment.into(),
    })
}
fn ready(index: u32) -> Step {
    Step::Event(ProviderEvent::ToolCallReady { block_index: index })
}
fn tool(index: u32) -> ExpectedTool {
    ExpectedTool {
        name: format!("tool{index}"),
        provider_call_id: Some(format!("native{index}")),
        arguments: json!({"x":index}),
    }
}
fn case(kind: CaseKind) -> Case {
    let mut expected = Expected::default();
    use CaseKind::*;
    match kind {
        Text => {
            expected.text.insert(0, "hello world".into());
        }
        OneTool => {
            expected.finish_reason = Some(FinishReason::ToolCalls);
            expected.tools.insert(0, tool(0));
        }
        InterleavedTools => {
            expected.finish_reason = Some(FinishReason::ToolCalls);
            expected.tools.insert(0, tool(0));
            expected.tools.insert(1, tool(1));
        }
        MalformedToolJson | ProviderError | RateLimit | SetupFailure | UnsupportedOption => {
            expected.outcome = ProviderOutcome::Failed;
            expected.finish_reason = None;
            expected.error = Some(error(match kind {
                MalformedToolJson => ModelErrorCode::MalformedStream,
                RateLimit => ModelErrorCode::RateLimit,
                UnsupportedOption => ModelErrorCode::UnsupportedCapability,
                _ => ModelErrorCode::ProviderFailure,
            }));
            if kind == UnsupportedOption {
                expected.error.as_mut().unwrap().message = "unsupported option mock".into();
            }
        }
        Refusal => expected.finish_reason = Some(FinishReason::Refusal),
        OutputLimit => expected.finish_reason = Some(FinishReason::OutputLimit),
        Usage => expected.usage = vec![usage(Some(2)), usage(Some(5)), usage(None)],
        NativeState => {
            expected.native.insert(
                0,
                NativeMetadata {
                    schema: "state.v1".into(),
                    bytes: 3,
                },
            );
            expected.continuation = Some(NativeMetadata {
                schema: "handle.v1".into(),
                bytes: 2,
            });
        }
        CancelSetup | CancelStreaming | Drop | Deadline | Request => {}
    }
    Case {
        kind,
        fixture: format!("{kind:?}"),
        request: request(),
        expected,
        unsupported_option: (kind == UnsupportedOption).then(|| "mock".into()),
    }
}
fn steps(kind: CaseKind) -> Vec<Step> {
    use CaseKind::*;
    let expected = case(kind).expected;
    match kind {
        Text => vec![
            start(),
            Step::text(0, "hello "),
            Step::text(0, "world"),
            Step::completed(FinishReason::Stop),
        ],
        OneTool => vec![
            start(),
            call(0),
            fragment(0, "{\"x\":"),
            fragment(0, "0}"),
            ready(0),
            Step::completed(FinishReason::ToolCalls),
        ],
        InterleavedTools => vec![
            start(),
            call(0),
            call(1),
            fragment(0, "{\"x\":"),
            fragment(1, "{\"x\":"),
            fragment(0, "0}"),
            ready(0),
            fragment(1, "1}"),
            ready(1),
            Step::completed(FinishReason::ToolCalls),
        ],
        MalformedToolJson => vec![
            start(),
            call(0),
            fragment(0, "{invalid"),
            Step::failed(expected.error.unwrap()),
        ],
        ProviderError | RateLimit | UnsupportedOption => {
            vec![start(), Step::failed(expected.error.unwrap())]
        }
        SetupFailure => vec![Step::failed(expected.error.unwrap())],
        Refusal | OutputLimit => vec![start(), Step::completed(expected.finish_reason.unwrap())],
        Usage => vec![
            start(),
            Step::Event(ProviderEvent::UsageUpdated {
                usage: usage(Some(2)),
            }),
            Step::Event(ProviderEvent::UsageUpdated {
                usage: usage(Some(5)),
            }),
            Step::Event(ProviderEvent::UsageUpdated { usage: usage(None) }),
            Step::completed(FinishReason::Stop),
        ],
        NativeState => vec![
            start(),
            Step::provider_state(0, "state.v1", vec![1, 2, 3]),
            Step::completed_with_continuation(
                FinishReason::Stop,
                NativeContinuation {
                    schema: "handle.v1".into(),
                    payload: vec![4, 5],
                },
            ),
        ],
        CancelSetup => vec![Step::AwaitCancel, Step::cancelled()],
        CancelStreaming => vec![
            start(),
            Step::text(0, "partial"),
            Step::AwaitCancel,
            Step::cancelled(),
        ],
        Drop => vec![start(), Step::Stall],
        Deadline | Request => vec![start(), Step::completed(FinishReason::Stop)],
    }
}
fn fixture(script: Vec<Step>) -> Fixture {
    let provider = Arc::new(ScriptedProvider::new([Script::new(script)]));
    let observed = provider.clone();
    Fixture {
        provider,
        observe: Arc::new(move || {
            let records = observed.streams();
            let record = records.first().unwrap();
            Observation {
                submitted: Some(SubmittedRequest {
                    tools: record
                        .request
                        .input
                        .tools
                        .iter()
                        .map(SubmittedTool::from)
                        .collect(),
                    options: record.request.input.options.clone(),
                }),
                released: record.dropped,
            }
        }),
    }
}
fn factory(name: &str) -> Fixture {
    fixture(steps(
        CaseKind::ALL
            .into_iter()
            .find(|kind| format!("{kind:?}") == name)
            .unwrap(),
    ))
}
async fn broken(kind: CaseKind, script: Vec<Step>, check: &'static str) {
    let script = Mutex::new(Some(script));
    let factory = |_: &str| fixture(script.lock().unwrap().take().unwrap());
    assert_eq!(
        run_case(&factory, &case(kind)).await.unwrap_err().check,
        check
    );
}

#[tokio::test]
async fn scripted_provider_passes_complete_public_manifest() {
    run(
        &factory,
        &CaseKind::ALL.into_iter().map(case).collect::<Vec<_>>(),
    )
    .await
    .unwrap();
}
#[tokio::test]
async fn manifest_requires_each_scenario_once() {
    assert_eq!(
        run(&factory, &[]).await.unwrap_err().check,
        "manifest_coverage"
    );
    let mut manifest = CaseKind::ALL.into_iter().map(case).collect::<Vec<_>>();
    manifest.push(case(CaseKind::Text));
    assert_eq!(
        run(&factory, &manifest).await.unwrap_err().check,
        "manifest_coverage"
    );
    let mut invalid = case(CaseKind::Request);
    invalid.request.input.tools.clear();
    assert_eq!(
        run_case(&factory, &invalid).await.unwrap_err().check,
        "manifest_scenario"
    );
}
#[tokio::test]
async fn broken_started_order_and_duplication_fail() {
    broken(
        CaseKind::Text,
        vec![
            Step::text(0, "hello world"),
            Step::completed(FinishReason::Stop),
        ],
        "started_first",
    )
    .await;
    broken(
        CaseKind::Text,
        vec![start(), start(), Step::completed(FinishReason::Stop)],
        "started_once",
    )
    .await;
}
#[tokio::test]
async fn broken_block_index_reuse_across_every_kind_fails() {
    for reused in [
        call(0),
        Step::provider_state(0, "state", vec![]),
        Step::Event(ProviderEvent::ReasoningSummaryDelta {
            block_index: 0,
            text: "not text".into(),
        }),
    ] {
        let check = if matches!(
            reused,
            Step::Event(ProviderEvent::ReasoningSummaryDelta { .. })
        ) {
            "block_kind_reused"
        } else {
            "duplicate_block"
        };
        broken(
            CaseKind::Text,
            vec![
                start(),
                Step::text(0, "text"),
                reused,
                Step::completed(FinishReason::Stop),
            ],
            check,
        )
        .await;
    }
    broken(
        CaseKind::NativeState,
        vec![
            start(),
            Step::provider_state(0, "state", vec![]),
            Step::text(0, "leak"),
            Step::completed(FinishReason::Stop),
        ],
        "block_kind_reused",
    )
    .await;
}
#[tokio::test]
async fn broken_tool_fragment_and_ready_lifecycle_fail() {
    broken(
        CaseKind::OneTool,
        vec![start(), fragment(0, "{}")],
        "tool_fragment_lifecycle",
    )
    .await;
    broken(
        CaseKind::OneTool,
        vec![start(), ready(0)],
        "tool_ready_lifecycle",
    )
    .await;
    broken(
        CaseKind::OneTool,
        vec![
            start(),
            call(0),
            fragment(0, "{}"),
            ready(0),
            fragment(0, "{}"),
        ],
        "tool_fragment_lifecycle",
    )
    .await;
    broken(
        CaseKind::OneTool,
        vec![start(), call(0), fragment(0, "{}"), ready(0), ready(0)],
        "tool_ready_lifecycle",
    )
    .await;
    broken(
        CaseKind::OneTool,
        vec![start(), call(0), call(0)],
        "duplicate_block",
    )
    .await;
    broken(
        CaseKind::OneTool,
        vec![
            start(),
            Step::Event(ProviderEvent::ToolCallStarted {
                block_index: 0,
                name: String::new(),
                provider_call_id: None,
            }),
        ],
        "tool_name",
    )
    .await;
}
#[tokio::test]
async fn broken_tool_json_and_incorrect_reassembly_fail() {
    for fragments in ["{broken", "[]", "{\"x\":0,\"x\":1}"] {
        broken(
            CaseKind::MalformedToolJson,
            vec![start(), call(0), fragment(0, fragments), ready(0)],
            "tool_json_object",
        )
        .await;
    }
    broken(
        CaseKind::OneTool,
        vec![
            start(),
            call(0),
            fragment(0, "{\"x\":999}"),
            ready(0),
            Step::completed(FinishReason::ToolCalls),
        ],
        "normalized_outcome",
    )
    .await;
    broken(
        CaseKind::OneTool,
        vec![
            start(),
            call(0),
            fragment(0, "{}"),
            Step::completed(FinishReason::ToolCalls),
        ],
        "tool_finish",
    )
    .await;
    broken(
        CaseKind::OneTool,
        vec![
            start(),
            call(0),
            fragment(0, "{}"),
            ready(0),
            Step::completed(FinishReason::Stop),
        ],
        "tool_finish",
    )
    .await;
}
#[tokio::test]
async fn broken_noninterleaved_fixture_fails() {
    broken(
        CaseKind::InterleavedTools,
        vec![
            start(),
            call(0),
            fragment(0, "{\"x\":0}"),
            ready(0),
            call(1),
            fragment(1, "{\"x\":1}"),
            ready(1),
            Step::completed(FinishReason::ToolCalls),
        ],
        "interleaved_fragments",
    )
    .await;
}
#[tokio::test]
async fn broken_terminal_count_and_order_fail() {
    broken(
        CaseKind::Text,
        vec![start(), Step::text(0, "hello world")],
        "finished_once",
    )
    .await;
    broken(
        CaseKind::Text,
        vec![
            start(),
            Step::completed(FinishReason::Stop),
            Step::completed(FinishReason::Stop),
        ],
        "finished_last",
    )
    .await;
    broken(
        CaseKind::Text,
        vec![
            start(),
            Step::completed(FinishReason::Stop),
            Step::text(0, "late"),
        ],
        "finished_last",
    )
    .await;
    broken(
        CaseKind::Text,
        vec![
            start(),
            Step::Event(ProviderEvent::Finished {
                outcome: ProviderOutcome::Failed,
                finish_reason: Some(FinishReason::Stop),
                error: None,
                continuation: None,
            }),
        ],
        "terminal_shape",
    )
    .await;
}
#[tokio::test]
async fn broken_outcomes_retry_usage_and_missing_text_fail() {
    for kind in [
        CaseKind::Text,
        CaseKind::Refusal,
        CaseKind::OutputLimit,
        CaseKind::ProviderError,
        CaseKind::RateLimit,
    ] {
        broken(
            kind,
            vec![start(), Step::completed(FinishReason::Stop)],
            "normalized_outcome",
        )
        .await;
    }
    for kind in [CaseKind::ProviderError, CaseKind::RateLimit] {
        let mut wrong = case(kind).expected.error.unwrap();
        wrong.retry_after_ms = None;
        broken(
            kind,
            vec![start(), Step::failed(wrong)],
            "normalized_outcome",
        )
        .await;
    }
    broken(
        CaseKind::Usage,
        vec![
            start(),
            Step::Event(ProviderEvent::UsageUpdated {
                usage: usage(Some(2)),
            }),
            Step::Event(ProviderEvent::UsageUpdated {
                usage: usage(Some(7)),
            }),
            Step::Event(ProviderEvent::UsageUpdated {
                usage: usage(Some(7)),
            }),
            Step::completed(FinishReason::Stop),
        ],
        "normalized_outcome",
    )
    .await;
    broken(
        CaseKind::Text,
        vec![
            start(),
            Step::text(0, "hello world"),
            Step::Event(ProviderEvent::UsageUpdated {
                usage: usage(Some(0)),
            }),
            Step::completed(FinishReason::Stop),
        ],
        "normalized_outcome",
    )
    .await;
    broken(
        CaseKind::SetupFailure,
        vec![
            start(),
            Step::failed(case(CaseKind::SetupFailure).expected.error.unwrap()),
        ],
        "setup_failure_started",
    )
    .await;
    broken(
        CaseKind::MalformedToolJson,
        vec![
            start(),
            Step::failed(case(CaseKind::MalformedToolJson).expected.error.unwrap()),
        ],
        "malformed_fixture",
    )
    .await;
}
#[tokio::test]
async fn broken_native_continuation_on_failed_finish_fails() {
    broken(
        CaseKind::NativeState,
        vec![
            start(),
            Step::Event(ProviderEvent::Finished {
                outcome: ProviderOutcome::Failed,
                finish_reason: None,
                error: Some(error(ModelErrorCode::ProviderFailure)),
                continuation: Some(NativeContinuation {
                    schema: "handle".into(),
                    payload: vec![0],
                }),
            }),
        ],
        "continuation_on_failure",
    )
    .await;
}
#[tokio::test]
async fn broken_native_schema_individual_and_aggregate_bounds_fail() {
    broken(
        CaseKind::NativeState,
        vec![start(), Step::provider_state(0, "", vec![])],
        "native_schema",
    )
    .await;
    broken(
        CaseKind::NativeState,
        vec![
            start(),
            Step::provider_state(0, "state", vec![0; 256 * 1024 + 1]),
        ],
        "native_bounds",
    )
    .await;
    let mut script = vec![start()];
    for index in 0..16 {
        script.push(Step::provider_state(index, "state", vec![0; 256 * 1024]));
    }
    script.push(Step::completed_with_continuation(
        FinishReason::Stop,
        NativeContinuation {
            schema: "handle".into(),
            payload: vec![0],
        },
    ));
    broken(CaseKind::NativeState, script, "native_bounds").await;
    broken(
        CaseKind::NativeState,
        vec![
            start(),
            Step::completed_with_continuation(
                FinishReason::Stop,
                NativeContinuation {
                    schema: "handle".into(),
                    payload: vec![0; 256 * 1024 + 1],
                },
            ),
        ],
        "native_bounds",
    )
    .await;
    broken(
        CaseKind::NativeState,
        vec![start(), Step::completed(FinishReason::Stop)],
        "normalized_outcome",
    )
    .await;
}
#[tokio::test(start_paused = true)]
async fn broken_control_ignored_cancellation_and_wrong_phase_fail() {
    broken(CaseKind::CancelSetup, vec![Step::Stall], "control_grace").await;
    broken(
        CaseKind::CancelStreaming,
        vec![start(), Step::Stall],
        "control_grace",
    )
    .await;
    broken(
        CaseKind::CancelSetup,
        vec![start(), Step::AwaitCancel, Step::cancelled()],
        "control_phase",
    )
    .await;
    broken(
        CaseKind::CancelStreaming,
        vec![Step::AwaitCancel, Step::cancelled()],
        "control_grace",
    )
    .await;
    broken(
        CaseKind::CancelSetup,
        vec![Step::completed(FinishReason::Stop)],
        "control_not_pending",
    )
    .await;
    broken(
        CaseKind::CancelStreaming,
        vec![
            start(),
            Step::AwaitCancel,
            Step::completed(FinishReason::Stop),
        ],
        "cancel_outcome",
    )
    .await;
    broken(CaseKind::Deadline, vec![Step::Stall], "control_grace").await;
}
#[tokio::test]
async fn control_accepts_eof_and_honored_or_ignored_deadline() {
    for kind in [CaseKind::CancelSetup, CaseKind::CancelStreaming] {
        let mut script = Vec::new();
        if kind == CaseKind::CancelStreaming {
            script.push(start());
        }
        script.push(Step::AwaitCancel);
        let script = Mutex::new(Some(script));
        run_case(
            &|_: &str| fixture(script.lock().unwrap().take().unwrap()),
            &case(kind),
        )
        .await
        .unwrap();
    }
    run_case(
        &|_: &str| fixture(vec![Step::cancelled()]),
        &case(CaseKind::Deadline),
    )
    .await
    .unwrap();
    run_case(&factory, &case(CaseKind::Deadline)).await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn broken_drop_transport_probe_fails() {
    let broken = |name: &str| {
        let mut fixture = factory(name);
        let original = fixture.observe;
        fixture.observe = Arc::new(move || {
            let mut observation = original();
            observation.released = false;
            observation
        });
        fixture
    };
    assert_eq!(
        run_case(&broken, &case(CaseKind::Drop))
            .await
            .unwrap_err()
            .check,
        "drop_release"
    );
}
#[tokio::test]
async fn broken_native_request_drops_tools_options_and_error_name_fail() {
    for mutation in 0..5 {
        let factory = |name: &str| {
            let mut fixture = factory(name);
            let original = fixture.observe;
            fixture.observe = Arc::new(move || {
                let mut observation = original();
                let submitted = observation.submitted.as_mut().unwrap();
                match mutation {
                    0 => submitted.tools.clear(),
                    1 => submitted.tools[0].description.clear(),
                    2 => submitted.options.max_output_tokens = 0,
                    3 => submitted.options.reasoning_effort = None,
                    _ => submitted.options.provider_options.clear(),
                }
                observation
            });
            fixture
        };
        assert_eq!(
            run_case(&factory, &case(CaseKind::Request))
                .await
                .unwrap_err()
                .check,
            "request_preservation"
        );
    }
    let mut wrong = case(CaseKind::UnsupportedOption).expected.error.unwrap();
    wrong.message = "unsupported capability".into();
    broken(
        CaseKind::UnsupportedOption,
        vec![start(), Step::failed(wrong)],
        "normalized_outcome",
    )
    .await;
    let mut invalid = case(CaseKind::UnsupportedOption);
    invalid.expected.error.as_mut().unwrap().message = "unsupported capability".into();
    let invalid_error = invalid.expected.error.clone().unwrap();
    assert_eq!(
        run_case(
            &|_: &str| fixture(vec![start(), Step::failed(invalid_error.clone())]),
            &invalid
        )
        .await
        .unwrap_err()
        .check,
        "unsupported_option"
    );
}

#[tokio::test]
async fn cancellation_during_streaming_waits_for_async_setup_first() {
    let factory = |_: &str| {
        let (release, wait) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let _ = release.send(());
        });
        fixture(vec![
            Step::Wait(wait),
            start(),
            Step::text(0, "partial"),
            Step::AwaitCancel,
            Step::cancelled(),
        ])
    };
    run_case(&factory, &case(CaseKind::CancelStreaming))
        .await
        .unwrap();
}
#[tokio::test]
async fn broken_unbounded_script_fails() {
    let mut script = vec![start()];
    script.extend((0..4096).map(|_| Step::text(0, "x")));
    broken(CaseKind::Text, script, "event_bound").await;
}

#[tokio::test]
async fn pre_submission_control_and_setup_failure_need_no_native_request() {
    let unsubmitted = |name: &str| {
        let mut fixture = factory(name);
        let original = fixture.observe;
        fixture.observe = Arc::new(move || {
            let mut observation = original();
            observation.submitted = None;
            observation
        });
        fixture
    };
    for kind in [
        CaseKind::CancelSetup,
        CaseKind::SetupFailure,
        CaseKind::Drop,
        CaseKind::Deadline,
    ] {
        run_case(&unsubmitted, &case(kind)).await.unwrap();
    }
    assert_eq!(
        run_case(&unsubmitted, &case(CaseKind::Request))
            .await
            .unwrap_err()
            .check,
        "request_preservation"
    );
}

#[tokio::test]
async fn native_state_accepts_optional_continuation_but_failure_still_rejects_it() {
    for continuation in [false, true] {
        let mut row = case(CaseKind::NativeState);
        let mut script = steps(CaseKind::NativeState);
        if !continuation {
            row.expected.continuation = None;
            *script.last_mut().unwrap() = Step::completed(FinishReason::Stop);
        }
        let script = Mutex::new(Some(script));
        run_case(
            &|_: &str| fixture(script.lock().unwrap().take().unwrap()),
            &row,
        )
        .await
        .unwrap();
    }
    let mut finish = Step::failed(error(ModelErrorCode::ProviderFailure));
    if let Step::Event(ProviderEvent::Finished { continuation, .. }) = &mut finish {
        *continuation = Some(NativeContinuation {
            schema: "handle.v1".into(),
            payload: vec![4, 5],
        });
    }
    broken(
        CaseKind::ProviderError,
        vec![start(), finish],
        "continuation_on_failure",
    )
    .await;
}

#[tokio::test]
async fn broken_native_state_on_failed_cancelled_and_eof_finishes_fails() {
    broken(
        CaseKind::NativeState,
        vec![
            start(),
            Step::provider_state(0, "state", vec![0]),
            Step::failed(error(ModelErrorCode::ProviderFailure)),
        ],
        "native_state_on_failure",
    )
    .await;
    broken(
        CaseKind::NativeState,
        vec![
            start(),
            Step::provider_state(0, "state", vec![0]),
            Step::cancelled(),
        ],
        "native_state_on_failure",
    )
    .await;
    broken(
        CaseKind::CancelStreaming,
        vec![
            start(),
            Step::provider_state(0, "state", vec![0]),
            Step::AwaitCancel,
        ],
        "native_state_on_failure",
    )
    .await;
}
