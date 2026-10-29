//! M4 driver proof: request replay survives a worker crash, and deadline
//! enforcement does not depend on adapter cooperation.
use super::*;
use crate::model::{ArtifactReadError, NativeContinuation};

fn native_reply(payload: &'static [u8]) -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, "answer"),
        Step::provider_state(1, "mock.state.v1", payload.to_vec()),
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
    assert_eq!(second.artifacts.read(&state).await.unwrap(), b"first state");
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

    fixture
        .handle
        .command(Command::SwitchModel {
            model: other.clone(),
        })
        .await
        .unwrap();
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
    assert_eq!(
        third.artifacts.read(&state).await,
        Err(ArtifactReadError::NotReferenced)
    );
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
