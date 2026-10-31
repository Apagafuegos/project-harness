//! Preview production against the real storage oracle and scripted model seam.
use super::*;
use crate::{
    context::compaction::*,
    model::{scripted::*, *},
    session::ContextMode,
};

#[tokio::test]
async fn multiblock_summary_discards_generated_reasoning_and_native_state() {
    let mut journal = prepared();
    let split = SUMMARY.find("## Files").unwrap();
    let provider = Arc::new(ScriptedProvider::new([Script::new([
        Step::Event(ProviderEvent::Started),
        Step::Event(ProviderEvent::ReasoningSummaryDelta {
            block_index: 0,
            text: "not summary text".into(),
        }),
        Step::text(2, &SUMMARY[split..]),
        Step::text(1, &SUMMARY[..split]),
        Step::provider_state(3, "summary.native", b"discard this".to_vec()),
        Step::completed_with_continuation(
            FinishReason::Stop,
            NativeContinuation {
                schema: "summary.handle".into(),
                payload: b"discard handle".to_vec(),
            },
        ),
    ])]));
    let proposed = RollingSummary::new(provider)
        .propose(
            input(journal.snapshot(), journal.configuration.clone()),
            OperationControl::default(),
        )
        .await
        .unwrap();
    let EntryPayload::Compaction(c) = &proposed.entry.payload else {
        panic!()
    };
    assert_eq!(c.summary, SUMMARY);
    assert_accepted(&mut journal, &proposed);
}

#[tokio::test]
async fn storage_acceptance_whole_tool_turn_and_scoped_media() {
    let mut journal = Journal::with_configuration(POLICY);
    let artifact = journal.root.put_artifact(b"media bytes").unwrap();
    journal.user(vec![
        text("inspect this file"),
        ContentBlock::File {
            artifact: artifact.clone(),
            name: "fixture.txt".into(),
            mime_type: "text/plain".into(),
        },
    ]);
    journal.respond(Response {
        finish: Some(FinishReason::ToolCalls),
        ..Response::completed(vec![call(&CallId::new(), "read")])
    });
    journal.answer();
    journal.respond(Response::completed(vec![text("inspected")]));
    for _ in 0..2 {
        journal.exchange("recent", "done");
    }
    let mut request_input = input(journal.snapshot(), journal.configuration.clone());
    let built = ContextBuilder::new(&request_input.context, &request_input.configuration)
        .build(&request_input.snapshot, request_input.head.as_ref(), None)
        .unwrap();
    let media = artifact.clone();
    request_input.artifacts = ArtifactSource::new(&built.projection.input, move |reference| {
        let valid = reference == media;
        Box::pin(async move {
            if valid {
                Ok(b"media bytes".to_vec())
            } else {
                Err(ArtifactReadError::Unavailable)
            }
        })
    });
    let provider = Arc::new(ScriptedProvider::new([script(SUMMARY)]));
    let proposed = RollingSummary::new(provider.clone())
        .propose(request_input, OperationControl::default())
        .await
        .unwrap();
    let streams = provider.streams();
    let request = &streams[0].request;
    assert_eq!(
        request.artifacts.references(),
        std::slice::from_ref(&artifact)
    );
    assert_eq!(
        request.artifacts.read(&artifact).await.unwrap(),
        b"media bytes"
    );
    assert!(
        matches!(&request.input.items[2], ContextItem::Assistant { content, .. } if matches!(&content[0], ContentBlock::ToolCall { name, .. } if name == "read"))
    );
    assert!(
        matches!(&request.input.items[3], ContextItem::ToolResult { result, .. } if result.status == ToolStatus::Denied && result.metadata.provenance == Provenance::Runtime)
    );
    let EntryPayload::Compaction(c) = &proposed.entry.payload else {
        panic!()
    };
    assert_eq!(c.covered_entry_ids.len(), 4);
    assert_accepted(&mut journal, &proposed);
}

const SUMMARY: &str = "## Task and constraints\nKeep the task.\n## Decisions\nUse Rust.\n## Files and actions\nREADME.md inspected; read succeeded.\n## Outcomes\nDone.\n## Unresolved work\nNone.\n";
const POLICY: &[u8] = br#"{"instructions":[{"level":"system","text":"session instruction"}],"compaction":{"summary_model":{"provider_instance":"summary","model":"cheap"}}}"#;

fn input(snapshot: SessionSnapshot, configuration: SessionConfiguration) -> CompactionInput {
    CompactionInput {
        head: snapshot.head().cloned(),
        snapshot,
        configuration,
        context: config_with_limit(&mock(), Some(128000)),
        tools: vec![],
        entry_id: EntryId::new(),
        response_id: ResponseId::new(),
        artifacts: ArtifactSource::default(),
    }
}
fn prepared() -> Journal {
    let mut journal = Journal::with_configuration(POLICY);
    for _ in 0..4 {
        journal.exchange("task", "done");
    }
    journal
}
fn script(summary: &str) -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, summary),
        Step::completed(FinishReason::Stop),
    ])
}
async fn preview(journal: &Journal) -> CompactionPreview {
    RollingSummary::new(Arc::new(ScriptedProvider::new([script(SUMMARY)])))
        .propose(
            input(journal.snapshot(), journal.configuration.clone()),
            OperationControl::default(),
        )
        .await
        .unwrap()
}
fn commit_preview(
    store: &mut JournalStore,
    root: &DataRoot,
    preview: &CompactionPreview,
) -> Result<CommitAck, StoreError> {
    let manifest = root.put_artifact(&preview.manifest_bytes).unwrap();
    let EntryPayload::Compaction(c) = &preview.entry.payload else {
        panic!()
    };
    assert_eq!(manifest, c.source_manifest);
    let snapshot = store.snapshot();
    store.append(
        Commit {
            command_id: None,
            command_fingerprint: None,
            head: HeadChange {
                from: snapshot.head().cloned(),
                to: Some(preview.entry.id.clone()),
                reason: HeadReason::Advance,
            },
            entries: vec![preview.entry.clone()],
            events: vec![],
        },
        at(),
    )
}
fn assert_accepted(journal: &mut Journal, preview: &CompactionPreview) {
    assert_accepted_with_tools(journal, preview, &[]);
}
fn assert_accepted_with_tools(
    journal: &mut Journal,
    preview: &CompactionPreview,
    tools: &[crate::tools::ToolDefinition],
) {
    commit_preview(&mut journal.store, &journal.root, preview).unwrap();
    assert_eq!(
        journal.snapshot().entry(&preview.entry.id).unwrap().entry,
        preview.entry
    );
    let actual = ContextBuilder::new(
        &input(journal.snapshot(), journal.configuration.clone()).context,
        &journal.configuration,
    )
    .with_tools(tools)
    .build(&journal.snapshot(), journal.snapshot().head(), None)
    .unwrap();
    let mut expected = preview.after.clone();
    expected.identity.journal_sequence = journal.snapshot().sequence();
    assert_eq!(actual.report, expected);
    assert!(
        matches!(actual.projection.input.items.iter().find(|i| matches!(i, ContextItem::Summary { .. })), Some(ContextItem::Summary { entry_id, summary }) if entry_id == &preview.entry.id && summary == SUMMARY)
    );
}

#[test]
fn configuration_defaults_and_invalid_shapes() {
    let parse = |bytes: &[u8]| {
        SessionConfiguration::parse(
            ArtifactRef {
                sha256: Digest::of(bytes),
                bytes: bytes.len() as u64,
            },
            bytes,
        )
    };
    assert_eq!(
        parse(b"{}").unwrap().compaction(),
        &CompactionConfig::default()
    );
    assert_eq!(parse(POLICY).unwrap().compaction().max_summary_tokens, 2048);
    assert!(!parse(b"{}").unwrap().compaction().automatic());
    assert!(parse(POLICY).unwrap().compaction().automatic());
    assert!(!parse(br#"{"compaction":{"summary_model":{"provider_instance":"summary","model":"cheap"},"automatic":false}}"#).unwrap().compaction().automatic());
    for bytes in [
        r#"{"compaction":null}"#,
        r#"{"compaction":[]}"#,
        r#"{"compaction":{"retain_recent_turns":1}}"#,
        r#"{"compaction":{"retain_recent_turns":-1}}"#,
        r#"{"compaction":{"max_summary_tokens":0}}"#,
        r#"{"compaction":{"max_summary_tokens":"lots"}}"#,
        r#"{"compaction":{"unknown":true}}"#,
        r#"{"compaction":{"automatic":true}}"#,
        r#"{"compaction":{"automatic":"yes"}}"#,
        r#"{"compaction":{"summary_model":{}}}"#,
        r#"{"compaction":{"summary_model":{"provider_instance":"","model":"m"}}}"#,
    ] {
        assert_eq!(
            parse(bytes.as_bytes()).unwrap_err().code(),
            "invalid_compaction",
            "{bytes}"
        );
    }
    let duplicate = br#"{"compaction":{"max_summary_tokens":1,"max_summary_tokens":2}}"#;
    assert_eq!(
        parse(duplicate).unwrap_err(),
        ConfigurationError::NotAnObject
    );
}

#[test]
fn selection_closed_reasons_and_retained_window() {
    let mut journal = Journal::new();
    let config = CompactionConfig::default();
    let select = |j: &Journal| select_span(&j.snapshot(), j.head().as_ref(), &config);
    assert_eq!(select(&journal), Err(NotEligible::Empty));
    assert_eq!(
        select_span(&journal.snapshot(), Some(&EntryId::new()), &config),
        Err(NotEligible::UnknownHead)
    );
    assert_eq!(
        select_span(
            &journal.snapshot(),
            None,
            &CompactionConfig {
                retain_recent_turns: 1,
                ..config.clone()
            }
        ),
        Err(NotEligible::InvalidConfig)
    );
    journal.user(vec![text("current")]);
    assert_eq!(select(&journal), Err(NotEligible::UnresolvedWork));
    journal.respond(Response::completed(vec![text("done")]));
    assert_eq!(select(&journal), Err(NotEligible::OnlyRecentTurns));
    journal.exchange("two", "done");
    assert_eq!(select(&journal), Err(NotEligible::OnlyRecentTurns));
    let first: Vec<_> = journal
        .snapshot()
        .ancestry(journal.head().as_ref())
        .unwrap()[..2]
        .iter()
        .map(|s| s.entry.id.clone())
        .collect();
    journal.exchange("three", "done");
    assert_eq!(select(&journal).unwrap().covered_entry_ids, first);
    journal.exchange("four", "done");
    let selected = select(&journal).unwrap();
    assert_eq!(selected.covered_entry_ids.len(), 4);
    let before = selected.clone();
    journal.user(vec![text("current five")]);
    assert_eq!(
        select(&journal).unwrap(),
        before,
        "current turn and two recent completed turns stay intact"
    );
    assert_eq!(
        select_span(
            &journal.snapshot(),
            journal.head().as_ref(),
            &CompactionConfig {
                retain_recent_turns: 3,
                ..config
            }
        )
        .unwrap()
        .covered_entry_ids,
        first
    );
}

#[test]
fn controlled_turns_are_refused_in_the_span_and_ignored_in_the_retained_window() {
    // Interim m6-01: compaction refuses a span holding a controlled turn.
    for mode in [ContextMode::Pinned, ContextMode::Excluded] {
        let mut journal = Journal::new();
        let first = journal.exchange("one", "first");
        journal.exchange("two", "second");
        journal.exchange("three", "third");
        journal.exchange("four", "fourth");
        let turn = journal.turn_of(&first[0]);
        journal.control(&turn, mode);
        assert_eq!(
            select_span(
                &journal.snapshot(),
                journal.head().as_ref(),
                &CompactionConfig::default()
            ),
            Err(NotEligible::ContextControlsInSpan),
            "{mode:?}"
        );
        assert_eq!(
            CompactionError::NotEligible(NotEligible::ContextControlsInSpan).code(),
            "context_controls_in_span"
        );

        // A controlled turn inside the retained window does not change the span.
        let mut journal = Journal::new();
        for (prompt, reply) in [
            ("one", "first"),
            ("two", "second"),
            ("three", "third"),
            ("four", "fourth"),
        ] {
            journal.exchange(prompt, reply);
        }
        let last_turn = journal.turn_of(&journal.snapshot().head().cloned().unwrap());
        journal.control(&last_turn, mode);
        let selected = select_span(
            &journal.snapshot(),
            journal.head().as_ref(),
            &CompactionConfig::default(),
        )
        .unwrap();
        let expected: Vec<_> = journal
            .snapshot()
            .ancestry(journal.head().as_ref())
            .unwrap()[..4]
            .iter()
            .map(|stored| stored.entry.id.clone())
            .collect();
        assert_eq!(selected.covered_entry_ids, expected, "{mode:?}");
        assert!(selected.supersedes.is_empty());
    }
}

#[test]
fn noncompleted_turn_is_a_contiguous_barrier() {
    let mut journal = Journal::new();
    journal.user(vec![text("failed")]);
    journal.respond(Response {
        outcome: ResponseOutcome::Failed,
        ..Response::completed(vec![])
    });
    for _ in 0..4 {
        journal.exchange("later", "done");
    }
    assert_eq!(
        select_span(
            &journal.snapshot(),
            journal.head().as_ref(),
            &CompactionConfig::default()
        ),
        Err(NotEligible::NonCompletedTurn)
    );
}

#[test]
fn unresolved_call_and_pending_approval_are_not_selected() {
    let fixture = Fixture::new("interrupted-execution");
    let snapshot = fixture.snapshot();
    assert_eq!(
        select_span(&snapshot, snapshot.head(), &CompactionConfig::default()),
        Err(NotEligible::UnresolvedWork)
    );
    // A completed prefix remains eligible while the current turn contains a proposal.
    let mut journal = prepared();
    let expected = select_span(
        &journal.snapshot(),
        journal.head().as_ref(),
        journal.configuration.compaction(),
    )
    .unwrap();
    journal.user(vec![text("proposal")]);
    journal.respond(Response {
        finish: Some(FinishReason::ToolCalls),
        ..Response::completed(vec![call(&CallId::new(), "read")])
    });
    assert_eq!(
        select_span(
            &journal.snapshot(),
            journal.head().as_ref(),
            journal.configuration.compaction()
        )
        .unwrap(),
        expected
    );
    journal.append(
        vec![],
        vec![journal.transition(RuntimeState::Processing, RuntimeState::Authorizing)],
    );
    let plan = journal.root.put_artifact(b"{}").unwrap();
    let snapshot = journal.snapshot();
    journal.append(
        vec![],
        vec![JournalEvent::ApprovalRequested(ApprovalRequested {
            turn_id: journal.turn(),
            approval_id: ApprovalId::new(),
            call: snapshot.0.pending(snapshot.head()).unwrap()[0].clone(),
            plan_digest: plan.sha256.clone(),
            plan,
            policy_revision: snapshot.policy_revision().into(),
            expires_at: "2026-10-22T12:00:00Z".parse().unwrap(),
        })],
    );
    assert_eq!(
        select_span(
            &journal.snapshot(),
            journal.head().as_ref(),
            journal.configuration.compaction()
        )
        .unwrap(),
        expected
    );
}

#[tokio::test]
async fn storage_acceptance_normal_and_rolling() {
    let mut journal = prepared();
    let old = journal.snapshot();
    let first = preview(&journal).await;
    assert_eq!(
        journal.snapshot().sequence(),
        old.sequence(),
        "generation never writes"
    );
    assert_eq!(first.binding.head.as_ref(), old.head());
    assert_eq!(first.binding.journal_sequence, old.sequence());
    assert_eq!(first.binding.session_model, mock());
    let EntryPayload::Compaction(c) = &first.entry.payload else {
        panic!()
    };
    assert_eq!(
        c.model.as_ref(),
        journal.configuration.compaction().summary_model.as_ref()
    );
    assert_eq!(c.source_fingerprint, Digest::of(&first.manifest_bytes));
    assert_eq!(first.binding.source_fingerprint, c.source_fingerprint);
    assert_eq!(
        first.binding.policy_fingerprint,
        Digest::of(
            &serde_json::to_vec(&(
                RollingSummary::NAME,
                RollingSummary::VERSION,
                journal.configuration.compaction()
            ))
            .unwrap()
        )
    );
    assert_accepted(&mut journal, &first);
    assert_eq!(
        select_span(
            &journal.snapshot(),
            journal.head().as_ref(),
            journal.configuration.compaction()
        ),
        Err(NotEligible::NothingNewSinceSummary)
    );
    journal.exchange("new", "done");
    let second = preview(&journal).await;
    let EntryPayload::Compaction(c2) = &second.entry.payload else {
        panic!()
    };
    assert_eq!(c2.supersedes, std::slice::from_ref(&first.entry.id));
    assert_eq!(c2.covered_entry_ids.len(), 6);
    assert!(
        c.covered_entry_ids
            .iter()
            .all(|id| c2.covered_entry_ids.contains(id))
    );
    assert_accepted(&mut journal, &second);
}

#[tokio::test]
async fn storage_acceptance_branch_isolation() {
    let mut journal = prepared();
    let shared = journal
        .snapshot()
        .ancestry(journal.head().as_ref())
        .unwrap()[3]
        .entry
        .id
        .clone();
    let first = preview(&journal).await;
    assert_accepted(&mut journal, &first);
    journal.navigate(Some(shared));
    journal.exchange("sibling three", "done");
    journal.exchange("sibling four", "done");
    let second = preview(&journal).await;
    let EntryPayload::Compaction(c) = &second.entry.payload else {
        panic!()
    };
    assert!(c.supersedes.is_empty());
    assert!(second.before.summaries.is_empty());
    assert!(
        !second
            .after
            .summaries
            .iter()
            .any(|s| s.entry_id == first.entry.id)
    );
    assert_accepted(&mut journal, &second);
    let source = journal.snapshot();
    let original = ContextBuilder::new(&ContextConfig::default(), &journal.configuration)
        .build(&source, Some(&first.entry.id), None)
        .unwrap();
    assert_eq!(original.report.summaries.len(), 1);
    assert_eq!(original.report.summaries[0].entry_id, first.entry.id);
}

#[tokio::test]
async fn storage_oracle_rejects_recent_manifest_order_and_missing_supersedes() {
    let mut journal = prepared();
    let first = preview(&journal).await;
    let mut recent = first.clone();
    let all: Vec<_> = journal
        .snapshot()
        .ancestry(journal.head().as_ref())
        .unwrap()
        .iter()
        .map(|s| s.entry.id.clone())
        .collect();
    let EntryPayload::Compaction(c) = &mut recent.entry.payload else {
        panic!()
    };
    c.covered_entry_ids = all;
    recent.manifest_bytes = compaction_manifest(
        &journal.snapshot(),
        &c.covered_entry_ids,
        &c.supersedes,
        &c.configuration_fingerprint,
    )
    .unwrap();
    c.source_fingerprint = Digest::of(&recent.manifest_bytes);
    c.source_manifest = ArtifactRef {
        sha256: c.source_fingerprint.clone(),
        bytes: recent.manifest_bytes.len() as u64,
    };
    assert!(matches!(
        commit_preview(&mut journal.store, &journal.root, &recent),
        Err(StoreError::Invalid("compaction_unfinished_or_recent_turn"))
    ));
    let mut wrong = first.clone();
    let mut manifest: Value = serde_json::from_slice(&wrong.manifest_bytes).unwrap();
    manifest["covered"].as_array_mut().unwrap().reverse();
    wrong.manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let EntryPayload::Compaction(c) = &mut wrong.entry.payload else {
        panic!()
    };
    c.source_fingerprint = Digest::of(&wrong.manifest_bytes);
    c.source_manifest = ArtifactRef {
        sha256: c.source_fingerprint.clone(),
        bytes: wrong.manifest_bytes.len() as u64,
    };
    assert!(matches!(
        commit_preview(&mut journal.store, &journal.root, &wrong),
        Err(StoreError::Invalid("source_manifest_mismatch"))
    ));
    assert_accepted(&mut journal, &first);
    journal.exchange("next", "done");
    let second = preview(&journal).await;
    let mut wrong = second.clone();
    let EntryPayload::Compaction(c) = &mut wrong.entry.payload else {
        panic!()
    };
    c.supersedes.clear();
    wrong.manifest_bytes = compaction_manifest(
        &journal.snapshot(),
        &c.covered_entry_ids,
        &[],
        &c.configuration_fingerprint,
    )
    .unwrap();
    c.source_fingerprint = Digest::of(&wrong.manifest_bytes);
    c.source_manifest = ArtifactRef {
        sha256: c.source_fingerprint.clone(),
        bytes: wrong.manifest_bytes.len() as u64,
    };
    assert!(matches!(
        commit_preview(&mut journal.store, &journal.root, &wrong),
        Err(StoreError::Invalid("overlapping_active_summaries"))
    ));
    assert_accepted(&mut journal, &second);
}

#[tokio::test]
async fn exact_request_has_independent_model_no_tools_or_native_state() {
    // Explicitly equal models still cannot leak native blocks or a continuation.
    let policy =
        br#"{"compaction":{"summary_model":{"provider_instance":"mock","model":"scripted"}}}"#;
    let mut journal = Journal::with_configuration(policy);
    journal.user(vec![text("task")]);
    let native = journal.root.put_artifact(b"private native").unwrap();
    journal.respond(Response {
        continuation: Some(native.clone()),
        ..Response::completed(vec![
            text("done"),
            ContentBlock::ProviderState {
                owner: mock(),
                schema: "secret".into(),
                artifact: native.clone(),
            },
        ])
    });
    for _ in 0..3 {
        journal.exchange("recent", "done");
    }
    let provider = Arc::new(ScriptedProvider::new([script(SUMMARY)]));
    let mut request_input = input(journal.snapshot(), journal.configuration.clone());
    request_input.tools = crate::tools::Registry::builtin()
        .definitions()
        .cloned()
        .collect();
    let strategy = RollingSummary::new(provider.clone());
    let proposed = strategy
        .propose(request_input, OperationControl::default())
        .await
        .unwrap();
    let streams = provider.streams();
    let request = &streams[0].request;
    assert_eq!(request.input.model, mock());
    assert!(request.input.tools.is_empty());
    assert!(request.input.continuation.is_none());
    assert!(request.artifacts.references().is_empty());
    assert_eq!(request.input.options.max_output_tokens, 2048);
    assert!(request.input.options.provider_options.is_empty());
    assert!(
        matches!(&request.input.items[0], ContextItem::Instructions { instructions } if instructions[0].text == SUMMARY_INSTRUCTIONS)
    );
    assert!(request.input.items.iter().all(|i| !matches!(i, ContextItem::Assistant { content, .. } if content.iter().any(|b| matches!(b, ContentBlock::ProviderState { .. })))));
    assert!(
        !serde_json::to_string(&request.input.items)
            .unwrap()
            .contains("secret")
    );
    let tools: Vec<_> = crate::tools::Registry::builtin()
        .definitions()
        .cloned()
        .collect();
    assert_accepted_with_tools(&mut journal, &proposed, &tools);
}

#[tokio::test]
async fn rolling_request_contains_prior_summary_and_only_new_eligible_dialogue() {
    let mut journal = prepared();
    let first = preview(&journal).await;
    assert_accepted(&mut journal, &first);
    journal.exchange("new recent", "done");
    let provider = Arc::new(ScriptedProvider::new([script(SUMMARY)]));
    let proposed = RollingSummary::new(provider.clone())
        .propose(
            input(journal.snapshot(), journal.configuration.clone()),
            OperationControl::default(),
        )
        .await
        .unwrap();
    let request = &provider.streams()[0].request;
    assert!(
        matches!(&request.input.items[1], ContextItem::Summary { entry_id, summary } if entry_id == &first.entry.id && summary == SUMMARY)
    );
    assert_eq!(request.input.items.len(), 4); // fixed instructions, prior summary, one new whole turn
    assert_eq!(request.input.model.provider_instance, "summary");
    assert!(
        matches!(&request.input.items[0], ContextItem::Instructions { instructions } if instructions[1].text.contains("session instruction"))
    );
    assert_accepted(&mut journal, &proposed);
    for _ in 0..3 {
        journal.exchange("more recent", "done");
    }
    let provider = Arc::new(ScriptedProvider::new([script(SUMMARY)]));
    let third = RollingSummary::new(provider.clone())
        .propose(
            input(journal.snapshot(), journal.configuration.clone()),
            OperationControl::default(),
        )
        .await
        .unwrap();
    let request = &provider.streams()[0].request;
    let summaries: Vec<_> = request
        .input
        .items
        .iter()
        .filter_map(|i| match i {
            ContextItem::Summary { entry_id, .. } => Some(entry_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        summaries,
        [&proposed.entry.id],
        "third generation never replays superseded summary text"
    );
    assert_accepted(&mut journal, &third);
}

#[tokio::test]
async fn generation_errors_are_coded_and_never_produce_previews() {
    let journal = prepared();
    let cases = [
        (script(""), "empty_summary"),
        (
            script("## Decisions\nOnly one section."),
            "malformed_summary",
        ),
        (script(&"a".repeat(8193)), "summary_too_long"),
        (
            Script::new([Step::failed(ModelError {
                code: ModelErrorCode::ProviderFailure,
                message: "boom".into(),
                provider_code: None,
                retry_after_ms: None,
            })]),
            "provider_failure",
        ),
        (
            Script::new([
                Step::Event(ProviderEvent::Started),
                Step::completed(FinishReason::Refusal),
            ]),
            "summary_refused",
        ),
        (
            Script::new([Step::Event(ProviderEvent::Started), Step::text(0, SUMMARY)]),
            "malformed_summary_stream",
        ),
        (
            Script::new([Step::text(0, SUMMARY), Step::completed(FinishReason::Stop)]),
            "malformed_summary_stream",
        ),
    ];
    let sequence = journal.snapshot().sequence();
    for (script, code) in cases {
        let provider = Arc::new(ScriptedProvider::new([script]));
        let error = RollingSummary::new(provider.clone())
            .propose(
                input(journal.snapshot(), journal.configuration.clone()),
                OperationControl::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), code);
        assert!(provider.streams()[0].dropped);
        assert_eq!(journal.snapshot().sequence(), sequence);
    }
}

#[tokio::test]
async fn unconfigured_summary_model_refuses_but_selection_works() {
    let mut journal = Journal::new();
    for _ in 0..3 {
        journal.exchange("task", "done");
    }
    assert!(
        select_span(
            &journal.snapshot(),
            journal.head().as_ref(),
            journal.configuration.compaction()
        )
        .is_ok()
    );
    let provider = Arc::new(ScriptedProvider::default());
    let error = RollingSummary::new(provider.clone())
        .propose(
            input(journal.snapshot(), journal.configuration.clone()),
            OperationControl::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "summary_model_unconfigured");
    assert!(provider.streams().is_empty());
}

#[tokio::test(start_paused = true)]
async fn cancel_during_generation_drops_stream_without_preview() {
    let journal = prepared();
    let control = OperationControl::default();
    let provider = Arc::new(ScriptedProvider::new([Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, "partial"),
        Step::AwaitCancel,
        Step::cancelled(),
    ])]));
    let strategy = RollingSummary::new(provider.clone());
    let token = control.cancel.clone();
    let generation = strategy.propose(
        input(journal.snapshot(), journal.configuration.clone()),
        control,
    );
    let cancel = async {
        tokio::task::yield_now().await;
        token.cancel();
    };
    let (result, _) = tokio::join!(generation, cancel);
    assert_eq!(result.unwrap_err(), CompactionError::Cancelled);
    assert!(provider.streams()[0].dropped);
    assert!(provider.streams()[0].cancelled);
    assert_eq!(provider.streams()[0].yielded, 2);
}

#[tokio::test(start_paused = true)]
async fn deadline_cancels_even_a_provider_that_ignores_control() {
    let journal = prepared();
    let control = OperationControl {
        deadline: Some(tokio::time::Instant::now() + std::time::Duration::from_secs(1)),
        ..OperationControl::default()
    };
    let token = control.cancel.clone();
    let provider = Arc::new(ScriptedProvider::new([Script::new([Step::Stall])]));
    assert_eq!(
        RollingSummary::new(provider.clone())
            .propose(
                input(journal.snapshot(), journal.configuration.clone()),
                control
            )
            .await
            .unwrap_err(),
        CompactionError::Cancelled
    );
    assert!(token.is_cancelled());
    assert!(provider.streams()[0].dropped);
}

#[tokio::test]
async fn golden_preview_reports_and_storage_acceptance() {
    let fixture = Fixture::new("compaction");
    let mut store = JournalStore::open(fixture.root.clone(), &fixture.session).unwrap();
    let configuration = fixture.root.put_artifact(POLICY).unwrap();
    let snapshot = store.snapshot();
    store
        .append(
            Commit {
                command_id: None,
                command_fingerprint: None,
                head: HeadChange {
                    from: snapshot.head().cloned(),
                    to: snapshot.head().cloned(),
                    reason: HeadReason::Retain,
                },
                entries: vec![],
                events: vec![JournalEvent::ConfigurationChanged(ConfigurationChanged {
                    configuration: configuration.clone(),
                    policy_revision: "summary-policy".into(),
                })],
            },
            at(),
        )
        .unwrap();
    let snapshot = store.snapshot();
    store
        .append(
            Commit {
                command_id: None,
                command_fingerprint: None,
                head: HeadChange {
                    from: snapshot.head().cloned(),
                    to: Some(id("2bf")),
                    reason: HeadReason::Navigate,
                },
                entries: vec![],
                events: vec![],
            },
            at(),
        )
        .unwrap();
    let mut request = input(
        store.snapshot(),
        SessionConfiguration::load(&fixture.root, &configuration).unwrap(),
    );
    request.context = config_with_limit(
        store.snapshot().model_at(store.snapshot().head()).unwrap(),
        Some(128000),
    );
    request.entry_id = id("999");
    let proposed = RollingSummary::new(Arc::new(ScriptedProvider::new([script(SUMMARY)])))
        .propose(request, OperationControl::default())
        .await
        .unwrap();
    golden("compaction-preview-before", &proposed.before);
    golden("compaction-preview-after", &proposed.after);
    let path = golden_path("compaction-preview-diff");
    let actual = format!(
        "{}\n",
        serde_json::to_string_pretty(&proposed.diff).unwrap()
    );
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &actual).unwrap();
    }
    assert_eq!(std::fs::read_to_string(path).unwrap(), actual);
    commit_preview(&mut store, &fixture.root, &proposed).unwrap();
    assert_eq!(
        store.snapshot().entry(&proposed.entry.id).unwrap().entry,
        proposed.entry
    );
}
