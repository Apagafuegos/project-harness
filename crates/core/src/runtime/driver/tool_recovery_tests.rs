//! Production reopen, storage artifact reads and executor-thread evidence.
use super::*;
use crate::tools::{FsProtectedPaths, FsWorkspace, RecoveryHint};

async fn resolve_unknown(handle: &SessionHandle, id: InvocationId) {
    assert!(matches!(
        bounded(handle.command(Command::ResolveUncertain {
            invocation_id: id,
            resolution: UserResolution::AcknowledgedUnknown
        }))
        .await,
        Ok(Accepted::Resolved { .. })
    ));
}
fn hint(view: &RuntimeView) -> Option<RecoveryHint> {
    view.awaiting.first().and_then(|v| v.hint.clone())
}

#[tokio::test(flavor = "current_thread")]
async fn r01_write_file_states_and_edit_reverse_hint_through_real_fs_reopen() {
    for (tool, current, expected) in [
        ("write", Some("before"), RecoveryHint::MatchesPreImage),
        ("write", Some("after"), RecoveryHint::MatchesPostImage),
        ("write", Some("other"), RecoveryHint::ChangedSince),
        ("write", None, RecoveryHint::Missing),
        ("edit", Some("after"), RecoveryHint::MatchesPostImage),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "before").unwrap();
        let workspace =
            Arc::new(FsWorkspace::open(dir.path(), &FsProtectedPaths::default()).unwrap());
        let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
        executor.panic.store(true, Ordering::Release);
        let args = if tool == "write" {
            json!({"path":"a","text":"after","overwrite":true})
        } else {
            json!({"path":"a","old_text":"before","new_text":"after"})
        };
        let (f, provider) = fixture(
            ToolHost {
                workspace,
                executors: vec![executor.clone()],
                files: None,
            },
            POLICY,
            &[(tool, args), ("read", json!({"path":"a"}))],
        )
        .await;
        start(&f).await;
        approve(&f, false, Decision::Allow).await;
        let before = bounded(f.wait(|v| v.phase == Phase::Faulted)).await;
        assert!(results(&before).is_empty());
        assert_eq!(executor.count(), 1);
        if let Some(current) = current {
            std::fs::write(dir.path().join("a"), current).unwrap();
        } else {
            std::fs::remove_file(dir.path().join("a")).unwrap();
        }
        bounded(f.handle.reopen()).await.unwrap();
        let view = bounded(f.wait(|v| hint(v).is_some())).await;
        assert_eq!(view.phase, Phase::AwaitingResolution);
        assert_eq!(hint(&view), Some(expected));
        assert_eq!(executor.count(), 1);
        assert_eq!(provider.streams().len(), 1);
        let mut sub = f.handle.subscribe();
        let Some(SubscriptionItem::Reset(client)) = bounded(sub.next()).await else {
            panic!("reset")
        };
        assert_eq!(client.view().awaiting, view.awaiting);
        let journal = std::fs::read_to_string(
            f.root
                .path()
                .join("sessions")
                .join(format!("{}.jsonl", f.session)),
        )
        .unwrap();
        for forbidden in [
            "recovery_hints",
            "matches_post_image",
            "matches_pre_image",
            "changed_since",
        ] {
            assert!(!journal.contains(forbidden));
        }
        resolve_unknown(&f.handle, view.awaiting[0].invocation_id.clone()).await;
        let view = bounded(f.wait(idle)).await;
        let results = results(&view);
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].metadata.provenance,
            Provenance::UserAcknowledgedUnknown
        );
        assert_eq!(results[1].metadata.provenance, Provenance::Runtime);
        assert_eq!(results[1].metadata.code.as_deref(), Some("process_restart"));
        assert!(results[1].invocation_id.is_none());
        assert_eq!(executor.count(), 1);
        f.handle.shutdown().await;
    }
}

struct HintWorkspace {
    inner: Workspace,
    hints: Arc<AtomicBool>,
    hold: Option<Arc<Hold>>,
    panic: bool,
}
impl WorkspaceView for HintWorkspace {
    fn identity(&self) -> &WorkspaceIdentity {
        self.inner.identity()
    }
    fn inspect(&self, path: &WorkspacePath) -> Result<FileFacts, Refusal> {
        assert_eq!(std::thread::current().name(), Some("harness-tools"));
        if self.hints.load(Ordering::Acquire) {
            if let Some(hold) = &self.hold {
                hold.wait(None, false);
            }
            assert!(!self.panic, "advisory panic");
        }
        self.inner.inspect(path)
    }
    fn read_text(&self, path: &WorkspacePath, max: u64) -> Result<String, Refusal> {
        self.inner.read_text(path, max)
    }
    fn capture(
        &self,
        plan: &crate::tools::Plan,
        artifacts: &crate::session::DataRoot,
    ) -> Result<crate::session::FileImage, Refusal> {
        self.inner.capture(plan, artifacts)
    }
}
struct Release(Arc<Hold>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_hint_does_not_delay_resolution_and_stale_epoch_is_ignored() {
    let hold = Arc::new(Hold::default());
    let _release = Release(hold.clone());
    let hints = Arc::new(AtomicBool::new(false));
    let workspace = Arc::new(HintWorkspace {
        inner: Workspace::new(),
        hints: hints.clone(),
        hold: Some(hold.clone()),
        panic: false,
    });
    let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
    executor.panic.store(true, Ordering::Release);
    let (f, _) = fixture(
        ToolHost {
            workspace,
            executors: vec![executor.clone()],
            files: None,
        },
        POLICY,
        &[
            ("write", json!({"path":"new","text":"after"})),
            ("read", json!({"path":"a"})),
        ],
    )
    .await;
    start(&f).await;
    approve(&f, false, Decision::Allow).await;
    bounded(f.wait(|v| v.phase == Phase::Faulted)).await;
    hints.store(true, Ordering::Release);
    bounded(f.handle.reopen()).await.unwrap();
    hold.entered().await;
    let before = bounded(f.handle.view()).await.unwrap();
    assert_eq!(before.phase, Phase::AwaitingResolution);
    assert_eq!(hint(&before), None);
    let (send, recv) = oneshot::channel();
    f.handle
        .commands
        .send(Message::ToolAnswer(
            Answer {
                epoch: RuntimeEpoch::new(),
                token: JobToken::Hints(HintToken(1)),
                input: Some(Input::Hints {
                    token: HintToken(1),
                    hints: vec![(
                        before.awaiting[0].invocation_id.clone(),
                        RecoveryHint::Missing,
                    )],
                }),
            },
            send,
        ))
        .await
        .unwrap();
    bounded(recv).await.unwrap();
    assert_eq!(
        bounded(f.handle.view()).await.unwrap().event_seq,
        before.event_seq
    );
    resolve_unknown(&f.handle, before.awaiting[0].invocation_id.clone()).await;
    let view = bounded(f.wait(idle)).await;
    assert_eq!(results(&view).len(), 2);
    hold.release();
    f.handle.shutdown().await;
    assert_eq!(executor.count(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn hint_panic_is_unavailable_not_a_session_fault() {
    let hints = Arc::new(AtomicBool::new(false));
    let workspace = Arc::new(HintWorkspace {
        inner: Workspace::new(),
        hints: hints.clone(),
        hold: None,
        panic: true,
    });
    let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
    executor.panic.store(true, Ordering::Release);
    let (f, _) = fixture(
        ToolHost {
            workspace,
            executors: vec![executor],
            files: None,
        },
        POLICY,
        &[("write", json!({"path":"new","text":"after"}))],
    )
    .await;
    start(&f).await;
    approve(&f, false, Decision::Allow).await;
    bounded(f.wait(|v| v.phase == Phase::Faulted)).await;
    hints.store(true, Ordering::Release);
    bounded(f.handle.reopen()).await.unwrap();
    let view = bounded(f.wait(|v| hint(v).is_some())).await;
    assert_eq!(view.phase, Phase::AwaitingResolution);
    assert_eq!(
        hint(&view),
        Some(RecoveryHint::Unavailable {
            reason: "infrastructure_failure".into()
        })
    );
    resolve_unknown(&f.handle, view.awaiting[0].invocation_id.clone()).await;
    f.handle.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn absent_host_and_unreadable_artifact_are_advice_only() {
    for no_host in [false, true] {
        let executor = Arc::new(Stub::new(ExecutionBackend::TrustedFs));
        executor.panic.store(true, Ordering::Release);
        let (f, _) = fixture(
            host(Workspace::new(), vec![executor]),
            POLICY,
            &[("write", json!({"path":"new","text":"after"}))],
        )
        .await;
        start(&f).await;
        approve(&f, false, Decision::Allow).await;
        bounded(f.wait(|v| v.phase == Phase::Faulted)).await;
        f.handle.shutdown().await;
        let mut config = config();
        if !no_host {
            config.tools = host(
                Workspace::new(),
                vec![Arc::new(Stub::new(ExecutionBackend::TrustedFs))],
            );
            // Verified artifact reads must respect the configured limit.
            config.max_artifact_bytes = 1;
        }
        let handle = SessionHandle::open_with_env(
            f.root.clone(),
            f.session.clone(),
            Arc::new(ScriptedProvider::new([])),
            config,
            DeterministicEnv::new(2),
        )
        .await
        .unwrap();
        let view = bounded(async {
            loop {
                let view = handle.view().await.unwrap();
                if hint(&view).is_some() {
                    break view;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(view.phase, Phase::AwaitingResolution);
        assert_eq!(
            hint(&view),
            Some(RecoveryHint::Unavailable {
                reason: if no_host {
                    "tools_unavailable"
                } else {
                    "artifact_unavailable"
                }
                .into()
            })
        );
        resolve_unknown(&handle, view.awaiting[0].invocation_id.clone()).await;
        handle.shutdown().await;
    }
}
