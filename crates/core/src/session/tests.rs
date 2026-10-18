use super::{codec, io::Point, state, *};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::Arc,
};

fn at() -> Timestamp {
    "2026-10-17T12:00:00Z".parse().unwrap()
}
fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/rfcs/001/examples")
}
fn environment() -> (tempfile::TempDir, Arc<DataRoot>) {
    let temp = tempfile::tempdir().unwrap();
    let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
    (temp, root)
}
fn source(name: &str) -> Vec<u8> {
    fs::read(examples().join(format!("{name}.jsonl"))).unwrap()
}
fn load_artifacts(root: &DataRoot) {
    for entry in fs::read_dir(examples().join("artifacts")).unwrap() {
        let entry = entry.unwrap();
        let reference = root.put_artifact(&fs::read(entry.path()).unwrap()).unwrap();
        assert_eq!(
            reference.sha256.as_str(),
            entry.file_name().to_str().unwrap()
        );
    }
}
fn install(root: &DataRoot, bytes: &[u8]) -> SessionId {
    let envelope: Value =
        serde_json::from_slice(bytes.split(|b| *b == b'\n').next().unwrap()).unwrap();
    let id: SessionId = serde_json::from_value(envelope["record"]["session_id"].clone()).unwrap();
    let mut file = root.journal(&id, true, true).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
    root.sessions.sync_all().unwrap();
    id
}
fn fixture(name: &str) -> (tempfile::TempDir, Arc<DataRoot>, SessionId) {
    let (temp, root) = environment();
    load_artifacts(&root);
    let id = install(&root, &source(name));
    (temp, root, id)
}
fn journal_path(root: &DataRoot, id: &SessionId) -> PathBuf {
    root.path().join("sessions").join(format!("{id}.jsonl"))
}
fn records(bytes: &[u8]) -> Vec<Value> {
    bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap()["record"].clone())
        .collect()
}
fn reframe(records: &[Value]) -> Vec<u8> {
    let mut previous = Value::Null;
    let mut out = Vec::new();
    for record in records {
        let mut record = record.clone();
        record["previous_checksum"] = previous;
        let raw = serde_json::to_vec(&record).unwrap();
        let digest = Digest::of(&raw);
        out.extend_from_slice(b"{\"record\":");
        out.extend_from_slice(&raw);
        out.extend_from_slice(format!(",\"sha256\":\"{digest}\"}}\n").as_bytes());
        previous = json!(digest);
    }
    out
}
fn fresh() -> (tempfile::TempDir, Arc<DataRoot>, JournalStore) {
    let (temp, root) = environment();
    let configuration = root.put_artifact(br#"{"policy":"fixture"}"#).unwrap();
    let header = SessionHeader {
        workspace: "/workspace".into(),
        initial_model: ModelRef {
            provider_instance: "mock".into(),
            model: "scripted".into(),
        },
        policy_revision: "initial".into(),
        configuration,
    };
    let store = JournalStore::create(root.clone(), SessionId::new(), header, at()).unwrap();
    (temp, root, store)
}
fn audit(snapshot: &SessionSnapshot, events: Vec<JournalEvent>) -> Commit {
    Commit {
        command_id: None,
        command_fingerprint: None,
        head: HeadChange {
            from: snapshot.head().cloned(),
            to: snapshot.head().cloned(),
            reason: HeadReason::Retain,
        },
        entries: vec![],
        events,
    }
}
fn navigate(snapshot: &SessionSnapshot, to: Option<EntryId>) -> Commit {
    Commit {
        head: HeadChange {
            from: snapshot.head().cloned(),
            to,
            reason: HeadReason::Navigate,
        },
        ..audit(snapshot, vec![])
    }
}
fn start_turn(snapshot: &SessionSnapshot, text: &str) -> Commit {
    let turn = TurnId::new();
    let entry = EntryId::new();
    Commit {
        command_id: None,
        command_fingerprint: None,
        head: HeadChange {
            from: snapshot.head().cloned(),
            to: Some(entry.clone()),
            reason: HeadReason::Advance,
        },
        entries: vec![Entry {
            id: entry.clone(),
            parent_id: snapshot.head().cloned(),
            turn_id: Some(turn.clone()),
            payload: EntryPayload::UserMessage(UserMessage {
                content: vec![ContentBlock::Text { text: text.into() }],
            }),
        }],
        events: vec![
            JournalEvent::TurnStarted(TurnStarted {
                turn_id: turn.clone(),
                user_entry_id: entry,
                configuration: snapshot.configuration().clone(),
            }),
            JournalEvent::StateChanged(StateChanged {
                turn_id: turn,
                from: RuntimeState::Idle,
                to: RuntimeState::Preparing,
            }),
        ],
    }
}
fn fail_turn(store: &mut JournalStore) {
    let snapshot = store.snapshot();
    let turn = snapshot.active_turn().unwrap().clone();
    store
        .append(
            audit(
                &snapshot,
                vec![JournalEvent::TurnFinished(TurnFinished {
                    turn_id: turn,
                    outcome: TurnOutcome::Failed,
                    code: Some("stopped_before_inference".into()),
                })],
            ),
            at(),
        )
        .unwrap();
}

fn finished_turn(snapshot: &SessionSnapshot, text: &str) -> Commit {
    let mut commit = start_turn(snapshot, text);
    commit.events.push(JournalEvent::TurnFinished(TurnFinished {
        turn_id: commit.entries[0].turn_id.clone().unwrap(),
        outcome: TurnOutcome::Failed,
        code: Some("stopped_before_inference".into()),
    }));
    commit
}

fn transition(snapshot: &SessionSnapshot, from: RuntimeState, to: RuntimeState) -> Commit {
    audit(
        snapshot,
        vec![JournalEvent::StateChanged(StateChanged {
            turn_id: snapshot.active_turn().unwrap().clone(),
            from,
            to,
        })],
    )
}

/// Benchmark workloads, each appended as a single chain from the root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload {
    /// One user entry plus a failed turn per commit, with 4 KiB of text.
    PlainTurns,
    /// Complete authorized tool exchanges spread over twelve commits per turn.
    ToolTurns,
    /// `ToolTurns` whose opening commit binds a command, as the runtime's
    /// `StartTurn` does: one more artifact write, sync and read-back per turn.
    ToolTurnsWithCommands,
}

/// Commits per turn of the tool workloads.
const TOOL_TURN_COMMITS: usize = 12;

/// Identifiers carried between the separate commits of benchmark tool turns.
struct ToolTurns {
    plan: ArtifactRef,
    step: usize,
    call: Option<CallRef>,
    invocation: Option<InvocationId>,
}
impl ToolTurns {
    fn next(&mut self, snapshot: &SessionSnapshot) -> Commit {
        use RuntimeState::*;
        let step = self.step;
        self.step = (step + 1) % TOOL_TURN_COMMITS;
        match step {
            0 => start_turn(snapshot, "use one tool"),
            1 | 9 => transition(snapshot, Preparing, Inferring),
            2 => {
                let call_id = CallId::new();
                let commit = response_commit(
                    snapshot,
                    vec![ContentBlock::ToolCall {
                        call_id: call_id.clone(),
                        name: "read".into(),
                        arguments: serde_json::Map::new(),
                        provider_call_id: None,
                    }],
                    FinishReason::ToolCalls,
                );
                self.call = Some(CallRef {
                    response_entry_id: commit.entries[0].id.clone(),
                    call_id,
                });
                commit
            }
            3 => transition(snapshot, Processing, Authorizing),
            4 => {
                let turn_id = snapshot.active_turn().unwrap().clone();
                let call = self.call.clone().unwrap();
                let decision_id = DecisionId::new();
                let invocation_id = InvocationId::new();
                self.invocation = Some(invocation_id.clone());
                audit(
                    snapshot,
                    vec![
                        JournalEvent::AuthorizationDecided(AuthorizationDecided {
                            turn_id: turn_id.clone(),
                            decision_id: decision_id.clone(),
                            call: call.clone(),
                            approval_id: None,
                            plan_digest: self.plan.sha256.clone(),
                            plan: self.plan.clone(),
                            policy_revision: snapshot.policy_revision().into(),
                            decision: Decision::Allow,
                            reason: "benchmark read".into(),
                        }),
                        JournalEvent::ExecutionIntent(ExecutionIntent {
                            turn_id,
                            invocation_id,
                            call,
                            decision_id,
                            plan_digest: self.plan.sha256.clone(),
                            backend: ExecutionBackend::TrustedFs,
                            timeout_ms: 30_000,
                        }),
                    ],
                )
            }
            5 => transition(snapshot, Authorizing, Executing),
            6 => self.tool_result_commit(snapshot),
            7 => transition(snapshot, Executing, Continuing),
            8 => transition(snapshot, Continuing, Preparing),
            10 => response_commit(
                snapshot,
                vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                FinishReason::Stop,
            ),
            _ => audit(
                snapshot,
                vec![JournalEvent::TurnFinished(TurnFinished {
                    turn_id: snapshot.active_turn().unwrap().clone(),
                    outcome: TurnOutcome::Completed,
                    code: None,
                })],
            ),
        }
    }
}

impl ToolTurns {
    /// The executor's successful result for the current call.
    fn tool_result_commit(&self, snapshot: &SessionSnapshot) -> Commit {
        tool_result(
            snapshot,
            self.call.clone().unwrap(),
            self.invocation.clone(),
            ToolStatus::Succeeded,
            Provenance::Executor,
        )
    }
}

/// Append `commits` commits of a workload, returning each append's duration.
fn append_workload(
    root: &DataRoot,
    store: &mut JournalStore,
    workload: Workload,
    commits: usize,
) -> Vec<std::time::Duration> {
    let text = "x".repeat(4096);
    let mut tools = ToolTurns {
        plan: root.put_artifact(br#"{"plan":"read"}"#).unwrap(),
        step: 0,
        call: None,
        invocation: None,
    };
    (0..commits)
        .map(|_| {
            let snapshot = store.snapshot();
            let mut commit = match workload {
                Workload::PlainTurns => finished_turn(&snapshot, &text),
                Workload::ToolTurns | Workload::ToolTurnsWithCommands => tools.next(&snapshot),
            };
            let started = std::time::Instant::now();
            if workload == Workload::ToolTurnsWithCommands
                && let Some(JournalEvent::TurnStarted(_)) = commit.events.first()
            {
                let command =
                    json!({"kind":"start_turn","content":[{"kind":"text","text":"use one tool"}]});
                store
                    .bind_command(&mut commit, CommandId::new(), &command)
                    .unwrap();
            }
            store.append(commit, at()).unwrap();
            started.elapsed()
        })
        .collect()
}

#[test]
#[ignore = "opt-in storage scaling benchmark; performs 54,000 durable commits"]
fn session_storage_scaling_benchmark() {
    let mean_ms = |durations: &[std::time::Duration]| {
        durations.iter().sum::<std::time::Duration>().as_secs_f64() * 1e3 / durations.len() as f64
    };
    state::without_oracle(|| {
        for workload in [
            Workload::PlainTurns,
            Workload::ToolTurns,
            Workload::ToolTurnsWithCommands,
        ] {
            for commits in [1_000, 2_000, 5_000, 10_000] {
                let (_temp, root, mut store) = fresh();
                let id = store.snapshot().session_id().clone();
                let appends = append_workload(&root, &mut store, workload, commits);
                drop(store);
                let started = std::time::Instant::now();
                let reopened = JournalStore::open(root, &id).unwrap();
                let reopen = started.elapsed();
                assert_eq!(reopened.snapshot().sequence(), commits as u64);
                let per_turn = if workload == Workload::PlainTurns {
                    String::new()
                } else {
                    format!(
                        ", {:.3} ms/turn",
                        mean_ms(&appends) * TOOL_TURN_COMMITS as f64
                    )
                };
                eprintln!(
                    "{workload:?} N={commits}: append {:.3} ms/commit, last 10% {:.3} ms/commit{per_turn}; \
                     reopen {:.3} s, {:.3} ms/record",
                    mean_ms(&appends),
                    mean_ms(&appends[commits - commits / 10..]),
                    reopen.as_secs_f64(),
                    reopen.as_secs_f64() * 1e3 / commits as f64,
                );
            }
        }
    });
}

fn assert_shared_entries(before: &SessionSnapshot, after: &SessionSnapshot) {
    for (id, entry) in before.entries() {
        assert!(Arc::ptr_eq(
            before.entry_arc(id).unwrap(),
            after.entry_arc(id).unwrap(),
        ));
        assert!(std::ptr::eq(entry, after.entry(id).unwrap()));
    }
}

#[test]
fn snapshots_share_entries_across_append_and_reopen() {
    let (_temp, root, mut store) = fresh();
    for _ in 0..50 {
        store
            .append(finished_turn(&store.snapshot(), "shared history"), at())
            .unwrap();
    }
    let before = store.snapshot().clone();
    store
        .append(finished_turn(&before, "one more"), at())
        .unwrap();
    assert_eq!(before.entries().len(), 50);
    assert_eq!(store.snapshot().entries().len(), 51);
    assert_shared_entries(&before, &store.snapshot());
    let entry_ids: Vec<_> = before.entries().map(|(id, _)| id).collect();
    assert!(entry_ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(
        before.entries().rev().map(|(id, _)| id).collect::<Vec<_>>(),
        entry_ids.into_iter().rev().collect::<Vec<_>>()
    );
    let turn_ids: Vec<_> = before.turns().map(|(id, _)| id).collect();
    assert_eq!(before.turns().len(), 50);
    assert!(turn_ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(
        before.turns().rev().map(|(id, _)| id).collect::<Vec<_>>(),
        turn_ids.into_iter().rev().collect::<Vec<_>>()
    );
    assert_eq!(before.events().len(), 150);
    assert_eq!(
        before.events().rev().collect::<Vec<_>>(),
        before
            .events()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
    );

    let id = before.session_id().clone();
    drop(store);
    let mut store = JournalStore::open(root, &id).unwrap();
    let reopened = store.snapshot();
    store
        .append(finished_turn(&reopened, "after reopen"), at())
        .unwrap();
    assert_eq!(reopened.entries().len(), 51);
    assert_eq!(store.snapshot().entries().len(), 52);
    assert_shared_entries(&reopened, &store.snapshot());
}

#[test]
fn rejected_commit_preserves_published_snapshot_identity() {
    let (_temp, _root, mut store) = fresh();
    store
        .append(finished_turn(&store.snapshot(), "existing"), at())
        .unwrap();
    let before = store.snapshot();
    let mut commit = finished_turn(&before, "stale");
    commit.head.from = None;
    assert!(matches!(
        store.append(commit, at()),
        Err(StoreError::StaleHead)
    ));
    assert!(Arc::ptr_eq(&before.0, &store.snapshot().0));
    assert!(!store.is_poisoned());
}

#[test]
fn failed_durability_preserves_snapshot_and_entry_identity() {
    for point in [Point::Written, Point::Synced] {
        let (_temp, root, mut store) = fresh();
        store
            .append(finished_turn(&store.snapshot(), "existing"), at())
            .unwrap();
        let before = store.snapshot();
        root.io.test.lock().unwrap().fail = Some(point);
        assert!(matches!(
            store.append(finished_turn(&before, "unacknowledged"), at()),
            Err(StoreError::Io(_))
        ));
        assert!(Arc::ptr_eq(&before.0, &store.snapshot().0), "{point:?}");
        assert_shared_entries(&before, &store.snapshot());
        assert!(store.is_poisoned());
    }
}

#[test]
fn mixed_live_history_equals_replay_including_deduplicated_commands() {
    let (_temp, root, id) = fixture("branching");
    let mut store = JournalStore::open(root.clone(), &id).unwrap();
    let snapshot = store.snapshot();
    let branch_point = snapshot.ancestry(snapshot.head()).unwrap()[1]
        .entry
        .id
        .clone();
    store
        .append(navigate(&snapshot, Some(branch_point)), at())
        .unwrap();
    let command_id = CommandId::new();
    let mut commit = finished_turn(&store.snapshot(), "another sibling");
    store
        .bind_command(
            &mut commit,
            command_id.clone(),
            &json!({"kind":"start_turn","text":"another sibling"}),
        )
        .unwrap();
    let acknowledgement = store.append(commit.clone(), at()).unwrap();
    store
        .append(navigate(&store.snapshot(), None), at())
        .unwrap();
    let before_duplicate = store.snapshot();
    assert_eq!(store.append(commit.clone(), at()).unwrap(), acknowledgement);
    assert!(Arc::ptr_eq(&before_duplicate.0, &store.snapshot().0));
    let live = store.snapshot();
    drop(store);
    let mut reopened = JournalStore::open(root, &id).unwrap();
    let replayed = reopened.snapshot();
    assert_eq!(replayed.head(), live.head());
    assert_eq!(replayed.sequence(), live.sequence());
    assert_eq!(replayed.checksum(), live.checksum());
    let entry_records = |snapshot: &SessionSnapshot| {
        snapshot
            .entries()
            .map(|(id, stored)| {
                (
                    id.clone(),
                    stored.entry.clone(),
                    stored.sequence,
                    stored.record_checksum.clone(),
                    stored.entry_index,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(entry_records(&replayed), entry_records(&live));
    let turn_records = |snapshot: &SessionSnapshot| {
        snapshot
            .turns()
            .map(|(id, info)| {
                assert!(std::ptr::eq(snapshot.turn(id).unwrap(), info));
                (id.clone(), info.user_entry_id.clone(), info.outcome)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(turn_records(&replayed), turn_records(&live));
    assert_eq!(
        replayed.events().collect::<Vec<_>>(),
        live.events().collect::<Vec<_>>()
    );
    assert_eq!(
        replayed.command_receipt(&command_id).unwrap().commit,
        live.command_receipt(&command_id).unwrap().commit
    );
    assert_eq!(reopened.append(commit, at()).unwrap(), acknowledgement);
    assert!(Arc::ptr_eq(&replayed.0, &reopened.snapshot().0));
}

#[test]
fn j01_all_specification_fixtures_round_trip_and_replay() {
    for name in ["branching", "interrupted-execution", "compaction"] {
        let (_temp, root, id) = fixture(name);
        for line in source(name)
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
        {
            let (record, _) = codec::decode(line).unwrap();
            let (encoded, _) = codec::encode(&record).unwrap();
            assert_eq!(
                codec::decode(&encoded[..encoded.len() - 1]).unwrap().0,
                record
            );
        }
        let inspection = JournalStore::inspect(&root, &id).unwrap();
        assert_eq!(
            inspection.diagnostic, None,
            "{name}: {:?}",
            inspection.diagnostic
        );
        let snapshot = inspection.snapshot.unwrap();
        assert_eq!(
            snapshot.sequence() as usize + 1,
            records(&source(name)).len()
        );
        assert_eq!(
            snapshot.requires_recovery(),
            name == "interrupted-execution"
        );
        if name == "branching" {
            assert_eq!(snapshot.entries().len(), 4);
            assert_eq!(snapshot.ancestry(snapshot.head()).unwrap().len(), 3);
        }
        if name == "interrupted-execution" {
            assert_eq!(snapshot.unresolved_invocations().len(), 1);
            assert_eq!(snapshot.state(), RuntimeState::Executing);
        }
        if name == "compaction" {
            assert_eq!(snapshot.entries().len(), 7);
        }
    }
}

#[test]
fn j01_exact_raw_checksums_are_not_parse_reserialize_checksums() {
    let original = records(&source("branching")).remove(0);
    let compact = serde_json::to_string(&original).unwrap();
    let spaced = compact.replacen(':', ": ", 1);
    let valid = format!(
        "{{\"sha256\":\"{}\",\"record\":{spaced}}}",
        Digest::of(spaced.as_bytes())
    );
    assert!(codec::decode(valid.as_bytes()).is_ok());
    let tampered = format!(
        "{{\"record\":{spaced},\"sha256\":\"{}\"}}",
        Digest::of(compact.as_bytes())
    );
    assert!(matches!(
        codec::decode(tampered.as_bytes()),
        Err(StoreError::Invalid("checksum_mismatch"))
    ));
}

#[test]
fn j01_duplicate_keys_rejected_at_all_depths_including_escaped_arguments() {
    for raw in [
        r#"{"version":1,"version":1}"#,
        r#"{"arguments":{"path":"one","\u0070ath":"two"}}"#,
        r#"{"content":[{"metadata":{"nested":[{"x":1,"x":2}]}}]}"#,
    ] {
        assert!(codec::unique_json(raw.as_bytes()).is_err());
    }
    // A duplicate buried in model arguments still fails with a correct checksum.
    let mut record = records(&source("interrupted-execution"))[3].clone();
    record["payload"]["entries"][0]["payload"]["content"][0]["arguments"] = json!({"sentinel":1});
    let raw = serde_json::to_string(&record)
        .unwrap()
        .replace("\"sentinel\":1", "\"path\":\"a\",\"path\":\"b\"");
    let line = format!(
        "{{\"record\":{raw},\"sha256\":\"{}\"}}",
        Digest::of(raw.as_bytes())
    );
    assert!(matches!(
        codec::decode(line.as_bytes()),
        Err(StoreError::Invalid("malformed_or_duplicate_json"))
    ));
}

#[test]
fn j01_invalid_complete_records_fail_closed_without_losing_valid_prefix() {
    type Mutation = (&'static str, Box<dyn Fn(&mut Vec<Value>)>);
    let mutations: Vec<Mutation> = vec![
        (
            "duplicate_record_id",
            Box::new(|r| r[8]["record_id"] = r[7]["record_id"].clone()),
        ),
        (
            "unknown_parent",
            Box::new(|r| r[7]["payload"]["entries"][0]["parent_id"] = json!(EntryId::new())),
        ),
        (
            "stale_head",
            Box::new(|r| r[8]["payload"]["head"]["from"] = Value::Null),
        ),
        (
            "unknown_head",
            Box::new(|r| r[6]["payload"]["head"]["to"] = json!(EntryId::new())),
        ),
        (
            "bad_transition",
            Box::new(|r| r[2]["payload"]["events"][0]["from"] = json!("idle")),
        ),
        (
            "role_violation",
            Box::new(|r| {
                r[1]["payload"]["entries"][0]["payload"]["content"][0]["kind"] =
                    json!("reasoning_summary")
            }),
        ),
        ("version", Box::new(|r| r[8]["version"] = json!(2))),
        ("sequence", Box::new(|r| r[8]["seq"] = json!(100))),
        ("unknown_field", Box::new(|r| r[8]["extra"] = json!(true))),
        (
            "missing_nullable",
            Box::new(|r| {
                r[8]["payload"]
                    .as_object_mut()
                    .unwrap()
                    .remove("command_id");
            }),
        ),
        (
            "unsafe_integer",
            Box::new(|r| r[8]["seq"] = json!(MAX_COUNTER + 1)),
        ),
        ("timestamp", Box::new(|r| r[8]["at"] = json!("nonsenseZ"))),
        (
            "uuid_version",
            Box::new(|r| r[8]["record_id"] = json!("019a0000-0000-4000-8000-000000000999")),
        ),
        (
            "duplicate_entry",
            Box::new(|r| {
                r[7]["payload"]["entries"][0]["id"] = r[1]["payload"]["entries"][0]["id"].clone()
            }),
        ),
        (
            "duplicate_turn",
            Box::new(|r| {
                r[7]["payload"]["events"][0]["turn_id"] =
                    r[1]["payload"]["events"][0]["turn_id"].clone()
            }),
        ),
    ];
    for (name, mutate) in mutations {
        let (_temp, root) = environment();
        load_artifacts(&root);
        let mut values = records(&source("branching"));
        mutate(&mut values);
        let bytes = reframe(&values);
        let id = install(&root, &bytes);
        let inspection = JournalStore::inspect(&root, &id).unwrap();
        assert!(inspection.diagnostic.is_some(), "accepted {name}");
        assert!(inspection.valid_bytes < bytes.len() as u64, "{name}");
        assert!(
            matches!(
                JournalStore::open(root.clone(), &id),
                Err(StoreError::Journal(_))
            ),
            "{name}"
        );
        assert!(
            matches!(
                JournalStore::repair_tail(root.clone(), &id, &inspection, at()),
                Err(StoreError::NotRepairable)
            ),
            "{name}"
        );
        assert_eq!(
            fs::read(journal_path(&root, &id)).unwrap(),
            bytes,
            "mutated {name}"
        );
    }
}

#[test]
fn j01_tampered_final_and_middle_records_and_reordered_sequences_are_rejected() {
    for index in [2, 8] {
        let (_temp, root) = environment();
        load_artifacts(&root);
        let mut lines: Vec<_> = source("branching")
            .split_inclusive(|b| *b == b'\n')
            .map(|b| b.to_vec())
            .collect();
        let line = String::from_utf8(lines[index].clone())
            .unwrap()
            .replace("2026-10-16", "2026-10-15");
        lines[index] = line.into_bytes();
        let bytes = lines.concat();
        let id = install(&root, &bytes);
        let inspection = JournalStore::inspect(&root, &id).unwrap();
        assert_eq!(inspection.diagnostic.unwrap().code, "checksum_mismatch");
        assert_eq!(inspection.snapshot.unwrap().sequence(), index as u64 - 1);
    }
    let (_temp, root) = environment();
    load_artifacts(&root);
    let mut values = records(&source("branching"));
    values.swap(7, 8);
    let id = install(&root, &reframe(&values));
    assert!(
        JournalStore::inspect(&root, &id)
            .unwrap()
            .diagnostic
            .is_some()
    );
}

#[test]
fn j02_branches_navigation_and_snapshots_are_durable_and_immutable() {
    let (_temp, root, id) = fixture("branching");
    let mut store = JournalStore::open(root.clone(), &id).unwrap();
    let before = store.snapshot();
    let branch_point = before.ancestry(before.head()).unwrap()[1].entry.id.clone();
    store
        .append(navigate(&before, Some(branch_point)), at())
        .unwrap();
    store
        .append(start_turn(&store.snapshot(), "Third sibling"), at())
        .unwrap();
    fail_turn(&mut store);
    assert_eq!(before.entries().len(), 4);
    assert_eq!(before.sequence(), 8);
    assert_eq!(store.snapshot().entries().len(), 5);
    let expected = store.snapshot();
    drop(store);
    let store = JournalStore::open(root, &id).unwrap();
    assert_eq!(store.snapshot().head(), expected.head());
    assert_eq!(store.snapshot().entries().len(), 5);
    assert_eq!(
        store
            .snapshot()
            .ancestry(store.snapshot().head())
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn j02_second_writer_and_file_inspection_are_locked_until_owner_drops() {
    let (_temp, root, store) = fresh();
    let id = store.snapshot().session_id().clone();
    assert!(matches!(
        JournalStore::open(root.clone(), &id),
        Err(StoreError::Locked)
    ));
    assert!(matches!(
        JournalStore::inspect(&root, &id),
        Err(StoreError::Locked)
    ));
    assert!(matches!(
        JournalStore::create(
            root.clone(),
            id.clone(),
            store.snapshot().header().clone(),
            at()
        ),
        Err(StoreError::Io(_))
    ));
    drop(store);
    assert!(JournalStore::open(root, &id).is_ok());
}

#[test]
fn j02_rejected_transactions_do_not_mutate_file_or_poison_writer() {
    let (_temp, root, mut store) = fresh();
    let before = store.snapshot();
    let id = before.session_id().clone();
    let path = journal_path(&root, &id);
    let bytes = fs::read(&path).unwrap();
    let mut commit = start_turn(&before, "bad");
    commit.head.from = Some(EntryId::new());
    assert!(matches!(
        store.append(commit, at()),
        Err(StoreError::StaleHead)
    ));
    assert!(store.append(audit(&before, vec![]), at()).is_err());
    let mut commit = start_turn(&before, "bad");
    commit.entries[0].payload = EntryPayload::UserMessage(UserMessage {
        content: vec![ContentBlock::ReasoningSummary {
            text: "invalid role".into(),
        }],
    });
    assert!(store.append(commit, at()).is_err());
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(store.snapshot().sequence(), 0);
    assert!(!store.is_poisoned());
    store.append(start_turn(&before, "valid"), at()).unwrap();
}

#[test]
fn j02_failures_at_every_append_barrier_never_publish_unacknowledged_state() {
    for point in [
        Point::BeforeWrite,
        Point::PartialWrite,
        Point::Written,
        Point::Flushed,
        Point::Synced,
        Point::BeforeAck,
    ] {
        let (_temp, root, mut store) = fresh();
        let before = store.snapshot();
        let id = before.session_id().clone();
        let command_id = CommandId::new();
        let mut commit = start_turn(&before, "once");
        store
            .bind_command(
                &mut commit,
                command_id.clone(),
                &json!({"kind":"start_turn","text":"once"}),
            )
            .unwrap();
        root.io.test.lock().unwrap().fail = Some(point);
        assert!(
            matches!(store.append(commit.clone(), at()), Err(StoreError::Io(_))),
            "{point:?}"
        );
        assert_eq!(store.snapshot().sequence(), 0, "published at {point:?}");
        assert_eq!(before.entries().len(), 0);
        assert!(store.is_poisoned());
        assert!(matches!(
            store.append(commit.clone(), at()),
            Err(StoreError::Poisoned)
        ));
        drop(store);
        if point == Point::PartialWrite {
            let inspection = JournalStore::inspect(&root, &id).unwrap();
            assert_eq!(
                inspection.diagnostic.as_ref().unwrap().kind,
                DiagnosticKind::TornTail
            );
            assert_eq!(inspection.snapshot.as_ref().unwrap().sequence(), 0);
            let repaired = JournalStore::repair_tail(root.clone(), &id, &inspection, at()).unwrap();
            assert!(repaired.snapshot().command_receipt(&command_id).is_none());
        } else {
            let mut reopened = JournalStore::open(root.clone(), &id).unwrap();
            if point == Point::BeforeWrite {
                assert_eq!(reopened.snapshot().sequence(), 0);
            } else {
                assert_eq!(reopened.snapshot().sequence(), 1);
                let length = fs::metadata(journal_path(&root, &id)).unwrap().len();
                let ack = reopened.append(commit, at()).unwrap();
                assert_eq!(ack.sequence, 1);
                assert_eq!(
                    fs::metadata(journal_path(&root, &id)).unwrap().len(),
                    length
                );
                assert_eq!(
                    reopened
                        .snapshot()
                        .command_receipt(&command_id)
                        .unwrap()
                        .commit
                        .entries
                        .len(),
                    1
                );
            }
        }
    }
}

#[test]
fn j02_command_deduplication_retains_original_receipt_after_head_moves() {
    let (_temp, _root, mut store) = fresh();
    let id = CommandId::new();
    let mut commit = start_turn(&store.snapshot(), "original");
    store
        .bind_command(
            &mut commit,
            id.clone(),
            &json!({"kind":"start_turn","prompt":"original"}),
        )
        .unwrap();
    let ack = store.append(commit.clone(), at()).unwrap();
    fail_turn(&mut store);
    store
        .append(navigate(&store.snapshot(), None), at())
        .unwrap();
    let seq = store.snapshot().sequence();
    assert_eq!(store.append(commit.clone(), at()).unwrap(), ack);
    assert_eq!(store.snapshot().sequence(), seq);
    store
        .bind_command(
            &mut commit,
            id,
            &json!({"kind":"start_turn","prompt":"different"}),
        )
        .unwrap();
    assert!(matches!(
        store.append(commit, at()),
        Err(StoreError::StaleCommand)
    ));
}

#[test]
fn j03_only_unterminated_suffixes_are_quarantined_and_repaired() {
    for tail in [
        b"{\"record\":".to_vec(),
        b"{}".to_vec(),
        vec![b'x'; MAX_LINE_BYTES + 1],
    ] {
        let (_temp, root, store) = fresh();
        let id = store.snapshot().session_id().clone();
        drop(store);
        let path = journal_path(&root, &id);
        let prefix = fs::read(&path).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&tail)
            .unwrap();
        let inspection = JournalStore::inspect(&root, &id).unwrap();
        assert_eq!(inspection.valid_bytes, prefix.len() as u64);
        assert_eq!(
            inspection.diagnostic.as_ref().unwrap().offset,
            prefix.len() as u64
        );
        assert_eq!(
            inspection.diagnostic.as_ref().unwrap().kind,
            DiagnosticKind::TornTail
        );
        assert!(matches!(
            JournalStore::open(root.clone(), &id),
            Err(StoreError::Journal(_))
        ));
        let repaired = JournalStore::repair_tail(root.clone(), &id, &inspection, at()).unwrap();
        assert_eq!(repaired.snapshot().sequence(), 1);
        let snapshot = repaired.snapshot();
        let JournalEvent::TailRepaired(event) = &snapshot.events().last().unwrap().2 else {
            panic!("missing repair audit")
        };
        assert_eq!(event.original_size, (prefix.len() + tail.len()) as u64);
        assert_eq!(
            root.read_artifact(&event.quarantine_artifact, tail.len())
                .unwrap(),
            tail
        );
        assert!(fs::read(&path).unwrap().starts_with(&prefix));
        drop(repaired);
        assert!(
            JournalStore::inspect(&root, &id)
                .unwrap()
                .diagnostic
                .is_none()
        );
        assert!(JournalStore::open(root.clone(), &id).is_ok());
    }
}

#[test]
fn j03_valid_json_without_lf_is_uncommitted_and_complete_invalid_lines_are_not_repairable() {
    let (_temp, root, store) = fresh();
    let id = store.snapshot().session_id().clone();
    drop(store);
    let path = journal_path(&root, &id);
    let prefix = fs::read(&path).unwrap();
    let mut unterminated = prefix.clone();
    unterminated.pop();
    fs::write(&path, &unterminated).unwrap();
    let inspection = JournalStore::inspect(&root, &id).unwrap();
    assert_eq!(
        inspection.diagnostic.as_ref().unwrap().kind,
        DiagnosticKind::TornTail
    );
    assert!(inspection.snapshot.is_none());
    assert!(matches!(
        JournalStore::repair_tail(root.clone(), &id, &inspection, at()),
        Err(StoreError::NotRepairable)
    ));
    for invalid in [
        b"{}\n".to_vec(),
        b"\n".to_vec(),
        vec![b'x'; MAX_LINE_BYTES + 1]
            .into_iter()
            .chain(*b"\n")
            .collect(),
    ] {
        let mut bytes = prefix.clone();
        bytes.extend(invalid);
        fs::write(&path, &bytes).unwrap();
        let inspection = JournalStore::inspect(&root, &id).unwrap();
        assert_ne!(
            inspection.diagnostic.as_ref().unwrap().kind,
            DiagnosticKind::TornTail
        );
        assert!(matches!(
            JournalStore::repair_tail(root.clone(), &id, &inspection, at()),
            Err(StoreError::NotRepairable)
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn j03_repair_rechecks_file_fingerprint_and_length_under_lock() {
    let (_temp, root, store) = fresh();
    let id = store.snapshot().session_id().clone();
    drop(store);
    let path = journal_path(&root, &id);
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"tail")
        .unwrap();
    let expected = JournalStore::inspect(&root, &id).unwrap();
    let mut bytes = fs::read(&path).unwrap();
    *bytes.last_mut().unwrap() = b'X';
    fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        JournalStore::repair_tail(root.clone(), &id, &expected, at()),
        Err(StoreError::StaleRepair)
    ));
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn j03_repair_crashes_preserve_prefix_and_never_claim_uncommitted_audit() {
    for point in [
        Point::ArtifactWritten,
        Point::ArtifactSynced,
        Point::ArtifactRenamed,
        Point::ArtifactDirectorySynced,
        Point::TailTruncated,
        Point::TailSynced,
        Point::PartialWrite,
        Point::Written,
        Point::Flushed,
        Point::Synced,
        Point::BeforeAck,
    ] {
        let (_temp, root, store) = fresh();
        let id = store.snapshot().session_id().clone();
        drop(store);
        let path = journal_path(&root, &id);
        let prefix = fs::read(&path).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"uncommitted evidence")
            .unwrap();
        let expected = JournalStore::inspect(&root, &id).unwrap();
        root.io.test.lock().unwrap().fail = Some(point);
        assert!(
            JournalStore::repair_tail(root.clone(), &id, &expected, at()).is_err(),
            "{point:?}"
        );
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.starts_with(&prefix), "lost prefix {point:?}");
        let inspection = JournalStore::inspect(&root, &id).unwrap();
        if matches!(point, Point::TailTruncated | Point::TailSynced) {
            assert_eq!(bytes, prefix);
            assert_eq!(inspection.snapshot.unwrap().events().len(), 0);
        } else if matches!(
            point,
            Point::Written | Point::Flushed | Point::Synced | Point::BeforeAck
        ) {
            assert!(inspection.diagnostic.is_none());
            assert_eq!(inspection.snapshot.unwrap().sequence(), 1);
        } else {
            assert_eq!(
                inspection.diagnostic.unwrap().kind,
                DiagnosticKind::TornTail
            );
        }
    }
}

#[test]
fn j03_missing_tampered_and_length_mismatched_artifacts_block_load() {
    for mode in ["missing", "hash", "length"] {
        let (_temp, root, id) = fixture("branching");
        let records = records(&source("branching"));
        let digest = records[0]["payload"]["configuration"]["sha256"]
            .as_str()
            .unwrap();
        let path = root.path().join("artifacts").join(digest);
        let original = fs::read(&path).unwrap();
        match mode {
            "missing" => fs::remove_file(&path).unwrap(),
            "hash" => {
                let mut bytes = original;
                bytes[0] = b'X';
                fs::write(&path, bytes).unwrap();
            }
            _ => fs::write(&path, b"short").unwrap(),
        }
        let inspection = JournalStore::inspect(&root, &id).unwrap();
        assert_eq!(
            inspection.diagnostic.unwrap().kind,
            DiagnosticKind::Artifact,
            "{mode}"
        );
        assert!(inspection.snapshot.is_none());
        assert!(JournalStore::open(root, &id).is_err());
    }
}

#[test]
fn artifact_publication_is_synced_before_a_journal_can_reference_it() {
    for point in [
        Point::ArtifactWritten,
        Point::ArtifactSynced,
        Point::ArtifactRenamed,
        Point::ArtifactDirectorySynced,
    ] {
        let (_temp, root, mut store) = fresh();
        root.io.test.lock().unwrap().fail = Some(point);
        let bytes = b"unique output";
        let reference = ArtifactRef {
            sha256: Digest::of(bytes),
            bytes: bytes.len() as u64,
        };
        assert!(root.put_artifact(bytes).is_err());
        let mut commit = start_turn(&store.snapshot(), "media");
        let EntryPayload::UserMessage(message) = &mut commit.entries[0].payload else {
            unreachable!()
        };
        message.content = vec![ContentBlock::File {
            artifact: reference,
            mime_type: "text/plain".into(),
            name: "output".into(),
        }];
        if matches!(point, Point::ArtifactWritten | Point::ArtifactSynced) {
            assert!(matches!(
                store.append(commit, at()),
                Err(StoreError::Artifact)
            ));
            assert_eq!(store.snapshot().sequence(), 0);
        } else {
            // Existing published bytes are verified and resynchronized before use.
            store.append(commit, at()).unwrap();
        }
        assert!(
            fs::read_dir(root.path().join("artifacts"))
                .unwrap()
                .all(|e| !e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".tmp-"))
        );
    }
    let (_temp, root, _store) = fresh();
    let reference = root.put_artifact(b"same bytes").unwrap();
    assert_eq!(root.put_artifact(b"same bytes").unwrap(), reference);
    assert!(matches!(
        root.read_artifact(&reference, 1),
        Err(StoreError::Limit)
    ));
    fs::write(
        root.path()
            .join("artifacts")
            .join(reference.sha256.as_str()),
        b"wrong data",
    )
    .unwrap();
    assert!(matches!(
        root.put_artifact(b"same bytes"),
        Err(StoreError::Artifact)
    ));
}

#[test]
fn creation_and_reopen_failures_never_return_an_unsynced_store() {
    for point in [
        Point::HeaderWritten,
        Point::HeaderSynced,
        Point::SessionDirectorySynced,
    ] {
        let (_temp, root) = environment();
        let configuration = root.put_artifact(b"configuration").unwrap();
        let id = SessionId::new();
        let header = SessionHeader {
            workspace: "/workspace".into(),
            initial_model: ModelRef {
                provider_instance: "mock".into(),
                model: "scripted".into(),
            },
            policy_revision: "initial".into(),
            configuration,
        };
        root.io.test.lock().unwrap().fail = Some(point);
        assert!(JournalStore::create(root.clone(), id.clone(), header, at()).is_err());
        let store = JournalStore::open(root.clone(), &id).unwrap();
        assert_eq!(store.snapshot().sequence(), 0);
        drop(store);
        root.io.test.lock().unwrap().fail = Some(Point::ReopenSynced);
        assert!(JournalStore::open(root.clone(), &id).is_err());
        assert!(JournalStore::open(root.clone(), &id).is_ok());
    }
}

#[test]
fn recovery_blocks_navigation_and_never_reexecutes_uncertain_intents() {
    let (_temp, root, id) = fixture("interrupted-execution");
    let mut store = JournalStore::open(root.clone(), &id).unwrap();
    let snapshot = store.snapshot();
    assert!(store.append(navigate(&snapshot, None), at()).is_err());
    let invocation = snapshot.unresolved_invocations()[0].intent.clone();
    store
        .append(
            audit(
                &snapshot,
                vec![
                    JournalEvent::ExecutionUncertain(ExecutionUncertain {
                        turn_id: invocation.turn_id.clone(),
                        invocation_id: invocation.invocation_id.clone(),
                        reason: "process restarted".into(),
                    }),
                    JournalEvent::TurnFinished(TurnFinished {
                        turn_id: invocation.turn_id.clone(),
                        outcome: TurnOutcome::Interrupted,
                        code: Some("restart".into()),
                    }),
                ],
            ),
            at(),
        )
        .unwrap();
    assert_eq!(store.snapshot().state(), RuntimeState::Recovering);
    assert!(
        store
            .append(start_turn(&store.snapshot(), "cannot evade recovery"), at())
            .is_err()
    );
    let result_id = EntryId::new();
    let commit = Commit {
        command_id: None,
        command_fingerprint: None,
        head: HeadChange {
            from: store.snapshot().head().cloned(),
            to: Some(result_id.clone()),
            reason: HeadReason::Advance,
        },
        entries: vec![Entry {
            id: result_id.clone(),
            parent_id: store.snapshot().head().cloned(),
            turn_id: Some(invocation.turn_id.clone()),
            payload: EntryPayload::ToolResult(ToolResultEntry {
                result: ToolResult {
                    call: invocation.call.clone(),
                    invocation_id: Some(invocation.invocation_id.clone()),
                    status: ToolStatus::Uncertain,
                    content: vec![ContentBlock::Text {
                        text: "User accepts unknown outcome".into(),
                    }],
                    metadata: ToolMetadata {
                        exit_code: None,
                        duration_ms: None,
                        output_truncated: false,
                        provenance: Provenance::UserAcknowledgedUnknown,
                        artifacts: vec![],
                    },
                },
            }),
        }],
        events: vec![JournalEvent::RecoveryResolved(RecoveryResolved {
            turn_id: invocation.turn_id.clone(),
            invocation_id: invocation.invocation_id.clone(),
            result_entry_id: result_id,
            resolution: Resolution::AcknowledgedUnknown,
            note: "Checked workspace; outcome remains unknown".into(),
        })],
    };
    let mut without_event = commit.clone();
    without_event.events.clear();
    assert!(store.append(without_event, at()).is_err());
    store.append(commit, at()).unwrap();
    assert_eq!(store.snapshot().state(), RuntimeState::Idle);
    assert!(store.snapshot().unresolved_invocations().is_empty());
    assert!(!store.snapshot().requires_recovery());
    store
        .append(navigate(&store.snapshot(), None), at())
        .unwrap();
    drop(store);
    let store = JournalStore::open(root, &id).unwrap();
    assert_eq!(store.snapshot().entries().len(), 3);
    assert!(store.snapshot().unresolved_invocations().is_empty());
}

fn response_commit(
    snapshot: &SessionSnapshot,
    content: Vec<ContentBlock>,
    finish: FinishReason,
) -> Commit {
    let turn = snapshot.active_turn().unwrap().clone();
    let entry_id = EntryId::new();
    Commit {
        command_id: None,
        command_fingerprint: None,
        head: HeadChange {
            from: snapshot.head().cloned(),
            to: Some(entry_id.clone()),
            reason: HeadReason::Advance,
        },
        entries: vec![Entry {
            id: entry_id,
            parent_id: snapshot.head().cloned(),
            turn_id: Some(turn.clone()),
            payload: EntryPayload::AssistantResponse(AssistantResponse {
                response_id: ResponseId::new(),
                model: snapshot.header().initial_model.clone(),
                outcome: ResponseOutcome::Completed,
                finish_reason: Some(finish),
                content,
                usage: None,
                continuation: None,
                error: None,
            }),
        }],
        events: vec![JournalEvent::StateChanged(StateChanged {
            turn_id: turn,
            from: RuntimeState::Inferring,
            to: RuntimeState::Processing,
        })],
    }
}
fn response(store: &mut JournalStore, content: Vec<ContentBlock>, finish: FinishReason) -> EntryId {
    let snapshot = store.snapshot();
    store
        .append(
            transition(&snapshot, RuntimeState::Preparing, RuntimeState::Inferring),
            at(),
        )
        .unwrap();
    let commit = response_commit(&store.snapshot(), content, finish);
    let entry_id = commit.entries[0].id.clone();
    store.append(commit, at()).unwrap();
    entry_id
}

#[test]
fn completed_turn_accepts_refusal_but_not_output_limit_or_tool_calls() {
    for reason in [
        FinishReason::Refusal,
        FinishReason::OutputLimit,
        FinishReason::ToolCalls,
    ] {
        let (_temp, root, mut store) = fresh();
        let id = store.snapshot().session_id().clone();
        store
            .append(start_turn(&store.snapshot(), "question"), at())
            .unwrap();
        let content = if reason == FinishReason::ToolCalls {
            vec![tool_call(&CallId::new())]
        } else {
            vec![ContentBlock::Text {
                text: "answer".into(),
            }]
        };
        response(&mut store, content, reason);
        let result = store.append(finish(&store.snapshot(), TurnOutcome::Completed), at());
        if reason == FinishReason::Refusal {
            result.unwrap();
            let snapshot = store.snapshot();
            drop(store);
            let reopened = JournalStore::open(root, &id).unwrap();
            assert_eq!(reopened.snapshot().checksum(), snapshot.checksum());
            assert_eq!(reopened.snapshot().state(), RuntimeState::Idle);
        } else {
            assert!(matches!(
                result,
                Err(StoreError::Invalid(
                    "completion_without_stop" | "unfinished_not_started_proposals"
                ))
            ));
        }
    }
}
fn tool_result(
    snapshot: &SessionSnapshot,
    call: CallRef,
    invocation_id: Option<InvocationId>,
    status: ToolStatus,
    provenance: Provenance,
) -> Commit {
    let id = EntryId::new();
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
            turn_id: snapshot.active_turn().cloned(),
            payload: EntryPayload::ToolResult(ToolResultEntry {
                result: ToolResult {
                    call,
                    invocation_id,
                    status,
                    content: vec![],
                    metadata: ToolMetadata {
                        exit_code: None,
                        duration_ms: None,
                        output_truncated: false,
                        provenance,
                        artifacts: vec![],
                    },
                },
            }),
        }],
        events: vec![],
    }
}
fn runtime_result(snapshot: &SessionSnapshot, call: CallRef) -> Commit {
    tool_result(
        snapshot,
        call,
        None,
        ToolStatus::InvalidArguments,
        Provenance::Runtime,
    )
}

#[test]
fn call_references_results_and_navigation_preserve_complete_tool_groups() {
    let (_temp, _root, mut store) = fresh();
    store
        .append(start_turn(&store.snapshot(), "tools"), at())
        .unwrap();
    let first = CallId::new();
    let second = CallId::new();
    let response_id = response(
        &mut store,
        vec![
            ContentBlock::ToolCall {
                call_id: first.clone(),
                name: "read".into(),
                arguments: serde_json::Map::new(),
                provider_call_id: None,
            },
            ContentBlock::ToolCall {
                call_id: second.clone(),
                name: "read".into(),
                arguments: serde_json::Map::new(),
                provider_call_id: None,
            },
        ],
        FinishReason::ToolCalls,
    );
    let call = |id| CallRef {
        response_entry_id: response_id.clone(),
        call_id: id,
    };
    assert!(
        store
            .append(
                runtime_result(&store.snapshot(), call(second.clone())),
                at()
            )
            .is_err()
    );
    assert!(
        store
            .append(runtime_result(&store.snapshot(), call(CallId::new())), at())
            .is_err()
    );
    let turn = store.snapshot().active_turn().unwrap().clone();
    assert!(
        store
            .append(
                audit(
                    &store.snapshot(),
                    vec![JournalEvent::TurnFinished(TurnFinished {
                        turn_id: turn.clone(),
                        outcome: TurnOutcome::Interrupted,
                        code: None
                    })]
                ),
                at()
            )
            .is_err()
    );
    store
        .append(runtime_result(&store.snapshot(), call(first.clone())), at())
        .unwrap();
    assert!(
        store
            .append(runtime_result(&store.snapshot(), call(first)), at())
            .is_err()
    );
    store
        .append(runtime_result(&store.snapshot(), call(second)), at())
        .unwrap();
    fail_turn(&mut store);
    assert!(
        store
            .append(navigate(&store.snapshot(), Some(response_id)), at())
            .is_err()
    );
    store
        .append(navigate(&store.snapshot(), None), at())
        .unwrap();
}

#[test]
fn multimodal_content_and_provider_owned_state_survive_reopen() {
    let (_temp, root, mut store) = fresh();
    let id = store.snapshot().session_id().clone();
    let media = root.put_artifact(b"media fixture").unwrap();
    let native = root.put_artifact(b"opaque native fixture").unwrap();
    let mut commit = start_turn(&store.snapshot(), "multimodal");
    let EntryPayload::UserMessage(message) = &mut commit.entries[0].payload else {
        unreachable!()
    };
    message.content = vec![
        ContentBlock::Image {
            artifact: media.clone(),
            mime_type: "image/png".into(),
        },
        ContentBlock::Audio {
            artifact: media.clone(),
            mime_type: "audio/wav".into(),
        },
        ContentBlock::File {
            artifact: media,
            mime_type: "application/octet-stream".into(),
            name: "attachment".into(),
        },
    ];
    store.append(commit, at()).unwrap();
    let model = store.snapshot().header().initial_model.clone();
    response(
        &mut store,
        vec![
            ContentBlock::ReasoningSummary {
                text: "displayable summary".into(),
            },
            ContentBlock::ProviderState {
                owner: model,
                schema: "native-v1".into(),
                artifact: native,
            },
            ContentBlock::Text {
                text: "Done".into(),
            },
        ],
        FinishReason::Stop,
    );
    let snapshot = store.snapshot();
    store
        .append(
            audit(
                &snapshot,
                vec![JournalEvent::TurnFinished(TurnFinished {
                    turn_id: snapshot.active_turn().unwrap().clone(),
                    outcome: TurnOutcome::Completed,
                    code: None,
                })],
            ),
            at(),
        )
        .unwrap();
    drop(store);
    let store = JournalStore::open(root, &id).unwrap();
    assert_eq!(
        store
            .snapshot()
            .ancestry(store.snapshot().head())
            .unwrap()
            .len(),
        2
    );
    assert!(!store.snapshot().requires_recovery());
}

#[test]
fn private_storage_refuses_symlinks_hardlinks_and_unsafe_permissions() {
    let (_temp, root, store) = fresh();
    let id = store.snapshot().session_id().clone();
    drop(store);
    let path = journal_path(&root, &id);
    assert_eq!(fs::metadata(root.path()).unwrap().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    let alias = root.path().join("alias");
    fs::hard_link(&path, &alias).unwrap();
    assert!(matches!(
        JournalStore::open(root.clone(), &id),
        Err(StoreError::UnsafePath)
    ));
    fs::remove_file(alias).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        JournalStore::open(root.clone(), &id),
        Err(StoreError::UnsafePath)
    ));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let real = root.path().join("real");
    fs::rename(&path, &real).unwrap();
    symlink(&real, &path).unwrap();
    assert!(JournalStore::open(root.clone(), &id).is_err());
    let link = root.path().parent().unwrap().join("linked-state");
    symlink(root.path(), &link).unwrap();
    assert!(DataRoot::open(&link, false).is_err());
    let artifact = root.put_artifact(b"artifact").unwrap();
    let artifact_path = root.path().join("artifacts").join(artifact.sha256.as_str());
    let moved = root.path().join("moved");
    fs::rename(&artifact_path, &moved).unwrap();
    symlink(&moved, &artifact_path).unwrap();
    assert!(matches!(
        root.read_artifact(&artifact, 100),
        Err(StoreError::Artifact)
    ));
}

#[test]
fn invalid_private_root_and_workspace_overlap_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let unsafe_root = temp.path().join("unsafe");
    fs::create_dir(&unsafe_root).unwrap();
    fs::set_permissions(&unsafe_root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        DataRoot::open(&unsafe_root, true),
        Err(StoreError::UnsafePath)
    ));
    let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
    let header = SessionHeader {
        workspace: temp.path().to_str().unwrap().into(),
        initial_model: ModelRef {
            provider_instance: "mock".into(),
            model: "scripted".into(),
        },
        policy_revision: "initial".into(),
        configuration: root.put_artifact(b"configuration").unwrap(),
    };
    assert!(matches!(
        JournalStore::create(root, SessionId::new(), header, at()),
        Err(StoreError::Invalid("storage_overlaps_workspace"))
    ));
}

#[test]
fn inline_serialization_is_bounded_before_journal_mutation() {
    let (_temp, root, mut store) = fresh();
    let path = journal_path(&root, store.snapshot().session_id());
    let length = fs::metadata(&path).unwrap().len();
    let commit = start_turn(&store.snapshot(), &"x".repeat(MAX_LINE_BYTES));
    assert!(matches!(store.append(commit, at()), Err(StoreError::Limit)));
    assert_eq!(fs::metadata(path).unwrap().len(), length);
    assert_eq!(store.snapshot().sequence(), 0);
}

#[test]
fn j01_invalid_call_authorization_and_intent_bindings_fail_replay() {
    type Mutation = (&'static str, Box<dyn Fn(&mut Vec<Value>)>);
    let cases: Vec<Mutation> = vec![
        (
            "unknown_call",
            Box::new(|r| r[4]["payload"]["events"][1]["call"]["call_id"] = json!(CallId::new())),
        ),
        (
            "wrong_response",
            Box::new(|r| {
                r[4]["payload"]["events"][1]["call"]["response_entry_id"] =
                    r[1]["payload"]["entries"][0]["id"].clone()
            }),
        ),
        (
            "ineligible_proposal",
            Box::new(|r| {
                r[3]["payload"]["entries"][0]["payload"]["outcome"] = json!("cancelled");
                r[3]["payload"]["entries"][0]["payload"]["finish_reason"] = Value::Null;
            }),
        ),
        (
            "denied_intent",
            Box::new(|r| r[4]["payload"]["events"][1]["decision"] = json!("deny")),
        ),
        (
            "unknown_decision",
            Box::new(|r| r[4]["payload"]["events"][2]["decision_id"] = json!(DecisionId::new())),
        ),
        (
            "plan_mismatch",
            Box::new(|r| {
                r[4]["payload"]["events"][2]["plan_digest"] = json!(Digest::of(b"different plan"))
            }),
        ),
        (
            "policy_mismatch",
            Box::new(|r| r[4]["payload"]["events"][1]["policy_revision"] = json!("other")),
        ),
        (
            "intent_before_decision",
            Box::new(|r| r[4]["payload"]["events"].as_array_mut().unwrap().swap(1, 2)),
        ),
    ];
    for (name, mutate) in cases {
        let (_temp, root) = environment();
        load_artifacts(&root);
        let mut values = records(&source("interrupted-execution"));
        mutate(&mut values);
        let id = install(&root, &reframe(&values));
        assert!(
            JournalStore::inspect(&root, &id)
                .unwrap()
                .diagnostic
                .is_some(),
            "accepted {name}"
        );
    }
}

#[test]
fn navigation_restores_selected_model_but_never_rewinds_session_policy() {
    let (_temp, root, mut store) = fresh();
    let id = store.snapshot().session_id().clone();
    let initial = store.snapshot().header().initial_model.clone();
    let target = ModelRef {
        provider_instance: "another".into(),
        model: "other-model".into(),
    };
    let entry_id = EntryId::new();
    store
        .append(
            Commit {
                command_id: None,
                command_fingerprint: None,
                head: HeadChange {
                    from: None,
                    to: Some(entry_id.clone()),
                    reason: HeadReason::Advance,
                },
                entries: vec![Entry {
                    id: entry_id.clone(),
                    parent_id: None,
                    turn_id: None,
                    payload: EntryPayload::ModelChanged(ModelChanged {
                        model: target.clone(),
                        portability: Portability::Reject,
                    }),
                }],
                events: vec![],
            },
            at(),
        )
        .unwrap();
    assert_eq!(store.snapshot().model_at(Some(&entry_id)).unwrap(), &target);
    let configuration = root.put_artifact(b"new policy configuration").unwrap();
    store
        .append(
            audit(
                &store.snapshot(),
                vec![JournalEvent::ConfigurationChanged(ConfigurationChanged {
                    configuration: configuration.clone(),
                    policy_revision: "new-policy".into(),
                })],
            ),
            at(),
        )
        .unwrap();
    store
        .append(navigate(&store.snapshot(), None), at())
        .unwrap();
    drop(store);
    let store = JournalStore::open(root, &id).unwrap();
    let snapshot = store.snapshot();
    assert_eq!(snapshot.head(), None);
    assert_eq!(snapshot.entries().len(), 1);
    assert_eq!(snapshot.model_at(None).unwrap(), &initial);
    assert_eq!(snapshot.policy_revision(), "new-policy");
    assert_eq!(snapshot.configuration(), &configuration);
}

#[test]
fn successful_repair_orders_quarantine_before_truncate_and_audit() {
    let (_temp, root, store) = fresh();
    let id = store.snapshot().session_id().clone();
    drop(store);
    fs::OpenOptions::new()
        .append(true)
        .open(journal_path(&root, &id))
        .unwrap()
        .write_all(b"torn")
        .unwrap();
    let expected = JournalStore::inspect(&root, &id).unwrap();
    root.io.test.lock().unwrap().trace.clear();
    let _store = JournalStore::repair_tail(root.clone(), &id, &expected, at()).unwrap();
    let trace = &root.io.test.lock().unwrap().trace;
    let pos = |point| trace.iter().position(|v| *v == point).unwrap();
    assert!(pos(Point::ArtifactSynced) < pos(Point::ArtifactRenamed));
    assert!(pos(Point::ArtifactRenamed) < pos(Point::ArtifactDirectorySynced));
    assert!(pos(Point::ArtifactDirectorySynced) < pos(Point::TailTruncated));
    assert!(pos(Point::TailTruncated) < pos(Point::TailSynced));
    assert!(pos(Point::TailSynced) < pos(Point::Written));
    assert!(pos(Point::Written) < pos(Point::Flushed));
    assert!(pos(Point::Flushed) < pos(Point::Synced));
    assert!(pos(Point::Synced) < pos(Point::BeforeAck));
}

#[test]
fn artifact_limits_reject_unvalidated_public_input_without_overflow() {
    let (_temp, root) = environment();
    let invalid = ArtifactRef {
        sha256: Digest::of(b""),
        bytes: u64::MAX,
    };
    assert!(matches!(
        root.read_artifact(&invalid, usize::MAX),
        Err(StoreError::Limit)
    ));
    assert!(matches!(
        root.verify_artifact(&invalid),
        Err(StoreError::Limit)
    ));
}

#[test]
fn one_inference_cannot_commit_two_response_snapshots() {
    let (_temp, _root, mut store) = fresh();
    store
        .append(start_turn(&store.snapshot(), "one inference"), at())
        .unwrap();
    let turn = store.snapshot().active_turn().unwrap().clone();
    store
        .append(
            audit(
                &store.snapshot(),
                vec![JournalEvent::StateChanged(StateChanged {
                    turn_id: turn.clone(),
                    from: RuntimeState::Preparing,
                    to: RuntimeState::Inferring,
                })],
            ),
            at(),
        )
        .unwrap();
    let make = |snapshot: SessionSnapshot| {
        let id = EntryId::new();
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
                payload: EntryPayload::AssistantResponse(AssistantResponse {
                    response_id: ResponseId::new(),
                    model: snapshot.header().initial_model.clone(),
                    outcome: ResponseOutcome::Completed,
                    finish_reason: Some(FinishReason::Stop),
                    content: vec![ContentBlock::Text {
                        text: "one snapshot".into(),
                    }],
                    usage: None,
                    continuation: None,
                    error: None,
                }),
            }],
            events: vec![],
        }
    };
    store.append(make(store.snapshot()), at()).unwrap();
    assert!(matches!(
        store.append(make(store.snapshot()), at()),
        Err(StoreError::Invalid("duplicate_inference_response"))
    ));
    store
        .append(
            audit(
                &store.snapshot(),
                vec![JournalEvent::StateChanged(StateChanged {
                    turn_id: turn,
                    from: RuntimeState::Inferring,
                    to: RuntimeState::Processing,
                })],
            ),
            at(),
        )
        .unwrap();
}

#[test]
fn failed_turns_do_not_displace_the_two_recent_completed_turns_in_compaction() {
    let (_temp, root, session_id) = fixture("compaction");
    // Keep all completed turns, remove the original summary, then add failed
    // turns. Those failures must not make a recent completed turn compressible.
    let original = source("compaction");
    let prefix: Vec<_> = original
        .split_inclusive(|b| *b == b'\n')
        .take(10)
        .flatten()
        .copied()
        .collect();
    fs::write(journal_path(&root, &session_id), prefix).unwrap();
    let mut store = JournalStore::open(root.clone(), &session_id).unwrap();
    for _ in 0..2 {
        store
            .append(start_turn(&store.snapshot(), "failed later turn"), at())
            .unwrap();
        fail_turn(&mut store);
    }
    let snapshot = store.snapshot();
    let ancestry = snapshot.ancestry(snapshot.head()).unwrap();
    let covered: Vec<_> = ancestry[2..4].iter().map(|e| e.entry.id.clone()).collect();
    let manifest = root
        .put_artifact(b"manifest would be checked after ancestry eligibility")
        .unwrap();
    let entry_id = EntryId::new();
    let commit = Commit {
        command_id: None,
        command_fingerprint: None,
        head: HeadChange {
            from: snapshot.head().cloned(),
            to: Some(entry_id.clone()),
            reason: HeadReason::Advance,
        },
        entries: vec![Entry {
            id: entry_id,
            parent_id: snapshot.head().cloned(),
            turn_id: None,
            payload: EntryPayload::Compaction(Compaction {
                strategy: "rolling".into(),
                strategy_version: "1".into(),
                covered_entry_ids: covered,
                source_fingerprint: manifest.sha256.clone(),
                source_manifest: manifest,
                supersedes: vec![],
                summary: "must retain this completed turn".into(),
                model: snapshot.header().initial_model.clone(),
                configuration_fingerprint: snapshot.configuration().sha256.clone(),
            }),
        }],
        events: vec![],
    };
    assert!(matches!(
        store.append(commit, at()),
        Err(StoreError::Invalid("compaction_unfinished_or_recent_turn"))
    ));
}

fn tool_call(call_id: &CallId) -> ContentBlock {
    ContentBlock::ToolCall {
        call_id: call_id.clone(),
        name: "read".into(),
        arguments: serde_json::Map::new(),
        provider_call_id: None,
    }
}
fn finish(snapshot: &SessionSnapshot, outcome: TurnOutcome) -> Commit {
    audit(
        snapshot,
        vec![JournalEvent::TurnFinished(TurnFinished {
            turn_id: snapshot.active_turn().unwrap().clone(),
            outcome,
            code: None,
        })],
    )
}

/// Append every commit of a fixture to a new live store, with the fixture's
/// header and timestamps, as if it were being written for the first time.
/// New records have new checksums, so compaction manifests are rebuilt.
fn append_fixture(name: &str) -> (tempfile::TempDir, Arc<DataRoot>, JournalStore) {
    let (temp, root) = environment();
    load_artifacts(&root);
    let mut store = None;
    for record in records(&source(name)) {
        let at: Timestamp = serde_json::from_value(record["at"].clone()).unwrap();
        match serde_json::from_value(record["payload"].clone()).unwrap() {
            RecordPayload::SessionCreated(header) => {
                store =
                    Some(JournalStore::create(root.clone(), SessionId::new(), header, at).unwrap());
            }
            RecordPayload::Commit(mut commit) => {
                let store = store.as_mut().unwrap();
                let snapshot = store.snapshot();
                for entry in &mut commit.entries {
                    if let EntryPayload::Compaction(compaction) = &mut entry.payload {
                        let references = |ids: &[EntryId]| -> Vec<Value> {
                            ids.iter()
                                .map(|id| {
                                    let stored = snapshot.entry(id).unwrap();
                                    json!({
                                        "entry_id": id,
                                        "record_checksum": stored.record_checksum,
                                        "entry_index": stored.entry_index,
                                    })
                                })
                                .collect()
                        };
                        let manifest = json!({
                            "covered": references(&compaction.covered_entry_ids),
                            "supersedes": references(&compaction.supersedes),
                            "configuration_fingerprint": compaction.configuration_fingerprint,
                        });
                        compaction.source_manifest = root
                            .put_artifact(&serde_json::to_vec(&manifest).unwrap())
                            .unwrap();
                        compaction.source_fingerprint = compaction.source_manifest.sha256.clone();
                    }
                }
                store.append(commit, at).unwrap();
            }
        }
    }
    (temp, root, store.unwrap())
}

#[test]
fn reopened_branch_facts_and_indexes_equal_the_live_snapshot() {
    let mut cases = vec![];
    for name in ["branching", "compaction", "interrupted-execution"] {
        cases.push((name.to_owned(), append_fixture(name)));
    }
    for workload in [Workload::PlainTurns, Workload::ToolTurns] {
        for commits in [1, 6, 7, 50] {
            let (temp, root, mut store) = fresh();
            append_workload(&root, &mut store, workload, commits);
            cases.push((format!("{workload:?} N={commits}"), (temp, root, store)));
        }
    }
    for (name, (_temp, root, store)) in cases {
        let live = store.snapshot();
        drop(store);
        let reopened = JournalStore::open(root, live.session_id())
            .unwrap()
            .snapshot();
        assert_eq!(reopened.sequence(), live.sequence(), "{name}");
        assert_eq!(reopened.0.derived(), live.0.derived(), "{name}");
        assert_eq!(
            reopened.entries().len(),
            live.0.derived().branch.len(),
            "{name}"
        );
    }
}

#[test]
fn branch_facts_follow_the_queried_branch_across_navigation() {
    let (_temp, _root, mut store) = fresh();
    let initial = store.snapshot().header().initial_model.clone();
    let other = ModelRef {
        provider_instance: "another".into(),
        model: "other-model".into(),
    };
    // Branch one: a model change, then an exchange with two calls.
    let change_id = EntryId::new();
    store
        .append(
            Commit {
                head: HeadChange {
                    from: None,
                    to: Some(change_id.clone()),
                    reason: HeadReason::Advance,
                },
                entries: vec![Entry {
                    id: change_id.clone(),
                    parent_id: None,
                    turn_id: None,
                    payload: EntryPayload::ModelChanged(ModelChanged {
                        model: other.clone(),
                        portability: Portability::Reject,
                    }),
                }],
                ..audit(&store.snapshot(), vec![])
            },
            at(),
        )
        .unwrap();
    store
        .append(start_turn(&store.snapshot(), "branch one"), at())
        .unwrap();
    store
        .append(
            transition(
                &store.snapshot(),
                RuntimeState::Preparing,
                RuntimeState::Inferring,
            ),
            at(),
        )
        .unwrap();
    let (first, second) = (CallId::new(), CallId::new());
    let mut commit = response_commit(
        &store.snapshot(),
        vec![tool_call(&first), tool_call(&second)],
        FinishReason::ToolCalls,
    );
    let EntryPayload::AssistantResponse(payload) = &mut commit.entries[0].payload else {
        unreachable!()
    };
    payload.model = other.clone();
    let one = commit.entries[0].id.clone();
    store.append(commit, at()).unwrap();
    let call = |response: &EntryId, call_id: &CallId| CallRef {
        response_entry_id: response.clone(),
        call_id: call_id.clone(),
    };
    let commit = runtime_result(&store.snapshot(), call(&one, &first));
    let one_result = commit.entries[0].id.clone();
    store.append(commit, at()).unwrap();
    store
        .append(runtime_result(&store.snapshot(), call(&one, &second)), at())
        .unwrap();
    fail_turn(&mut store);
    let one_end = store.snapshot().head().cloned();

    // Branch two from the root: no model change, one call left open.
    store
        .append(navigate(&store.snapshot(), None), at())
        .unwrap();
    store
        .append(start_turn(&store.snapshot(), "branch two"), at())
        .unwrap();
    let third = CallId::new();
    let two = response(&mut store, vec![tool_call(&third)], FinishReason::ToolCalls);

    // The head is inside branch two's exchange; queries on branch one's
    // entries still describe branch one.
    let snapshot = store.snapshot();
    let state = &snapshot.0;
    assert_eq!(snapshot.head(), Some(&two));
    assert_eq!(state.pending(Some(&two)).unwrap(), [call(&two, &third)]);
    assert_eq!(snapshot.model_at(Some(&two)).unwrap(), &initial);
    assert_eq!(
        state.pending(Some(&one)).unwrap(),
        [call(&one, &first), call(&one, &second)]
    );
    assert_eq!(
        state.pending(Some(&one_result)).unwrap(),
        [call(&one, &second)]
    );
    assert!(state.pending(one_end.as_ref()).unwrap().is_empty());
    for id in [&change_id, &one, &one_result] {
        assert_eq!(snapshot.model_at(Some(id)).unwrap(), &other);
    }
    assert_eq!(snapshot.model_at(None).unwrap(), &initial);
    assert!(state.pending(None).unwrap().is_empty());
    assert!(matches!(
        snapshot.model_at(Some(&EntryId::new())),
        Err(StoreError::Invalid("unknown_head_or_parent"))
    ));
    // Branch two's open call is next; branch one's calls are not.
    assert!(matches!(
        store.append(runtime_result(&snapshot, call(&one, &first)), at()),
        Err(StoreError::Invalid(_))
    ));
    store
        .append(runtime_result(&snapshot, call(&two, &third)), at())
        .unwrap();
    fail_turn(&mut store);

    // Navigating back selects branch one's model and an empty exchange.
    store
        .append(navigate(&store.snapshot(), one_end.clone()), at())
        .unwrap();
    let snapshot = store.snapshot();
    assert_eq!(snapshot.model_at(snapshot.head()).unwrap(), &other);
    assert!(snapshot.0.pending(snapshot.head()).unwrap().is_empty());

    // Heads inside either exchange remain unselectable.
    for head in [&one, &one_result] {
        assert!(matches!(
            store.append(navigate(&store.snapshot(), Some(head.clone())), at()),
            Err(StoreError::Invalid("head_inside_tool_exchange"))
        ));
    }
}

#[test]
fn rejected_commits_publish_no_branch_facts_or_index_entries() {
    let (_temp, root, mut store) = fresh();
    let mut tools = ToolTurns {
        plan: root.put_artifact(br#"{"plan":"read"}"#).unwrap(),
        step: 0,
        call: None,
        invocation: None,
    };
    for _ in 0..6 {
        let commit = tools.next(&store.snapshot());
        store.append(commit, at()).unwrap();
    }
    let before = store.snapshot();
    let call = tools.call.clone().unwrap();
    let invocation = tools.invocation.clone().unwrap();
    assert!(before.0.derived().unresolved.contains(&invocation));

    // The first entry's facts are staged before the second breaks the
    // result order, so the whole commit must be dropped.
    let mut commit = tools.next(&before);
    let first = commit.entries[0].clone();
    let mut duplicate = first.clone();
    duplicate.id = EntryId::new();
    duplicate.parent_id = Some(first.id.clone());
    commit.entries.push(duplicate.clone());
    commit.head.to = Some(duplicate.id.clone());
    assert!(matches!(
        store.append(commit, at()),
        Err(StoreError::Invalid("orphan_duplicate_or_reordered_result"))
    ));
    // A valid result whose commit fails on a later event leaves the
    // invocation unresolved.
    let mut commit = tools.tool_result_commit(&before);
    commit.events.push(
        transition(&before, RuntimeState::Executing, RuntimeState::Idle)
            .events
            .remove(0),
    );
    assert!(matches!(
        store.append(commit, at()),
        Err(StoreError::Invalid("illegal_state_transition"))
    ));
    let after = store.snapshot();
    assert!(Arc::ptr_eq(&before.0, &after.0));
    assert!(!after.0.derived().branch.contains_key(&first.id));
    assert!(!after.0.derived().branch.contains_key(&duplicate.id));
    assert!(after.0.derived().unresolved.contains(&invocation));
    assert_eq!(after.0.derived(), before.0.derived());
    assert_eq!(after.0.pending(after.head()).unwrap(), [call]);
    assert!(!store.is_poisoned());
}

#[test]
fn recovery_keeps_invocations_unresolved_until_explicitly_resolved() {
    let (_temp, root, id) = fixture("interrupted-execution");
    let mut store = JournalStore::open(root, &id).unwrap();
    let invocation = store.snapshot().unresolved_invocations()[0].intent.clone();
    let unresolved = |store: &JournalStore| {
        store
            .snapshot()
            .0
            .derived()
            .unresolved
            .contains(&invocation.invocation_id)
    };
    assert!(unresolved(&store));
    assert!(matches!(
        store.append(start_turn(&store.snapshot(), "blocked"), at()),
        Err(StoreError::Invalid("recovery_required"))
    ));
    store
        .append(
            audit(
                &store.snapshot(),
                vec![
                    JournalEvent::ExecutionUncertain(ExecutionUncertain {
                        turn_id: invocation.turn_id.clone(),
                        invocation_id: invocation.invocation_id.clone(),
                        reason: "process restarted".into(),
                    }),
                    JournalEvent::TurnFinished(TurnFinished {
                        turn_id: invocation.turn_id.clone(),
                        outcome: TurnOutcome::Interrupted,
                        code: Some("restart".into()),
                    }),
                ],
            ),
            at(),
        )
        .unwrap();
    assert!(unresolved(&store));
    assert!(store.snapshot().requires_recovery());
    let snapshot = store.snapshot();
    let mut commit = tool_result(
        &snapshot,
        invocation.call.clone(),
        Some(invocation.invocation_id.clone()),
        ToolStatus::Uncertain,
        Provenance::UserAcknowledgedUnknown,
    );
    commit.entries[0].turn_id = Some(invocation.turn_id.clone());
    commit
        .events
        .push(JournalEvent::RecoveryResolved(RecoveryResolved {
            turn_id: invocation.turn_id.clone(),
            invocation_id: invocation.invocation_id.clone(),
            result_entry_id: commit.entries[0].id.clone(),
            resolution: Resolution::AcknowledgedUnknown,
            note: "outcome remains unknown".into(),
        }));
    let mut without_event = commit.clone();
    without_event.events.clear();
    assert!(matches!(
        store.append(without_event, at()),
        Err(StoreError::Invalid("recovery_result_without_resolution"))
    ));
    assert!(unresolved(&store));
    store.append(commit, at()).unwrap();
    assert!(!unresolved(&store));
    assert!(store.snapshot().0.derived().unresolved.is_empty());
    assert_eq!(store.snapshot().state(), RuntimeState::Idle);
    store
        .append(start_turn(&store.snapshot(), "recovered"), at())
        .unwrap();
}

#[test]
fn turn_end_drains_unused_approvals_and_decisions() {
    let (_temp, root, mut store) = fresh();
    let plan = root.put_artifact(br#"{"plan":"read"}"#).unwrap();
    store
        .append(start_turn(&store.snapshot(), "two calls"), at())
        .unwrap();
    let (first, second) = (CallId::new(), CallId::new());
    let response_id = response(
        &mut store,
        vec![tool_call(&first), tool_call(&second)],
        FinishReason::ToolCalls,
    );
    let call = |call_id: &CallId| CallRef {
        response_entry_id: response_id.clone(),
        call_id: call_id.clone(),
    };
    store
        .append(
            transition(
                &store.snapshot(),
                RuntimeState::Processing,
                RuntimeState::Authorizing,
            ),
            at(),
        )
        .unwrap();
    let turn_id = store.snapshot().active_turn().unwrap().clone();
    // The first call's approval is requested but never decided.
    store
        .append(
            audit(
                &store.snapshot(),
                vec![JournalEvent::ApprovalRequested(ApprovalRequested {
                    turn_id: turn_id.clone(),
                    approval_id: ApprovalId::new(),
                    call: call(&first),
                    plan_digest: plan.sha256.clone(),
                    plan: plan.clone(),
                    policy_revision: "initial".into(),
                    expires_at: "2026-10-17T13:00:00Z".parse().unwrap(),
                })],
            ),
            at(),
        )
        .unwrap();
    assert_eq!(store.snapshot().0.derived().unused_approvals.len(), 1);
    let mut cancelled = runtime_result(&store.snapshot(), call(&first));
    let EntryPayload::ToolResult(result) = &mut cancelled.entries[0].payload else {
        unreachable!()
    };
    result.result.status = ToolStatus::Cancelled;
    store.append(cancelled, at()).unwrap();
    // The second call is denied, so its decision is never used by an intent.
    store
        .append(
            audit(
                &store.snapshot(),
                vec![JournalEvent::AuthorizationDecided(AuthorizationDecided {
                    turn_id,
                    decision_id: DecisionId::new(),
                    call: call(&second),
                    approval_id: None,
                    plan_digest: plan.sha256.clone(),
                    plan,
                    policy_revision: "initial".into(),
                    decision: Decision::Deny,
                    reason: "not allowed".into(),
                })],
            ),
            at(),
        )
        .unwrap();
    let mut denied = runtime_result(&store.snapshot(), call(&second));
    let EntryPayload::ToolResult(result) = &mut denied.entries[0].payload else {
        unreachable!()
    };
    result.result.status = ToolStatus::Denied;
    store.append(denied, at()).unwrap();
    let derived = store.snapshot().0.derived();
    assert_eq!(derived.unused_approvals.len(), 1);
    assert_eq!(derived.unused_decisions.len(), 1);
    store
        .append(finish(&store.snapshot(), TurnOutcome::Failed), at())
        .unwrap();
    let derived = store.snapshot().0.derived();
    assert!(derived.unused_approvals.is_empty());
    assert!(derived.unused_decisions.is_empty());
    assert_eq!(store.snapshot().state(), RuntimeState::Idle);
}
