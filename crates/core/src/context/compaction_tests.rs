//! Preview production against the real storage oracle and scripted model seam.
use super::*;
use crate::{
    context::compaction::*,
    model::{scripted::*, *},
    session::{ContextMode, TurnId},
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
fn controlled_turns_are_skipped_and_ignored_in_the_retained_window() {
    // Decision 14 (m6-02): coverage is the selected span minus every turn the
    // user pinned or excluded, and m6-01's refusal no longer exists.
    let config = CompactionConfig::default();
    let select = |j: &Journal| select_span(&j.snapshot(), j.head().as_ref(), &config);
    let mut journal = Journal::new();
    let turns: Vec<TurnId> = (0..6)
        .map(|index| {
            let ids = journal.exchange(&format!("p{index}"), &format!("r{index}"));
            journal.turn_of(&ids[0])
        })
        .collect();
    // The span is the first four completed turns; two are retained.
    let span: Vec<EntryId> = journal
        .snapshot()
        .ancestry(journal.head().as_ref())
        .unwrap()[..8]
        .iter()
        .map(|stored| stored.entry.id.clone())
        .collect();
    assert_eq!(select(&journal).unwrap().covered_entry_ids, span);
    let without = |journal: &Journal, turns: &[TurnId]| -> Vec<EntryId> {
        span.iter()
            .filter(|id| {
                !turns
                    .iter()
                    .any(|turn| turn_entries(journal, turn).contains(id))
            })
            .cloned()
            .collect()
    };
    // A pinned turn in the middle is skipped, keeping ancestral order.
    journal.control(&turns[1], ContextMode::Pinned);
    let pinned = select(&journal).unwrap();
    assert_eq!(
        pinned.covered_entry_ids,
        without(&journal, &[turns[1].clone()])
    );
    assert!(pinned.supersedes.is_empty());
    // An excluded turn in the middle is skipped the same way.
    journal.control(&turns[2], ContextMode::Excluded);
    assert_eq!(
        select(&journal).unwrap().covered_entry_ids,
        without(&journal, &[turns[1].clone(), turns[2].clone()])
    );
    // Resetting the pin brings that turn back into the span's coverage.
    journal.control(&turns[1], ContextMode::Default);
    assert_eq!(
        select(&journal).unwrap().covered_entry_ids,
        without(&journal, &[turns[2].clone()])
    );

    // Every turn in the span controlled leaves nothing to cover.
    let mut journal = Journal::new();
    let turns: Vec<TurnId> = (0..4)
        .map(|index| {
            let ids = journal.exchange(&format!("p{index}"), &format!("r{index}"));
            journal.turn_of(&ids[0])
        })
        .collect();
    journal.control(&turns[0], ContextMode::Pinned);
    journal.control(&turns[1], ContextMode::Excluded);
    assert_eq!(select(&journal), Err(NotEligible::OnlyControlledTurns));
    assert_eq!(
        CompactionError::NotEligible(NotEligible::OnlyControlledTurns).code(),
        "only_controlled_turns"
    );

    // A controlled turn inside the retained window does not change the span.
    for mode in [ContextMode::Pinned, ContextMode::Excluded] {
        let mut journal = Journal::new();
        for index in 0..4 {
            journal.exchange(&format!("p{index}"), &format!("r{index}"));
        }
        let last = journal.turn_of(&journal.snapshot().head().cloned().unwrap());
        journal.control(&last, mode);
        let selected = select(&journal).unwrap();
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

/// Branch entries of `turn`, in ancestral order.
fn turn_entries(journal: &Journal, turn: &TurnId) -> Vec<EntryId> {
    journal
        .snapshot()
        .ancestry(journal.head().as_ref())
        .unwrap()
        .iter()
        .filter(|stored| stored.entry.turn_id.as_ref() == Some(turn))
        .map(|stored| stored.entry.id.clone())
        .collect()
}

/// A journal with `count` completed turns named `p0`.. and their turns.
fn turns_journal(count: usize) -> (Journal, Vec<TurnId>) {
    let mut journal = Journal::with_configuration(POLICY);
    let turns = (0..count)
        .map(|index| {
            let ids = journal.exchange(&format!("p{index}"), &format!("r{index}"));
            journal.turn_of(&ids[0])
        })
        .collect();
    (journal, turns)
}

/// The included items of a build, in projection order.
fn order(built: &BuiltContext) -> Vec<(Option<EntryId>, ItemKind)> {
    built
        .report
        .included
        .iter()
        .map(|record| (record.entry_id.clone(), record.kind))
        .collect()
}

/// The user and assistant items of each turn, in turn order.
fn items(journal: &Journal, turns: &[TurnId]) -> Vec<(Option<EntryId>, ItemKind)> {
    turns
        .iter()
        .flat_map(|turn| turn_entries(journal, turn))
        .map(|id| {
            let kind = match &journal.snapshot().entry(&id).unwrap().entry.payload {
                EntryPayload::UserMessage(_) => ItemKind::User,
                EntryPayload::AssistantResponse(_) => ItemKind::Assistant,
                EntryPayload::ToolResult(_) => ItemKind::ToolResult,
                _ => panic!("dialogue entry"),
            };
            (Some(id), kind)
        })
        .collect()
}

/// `text` blocks of an included item.
fn item_text(built: &BuiltContext, id: &EntryId) -> String {
    let item = built
        .projection
        .input
        .items
        .iter()
        .find(|item| item.entry_id() == Some(id))
        .expect("included item");
    match item {
        ContextItem::User { content, .. } | ContextItem::Assistant { content, .. } => {
            serde_json::to_string(content).unwrap()
        }
        other => panic!("not dialogue: {other:?}"),
    }
}

#[tokio::test]
async fn summarizer_never_receives_a_controlled_entry() {
    let (mut journal, turns) = turns_journal(6);
    journal.control(&turns[1], ContextMode::Pinned);
    journal.control(&turns[2], ContextMode::Excluded);
    let pinned = turn_entries(&journal, &turns[1]);
    let excluded = turn_entries(&journal, &turns[2]);
    let provider = Arc::new(ScriptedProvider::new([script(SUMMARY)]));
    let proposed = RollingSummary::new(provider.clone())
        .propose(
            input(journal.snapshot(), journal.configuration.clone()),
            OperationControl::default(),
        )
        .await
        .unwrap();
    let request = &provider.streams()[0].request;
    let sent: Vec<&EntryId> = request
        .input
        .items
        .iter()
        .filter_map(ContextItem::entry_id)
        .collect();
    for id in pinned.iter().chain(&excluded) {
        assert!(
            !sent.contains(&id),
            "controlled entry {id} reached the summarizer"
        );
    }
    // Only the instructions and the two covered turns are sent.
    assert_eq!(request.input.items.len(), 5);
    let json = serde_json::to_string(&request.input.items).unwrap();
    for covered in [&turns[0], &turns[3]] {
        for id in turn_entries(&journal, covered) {
            assert!(json.contains(id.as_str()), "covered entry {id} is missing");
        }
    }
    let EntryPayload::Compaction(compaction) = &proposed.entry.payload else {
        panic!()
    };
    assert_eq!(compaction.covered_entry_ids.len(), 4, "two whole turns");
    assert_accepted(&mut journal, &proposed);
}

#[tokio::test]
async fn excluded_turn_stays_excluded_after_compaction() {
    let (mut journal, turns) = turns_journal(6);
    journal.control(&turns[2], ContextMode::Excluded);
    let excluded = turn_entries(&journal, &turns[2]);
    let proposed = preview(&journal).await;
    let EntryPayload::Compaction(compaction) = &proposed.entry.payload else {
        panic!()
    };
    for id in &excluded {
        assert!(!compaction.covered_entry_ids.contains(id));
    }
    assert_accepted(&mut journal, &proposed);
    let built = journal.build();
    for id in &excluded {
        assert!(
            built.report.excluded.iter().any(|exclusion| {
                &exclusion.entry_id == id
                    && exclusion.part == Part::Entry
                    && exclusion.reason == ExclusionReason::UserExcluded
            }),
            "{id} is still the user's choice"
        );
        assert!(
            !built
                .projection
                .input
                .items
                .iter()
                .any(|item| item.entry_id() == Some(id))
        );
    }
}

#[tokio::test]
async fn projection_order_is_summary_then_pinned_turn_then_retained() {
    let (mut journal, turns) = turns_journal(6);
    journal.control(&turns[1], ContextMode::Pinned);
    let pinned = turn_entries(&journal, &turns[1]);
    let proposed = preview(&journal).await;
    let EntryPayload::Compaction(compaction) = &proposed.entry.payload else {
        panic!()
    };
    for id in &pinned {
        assert!(!compaction.covered_entry_ids.contains(id));
    }
    assert_accepted(&mut journal, &proposed);
    let built = journal.build();
    // Instructions, the summary, the skipped pinned turn verbatim, then the
    // two retained turns. Nothing of the covered turns 1, 3 and 4.
    let mut expected = vec![
        (None, ItemKind::Instructions),
        (Some(proposed.entry.id.clone()), ItemKind::Summary),
    ];
    expected.extend(items(&journal, &[turns[1].clone()]));
    expected.extend(items(&journal, &[turns[4].clone(), turns[5].clone()]));
    assert_eq!(order(&built), expected);
    // The summary stands at its first covered entry, turn 1 of the span.
    assert_eq!(
        built.report.summaries[0].covered_entry_ids.first(),
        turn_entries(&journal, &turns[0]).first()
    );
    // Only the pinned turn's items carry the marker, and they are verbatim.
    let marked: Vec<EntryId> = built
        .report
        .included
        .iter()
        .filter(|record| record.pinned)
        .filter_map(|record| record.entry_id.clone())
        .collect();
    assert_eq!(marked, pinned);
    assert!(item_text(&built, &pinned[0]).contains("p1"));
    assert!(item_text(&built, &pinned[1]).contains("r1"));
}

#[tokio::test]
async fn rolling_compaction_skips_pins_and_keeps_the_order() {
    let (mut journal, turns) = turns_journal(6);
    journal.control(&turns[1], ContextMode::Pinned);
    let first = preview(&journal).await;
    assert_accepted(&mut journal, &first);
    let pinned = turn_entries(&journal, &turns[1]);
    let mut later = Vec::new();
    for index in 6..9 {
        let ids = journal.exchange(&format!("p{index}"), &format!("r{index}"));
        later.push(journal.turn_of(&ids[0]));
    }
    let second = preview(&journal).await;
    let (EntryPayload::Compaction(previous), EntryPayload::Compaction(rolled)) =
        (&first.entry.payload, &second.entry.payload)
    else {
        panic!()
    };
    assert_eq!(rolled.supersedes, std::slice::from_ref(&first.entry.id));
    assert!(
        previous
            .covered_entry_ids
            .iter()
            .all(|id| rolled.covered_entry_ids.contains(id)),
        "rolling replacement keeps the earlier source"
    );
    for id in &pinned {
        assert!(!rolled.covered_entry_ids.contains(id));
    }
    assert!(
        turn_entries(&journal, &later[0])
            .iter()
            .all(|id| rolled.covered_entry_ids.contains(id))
    );
    assert_accepted(&mut journal, &second);
    let built = journal.build();
    let mut expected = vec![
        (None, ItemKind::Instructions),
        (Some(second.entry.id.clone()), ItemKind::Summary),
    ];
    expected.extend(items(&journal, &[turns[1].clone()]));
    expected.extend(items(&journal, &[later[1].clone(), later[2].clone()]));
    assert_eq!(order(&built), expected);
    // Only one summary is projected; the first one is superseded, not covered.
    assert_eq!(built.report.summaries.len(), 1);
    assert_eq!(built.report.summaries[0].entry_id, second.entry.id);
    assert!(built.report.excluded.iter().any(|exclusion| {
        exclusion.entry_id == first.entry.id
            && exclusion.part == Part::Entry
            && exclusion.reason == ExclusionReason::SupersededSummary
    }));
}

#[tokio::test]
async fn a_large_pinned_turn_is_never_truncated_and_overflows_the_budget() {
    let mut journal = Journal::with_configuration(POLICY);
    let big = "x".repeat(40_000);
    let user = journal.user(vec![text(&big)]);
    let pinned_turn = journal.turn_of(&user);
    journal.respond(Response::completed(vec![text("acknowledged")]));
    for index in 1..6 {
        journal.exchange(&format!("p{index}"), &format!("r{index}"));
    }
    journal.control(&pinned_turn, ContextMode::Pinned);
    let limited = config_with_limit(&mock(), Some(20_000));
    let mut request = input(journal.snapshot(), journal.configuration.clone());
    request.context = limited.clone();
    let provider = Arc::new(ScriptedProvider::new([script(SUMMARY)]));
    let proposed = RollingSummary::new(provider)
        .propose(request, OperationControl::default())
        .await
        .unwrap();
    assert!(proposed.after.flags.overflow, "the pin cannot fit");
    assert!(!proposed.diff.fits_budget, "a manual preview says so");
    commit_preview(&mut journal.store, &journal.root, &proposed).unwrap();
    let built = journal
        .build_with(&limited, journal.head().as_ref(), None)
        .unwrap();
    assert!(built.report.flags.overflow);
    // Byte for byte: a pinned turn is never truncated to fit.
    let item = built
        .projection
        .input
        .items
        .iter()
        .find(|item| item.entry_id() == Some(&user))
        .expect("the pinned message is still projected");
    assert!(matches!(
        item,
        ContextItem::User { content, .. }
            if content == &vec![text(&big)]
    ));
    assert!(
        built
            .report
            .included
            .iter()
            .any(|record| record.entry_id.as_ref() == Some(&user) && record.pinned)
    );
}

#[tokio::test]
async fn resetting_a_skipped_excluded_turn_restores_it_in_place() {
    let (mut journal, turns) = turns_journal(6);
    journal.control(&turns[1], ContextMode::Pinned);
    journal.control(&turns[2], ContextMode::Excluded);
    let pinned = turn_entries(&journal, &turns[1]);
    let excluded = turn_entries(&journal, &turns[2]);
    let proposed = preview(&journal).await;
    assert_accepted(&mut journal, &proposed);
    assert!(
        !journal
            .build()
            .projection
            .input
            .items
            .iter()
            .filter_map(ContextItem::entry_id)
            .any(|id| excluded.contains(id))
    );
    // Resetting is how a control is undone, and a skipped turn was never
    // summarized: it comes back verbatim, after the pinned turn.
    journal.control(&turns[2], ContextMode::Default);
    let built = journal.build();
    let mut expected = vec![
        (None, ItemKind::Instructions),
        (Some(proposed.entry.id.clone()), ItemKind::Summary),
    ];
    expected.extend(items(&journal, &[turns[1].clone(), turns[2].clone()]));
    expected.extend(items(&journal, &[turns[4].clone(), turns[5].clone()]));
    assert_eq!(order(&built), expected);
    assert!(item_text(&built, &pinned[0]).contains("p1"));
    assert!(item_text(&built, &excluded[0]).contains("p2"));
    assert!(item_text(&built, &excluded[1]).contains("r2"));
    let _ = pinned;
}

#[tokio::test]
async fn compaction_keeps_an_earlier_continuation_valid() {
    // A compaction is appended at the head, so it cannot change the projection
    // of any entry before it: the prefix a continuation bound is untouched.
    let mut journal = Journal::with_configuration(POLICY);
    let first = journal.exchange("p0", "r0");
    let pinned_turn = journal.turn_of(&first[0]);
    journal.exchange("p1", "r1");
    journal.exchange("p2", "r2");
    journal.exchange("p3", "r3");
    let native = journal.root.put_artifact(b"private native state").unwrap();
    journal.user(vec![text("p4")]);
    let response = journal.respond(Response {
        continuation: Some(native),
        ..Response::completed(vec![text("r4")])
    });
    journal.control(&pinned_turn, ContextMode::Pinned);
    let proposed = preview(&journal).await;
    assert_accepted(&mut journal, &proposed);
    let built = journal.build();
    assert!(built.report.included.iter().any(|record| {
        record.entry_id.as_ref() == Some(&response)
            && record
                .blocks
                .iter()
                .any(|block| block.part == Part::Continuation)
    }));
    assert!(item_text(&built, &first[0]).contains("p0"));
}

#[tokio::test]
async fn a_control_after_a_compaction_invalidates_a_continuation() {
    // Controls are folded at the head and applied to every rebuilt prefix, so a
    // later exclusion does change the prefix a continuation bound.
    let (mut journal, turns) = turns_journal(6);
    let proposed = preview(&journal).await;
    assert_accepted(&mut journal, &proposed);
    let native = journal.root.put_artifact(b"private native state").unwrap();
    journal.user(vec![text("p6")]);
    let response = journal.respond(Response {
        continuation: Some(native),
        ..Response::completed(vec![text("r6")])
    });
    let before = journal.build();
    assert!(before.report.included.iter().any(|record| {
        record.entry_id.as_ref() == Some(&response)
            && record
                .blocks
                .iter()
                .any(|block| block.part == Part::Continuation)
    }));
    // Turn 4 is retained by the compaction, so it can still be excluded.
    journal.control(&turns[4], ContextMode::Excluded);
    let after = journal.build();
    assert!(after.report.excluded.iter().any(|exclusion| {
        exclusion.entry_id == response
            && exclusion.part == Part::Continuation
            && exclusion.reason == ExclusionReason::ContinuationPrefixMismatch
    }));
    assert!(after.projection.input.items.iter().any(|item| matches!(
        item,
        ContextItem::Assistant { entry_id, content, .. }
            if entry_id == &response
                && content.len() == 1
                && matches!(&content[0], ContentBlock::Text { text } if text == "r6")
    )));
}

#[test]
fn no_eligible_new_dialogue_after_a_summary_is_ineligible() {
    let (mut journal, turns) = turns_journal(4);
    let covered: Vec<EntryId> = turn_entries(&journal, &turns[0])
        .into_iter()
        .chain(turn_entries(&journal, &turns[1]))
        .collect();
    journal.compact(covered, vec![], "older summary");
    let mut later = Vec::new();
    for index in 4..7 {
        let ids = journal.exchange(&format!("p{index}"), &format!("r{index}"));
        later.push(journal.turn_of(&ids[0]));
    }
    // Every turn after the summary that the span reaches is pinned.
    for turn in [&turns[2], &turns[3], &later[0]] {
        journal.control(turn, ContextMode::Pinned);
    }
    assert_eq!(
        select_span(
            &journal.snapshot(),
            journal.head().as_ref(),
            &CompactionConfig::default()
        ),
        Err(NotEligible::OnlyControlledTurns),
        "the only new dialogue is pinned"
    );
    // Resetting one brings back eligible new dialogue.
    journal.control(&turns[2], ContextMode::Default);
    assert!(
        select_span(
            &journal.snapshot(),
            journal.head().as_ref(),
            &CompactionConfig::default()
        )
        .is_ok()
    );
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
