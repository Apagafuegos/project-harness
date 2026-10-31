//! Seeded random interleavings of commands, provider events, stale streams,
//! timers, persist failures and process crashes, all against real storage.
//! After a crash the process restarts: it repairs a torn tail with the
//! inspected fingerprint, recovers, and resolves uncertain invocations with
//! a PRNG-chosen statement before continuing.

use super::pipeline::{configuration, outcome};
use super::*;
use crate::context::{ContextBuilder, ContextItem, ModelLimits};
use crate::model::NativeContinuation;
use crate::session::ContextMode;
use std::sync::atomic::Ordering;

/// Seeds run by default. Set `RUNTIME_SEED` to rerun one seed, or
/// `RUNTIME_SEEDS` to run more.
const SEEDS: u64 = 512;

/// SplitMix64: small, seedable and good enough for test scheduling.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

/// Prints the seed of a failing run, even when the failure is a panic.
struct SeedGuard(u64);
impl Drop for SeedGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "randomized runtime run failed: rerun with RUNTIME_SEED={}",
                self.0
            );
        }
    }
}

const FAILURES: [Point; 10] = [
    Point::BeforeWrite,
    Point::PartialWrite,
    Point::Written,
    Point::Flushed,
    Point::Synced,
    Point::BeforeAck,
    Point::ArtifactWritten,
    Point::ArtifactSynced,
    Point::ArtifactRenamed,
    Point::ArtifactDirectorySynced,
];

/// A provider event in a plausible order most of the time, invalid sometimes.
fn provider_event(rng: &mut Rng) -> ProviderEvent {
    let block_index = rng.below(4) as u32;
    match rng.below(20) {
        0..=1 => ProviderEvent::Started,
        2..=5 => ProviderEvent::TextDelta {
            block_index,
            text: rng.pick(&["a", "bc", "\"q\"\n", "é", ""]).to_string(),
        },
        6 => ProviderEvent::ReasoningSummaryDelta {
            block_index,
            text: "think".into(),
        },
        7..=8 => ProviderEvent::ToolCallStarted {
            block_index,
            name: rng.pick(&["read", "write", ""]).to_string(),
            provider_call_id: rng.chance(50).then(|| "native".to_string()),
        },
        9..=11 => ProviderEvent::ToolArgumentsDelta {
            block_index,
            fragment: rng
                .pick(&["{}", "{\"a\":", "1}", "[1]", "{\"a\":1,\"a\":2}"])
                .to_string(),
        },
        12..=13 => ProviderEvent::ToolCallReady { block_index },
        14 => ProviderEvent::UsageUpdated {
            usage: Usage {
                input_tokens: Some(rng.below(100)),
                output_tokens: rng.chance(50).then(|| rng.below(100)),
                cached_input_tokens: None,
                reasoning_tokens: rng.chance(5).then_some(u64::MAX),
            },
        },
        15 => ProviderEvent::ProviderState {
            block_index,
            schema: "mock.state.v1".into(),
            payload: vec![rng.below(256) as u8; rng.below(32) as usize],
        },
        _ => {
            let outcome = *rng.pick(&[
                ProviderOutcome::Completed,
                ProviderOutcome::Completed,
                ProviderOutcome::Completed,
                ProviderOutcome::Failed,
                ProviderOutcome::Cancelled,
            ]);
            let finish_reason = match outcome {
                ProviderOutcome::Completed if rng.chance(95) => Some(*rng.pick(&[
                    FinishReason::Stop,
                    FinishReason::ToolCalls,
                    FinishReason::ToolCalls,
                    FinishReason::OutputLimit,
                    FinishReason::Refusal,
                ])),
                _ if rng.chance(5) => Some(FinishReason::Stop),
                _ => None,
            };
            let error =
                (outcome == ProviderOutcome::Failed && rng.chance(90)).then(|| ModelError {
                    code: ModelErrorCode::ProviderFailure,
                    message: rng.pick(&["boom", ""]).to_string(),
                    provider_code: None,
                    retry_after_ms: None,
                });
            ProviderEvent::Finished {
                outcome,
                finish_reason,
                error,
                continuation: rng.chance(40).then(|| NativeContinuation {
                    schema: "mock.continuation.v1".into(),
                    payload: vec![1, 2, 3],
                }),
            }
        }
    }
}

/// A well-formed stream for one inference, ending in a random way. Steps
/// interleave it with other inputs and occasionally corrupt it.
fn script(rng: &mut Rng) -> std::collections::VecDeque<ProviderEvent> {
    let mut events = std::collections::VecDeque::from([ProviderEvent::Started]);
    for _ in 0..rng.below(4) {
        events.push_back(ProviderEvent::TextDelta {
            block_index: 0,
            text: rng.pick(&["Hello", " wor\"ld\"", "\n", "é"]).to_string(),
        });
    }
    let ending = rng.below(100);
    let calls = if (40..80).contains(&ending) || rng.chance(10) {
        1 + rng.below(3) as u32
    } else {
        0
    };
    let mut fragments = Vec::new();
    for call in 0..calls {
        let name = *rng.pick(&["read", "read", "write", "edit", "bash", "unknown"]);
        let arguments = if rng.chance(12) { serde_json::json!({"invalid":true}) } else {
            match name {
                "write" => serde_json::json!({"path":"new", "text":"written"}),
                "edit" => serde_json::json!({"path":format!("f{call}"), "old_text":"hello", "new_text":"hi"}),
                "bash" => serde_json::json!({"command":"true"}),
                _ => serde_json::json!({"path":format!("f{call}")}),
            }
        }.to_string();
        let split = arguments.len() / 2;
        events.push_back(ProviderEvent::ToolCallStarted {
            block_index: 1 + call,
            name: name.into(),
            provider_call_id: rng.chance(50).then(|| format!("native-{call}")),
        });
        fragments.push((
            1 + call,
            vec![arguments[..split].into(), arguments[split..].into()],
        ));
    }
    // Interleave argument streams of different calls.
    while fragments.iter().any(|(_, f)| !f.is_empty()) {
        let open: Vec<_> = fragments
            .iter()
            .filter(|(_, f)| !f.is_empty())
            .map(|(b, _)| *b)
            .collect();
        let block_index = *rng.pick(&open);
        let (_, parts) = fragments
            .iter_mut()
            .find(|(b, _)| *b == block_index)
            .unwrap();
        events.push_back(ProviderEvent::ToolArgumentsDelta {
            block_index,
            fragment: parts.remove(0),
        });
    }
    for call in 0..calls {
        if rng.chance(95) {
            events.push_back(ProviderEvent::ToolCallReady {
                block_index: 1 + call,
            });
        }
    }
    if rng.chance(50) {
        events.push_back(ProviderEvent::UsageUpdated {
            usage: Usage {
                input_tokens: Some(10),
                output_tokens: Some(rng.below(50)),
                cached_input_tokens: None,
                reasoning_tokens: None,
            },
        });
    }
    if rng.chance(50) {
        events.push_back(ProviderEvent::ProviderState {
            block_index: 8,
            schema: "mock.state.v1".into(),
            payload: vec![rng.below(256) as u8; 16],
        });
    }
    let continue_native = rng.chance(50);
    let finished = |outcome, finish_reason, error| ProviderEvent::Finished {
        outcome,
        finish_reason,
        error,
        continuation: (continue_native && outcome == ProviderOutcome::Completed).then(|| {
            NativeContinuation {
                schema: "mock.continuation.v1".into(),
                payload: b"handle".to_vec(),
            }
        }),
    };
    match ending {
        0..=39 => events.push_back(finished(
            ProviderOutcome::Completed,
            Some(FinishReason::Stop),
            None,
        )),
        40..=79 => events.push_back(finished(
            ProviderOutcome::Completed,
            Some(FinishReason::ToolCalls),
            None,
        )),
        80..=83 => events.push_back(finished(
            ProviderOutcome::Completed,
            Some(FinishReason::OutputLimit),
            None,
        )),
        84..=86 => events.push_back(finished(
            ProviderOutcome::Completed,
            Some(FinishReason::Refusal),
            None,
        )),
        87..=90 => events.push_back(finished(
            ProviderOutcome::Failed,
            None,
            Some(ModelError {
                code: ModelErrorCode::Transport,
                message: "reset".into(),
                provider_code: None,
                retry_after_ms: None,
            }),
        )),
        91..=92 => events.push_back(finished(ProviderOutcome::Cancelled, None, None)),
        // The stream closes without `Finished`.
        _ => {}
    }
    events
}

fn random_command(d: &mut Driver, rng: &mut Rng, issued: &mut Vec<(CommandId, String)>) {
    let command = match rng.below(12) {
        0..=3 => {
            let prompt = rng.pick(&["hello", "use tools", "again"]).to_string();
            let (id, prompt) = match rng.below(10) {
                // Retry an earlier command, with the same or different content.
                0..=1 if !issued.is_empty() => {
                    let (id, original) = rng.pick(issued).clone();
                    (id, if rng.chance(70) { original } else { prompt })
                }
                _ => (d.new_command_id(), prompt),
            };
            issued.push((id.clone(), prompt.clone()));
            let content = match rng.below(20) {
                0 => Vec::new(),
                1 => vec![ContentBlock::Image {
                    artifact: ArtifactRef {
                        sha256: Digest::of(b"missing"),
                        bytes: 7,
                    },
                    mime_type: "image/png".into(),
                }],
                _ => text(&prompt),
            };
            Command::StartTurn {
                command_id: id,
                content,
            }
        }
        4..=5 => {
            let turn_id = match d.active_turn() {
                Some(turn) if rng.chance(85) => turn,
                _ => {
                    let turns: Vec<_> = d
                        .runtime
                        .snapshot()
                        .turns()
                        .map(|(id, _)| id.clone())
                        .collect();
                    if turns.is_empty() {
                        DeterministicEnv::new(0xffe).turn_id()
                    } else {
                        rng.pick(&turns).clone()
                    }
                }
            };
            Command::Cancel { turn_id }
        }
        6..=8 => {
            let snapshot = d.runtime.snapshot();
            let entries: Vec<_> = snapshot.entries().map(|(id, _)| id.clone()).collect();
            let head = match rng.below(10) {
                0 => None,
                1 => Some(DeterministicEnv::new(0xffd).entry_id()),
                _ if entries.is_empty() => None,
                _ => Some(rng.pick(&entries).clone()),
            };
            Command::SelectHead { head }
        }
        // Idle-only whole-turn controls: accepted, no-ops, unfinished and
        // off-branch targets all reach the core, which must stay consistent.
        9 => {
            let turns: Vec<_> = d
                .runtime
                .snapshot()
                .turns()
                .map(|(id, _)| id.clone())
                .collect();
            let turn_id = if turns.is_empty() {
                DeterministicEnv::new(0xffc).turn_id()
            } else {
                rng.pick(&turns).clone()
            };
            Command::SetContextControl {
                turn_id,
                mode: *rng.pick(&[
                    ContextMode::Default,
                    ContextMode::Pinned,
                    ContextMode::Excluded,
                ]),
            }
        }
        _ => Command::SwitchModel {
            policy: *rng.pick(&[
                Portability::Reject,
                Portability::OmitOptional,
                Portability::ConvertSupported,
            ]),
            model: ModelRef {
                provider_instance: "mock".into(),
                model: rng.pick(&["a", "b", ""]).to_string(),
            },
        },
    };
    d.command(command);
}

type Scripts = BTreeMap<InferenceId, std::collections::VecDeque<ProviderEvent>>;

/// A resolution for the first waiting invocation: usually valid, sometimes
/// one the core must refuse.
fn resolution(d: &Driver, rng: &mut Rng) -> Command {
    let invocation_id = if rng.chance(5) {
        DeterministicEnv::new(0xffb)
            .entry_id()
            .to_string()
            .parse()
            .unwrap()
    } else {
        d.runtime.view().awaiting[0].invocation_id.clone()
    };
    let resolution = if rng.chance(50) {
        UserResolution::AcknowledgedUnknown
    } else {
        UserResolution::Observed {
            status: *rng.pick(&[
                ToolStatus::Succeeded,
                ToolStatus::Succeeded,
                ToolStatus::Failed,
                ToolStatus::TimedOut,
                ToolStatus::Uncertain,
            ]),
            text: rng.pick(&["it ran", "nothing changed", ""]).to_string(),
        }
    };
    Command::ResolveUncertain {
        invocation_id,
        resolution,
    }
}

/// Whether a healthy runtime's journal holds a response whose calls are not
/// answered yet: where a tool runtime would authorize and run them.
fn processing(d: &Driver) -> bool {
    matches!(d.runtime.phase(), Phase::Durable(_))
        && d.store.as_ref().is_some_and(|store| {
            let snapshot = store.snapshot();
            snapshot.state() == RuntimeState::Processing
                && snapshot
                    .0
                    .pending(snapshot.head())
                    .is_ok_and(|pending| !pending.is_empty())
        })
}

/// Kill the process at this step and restart it. With a response's calls
/// pending, a tool runtime sometimes wrote its authorization, and sometimes
/// its execution intent, just before the crash.
fn crash(d: &mut Driver, rng: &mut Rng) {
    let kind = *rng.pick(&[Crash::Lost, Crash::Durable, Crash::Torn]);
    d.crash(kind);
}

fn random_step(
    d: &mut Driver,
    rng: &mut Rng,
    issued: &mut Vec<(CommandId, String)>,
    scripts: &mut Scripts,
) {
    // Calls wait for answers only briefly, so crash there more often.
    if rng.chance(
        if matches!(
            d.runtime.phase(),
            Phase::Durable(
                RuntimeState::Processing | RuntimeState::Authorizing | RuntimeState::Executing
            )
        ) {
            12
        } else {
            2
        },
    ) {
        return crash(d, rng);
    }
    match d.runtime.phase() {
        // A process that faulted is restarted eventually.
        Phase::Faulted if rng.chance(20) => return d.crash(Crash::Lost),
        Phase::AwaitingResolution if d.pending.is_none() && rng.chance(25) => {
            let command = resolution(d, rng);
            d.command(command);
            return;
        }
        _ => {}
    }
    if d.planning.is_some() && rng.chance(55) {
        d.plan();
        return;
    }
    if d.executing.is_some() && rng.chance(35) {
        let (status, code) = *rng.pick(&[
            (ToolStatus::Succeeded, None),
            (ToolStatus::Failed, Some(ResultCode::StalePlan)),
            (ToolStatus::Failed, Some(ResultCode::NonzeroExit)),
            (ToolStatus::TimedOut, Some(ResultCode::Timeout)),
            (
                ToolStatus::InfrastructureFailure,
                Some(ResultCode::SandboxUnavailable),
            ),
            (ToolStatus::Cancelled, Some(ResultCode::Cancelled)),
        ]);
        d.executor
            .success_wins
            .store(rng.chance(30), Ordering::Release);
        d.executor
            .outcomes
            .lock()
            .unwrap()
            .push_back(outcome(status, code));
        d.execute();
        return;
    }
    if let Some(ticket) = d.runtime.view().pending_approval
        && d.pending.is_none()
        && rng.chance(35)
    {
        let choice = rng.below(5);
        d.command(Command::SubmitApproval {
            approval_id: ticket.approval_id,
            plan_digest: if choice == 0 {
                Digest::of(b"mismatch")
            } else {
                ticket.plan_digest
            },
            decision: if choice == 1 {
                Decision::Deny
            } else {
                Decision::Allow
            },
            remember: choice >= 3,
        });
        return;
    }
    if d.verification.is_some() && rng.chance(50) {
        let (token, refs) = d.verification.take().unwrap();
        let missing = if rng.chance(50) {
            refs
        } else {
            d.root.put_artifact(b"missing").unwrap();
            Vec::new()
        };
        d.step(Input::ArtifactsVerified { token, missing });
        return;
    }
    // Cancellation racing a commit in flight is the subtle case; favor it.
    if d.pending.is_some()
        && rng.chance(4)
        && let Some(turn_id) = d.active_turn()
    {
        d.command(Command::Cancel { turn_id });
        return;
    }
    if d.pending.is_some() && rng.chance(40) {
        if rng.chance(3) {
            d.fail = Some(*rng.pick(&FAILURES));
        }
        d.complete();
        return;
    }
    match rng.below(20) {
        0..=3 => random_command(d, rng, issued),
        4..=14 if d.inference.is_some() => {
            let inference = d.inference.unwrap();
            let script = scripts.entry(inference).or_insert_with(|| script(rng));
            let event = match script.pop_front() {
                Some(event) if rng.chance(97) => event,
                _ if script.is_empty() && rng.chance(70) => {
                    d.ended();
                    return;
                }
                _ => provider_event(rng),
            };
            d.provider(event);
        }
        15 => {
            // A stale or never-issued stream.
            let inference = if !d.started.is_empty() && rng.chance(70) {
                *rng.pick(&d.started)
            } else {
                InferenceId(1000 + rng.below(3))
            };
            let event = provider_event(rng);
            d.step(Input::Provider { inference, event });
        }
        // A stream closing early, without `Finished`.
        16 if d.inference.is_some() && rng.chance(25) => {
            d.ended();
        }
        18 if rng.chance(50) => {
            d.step(Input::Planned {
                token: PlanToken(9000),
                result: Err(crate::tools::Refusal {
                    status: ToolStatus::InvalidArguments,
                    code: ResultCode::InvalidArguments,
                    detail: "stale".into(),
                }),
            });
        }
        19 if rng.chance(50) => {
            d.step(Input::ToolFinished {
                token: ExecutionToken(9000),
                outcome: outcome(ToolStatus::Succeeded, None),
            });
        }
        17 => {
            let timer = match d.timers.iter().next() {
                Some(timer) if rng.chance(80) => *timer,
                _ => TimerId(500 + rng.below(3)),
            };
            d.fire(timer);
        }
        _ => {
            d.complete();
        }
    }
}

/// Answer commits, play out or end streams and fire timers until the turn is over.
fn quiesce(d: &mut Driver, rng: &mut Rng, scripts: &mut Scripts) {
    for _ in 0..10_000 {
        if processing(d) && rng.chance(12) {
            crash(d, rng);
            continue;
        }
        if d.complete() || d.plan() || d.execute() {
            continue;
        }
        if d.runtime.view().pending_approval.is_some()
            && let Some(timer) = d.timers.iter().next().copied()
        {
            d.fire(timer);
            continue;
        }
        let phase = d.runtime.phase();
        match phase {
            Phase::Faulted => {
                d.crash(Crash::Lost);
                continue;
            }
            Phase::AwaitingResolution => {
                let command = resolution(d, rng);
                d.command(command);
                continue;
            }
            Phase::RecoveryRequired => panic!("recovery stalled with nothing in flight"),
            Phase::Durable(_) if d.runtime.snapshot().active_turn().is_none() => return,
            Phase::Durable(_) => {}
        }
        let before = d.segment.effects.len();
        if let Some(inference) = d.inference {
            let script = scripts.entry(inference).or_insert_with(|| script(rng));
            match script.pop_front() {
                Some(event) => {
                    d.provider(event);
                    continue;
                }
                None => {
                    d.ended();
                }
            }
        }
        if d.segment.effects.len() == before
            && let Some(timer) = d.timers.iter().next().copied()
        {
            d.fire(timer);
        }
        assert!(d.segment.effects.len() > before, "no progress in {phase:?}");
    }
    panic!("did not quiesce");
}

/// What the runs reached, printed with `--nocapture` to show coverage.
type Coverage = BTreeMap<String, u64>;

fn tally(finished: &Finished, coverage: &mut Coverage) {
    let mut count = |key: String| *coverage.entry(key).or_default() += 1;
    count("runs".into());
    if finished.faulted {
        count("runs faulted by injected I/O".into());
    }
    *coverage.entry("crashes".into()).or_default() += finished.crashes;
    *coverage.entry("torn tails repaired".into()).or_default() += finished.repairs;
    let mut count = |key: String| *coverage.entry(key).or_default() += 1;
    for (_, _, event) in finished.reopened.events() {
        match event {
            JournalEvent::TurnFinished(f) => count(format!(
                "turn {:?} {}",
                f.outcome,
                f.code.as_deref().unwrap_or("-")
            )),
            JournalEvent::StateChanged(c) => count(format!("transition {:?}>{:?}", c.from, c.to)),
            JournalEvent::ExecutionIntent(_) => count("execution intents".into()),
            JournalEvent::ExecutionUncertain(_) => count("execution uncertain".into()),
            JournalEvent::RecoveryResolved(r) => count(format!("resolved {:?}", r.resolution)),
            JournalEvent::ApprovalRequested(_) => count("approval requests".into()),
            JournalEvent::AuthorizationDecided(a) => {
                count(format!("authorization {:?} {}", a.decision, a.reason))
            }
            JournalEvent::ConfigurationChanged(_) => count("remember grants".into()),
            _ => {}
        }
    }
    for (_, stored) in finished.reopened.entries() {
        if let EntryPayload::ToolResult(r) = &stored.entry.payload {
            count(format!("tool result {:?}", r.result.status));
            if r.result.invocation_id.is_some()
                && r.result.metadata.provenance == Provenance::Runtime
                && r.result.metadata.code.as_deref() == Some("process_restart")
            {
                count("D9 read intent closures".into());
            }
            count(format!(
                "tool code {}",
                r.result.metadata.code.as_deref().unwrap_or("succeeded")
            ));
        }
    }
    for segment in finished
        .segments
        .iter()
        .take(finished.segments.len().saturating_sub(1))
    {
        let view = segment.views.last().unwrap();
        count(format!("restart from {:?}", view.phase));
        if view.pending_approval.is_some() {
            count("approvals abandoned by restart".into());
        }
        if view.snapshot.active_turn().is_some()
            && view.snapshot.events().last().is_some_and(|(_, _, e)| matches!(e, JournalEvent::AuthorizationDecided(a) if a.decision == Decision::Deny)) {
            count("restarts between denial and result".into());
        }
    }
    for effect in finished.segments.iter().flat_map(|s| &s.effects) {
        match effect {
            Effect::PlanCall { .. } => count("planning effects".into()),
            Effect::Execute { .. } => count("executor dispatches".into()),
            Effect::CancelExecution { .. } => count("executor cancellation requests".into()),
            Effect::Persist { .. } => count("commits".into()),
            Effect::Emit(RuntimeEvent {
                kind: EventKind::Recovered { .. },
                ..
            }) => count("turns closed by recovery".into()),
            Effect::Emit(RuntimeEvent {
                kind: EventKind::ContextBuilt { flags, .. },
                ..
            }) => count(format!(
                "context built{}{}{}",
                if flags.budget_unknown {
                    ", budget unknown"
                } else {
                    ""
                },
                if flags.overflow { ", overflow" } else { "" },
                if flags.compaction_recommended {
                    ", compaction recommended"
                } else {
                    ""
                }
            )),
            Effect::Reply { result: Ok(a), .. } => count(format!(
                "accepted {}",
                format!("{a:?}").split(' ').next().unwrap()
            )),
            Effect::Reply { result: Err(r), .. } => count(format!("rejected {r:?}")),
            Effect::Emit(RuntimeEvent {
                kind: EventKind::Diagnostic(d),
                ..
            }) => count(format!(
                "diagnostic {}",
                format!("{d:?}").split(' ').next().unwrap()
            )),
            _ => {}
        }
    }
}

/// Every context the core built during the run is rebuilt from the reopened
/// journal with the same fingerprint and flags, every stream started from a
/// known budget that fits, and builds at a random head, for its own model and
/// as a preview, pass the builder's invariant checks. A broken invariant
/// would also have faulted the core with an internal cause.
fn check_contexts(finished: &Finished, rng: &mut Rng) {
    let first = &finished.segments[0];
    let snapshot = &finished.reopened;
    // A remembered allowance changes the configuration artifact mid-turn.
    // Rebuild each immutable projection under its own published configuration,
    // not the final session-wide policy (navigation never undoes that policy).
    let mut configurations = BTreeMap::new();
    for segment in &finished.segments {
        configurations.insert(
            segment.configuration.artifact().sha256.clone(),
            segment.configuration.clone(),
        );
        for effect in &segment.effects {
            if let Effect::Persist { artifacts, .. } = effect {
                for bytes in artifacts {
                    let reference = ArtifactRef {
                        sha256: Digest::of(bytes),
                        bytes: bytes.len() as u64,
                    };
                    if let Ok(configuration) = SessionConfiguration::parse(reference.clone(), bytes)
                    {
                        configurations.insert(reference.sha256, configuration);
                    }
                }
            }
        }
    }
    let mut built_contexts = BTreeMap::new();
    for segment in &finished.segments {
        for event in segment.events() {
            match &event.kind {
                EventKind::ContextBuilt {
                    head,
                    fingerprint,
                    flags,
                    ..
                } => {
                    let view = segment
                        .views
                        .iter()
                        .find(|v| v.event_seq >= event.seq)
                        .unwrap();
                    let configuration = &configurations[&view.snapshot.configuration().sha256];
                    let definitions = crate::tools::PolicySnapshot::parse(
                        configuration.artifact().clone(),
                        configuration.bytes(),
                    )
                    .map(|p| segment.tools.definitions(&p))
                    .unwrap_or_default();
                    let builder = ContextBuilder::new(&segment.config.context, configuration)
                        .with_tools(&definitions);
                    let rebuilt = builder.build(&view.snapshot, Some(head), None).unwrap();
                    assert_eq!(
                        &rebuilt.projection.fingerprint, fingerprint,
                        "rebuild after reopen differs"
                    );
                    assert_eq!(&rebuilt.report.flags, flags);
                    for (id, entry) in view.snapshot.entries() {
                        assert_eq!(
                            snapshot.entry(id).unwrap().record_checksum,
                            entry.record_checksum
                        );
                    }
                    built_contexts.insert((head.clone(), fingerprint.clone()), *flags);
                }
                EventKind::Fault(FaultCause::Internal(code)) => panic!("internal fault: {code}"),
                _ => {}
            }
        }
    }
    for effect in finished.segments.iter().flat_map(|s| &s.effects) {
        if let Effect::StartInference {
            head,
            fingerprint,
            input,
            ..
        } = effect
        {
            assert_native_owners(input);
            assert_eq!(crate::context::fingerprint(input), *fingerprint);
            let flags = built_contexts
                .get(&(head.clone(), fingerprint.clone()))
                .unwrap();
            assert!(!flags.budget_unknown && !flags.overflow);
        }
    }
    let builder = ContextBuilder::new(
        &first.config.context,
        &configurations[&snapshot.configuration().sha256],
    );
    let ids: Vec<EntryId> = snapshot.entries().map(|(id, _)| id.clone()).collect();
    let head = (!ids.is_empty() && rng.chance(90)).then(|| rng.pick(&ids).clone());
    let other = ModelRef {
        provider_instance: "other".into(),
        model: "model".into(),
    };
    let built = builder.build(snapshot, head.as_ref(), None).unwrap();
    assert_eq!(builder.build(snapshot, head.as_ref(), None).unwrap(), built);
    assert_native_owners(&built.projection.input);
    let switched = builder
        .build(snapshot, head.as_ref(), Some(&other))
        .unwrap();
    assert_native_owners(&switched.projection.input);
}

/// Bias part of each known-budget run toward the new gates. Purely random
/// provider fragments otherwise reach a valid tool finish too rarely to cover
/// approval/cancellation races reliably. Outcomes, crash fates and ack races
/// remain seeded, and every effect still passes the same storage oracle.
fn tool_interleavings(d: &mut Driver, rng: &mut Rng, seed: u64) {
    let action = seed % 8;
    let name = if action == 6 {
        "read"
    } else {
        *rng.pick(&["write", "edit", "bash"])
    };
    let arguments = match name {
        "read" => serde_json::json!({"path":"a"}),
        "edit" => serde_json::json!({"path":"a","old_text":"hello","new_text":"hi"}),
        "bash" => serde_json::json!({"command":"true"}),
        _ => serde_json::json!({"path":"new","text":"written"}),
    };
    super::pipeline::propose(d, &[(name, arguments.clone()), (name, arguments)]);
    for _ in 0..200 {
        if let Some(ticket) = d.runtime.view().pending_approval
            && d.pending.is_none()
        {
            match action {
                1 => {
                    d.approval(Decision::Deny, false);
                }
                3 => {
                    let request = d.command(Command::SubmitApproval {
                        approval_id: ticket.approval_id,
                        plan_digest: Digest::of(b"mismatch"),
                        decision: Decision::Allow,
                        remember: true,
                    });
                    assert_eq!(d.rejected(request), Rejection::Invalid("approval_mismatch"));
                    d.approval(Decision::Allow, false);
                }
                4 => {
                    // Never answer this ticket: expire it or kill the process.
                    if rng.chance(50) {
                        d.fire(*d.timers.iter().next().unwrap());
                    } else {
                        crash(d, rng);
                    }
                }
                5 => {
                    d.cancel();
                }
                7 => {
                    d.approval(Decision::Deny, false);
                    d.complete(); // denial durable, result is not yet acknowledged
                    crash(d, rng);
                }
                _ => {
                    d.approval(Decision::Allow, action == 2);
                }
            }
            continue;
        }
        if d.executing.is_some() && d.pending.is_none() {
            if name == "read" && rng.chance(50) {
                // Crash with the intent durable but no executor answer: D9
                // must close it without uncertainty or storage rejection.
                crash(d, rng);
                continue;
            }
            let (status, code) = *rng.pick(&[
                (ToolStatus::Succeeded, None),
                (ToolStatus::Failed, Some(ResultCode::StalePlan)),
                (ToolStatus::Failed, Some(ResultCode::NonzeroExit)),
                (ToolStatus::TimedOut, Some(ResultCode::Timeout)),
                (
                    ToolStatus::InfrastructureFailure,
                    Some(ResultCode::SandboxUnavailable),
                ),
                (ToolStatus::Cancelled, Some(ResultCode::Cancelled)),
            ]);
            d.executor
                .outcomes
                .lock()
                .unwrap()
                .push_back(outcome(status, code));
            if action == 6 || rng.chance(20) {
                d.executor
                    .success_wins
                    .store(rng.chance(50), Ordering::Release);
                d.cancel();
                if rng.chance(50) {
                    d.complete();
                } // completion before or after cancel ack
            }
            d.execute();
            if rng.chance(12) {
                crash(d, rng);
            } // result written, lost, or torn
            continue;
        }
        if d.complete() || d.plan() {
            continue;
        }
        match d.runtime.phase() {
            Phase::AwaitingResolution => {
                d.acknowledge_all();
            }
            Phase::Faulted => crash(d, rng),
            Phase::Durable(RuntimeState::Idle) => return,
            Phase::Durable(RuntimeState::Inferring) => {
                d.provider(ProviderEvent::Started);
                d.provider(ProviderEvent::Finished {
                    outcome: ProviderOutcome::Completed,
                    finish_reason: Some(FinishReason::Stop),
                    error: None,
                    continuation: None,
                });
            }
            phase => panic!("tool-biased run stalled in {phase:?}"),
        }
    }
    panic!("tool-biased run did not finish");
}

fn run_seed(seed: u64) -> Finished {
    let _guard = SeedGuard(seed);
    let mut rng = Rng(seed);
    let mut config = RuntimeConfig {
        max_inferences_per_turn: 1 + rng.below(3) as u32,
        max_partial_bytes: *rng.pick(&[600, 4_000, 1 << 20]),
        inference_deadline: rng.chance(30).then(|| std::time::Duration::from_secs(10)),
        ..RuntimeConfig::default()
    };
    // A separate generator, so the interleavings of each seed are unchanged.
    let mut context = Rng(seed ^ 0xc0e7_e7c7);
    // Without a limit every turn fails in `Preparing`, so that case is rarer.
    let limit = *context.pick(&[
        None,
        Some(16_400),
        Some(16_420),
        Some(16_400),
        Some(16_420),
        Some(1_000_000),
        Some(1_000_000),
        Some(1_000_000),
    ]);
    if let Some(context_tokens) = limit {
        // With the default 16,384 tokens of deductions, small limits leave
        // room for a few turns only, so overflow is reached in some runs.
        // In some runs `b` stays unlisted: turns after a switch to it fail
        // with `context_budget_unknown`, as every turn does without a limit.
        let listed: &[&str] = if context.chance(25) {
            &["scripted", "a"]
        } else {
            &["scripted", "a", "b"]
        };
        for model in listed {
            config.context.catalog.insert(
                ModelRef {
                    provider_instance: "mock".into(),
                    model: (*model).into(),
                },
                ModelLimits {
                    context_tokens: Some(context_tokens),
                    max_output_tokens: None,
                },
            );
        }
    }
    let mut policy = configuration();
    policy["workspace_read"] = serde_json::json!(if rng.chance(30) { "ask" } else { "allow" });
    if rng.chance(10) {
        policy["workspace_mutation"] = serde_json::json!("deny");
    }
    if rng.chance(10) {
        policy["enabled_tools"] = serde_json::json!(["read", "write"]);
    }
    let backends = if rng.chance(10) {
        Vec::new()
    } else if rng.chance(15) {
        vec![ExecutionBackend::TrustedFs]
    } else {
        vec![ExecutionBackend::TrustedFs, ExecutionBackend::Bubblewrap]
    };
    let mut d = Driver::with_tools(
        config,
        &serde_json::to_vec(&policy).unwrap(),
        ToolSet::new(backends),
    );
    if limit == Some(1_000_000) {
        tool_interleavings(&mut d, &mut rng, seed);
    }
    let mut issued = Vec::new();
    let mut scripts = Scripts::new();
    for _ in 0..60 + rng.below(120) {
        random_step(&mut d, &mut rng, &mut issued, &mut scripts);
    }
    quiesce(&mut d, &mut rng, &mut scripts);
    let finished = d.finish();
    check_turns(&finished);
    check_resync(&finished);
    check_replay(&finished);
    check_contexts(&finished, &mut context);
    compaction::randomized(seed);
    finished
}

#[test]
fn randomized_interleavings_keep_storage_and_events_consistent() {
    if let Some(seed) = std::env::var("RUNTIME_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        run_seed(seed);
        return;
    }
    let seeds = std::env::var("RUNTIME_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(SEEDS);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()) as u64;
    let coverage = std::sync::Mutex::new(Coverage::new());
    std::thread::scope(|scope| {
        for thread in 0..threads {
            let coverage = &coverage;
            scope.spawn(move || {
                // Each thread's oracle switch is independent and on by default.
                let mut local = Coverage::new();
                for seed in (thread..seeds).step_by(threads as usize) {
                    tally(&run_seed(seed), &mut local);
                }
                let mut total = coverage.lock().unwrap();
                for (key, n) in local {
                    *total.entry(key).or_default() += n;
                }
            });
        }
    });
    let coverage = coverage.into_inner().unwrap();
    assert_eq!(coverage["runs"], seeds);
    if seeds >= SEEDS {
        for gate in [
            "approval requests",
            "approvals abandoned by restart",
            "authorization Allow allowed",
            "authorization Deny approval_denied",
            "authorization Deny approval_expired",
            "remember grants",
            "rejected Invalid(\"approval_mismatch\")",
            "executor dispatches",
            "executor cancellation requests",
            "planning effects",
            "restarts between denial and result",
            "D9 read intent closures",
            "restart from Durable(Authorizing)",
            "restart from Durable(Executing)",
            "tool result Failed",
            "tool result TimedOut",
            "tool result InfrastructureFailure",
            "tool result Succeeded",
            "tool result Cancelled",
        ] {
            assert!(
                coverage.get(gate).copied().unwrap_or(0) > 0,
                "randomized run missed {gate}"
            );
        }
    }
    for (key, n) in &coverage {
        eprintln!("{n:>7}  {key}");
    }
}

fn assert_native_owners(input: &crate::model::ModelInput) {
    if let Some(continuation) = &input.continuation {
        assert_eq!(
            continuation.owner, input.model,
            "continuation crossed owners"
        );
    }
    for item in &input.items {
        if let ContextItem::Assistant { content, .. } = item {
            for block in content {
                if let ContentBlock::ProviderState { owner, .. } = block {
                    assert_eq!(owner, &input.model, "native state crossed owners");
                }
            }
        }
    }
}
