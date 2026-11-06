//! Session export, journal-only deletion and explicit artifact collection.
//!
//! Every test uses real storage in a temporary directory and the built binary
//! for the command surface, so exit statuses and stderr codes are checked as a
//! user would see them. The fixture journals are built through the public
//! storage API with fixed identities, which keeps the Markdown golden exact.
use harness_core::session::*;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, SystemTime},
};

/// These tests fork the CLI binary while other test threads hold journal locks.
/// A forked child briefly shares those descriptors until `exec` applies
/// `CLOEXEC`, so an unrelated non-blocking `flock` can see a transient conflict
/// on a machine with many concurrent forks. Taking turns keeps every lock
/// assertion exact; nothing about the assertions is relaxed.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn at() -> Timestamp {
    "2026-11-03T12:00:00Z".parse().unwrap()
}

fn uid(n: u128) -> String {
    format!("019a0000-0000-7000-8000-{n:012x}")
}

fn model() -> ModelRef {
    ModelRef {
        provider_instance: "mock".into(),
        model: "scripted".into(),
    }
}

/// Run `harness sessions ...` and return the whole process result.
fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .arg("sessions")
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
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

fn append(store: &mut JournalStore, commit: Commit) {
    store.append(commit, at()).unwrap();
}

/// Begin a turn with one user entry and return its identity.
fn start(
    store: &mut JournalStore,
    id: u128,
    turn: u128,
    parent: Option<EntryId>,
    content: Vec<ContentBlock>,
) -> EntryId {
    let snapshot = store.snapshot();
    let entry = uid(id).parse::<EntryId>().unwrap();
    let turn_id = uid(turn).parse::<TurnId>().unwrap();
    append(
        store,
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
                parent_id: parent.or_else(|| snapshot.head().cloned()),
                turn_id: Some(turn_id.clone()),
                payload: EntryPayload::UserMessage(UserMessage { content }),
            }],
            events: vec![
                JournalEvent::TurnStarted(TurnStarted {
                    turn_id: turn_id.clone(),
                    user_entry_id: entry.clone(),
                    configuration: snapshot.configuration().clone(),
                }),
                JournalEvent::StateChanged(StateChanged {
                    turn_id,
                    from: RuntimeState::Idle,
                    to: RuntimeState::Preparing,
                }),
            ],
        },
    );
    entry
}

fn response(store: &mut JournalStore, id: u128, content: Vec<ContentBlock>, finish: FinishReason) {
    let snapshot = store.snapshot();
    if snapshot.state() == RuntimeState::Continuing {
        append(
            store,
            transition(&snapshot, RuntimeState::Continuing, RuntimeState::Preparing),
        );
    }
    let snapshot = store.snapshot();
    append(
        store,
        transition(&snapshot, RuntimeState::Preparing, RuntimeState::Inferring),
    );
    let snapshot = store.snapshot();
    let entry = uid(id).parse::<EntryId>().unwrap();
    let turn = snapshot.active_turn().unwrap().clone();
    append(
        store,
        Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: snapshot.head().cloned(),
                to: Some(entry.clone()),
                reason: HeadReason::Advance,
            },
            entries: vec![Entry {
                id: entry,
                parent_id: snapshot.head().cloned(),
                turn_id: Some(turn.clone()),
                payload: EntryPayload::AssistantResponse(AssistantResponse {
                    response_id: uid(id + 5000).parse::<ResponseId>().unwrap(),
                    model: model(),
                    outcome: ResponseOutcome::Completed,
                    finish_reason: Some(finish),
                    content,
                    usage: None,
                    continuation: None,
                    error: None,
                    projection_fingerprint: None,
                }),
            }],
            events: vec![JournalEvent::StateChanged(StateChanged {
                turn_id: turn,
                from: RuntimeState::Inferring,
                to: RuntimeState::Processing,
            })],
        },
    );
}

/// Authorize and dispatch one call: the exact shape a trusted read result
/// needs before its executor result is accepted.
fn authorize(
    store: &mut JournalStore,
    plan: &ArtifactRef,
    response_entry: u128,
    call_id: u128,
    decision: u128,
    invocation: u128,
) {
    let snapshot = store.snapshot();
    append(
        store,
        transition(
            &snapshot,
            RuntimeState::Processing,
            RuntimeState::Authorizing,
        ),
    );
    let snapshot = store.snapshot();
    let turn = snapshot.active_turn().unwrap().clone();
    let reference = CallRef {
        response_entry_id: uid(response_entry).parse::<EntryId>().unwrap(),
        call_id: uid(call_id).parse::<CallId>().unwrap(),
    };
    let decision_id = uid(decision).parse::<DecisionId>().unwrap();
    append(
        store,
        audit(
            &snapshot,
            vec![
                JournalEvent::AuthorizationDecided(AuthorizationDecided {
                    turn_id: turn.clone(),
                    decision_id: decision_id.clone(),
                    call: reference.clone(),
                    approval_id: None,
                    plan_digest: plan.sha256.clone(),
                    plan: plan.clone(),
                    policy_revision: snapshot.policy_revision().into(),
                    decision: Decision::Allow,
                    reason: "fixture read".into(),
                }),
                JournalEvent::ExecutionIntent(ExecutionIntent {
                    turn_id: turn,
                    invocation_id: uid(invocation).parse::<InvocationId>().unwrap(),
                    call: reference,
                    decision_id,
                    plan_digest: plan.sha256.clone(),
                    backend: ExecutionBackend::TrustedFs,
                    timeout_ms: 30_000,
                    effects: EffectSet::new([EffectClass::Read]),
                    file_image: None,
                }),
            ],
        ),
    );
    let snapshot = store.snapshot();
    append(
        store,
        transition(
            &snapshot,
            RuntimeState::Authorizing,
            RuntimeState::Executing,
        ),
    );
}

/// What one executor result reports, so the call site reads as a statement.
struct ResultSpec<'a> {
    entry: u128,
    response_entry: u128,
    call: u128,
    invocation: u128,
    status: ToolStatus,
    code: Option<&'a str>,
    content: Vec<ContentBlock>,
    exit_code: Option<i32>,
    artifacts: Vec<ArtifactRef>,
}

fn tool_result(store: &mut JournalStore, spec: ResultSpec<'_>) {
    let ResultSpec {
        entry: id,
        response_entry,
        call,
        invocation,
        status,
        code,
        content,
        exit_code,
        artifacts,
    } = spec;
    let snapshot = store.snapshot();
    let entry = uid(id).parse::<EntryId>().unwrap();
    append(
        store,
        Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: snapshot.head().cloned(),
                to: Some(entry.clone()),
                reason: HeadReason::Advance,
            },
            entries: vec![Entry {
                id: entry,
                parent_id: snapshot.head().cloned(),
                turn_id: snapshot.active_turn().cloned(),
                payload: EntryPayload::ToolResult(ToolResultEntry {
                    result: ToolResult {
                        call: CallRef {
                            response_entry_id: uid(response_entry).parse::<EntryId>().unwrap(),
                            call_id: uid(call).parse::<CallId>().unwrap(),
                        },
                        invocation_id: Some(uid(invocation).parse::<InvocationId>().unwrap()),
                        status,
                        content,
                        metadata: ToolMetadata {
                            exit_code,
                            duration_ms: Some(12),
                            output_truncated: false,
                            provenance: Provenance::Executor,
                            artifacts,
                            code: code.map(str::to_owned),
                            file_effect: None,
                            file_paths: Vec::new(),
                        },
                    },
                }),
            }],
            events: vec![],
        },
    );
}

fn finish(store: &mut JournalStore, outcome: TurnOutcome) {
    let snapshot = store.snapshot();
    append(
        store,
        audit(
            &snapshot,
            vec![JournalEvent::TurnFinished(TurnFinished {
                turn_id: snapshot.active_turn().unwrap().clone(),
                outcome,
                code: None,
            })],
        ),
    );
}

fn call(call_id: u128, name: &str, arguments: &str) -> ContentBlock {
    ContentBlock::ToolCall {
        call_id: uid(call_id).parse::<CallId>().unwrap(),
        name: name.into(),
        arguments: match serde_json::from_str(arguments).unwrap() {
            serde_json::Value::Object(map) => map,
            _ => unreachable!("object arguments"),
        },
        provider_call_id: None,
    }
}

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text { text: value.into() }
}

/// A session with tool calls and results, media, a compaction and a pin on the
/// active branch, plus one sibling branch for `--all-branches`.
struct Fixture {
    _temp: tempfile::TempDir,
    root: Arc<DataRoot>,
    id: SessionId,
    /// Head of the rich active branch.
    head: EntryId,
    /// Head of the sibling branch.
    sibling: EntryId,
    /// Content-addressed artifacts the journal references.
    referenced: Vec<Digest>,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
    let configuration = root.put_artifact(br#"{"policy":"fixture"}"#).unwrap();
    let image = root
        .put_artifact(b"\x89PNG\r\n\x1a\nfixture image bytes")
        .unwrap();
    let report = root.put_artifact(b"build log: 0 warnings\n").unwrap();
    let plan = root.put_artifact(br#"{"plan":"read"}"#).unwrap();
    let id = uid(1).parse::<SessionId>().unwrap();
    let header = SessionHeader {
        workspace: "/workspace".into(),
        initial_model: model(),
        policy_revision: "initial".into(),
        configuration: configuration.clone(),
    };
    let mut store = JournalStore::create(root.clone(), id.clone(), header, at()).unwrap();

    // Turn 1: reasoning, a successful tool exchange with media, a failed one, then text.
    start(
        &mut store,
        0x100,
        0x900,
        None,
        vec![text("Summarize the release notes.")],
    );
    response(
        &mut store,
        0x101,
        vec![
            ContentBlock::ReasoningSummary {
                text: "read the notes first\ncheck the version".into(),
            },
            text("Reading the notes."),
            call(0x201, "read", r#"{"path":"notes.md"}"#),
        ],
        FinishReason::ToolCalls,
    );
    authorize(&mut store, &plan, 0x101, 0x201, 0x301, 0x401);
    tool_result(
        &mut store,
        ResultSpec {
            entry: 0x102,
            response_entry: 0x101,
            call: 0x201,
            invocation: 0x401,
            status: ToolStatus::Succeeded,
            code: None,
            content: vec![text("release notes body")],
            exit_code: Some(0),
            artifacts: vec![report.clone()],
        },
    );
    let snapshot = store.snapshot();
    append(
        &mut store,
        transition(&snapshot, RuntimeState::Executing, RuntimeState::Continuing),
    );
    response(
        &mut store,
        0x103,
        vec![
            text("The notes match the tag."),
            call(0x202, "bash", r#"{"command":"grep -c FIXME notes.md"}"#),
        ],
        FinishReason::ToolCalls,
    );
    authorize(&mut store, &plan, 0x103, 0x202, 0x302, 0x402);
    tool_result(
        &mut store,
        ResultSpec {
            entry: 0x104,
            response_entry: 0x103,
            call: 0x202,
            invocation: 0x402,
            status: ToolStatus::Failed,
            code: Some("no_match"),
            content: vec![text("grep: no match")],
            exit_code: Some(1),
            artifacts: vec![],
        },
    );
    let snapshot = store.snapshot();
    append(
        &mut store,
        transition(&snapshot, RuntimeState::Executing, RuntimeState::Continuing),
    );
    response(&mut store, 0x105, vec![text("Done.\n")], FinishReason::Stop);
    finish(&mut store, TurnOutcome::Completed);

    // Turns 2 and 3 keep the journal in the shape a summary can cover turn 1.
    start(
        &mut store,
        0x110,
        0x901,
        None,
        vec![
            text("What changed since then?"),
            ContentBlock::Image {
                artifact: image.clone(),
                mime_type: "image/png".into(),
            },
        ],
    );
    response(
        &mut store,
        0x111,
        vec![text("A new export command.")],
        FinishReason::Stop,
    );
    finish(&mut store, TurnOutcome::Completed);
    start(&mut store, 0x120, 0x902, None, vec![text("Any risks?")]);
    response(
        &mut store,
        0x121,
        vec![text("Shared artifacts.")],
        FinishReason::Stop,
    );
    finish(&mut store, TurnOutcome::Completed);

    // A compaction over the whole first turn, then a pin the summary cannot cover.
    let snapshot = store.snapshot();
    let covered: Vec<EntryId> = (0x100u128..=0x105)
        .map(|n| uid(n).parse::<EntryId>().unwrap())
        .collect();
    let configuration_fingerprint = snapshot.configuration().sha256.clone();
    let manifest =
        compaction_manifest(&snapshot, &covered, &[], &configuration_fingerprint).unwrap();
    let source_manifest = root.put_artifact(&manifest).unwrap();
    let compaction_entry = uid(0x130).parse::<EntryId>().unwrap();
    append(
        &mut store,
        Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: snapshot.head().cloned(),
                to: Some(compaction_entry.clone()),
                reason: HeadReason::Advance,
            },
            entries: vec![Entry {
                id: compaction_entry.clone(),
                parent_id: snapshot.head().cloned(),
                turn_id: None,
                payload: EntryPayload::Compaction(Compaction {
                    strategy: "observation_masking".into(),
                    strategy_version: "1".into(),
                    covered_entry_ids: covered,
                    source_fingerprint: source_manifest.sha256.clone(),
                    source_manifest,
                    supersedes: vec![],
                    summary: "Earlier turns summarized deterministically.".into(),
                    model: None,
                    configuration_fingerprint,
                }),
            }],
            events: vec![],
        },
    );
    let snapshot = store.snapshot();
    let pin = uid(0x140).parse::<EntryId>().unwrap();
    append(
        &mut store,
        Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: snapshot.head().cloned(),
                to: Some(pin.clone()),
                reason: HeadReason::Advance,
            },
            entries: vec![Entry {
                id: pin.clone(),
                parent_id: snapshot.head().cloned(),
                turn_id: None,
                payload: EntryPayload::ContextControl(ContextControl {
                    turn_id: uid(0x902).parse::<TurnId>().unwrap(),
                    mode: ContextMode::Pinned,
                }),
            }],
            events: vec![],
        },
    );

    // A sibling branch from turn 2, then back to the rich branch as the head.
    let snapshot = store.snapshot();
    let parent: Option<EntryId> = Some(uid(0x111).parse::<EntryId>().unwrap());
    append(
        &mut store,
        Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: snapshot.head().cloned(),
                to: parent.clone(),
                reason: HeadReason::Navigate,
            },
            entries: vec![],
            events: vec![],
        },
    );
    start(
        &mut store,
        0x150,
        0x903,
        None,
        vec![text("Try the other branch.")],
    );
    response(
        &mut store,
        0x151,
        vec![text("Branch answer.")],
        FinishReason::Stop,
    );
    finish(&mut store, TurnOutcome::Completed);
    let sibling = uid(0x151).parse::<EntryId>().unwrap();
    let snapshot = store.snapshot();
    append(
        &mut store,
        Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: snapshot.head().cloned(),
                to: Some(pin.clone()),
                reason: HeadReason::Navigate,
            },
            entries: vec![],
            events: vec![],
        },
    );
    let mut referenced: Vec<Digest> = store
        .snapshot()
        .events()
        .filter_map(|(_, _, event)| match event {
            JournalEvent::TurnStarted(start) => Some(start.configuration.sha256.clone()),
            _ => None,
        })
        .collect();
    referenced.extend([
        configuration.sha256.clone(),
        image.sha256.clone(),
        report.sha256.clone(),
        plan.sha256.clone(),
    ]);
    drop(store);
    Fixture {
        _temp: temp,
        root,
        id,
        head: pin,
        sibling,
        referenced,
    }
}

fn artifact_path(root: &DataRoot, digest: &Digest) -> PathBuf {
    root.path().join("artifacts").join(digest.as_str())
}

fn journal_path(root: &DataRoot, id: &SessionId) -> PathBuf {
    root.path().join("sessions").join(format!("{id}.jsonl"))
}

fn exported(fixture: &Fixture, name: &str, args: &[&str]) -> String {
    let output = fixture._temp.path().join(name);
    let mut all: Vec<&str> = vec!["export", fixture.id.as_str(), "--output"];
    let text = output.to_str().unwrap();
    all.push(text);
    all.extend(args);
    let result = run(&all);
    assert!(
        result.status.success(),
        "export failed: {}",
        stderr(&result)
    );
    fs::read_to_string(&output).unwrap()
}

#[test]
fn markdown_export_matches_the_active_branch_golden() {
    let _serial = serial();
    let fixture = fixture();
    let text = exported(
        &fixture,
        "active.md",
        &["--data-root", fixture.root.path().to_str().unwrap()],
    );
    assert_eq!(
        text,
        include_str!("fixtures/markdown-active-branch.md"),
        "the active-branch Markdown golden changed"
    );
}

#[test]
fn markdown_export_matches_the_all_branches_golden() {
    let _serial = serial();
    let fixture = fixture();
    let text = exported(
        &fixture,
        "branches.md",
        &[
            "--data-root",
            fixture.root.path().to_str().unwrap(),
            "--all-branches",
        ],
    );
    let golden = include_str!("fixtures/markdown-all-branches.md");
    let golden = golden
        .replace("<SIBLING>", fixture.sibling.as_str())
        .replace("<HEAD>", fixture.head.as_str())
        .replace("<SESSION>", fixture.id.as_str());
    assert_eq!(text, golden, "the all-branches Markdown golden changed");
    // The shared prefix appears once, and each branch head names its own tail.
    assert_eq!(text.matches("Summarize the release notes.").count(), 1);
    assert_eq!(text.matches("## Shared history").count(), 1);
    assert!(text.contains(&format!("## Branch {}", fixture.sibling)));
}

#[test]
fn all_branches_names_a_single_branch_head() {
    let _serial = serial();
    let temp = tempfile::tempdir().unwrap();
    let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
    let configuration = root.put_artifact(br#"{"policy":"fixture"}"#).unwrap();
    let id = uid(2).parse::<SessionId>().unwrap();
    let header = SessionHeader {
        workspace: "/workspace".into(),
        initial_model: model(),
        policy_revision: "initial".into(),
        configuration,
    };
    let mut store = JournalStore::create(root.clone(), id.clone(), header, at()).unwrap();
    start(&mut store, 0x200, 0x910, None, vec![text("Only turn.")]);
    response(
        &mut store,
        0x201,
        vec![text("Only answer.")],
        FinishReason::Stop,
    );
    finish(&mut store, TurnOutcome::Completed);
    drop(store);
    let text = root.export_markdown(&id, true).unwrap();
    assert!(
        text.contains(&format!("## Branch {}", uid(0x201))),
        "{text}"
    );
    assert!(!text.contains("## Shared history"), "{text}");
    assert_eq!(text.matches("## Branch").count(), 1);
    // The active-branch form stays at level two, with no branch heading.
    let active = root.export_markdown(&id, false).unwrap();
    assert!(!active.contains("## Branch"), "{active}");
    assert!(active.contains("\n## You\n"), "{active}");
}

#[test]
fn markdown_export_is_deterministic_and_never_overwrites() {
    let _serial = serial();
    let fixture = fixture();
    let data_root = fixture.root.path().to_str().unwrap();
    let first = exported(&fixture, "one.md", &["--data-root", data_root]);
    let second = exported(&fixture, "two.md", &["--data-root", data_root]);
    assert_eq!(first, second, "the same journal must export the same bytes");
    // Exporting again to the same path is refused and changes nothing.
    let path = fixture._temp.path().join("one.md");
    let output = run(&[
        "export",
        fixture.id.as_str(),
        "--output",
        path.to_str().unwrap(),
        "--data-root",
        data_root,
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("export_exists"), "{output:?}");
    assert_eq!(fs::read_to_string(&path).unwrap(), first);
}

#[test]
fn journal_export_is_a_replayable_data_root_with_verified_artifacts() {
    let _serial = serial();
    let fixture = fixture();
    let destination = fixture._temp.path().join("copy");
    let output = run(&[
        "export",
        fixture.id.as_str(),
        "--format",
        "journal",
        "--output",
        destination.to_str().unwrap(),
        "--data-root",
        fixture.root.path().to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    // The copy is itself a data root, and it replays to the same head.
    let root = DataRoot::open(&destination, false).unwrap();
    let copied = fs::read(journal_path(&root, &fixture.id)).unwrap();
    assert_eq!(
        copied,
        fs::read(journal_path(&fixture.root, &fixture.id)).unwrap()
    );
    let snapshot = JournalStore::open(Arc::new(root), &fixture.id)
        .unwrap()
        .snapshot();
    assert_eq!(snapshot.head(), Some(&fixture.head));
    let original = JournalStore::inspect(&fixture.root, &fixture.id)
        .unwrap()
        .snapshot
        .unwrap();
    assert_eq!(snapshot.sequence(), original.sequence());
    // Every referenced artifact came along, and nothing else did.
    for digest in &fixture.referenced {
        assert!(artifact_path(&DataRoot::open(&destination, false).unwrap(), digest).exists());
    }
    // A second export to the same directory is refused.
    let again = run(&[
        "export",
        fixture.id.as_str(),
        "--format",
        "journal",
        "--output",
        destination.to_str().unwrap(),
        "--data-root",
        fixture.root.path().to_str().unwrap(),
    ]);
    assert_eq!(again.status.code(), Some(2), "{again:?}");
    assert!(stderr(&again).contains("export_exists"));
}

#[test]
fn journal_export_fails_on_a_corrupt_artifact_and_removes_the_partial_copy() {
    let _serial = serial();
    let fixture = fixture();
    // Truncate one referenced artifact: the journal is now invalid too, so the
    // export refuses before copying anything.
    let victim = artifact_path(&fixture.root, &fixture.referenced[0]);
    let mut bytes = fs::read(&victim).unwrap();
    bytes.pop();
    fs::write(&victim, &bytes).unwrap();
    let destination = fixture._temp.path().join("partial");
    let output = run(&[
        "export",
        fixture.id.as_str(),
        "--format",
        "journal",
        "--output",
        destination.to_str().unwrap(),
        "--data-root",
        fixture.root.path().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let refusal = stderr(&output);
    assert!(
        refusal.contains("export_artifact_invalid") || refusal.contains("journal_invalid"),
        "{refusal}"
    );
    assert!(
        !destination.exists(),
        "a failed export left {destination:?}"
    );
}

#[test]
fn delete_needs_confirmation_and_leaves_every_artifact() {
    let _serial = serial();
    let fixture = fixture();
    let data_root = fixture.root.path().to_str().unwrap().to_owned();
    let artifacts_before: Vec<_> = fs::read_dir(fixture.root.path().join("artifacts"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    // A locked session cannot be deleted, even with --yes.
    let writer = writer(&fixture);
    let output = run(&[
        "delete",
        fixture.id.as_str(),
        "--yes",
        "--data-root",
        &data_root,
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("session_locked"), "{output:?}");
    assert!(journal_path(&fixture.root, &fixture.id).exists());
    drop(writer);
    // Without a terminal and without --yes, nothing is decided for the user.
    let output = run(&["delete", fixture.id.as_str(), "--data-root", &data_root]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        stderr(&output).contains("confirmation_required"),
        "{output:?}"
    );
    assert!(journal_path(&fixture.root, &fixture.id).exists());
    // With --yes the journal goes and every artifact stays.
    let output = run(&[
        "delete",
        fixture.id.as_str(),
        "--yes",
        "--data-root",
        &data_root,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("run 'harness sessions gc'"),
        "{output:?}"
    );
    assert!(!journal_path(&fixture.root, &fixture.id).exists());
    let artifacts_after: Vec<_> = fs::read_dir(fixture.root.path().join("artifacts"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(artifacts_before.len(), artifacts_after.len());
    assert!(fixture.root.sessions().unwrap().is_empty());
    // The session identity is gone from the catalog and from the files.
    let output = run(&[
        "delete",
        fixture.id.as_str(),
        "--yes",
        "--data-root",
        &data_root,
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("session_not_found"), "{output:?}");
}

#[test]
fn gc_keeps_referenced_and_young_artifacts_and_removes_old_orphans() {
    let _serial = serial();
    let fixture = fixture();
    let data_root = fixture.root.path().to_str().unwrap().to_owned();
    let old = fixture
        .root
        .put_artifact(b"orphan of a deleted session")
        .unwrap();
    let young = fixture
        .root
        .put_artifact(b"just written by a writer")
        .unwrap();
    age(artifact_path(&fixture.root, &old.sha256), 30);
    // A dry run lists the candidate and changes nothing.
    let dry = run(&["gc", "--dry-run", "--data-root", &data_root]);
    assert!(dry.status.success(), "{}", stderr(&dry));
    let text = stdout(&dry);
    assert!(text.starts_with("would remove 1 artifact,"), "{text}");
    assert!(text.contains(old.sha256.as_str()), "{text}");
    assert!(text.contains("run 'harness sessions gc --yes'"), "{text}");
    assert!(artifact_path(&fixture.root, &old.sha256).exists());
    // A real run without a terminal needs --yes.
    let refused = run(&["gc", "--data-root", &data_root]);
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    assert!(stderr(&refused).contains("confirmation_required"));
    assert!(artifact_path(&fixture.root, &old.sha256).exists());
    // Now remove it: the young orphan and every referenced artifact stay.
    let real = run(&["gc", "--yes", "--data-root", &data_root]);
    assert!(real.status.success(), "{}", stderr(&real));
    assert!(stdout(&real).starts_with("removed 1 artifact,"), "{real:?}");
    assert!(!artifact_path(&fixture.root, &old.sha256).exists());
    assert!(artifact_path(&fixture.root, &young.sha256).exists());
    for digest in &fixture.referenced {
        assert!(
            artifact_path(&fixture.root, digest).exists(),
            "gc removed referenced artifact {digest}"
        );
    }
}

#[test]
fn gc_refuses_the_whole_run_when_a_journal_cannot_be_read() {
    let _serial = serial();
    let fixture = fixture();
    let data_root = fixture.root.path().to_str().unwrap().to_owned();
    let orphan = fixture.root.put_artifact(b"orphan").unwrap();
    age(artifact_path(&fixture.root, &orphan.sha256), 30);
    // A second journal with an unterminated tail is unreadable, so nothing is
    // removed: that journal might reference anything.
    let broken = uid(2).parse::<SessionId>().unwrap();
    fs::write(journal_path(&fixture.root, &broken), b"{\"record\":").unwrap();
    let output = run(&["gc", "--yes", "--data-root", &data_root]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let refusal = stderr(&output);
    assert!(refusal.contains("gc_journal_unreadable"), "{refusal}");
    assert!(refusal.contains(broken.as_str()), "{refusal}");
    assert!(artifact_path(&fixture.root, &orphan.sha256).exists());
}

#[test]
fn gc_refuses_while_another_engine_holds_a_journal() {
    let _serial = serial();
    let fixture = fixture();
    let data_root = fixture.root.path().to_str().unwrap().to_owned();
    let orphan = fixture.root.put_artifact(b"orphan").unwrap();
    age(artifact_path(&fixture.root, &orphan.sha256), 30);
    let writer = writer(&fixture);
    let output = run(&["gc", "--yes", "--data-root", &data_root]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("gc_journal_unreadable"));
    assert!(artifact_path(&fixture.root, &orphan.sha256).exists());
    drop(writer);
}

#[test]
fn put_artifact_refreshes_the_mtime_of_a_deduplicated_artifact() {
    let _serial = serial();
    let fixture = fixture();
    let bytes = b"reused bytes";
    let first = fixture.root.put_artifact(bytes).unwrap();
    let path = artifact_path(&fixture.root, &first.sha256);
    age(path.clone(), 30);
    let aged = fs::metadata(&path).unwrap().modified().unwrap();
    assert!(aged < SystemTime::now() - Duration::from_secs(29 * 24 * 3600));
    // A dedupe hit is a reuse, so the artifact is young again.
    let second = fixture.root.put_artifact(bytes).unwrap();
    assert_eq!(second.sha256, first.sha256);
    let refreshed = fs::metadata(&path).unwrap().modified().unwrap();
    assert!(
        refreshed > aged,
        "dedupe left the modification time at {aged:?}"
    );
    // So an explicit collection keeps it even though no journal references it.
    let report = fixture
        .root
        .collect_artifacts(&GcOptions::default())
        .unwrap();
    assert!(
        report
            .candidates
            .iter()
            .all(|candidate| candidate.sha256 != first.sha256),
        "gc removed a just-deduplicated artifact"
    );
    assert!(path.exists());
}

#[test]
fn export_refuses_a_session_another_engine_holds() {
    let _serial = serial();
    let fixture = fixture();
    let writer = writer(&fixture);
    for format in ["markdown", "journal"] {
        let destination = fixture._temp.path().join(format!("held-{format}"));
        let output = run(&[
            "export",
            fixture.id.as_str(),
            "--format",
            format,
            "--output",
            destination.to_str().unwrap(),
            "--data-root",
            fixture.root.path().to_str().unwrap(),
        ]);
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(stderr(&output).contains("session_locked"), "{output:?}");
        assert!(!destination.exists());
    }
    drop(writer);
}

#[test]
fn export_never_changes_the_data_root() {
    let _serial = serial();
    let fixture = fixture();
    let before = whole_tree(fixture.root.path());
    let data_root = fixture.root.path().to_str().unwrap().to_owned();
    exported(&fixture, "read-only.md", &["--data-root", &data_root]);
    let copy = fixture._temp.path().join("read-only-copy");
    let output = run(&[
        "export",
        fixture.id.as_str(),
        "--format",
        "journal",
        "--output",
        copy.to_str().unwrap(),
        "--data-root",
        &data_root,
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    let dry = run(&["gc", "--dry-run", "--data-root", &data_root]);
    assert!(dry.status.success(), "{}", stderr(&dry));
    assert_eq!(
        whole_tree(fixture.root.path()),
        before,
        "a read-only command changed the data root"
    );
}

#[test]
fn maintenance_never_follows_a_symlink() {
    let _serial = serial();
    let fixture = fixture();
    let outside = fixture._temp.path().join("outside.txt");
    fs::write(&outside, b"untouched").unwrap();
    let digest: Digest = "ab".repeat(32).parse().unwrap();
    let linked = artifact_path(&fixture.root, &digest);
    std::os::unix::fs::symlink(&outside, &linked).unwrap();
    // A symlinked journal cannot be validated, so collection refuses the run and
    // the link and its target are untouched.
    let broken = uid(3).parse::<SessionId>().unwrap();
    let linked_journal = journal_path(&fixture.root, &broken);
    std::os::unix::fs::symlink(&outside, &linked_journal).unwrap();
    let output = run(&[
        "gc",
        "--yes",
        "--data-root",
        fixture.root.path().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("gc_journal_unreadable"));
    assert!(fs::symlink_metadata(&linked).is_ok());
    assert!(fs::symlink_metadata(&linked_journal).is_ok());
    assert_eq!(fs::read(&outside).unwrap(), b"untouched");
    // Deletion refuses a symlinked journal too, and never follows it.
    let output = run(&[
        "delete",
        broken.as_str(),
        "--yes",
        "--data-root",
        fixture.root.path().to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(fs::symlink_metadata(&linked_journal).is_ok());
    assert_eq!(fs::read(&outside).unwrap(), b"untouched");
    // Without the symlinked journal, collection runs and still leaves the
    // symlinked artifact and its target alone.
    fs::remove_file(&linked_journal).unwrap();
    let orphan = fixture.root.put_artifact(b"real orphan").unwrap();
    age(artifact_path(&fixture.root, &orphan.sha256), 30);
    let output = run(&[
        "gc",
        "--yes",
        "--data-root",
        fixture.root.path().to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!artifact_path(&fixture.root, &orphan.sha256).exists());
    assert!(fs::symlink_metadata(&linked).is_ok());
    assert_eq!(fs::read(&outside).unwrap(), b"untouched");
}

#[test]
fn the_listing_is_identical_with_and_without_the_subcommand() {
    let _serial = serial();
    let fixture = fixture();
    let data_root = fixture.root.path().to_str().unwrap().to_owned();
    let legacy = run(&["--data-root", &data_root]);
    let listed = run(&["list", "--data-root", &data_root]);
    assert!(legacy.status.success());
    assert!(listed.status.success());
    assert_eq!(stdout(&legacy), stdout(&listed));
    // Export and export refuse to touch the root: the listing is unchanged.
    let before = fs::read(journal_path(&fixture.root, &fixture.id)).unwrap();
    exported(&fixture, "unchanged.md", &["--data-root", &data_root]);
    assert_eq!(
        fs::read(journal_path(&fixture.root, &fixture.id)).unwrap(),
        before
    );
}

#[test]
fn usage_errors_are_refused_for_unknown_options() {
    let _serial = serial();
    let fixture = fixture();
    let output = run(&[
        "export",
        fixture.id.as_str(),
        "--format",
        "html",
        "--output",
        "out.html",
        "--data-root",
        fixture.root.path().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let output = run(&[
        "export",
        fixture.id.as_str(),
        "--format",
        "journal",
        "--all-branches",
        "--output",
        "out",
        "--data-root",
        fixture.root.path().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("--all-branches"));
}

/// Hold the writer lock of the fixture's own session.
fn writer(fixture: &Fixture) -> JournalStore {
    JournalStore::open(fixture.root.clone(), &fixture.id).unwrap()
}

/// Every path and every byte under a directory, for read-only assertions.
fn whole_tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(directory: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        for item in fs::read_dir(directory).unwrap() {
            let path = item.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                out.push((path.clone(), Vec::new()));
                walk(&path, out);
            } else {
                out.push((path.clone(), fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

/// Set an artifact's modification time back by `days`, through the test-only
/// `set_modified` API; the grace window itself is never shortened.
fn age(path: PathBuf, days: u64) {
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(days * 24 * 3600))
        .unwrap();
    let _: &Path = &path;
}

#[test]
fn export_refuses_symlink_outputs_without_touching_the_target() {
    let _serial = serial();
    let f = fixture();
    let outside = f._temp.path().join("outside");
    fs::write(&outside, b"keep").unwrap();
    let linked = f._temp.path().join("link");
    std::os::unix::fs::symlink(&outside, &linked).unwrap();
    for result in [
        f.root.export_markdown_to(&f.id, &linked, false),
        f.root.export_journal(&f.id, &linked),
    ] {
        assert!(matches!(result, Err(MaintenanceError::Exists)));
    }
    assert_eq!(fs::read(outside).unwrap(), b"keep");
}
