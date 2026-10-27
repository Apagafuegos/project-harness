//! Additional durability/race gates and the exact D16 storage exception.
use super::pipeline::{configuration, configured, enabled, outcome, propose, results, stop};
use super::*;
use serde_json::json;
use std::time::Duration;

#[test]
fn approval_wall_clock_expiry_cannot_be_bypassed_by_delaying_the_timer() {
    scenario_from(enabled, |d| {
        propose(d, &[("write", json!({"path":"new","text":"x"}))]);
        d.run_tools();
        d.env.advance(Duration::from_secs(300));
        let request = d.approval(Decision::Allow, true);
        d.run_tools();
        d.accepted(request);
        assert_eq!(d.count(), 0);
        assert_eq!(
            results(d)[0].metadata.code.as_deref(),
            Some("approval_expired")
        );
        assert_eq!(d.runtime.snapshot().policy_revision(), "initial");
        stop(d);
    });
}

#[test]
fn denial_decision_is_durable_before_result_and_approval_reply() {
    scenario_from(enabled, |d| {
        propose(d, &[("write", json!({"path":"new","text":"x"}))]);
        d.run_tools();
        let request = d.approval(Decision::Deny, false);
        let pending = d.pending.as_ref().unwrap();
        assert!(pending.commit.entries.is_empty());
        assert!(
            matches!(&pending.commit.events[..], [JournalEvent::AuthorizationDecided(a)] if a.decision == Decision::Deny && a.reason == "approval_denied")
        );
        assert!(d.reply(request).is_none());
        assert!(results(d).is_empty());
        assert!(d.complete());
        d.accepted(request);
        assert!(results(d).is_empty());
        assert!(!d.pending.as_ref().unwrap().commit.entries.is_empty());
        d.settle();
        assert_eq!(
            results(d)[0].metadata.code.as_deref(),
            Some("approval_denied")
        );
        stop(d);
    });
}

#[test]
fn cancelling_authorizing_or_unacknowledged_intent_never_runs_a_second_call() {
    for gate in 0..3 {
        scenario_from(enabled, |d| {
            propose(
                d,
                &[
                    ("read", json!({"path":"a"})),
                    ("read", json!({"path":"f1"})),
                ],
            );
            d.complete(); // Authorizing transition in flight
            if gate >= 1 {
                d.complete();
            } // PlanCall outstanding
            if gate == 2 {
                d.plan();
            } // intent in flight
            let request = d.cancel();
            d.settle();
            if gate == 2 {
                assert!(d.executing.as_ref().unwrap().2.is_cancelled());
                d.execute();
                d.settle();
            } else if d.planning.is_some() {
                let effects_before = d.segment.effects.len();
                d.plan(); // late planning reply after cancellation, ignored
                assert_eq!(d.segment.effects.len(), effects_before);
            }
            d.accepted(request);
            assert_eq!(d.count(), usize::from(gate == 2));
            assert_eq!(results(d).len(), 2);
            assert!(results(d).iter().all(|r| r.status == ToolStatus::Cancelled));
            assert_eq!(d.runtime.phase(), Phase::Durable(RuntimeState::Idle));
        });
    }
}

#[test]
fn executor_completion_during_cancel_commit_is_preserved_and_stale_answers_ignored() {
    scenario_from(enabled, |d| {
        propose(d, &[("read", json!({"path":"a"}))]);
        d.settle();
        d.plan();
        d.settle();
        let token = d.executing.as_ref().unwrap().0;
        let stale = d.step(Input::ToolFinished {
            token: ExecutionToken(token.0 + 1),
            outcome: outcome(ToolStatus::Succeeded, None),
        });
        assert!(stale.is_empty());
        d.executor
            .success_wins
            .store(true, std::sync::atomic::Ordering::Release);
        let request = d.cancel(); // cancellation state commit not yet acknowledged
        d.execute(); // executor already succeeded; buffer its answer until ack
        d.settle();
        d.accepted(request);
        assert_eq!(results(d)[0].status, ToolStatus::Succeeded);
        assert!(
            d.step(Input::ToolFinished {
                token,
                outcome: outcome(ToolStatus::Failed, Some(ResultCode::StalePlan))
            })
            .is_empty()
        );
        assert_eq!(results(d).len(), 1);
    });
}

#[test]
fn enabled_tool_without_its_backend_refuses_before_validation_or_planning() {
    scenario_from(
        || {
            Driver::with_tools(
                known(),
                &serde_json::to_vec(&configuration()).unwrap(),
                ToolSet::new([ExecutionBackend::TrustedFs]),
            )
        },
        |d| {
            propose(d, &[("bash", json!({"invalid":true}))]);
            d.run_tools();
            assert_eq!(d.count(), 0);
            assert_eq!(results(d)[0].status, ToolStatus::Denied);
            assert_eq!(
                results(d)[0].metadata.code.as_deref(),
                Some("tools_unavailable")
            );
            assert!(
                !d.segment
                    .effects
                    .iter()
                    .any(|e| matches!(e, Effect::PlanCall { .. }))
            );
            stop(d);
        },
    );
}

#[test]
fn artifact_publication_failure_never_dispatches_an_uncommitted_intent() {
    for point in [
        Point::ArtifactWritten,
        Point::ArtifactSynced,
        Point::ArtifactRenamed,
        Point::ArtifactDirectorySynced,
    ] {
        scenario_from(enabled, |d| {
            propose(d, &[("read", json!({"path":"a"}))]);
            d.settle();
            d.plan();
            d.fail = Some(point);
            d.complete();
            assert_eq!(d.runtime.phase(), Phase::Faulted);
            assert!(d.executing.is_none());
            assert_eq!(d.count(), 0);
        });
    }
}

#[test]
fn remember_storage_exception_requires_same_commit_prior_allow_ticket_and_new_revision() {
    scenario_from(enabled, |d| {
        propose(d, &[("write", json!({"path":"new","text":"x"}))]);
        d.run_tools();
        d.approval(Decision::Allow, true);
        let pending = d.pending.as_ref().unwrap();
        for bytes in &pending.artifacts {
            d.root.put_artifact(bytes).unwrap();
        }
        let allow = pending
            .commit
            .events
            .iter()
            .find_map(|e| {
                if let JournalEvent::AuthorizationDecided(a) = e {
                    Some(a.clone())
                } else {
                    None
                }
            })
            .unwrap();
        let grant = pending
            .commit
            .events
            .iter()
            .find_map(|e| {
                if let JournalEvent::ConfigurationChanged(c) = e {
                    Some(c.clone())
                } else {
                    None
                }
            })
            .unwrap();
        let snapshot = d.runtime.snapshot().clone();
        let candidate = |events| Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: snapshot.head().cloned(),
                to: snapshot.head().cloned(),
                reason: HeadReason::Retain,
            },
            entries: Vec::new(),
            events,
        };
        let mut no_ticket = allow.clone();
        no_ticket.approval_id = None;
        let mut deny = allow.clone();
        deny.decision = Decision::Deny;
        let mut same_revision = grant.clone();
        same_revision.policy_revision = snapshot.policy_revision().into();
        for events in [
            vec![JournalEvent::ConfigurationChanged(grant.clone())],
            vec![
                JournalEvent::ConfigurationChanged(grant.clone()),
                JournalEvent::AuthorizationDecided(allow.clone()),
            ],
            vec![
                JournalEvent::AuthorizationDecided(no_ticket),
                JournalEvent::ConfigurationChanged(grant.clone()),
            ],
            vec![
                JournalEvent::AuthorizationDecided(deny),
                JournalEvent::ConfigurationChanged(grant.clone()),
            ],
            vec![
                JournalEvent::AuthorizationDecided(allow.clone()),
                JournalEvent::ConfigurationChanged(same_revision),
            ],
        ] {
            assert!(matches!(
                d.store
                    .as_mut()
                    .unwrap()
                    .append(candidate(events), d.env.now()),
                Err(StoreError::Invalid("configuration_inside_turn"))
            ));
            assert!(same(&snapshot, &d.store.as_ref().unwrap().snapshot()));
        }
        d.run_tools();
        assert_eq!(d.count(), 1);
        assert_ne!(d.runtime.snapshot().policy_revision(), "initial");
        stop(d);
    });
}

#[test]
fn remember_storage_exception_rejects_allow_from_an_earlier_commit() {
    let mut d = configured(configuration());
    propose(&mut d, &[("write", json!({"path":"new","text":"x"}))]);
    d.run_tools();
    d.approval(Decision::Allow, true);
    let pending = d.pending.take().unwrap();
    for bytes in &pending.artifacts {
        d.root.put_artifact(bytes).unwrap();
    }
    let allow = pending
        .commit
        .events
        .iter()
        .find(|e| matches!(e, JournalEvent::AuthorizationDecided(_)))
        .unwrap()
        .clone();
    let grant = pending
        .commit
        .events
        .iter()
        .find(|e| matches!(e, JournalEvent::ConfigurationChanged(_)))
        .unwrap()
        .clone();
    let mut audit = pending.commit.clone();
    audit.events = vec![allow];
    d.store
        .as_mut()
        .unwrap()
        .append(audit.clone(), d.env.now())
        .unwrap();
    audit.events = vec![grant];
    assert!(matches!(
        d.store.as_mut().unwrap().append(audit, d.env.now()),
        Err(StoreError::Invalid("configuration_inside_turn"))
    ));
    // This was an injected durable decision, not an acknowledgement to the
    // dying core. Recovery must close it as known-not-started, not uncertain.
    d.kill(Crash::Lost);
    d.restart();
    d.settle();
    d.acknowledge_all();
    assert_eq!(d.count(), 0);
    assert_eq!(
        results(&d)[0].metadata.code.as_deref(),
        Some("process_restart")
    );
    super::pipeline::checked(d);
}
