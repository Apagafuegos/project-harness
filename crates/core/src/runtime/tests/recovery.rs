//! Startup recovery against real storage: a crash in every phase, unanswered
//! and uncertain calls, crashes during recovery itself, and what the model
//! sees after a user resolves an uncertain invocation. Each scenario also
//! runs through `scenario`, which repeats, replays and checks it.

use super::scenarios::{INFER, OPEN, begin, call, contexts, finish, persists, ready, results, say};
use super::*;
use crate::context::ContextItem;
use crate::runtime::tests::{scenario, scenario_from};

const CLOSE: &str = "finished(interrupted, process_restart)";
const TOOL_CALLS: &str = "assistant(completed/toolcalls) inferring>processing";

/// A record's entries, with their index in it, and its events.
type Record = (Vec<(usize, Entry)>, Vec<JournalEvent>);

/// One line per journal record after the header, rebuilt from the published
/// snapshot: unlike `shapes`, it omits commits a crash lost.
fn journal(d: &Driver) -> Vec<String> {
    let snapshot = d.runtime.snapshot();
    let mut commits: BTreeMap<u64, Record> = (1..=snapshot.sequence())
        .map(|sequence| (sequence, Default::default()))
        .collect();
    for (_, stored) in snapshot.entries() {
        let commit = commits.get_mut(&stored.sequence).unwrap();
        commit.0.push((stored.entry_index, stored.entry.clone()));
    }
    for (sequence, _, event) in snapshot.events() {
        commits.get_mut(sequence).unwrap().1.push(event.clone());
    }
    commits
        .into_values()
        .map(|(mut entries, events)| {
            entries.sort_by_key(|(index, _)| *index);
            let line = shape(&Commit {
                command_id: None,
                command_fingerprint: None,
                head: HeadChange {
                    from: None,
                    to: None,
                    reason: HeadReason::Advance,
                },
                entries: entries.into_iter().map(|(_, entry)| entry).collect(),
                events: events
                    .into_iter()
                    .filter(|e| !matches!(e, JournalEvent::TailRepaired(_)))
                    .collect(),
            });
            if line.is_empty() {
                "tail_repaired".into()
            } else {
                line
            }
        })
        .collect()
}

/// `Recovered` events of the current runtime: turn, answered calls, code.
fn recovered(d: &Driver) -> Vec<(TurnId, Vec<CallRef>, String)> {
    d.segment
        .events()
        .filter_map(|event| match &event.kind {
            EventKind::Recovered {
                turn_id,
                answered,
                code,
            } => Some((turn_id.clone(), answered.clone(), code.clone())),
            _ => None,
        })
        .collect()
}

fn idle(d: &Driver) -> bool {
    d.runtime.phase() == Phase::Durable(RuntimeState::Idle) && d.active_turn().is_none()
}

/// The calls pending at the published head.
fn pending(d: &Driver) -> Vec<CallRef> {
    let snapshot = d.runtime.snapshot();
    snapshot.0.pending(snapshot.head()).unwrap().to_vec()
}

/// Start a turn whose response proposes `calls` calls, and acknowledge it
/// (C3). The runtime then has C4, the answers, in flight.
fn proposed(d: &mut Driver, calls: u32) -> TurnId {
    let request = begin(d, "use tools");
    for index in 0..calls {
        call(d, index, "read");
        d.provider(ProviderEvent::ToolArgumentsDelta {
            block_index: index,
            fragment: format!(r#"{{"path":"f{index}"}}"#),
        });
        ready(d, index);
    }
    finish(d, ProviderOutcome::Completed, Some(FinishReason::ToolCalls));
    assert!(d.complete(), "C3");
    assert_eq!(d.runtime.snapshot().state(), RuntimeState::Processing);
    assert!(d.pending.is_some(), "C4 in flight");
    super::scenarios::turn_of(d, request)
}

/// An audit-only commit retaining the head.
fn retain(snapshot: &SessionSnapshot, events: Vec<JournalEvent>) -> Commit {
    Commit {
        command_id: None,
        command_fingerprint: None,
        head: HeadChange {
            from: snapshot.head().cloned(),
            to: snapshot.head().cloned(),
            reason: HeadReason::Retain,
        },
        entries: Vec::new(),
        events,
    }
}

fn state(turn: &TurnId, from: RuntimeState, to: RuntimeState) -> JournalEvent {
    JournalEvent::StateChanged(StateChanged {
        turn_id: turn.clone(),
        from,
        to,
    })
}

/// What a tool runtime writes before running the first pending call:
/// `processing>authorizing` and an allow decision, then, with `intent`, the
/// execution intent and `authorizing>executing`. A crash right after leaves
/// the call not started, or its outcome unknown.
pub(super) fn authorize(
    snapshot: &SessionSnapshot,
    env: &mut DeterministicEnv,
    root: &DataRoot,
    intent: bool,
) -> Commit {
    let turn = snapshot.active_turn().unwrap().clone();
    let call = snapshot.0.pending(snapshot.head()).unwrap()[0].clone();
    // A v4 mutation intent binds a captured image, and its plan must agree.
    let pre = root.put_artifact(b"before").unwrap();
    let post = crate::session::Digest::of(b"after");
    let plan_bytes = serde_json::to_vec(&serde_json::json!({
        "version": 2,
        "name": "write",
        "workspace": "/workspace",
        "target": {
            "path": "f0",
            "pre_image": pre.sha256,
            "content_sha256": post,
        },
    }))
    .unwrap();
    let plan = root.put_artifact(&plan_bytes).unwrap();
    let decision_id: DecisionId = env.entry_id().to_string().parse().unwrap();
    let mut events = vec![
        state(&turn, RuntimeState::Processing, RuntimeState::Authorizing),
        JournalEvent::AuthorizationDecided(AuthorizationDecided {
            turn_id: turn.clone(),
            decision_id: decision_id.clone(),
            call: call.clone(),
            approval_id: None,
            plan_digest: plan.sha256.clone(),
            plan: plan.clone(),
            policy_revision: snapshot.policy_revision().into(),
            decision: Decision::Allow,
            reason: "test policy".into(),
        }),
    ];
    if intent {
        events.push(JournalEvent::ExecutionIntent(ExecutionIntent {
            turn_id: turn.clone(),
            invocation_id: env.entry_id().to_string().parse().unwrap(),
            call,
            decision_id,
            plan_digest: plan.sha256,
            backend: ExecutionBackend::TrustedFs,
            timeout_ms: 1_000,
            // These recovery scenarios exercise uncertain side effects.
            effects: EffectSet::new([EffectClass::FilesystemMutation]),
            file_image: Some(crate::session::FileImage {
                path: "f0".into(),
                pre_image: Some(pre),
                pre_mode: Some(0o644),
                post_image: post,
                additional_targets: Vec::new(),
            }),
        }));
        events.push(state(
            &turn,
            RuntimeState::Authorizing,
            RuntimeState::Executing,
        ));
    }
    retain(snapshot, events)
}

/// Kill the process with C4 in flight and write a tool runtime's intent for
/// the first call before restarting.
fn crash_after_intent(d: &mut Driver) {
    d.inject(|snapshot, env, root| authorize(snapshot, env, root, true));
    d.crash(Crash::Lost);
    d.settle();
}

fn resolve(d: &mut Driver, resolution: UserResolution) -> RequestId {
    let invocation_id = d.runtime.view().awaiting[0].invocation_id.clone();
    let request = d.command(Command::ResolveUncertain {
        invocation_id,
        resolution,
    });
    d.settle();
    request
}

fn observed(status: ToolStatus, text: &str) -> UserResolution {
    UserResolution::Observed {
        status,
        text: text.into(),
        file_effect: None,
    }
}

/// A user statement about a captured write or edit: the rename outcome is the
/// user's, and only they may classify it.
fn observed_mutation(status: ToolStatus, text: &str) -> UserResolution {
    UserResolution::Observed {
        status,
        text: text.into(),
        file_effect: Some(crate::session::FileEffect::Applied),
    }
}

/// Every command but a resolution is refused while waiting for one.
fn refuses_all_but_resolution(d: &mut Driver, turn: &TurnId) {
    for command in [
        Command::StartTurn {
            command_id: d.new_command_id(),
            content: text("new"),
        },
        Command::Cancel {
            turn_id: turn.clone(),
        },
        Command::SelectHead { head: None },
        Command::SwitchModel {
            policy: Portability::Reject,
            model: ModelRef {
                provider_instance: "mock".into(),
                model: "other".into(),
            },
        },
    ] {
        let request = d.command(command);
        assert_eq!(d.rejected(request), Rejection::ResolutionRequired);
    }
    assert!(d.pending.is_none());
}

macro_rules! recovery {
    ($($name:ident),+ $(,)?) => {$(
        #[test]
        fn $name() {
            scenario(known(), bodies::$name);
        }
    )+};
}

recovery!(
    crash_while_preparing_closes_the_turn,
    crash_while_inferring_closes_the_turn_and_writes_no_response,
    crash_while_processing_answers_unanswered_calls,
    crash_while_authorizing_answers_calls_that_never_started,
    crash_while_continuing_closes_the_turn,
    crash_while_cancelling_closes_the_turn,
    uncertain_call_alone_waits_and_is_acknowledged_unknown,
    uncertain_call_alone_is_resolved_as_observed,
    uncertain_call_followed_by_a_not_started_call_keeps_the_turn_open,
    crash_during_recovery_lost_redoes_it_once,
    crash_during_recovery_durable_is_not_repeated,
    crash_during_recovery_torn_is_repaired_then_redone,
    crash_while_awaiting_resolution_records_uncertainty_once,
    crash_during_resolution_never_resolves_twice,
    resolved_result_is_shown_to_the_model_with_its_provenance,
);

/// The interrupted-execution fixture: a completed `read` proposal with an
/// execution intent and no result.
pub(super) fn fixture() -> Driver {
    fixture_prefix(usize::MAX)
}

/// The first `records` records of the v1 interrupted-execution fixture.
fn fixture_prefix(records: usize) -> Driver {
    let examples =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/rfcs/001/examples");
    let temp = tempfile::tempdir().unwrap();
    let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
    for entry in std::fs::read_dir(examples.join("artifacts")).unwrap() {
        root.put_artifact(&std::fs::read(entry.unwrap().path()).unwrap())
            .unwrap();
    }
    let journal: Vec<u8> = std::fs::read(examples.join("interrupted-execution.jsonl"))
        .unwrap()
        .split_inclusive(|b| *b == b'\n')
        .take(records)
        .flatten()
        .copied()
        .collect();
    let session: SessionId = "019a0000-0000-7000-8000-000000000002".parse().unwrap();
    {
        use std::io::Write;
        let mut file = root.journal(&session, true, true).unwrap();
        file.write_all(&journal).unwrap();
        file.sync_all().unwrap();
    }
    let mut config = known();
    config.context.catalog.insert(
        ModelRef {
            provider_instance: "mock-default".into(),
            model: "scripted-text".into(),
        },
        crate::context::ModelLimits {
            context_tokens: Some(128_000),
            max_output_tokens: None,
        },
    );
    Driver::open_existing(temp, root, session, config)
}

#[test]
fn v1_intent_without_effects_never_qualifies_for_d9() {
    scenario_from(fixture, |d| {
        d.settle();
        assert!(
            d.runtime.snapshot().unresolved_invocations()[0]
                .intent
                .effects
                .is_none()
        );
        assert_eq!(d.runtime.phase(), Phase::AwaitingResolution);
        assert_eq!(d.runtime.view().awaiting.len(), 1);
        resolve(d, UserResolution::AcknowledgedUnknown);
        assert!(idle(d));
    });
}

/// The version of every record in the driver's journal, in order.
fn versions(d: &Driver) -> Vec<u64> {
    let path = d
        .root
        .path()
        .join("sessions")
        .join(format!("{}.jsonl", d.session));
    std::fs::read(path)
        .unwrap()
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice::<serde_json::Value>(line).unwrap()["record"]["version"]
                .as_u64()
                .unwrap()
        })
        .collect()
}

#[test]
fn recovery_over_a_v1_turn_answers_its_calls_as_v4() {
    // The v1 prefix ends in `processing`: the proposal, and no intent.
    scenario_from(
        || fixture_prefix(4),
        |d| {
            d.settle();
            assert!(idle(d));
            assert_eq!(versions(d), [1, 1, 1, 1, 4]);
            let snapshot = d.runtime.snapshot();
            let ancestry = snapshot.ancestry(snapshot.head()).unwrap();
            let [.., proposal, answer] = &ancestry[..] else {
                panic!("a proposal and its answer")
            };
            let (EntryPayload::AssistantResponse(response), EntryPayload::ToolResult(result)) =
                (&proposal.entry.payload, &answer.entry.payload)
            else {
                panic!("a v1 response answered by a v3 result")
            };
            assert_eq!(response.projection_fingerprint, None, "v1 has none");
            let result = &result.result;
            assert_eq!(result.call.response_entry_id, proposal.entry.id);
            assert!(response.content.iter().any(|block| matches!(block,
                ContentBlock::ToolCall { call_id, .. } if *call_id == result.call.call_id)));
            assert_eq!(result.status, ToolStatus::Cancelled);
            assert_eq!(result.metadata.code.as_deref(), Some("process_restart"));
        },
    );
}

#[test]
fn recovery_over_a_v1_intent_records_v4_and_unknown_effects() {
    scenario_from(fixture, |d| {
        d.settle();
        assert_eq!(versions(d), [1, 1, 1, 1, 1, 4], "execution_uncertain is v4");
        let [invocation] = &d.runtime.snapshot().unresolved_invocations()[..] else {
            panic!("one invocation waits")
        };
        assert_eq!(invocation.intent.effects, None, "recorded before v2");
        let request = resolve(d, observed(ToolStatus::Succeeded, "read"));
        d.accepted(request);
        assert!(idle(d));
        assert_eq!(versions(d), [1, 1, 1, 1, 1, 4, 4]);
        let [result] = &results(d)[..] else {
            panic!("one result")
        };
        assert_eq!(result.metadata.provenance, Provenance::UserObserved);
        assert_eq!(result.metadata.code, None);
    });
}

#[test]
fn interrupted_execution_fixture_waits_then_observed_returns_to_idle() {
    scenario_from(fixture, |d| {
        assert_eq!(
            persists(&d.segment.effects),
            1,
            "Start recorded the uncertainty"
        );
        d.settle();
        let tail = journal(d).split_off(4);
        assert_eq!(tail, [format!("execution_uncertain {CLOSE}")]);
        assert_eq!(d.runtime.phase(), Phase::AwaitingResolution);
        let view = d.runtime.view();
        let [waiting] = &view.awaiting[..] else {
            panic!("one invocation waits: {:?}", view.awaiting)
        };
        assert_eq!(waiting.tool, "read");
        assert_eq!(
            waiting.invocation_id.as_str(),
            "019a0000-0000-7000-8000-00000000019d"
        );
        assert!(d.segment.events().any(|e| matches!(
            &e.kind,
            EventKind::ResolutionRequired { invocations } if invocations == &view.awaiting
        )));
        let turn = waiting.turn_id.clone();
        assert_eq!(
            recovered(d),
            [(turn.clone(), vec![], "process_restart".into())]
        );
        refuses_all_but_resolution(d, &turn);
        let request = resolve(d, observed(ToolStatus::Succeeded, "README.md was read"));
        assert!(matches!(d.accepted(request), Accepted::Resolved { .. }));
        assert!(idle(d));
        assert!(d.runtime.view().awaiting.is_empty());
        let result = results(d).pop().unwrap();
        assert_eq!(result.metadata.provenance, Provenance::UserObserved);
        assert_eq!(result.status, ToolStatus::Succeeded);
        assert_eq!(
            journal(d).last().unwrap(),
            "result(succeeded) recovery_resolved(observed)"
        );
    });
}

#[test]
fn interrupted_execution_fixture_acknowledged_unknown_returns_to_idle() {
    scenario_from(fixture, |d| {
        d.settle();
        assert_eq!(d.runtime.phase(), Phase::AwaitingResolution);
        let request = resolve(d, UserResolution::AcknowledgedUnknown);
        assert!(matches!(d.accepted(request), Accepted::Resolved { .. }));
        assert!(idle(d));
        let result = results(d).pop().unwrap();
        assert_eq!(result.status, ToolStatus::Uncertain);
        assert_eq!(
            result.metadata.provenance,
            Provenance::UserAcknowledgedUnknown
        );
        // The next turn's context shows the model it was the user's statement.
        begin(d, "what happened?");
        let (_, started) = contexts(d);
        let items = &started.last().unwrap().2;
        assert!(items.iter().any(|item| matches!(
            item,
            ContextItem::ToolResult { provenance: Provenance::UserAcknowledgedUnknown, result, .. }
                if result.status == ToolStatus::Uncertain
        )));
    });
}

pub mod bodies {
    use super::*;

    pub fn crash_while_preparing_closes_the_turn(d: &mut Driver) {
        let (request, _) = d.start("crash");
        assert!(d.complete(), "C1");
        let turn = super::super::scenarios::turn_of(d, request);
        assert_eq!(d.runtime.snapshot().state(), RuntimeState::Preparing);
        d.crash(Crash::Lost);
        assert_eq!(d.runtime.phase(), Phase::RecoveryRequired);
        d.settle();
        assert!(idle(d));
        assert_eq!(
            journal(d),
            [OPEN.to_owned(), format!("preparing>recovering {CLOSE}")]
        );
        assert_eq!(recovered(d), [(turn, vec![], "process_restart".into())]);
    }

    pub fn crash_while_inferring_closes_the_turn_and_writes_no_response(d: &mut Driver) {
        begin(d, "crash");
        say(d, 0, "a partial response that was never persisted");
        d.crash(Crash::Lost);
        d.settle();
        assert!(idle(d));
        assert_eq!(
            journal(d),
            [OPEN, INFER, &format!("inferring>recovering {CLOSE}")]
        );
        assert!(super::super::scenarios::responses(d).is_empty());
    }

    pub fn crash_while_processing_answers_unanswered_calls(d: &mut Driver) {
        let turn = proposed(d, 2);
        let calls = pending(d);
        d.crash(Crash::Lost);
        d.settle();
        assert!(idle(d));
        assert_eq!(
            journal(d),
            [
                OPEN,
                INFER,
                TOOL_CALLS,
                &format!("result(cancelled) result(cancelled) processing>recovering {CLOSE}")
            ]
        );
        for result in results(d) {
            assert_eq!(result.metadata.provenance, Provenance::Runtime);
            assert!(result.invocation_id.is_none());
            let ContentBlock::Text { text } = &result.content[0] else {
                panic!("text result")
            };
            assert!(text.starts_with("process_restart"), "{text}");
            assert_eq!(result.metadata.code.as_deref(), Some("process_restart"));
        }
        assert_eq!(recovered(d), [(turn, calls, "process_restart".into())]);
    }

    pub fn crash_while_authorizing_answers_calls_that_never_started(d: &mut Driver) {
        proposed(d, 2);
        d.inject(|snapshot, env, root| authorize(snapshot, env, root, false));
        d.crash(Crash::Lost);
        d.settle();
        assert!(idle(d));
        assert_eq!(
            journal(d)[3..],
            [
                "processing>authorizing authorization_decided".to_owned(),
                format!("result(cancelled) result(cancelled) authorizing>recovering {CLOSE}")
            ]
        );
    }

    pub fn crash_while_continuing_closes_the_turn(d: &mut Driver) {
        let turn = proposed(d, 1);
        d.inject(|snapshot, env, _| {
            let call = snapshot.0.pending(snapshot.head()).unwrap()[0].clone();
            let id = env.entry_id();
            Commit {
                command_id: None,
                command_fingerprint: None,
                head: HeadChange {
                    from: snapshot.head().cloned(),
                    to: Some(id.clone()),
                    reason: HeadReason::Advance,
                },
                entries: vec![Entry {
                    id,
                    parent_id: snapshot.head().cloned(),
                    turn_id: Some(turn.clone()),
                    payload: EntryPayload::ToolResult(ToolResultEntry {
                        result: ToolResult {
                            call,
                            invocation_id: None,
                            status: ToolStatus::Denied,
                            content: text("denied by policy"),
                            metadata: ToolMetadata {
                                exit_code: None,
                                duration_ms: None,
                                output_truncated: false,
                                provenance: Provenance::Runtime,
                                artifacts: Vec::new(),
                                code: Some("policy_denied".into()),
                                file_effect: None,
                                file_paths: Vec::new(),
                            },
                        },
                    }),
                }],
                events: vec![state(
                    &turn,
                    RuntimeState::Processing,
                    RuntimeState::Continuing,
                )],
            }
        });
        d.crash(Crash::Lost);
        d.settle();
        assert!(idle(d));
        assert_eq!(
            journal(d)[3..],
            [
                "result(denied) processing>continuing".to_owned(),
                format!("continuing>recovering {CLOSE}")
            ]
        );
    }

    pub fn crash_while_cancelling_closes_the_turn(d: &mut Driver) {
        begin(d, "cancel me");
        say(d, 0, "partial");
        let cancel = d.cancel();
        assert!(d.complete(), "inferring>cancelling");
        assert!(matches!(d.accepted(cancel), Accepted::Cancelling { .. }));
        assert_eq!(d.runtime.snapshot().state(), RuntimeState::Cancelling);
        d.crash(Crash::Lost);
        d.settle();
        assert!(idle(d));
        assert_eq!(
            journal(d)[2..],
            [
                "inferring>cancelling".to_owned(),
                format!("cancelling>recovering {CLOSE}")
            ]
        );
    }

    pub fn uncertain_call_alone_waits_and_is_acknowledged_unknown(d: &mut Driver) {
        let turn = proposed(d, 1);
        crash_after_intent(d);
        assert_eq!(
            journal(d)[3..],
            [
                "processing>authorizing authorization_decided execution_intent authorizing>executing"
                    .to_owned(),
                format!("execution_uncertain {CLOSE}")
            ]
        );
        assert_eq!(d.runtime.phase(), Phase::AwaitingResolution);
        assert!(d.active_turn().is_none(), "the turn is closed");
        assert_eq!(d.runtime.snapshot().state(), RuntimeState::Recovering);
        assert_eq!(d.runtime.view().awaiting[0].tool, "read");
        refuses_all_but_resolution(d, &turn);
        let unknown = d.command(Command::ResolveUncertain {
            invocation_id: DeterministicEnv::new(0xffb)
                .entry_id()
                .to_string()
                .parse()
                .unwrap(),
            resolution: UserResolution::AcknowledgedUnknown,
        });
        assert_eq!(
            d.rejected(unknown),
            Rejection::Invalid("unknown_invocation")
        );
        let invocation_id = d.runtime.view().awaiting[0].invocation_id.clone();
        for (resolution, code) in [
            (
                observed(ToolStatus::Uncertain, "?"),
                "observed_status_uncertain",
            ),
            (observed(ToolStatus::Succeeded, ""), "empty_content"),
            (
                observed(ToolStatus::Succeeded, &"x".repeat(1024 * 1024 + 1)),
                "content_too_large",
            ),
        ] {
            let request = d.command(Command::ResolveUncertain {
                invocation_id: invocation_id.clone(),
                resolution,
            });
            assert_eq!(d.rejected(request), Rejection::Invalid(code));
        }
        let request = resolve(d, UserResolution::AcknowledgedUnknown);
        assert!(
            matches!(d.accepted(request), Accepted::Resolved { invocation_id: id, .. } if *id == invocation_id)
        );
        assert!(idle(d));
        assert_eq!(
            journal(d).last().unwrap(),
            "result(uncertain) recovery_resolved(acknowledgedunknown)"
        );
        let again = d.command(Command::ResolveUncertain {
            invocation_id,
            resolution: UserResolution::AcknowledgedUnknown,
        });
        assert_eq!(d.rejected(again), Rejection::Invalid("nothing_to_resolve"));
    }

    pub fn uncertain_call_alone_is_resolved_as_observed(d: &mut Driver) {
        proposed(d, 1);
        crash_after_intent(d);
        let request = resolve(
            d,
            observed_mutation(ToolStatus::Failed, "f0 does not exist"),
        );
        d.accepted(request);
        assert!(idle(d));
        let result = results(d).pop().unwrap();
        assert_eq!(result.status, ToolStatus::Failed);
        assert_eq!(result.metadata.provenance, Provenance::UserObserved);
        assert!(result.invocation_id.is_some());
        assert_eq!(
            journal(d).last().unwrap(),
            "result(failed) recovery_resolved(observed)"
        );
    }

    pub fn uncertain_call_followed_by_a_not_started_call_keeps_the_turn_open(d: &mut Driver) {
        let turn = proposed(d, 2);
        let second = pending(d)[1].clone();
        crash_after_intent(d);
        assert_eq!(journal(d).last().unwrap(), "execution_uncertain");
        assert_eq!(d.runtime.phase(), Phase::AwaitingResolution);
        assert_eq!(d.active_turn(), Some(turn.clone()), "the turn stays open");
        assert!(recovered(d).is_empty());
        refuses_all_but_resolution(d, &turn);
        let request = resolve(d, observed_mutation(ToolStatus::Succeeded, "f0 was read"));
        d.accepted(request);
        assert!(idle(d));
        assert_eq!(
            journal(d).last().unwrap(),
            &format!("result(succeeded) result(cancelled) recovery_resolved(observed) {CLOSE}")
        );
        // The user's statement has no code; the call it left unstarted does.
        let codes: Vec<_> = results(d)
            .into_iter()
            .map(|r| (r.metadata.provenance, r.metadata.code))
            .collect();
        assert_eq!(
            codes,
            [
                (Provenance::UserObserved, None),
                (Provenance::Runtime, Some("process_restart".into())),
            ]
        );
        assert_eq!(
            recovered(d),
            [(turn, vec![second], "process_restart".into())]
        );
    }

    /// Crash in `Processing`, restart, then crash again with the recovery
    /// commit in flight.
    fn crash_during_recovery(d: &mut Driver, crash: Crash) {
        proposed(d, 2);
        d.crash(Crash::Lost);
        assert!(d.pending.is_some(), "recovery commit in flight");
        d.crash(crash);
        d.settle();
        assert!(idle(d));
    }

    pub fn crash_during_recovery_lost_redoes_it_once(d: &mut Driver) {
        crash_during_recovery(d, Crash::Lost);
        assert_eq!(
            journal(d)[3..],
            [format!(
                "result(cancelled) result(cancelled) processing>recovering {CLOSE}"
            )]
        );
    }

    pub fn crash_during_recovery_durable_is_not_repeated(d: &mut Driver) {
        crash_during_recovery(d, Crash::Durable);
        assert_eq!(persists(&d.segment.effects), 0, "nothing left to recover");
        assert_eq!(
            journal(d)[3..],
            [format!(
                "result(cancelled) result(cancelled) processing>recovering {CLOSE}"
            )]
        );
    }

    pub fn crash_during_recovery_torn_is_repaired_then_redone(d: &mut Driver) {
        crash_during_recovery(d, Crash::Torn);
        assert_eq!(d.repairs, 1);
        assert_eq!(
            journal(d)[3..],
            [
                "tail_repaired".to_owned(),
                format!("result(cancelled) result(cancelled) processing>recovering {CLOSE}")
            ]
        );
    }

    pub fn crash_while_awaiting_resolution_records_uncertainty_once(d: &mut Driver) {
        proposed(d, 2);
        crash_after_intent(d);
        let before = journal(d);
        d.crash(Crash::Lost);
        assert_eq!(
            persists(&d.segment.effects),
            0,
            "uncertainty is already recorded"
        );
        assert_eq!(d.runtime.phase(), Phase::AwaitingResolution);
        assert_eq!(journal(d), before);
        resolve(d, UserResolution::AcknowledgedUnknown);
        assert!(idle(d));
    }

    pub fn crash_during_resolution_never_resolves_twice(d: &mut Driver) {
        proposed(d, 1);
        crash_after_intent(d);
        let invocation_id = d.runtime.view().awaiting[0].invocation_id.clone();
        d.command(Command::ResolveUncertain {
            invocation_id: invocation_id.clone(),
            resolution: observed_mutation(ToolStatus::Succeeded, "done"),
        });
        d.crash(Crash::Durable);
        assert!(idle(d), "the durable resolution needs nothing more");
        let retry = d.command(Command::ResolveUncertain {
            invocation_id,
            resolution: observed(ToolStatus::Succeeded, "done"),
        });
        assert_eq!(d.rejected(retry), Rejection::Invalid("nothing_to_resolve"));
        let resolved = d
            .runtime
            .snapshot()
            .events()
            .filter(|(_, _, e)| matches!(e, JournalEvent::RecoveryResolved(_)))
            .count();
        assert_eq!(resolved, 1);
    }

    pub fn resolved_result_is_shown_to_the_model_with_its_provenance(d: &mut Driver) {
        proposed(d, 1);
        crash_after_intent(d);
        resolve(
            d,
            observed_mutation(ToolStatus::Succeeded, "the user saw f0"),
        );
        let entry = d.runtime.snapshot().head().cloned().unwrap();
        let report = d.runtime.inspect_context(None, None).unwrap();
        let record = report
            .included
            .iter()
            .find(|item| item.entry_id.as_ref() == Some(&entry))
            .unwrap();
        assert_eq!(record.provenance, Some(Provenance::UserObserved));
        let json = serde_json::to_value(record).unwrap();
        assert_eq!(json["provenance"], "user_observed");
        begin(d, "next");
        let (_, started) = contexts(d);
        let items = &started.last().unwrap().2;
        assert!(items.iter().any(|item| matches!(
            item,
            ContextItem::ToolResult { entry_id, provenance: Provenance::UserObserved, .. }
                if *entry_id == entry
        )));
        say(d, 0, "noted");
        super::super::scenarios::stop(d);
        d.settle();
    }
}
