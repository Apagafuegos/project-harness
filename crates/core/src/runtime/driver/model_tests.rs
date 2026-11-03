//! M4 driver proof: request replay survives a worker crash, and deadline
//! enforcement does not depend on adapter cooperation.
use super::*;
use crate::model::{ArtifactReadError, NativeContinuation};

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn switch_required_media_rejected_under_both_policies() {
    use crate::context::{InputMediaKind, ModelCapabilities, ModelInfo, RequiredCapability};
    for (kind, capability) in [
        (InputMediaKind::Image, RequiredCapability::Image),
        (InputMediaKind::Audio, RequiredCapability::Audio),
        (InputMediaKind::File, RequiredCapability::File),
    ] {
        let mut settings = config();
        let other = ModelRef {
            provider_instance: "mock".into(),
            model: "text-only".into(),
        };
        settings.runtime.context.catalog.insert(
            test_model(),
            ModelInfo {
                limits: ModelLimits {
                    context_tokens: Some(1_000_000),
                    max_output_tokens: None,
                },
                capabilities: ModelCapabilities {
                    input_media: [kind].into(),
                    ..Default::default()
                },
            },
        );
        settings.runtime.context.catalog.insert(
            other.clone(),
            ModelLimits {
                context_tokens: Some(1_000_000),
                max_output_tokens: None,
            },
        );
        let context = settings.runtime.context.clone();
        let (fixture, provider) = Fixture::scripted(
            settings,
            WorkerOptions::default(),
            [reply("seen"), reply("converted"), reply("restored")],
        )
        .await;
        let artifact = fixture
            .handle
            .put_artifact(b"media".to_vec())
            .await
            .unwrap();
        let block = match kind {
            InputMediaKind::Image => ContentBlock::Image {
                artifact,
                mime_type: "image/png".into(),
            },
            InputMediaKind::Audio => ContentBlock::Audio {
                artifact,
                mime_type: "audio/wav".into(),
            },
            InputMediaKind::File => ContentBlock::File {
                artifact,
                mime_type: "application/pdf".into(),
                name: "file.pdf".into(),
            },
        };
        let Command::StartTurn { command_id, .. } = Fixture::command(1) else {
            unreachable!()
        };
        fixture
            .handle
            .command(Command::StartTurn {
                command_id,
                content: vec![block.clone()],
            })
            .await
            .unwrap();
        let before = fixture.wait(idle).await;
        for policy in [Portability::Reject, Portability::OmitOptional] {
            let rejection = fixture
                .handle
                .command(Command::SwitchModel {
                    model: other.clone(),
                    policy,
                })
                .await
                .unwrap_err();
            let Rejection::PortabilityLoss(preview) = rejection else {
                panic!("expected portability_loss")
            };
            assert!(preview.would_stop_being_portable.is_empty());
            assert_eq!(preview.required_incompatibilities.len(), 1);
            assert_eq!(preview.required_incompatibilities[0].capability, capability);
            assert_eq!(
                preview.required_incompatibilities[0].part,
                crate::context::Part::Block { index: 0 }
            );
            assert_eq!(
                fixture.handle.view().await.unwrap().snapshot.sequence(),
                before.snapshot.sequence()
            );
        }
        let accepted = fixture
            .handle
            .command(Command::SwitchModel {
                model: other.clone(),
                policy: Portability::ConvertSupported,
            })
            .await
            .unwrap();
        let Accepted::ModelSwitched { preview, .. } = accepted else {
            panic!("switch")
        };
        assert_eq!(preview.would_convert.len(), 1);
        assert_eq!(preview.would_convert[0].capability, capability);
        let switched = fixture.handle.view().await.unwrap();
        let configuration = crate::context::SessionConfiguration::load(
            &fixture.root,
            switched.snapshot.configuration(),
        )
        .unwrap();
        let builder = crate::context::ContextBuilder::new(&context, &configuration);
        let raw = builder
            .build(&before.snapshot, before.snapshot.head(), Some(&other))
            .unwrap();
        let projected = builder
            .build(&switched.snapshot, switched.snapshot.head(), None)
            .unwrap();
        assert_ne!(raw.projection.fingerprint, projected.projection.fingerprint);
        assert_eq!(
            projected.projection.fingerprint,
            crate::context::fingerprint(&projected.projection.input)
        );
        assert_eq!(
            serde_json::to_vec(&projected.projection.input.items).unwrap(),
            serde_json::to_vec(
                &builder
                    .build(&switched.snapshot, switched.snapshot.head(), None)
                    .unwrap()
                    .projection
                    .input
                    .items
            )
            .unwrap()
        );
        let capable = builder
            .build(
                &switched.snapshot,
                switched.snapshot.head(),
                Some(&test_model()),
            )
            .unwrap();
        assert!(
            matches!(&capable.projection.input.items[0], ContextItem::User { content, .. } if content == &vec![block.clone()])
        );
        assert!(
            capable
                .report
                .included
                .iter()
                .all(|item| item.conversions.is_empty())
        );
        // inspect_context is stable and contains the original source part.
        let report = fixture.handle.inspect_context(None, None).await.unwrap();
        assert_eq!(
            report
                .included
                .iter()
                .flat_map(|item| &item.conversions)
                .collect::<Vec<_>>(),
            preview.would_convert.iter().collect::<Vec<_>>()
        );
        assert_eq!(
            report,
            fixture.handle.inspect_context(None, None).await.unwrap()
        );
        fixture.handle.command(Fixture::command(2)).await.unwrap();
        fixture.wait(idle).await;
        let converted = provider.streams()[1].request.clone();
        let ContextItem::User { content, .. } = &converted.input.items[0] else {
            panic!("user")
        };
        let ContentBlock::Text { text } = &content[0] else {
            panic!("reference")
        };
        assert!(text.contains("omitted for this model:"));
        assert!(text.contains("5 bytes, sha256 721c9525ade2"));
        assert!(converted.artifacts.references().is_empty());
        assert!(converted.input.items.iter().all(|item| !matches!(item, ContextItem::User { content, .. } if content.iter().any(|b| matches!(b, ContentBlock::Image { .. } | ContentBlock::Audio { .. } | ContentBlock::File { .. })))));
        fixture
            .handle
            .command(Command::SwitchModel {
                model: test_model(),
                policy: Portability::ConvertSupported,
            })
            .await
            .unwrap();
        fixture.handle.command(Fixture::command(3)).await.unwrap();
        fixture.wait(idle).await;
        let back = provider.streams()[2].request.clone();
        assert!(
            matches!(&back.input.items[0], ContextItem::User { content, .. } if content == &vec![block.clone()])
        );
        let stored = fixture.handle.view().await.unwrap();
        assert_eq!(
            stored
                .snapshot
                .entry(before.snapshot.head().unwrap())
                .unwrap()
                .entry,
            before
                .snapshot
                .entry(before.snapshot.head().unwrap())
                .unwrap()
                .entry
        );

        fixture.handle.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn convert_supported_switch_back_restores_original_media() {
    use crate::context::{InputMediaKind, ModelCapabilities, ModelInfo};
    let mut settings = config();
    settings.runtime.context.catalog.insert(
        test_model(),
        ModelInfo {
            limits: ModelLimits {
                context_tokens: Some(1_000_000),
                max_output_tokens: None,
            },
            capabilities: ModelCapabilities {
                input_media: [InputMediaKind::Image].into(),
                ..Default::default()
            },
        },
    );
    let (fixture, provider) = Fixture::scripted(
        settings,
        WorkerOptions::default(),
        [reply("seen"), reply("back")],
    )
    .await;
    let image = ContentBlock::Image {
        artifact: fixture
            .handle
            .put_artifact(b"media".to_vec())
            .await
            .unwrap(),
        mime_type: "image/png".into(),
    };
    fixture
        .handle
        .command(Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![image.clone()],
        })
        .await
        .unwrap();
    let original = fixture.wait(idle).await;
    let target = ModelRef {
        provider_instance: "mock".into(),
        model: "text-only".into(),
    };
    fixture
        .handle
        .command(Command::SwitchModel {
            model: target,
            policy: Portability::ConvertSupported,
        })
        .await
        .unwrap();
    fixture
        .handle
        .command(Command::SwitchModel {
            model: test_model(),
            policy: Portability::ConvertSupported,
        })
        .await
        .unwrap();
    fixture.handle.command(Fixture::command(2)).await.unwrap();
    let back = fixture.wait(idle).await;
    assert!(
        matches!(&provider.streams()[1].request.input.items[0], ContextItem::User { content, .. } if content == &vec![image])
    );
    for (id, stored) in original.snapshot.entries() {
        assert_eq!(back.snapshot.entry(id).unwrap().entry, stored.entry);
    }
    fixture.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn switch_required_tools_rejected_and_clean_switch_policies() {
    use crate::context::{ModelCapabilities, ModelInfo, RequiredCapability};
    let mut settings = config();
    let other = ModelRef {
        provider_instance: "mock".into(),
        model: "no-tools".into(),
    };
    settings.runtime.context.catalog.insert(
        other.clone(),
        ModelInfo {
            capabilities: ModelCapabilities {
                tools: false,
                ..Default::default()
            },
            limits: ModelLimits {
                context_tokens: Some(1_000_000),
                max_output_tokens: None,
            },
        },
    );
    let (fixture, provider) = Fixture::scripted(
        settings,
        WorkerOptions::default(),
        [
            Script::new(tool_script().into_iter().map(Step::Event)),
            reply("done"),
            reply("converted"),
        ],
    )
    .await;
    // A clean projection succeeds under either supported policy, even with unknown limits.
    for policy in [Portability::Reject, Portability::OmitOptional] {
        let accepted = fixture
            .handle
            .command(Command::SwitchModel {
                model: test_model(),
                policy,
            })
            .await
            .unwrap();
        let Accepted::ModelSwitched { preview, .. } = accepted else {
            panic!("switch")
        };
        assert!(preview.would_stop_being_portable.is_empty());
        assert!(preview.required_incompatibilities.is_empty());
    }
    assert!(matches!(
        Command::switch_model(other.clone()),
        Command::SwitchModel {
            policy: Portability::Reject,
            ..
        }
    ));
    fixture.handle.command(Fixture::command(1)).await.unwrap();
    let before = fixture.wait(idle).await;
    for policy in [Portability::Reject, Portability::OmitOptional] {
        let rejected = fixture
            .handle
            .command(Command::SwitchModel {
                model: other.clone(),
                policy,
            })
            .await
            .unwrap_err();
        let Rejection::PortabilityLoss(preview) = rejected else {
            panic!("portability_loss")
        };
        assert_eq!(preview.required_incompatibilities.len(), 2);
        assert!(
            preview
                .required_incompatibilities
                .iter()
                .all(|part| part.capability == RequiredCapability::Tools)
        );
        assert_eq!(
            fixture.handle.view().await.unwrap().snapshot.sequence(),
            before.snapshot.sequence()
        );
    }
    let Accepted::ModelSwitched { preview, .. } = fixture
        .handle
        .command(Command::SwitchModel {
            model: other,
            policy: Portability::ConvertSupported,
        })
        .await
        .unwrap()
    else {
        panic!("switch")
    };
    assert_eq!(preview.would_convert.len(), 2);
    let report = fixture.handle.inspect_context(None, None).await.unwrap();
    assert_eq!(
        report
            .included
            .iter()
            .flat_map(|item| &item.conversions)
            .count(),
        2
    );
    fixture.handle.command(Fixture::command(2)).await.unwrap();
    fixture.wait(idle).await;
    let input = provider.streams()[2].request.input.clone();
    assert!(input.tools.is_empty());
    assert!(input.items.iter().all(|item| {
        match item {
            ContextItem::ToolResult { .. } => false,
            ContextItem::Assistant { content, .. } => !content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolCall { .. })),
            _ => true,
        }
    }));
    let text = input
        .items
        .iter()
        .flat_map(|item| match item {
            ContextItem::Assistant { content, .. } | ContextItem::User { content, .. } => content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();
    let call = text
        .iter()
        .position(|text| text.starts_with("[tool call "))
        .unwrap();
    let result = text
        .iter()
        .position(|text| text.starts_with("[tool result "))
        .unwrap();
    assert_eq!(result, call + 1);
    assert!(text[result].contains("denied"));
    fixture.handle.shutdown().await;
}

fn native_reply(payload: &'static [u8]) -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, "answer"),
        Step::provider_state(1, "anthropic.messages.thinking.v1", serde_json::to_vec(&serde_json::json!({"thinking": String::from_utf8_lossy(payload), "signature": "SIGNED_THINKING_SENTINEL"})).unwrap()),
        Step::completed_with_continuation(
            FinishReason::Stop,
            NativeContinuation {
                schema: "mock.continuation.v1".into(),
                payload: b"native handle".to_vec(),
            },
        ),
    ])
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn native_replay_survives_crash_reopen_and_is_omitted_after_model_switch() {
    native_switch_omission(Portability::OmitOptional).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn convert_supported_opaque_state_is_omitted_never_textualized() {
    native_switch_omission(Portability::ConvertSupported).await;
}

async fn native_switch_omission(policy: Portability) {
    let mut settings = config();
    let other = ModelRef {
        provider_instance: "mock".into(),
        model: "other".into(),
    };
    settings.runtime.context.catalog.insert(
        other.clone(),
        ModelLimits {
            context_tokens: Some(1_000_000),
            max_output_tokens: None,
        },
    );
    let (fixture, provider) = Fixture::scripted(
        settings,
        WorkerOptions::default(),
        [
            native_reply(b"first state"),
            native_reply(b"second state"),
            reply("switched"),
            reply("back"),
        ],
    )
    .await;
    fixture.handle.command(Fixture::command(1)).await.unwrap();
    let first = fixture.wait(idle).await;
    let first_response = responses(&first)[0].clone();
    let first_continuation = first_response.continuation.as_ref().unwrap();
    assert_eq!(first_continuation.owner, test_model());
    assert_eq!(
        first_continuation.input_fingerprint,
        first_response.projection_fingerprint.clone().unwrap()
    );

    // Simulate process death before another opening append; the worker exits
    // and releases the writer. The new runtime rebuilds only durable history.
    fixture.options.panic_next.store(true, Ordering::SeqCst);
    assert_eq!(
        fixture.handle.command(Fixture::command(2)).await,
        Err(Rejection::Faulted)
    );
    fixture.wait(|v| v.phase == Phase::Faulted).await;
    assert!(idle(&fixture.handle.reopen().await.unwrap()));
    assert_eq!(provider.streams().len(), 1, "recovery must not infer");
    fixture.handle.command(Fixture::command(3)).await.unwrap();
    fixture.wait(idle).await;
    let second = provider.streams()[1].request.clone();
    assert_eq!(second.input.model, test_model());
    assert_eq!(second.input.continuation.as_ref(), Some(first_continuation));
    let state = second
        .input
        .items
        .iter()
        .find_map(|item| match item {
            ContextItem::Assistant { content, .. } => {
                content.iter().find_map(|block| match block {
                    ContentBlock::ProviderState {
                        owner, artifact, ..
                    } => {
                        assert_eq!(owner, &second.input.model);
                        Some(artifact.clone())
                    }
                    _ => None,
                })
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        second.artifacts.read(&state).await.unwrap(),
        br#"{"signature":"SIGNED_THINKING_SENTINEL","thinking":"first state"}"#
    );
    assert_eq!(
        second
            .artifacts
            .read(&first_continuation.artifact)
            .await
            .unwrap(),
        b"native handle"
    );
    assert_eq!(
        second.artifacts.read(first.snapshot.configuration()).await,
        Err(ArtifactReadError::NotReferenced)
    );
    let widened = ArtifactRef {
        bytes: state.bytes + 1,
        ..state.clone()
    };
    assert_eq!(
        second.artifacts.read(&widened).await,
        Err(ArtifactReadError::NotReferenced)
    );

    let before = fixture.handle.view().await.unwrap().snapshot.sequence();
    let rejected = fixture
        .handle
        .command(Command::SwitchModel {
            model: other.clone(),
            policy: Portability::Reject,
        })
        .await
        .unwrap_err();
    let Rejection::PortabilityLoss(preview) = rejected else {
        panic!("expected portability_loss")
    };
    assert_eq!(preview.would_stop_being_portable.len(), 4);
    assert!(preview.required_incompatibilities.is_empty());
    assert_eq!(
        fixture.handle.view().await.unwrap().snapshot.sequence(),
        before
    );
    let accepted = fixture
        .handle
        .command(Command::SwitchModel {
            model: other.clone(),
            policy,
        })
        .await
        .unwrap();
    let Accepted::ModelSwitched {
        preview: accepted_preview,
        ..
    } = accepted
    else {
        panic!("expected switch")
    };
    assert_eq!(accepted_preview, preview);
    fixture.handle.command(Fixture::command(4)).await.unwrap();
    fixture.wait(idle).await;
    let third = provider.streams()[2].request.clone();
    assert_eq!(third.input.model, other);
    assert!(third.input.continuation.is_none());
    assert!(third.input.items.iter().all(|item| {
        match item {
            ContextItem::Assistant { content, .. } => !content
                .iter()
                .any(|block| matches!(block, ContentBlock::ProviderState { .. })),
            _ => true,
        }
    }));
    assert!(third.artifacts.references().is_empty());
    let serialized = serde_json::to_string(&third.input.items).unwrap();
    assert!(!serialized.contains("anthropic.messages.thinking.v1"));
    assert!(!serialized.contains("SIGNED_THINKING_SENTINEL"));
    assert!(!serialized.contains("mock.continuation.v1"));
    assert!(!serialized.contains(state.sha256.as_str()));
    assert!(!serialized.contains("first state"));
    assert_eq!(
        third.artifacts.read(&state).await,
        Err(ArtifactReadError::NotReferenced)
    );
    fixture
        .handle
        .command(Command::SwitchModel {
            model: test_model(),
            policy: Portability::Reject,
        })
        .await
        .unwrap();
    fixture.handle.command(Fixture::command(5)).await.unwrap();
    fixture.wait(idle).await;
    let back = provider.streams()[3].request.clone();
    assert!(
        back.input.continuation.is_none(),
        "B is now the latest response"
    );
    assert!(back.input.items.iter().any(|item| matches!(item,
        ContextItem::Assistant { content, .. } if content.iter().any(|block|
            matches!(block, ContentBlock::ProviderState { owner, .. } if owner == &test_model()))
    )));
    fixture.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn inference_deadline_ignores_progress_and_bounds_an_adapter_ignoring_cancellation() {
    for cooperative in [false, true] {
        let (release, wait) = oneshot::channel();
        let mut settings = config();
        settings.runtime.inference_deadline = Some(Duration::from_secs(10));
        let grace = settings.runtime.cancel_grace;
        let end = if cooperative {
            vec![Step::AwaitCancel, Step::cancelled()]
        } else {
            vec![Step::Stall]
        };
        let script = Script::new(
            [
                Step::Event(ProviderEvent::Started),
                Step::text(0, "a"),
                Step::Wait(wait),
                Step::text(0, "b"),
            ]
            .into_iter()
            .chain(end),
        );
        let (fixture, provider) =
            Fixture::scripted(settings, WorkerOptions::default(), [script]).await;
        let started = tokio::time::Instant::now();
        fixture.handle.command(Fixture::command(1)).await.unwrap();
        fixture
            .wait(|v| {
                v.partial
                    .as_ref()
                    .is_some_and(|p| p.blocks.get(&0) == Some(&PartialBlock::Text("a".into())))
            })
            .await;
        assert_eq!(
            provider.streams()[0].deadline,
            Some(started + Duration::from_secs(10))
        );
        tokio::time::advance(Duration::from_secs(9)).await;
        release.send(()).unwrap();
        fixture
            .wait(|v| {
                v.partial
                    .as_ref()
                    .is_some_and(|p| p.blocks.get(&0) == Some(&PartialBlock::Text("ab".into())))
            })
            .await;
        tokio::time::advance(Duration::from_secs(1)).await;
        fixture.wait(|_| provider.streams()[0].cancelled).await;
        if !cooperative {
            assert!(!provider.streams()[0].dropped);
            tokio::time::advance(grace).await;
        }
        let view = fixture.wait(idle).await;
        assert_eq!(
            finishes(&view),
            [(TurnOutcome::Failed, Some("inference_deadline".into()))]
        );
        assert_eq!(responses(&view)[0].outcome, ResponseOutcome::Interrupted);
        assert_eq!(
            responses(&view)[0].content,
            [ContentBlock::Text { text: "ab".into() }]
        );
        assert!(provider.streams()[0].dropped);
        fixture.handle.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn inference_deadline_covers_setup_and_default_has_no_deadline() {
    let mut settings = config();
    settings.runtime.inference_deadline = Some(Duration::from_secs(10));
    let (fixture, provider) = Fixture::scripted(
        settings,
        WorkerOptions::default(),
        [Script::new([Step::Stall])],
    )
    .await;
    fixture.handle.command(Fixture::command(1)).await.unwrap();
    fixture.wait(|v| v.partial.is_some()).await;
    tokio::time::advance(Duration::from_secs(10)).await;
    fixture.wait(|_| provider.streams()[0].cancelled).await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let view = fixture.wait(idle).await;
    assert_eq!(
        finishes(&view),
        [(TurnOutcome::Failed, Some("inference_deadline".into()))]
    );
    assert!(responses(&view)[0].content.is_empty());
    fixture.handle.shutdown().await;

    let (fixture, provider) =
        Fixture::scripted(config(), WorkerOptions::default(), [reply("no deadline")]).await;
    fixture.handle.command(Fixture::command(1)).await.unwrap();
    fixture.wait(idle).await;
    assert_eq!(provider.streams()[0].deadline, None);
    fixture.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn request_source_reads_verified_media_and_refuses_unreferenced_artifacts() {
    let (fixture, provider) =
        Fixture::scripted(config(), WorkerOptions::default(), [reply("media")]).await;
    let media = fixture
        .handle
        .put_artifact(b"media bytes".to_vec())
        .await
        .unwrap();
    let Command::StartTurn { command_id, .. } = Fixture::command(1) else {
        unreachable!()
    };
    fixture
        .handle
        .command(Command::StartTurn {
            command_id,
            content: vec![ContentBlock::Image {
                artifact: media.clone(),
                mime_type: "image/png".into(),
            }],
        })
        .await
        .unwrap();
    fixture.wait(idle).await;
    let request = provider.streams()[0].request.clone();
    assert_eq!(request.artifacts.references(), std::slice::from_ref(&media));
    assert_eq!(
        request.artifacts.read(&media).await.unwrap(),
        b"media bytes"
    );
    fixture.handle.shutdown().await;
}
