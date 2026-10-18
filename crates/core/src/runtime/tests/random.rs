//! Seeded random interleavings of commands, provider events, stale streams,
//! timers and persist failures, all against real storage.

use super::*;

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
        events.push_back(ProviderEvent::ToolCallStarted {
            block_index: 1 + call,
            name: rng.pick(&["read", "write"]).to_string(),
            provider_call_id: rng.chance(50).then(|| format!("native-{call}")),
        });
        fragments.push((
            1 + call,
            vec![format!("{{\"path\":\"f{call}\""), "}".to_string()],
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
    let finished = |outcome, finish_reason, error| ProviderEvent::Finished {
        outcome,
        finish_reason,
        error,
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
    let command = match rng.below(10) {
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
        _ => Command::SwitchModel {
            model: ModelRef {
                provider_instance: "mock".into(),
                model: rng.pick(&["a", "b", ""]).to_string(),
            },
        },
    };
    d.command(command);
}

type Scripts = BTreeMap<InferenceId, std::collections::VecDeque<ProviderEvent>>;

fn random_step(
    d: &mut Driver,
    rng: &mut Rng,
    issued: &mut Vec<(CommandId, String)>,
    scripts: &mut Scripts,
) {
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
        if d.complete() {
            continue;
        }
        let phase = d.runtime.phase();
        if matches!(phase, Phase::Faulted | Phase::RecoveryRequired)
            || d.runtime.snapshot().active_turn().is_none()
        {
            return;
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
    for (_, _, event) in finished.reopened.events() {
        match event {
            JournalEvent::TurnFinished(f) => count(format!(
                "turn {:?} {}",
                f.outcome,
                f.code.as_deref().unwrap_or("-")
            )),
            JournalEvent::StateChanged(c) => count(format!("transition {:?}>{:?}", c.from, c.to)),
            _ => {}
        }
    }
    for (_, stored) in finished.reopened.entries() {
        if let EntryPayload::ToolResult(r) = &stored.entry.payload {
            count(format!("tool result {:?}", r.result.status));
        }
    }
    for effect in finished.segments.iter().flat_map(|s| &s.effects) {
        match effect {
            Effect::Persist { .. } => count("commits".into()),
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

fn run_seed(seed: u64) -> Finished {
    let _guard = SeedGuard(seed);
    let mut rng = Rng(seed);
    let config = RuntimeConfig {
        max_inferences_per_turn: 1 + rng.below(3) as u32,
        max_partial_bytes: *rng.pick(&[600, 4_000, 1 << 20]),
        ..RuntimeConfig::default()
    };
    let mut d = Driver::with_config(config);
    let mut issued = Vec::new();
    let mut scripts = Scripts::new();
    for _ in 0..60 + rng.below(120) {
        random_step(&mut d, &mut rng, &mut issued, &mut scripts);
    }
    quiesce(&mut d, &mut rng, &mut scripts);
    let finished = d.finish();
    check_turns(&finished);
    check_resync(&finished);
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
    for (key, n) in &coverage {
        eprintln!("{n:>7}  {key}");
    }
}
