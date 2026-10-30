//! Driver execution/cancellation/deadline and read-only inspection for C02.
use super::*;
use crate::context::compaction::{CompactionError, NotEligible};

const POLICY: &[u8] = br#"{"compaction":{"summary_model":{"provider_instance":"summary","model":"cheap"},"automatic":false}}"#;
const SUMMARY: &str = "## Task and constraints\nKeep task.\n## Decisions\nRust.\n## Files and actions\nRead succeeded.\n## Outcomes\nDone.\n## Unresolved work\nNone.\n";
fn summary() -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, SUMMARY),
        Step::completed(FinishReason::Stop),
    ])
}
async fn prepared(extra: impl IntoIterator<Item = Script>) -> (Fixture, Arc<ScriptedProvider>) {
    let scripts = (0..4).map(|_| reply("done")).chain(extra);
    let provider = Arc::new(ScriptedProvider::new(scripts));
    let fixture = Fixture::with_configuration(
        config(),
        WorkerOptions::default(),
        Some(provider.clone()),
        POLICY,
    )
    .await;
    for n in 1..=4 {
        fixture.handle.command(Fixture::command(n)).await.unwrap();
        fixture.wait(idle).await;
    }
    (fixture, provider)
}
async fn streams(provider: &ScriptedProvider, count: usize) {
    for _ in 0..100_000 {
        if provider.streams().len() >= count {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("expected {count} streams");
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn driver_manual_preview_inspection_diff_commit_and_single_use() {
    let (fixture, provider) = prepared([summary()]).await;
    let view = fixture.handle.view().await.unwrap();
    let Accepted::CompactionPreviewed {
        preview_id,
        preview,
    } = fixture
        .handle
        .command(Command::PreviewCompaction)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        fixture.handle.view().await.unwrap().snapshot.sequence(),
        view.snapshot.sequence()
    );
    assert_eq!(provider.streams().len(), 5);
    assert_eq!(
        provider.streams()[4].request.input.model.provider_instance,
        "summary"
    );
    assert_eq!(
        fixture
            .handle
            .inspect_context_preview(preview_id.clone())
            .await
            .unwrap(),
        *preview
    );
    assert_eq!(
        fixture.handle.inspect_context(None, None).await.unwrap(),
        preview.before
    );
    assert_eq!(provider.streams().len(), 5, "inspection never regenerates");
    let accepted = fixture
        .handle
        .command(Command::CommitCompaction {
            preview_id: preview_id.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(accepted, Accepted::CompactionCommitted { .. }));
    let after = fixture.handle.inspect_context(None, None).await.unwrap();
    let mut expected = preview.after.clone();
    expected.identity.journal_sequence = after.identity.journal_sequence;
    assert_eq!(after, expected);
    assert!(matches!(
        fixture
            .handle
            .command(Command::CommitCompaction { preview_id })
            .await,
        Err(Rejection::StalePreview { .. })
    ));
    assert!(provider.streams().iter().all(|s| s.dropped));
    fixture.handle.shutdown().await;
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn driver_cancel_and_start_turn_drop_summary_stream() {
    let (fixture, provider) = prepared([
        Script::new([Step::Stall]),
        Script::new([Step::Stall]),
        reply("done"),
    ])
    .await;
    let handle = fixture.handle.clone();
    let pending = tokio::spawn(async move { handle.command(Command::PreviewCompaction).await });
    streams(&provider, 5).await;
    assert_eq!(
        fixture
            .handle
            .command(Command::CancelCompaction)
            .await
            .unwrap(),
        Accepted::CompactionCancelled
    );
    assert_eq!(
        pending.await.unwrap(),
        Err(Rejection::Compaction(CompactionError::Cancelled))
    );
    assert!(provider.streams()[4].cancelled && provider.streams()[4].dropped);
    let handle = fixture.handle.clone();
    let pending = tokio::spawn(async move { handle.command(Command::PreviewCompaction).await });
    streams(&provider, 6).await;
    fixture.handle.command(Fixture::command(5)).await.unwrap();
    assert_eq!(
        pending.await.unwrap(),
        Err(Rejection::Compaction(CompactionError::Cancelled))
    );
    fixture.wait(idle).await;
    assert!(provider.streams()[5].cancelled && provider.streams()[5].dropped);
    assert_eq!(provider.streams().len(), 7);
    fixture.handle.shutdown().await;
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn driver_summary_deadline_is_absolute_and_cancels_ignoring_provider() {
    let (fixture, provider) = prepared([Script::new([Step::Stall]), summary()]).await;
    let handle = fixture.handle.clone();
    let pending = tokio::spawn(async move { handle.command(Command::PreviewCompaction).await });
    streams(&provider, 5).await;
    tokio::time::advance(Duration::from_secs(300)).await;
    assert_eq!(
        pending.await.unwrap(),
        Err(Rejection::Compaction(CompactionError::Cancelled))
    );
    assert!(provider.streams()[4].cancelled && provider.streams()[4].dropped);
    assert!(idle(&fixture.handle.view().await.unwrap()));
    assert!(matches!(
        fixture.handle.command(Command::PreviewCompaction).await,
        Ok(Accepted::CompactionPreviewed { .. })
    ));
    fixture.handle.shutdown().await;
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn driver_shutdown_cancels_generation_and_reopen_has_no_preview_or_summary() {
    let (fixture, provider) = prepared([Script::new([Step::Stall])]).await;
    let before = fixture.handle.view().await.unwrap().snapshot.sequence();
    let handle = fixture.handle.clone();
    let pending = tokio::spawn(async move { handle.command(Command::PreviewCompaction).await });
    streams(&provider, 5).await;
    fixture.handle.shutdown().await;
    assert_eq!(
        pending.await.unwrap(),
        Err(Rejection::Compaction(CompactionError::Cancelled))
    );
    assert!(provider.streams()[4].cancelled && provider.streams()[4].dropped);
    let fresh = Arc::new(ScriptedProvider::default());
    let reopened = SessionHandle::open(
        fixture.root.clone(),
        fixture.session.clone(),
        fresh.clone(),
        config(),
    )
    .await
    .unwrap();
    let view = reopened.view().await.unwrap();
    assert!(idle(&view));
    assert_eq!(view.snapshot.sequence(), before);
    for _ in 0..4 {
        let report = reopened.inspect_context(None, None).await.unwrap();
        assert_eq!(report.included.len(), 8);
    }
    assert!(
        fresh.streams().is_empty(),
        "zero streams over a full branch projection"
    );
    reopened.shutdown().await;
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn driver_not_eligible_never_opens_provider() {
    let (fixture, provider) = Fixture::scripted(config(), WorkerOptions::default(), []).await;
    assert_eq!(
        fixture.handle.command(Command::PreviewCompaction).await,
        Err(Rejection::Compaction(CompactionError::NotEligible(
            NotEligible::Empty
        )))
    );
    assert!(provider.streams().is_empty());
    fixture.handle.shutdown().await;
}
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn driver_automatic_events_have_diff_and_fit_or_explicit_failure() {
    for usable in [20, 100] {
        let mut driver_config = config();
        driver_config.runtime.context.compaction_trigger_permille = 1;
        // 20 usable tokens: three hello/done turns reach the trigger, but fit.
        driver_config.runtime.context.catalog.insert(
            test_model(),
            ModelLimits {
                context_tokens: Some(16384 + usable),
                max_output_tokens: None,
            },
        );
        let provider = Arc::new(ScriptedProvider::new([
            reply("done"),
            reply("done"),
            reply("done"),
            summary(),
        ]));
        let fixture = Fixture::with_configuration(
            driver_config,
            WorkerOptions::default(),
            Some(provider.clone()),
            br#"{"compaction":{"summary_model":{"provider_instance":"summary","model":"cheap"}}}"#,
        )
        .await;
        let mut subscription = fixture.handle.subscribe();
        subscription.next().await;
        for n in 1..=3 {
            fixture.handle.command(Fixture::command(n)).await.unwrap();
            fixture.wait(idle).await;
        }
        // Summary text cannot fit this tiny budget; failure is explicit and no entry is written.
        let mut saw_preview = false;
        loop {
            match subscription.next().await.unwrap() {
                SubscriptionItem::Event(RuntimeEvent {
                    kind:
                        EventKind::CompactionPreviewed {
                            automatic, preview, ..
                        },
                    ..
                }) => {
                    assert!(automatic);
                    assert_eq!(preview.diff.fits_budget, usable == 100);
                    saw_preview = true;
                }
                SubscriptionItem::Event(RuntimeEvent {
                    kind:
                        EventKind::CompactionFailed {
                            code: "context_budget_exceeded",
                        },
                    ..
                }) if saw_preview => {
                    assert_eq!(usable, 20);
                    break;
                }
                SubscriptionItem::Event(RuntimeEvent {
                    kind: EventKind::CompactionCommitted { preview, .. },
                    ..
                }) => {
                    assert_eq!(usable, 100);
                    assert!(saw_preview && preview.diff.fits_budget);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(provider.streams().len(), 4);
        assert_eq!(
            fixture
                .handle
                .view()
                .await
                .unwrap()
                .snapshot
                .entries()
                .filter(|(_, s)| matches!(s.entry.payload, EntryPayload::Compaction(_)))
                .count(),
            usize::from(usable == 100)
        );
        fixture.handle.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stale_compaction_epoch_cannot_alias_new_runtime_token() {
    let (mut fixture, provider) =
        prepared([Script::new([Step::Stall]), Script::new([Step::Stall])]).await;
    fixture.handle.shutdown().await;
    fixture.handle = SessionHandle::open(
        fixture.root.clone(),
        fixture.session.clone(),
        provider.clone(),
        config(),
    )
    .await
    .unwrap();
    let old_epoch = fixture.handle.view().await.unwrap().epoch;
    let handle = fixture.handle.clone();
    let pending = tokio::spawn(async move { handle.command(Command::PreviewCompaction).await });
    streams(&provider, 5).await;
    let (reply, receive) = oneshot::channel();
    fixture
        .handle
        .commands
        .send(Message::Inject(
            Input::DriverInvariant {
                code: "injected_fault",
            },
            reply,
        ))
        .await
        .unwrap();
    receive.await.unwrap();
    assert_eq!(pending.await.unwrap(), Err(Rejection::Faulted));
    fixture.handle.reopen().await.unwrap();
    let handle = fixture.handle.clone();
    let pending = tokio::spawn(async move { handle.command(Command::PreviewCompaction).await });
    streams(&provider, 6).await;
    let before = fixture.handle.view().await.unwrap();
    assert_ne!(before.epoch, old_epoch);
    let (reply, receive) = oneshot::channel();
    fixture
        .handle
        .commands
        .send(Message::CompactionAnswer(
            old_epoch,
            CompactionToken(1),
            Err(CompactionError::Invalid("late_failure")),
            reply,
        ))
        .await
        .unwrap();
    receive.await.unwrap();
    assert_eq!(
        fixture.handle.view().await.unwrap().event_seq,
        before.event_seq
    );
    assert_eq!(
        fixture
            .handle
            .command(Command::CancelCompaction)
            .await
            .unwrap(),
        Accepted::CompactionCancelled
    );
    assert_eq!(
        pending.await.unwrap(),
        Err(Rejection::Compaction(CompactionError::Cancelled))
    );
    assert!(
        provider.streams()[4..]
            .iter()
            .all(|s| s.cancelled && s.dropped)
    );
    fixture.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn driver_compacts_before_overflowing_start_and_opens_only_session_inference_after_ack() {
    let mut driver_config = config();
    driver_config.runtime.context.catalog.insert(
        test_model(),
        ModelLimits {
            context_tokens: Some(17384),
            max_output_tokens: None,
        },
    );
    let provider = Arc::new(ScriptedProvider::new(
        (0..4)
            .map(|_| reply("done"))
            .chain([summary(), reply("final")]),
    ));
    let fixture = Fixture::with_configuration(
        driver_config,
        WorkerOptions::default(),
        Some(provider.clone()),
        br#"{"compaction":{"summary_model":{"provider_instance":"summary","model":"cheap"}}}"#,
    )
    .await;
    let large_command = |n| {
        let Command::StartTurn { command_id, .. } = Fixture::command(n) else {
            unreachable!()
        };
        Command::StartTurn {
            command_id,
            content: vec![ContentBlock::Text {
                text: "u".repeat(800),
            }],
        }
    };
    for n in 1..=4 {
        fixture.handle.command(large_command(n)).await.unwrap();
        fixture.wait(idle).await;
    }
    assert_eq!(provider.streams().len(), 4);
    let Accepted::TurnStarted { ack, .. } = fixture.handle.command(large_command(5)).await.unwrap()
    else {
        panic!()
    };
    fixture.wait(idle).await;
    let view = fixture.handle.view().await.unwrap();
    let summary = view
        .snapshot
        .entries()
        .find(|(_, s)| matches!(s.entry.payload, EntryPayload::Compaction(_)))
        .unwrap()
        .1;
    assert!(summary.sequence < ack.sequence);
    assert_eq!(provider.streams().len(), 6);
    assert_eq!(
        provider.streams()[4].request.input.model.provider_instance,
        "summary"
    );
    assert_eq!(provider.streams()[5].request.input.model, test_model());
    fixture.handle.shutdown().await;
}
