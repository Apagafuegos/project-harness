//! D9/D30 histories accepted by the real journal, and nonblocking advice.
use super::pipeline::{enabled, propose, results};
use super::*;
use serde_json::json;

fn read_intent(d: &mut Driver, calls: &[(&str, serde_json::Value)]) {
    propose(d, calls);
    d.settle();
    assert!(d.plan());
    d.settle();
    assert!(d.executing.is_some());
}
fn write_intent(d: &mut Driver, leading_read: bool) {
    let mut calls = Vec::new();
    if leading_read {
        calls.push(("read", json!({"path":"a"})));
    }
    calls.push(("write", json!({"path":"new","text":"post"})));
    calls.push(("read", json!({"path":"a"})));
    propose(d, &calls);
    d.settle();
    if leading_read {
        assert!(d.plan());
        d.settle();
        assert!(d.execute());
        d.settle();
    }
    assert!(d.plan());
    d.settle();
    d.approval(Decision::Allow, false);
    d.settle();
    assert!(d.executing.is_some());
}
fn hints_effect(d: &Driver) -> (HintToken, Vec<(InvocationId, ArtifactRef)>) {
    d.segment
        .effects
        .iter()
        .find_map(|e| match e {
            Effect::InspectUncertain { token, invocations } => Some((*token, invocations.clone())),
            _ => None,
        })
        .expect("inspection")
}
fn resolve(d: &mut Driver) {
    let id = d.runtime.view().awaiting[0].invocation_id.clone();
    let request = d.command(Command::ResolveUncertain {
        invocation_id: id,
        resolution: UserResolution::AcknowledgedUnknown,
    });
    d.settle();
    assert!(matches!(d.replies[&request], Ok(Accepted::Resolved { .. })));
    assert_eq!(d.runtime.phase(), Phase::Durable(RuntimeState::Idle));
}
#[test]
fn d9_read_intent_then_not_started_calls_closes_in_branch_order() {
    for fate in [Crash::Lost, Crash::Durable, Crash::Torn] {
        scenario_from(enabled, |d| {
            read_intent(
                d,
                &[
                    ("read", json!({"path":"a"})),
                    ("write", json!({"path":"new","text":"post"})),
                    ("read", json!({"path":"a"})),
                ],
            );
            let invocation = d.runtime.snapshot().unresolved_invocations()[0]
                .intent
                .invocation_id
                .clone();
            d.kill(fate);
            d.restart();
            d.settle();
            assert_eq!(d.count(), 0);
            assert_eq!(d.runtime.phase(), Phase::Durable(RuntimeState::Idle));
            assert!(d.runtime.view().awaiting.is_empty());
            let results = results(d);
            assert_eq!(results.len(), 3);
            for r in &results {
                assert_eq!(r.status, ToolStatus::Cancelled);
                assert_eq!(r.metadata.provenance, Provenance::Runtime);
                assert_eq!(r.metadata.code.as_deref(), Some("process_restart"));
            }
            assert_eq!(results[0].invocation_id, Some(invocation));
            assert!(results[1..].iter().all(|r| r.invocation_id.is_none()));
            assert!(d.runtime.snapshot().events().all(|(_, _, e)| !matches!(
                e,
                JournalEvent::ExecutionUncertain(_) | JournalEvent::RecoveryResolved(_)
            )));
            assert!(
                d.runtime
                    .snapshot()
                    .turns()
                    .all(|(_, t)| t.outcome == Some(TurnOutcome::Interrupted))
            );
        });
    }
}
#[test]
fn d30_write_then_read_and_read_write_read_wait_for_the_user() {
    for leading_read in [false, true] {
        scenario_from(enabled, |d| {
            write_intent(d, leading_read);
            let count = d.count();
            d.kill(Crash::Lost);
            d.restart();
            d.settle();
            assert_eq!(d.runtime.phase(), Phase::AwaitingResolution);
            assert_eq!(d.runtime.view().awaiting[0].tool, "write");
            assert_eq!(results(d).len(), usize::from(leading_read));
            resolve(d);
            assert_eq!(d.count(), count);
            let results = results(d);
            let last = results.last().unwrap();
            assert_eq!(last.status, ToolStatus::Cancelled);
            assert_eq!(last.metadata.code.as_deref(), Some("process_restart"));
            assert!(last.invocation_id.is_none());
            assert_eq!(last.metadata.provenance, Provenance::Runtime);
            if leading_read {
                assert_eq!(results[0].status, ToolStatus::Succeeded);
            }
        });
    }
}
#[test]
fn previously_uncertain_read_still_requires_user_resolution() {
    scenario_from(enabled, |d| {
        read_intent(d, &[("read", json!({"path":"a"}))]);
        d.inject(|snapshot, _, _| {
            let intent = &snapshot.unresolved_invocations()[0].intent;
            let mut commit = Commit {
                command_id: None,
                command_fingerprint: None,
                head: HeadChange {
                    from: snapshot.head().cloned(),
                    to: snapshot.head().cloned(),
                    reason: HeadReason::Retain,
                },
                entries: Vec::new(),
                events: Vec::new(),
            };
            commit
                .events
                .push(JournalEvent::ExecutionUncertain(ExecutionUncertain {
                    turn_id: intent.turn_id.clone(),
                    invocation_id: intent.invocation_id.clone(),
                    reason: "process_restart".into(),
                }));
            commit
        });
        d.kill(Crash::Lost);
        d.restart();
        d.settle();
        assert_eq!(d.runtime.phase(), Phase::AwaitingResolution);
        assert!(results(d).is_empty());
        assert!(
            d.segment
                .effects
                .iter()
                .all(|e| !matches!(e, Effect::InspectUncertain { .. }))
        );
        resolve(d);
    });
}
#[test]
fn resolve_without_hints_and_drop_late_and_stale_answers() {
    scenario_from(enabled, |d| {
        write_intent(d, false);
        d.kill(Crash::Lost);
        d.restart();
        d.settle();
        let (token, invocations) = hints_effect(d);
        let id = invocations[0].0.clone();
        assert!(d.runtime.view().awaiting[0].hint.is_none());
        let before = d.runtime.view().event_seq;
        d.step(Input::Hints {
            token: HintToken(token.0 + 1),
            hints: vec![(id.clone(), RecoveryHint::Missing)],
        });
        assert_eq!(d.runtime.view().event_seq, before);
        resolve(d);
        let before = d.runtime.view().event_seq;
        d.step(Input::Hints {
            token,
            hints: vec![(id, RecoveryHint::MatchesPostImage)],
        });
        assert_eq!(d.runtime.view().event_seq, before);
    });
}
#[test]
fn hints_bind_authorized_artifact_and_survive_view_resync_without_journaling() {
    scenario_from(enabled, |d| {
        write_intent(d, false);
        d.kill(Crash::Lost);
        d.restart();
        d.settle();
        let (token, invocations) = hints_effect(d);
        let (id, artifact) = invocations[0].clone();
        let intent = &d.runtime.snapshot().unresolved_invocations()[0].intent;
        assert_eq!(artifact.sha256, intent.plan_digest);
        assert_eq!(id, intent.invocation_id);
        let bytes = d.root.read_artifact(&artifact, 16 * 1024 * 1024).unwrap();
        let hint = crate::tools::recovery_hint(&bytes, &d.workspace);
        assert_eq!(hint, RecoveryHint::MatchesPreImage);
        let sequence = d.runtime.snapshot().sequence();
        d.step(Input::Hints {
            token,
            hints: vec![
                (InvocationId::new(), RecoveryHint::Missing),
                (id, hint.clone()),
            ],
        });
        assert_eq!(d.runtime.view().awaiting[0].hint, Some(hint));
        assert_eq!(d.runtime.snapshot().sequence(), sequence);
        assert_eq!(
            ClientView::from(d.runtime.view()).view().awaiting,
            d.runtime.view().awaiting
        );
        let before = d.runtime.view().event_seq;
        d.step(Input::Hints {
            token,
            hints: Vec::new(),
        });
        assert_eq!(d.runtime.view().event_seq, before);
        resolve(d);
    });
}

#[test]
fn d9_search_intent_is_closed_read_only_without_reexecution() {
    fn search_enabled() -> Driver {
        let mut config = super::pipeline::configuration();
        config["enabled_tools"] = json!(["read", "write", "edit", "bash", "list", "glob", "grep"]);
        super::pipeline::configured(config)
    }
    for fate in [Crash::Lost, Crash::Durable, Crash::Torn] {
        scenario_from(search_enabled, |d| {
            read_intent(d, &[("grep", json!({"pattern":"hello"}))]);
            let invocation = d.runtime.snapshot().unresolved_invocations()[0]
                .intent
                .invocation_id
                .clone();
            d.kill(fate);
            d.restart();
            d.settle();
            assert_eq!(d.count(), 0);
            assert_eq!(d.runtime.phase(), Phase::Durable(RuntimeState::Idle));
            assert!(d.runtime.view().awaiting.is_empty());
            let results = results(d);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].invocation_id, Some(invocation));
            assert_eq!(results[0].status, ToolStatus::Cancelled);
            assert_eq!(results[0].metadata.code.as_deref(), Some("process_restart"));
            assert_eq!(results[0].metadata.provenance, Provenance::Runtime);
            assert!(d.runtime.snapshot().events().all(|(_, _, e)| !matches!(
                e,
                JournalEvent::ExecutionUncertain(_) | JournalEvent::RecoveryResolved(_)
            )));
        });
    }
}
