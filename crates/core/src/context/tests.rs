//! Builder tests against real journals: golden reports for the RFC-001
//! fixtures, and journals appended through `JournalStore` for histories the
//! runtime core does not produce, such as tool calls in cancelled responses.

#[path = "compaction_tests.rs"]
mod compaction_tests;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde_json::{Value, json};

use super::{projection::fingerprint_with_version, *};
use crate::session::*;

fn at() -> Timestamp {
    "2026-10-21T12:00:00Z".parse().unwrap()
}

fn mock() -> ModelRef {
    ModelRef {
        provider_instance: "mock".into(),
        model: "scripted".into(),
    }
}

fn other() -> ModelRef {
    ModelRef {
        provider_instance: "other".into(),
        model: "model".into(),
    }
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text { text: text.into() }
}

fn call(call_id: &CallId, name: &str) -> ContentBlock {
    ContentBlock::ToolCall {
        call_id: call_id.clone(),
        name: name.into(),
        arguments: serde_json::from_value(json!({"path": "README.md"})).unwrap(),
        provider_call_id: Some("native-1".into()),
    }
}

/// `context_tokens` for `model`, with the default deductions.
fn config_with_limit(model: &ModelRef, context_tokens: Option<u32>) -> ContextConfig {
    let mut config = ContextConfig::default();
    config.catalog.insert(
        model.clone(),
        ModelLimits {
            context_tokens,
            max_output_tokens: None,
        },
    );
    config
}

// ----- fixtures and golden reports -------------------------------------------

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/rfcs/001/examples")
}

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/context/golden")
        .join(format!("{name}.json"))
}

/// An RFC-001 example journal and its artifacts in a fresh data root.
struct Fixture {
    _temp: tempfile::TempDir,
    root: Arc<DataRoot>,
    session: SessionId,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
        for entry in fs::read_dir(examples().join("artifacts")).unwrap() {
            root.put_artifact(&fs::read(entry.unwrap().path()).unwrap())
                .unwrap();
        }
        let bytes = fs::read(examples().join(format!("{name}.jsonl"))).unwrap();
        let first: Value =
            serde_json::from_slice(bytes.split(|b| *b == b'\n').next().unwrap()).unwrap();
        let session: SessionId =
            serde_json::from_value(first["record"]["session_id"].clone()).unwrap();
        let mut file = root.journal(&session, true, true).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
        Self {
            _temp: temp,
            root,
            session,
        }
    }

    /// The read-only path `harness context` takes.
    fn snapshot(&self) -> SessionSnapshot {
        let inspection = JournalStore::inspect(&self.root, &self.session).unwrap();
        assert!(inspection.diagnostic.is_none());
        inspection.snapshot.unwrap()
    }

    fn configuration(&self) -> SessionConfiguration {
        let snapshot = self.snapshot();
        SessionConfiguration::load(&self.root, snapshot.configuration()).unwrap()
    }

    /// What `harness context <root> <id> --context-tokens 128000` builds.
    fn build(&self, head: Option<&EntryId>) -> BuiltContext {
        let snapshot = self.snapshot();
        let head = head.or(snapshot.head());
        let config = config_with_limit(snapshot.model_at(head).unwrap(), Some(128_000));
        ContextBuilder::new(&config, &self.configuration())
            .build(&snapshot, head, None)
            .unwrap()
    }
}

/// Compare a report with its checked-in golden file, byte for byte, in the
/// format `harness context` prints. `UPDATE_GOLDEN=1` rewrites the file.
fn golden(name: &str, report: &ContextReport) {
    let actual = serde_json::to_string_pretty(report).unwrap() + "\n";
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::write(&path, &actual).unwrap();
    }
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {}; run with UPDATE_GOLDEN=1", path.display()));
    assert!(
        actual == expected,
        "{name} report differs from {}; rerun with UPDATE_GOLDEN=1 and review the diff\n{actual}",
        path.display()
    );
    // The serialization is stable: it round-trips to the same value.
    let parsed: ContextReport = serde_json::from_str(&expected).unwrap();
    assert_eq!(&parsed, report);
}

fn id(suffix: &str) -> EntryId {
    format!("019a0000-0000-7000-8000-000000000{suffix}")
        .parse()
        .unwrap()
}

fn included_ids(report: &ContextReport) -> Vec<Option<EntryId>> {
    report.included.iter().map(|i| i.entry_id.clone()).collect()
}

#[test]
fn golden_branching_reports_only_the_selected_branch() {
    let fixture = Fixture::new("branching");
    let built = fixture.build(None);
    golden("branching", &built.report);
    let report = &built.report;
    // Approach B is selected; approach A is a sibling and never appears.
    assert_eq!(
        included_ids(report),
        [Some(id("065")), Some(id("067")), Some(id("12d"))]
    );
    assert!(report.excluded.is_empty());
    let mentions_sibling = serde_json::to_string(report)
        .unwrap()
        .contains(id("0c9").as_str());
    assert!(!mentions_sibling, "a sibling entry appears in the report");
    assert_eq!(report.instructions, InstructionsReport::Empty);

    // Inspecting the sibling gives its own branch, projection and fingerprint.
    let sibling = fixture.build(Some(&id("0c9")));
    assert_eq!(
        included_ids(&sibling.report),
        [Some(id("065")), Some(id("067")), Some(id("0c9"))]
    );
    assert_ne!(sibling.projection.input.items, built.projection.input.items);
    assert_ne!(sibling.projection.fingerprint, built.projection.fingerprint);
}

#[test]
fn golden_compaction_places_the_summary_at_its_first_covered_entry() {
    let fixture = Fixture::new("compaction");
    let built = fixture.build(None);
    golden("compaction", &built.report);
    let report = &built.report;
    // Summary first, then the two retained turns. The summary's own entry is
    // last on the branch but its item takes the first covered position.
    assert_eq!(
        included_ids(report),
        [
            Some(id("320")),
            Some(id("259")),
            Some(id("25b")),
            Some(id("2bd")),
            Some(id("2bf"))
        ]
    );
    let ContextItem::Summary { summary, .. } = &built.projection.input.items[0] else {
        panic!("summary is not first");
    };
    assert!(summary.starts_with("Task/constraint"));
    let covered: Vec<_> = report
        .excluded
        .iter()
        .map(|e| (e.entry_id.clone(), e.part, e.reason, e.summary_id.clone()))
        .collect();
    assert_eq!(
        covered,
        [id("1f5"), id("1f7")].map(|entry| (
            entry,
            Part::Entry,
            ExclusionReason::CoveredBySummary,
            Some(id("320"))
        ))
    );
    assert_eq!(report.summaries.len(), 1);
    assert_eq!(
        report.summaries[0].covered_entry_ids,
        [id("1f5"), id("1f7")]
    );
    assert!(report.summaries[0].included);
    assert_eq!(
        report.totals.estimated_summaries,
        report.summaries[0].estimated_tokens
    );
}

#[test]
fn golden_context_controls_pin_and_exclude_whole_turns() {
    let fixture = Fixture::new("context-controls");
    let built = fixture.build(None);
    golden("context-controls", &built.report);
    // The fixture's control entries have four-digit identities.
    let control = |suffix: &str| -> EntryId {
        format!("019a0000-0000-7000-8000-00000000{suffix}")
            .parse()
            .unwrap()
    };
    let report = &built.report;
    // Turn 1 is pinned, turn 2 is excluded, turn 3 is back to default, and
    // the two controls for turn 3 are markers only.
    assert_eq!(
        included_ids(report),
        [
            Some(id("4a1")),
            Some(id("4a3")),
            Some(id("4c1")),
            Some(id("4c3"))
        ]
    );
    assert_eq!(
        report
            .included
            .iter()
            .map(|item| item.pinned)
            .collect::<Vec<_>>(),
        [true, true, false, false]
    );
    let excluded: Vec<_> = report
        .excluded
        .iter()
        .map(|e| (e.entry_id.clone(), e.part, e.reason))
        .collect();
    assert_eq!(
        excluded,
        [
            (id("4b1"), Part::Entry, ExclusionReason::UserExcluded),
            (id("4b3"), Part::Entry, ExclusionReason::UserExcluded),
            (
                control("4050"),
                Part::Entry,
                ExclusionReason::ContextControlMarker
            ),
            (
                control("4051"),
                Part::Entry,
                ExclusionReason::ContextControlMarker
            ),
            (
                control("4052"),
                Part::Entry,
                ExclusionReason::ContextControlMarker
            ),
            (
                control("4053"),
                Part::Entry,
                ExclusionReason::ContextControlMarker
            ),
        ]
    );
    assert_eq!(
        report.controls,
        [
            ControlReport {
                turn_id: "019a0000-0000-7000-8000-0000000004a0".parse().unwrap(),
                mode: ContextMode::Pinned,
                control_entry_id: control("4050"),
            },
            ControlReport {
                turn_id: "019a0000-0000-7000-8000-0000000004b0".parse().unwrap(),
                mode: ContextMode::Excluded,
                control_entry_id: control("4051"),
            }
        ]
    );
    // Excluding a turn changes what is sent, so it changes the fingerprint;
    // pinning changes only the report.
    let mut journal = Journal::new();
    journal.exchange("one", "first");
    journal.exchange("two", "second");
    let plain = journal.build().projection.fingerprint;
    let turn = journal.turn_of(&journal.snapshot().head().cloned().unwrap());
    journal.control(&turn, ContextMode::Pinned);
    assert_eq!(journal.build().projection.fingerprint, plain);
    journal.control(&turn, ContextMode::Excluded);
    assert_ne!(journal.build().projection.fingerprint, plain);
    // The golden's own fingerprint is over the two turns left in the request.
    assert_ne!(built.projection.fingerprint, plain);
}

#[test]
fn golden_interrupted_execution_excludes_the_unanswered_call() {
    let fixture = Fixture::new("interrupted-execution");
    let built = fixture.build(None);
    golden("interrupted-execution", &built.report);
    // The fixture's response completed with `tool_calls`; its read has an
    // execution intent and no result, so the call has no result on this branch.
    assert_eq!(
        built.report.excluded,
        [Exclusion {
            entry_id: id("19a"),
            part: Part::Block { index: 0 },
            reason: ExclusionReason::OrphanToolCall,
            summary_id: None,
            effective_summary: None,
        }]
    );
    // With its only block excluded, the response is no item at all; the
    // block's exclusion is its whole account.
    let [ContextItem::User { .. }] = &built.projection.input.items[..] else {
        panic!("unexpected items {:?}", built.projection.input.items);
    };
}

#[test]
fn fixture_reports_are_identical_after_reopening_and_across_data_roots() {
    for name in ["branching", "compaction", "interrupted-execution"] {
        let first = Fixture::new(name);
        let built = first.build(None);
        assert_eq!(first.build(None), built, "two builds of {name} differ");
        // A writable open replays the same journal into a new snapshot.
        let store = JournalStore::open(first.root.clone(), &first.session).unwrap();
        let reopened = store.snapshot();
        drop(store);
        let config = config_with_limit(reopened.model_at(reopened.head()).unwrap(), Some(128_000));
        let rebuilt = ContextBuilder::new(&config, &first.configuration())
            .build(&reopened, reopened.head(), None)
            .unwrap();
        assert_eq!(rebuilt, built, "{name} differs after reopening");
        assert_eq!(
            Fixture::new(name).build(None),
            built,
            "{name} differs elsewhere"
        );
        assert_eq!(
            built.projection.fingerprint,
            fingerprint(&built.projection.input)
        );
    }
}

#[test]
fn projection_version_is_part_of_the_fingerprint() {
    let built = Fixture::new("compaction").build(None);
    let input = &built.projection.input;
    assert_eq!(
        fingerprint_with_version(PROJECTION_VERSION, input),
        built.projection.fingerprint
    );
    assert_ne!(
        fingerprint_with_version(PROJECTION_VERSION + 1, input),
        built.projection.fingerprint
    );
    // The model is hashed too.
    assert_ne!(
        fingerprint(&crate::model::ModelInput {
            model: other(),
            ..input.clone()
        }),
        built.projection.fingerprint
    );
}

#[test]
fn canonical_projection_bytes_are_compact_with_sorted_argument_keys() {
    // `serde_json` must not preserve insertion order: the fingerprint relies
    // on tool-call arguments serializing with sorted keys.
    let arguments: serde_json::Map<String, Value> =
        serde_json::from_str(r#"{"z":1,"a":{"y":2,"b":3}}"#).unwrap();
    assert_eq!(
        serde_json::to_string(&arguments).unwrap(),
        r#"{"a":{"b":3,"y":2},"z":1}"#,
        "serde_json's preserve_order feature is enabled; fingerprints would \
         depend on argument order"
    );
    let entry_id = id("065");
    let call_id: CallId = "019a0000-0000-7000-8000-000000000066".parse().unwrap();
    let items = [ContextItem::Assistant {
        entry_id,
        outcome: ResponseOutcome::Completed,
        content: vec![ContentBlock::ToolCall {
            call_id,
            name: "read".into(),
            arguments,
            provider_call_id: None,
        }],
    }];
    let input = crate::model::ModelInput {
        model: mock(),
        items: items.to_vec(),
        tools: vec![],
        options: crate::model::InferenceOptions {
            max_output_tokens: 4096,
            reasoning_effort: None,
            provider_options: Default::default(),
        },
        continuation: None,
    };
    let bytes = super::projection::canonical_bytes(&input);
    assert_eq!(
        String::from_utf8(bytes.clone()).unwrap(),
        concat!(
            r#"{"items":[{"kind":"assistant","entry_id":"019a0000-0000-7000-8000-000000000065","#,
            r#""outcome":"completed","content":[{"kind":"tool_call","#,
            r#""call_id":"019a0000-0000-7000-8000-000000000066","name":"read","#,
            r#""arguments":{"a":{"b":3,"y":2},"z":1},"provider_call_id":null}]}],"#,
            r#""model":{"provider_instance":"mock","model":"scripted"},"tools":[],"#,
            r#""options":{"max_output_tokens":4096,"reasoning_effort":null,"provider_options":{}},"projection_version":4}"#
        )
    );
    assert_eq!(fingerprint(&input), Digest::of(&bytes));
}

// ----- journals appended through storage -------------------------------------

/// A session appended commit by commit, as the runtime would, but able to
/// record histories the runtime does not produce.
struct Journal {
    _temp: tempfile::TempDir,
    root: Arc<DataRoot>,
    store: JournalStore,
    configuration: SessionConfiguration,
}

impl Journal {
    fn new() -> Self {
        Self::with_configuration(br#"{"policy":"context-test"}"#)
    }

    fn with_configuration(bytes: &[u8]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
        let configuration = root.put_artifact(bytes).unwrap();
        let header = SessionHeader {
            workspace: "/workspace".into(),
            initial_model: mock(),
            policy_revision: "initial".into(),
            configuration: configuration.clone(),
        };
        let store = JournalStore::create(root.clone(), SessionId::new(), header, at()).unwrap();
        Self {
            configuration: SessionConfiguration::load(&root, &configuration).unwrap(),
            _temp: temp,
            root,
            store,
        }
    }

    fn snapshot(&self) -> SessionSnapshot {
        self.store.snapshot()
    }

    fn head(&self) -> Option<EntryId> {
        self.snapshot().head().cloned()
    }

    fn turn(&self) -> TurnId {
        self.snapshot().active_turn().unwrap().clone()
    }

    fn append(&mut self, entries: Vec<(Option<TurnId>, EntryPayload)>, events: Vec<JournalEvent>) {
        let snapshot = self.snapshot();
        let mut parent = snapshot.head().cloned();
        let entries: Vec<Entry> = entries
            .into_iter()
            .map(|(turn_id, payload)| {
                let id = EntryId::new();
                Entry {
                    id: id.clone(),
                    parent_id: parent.replace(id),
                    turn_id,
                    payload,
                }
            })
            .collect();
        let to = entries.last().map(|e| e.id.clone()).or(parent);
        let reason = if entries.is_empty() {
            HeadReason::Retain
        } else {
            HeadReason::Advance
        };
        let commit = Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: snapshot.head().cloned(),
                to,
                reason,
            },
            entries,
            events,
        };
        self.store.append(commit, at()).unwrap();
    }

    fn transition(&self, from: RuntimeState, to: RuntimeState) -> JournalEvent {
        JournalEvent::StateChanged(StateChanged {
            turn_id: self.turn(),
            from,
            to,
        })
    }

    fn finished(&self, outcome: TurnOutcome, code: Option<&str>) -> JournalEvent {
        JournalEvent::TurnFinished(TurnFinished {
            turn_id: self.turn(),
            outcome,
            code: code.map(Into::into),
        })
    }

    /// C1 and C2: a user message, then `preparing>inferring`.
    fn user(&mut self, content: Vec<ContentBlock>) -> EntryId {
        let turn = TurnId::new();
        let snapshot = self.snapshot();
        let entry = EntryId::new();
        let commit = Commit {
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
                payload: EntryPayload::UserMessage(UserMessage { content }),
            }],
            events: vec![
                JournalEvent::TurnStarted(TurnStarted {
                    turn_id: turn.clone(),
                    user_entry_id: entry.clone(),
                    configuration: snapshot.configuration().clone(),
                }),
                JournalEvent::StateChanged(StateChanged {
                    turn_id: turn,
                    from: RuntimeState::Idle,
                    to: RuntimeState::Preparing,
                }),
            ],
        };
        self.store.append(commit, at()).unwrap();
        self.append(
            vec![],
            vec![self.transition(RuntimeState::Preparing, RuntimeState::Inferring)],
        );
        entry
    }

    /// C3: the response and whatever its outcome implies, as the core records it.
    fn respond(&mut self, response: Response) -> EntryId {
        let snapshot = self.snapshot();
        let model = snapshot.model_at(snapshot.head()).unwrap().clone();
        let (finish_reason, error, events) = match response.outcome {
            ResponseOutcome::Completed => {
                let finish = response.finish.unwrap_or(FinishReason::Stop);
                let mut events =
                    vec![self.transition(RuntimeState::Inferring, RuntimeState::Processing)];
                if finish != FinishReason::ToolCalls {
                    events.push(self.finished(TurnOutcome::Completed, None));
                }
                (Some(finish), None, events)
            }
            ResponseOutcome::Failed => (
                None,
                Some(ModelError {
                    code: ModelErrorCode::ProviderFailure,
                    message: "boom".into(),
                    provider_code: None,
                    retry_after_ms: None,
                }),
                vec![self.finished(TurnOutcome::Failed, Some("provider_failure"))],
            ),
            ResponseOutcome::Cancelled => (
                None,
                None,
                vec![self.finished(TurnOutcome::Cancelled, Some("provider_cancelled"))],
            ),
            ResponseOutcome::Interrupted => (
                None,
                None,
                vec![self.finished(TurnOutcome::Failed, Some("provider_eof"))],
            ),
        };
        let continuation = response.continuation.map(|artifact| ProviderContinuation {
            owner: model.clone(),
            schema: "mock.continuation.v1".into(),
            artifact,
            input_fingerprint: response
                .input_fingerprint
                .unwrap_or_else(|| self.build().projection.fingerprint),
        });
        self.append(
            vec![(
                Some(self.turn()),
                EntryPayload::AssistantResponse(AssistantResponse {
                    response_id: ResponseId::new(),
                    model,
                    outcome: response.outcome,
                    finish_reason,
                    content: response.content,
                    usage: response.usage,
                    continuation,
                    error,
                    projection_fingerprint: None,
                }),
            )],
            events,
        );
        self.head().unwrap()
    }

    /// C4: deny every pending call, then `preparing>inferring` again.
    fn answer(&mut self) -> Vec<EntryId> {
        let snapshot = self.snapshot();
        let pending = snapshot.0.pending(snapshot.head()).unwrap().to_vec();
        let results = pending
            .into_iter()
            .map(|call| {
                (
                    Some(self.turn()),
                    EntryPayload::ToolResult(ToolResultEntry {
                        result: ToolResult {
                            call,
                            invocation_id: None,
                            status: ToolStatus::Denied,
                            content: vec![text("tools_unavailable: none")],
                            metadata: ToolMetadata {
                                exit_code: None,
                                duration_ms: None,
                                output_truncated: false,
                                provenance: Provenance::Runtime,
                                artifacts: vec![],
                                code: Some("tools_unavailable".into()),
                            },
                        },
                    }),
                )
            })
            .collect::<Vec<_>>();
        let count = results.len();
        self.append(
            results,
            vec![
                self.transition(RuntimeState::Processing, RuntimeState::Continuing),
                self.transition(RuntimeState::Continuing, RuntimeState::Preparing),
            ],
        );
        self.append(
            vec![],
            vec![self.transition(RuntimeState::Preparing, RuntimeState::Inferring)],
        );
        let ancestry = self.snapshot();
        let ancestry = ancestry.ancestry(ancestry.head()).unwrap();
        ancestry[ancestry.len() - count..]
            .iter()
            .map(|e| e.entry.id.clone())
            .collect()
    }

    /// A completed turn: `[user, assistant]`.
    fn exchange(&mut self, prompt: &str, reply: &str) -> [EntryId; 2] {
        let user = self.user(vec![text(prompt)]);
        let assistant = self.respond(Response::completed(vec![text(reply)]));
        [user, assistant]
    }

    /// A `context_control` entry at the current head, while idle.
    fn control(&mut self, turn: &TurnId, mode: ContextMode) -> EntryId {
        self.append(
            vec![(
                None,
                EntryPayload::ContextControl(ContextControl {
                    turn_id: turn.clone(),
                    mode,
                }),
            )],
            vec![],
        );
        self.head().unwrap()
    }

    /// The turn a user or assistant entry belongs to.
    fn turn_of(&self, entry: &EntryId) -> TurnId {
        self.snapshot()
            .entry(entry)
            .unwrap()
            .entry
            .turn_id
            .clone()
            .unwrap()
    }

    fn switch_model(&mut self, model: ModelRef) -> EntryId {
        self.append(
            vec![(
                None,
                EntryPayload::ModelChanged(ModelChanged {
                    model,
                    portability: Portability::Reject,
                }),
            )],
            vec![],
        );
        self.head().unwrap()
    }

    fn compact(
        &mut self,
        covered: Vec<EntryId>,
        supersedes: Vec<EntryId>,
        summary: &str,
    ) -> EntryId {
        // Storage checks the manifest binds the exact source entries.
        let snapshot = self.snapshot();
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
            "covered": references(&covered),
            "supersedes": references(&supersedes),
            "configuration_fingerprint": snapshot.configuration().sha256,
        });
        let manifest = self
            .root
            .put_artifact(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        self.append(
            vec![(
                None,
                EntryPayload::Compaction(Compaction {
                    strategy: "rolling".into(),
                    strategy_version: "1".into(),
                    covered_entry_ids: covered,
                    source_fingerprint: manifest.sha256.clone(),
                    source_manifest: manifest,
                    supersedes,
                    summary: summary.into(),
                    model: Some(snapshot.model_at(snapshot.head()).unwrap().clone()),
                    configuration_fingerprint: snapshot.configuration().sha256.clone(),
                }),
            )],
            vec![],
        );
        self.head().unwrap()
    }

    fn navigate(&mut self, to: Option<EntryId>) {
        let snapshot = self.snapshot();
        let commit = Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: snapshot.head().cloned(),
                to,
                reason: HeadReason::Navigate,
            },
            entries: vec![],
            events: vec![],
        };
        self.store.append(commit, at()).unwrap();
    }

    fn build_with(
        &self,
        config: &ContextConfig,
        head: Option<&EntryId>,
        preview: Option<&ModelRef>,
    ) -> Result<BuiltContext, ContextError> {
        let snapshot = self.snapshot();
        ContextBuilder::new(config, &self.configuration).build(&snapshot, head, preview)
    }

    fn build(&self) -> BuiltContext {
        self.build_with(&ContextConfig::default(), self.head().as_ref(), None)
            .unwrap()
    }
}

struct Response {
    outcome: ResponseOutcome,
    finish: Option<FinishReason>,
    content: Vec<ContentBlock>,
    usage: Option<Usage>,
    continuation: Option<ArtifactRef>,
    input_fingerprint: Option<Digest>,
}

impl Response {
    fn completed(content: Vec<ContentBlock>) -> Self {
        Self::ended(ResponseOutcome::Completed, content)
    }
    fn ended(outcome: ResponseOutcome, content: Vec<ContentBlock>) -> Self {
        Self {
            outcome,
            finish: None,
            content,
            usage: None,
            continuation: None,
            input_fingerprint: None,
        }
    }
    fn finish(self, finish: FinishReason) -> Self {
        Self {
            finish: Some(finish),
            ..self
        }
    }
}

fn reasons(report: &ContextReport) -> Vec<(Part, ExclusionReason)> {
    report.excluded.iter().map(|e| (e.part, e.reason)).collect()
}

#[test]
fn partial_responses_keep_text_and_exclude_orphan_calls_reasoning_and_failures() {
    let mut journal = Journal::new();
    let mut partial = Vec::new();
    for outcome in [ResponseOutcome::Cancelled, ResponseOutcome::Interrupted] {
        journal.user(vec![text("go")]);
        let call_id = CallId::new();
        // Storage accepts calls in a partial response (storage review finding
        // 3). They have no result, so no provider may ever see them.
        partial.push(journal.respond(Response::ended(
            outcome,
            vec![
                ContentBlock::ReasoningSummary {
                    text: "thinking".into(),
                },
                text("partial"),
                call(&call_id, "read"),
            ],
        )));
    }
    journal.user(vec![text("again")]);
    let failed = journal.respond(Response::ended(ResponseOutcome::Failed, vec![text("lost")]));

    let built = journal.build();
    let report = &built.report;
    let expected: Vec<_> = partial
        .iter()
        .flat_map(|_| {
            [
                (
                    Part::Block { index: 0 },
                    ExclusionReason::ReasoningDisplayOnly,
                ),
                (Part::Block { index: 2 }, ExclusionReason::OrphanToolCall),
            ]
        })
        .chain([(Part::Entry, ExclusionReason::FailedResponse)])
        .collect();
    assert_eq!(reasons(report), expected);
    assert_eq!(report.excluded.last().unwrap().entry_id, failed);
    for (item, outcome) in built
        .projection
        .input
        .items
        .iter()
        .filter(|item| matches!(item, ContextItem::Assistant { .. }))
        .zip([ResponseOutcome::Cancelled, ResponseOutcome::Interrupted])
    {
        let ContextItem::Assistant {
            content,
            outcome: recorded,
            ..
        } = item
        else {
            unreachable!()
        };
        assert_eq!(content, &[text("partial")], "the user saw this text");
        assert_eq!(*recorded, outcome);
    }
    // Five user messages and responses, minus the failed response.
    assert_eq!(built.projection.input.items.len(), 5);
}

#[test]
fn responses_with_nothing_to_project_are_accounted_for_without_an_item() {
    let mut journal = Journal::new();
    let user = journal.user(vec![text("go")]);
    // Cancelled before any delta: no blocks, no continuation.
    let empty = journal.respond(Response::ended(ResponseOutcome::Cancelled, vec![]));
    let again = journal.user(vec![text("again")]);
    // Every block excluded: the part exclusions are the whole account.
    let excluded = journal.respond(Response::ended(
        ResponseOutcome::Interrupted,
        vec![
            ContentBlock::ReasoningSummary {
                text: "thinking".into(),
            },
            call(&CallId::new(), "read"),
        ],
    ));

    let built = journal.build();
    assert_eq!(included_ids(&built.report), [user, again].map(Some));
    assert!(
        built
            .projection
            .input
            .items
            .iter()
            .all(|item| !matches!(item, ContextItem::Assistant { .. }))
    );
    let decisions: Vec<_> = built
        .report
        .excluded
        .iter()
        .map(|e| (e.entry_id.clone(), e.part, e.reason))
        .collect();
    assert_eq!(
        decisions,
        [
            (empty, Part::Entry, ExclusionReason::EmptyResponse),
            (
                excluded.clone(),
                Part::Block { index: 0 },
                ExclusionReason::ReasoningDisplayOnly
            ),
            (
                excluded,
                Part::Block { index: 1 },
                ExclusionReason::OrphanToolCall
            ),
        ]
    );

    // An entry-level decision beside the part decisions decides it twice.
    let mut twice = built.report.clone();
    twice.excluded.push(Exclusion {
        entry_id: twice.excluded[1].entry_id.clone(),
        part: Part::Entry,
        reason: ExclusionReason::FailedResponse,
        summary_id: None,
        effective_summary: None,
    });
    let snapshot = journal.snapshot();
    let branch = snapshot.ancestry(snapshot.head()).unwrap();
    let applied = super::builder::Controls::fold(&branch);
    assert_eq!(
        super::check::check(&branch, &built.projection.input, &twice, &applied),
        Err("duplicate_decision")
    );
    // Dropping one part decision leaves the response incompletely decided.
    let mut partial = built.report.clone();
    partial.excluded.pop();
    assert_eq!(
        super::check::check(&branch, &built.projection.input, &partial, &applied),
        Err("incomplete_part_decisions")
    );
}

#[test]
fn switch_preview_classifies_every_projected_tool_part_and_unknown_model_defaults() {
    let mut journal = Journal::new();
    journal.user(vec![text("tools")]);
    let proposal = journal.respond(
        Response::completed(vec![
            text("look"),
            call(&CallId::new(), "read"),
            call(&CallId::new(), "list"),
        ])
        .finish(FinishReason::ToolCalls),
    );
    let results = journal.answer();
    journal.respond(Response::completed(vec![text("done")]));
    let mut config = ContextConfig::default();
    assert_eq!(
        config.catalog.capabilities(&other()),
        ModelCapabilities::default()
    );
    assert!(config.catalog.limits(&other()).is_none());
    config.catalog.insert(
        other(),
        ModelInfo {
            capabilities: ModelCapabilities {
                tools: false,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let preview = journal
        .build_with(&config, journal.snapshot().head(), Some(&other()))
        .unwrap()
        .report
        .preview
        .unwrap();
    assert_eq!(
        preview.required_incompatibilities,
        vec![
            RequiredIncompatibility {
                entry_id: proposal.clone(),
                part: Part::Block { index: 1 },
                capability: RequiredCapability::Tools
            },
            RequiredIncompatibility {
                entry_id: proposal,
                part: Part::Block { index: 2 },
                capability: RequiredCapability::Tools
            },
            RequiredIncompatibility {
                entry_id: results[0].clone(),
                part: Part::Entry,
                capability: RequiredCapability::Tools
            },
            RequiredIncompatibility {
                entry_id: results[1].clone(),
                part: Part::Entry,
                capability: RequiredCapability::Tools
            },
        ]
    );
}

#[test]
fn answered_calls_are_included_with_their_results_directly_after() {
    let mut journal = Journal::new();
    let user = journal.user(vec![text("use tools")]);
    let (first, second) = (CallId::new(), CallId::new());
    let proposal = journal.respond(
        Response::completed(vec![
            text("Let me look."),
            call(&first, "read"),
            call(&second, "list"),
        ])
        .finish(FinishReason::ToolCalls),
    );
    let results = journal.answer();
    let stop = journal.respond(Response::completed(vec![text("Done.")]));

    let built = journal.build();
    let kinds: Vec<_> = built.report.included.iter().map(|i| i.kind).collect();
    assert_eq!(
        kinds,
        [
            ItemKind::User,
            ItemKind::Assistant,
            ItemKind::ToolResult,
            ItemKind::ToolResult,
            ItemKind::Assistant
        ]
    );
    assert_eq!(
        included_ids(&built.report),
        [
            user,
            proposal.clone(),
            results[0].clone(),
            results[1].clone(),
            stop
        ]
        .map(Some)
    );
    assert!(built.report.excluded.is_empty());
    let totals = &built.report.totals;
    assert!(totals.estimated_tool_calls > 0 && totals.estimated_tool_results > 0);
    assert_eq!(
        built.report.included[1]
            .blocks
            .iter()
            .map(|b| b.kind)
            .collect::<Vec<_>>(),
        [BlockKind::Text, BlockKind::ToolCall, BlockKind::ToolCall]
    );

    // At the proposal itself no result is on the branch yet: the calls are
    // excluded rather than sent unanswered.
    let at_proposal = journal
        .build_with(&ContextConfig::default(), Some(&proposal), None)
        .unwrap();
    assert_eq!(
        reasons(&at_proposal.report),
        [
            (Part::Block { index: 1 }, ExclusionReason::OrphanToolCall),
            (Part::Block { index: 2 }, ExclusionReason::OrphanToolCall)
        ]
    );
    // Between the two results, only the answered call is included.
    let between = journal
        .build_with(&ContextConfig::default(), Some(&results[0]), None)
        .unwrap();
    assert_eq!(
        reasons(&between.report),
        [(Part::Block { index: 2 }, ExclusionReason::OrphanToolCall)]
    );
}

#[test]
fn provider_state_is_owner_scoped_and_previews_show_what_a_switch_loses() {
    let mut journal = Journal::new();
    let state = journal.root.put_artifact(b"opaque native state").unwrap();
    let continuation = journal.root.put_artifact(b"opaque continuation").unwrap();
    journal.user(vec![text("hello")]);
    let response = journal.respond(Response {
        continuation: Some(continuation.clone()),
        ..Response::completed(vec![
            text("hi"),
            ContentBlock::ProviderState {
                owner: mock(),
                schema: "mock.state.v1".into(),
                artifact: state.clone(),
            },
        ])
    });
    let opaque = state.bytes + continuation.bytes;

    // Included for its own model, attached to its response, counted in bytes.
    let own = journal.build();
    assert!(own.report.excluded.is_empty() && own.report.portability.is_empty());
    let ContextItem::Assistant { content, .. } = &own.projection.input.items[1] else {
        panic!("no assistant item");
    };
    assert_eq!(content.len(), 2);
    assert!(own.projection.input.continuation.is_some());
    assert_eq!(own.report.totals.opaque_provider_state_bytes, opaque);
    assert_eq!(own.report.included[1].opaque_bytes, opaque);
    assert!(matches!(
        own.report.budget,
        Budget::Unknown { unestimated_opaque_bytes, .. } if unestimated_opaque_bytes == opaque
    ));

    // A preview for another model at the same head: excluded, and listed as
    // what the switch would lose. Nothing is committed.
    let limits = config_with_limit(&other(), Some(1000)).catalog;
    let config = ContextConfig {
        catalog: limits,
        ..ContextConfig::default()
    };
    let before = journal.snapshot().sequence();
    let preview = journal
        .build_with(&config, Some(&response), Some(&other()))
        .unwrap();
    assert_eq!(journal.snapshot().sequence(), before);
    let report = &preview.report;
    assert!(report.flags.preview && preview.projection.preview);
    assert_eq!(report.identity.model, other());
    let lost = [
        (
            Part::Block { index: 1 },
            ExclusionReason::ProviderStateNotPortable,
        ),
        (
            Part::Continuation,
            ExclusionReason::ProviderStateNotPortable,
        ),
    ];
    assert_eq!(reasons(report), lost);
    let detail = report.preview.as_ref().unwrap();
    assert_eq!(detail.model_at_head, mock());
    assert_eq!(detail.would_stop_being_portable, report.portability);
    assert_eq!(detail.would_stop_being_portable.len(), 2);
    assert!(detail.would_become_portable.is_empty());
    // The budget is the preview model's; the head model's is shown beside it.
    assert!(matches!(
        report.budget,
        Budget::Known {
            context_tokens: 1000,
            ..
        }
    ));
    assert!(matches!(
        detail.budget_at_head,
        Budget::Unknown {
            reason: UnknownLimit::ModelNotInCatalog,
            ..
        }
    ));

    // After the switch is committed, the head's own build excludes the state,
    // and the marker is excluded as a marker.
    let marker = journal.switch_model(other());
    let switched = journal.build();
    let mut expected = lost.to_vec();
    expected.push((Part::Entry, ExclusionReason::ModelChangeMarker));
    assert_eq!(reasons(&switched.report), expected);
    assert_eq!(switched.report.excluded[2].entry_id, marker);
    assert_eq!(switched.report.identity.model, other());
    assert_eq!(switched.report.totals.opaque_provider_state_bytes, 0);
    // A preview back to the original model would make it portable again.
    let back = journal
        .build_with(&ContextConfig::default(), Some(&marker), Some(&mock()))
        .unwrap();
    assert_eq!(back.report.preview.unwrap().would_become_portable.len(), 2);
}

#[test]
fn superseded_and_nested_summaries_are_excluded_with_their_summary() {
    let mut journal = Journal::new();
    let t1 = journal.exchange("one", "1");
    let t2 = journal.exchange("two", "2");
    let t3 = journal.exchange("three", "3");
    let s1 = journal.compact(t1.to_vec(), vec![], "summary of one");
    let t4 = journal.exchange("four", "4");
    let t5 = journal.exchange("five", "5");
    let fork = journal.head();

    // Rolling replacement: S2 covers turns 1-3 and S1, superseding S1.
    let mut covered: Vec<_> = [t1.clone(), t2.clone(), t3.clone()].concat();
    covered.push(s1.clone());
    let s2 = journal.compact(covered, vec![s1.clone()], "summary of one to three");
    let built = journal.build();
    assert_eq!(
        included_ids(&built.report),
        [
            s2.clone(),
            t4[0].clone(),
            t4[1].clone(),
            t5[0].clone(),
            t5[1].clone()
        ]
        .map(Some)
    );
    for exclusion in &built.report.excluded {
        let expected = if exclusion.entry_id == s1 {
            ExclusionReason::SupersededSummary
        } else {
            ExclusionReason::CoveredBySummary
        };
        assert_eq!(exclusion.reason, expected);
        assert_eq!(exclusion.summary_id.as_ref(), Some(&s2));
    }
    assert_eq!(built.report.excluded.len(), 7);
    let summaries: Vec<_> = built.report.summaries.iter().map(|s| &s.entry_id).collect();
    assert_eq!(summaries, [&s2], "only S2 is active");
    assert_eq!(
        built.report.summaries[0].supersedes,
        std::slice::from_ref(&s1)
    );

    // On a sibling branch S2 does not exist, so S1 is active again.
    journal.navigate(fork);
    let t6 = journal.exchange("six", "6");
    // Nested: S3 covers turns 2-3 and S1's entry but none of S1's sources, so
    // it need not supersede S1. Turn 1 stays excluded by S1, S1 by S3.
    let mut covered: Vec<_> = [t2, t3].concat();
    covered.push(s1.clone());
    let s3 = journal.compact(covered, vec![], "summary of two, three and one");
    let nested = journal.build();
    assert_eq!(
        included_ids(&nested.report),
        [
            s3.clone(),
            t4[0].clone(),
            t4[1].clone(),
            t5[0].clone(),
            t5[1].clone(),
            t6[0].clone(),
            t6[1].clone()
        ]
        .map(Some)
    );
    let by: Vec<_> = nested
        .report
        .excluded
        .iter()
        .map(|e| (e.entry_id.clone(), e.summary_id.clone().unwrap()))
        .collect();
    assert_eq!(
        by[..2],
        [(t1[0].clone(), s1.clone()), (t1[1].clone(), s1.clone())]
    );
    assert!(by.contains(&(s1.clone(), s3.clone())));
    // Turn 1 names S1, which is itself covered: S3 stands for it in the
    // projection. Every other link already names a projected summary.
    let effective: Vec<_> = nested
        .report
        .excluded
        .iter()
        .filter_map(|e| Some((e.entry_id.clone(), e.effective_summary.clone()?)))
        .collect();
    assert_eq!(
        effective,
        [(t1[0].clone(), s3.clone()), (t1[1].clone(), s3.clone())]
    );
    assert!(
        built
            .report
            .excluded
            .iter()
            .all(|e| e.effective_summary.is_none())
    );
    assert!(
        !serde_json::to_string(&nested.report)
            .unwrap()
            .contains(s2.as_str())
    );
    let flags: Vec<_> = nested
        .report
        .summaries
        .iter()
        .map(|s| (s.entry_id.clone(), s.included))
        .collect();
    assert_eq!(flags, [(s1, false), (s3, true)]);
}

#[test]
fn instructions_come_first_and_count_like_any_item() {
    let mut journal = Journal::with_configuration(
        br#"{"instructions":[{"level":"system","text":"Be brief."},{"level":"developer","text":"Cite files."}]}"#,
    );
    journal.exchange("hi", "hello");
    let built = journal.build();
    let ContextItem::Instructions { instructions } = &built.projection.input.items[0] else {
        panic!("instructions are not first");
    };
    assert_eq!(instructions[1].level, InstructionLevel::Developer);
    assert_eq!(built.report.included[0].entry_id, None);
    assert_eq!(built.report.included[0].kind, ItemKind::Instructions);
    // "Be brief." and "Cite files." are 9 and 11 bytes: 3 + 3 tokens.
    assert_eq!(
        built.report.instructions,
        InstructionsReport::Included {
            count: 2,
            estimated_tokens: 6
        }
    );
    assert_eq!(built.report.totals.estimated_instructions, 6);

    assert_eq!(
        Journal::new().build().report.instructions,
        InstructionsReport::Absent
    );
    assert!(Journal::new().build().projection.input.items.is_empty());
    let bad = |bytes: &[u8]| {
        SessionConfiguration::parse(
            ArtifactRef {
                sha256: Digest::of(bytes),
                bytes: bytes.len() as u64,
            },
            bytes,
        )
        .unwrap_err()
    };
    assert_eq!(
        bad(br#"{"instructions":"text"}"#),
        ConfigurationError::InvalidInstructions
    );
    assert_eq!(
        bad(br#"{"instructions":[{"level":"user","text":"x"}]}"#),
        ConfigurationError::InvalidInstructions
    );
    assert_eq!(bad(b"[]"), ConfigurationError::NotAnObject);
    assert_eq!(bad(br#"{"a":1,"a":2}"#), ConfigurationError::NotAnObject);
}

#[test]
fn the_builder_refuses_unknown_heads_and_another_configuration() {
    let journal = Journal::new();
    let config = ContextConfig::default();
    assert_eq!(
        journal.build_with(&config, Some(&EntryId::new()), None),
        Err(ContextError::UnknownHead)
    );
    let elsewhere = Journal::with_configuration(br#"{"policy":"other"}"#);
    let snapshot = journal.snapshot();
    assert_eq!(
        ContextBuilder::new(&config, &elsewhere.configuration).build(&snapshot, None, None),
        Err(ContextError::ConfigurationMismatch)
    );
    let invalid = ContextConfig {
        compaction_trigger_permille: 1001,
        ..ContextConfig::default()
    };
    assert_eq!(
        journal.build_with(&invalid, None, None),
        Err(ContextError::InvalidConfig("compaction_trigger_permille"))
    );
}

#[test]
fn budget_is_unknown_without_a_limit_and_flags_overflow_and_compaction() {
    let mut journal = Journal::new();
    // 400 bytes of text: 100 tokens, plus 1 for "ok".
    journal.exchange(&"x".repeat(400), "ok");
    let head = journal.head();
    let estimate = journal.build().report.totals.estimated_total;
    assert_eq!(estimate, 101);

    let unknown = journal.build().report;
    assert_eq!(
        unknown.budget,
        Budget::Unknown {
            reason: UnknownLimit::ModelNotInCatalog,
            limits: None,
            deductions: Deductions {
                output_reserve: 4096,
                tool_headroom: 8192,
                safety_margin: 4096
            },
            estimated_tokens: 101,
            unestimated_opaque_bytes: 0,
        }
    );
    assert!(!unknown.flags.overflow && !unknown.flags.compaction_recommended);
    let unconfigured = journal
        .build_with(&config_with_limit(&mock(), None), head.as_ref(), None)
        .unwrap();
    assert!(matches!(
        unconfigured.report.budget,
        Budget::Unknown {
            reason: UnknownLimit::ContextLimitNotConfigured,
            ..
        }
    ));

    let budget = |context: u32| {
        let config = ContextConfig {
            output_reserve: 10,
            tool_headroom: 5,
            safety_margin: 5,
            ..config_with_limit(&mock(), Some(context))
        };
        journal
            .build_with(&config, head.as_ref(), None)
            .unwrap()
            .report
    };
    // usable 100: one token over.
    let over = budget(120);
    assert!(over.flags.overflow && over.flags.compaction_recommended);
    assert!(matches!(
        over.budget,
        Budget::Known {
            usable_tokens: 100,
            overflow_tokens: 1,
            headroom_tokens: 0,
            ..
        }
    ));
    // usable 101: fits exactly, at or above 85%.
    let exact = budget(121);
    assert!(!exact.flags.overflow && exact.flags.compaction_recommended);
    // usable 119: threshold ceil(119 * 0.85) = 102, so 101 is below it.
    let below = budget(139);
    assert!(!below.flags.overflow && !below.flags.compaction_recommended);
    assert!(matches!(
        below.budget,
        Budget::Known {
            compaction_threshold_tokens: 102,
            headroom_tokens: 18,
            ..
        }
    ));
    // Deductions larger than the limit leave nothing usable.
    assert!(matches!(
        budget(15).budget,
        Budget::Known {
            usable_tokens: 0,
            ..
        }
    ));
}

#[test]
fn reported_usage_is_labelled_and_never_added_to_estimates() {
    let mut journal = Journal::new();
    journal.user(vec![text("count me")]);
    let usage = Usage {
        input_tokens: Some(1234),
        output_tokens: Some(56),
        cached_input_tokens: None,
        reasoning_tokens: None,
    };
    let response = journal.respond(Response {
        usage: Some(usage.clone()),
        ..Response::completed(vec![text("counted")])
    });
    let report = journal.build().report;
    assert_eq!(
        report.reported_usage,
        Some(ReportedUsage {
            basis: UsageBasis::ProviderReported,
            entry_id: response,
            response_id: match &journal
                .snapshot()
                .entry(journal.head().as_ref().unwrap())
                .unwrap()
                .entry
                .payload
            {
                EntryPayload::AssistantResponse(r) => r.response_id.clone(),
                _ => unreachable!(),
            },
            model: mock(),
            usage,
        })
    );
    // "count me" and "counted": 2 + 2 estimated tokens; 1234 is not in them.
    assert_eq!(report.totals.estimated_total, 4);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["reported_usage"]["basis"], "provider_reported");

    // A newer completed response without usage replaces it: older figures
    // describe an older request.
    journal.exchange("again", "no usage");
    assert_eq!(journal.build().report.reported_usage, None);
}

#[test]
fn media_blocks_use_flat_estimates() {
    let mut journal = Journal::new();
    let image = journal.root.put_artifact(b"png").unwrap();
    journal.user(vec![
        text("look"),
        ContentBlock::Image {
            artifact: image.clone(),
            mime_type: "image/png".into(),
        },
        ContentBlock::File {
            artifact: image,
            mime_type: "text/plain".into(),
            name: "a.txt".into(),
        },
    ]);
    journal.respond(Response::completed(vec![text("seen")]));
    let totals = journal.build().report.totals;
    assert_eq!(totals.estimated_media, 1600 + 4000);
    assert_eq!(totals.estimated_user, 1);
    assert_eq!(totals.estimated_total, 1 + 1600 + 4000 + 1);
}

/// v2 added fields the model never sees: the same entries written as v1 or
/// as v2 build byte-identical reports, fingerprints and items.
#[test]
fn entries_written_before_v3_project_identically() {
    let mut journal = Journal::new();
    journal.user(vec![text("use tools")]);
    let first = CallId::new();
    journal.respond(
        Response::completed(vec![text("Let me look."), call(&first, "read")])
            .finish(FinishReason::ToolCalls),
    );
    journal.answer();
    journal.respond(Response::completed(vec![text("Done.")]));
    let session = journal.snapshot().session_id().clone();
    let path = journal
        .root
        .path()
        .join("sessions")
        .join(format!("{session}.jsonl"));
    let v3: Vec<Value> = fs::read(path)
        .unwrap()
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_slice::<Value>(l).unwrap()["record"].clone())
        .collect();
    assert!(v3.iter().all(|r| r["version"] == 3));
    // Give the responses a fingerprint too, so every v2 field is set.
    let mut with_fingerprints = v3.clone();
    let mut v1 = v3.clone();
    let mut v2 = v3.clone();
    for record in &mut v2 {
        record["version"] = json!(2);
    }
    for (v2, v1) in with_fingerprints.iter_mut().zip(&mut v1) {
        v1["version"] = json!(1);
        let (Some(Value::Array(new)), Some(Value::Array(old))) = (
            v2.pointer_mut("/payload/entries"),
            v1.pointer_mut("/payload/entries"),
        ) else {
            continue;
        };
        for (new, old) in new.iter_mut().zip(old) {
            let (new, old) = (&mut new["payload"], &mut old["payload"]);
            match new["kind"].as_str().unwrap() {
                "assistant_response" => {
                    new["projection_fingerprint"] = json!(Digest::of(b"some projection"));
                    old.as_object_mut()
                        .unwrap()
                        .remove("projection_fingerprint");
                }
                "tool_result" => {
                    assert_eq!(new["result"]["metadata"]["code"], "tools_unavailable");
                    let metadata = old["result"]["metadata"].as_object_mut().unwrap();
                    metadata.remove("code").unwrap();
                }
                _ => {}
            }
        }
    }
    let build = |records: &[Value]| {
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::open(temp.path().join("state"), true).unwrap();
        root.put_artifact(br#"{"policy":"context-test"}"#).unwrap();
        let mut previous = Value::Null;
        let mut bytes = Vec::new();
        for record in records {
            let mut record = record.clone();
            record["previous_checksum"] = previous;
            let raw = serde_json::to_string(&record).unwrap();
            let digest = Digest::of(raw.as_bytes());
            bytes.extend(format!("{{\"record\":{raw},\"sha256\":\"{digest}\"}}\n").bytes());
            previous = json!(digest);
        }
        let mut file = root.journal(&session, true, true).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
        let inspection = JournalStore::inspect(&root, &session).unwrap();
        assert_eq!(inspection.diagnostic, None);
        let snapshot = inspection.snapshot.unwrap();
        ContextBuilder::new(&ContextConfig::default(), &journal.configuration)
            .build(&snapshot, snapshot.head(), None)
            .unwrap()
    };
    let built = journal.build();
    for records in [&v3, &v2, &with_fingerprints, &v1] {
        let rebuilt = build(records);
        assert_eq!(
            serde_json::to_string_pretty(&rebuilt.report).unwrap(),
            serde_json::to_string_pretty(&built.report).unwrap()
        );
        assert_eq!(rebuilt.projection, built.projection);
    }
    // The v2 code is stored, but not projected.
    let ContextItem::ToolResult { result, .. } = &built.projection.input.items[2] else {
        panic!("the third item is the result")
    };
    assert_eq!(result.metadata.code, None);
}

#[test]
fn invariant_checks_reject_incomplete_foreign_and_unpaired_reports() {
    let mut journal = Journal::new();
    journal.user(vec![text("use tools")]);
    let first = CallId::new();
    let proposal = journal
        .respond(Response::completed(vec![call(&first, "read")]).finish(FinishReason::ToolCalls));
    journal.answer();
    journal.respond(Response::completed(vec![
        ContentBlock::ReasoningSummary { text: "hm".into() },
        text("done"),
    ]));
    let snapshot = journal.snapshot();
    let branch = snapshot.ancestry(snapshot.head()).unwrap();
    let built = journal.build();
    let items = &built.projection.input.items;
    let applied = super::builder::Controls::fold(&branch);
    let check = |items: &[ContextItem], report: &ContextReport| {
        super::check::check(
            &branch,
            &crate::model::ModelInput {
                items: items.to_vec(),
                ..built.projection.input.clone()
            },
            report,
            &applied,
        )
    };
    assert_eq!(check(items, &built.report), Ok(()));

    let mut dropped = built.report.clone();
    dropped.excluded.clear();
    assert_eq!(check(items, &dropped), Err("incomplete_part_decisions"));

    let mut twice = built.report.clone();
    twice.excluded.push(Exclusion {
        entry_id: proposal.clone(),
        part: Part::Entry,
        reason: ExclusionReason::FailedResponse,
        summary_id: None,
        effective_summary: None,
    });
    assert_eq!(check(items, &twice), Err("duplicate_decision"));

    let mut foreign = built.report.clone();
    foreign.excluded.push(Exclusion {
        entry_id: EntryId::new(),
        part: Part::Entry,
        reason: ExclusionReason::ModelChangeMarker,
        summary_id: None,
        effective_summary: None,
    });
    assert_eq!(check(items, &foreign), Err("entry_from_other_branch"));

    let mut unlinked = built.report.clone();
    unlinked.excluded[0].summary_id = Some(proposal.clone());
    assert_eq!(check(items, &unlinked), Err("invalid_exclusion"));
    let mut effective = built.report.clone();
    effective.excluded[0].effective_summary = Some(proposal.clone());
    assert_eq!(check(items, &effective), Err("invalid_exclusion"));

    // An assistant item with nothing in it is never emitted.
    let mut hollow_items = items.clone();
    let mut hollow = built.report.clone();
    let ContextItem::Assistant { content, .. } = hollow_items.last_mut().unwrap() else {
        panic!("the last item is the final response");
    };
    content.clear();
    hollow.included.last_mut().unwrap().blocks.clear();
    assert_eq!(check(&hollow_items, &hollow), Err("empty_assistant_item"));

    // A result's provenance is the stored one, in the item and the report.
    assert_eq!(
        built.report.included[2].provenance,
        Some(Provenance::Runtime)
    );
    let mut relabelled = items.clone();
    let ContextItem::ToolResult { provenance, .. } = &mut relabelled[2] else {
        panic!("the third item is the result");
    };
    *provenance = Provenance::UserObserved;
    assert_eq!(
        check(&relabelled, &built.report),
        Err("tool_result_provenance_mismatch")
    );
    let mut unlabelled = built.report.clone();
    unlabelled.included[2].provenance = None;
    assert_eq!(
        check(items, &unlabelled),
        Err("report_disagrees_with_projection")
    );

    // Remove the result item and its record: the call is left unanswered.
    let mut unpaired_items = items.clone();
    let mut unpaired = built.report.clone();
    unpaired_items.remove(2);
    unpaired.included.remove(2);
    assert_eq!(
        check(&unpaired_items, &unpaired),
        Err("entry_without_decision")
    );
    assert_eq!(
        super::check::paired(&unpaired_items),
        Err("tool_call_without_result")
    );
    // Move the result before its call.
    let mut reordered = items.clone();
    reordered.swap(1, 2);
    assert_eq!(
        super::check::paired(&reordered),
        Err("tool_result_without_call")
    );
}

/// Opt-in: build a 10,000-entry branch and report the build time and the
/// report's size. Run with
/// `cargo test --release -p harness-core context_build_benchmark -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark"]
fn context_build_benchmark() {
    const TURNS: usize = 2_500;
    let mut journal = Journal::new();
    crate::session::state::without_oracle(|| {
        let started = std::time::Instant::now();
        for n in 0..TURNS {
            journal.user(vec![text(&format!(
                "Request {n}: {}",
                "lorem ipsum ".repeat(16)
            ))]);
            journal.respond(
                Response::completed(vec![text("Let me check."), call(&CallId::new(), "read")])
                    .finish(FinishReason::ToolCalls),
            );
            journal.answer();
            journal.respond(Response::completed(vec![text(&format!(
                "Answer {n}: {}",
                "dolor sit amet ".repeat(12)
            ))]));
        }
        eprintln!("appended {} turns in {:?}", TURNS, started.elapsed());
    });
    let snapshot = journal.snapshot();
    let branch = snapshot.ancestry(snapshot.head()).unwrap().len();
    assert_eq!(branch, TURNS * 4);
    let config = config_with_limit(&mock(), Some(10_000_000));
    let builder = ContextBuilder::new(&config, &journal.configuration);
    let mut times = Vec::new();
    let mut built = None;
    for _ in 0..21 {
        let started = std::time::Instant::now();
        built = Some(builder.build(&snapshot, snapshot.head(), None).unwrap());
        times.push(started.elapsed());
    }
    times.sort();
    let built = built.unwrap();
    let pretty = serde_json::to_vec_pretty(&built.report).unwrap().len();
    let compact = serde_json::to_vec(&built.report).unwrap().len();
    let time = |f: &dyn Fn()| {
        let started = std::time::Instant::now();
        for _ in 0..21 {
            f();
        }
        started.elapsed() / 21
    };
    let ancestry = time(&|| {
        std::hint::black_box(snapshot.ancestry(snapshot.head()).unwrap());
    });
    let input = &built.projection.input;
    let hashing = time(&|| {
        std::hint::black_box(fingerprint(input));
    });
    let branch_entries = snapshot.ancestry(snapshot.head()).unwrap();
    let applied = super::builder::Controls::fold(&branch_entries);
    let checking = time(&|| {
        super::check::check(&branch_entries, input, &built.report, &applied).unwrap();
    });
    eprintln!(
        "context build over {branch} entries: median {:?}, min {:?}, max {:?} \
         (ancestry {ancestry:?}, fingerprint {hashing:?}, checks {checking:?}); {} items, \
         {} exclusions; report {compact} bytes \
         compact, {pretty} bytes pretty; estimated {} tokens",
        times[times.len() / 2],
        times[0],
        times[times.len() - 1],
        built.report.included.len(),
        built.report.excluded.len(),
        built.report.totals.estimated_total,
    );
}

// M4: continuation validity is a logical-input rule, independent of I/O.
fn native_journal() -> (Journal, EntryId) {
    let mut journal = Journal::new();
    journal.user(vec![text("hello")]);
    let artifact = journal.root.put_artifact(b"continuation").unwrap();
    let response = journal.respond(Response {
        continuation: Some(artifact),
        ..Response::completed(vec![text("answer")])
    });
    assert!(journal.build().projection.input.continuation.is_some());
    (journal, response)
}

fn replace_configuration(journal: &mut Journal, bytes: &[u8]) {
    let artifact = journal.root.put_artifact(bytes).unwrap();
    journal.append(
        vec![],
        vec![JournalEvent::ConfigurationChanged(ConfigurationChanged {
            configuration: artifact.clone(),
            policy_revision: "changed".into(),
        })],
    );
    journal.configuration = SessionConfiguration::load(&journal.root, &artifact).unwrap();
}

fn prefix_mismatch(built: &BuiltContext) {
    assert!(built.projection.input.continuation.is_none());
    assert!(
        built
            .report
            .excluded
            .iter()
            .any(|e| e.part == Part::Continuation
                && e.reason == ExclusionReason::ContinuationPrefixMismatch)
    );
}

#[test]
fn a_pin_marks_its_turn_without_changing_the_projection() {
    let mut journal = Journal::new();
    let first = journal.exchange("one", "first");
    let turn = journal.turn_of(&first[0]);
    let before = journal.build();
    assert!(!before.report.included.iter().any(|item| item.pinned));
    assert!(before.report.controls.is_empty());

    journal.control(&turn, ContextMode::Pinned);
    let pinned = journal.build();
    // Pinning changes what the report says, not what is sent.
    assert_eq!(pinned.projection.input.items, before.projection.input.items);
    assert_eq!(pinned.projection.fingerprint, before.projection.fingerprint);
    assert!(pinned.report.included.iter().all(|item| item.pinned));
    let [control] = &pinned.report.controls[..] else {
        panic!("one effective control")
    };
    assert_eq!(control.turn_id, turn);
    assert_eq!(control.mode, ContextMode::Pinned);
    assert!(matches!(
        pinned.report.excluded.as_slice(),
        [Exclusion {
            reason: ExclusionReason::ContextControlMarker,
            part: Part::Entry,
            summary_id: None,
            effective_summary: None,
            ..
        }]
    ));

    // A reset is another control, and restores the original bytes exactly.
    journal.control(&turn, ContextMode::Default);
    let reset = journal.build();
    assert_eq!(reset.projection.input, before.projection.input);
    assert_eq!(reset.projection.fingerprint, before.projection.fingerprint);
    assert!(!reset.report.included.iter().any(|item| item.pinned));
    assert!(reset.report.controls.is_empty());
}

#[test]
fn controls_apply_to_whole_turns_of_the_selected_ancestry() {
    let mut journal = Journal::new();
    let first = journal.exchange("one", "first");
    let second = journal.exchange("two", "second");
    let third = journal.exchange("three", "third");
    let (a, b, c) = (
        journal.turn_of(&first[0]),
        journal.turn_of(&second[0]),
        journal.turn_of(&third[0]),
    );
    let pin_a = journal.control(&a, ContextMode::Pinned);
    let exclude_b = journal.control(&b, ContextMode::Excluded);
    journal.control(&c, ContextMode::Pinned);
    journal.control(&c, ContextMode::Default);
    let built = journal.build();
    // Only turns A and C are sent; B's user message and response are both out.
    assert_eq!(
        included_ids(&built.report),
        [
            Some(first[0].clone()),
            Some(first[1].clone()),
            Some(third[0].clone()),
            Some(third[1].clone())
        ]
    );
    assert_eq!(
        built
            .report
            .included
            .iter()
            .map(|item| item.pinned)
            .collect::<Vec<_>>(),
        [true, true, false, false]
    );
    // Every entry of the excluded turn is out, as one entry-level decision.
    let decisions: Vec<_> = built
        .report
        .excluded
        .iter()
        .filter(|e| [&second[0], &second[1]].contains(&&e.entry_id))
        .map(|e| (e.entry_id.clone(), e.part, e.reason))
        .collect();
    assert_eq!(
        decisions,
        [
            (
                second[0].clone(),
                Part::Entry,
                ExclusionReason::UserExcluded
            ),
            (
                second[1].clone(),
                Part::Entry,
                ExclusionReason::UserExcluded
            ),
        ]
    );
    assert_eq!(
        built.report.controls,
        [
            ControlReport {
                turn_id: a.clone(),
                mode: ContextMode::Pinned,
                control_entry_id: pin_a,
            },
            ControlReport {
                turn_id: b.clone(),
                mode: ContextMode::Excluded,
                control_entry_id: exclude_b,
            }
        ]
    );
    // The pinned turn is included, and the reset control leaves no trace in
    // the effective summary.
    assert!(built.report.controls.iter().all(|item| item.turn_id != c));
}

#[test]
fn a_control_on_a_sibling_branch_never_applies() {
    let mut journal = Journal::new();
    let first = journal.exchange("one", "first");
    let turn = journal.turn_of(&first[0]);
    let user = first[0].clone();
    // A sibling branch forked before the control. Only the shared user entry
    // of the pinned turn is on it.
    journal.navigate(Some(user.clone()));
    journal.exchange("sibling", "reply");
    journal.control(&turn, ContextMode::Pinned);
    let sibling = journal.build();
    assert_eq!(
        sibling
            .report
            .included
            .iter()
            .map(|item| (item.entry_id.clone(), item.pinned))
            .collect::<Vec<_>>(),
        [
            (Some(user.clone()), true),
            (
                Some(sibling.report.included[1].entry_id.clone().unwrap()),
                false
            ),
            (
                Some(sibling.report.included[2].entry_id.clone().unwrap()),
                false
            )
        ]
    );
    assert_eq!(sibling.report.controls.len(), 1);
    // The first branch does not see it.
    journal.navigate(Some(first[1].clone()));
    let built = journal.build();
    assert!(!built.report.included.iter().any(|item| item.pinned));
    assert!(built.report.controls.is_empty());
    assert!(
        !built
            .report
            .excluded
            .iter()
            .any(|e| e.reason == ExclusionReason::ContextControlMarker)
    );
}

#[test]
fn excluding_a_turn_never_splits_a_tool_group() {
    let mut journal = Journal::new();
    journal.user(vec![text("use tools")]);
    let call_id = CallId::new();
    journal.respond(
        Response::completed(vec![text("Let me look."), call(&call_id, "read")])
            .finish(FinishReason::ToolCalls),
    );
    journal.answer();
    journal.respond(Response::completed(vec![text("Done.")]));
    let turn = journal.turn_of(&journal.snapshot().head().unwrap().clone());
    journal.control(&turn, ContextMode::Excluded);
    let built = journal.build();
    assert!(
        built.projection.input.items.is_empty(),
        "the whole turn is out"
    );
    let reasons = reasons(&built.report);
    assert!(
        reasons
            .iter()
            .any(|(part, reason)| *part == Part::Entry && *reason == ExclusionReason::UserExcluded)
    );
    // The proposal, its result and the final response are all excluded as the
    // user's choice; no orphan-call reason is invented.
    assert!(!reasons.iter().any(|(_, reason)| matches!(
        reason,
        ExclusionReason::OrphanToolCall
            | ExclusionReason::FailedResponse
            | ExclusionReason::EmptyResponse
    )));
    assert_eq!(
        reasons
            .iter()
            .filter(|(_, reason)| *reason == ExclusionReason::UserExcluded)
            .count(),
        4
    );
}

#[test]
fn excluding_a_turn_invalidates_a_continuation_bound_to_the_old_prefix() {
    let mut journal = Journal::new();
    let first = journal.exchange("one", "first");
    let turn = journal.turn_of(&first[0]);
    journal.user(vec![text("two")]);
    let artifact = journal.root.put_artifact(b"continuation").unwrap();
    journal.respond(Response {
        continuation: Some(artifact),
        ..Response::completed(vec![text("second")])
    });
    assert!(journal.build().projection.input.continuation.is_some());
    journal.control(&turn, ContextMode::Excluded);
    let built = journal.build();
    // The control is folded over the head's ancestry, so the rebuilt prefix
    // no longer matches. The existing rule drops the continuation; no
    // control-specific branch exists.
    prefix_mismatch(&built);
    assert!(
        !built
            .projection
            .input
            .items
            .iter()
            .any(|item| item.entry_id() == Some(&first[0]))
    );
}

#[test]
fn continuation_changed_instructions_invalidates_rebuilt_prefix() {
    let (mut journal, _) = native_journal();
    replace_configuration(
        &mut journal,
        br#"{"instructions":[{"level":"system","text":"new rules"}]}"#,
    );
    prefix_mismatch(&journal.build());
}

#[test]
fn continuation_changed_tool_set_invalidates_rebuilt_prefix() {
    let (journal, _) = native_journal();
    let tools: Vec<_> = crate::tools::Registry::builtin()
        .definitions()
        .cloned()
        .collect();
    assert!(!tools.is_empty());
    let snapshot = journal.snapshot();
    let config = ContextConfig::default();
    let built = ContextBuilder::new(&config, &journal.configuration)
        .with_tools(&tools)
        .build(&snapshot, snapshot.head(), None)
        .unwrap();
    assert_eq!(built.projection.input.tools, tools);
    prefix_mismatch(&built);
}

#[test]
fn continuation_changed_options_invalidates_rebuilt_prefix() {
    for bytes in [
        br#"{"inference":{"reasoning_effort":"high"}}"#.as_slice(),
        br#"{"inference":{"provider_options":{"mock":{"required_mode":"native"}}}}"#,
    ] {
        let (mut journal, _) = native_journal();
        replace_configuration(&mut journal, bytes);
        prefix_mismatch(&journal.build());
    }
    let (journal, _) = native_journal();
    let config = ContextConfig {
        output_reserve: 123,
        ..ContextConfig::default()
    };
    let built = journal
        .build_with(&config, journal.head().as_ref(), None)
        .unwrap();
    assert_eq!(built.projection.input.options.max_output_tokens, 123);
    prefix_mismatch(&built);
}

#[test]
fn continuation_branch_change_rejects_a_handle_bound_to_another_prefix() {
    let mut journal = Journal::new();
    journal.user(vec![text("branch A")]);
    let fingerprint_a = journal.build().projection.fingerprint;
    journal.respond(Response::completed(vec![text("A")]));
    journal.navigate(None);
    journal.user(vec![text("branch B")]);
    let artifact = journal
        .root
        .put_artifact(b"copied handle from branch A")
        .unwrap();
    journal.respond(Response {
        continuation: Some(artifact),
        input_fingerprint: Some(fingerprint_a),
        ..Response::completed(vec![text("B")])
    });
    prefix_mismatch(&journal.build());
}

#[test]
fn continuation_model_switch_later_response_and_other_branch_omit_it() {
    let (mut journal, response) = native_journal();
    journal.user(vec![text("next")]);
    assert!(journal.build().projection.input.continuation.is_some());
    journal.respond(Response::completed(vec![text("no handle")]));
    let built = journal.build();
    assert!(built.projection.input.continuation.is_none());
    assert!(
        built
            .report
            .excluded
            .iter()
            .any(|e| e.reason == ExclusionReason::ContinuationNotLatest)
    );
    journal.navigate(Some(response));
    journal.switch_model(other());
    let built = journal.build();
    assert!(built.projection.input.continuation.is_none());
    assert!(
        built
            .report
            .excluded
            .iter()
            .any(|e| e.reason == ExclusionReason::ProviderStateNotPortable)
    );
    journal.navigate(None);
    journal.user(vec![text("unrelated branch")]);
    assert!(journal.build().projection.input.continuation.is_none());
    journal.respond(Response::completed(vec![text("done")]));
}

#[test]
fn fingerprint_excludes_continuation_but_covers_options_and_tool_definitions() {
    let (journal, _) = native_journal();
    let mut input = journal.build().projection.input;
    let before = fingerprint(&input);
    input.continuation = None;
    assert_eq!(before, fingerprint(&input));
    input.options.max_output_tokens += 1;
    assert_ne!(before, fingerprint(&input));
    input.options.max_output_tokens -= 1;
    input.tools = crate::tools::Registry::builtin()
        .definitions()
        .cloned()
        .collect();
    assert_ne!(before, fingerprint(&input));
    let tools_fingerprint = fingerprint(&input);
    input.tools[0].description.push_str("changed");
    assert_ne!(tools_fingerprint, fingerprint(&input));
}

#[test]
fn inference_configuration_is_shape_checked_and_passed_through() {
    for value in [
        json!(null),
        json!([]),
        json!(true),
        json!({"reasoning_effort":"extreme"}),
        json!({"max_output_tokens":42}),
        json!({"provider_options":null}),
        json!({"provider_options":[]}),
        json!({"provider_options":{"mock":false}}),
        json!({"provider_options":{"":{}}}),
        json!({"unknown":1}),
    ] {
        let bytes = serde_json::to_vec(&json!({"inference":value})).unwrap();
        let reference = ArtifactRef {
            sha256: Digest::of(&bytes),
            bytes: bytes.len() as u64,
        };
        assert_eq!(
            SessionConfiguration::parse(reference, &bytes).unwrap_err(),
            ConfigurationError::InvalidInference
        );
    }
    let journal = Journal::with_configuration(br#"{"inference":{"reasoning_effort":"medium","provider_options":{"mock":{"required":true}}}}"#);
    let input = journal.build().projection.input;
    assert_eq!(
        input.options.max_output_tokens,
        ContextConfig::default().output_reserve
    );
    assert_eq!(
        input.options.reasoning_effort,
        Some(crate::model::ReasoningEffort::Medium)
    );
    assert_eq!(
        input.options.provider_options,
        json!({"mock":{"required":true}})
            .as_object()
            .unwrap()
            .clone()
    );
}

#[test]
fn continuation_only_response_replays_and_invalid_one_is_omitted() {
    let mut journal = Journal::new();
    journal.user(vec![text("hello")]);
    let artifact = journal.root.put_artifact(b"handle").unwrap();
    let response = journal.respond(Response {
        continuation: Some(artifact),
        ..Response::completed(vec![])
    });
    let built = journal.build();
    assert!(built.projection.input.continuation.is_some());
    assert!(
        matches!(&built.projection.input.items[1], ContextItem::Assistant { content, .. } if content.is_empty())
    );
    assert_eq!(built.report.included[1].blocks[0].part, Part::Continuation);
    replace_configuration(
        &mut journal,
        br#"{"inference":{"reasoning_effort":"high"}}"#,
    );
    let built = journal.build();
    prefix_mismatch(&built);
    assert!(
        !built
            .projection
            .input
            .items
            .iter()
            .any(|i| i.entry_id() == Some(&response))
    );
}

#[test]
fn continuation_prefix_rebuild_includes_an_earlier_continuation_only_response() {
    let mut journal = Journal::new();
    journal.user(vec![text("hello")]);
    let artifact = journal.root.put_artifact(b"handle").unwrap();
    let first = journal.respond(Response {
        continuation: Some(artifact.clone()),
        ..Response::completed(vec![])
    });
    journal.user(vec![text("next")]);
    let prefix = journal.build().projection.fingerprint;
    let second = journal.respond(Response {
        continuation: Some(artifact),
        ..Response::completed(vec![])
    });
    let built = journal.build();
    assert_eq!(
        built
            .projection
            .input
            .continuation
            .as_ref()
            .unwrap()
            .input_fingerprint,
        prefix
    );
    assert!(
        !built
            .projection
            .input
            .items
            .iter()
            .any(|i| i.entry_id() == Some(&first))
    );
    assert!(
        built
            .projection
            .input
            .items
            .iter()
            .any(|i| i.entry_id() == Some(&second))
    );
    assert!(
        built
            .report
            .excluded
            .iter()
            .any(|e| e.entry_id == first && e.reason == ExclusionReason::ContinuationNotLatest)
    );
}
