//! RFC-001 C02 against the real journal validator (oracle enabled).
use super::*;
use crate::context::compaction::*;
use crate::context::{ContextBuilder, ContextItem, SessionConfiguration};
use crate::model::{OperationControl, scripted::*};
use crate::session::ContextMode;
use scenarios::plain_turn;

pub(super) const SUMMARY: &str = "## Task and constraints\nKeep task.\n## Decisions\nRust.\n## Files and actions\nRead succeeded.\n## Outcomes\nDone.\n## Unresolved work\nNone.\n";
const MANUAL: &[u8] = br#"{"compaction":{"summary_model":{"provider_instance":"summary","model":"cheap"},"automatic":false}}"#;
const AUTOMATIC: &[u8] = br#"{"compaction":{"summary_model":{"provider_instance":"summary","model":"cheap"},"retain_recent_turns":3}}"#;
/// The default retention of two turns, with automatic compaction on.
const AUTOMATIC_TWO: &[u8] =
    br#"{"compaction":{"summary_model":{"provider_instance":"summary","model":"cheap"}}}"#;

pub(super) fn summary_script() -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, SUMMARY),
        Step::completed(FinishReason::Stop),
    ])
}
impl Driver {
    fn generate_summary_recording(
        &mut self,
    ) -> (CompactionToken, CompactionPreview, Arc<ScriptedProvider>) {
        let (token, input) = self.compaction.take().expect("generation effect");
        let provider = Arc::new(ScriptedProvider::new([summary_script()]));
        let preview = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(
                RollingSummary::new(provider.clone()).propose(input, OperationControl::default()),
            )
            .unwrap();
        assert_eq!(provider.streams().len(), 1);
        (token, preview, provider)
    }
    fn generate_summary(&mut self) -> (CompactionToken, CompactionPreview) {
        let (token, preview, _) = self.generate_summary_recording();
        (token, preview)
    }
    fn propose_summary(&mut self) {
        let (token, preview) = self.generate_summary();
        self.step(Input::CompactionProposed {
            token,
            result: Ok(Box::new(preview)),
        });
    }
    fn manual_preview(&mut self) -> (EntryId, CompactionPreview) {
        let request = self.command(Command::PreviewCompaction);
        assert!(self.reply(request).is_none());
        self.propose_summary();
        let Accepted::CompactionPreviewed {
            preview_id,
            preview,
        } = self.accepted(request)
        else {
            panic!("preview reply")
        };
        (preview_id.clone(), *preview.clone())
    }
}
fn prepared() -> Driver {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
    let configuration = root.put_artifact(MANUAL).unwrap();
    let session = SessionId::new();
    let header = SessionHeader {
        workspace: workspace.to_str().unwrap().into(),
        initial_model: ModelRef {
            provider_instance: "mock".into(),
            model: "scripted".into(),
        },
        policy_revision: "initial".into(),
        configuration,
    };
    drop(
        JournalStore::create(
            root.clone(),
            session.clone(),
            header,
            DeterministicEnv::BASE.parse().unwrap(),
        )
        .unwrap(),
    );
    let mut d = Driver::open_existing(temp, root, session, known());
    for _ in 0..4 {
        plain_turn(&mut d, "old task", "done");
    }
    d
}
fn journal_bytes(d: &Driver) -> Vec<u8> {
    std::fs::read(
        d.root
            .path()
            .join("sessions")
            .join(format!("{}.jsonl", d.session)),
    )
    .unwrap()
}
fn tree(path: &std::path::Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn walk(
        root: &std::path::Path,
        path: &std::path::Path,
        files: &mut BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(path, path, &mut files);
    files
}
fn checked(d: Driver) {
    let finished = d.finish();
    check_replay(&finished);
    check_resync(&finished);
    check_turns(&finished);
}

#[test]
fn c02_manual_preview_leaves_journal_and_workspace_unchanged() {
    let mut d = prepared();
    let workspace = d._temp.path().join("workspace");
    assert_eq!(
        d.runtime.snapshot().header().workspace,
        workspace.to_str().unwrap()
    );
    std::fs::write(workspace.join("sentinel"), b"unaltered").unwrap();
    let journal = journal_bytes(&d);
    let files = tree(d.root.path());
    let original_workspace = tree(&workspace);
    let (id, preview) = d.manual_preview();
    assert_eq!(journal_bytes(&d), journal);
    assert_eq!(
        tree(d.root.path()),
        files,
        "not even a manifest was published"
    );
    assert_eq!(tree(&workspace), original_workspace);
    assert_eq!(d.runtime.inspect_context_preview(&id).unwrap(), preview);
    assert_eq!(
        preview.diff,
        CompactionDiff::between(&preview.before, &preview.after)
    );
    checked(d);
}

fn stale(field: &'static str, change: impl FnOnce(&mut CompactionPreview)) {
    let mut d = prepared();
    let (id, _) = d.manual_preview();
    d.runtime.edit_compaction_preview_for_test(change);
    let before = journal_bytes(&d);
    let files = tree(d.root.path());
    let request = d.command(Command::CommitCompaction {
        preview_id: id.clone(),
    });
    assert_eq!(d.rejected(request), Rejection::StalePreview { field });
    assert!(d.pending.is_none());
    assert_eq!(journal_bytes(&d), before);
    assert_eq!(tree(d.root.path()), files);
    let retry = d.command(Command::CommitCompaction { preview_id: id });
    assert!(matches!(d.rejected(retry), Rejection::StalePreview { .. }));
    // Deliberately corrupted private state is not replay input.
    d.requests.clear();
}
#[test]
fn c02_stale_head() {
    stale("head", |p| p.binding.head = None);
}
#[test]
fn c02_stale_sequence() {
    stale("journal_sequence", |p| p.binding.journal_sequence -= 1);
}
#[test]
fn c02_stale_model() {
    stale("session_model", |p| {
        p.binding.session_model.model = "different".into()
    });
}
#[test]
fn c02_stale_policy() {
    stale("policy_fingerprint", |p| {
        p.binding.policy_fingerprint = Digest::of(b"changed policy")
    });
}
#[test]
fn c02_stale_source_manifest_substitution() {
    stale("source_fingerprint", |p| {
        let mut manifest: serde_json::Value = serde_json::from_slice(&p.manifest_bytes).unwrap();
        manifest["covered"][0]["record_checksum"] =
            serde_json::json!(Digest::of(b"substituted provenance"));
        p.manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let digest = Digest::of(&p.manifest_bytes);
        p.binding.source_fingerprint = digest.clone();
        let EntryPayload::Compaction(c) = &mut p.entry.payload else {
            panic!()
        };
        c.source_fingerprint = digest.clone();
        c.source_manifest = ArtifactRef {
            sha256: digest,
            bytes: p.manifest_bytes.len() as u64,
        };
    });
}

#[test]
fn c02_preview_never_revives_after_commit_navigation_restart_or_fault() {
    let mut d = prepared();
    let head = d.runtime.snapshot().head().cloned();
    let (id, _) = d.manual_preview();
    let navigate = d.command(Command::SelectHead { head: None });
    d.settle();
    d.accepted(navigate);
    let navigate = d.command(Command::SelectHead { head });
    d.settle();
    d.accepted(navigate);
    let request = d.command(Command::CommitCompaction { preview_id: id });
    assert_eq!(
        d.rejected(request),
        Rejection::StalePreview { field: "head" }
    );
    let (id, _) = d.manual_preview();
    d.reopen();
    let request = d.command(Command::CommitCompaction { preview_id: id });
    assert_eq!(
        d.rejected(request),
        Rejection::StalePreview {
            field: "preview_id"
        }
    );
    let (id, _) = d.manual_preview();
    d.step(Input::DriverInvariant {
        code: "injected_fault",
    });
    assert!(d.runtime.inspect_context_preview(&id).is_err());
    let request = d.command(Command::CommitCompaction { preview_id: id });
    assert_eq!(d.rejected(request), Rejection::Faulted);
    checked(d);
}

fn automatic() -> Driver {
    let mut config = known();
    config.context.compaction_trigger_permille = 1;
    let mut d = Driver::with_configuration(config, AUTOMATIC);
    for _ in 0..5 {
        plain_turn(&mut d, &"u".repeat(1200), "done");
    }
    assert!(
        d.compaction.is_some(),
        "threshold reached at fifth finish with three retained turns"
    );
    d
}
fn automatic_summary() -> (Driver, CompactionPreview) {
    let mut d = automatic();
    let (token, preview) = d.generate_summary();
    d.step(Input::CompactionProposed {
        token,
        result: Ok(Box::new(preview.clone())),
    });
    d.settle();
    assert!(
        d.compaction.is_none(),
        "summary commit cannot retrigger generation"
    );
    (d, preview)
}
#[test]
fn c02_automatic_retains_configured_recent_turns() {
    let (d, preview) = automatic_summary();
    let EntryPayload::Compaction(c) = &preview.entry.payload else {
        panic!()
    };
    assert_eq!(c.covered_entry_ids.len(), 4);
    let branch = d
        .runtime
        .snapshot()
        .ancestry(d.runtime.snapshot().head())
        .unwrap();
    let recent: BTreeSet<_> = branch
        .iter()
        .filter_map(|s| s.entry.turn_id.clone())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .take(6)
        .collect();
    assert_eq!(recent.len(), 3);
    assert!(c.covered_entry_ids.iter().all(|id| {
        !recent.contains(
            d.runtime
                .snapshot()
                .entry(id)
                .unwrap()
                .entry
                .turn_id
                .as_ref()
                .unwrap(),
        )
    }));
    checked(d);
}
#[test]
fn c02_automatic_references_exact_source_ids() {
    let (d, preview) = automatic_summary();
    let EntryPayload::Compaction(c) = &preview.entry.payload else {
        panic!()
    };
    assert_eq!(
        compaction_manifest(
            d.runtime.snapshot(),
            &c.covered_entry_ids,
            &c.supersedes,
            &c.configuration_fingerprint
        )
        .unwrap(),
        preview.manifest_bytes
    );
    assert_eq!(
        d.root
            .read_artifact(&c.source_manifest, usize::MAX)
            .unwrap(),
        preview.manifest_bytes
    );
    checked(d);
}
#[test]
fn c02_automatic_supersedes_only_ancestor_summaries() {
    let (mut d, first) = automatic_summary();
    plain_turn(&mut d, &"n".repeat(1200), "done");
    let (token, second) = d.generate_summary();
    let EntryPayload::Compaction(c) = &second.entry.payload else {
        panic!()
    };
    assert_eq!(c.supersedes, std::slice::from_ref(&first.entry.id));
    d.step(Input::CompactionProposed {
        token,
        result: Ok(Box::new(second)),
    });
    d.settle();
    let earlier = d
        .runtime
        .snapshot()
        .ancestry(Some(&first.entry.id))
        .unwrap()[3]
        .entry
        .id
        .clone();
    let request = d.command(Command::SelectHead {
        head: Some(earlier),
    });
    d.settle();
    d.accepted(request);
    // The branch has two completed turns; add enough to meet retention/threshold.
    for _ in 0..3 {
        plain_turn(&mut d, &"b".repeat(1200), "done");
    }
    let (token, sibling) = d.generate_summary();
    let EntryPayload::Compaction(c) = &sibling.entry.payload else {
        panic!()
    };
    assert!(c.supersedes.is_empty());
    assert!(
        !sibling
            .before
            .summaries
            .iter()
            .any(|s| s.entry_id == first.entry.id)
    );
    d.step(Input::CompactionProposed {
        token,
        result: Ok(Box::new(sibling)),
    });
    d.settle();
    checked(d);
}
#[test]
fn c02_original_entries_remain_readable_byte_identical() {
    let mut d = automatic();
    let before = journal_bytes(&d);
    let originals: Vec<_> = d
        .runtime
        .snapshot()
        .entries()
        .map(|(id, s)| {
            (
                id.clone(),
                serde_json::to_vec(&s.entry).unwrap(),
                s.record_checksum.clone(),
            )
        })
        .collect();
    d.propose_summary();
    d.settle();
    assert!(journal_bytes(&d).starts_with(&before));
    for (id, bytes, checksum) in originals {
        let stored = d.runtime.snapshot().entry(&id).unwrap();
        assert_eq!(serde_json::to_vec(&stored.entry).unwrap(), bytes);
        assert_eq!(stored.record_checksum, checksum);
    }
    checked(d);
}
#[test]
fn c02_budget_fits_or_failure_is_explicit() {
    let (d, preview) = automatic_summary();
    assert!(preview.diff.fits_budget);
    checked(d);
    let mut config = known();
    config.context.catalog.insert(
        ModelRef {
            provider_instance: "mock".into(),
            model: "scripted".into(),
        },
        ModelLimits {
            context_tokens: Some(16385),
            max_output_tokens: None,
        },
    );
    let mut d = Driver::with_configuration(config, AUTOMATIC);
    let before = journal_bytes(&d);
    let (request, _) = d.start("too large");
    assert_eq!(
        d.rejected(request),
        Rejection::ContextBudgetExceeded {
            reason: Some(NotEligible::Empty)
        }
    );
    assert_eq!(journal_bytes(&d), before);
    assert!(d.compaction.is_none() && d.pending.is_none() && d.started.is_empty());
    checked(d);
}
#[test]
fn c02_automatic_summaries_respect_pins() {
    // The C02 pin clause: a pinned turn is never summarized and never lost.
    let mut d = before_overflow();
    let snapshot = d.runtime.snapshot();
    let branch = snapshot.ancestry(snapshot.head()).unwrap();
    let pinned_turn = branch[0].entry.turn_id.clone().unwrap();
    let pinned: Vec<EntryId> = branch
        .iter()
        .filter(|stored| stored.entry.turn_id.as_ref() == Some(&pinned_turn))
        .map(|stored| stored.entry.id.clone())
        .collect();
    let pin = d.command(Command::SetContextControl {
        turn_id: pinned_turn,
        mode: ContextMode::Pinned,
    });
    d.settle();
    d.accepted(pin);
    assert!(d.compaction.is_none(), "a pin alone triggers nothing");

    // The held start overflows, so automatic compaction runs while idle.
    let prompt = "u".repeat(800);
    let (request, _) = d.start(&prompt);
    assert!(d.reply(request).is_none());
    assert!(
        d.compaction.is_some(),
        "an overflowing start compacts first"
    );
    let (token, preview, provider) = d.generate_summary_recording();
    // The summarizer never receives an entry of the pinned turn.
    let streams = provider.streams();
    let sent: Vec<&EntryId> = streams[0]
        .request
        .input
        .items
        .iter()
        .filter_map(|item| item.entry_id())
        .collect();
    assert!(!sent.is_empty());
    assert!(sent.iter().all(|id| !pinned.contains(id)));
    let EntryPayload::Compaction(compaction) = &preview.entry.payload else {
        panic!()
    };
    assert!(
        compaction
            .covered_entry_ids
            .iter()
            .all(|id| !pinned.contains(id))
    );
    assert_eq!(compaction.covered_entry_ids.len(), 2, "the other span turn");
    d.step(Input::CompactionProposed {
        token,
        result: Ok(Box::new(preview.clone())),
    });
    d.settle();

    // The projection keeps the pinned turn verbatim and marked, with the
    // summary standing for the covered turn.
    let snapshot = d.runtime.snapshot().clone();
    let configuration = SessionConfiguration::load(&d.root, snapshot.configuration()).unwrap();
    let built = ContextBuilder::new(&budget_config().context, &configuration)
        .build(&snapshot, snapshot.head(), None)
        .unwrap();
    for id in &pinned {
        assert!(
            built
                .report
                .included
                .iter()
                .any(|record| record.entry_id.as_ref() == Some(id) && record.pinned),
            "pinned entry {id} is included and marked"
        );
    }
    assert!(built.projection.input.items.iter().any(|item| matches!(
        item,
        ContextItem::Summary { entry_id, .. } if entry_id == &preview.entry.id
    )));
    let pinned_user = built
        .projection
        .input
        .items
        .iter()
        .find(|item| item.entry_id() == Some(&pinned[0]))
        .expect("the pinned message is projected");
    assert!(matches!(
        pinned_user,
        ContextItem::User { content, .. }
            if content.len() == 1
                && matches!(&content[0], ContentBlock::Text { text } if *text == prompt)
    ));
    d.settle();
    d.accepted(request);
    checked(d);
}

/// A summary long enough that replacing one small turn leaves a pinned turn
/// over budget: the failure may not be papered over by a bigger summary.
fn padded_summary() -> String {
    format!(
        "## Task and constraints\n{}\n## Decisions\nRust.\n## Files and actions\nRead succeeded.\n## Outcomes\nDone.\n## Unresolved work\nNone.\n",
        "padding ".repeat(300)
    )
}

#[test]
fn a_pinned_turn_that_cannot_fit_fails_explicitly() {
    // A 1000-token usable budget. The pinned turn is most of it, so pinned plus
    // summary plus retained turns cannot fit and nothing is truncated.
    let mut config = budget_config();
    config.context.compaction_trigger_permille = 850;
    let mut d = Driver::with_configuration(config.clone(), AUTOMATIC_TWO);
    plain_turn(&mut d, &"p".repeat(2800), "done");
    let snapshot = d.runtime.snapshot();
    let branch = snapshot.ancestry(snapshot.head()).unwrap();
    let pinned_turn = branch[0].entry.turn_id.clone().unwrap();
    let pinned: Vec<EntryId> = branch
        .iter()
        .filter(|stored| stored.entry.turn_id.as_ref() == Some(&pinned_turn))
        .map(|stored| stored.entry.id.clone())
        .collect();
    let pin = d.command(Command::SetContextControl {
        turn_id: pinned_turn,
        mode: ContextMode::Pinned,
    });
    d.settle();
    d.accepted(pin);
    for _ in 0..2 {
        plain_turn(&mut d, "next", "done");
        assert!(d.compaction.is_none(), "still under the trigger");
    }
    // The fourth turn reaches the trigger and the summary would overflow.
    plain_turn(&mut d, &"n".repeat(600), "done");
    assert!(d.compaction.is_some());
    let sequence = d.runtime.snapshot().sequence();
    let (token, input) = d.compaction.take().expect("generation effect");
    let provider = Arc::new(ScriptedProvider::new([Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, padded_summary()),
        Step::completed(FinishReason::Stop),
    ])]));
    let preview = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(RollingSummary::new(provider).propose(input, OperationControl::default()))
        .unwrap();
    assert!(
        preview.after.flags.overflow,
        "pinned plus summary plus retained cannot fit"
    );
    assert!(!preview.diff.fits_budget);
    d.step(Input::CompactionProposed {
        token,
        result: Ok(Box::new(preview)),
    });
    d.settle();
    assert!(d.segment.events().any(|event| matches!(
        &event.kind,
        EventKind::CompactionFailed {
            code: "context_budget_exceeded"
        }
    )));
    assert_eq!(
        d.runtime.snapshot().sequence(),
        sequence,
        "an ineffective summary is not committed"
    );
    assert!(
        d.runtime
            .snapshot()
            .entries()
            .all(|(_, stored)| !matches!(stored.entry.payload, EntryPayload::Compaction(_)))
    );
    assert!(d.compaction.is_none() && d.pending.is_none());
    // The pinned turn is still projected in full, never truncated. The head
    // itself fits: it was the summary that would not.
    let snapshot = d.runtime.snapshot().clone();
    let configuration = SessionConfiguration::load(&d.root, snapshot.configuration()).unwrap();
    let built = ContextBuilder::new(&config.context, &configuration)
        .build(&snapshot, snapshot.head(), None)
        .unwrap();
    assert!(!built.report.flags.overflow);
    let item = built
        .projection
        .input
        .items
        .iter()
        .find(|item| item.entry_id() == Some(&pinned[0]))
        .expect("the pinned message is projected");
    assert!(matches!(
        item,
        ContextItem::User { content, .. }
            if content.len() == 1
                && matches!(&content[0], ContentBlock::Text { text } if text.len() == 2800)
    ));
    checked(d);
}
#[test]
fn c02_no_summary_regenerated_during_pure_projection_or_prepare() {
    let mut d = prepared();
    for _ in 0..4 {
        d.runtime.inspect_context(None, None).unwrap();
    }
    let (id, _) = d.manual_preview();
    for _ in 0..4 {
        d.runtime.inspect_context_preview(&id).unwrap();
    }
    let index = d.segment.effects.len();
    let (request, _) = d.start("next");
    d.settle();
    d.accepted(request);
    assert!(
        !d.segment.effects[index..]
            .iter()
            .any(|e| matches!(e, Effect::ProposeCompaction { .. }))
    );
    // The connected-provider zero-stream check is in the driver inspection test.
    // This core-only preparation has not dispatched the normal inference either.
    d.reopen();
    d.settle();
    checked(d);
}
#[test]
fn generation_cancellation_start_turn_and_stale_answer() {
    let mut d = prepared();
    let request = d.command(Command::PreviewCompaction);
    let (token, preview) = d.generate_summary();
    let cancel = d.command(Command::CancelCompaction);
    assert_eq!(d.accepted(cancel), &Accepted::CompactionCancelled);
    assert_eq!(
        d.rejected(request),
        Rejection::Compaction(CompactionError::Cancelled)
    );
    assert!(
        d.step(Input::CompactionProposed {
            token,
            result: Ok(Box::new(preview))
        })
        .is_empty()
    );
    let request = d.command(Command::PreviewCompaction);
    let (token, preview) = d.generate_summary();
    let (start, _) = d.start("preempts generation");
    d.settle();
    d.accepted(start);
    assert_eq!(
        d.rejected(request),
        Rejection::Compaction(CompactionError::Cancelled)
    );
    assert!(
        d.step(Input::CompactionProposed {
            token,
            result: Ok(Box::new(preview))
        })
        .is_empty()
    );
    d.reopen();
    d.settle();
    checked(d);
}
#[test]
fn automatic_single_attempt_per_trigger_and_failure_never_blocks() {
    let mut d = automatic();
    let (token, _) = d.compaction.take().unwrap();
    let effects = d.step(Input::CompactionProposed {
        token,
        result: Err(CompactionError::Invalid("provider_failure")),
    });
    assert!(effects.iter().any(|e| matches!(
        e,
        Effect::Emit(RuntimeEvent {
            kind: EventKind::CompactionFailed {
                code: "provider_failure"
            },
            ..
        })
    )));
    assert!(d.compaction.is_none() && d.pending.is_none());
    d.step(Input::PublishView);
    assert!(d.compaction.is_none(), "no automatic retry loop");
    plain_turn(&mut d, "next trigger", "done");
    assert!(d.compaction.is_some());
    d.propose_summary();
    d.settle();
    checked(d);
}
#[test]
fn crash_generation_manifest_publication_and_commit_before_reply() {
    let mut d = prepared();
    let before = journal_bytes(&d);
    let files = tree(d.root.path());
    d.command(Command::PreviewCompaction);
    d.crash(Crash::Lost);
    assert_eq!(d.runtime.snapshot().state(), RuntimeState::Idle);
    assert_eq!(journal_bytes(&d), before);
    assert_eq!(tree(d.root.path()), files);
    let (id, preview) = d.manual_preview();
    d.command(Command::CommitCompaction { preview_id: id });
    d.root.put_artifact(&preview.manifest_bytes).unwrap();
    d.crash(Crash::Lost);
    assert_eq!(journal_bytes(&d), before);
    assert!(d.runtime.snapshot().entry(&preview.entry.id).is_none());
    assert_eq!(d.runtime.snapshot().state(), RuntimeState::Idle);
    let (id, preview) = d.manual_preview();
    d.command(Command::CommitCompaction {
        preview_id: id.clone(),
    });
    d.crash(Crash::Durable);
    assert!(d.runtime.snapshot().entry(&preview.entry.id).is_some());
    assert_eq!(d.runtime.snapshot().state(), RuntimeState::Idle);
    let sequence = d.runtime.snapshot().sequence();
    let request = d.command(Command::CommitCompaction { preview_id: id });
    assert_eq!(
        d.rejected(request),
        Rejection::StalePreview {
            field: "preview_id"
        }
    );
    assert_eq!(d.runtime.snapshot().sequence(), sequence);
    assert_eq!(
        d.runtime
            .snapshot()
            .entries()
            .filter(|(_, s)| matches!(s.entry.payload, EntryPayload::Compaction(_)))
            .count(),
        1
    );
    checked(d);
}

fn budget_config() -> RuntimeConfig {
    let mut config = known();
    config.context.catalog.insert(
        ModelRef {
            provider_instance: "mock".into(),
            model: "scripted".into(),
        },
        ModelLimits {
            context_tokens: Some(17384),
            max_output_tokens: None,
        },
    );
    config
}
fn before_overflow() -> Driver {
    let mut d = Driver::with_configuration(
        budget_config(),
        br#"{"compaction":{"summary_model":{"provider_instance":"summary","model":"cheap"}}}"#,
    );
    for _ in 0..4 {
        plain_turn(&mut d, &"u".repeat(800), "done");
    }
    assert!(d.compaction.is_none());
    d
}
#[test]
fn overflowing_start_waits_for_idle_compaction_then_deduplicated_c1() {
    let mut d = before_overflow();
    let sequence = d.runtime.snapshot().sequence();
    let (request, command_id) = d.start(&"u".repeat(800));
    assert!(d.reply(request).is_none());
    assert!(d.pending.is_none() && d.compaction.is_some());
    assert_eq!(d.runtime.snapshot().state(), RuntimeState::Idle);
    assert_eq!(d.runtime.snapshot().sequence(), sequence);
    let retry = d.command(Command::StartTurn {
        command_id: command_id.clone(),
        content: text(&"u".repeat(800)),
    });
    assert!(d.reply(retry).is_none());
    let bad = d.command(Command::StartTurn {
        command_id: command_id.clone(),
        content: text("different"),
    });
    assert_eq!(d.rejected(bad), Rejection::StaleCommand);
    let (busy, _) = d.start("another command");
    assert_eq!(d.rejected(busy), Rejection::Busy);
    d.propose_summary();
    assert_eq!(d.pending.as_ref().unwrap().commit.entries.len(), 1);
    assert!(matches!(
        d.pending.as_ref().unwrap().commit.entries[0].payload,
        EntryPayload::Compaction(_)
    ));
    let retry_during_commit = d.command(Command::StartTurn {
        command_id: command_id.clone(),
        content: text(&"u".repeat(800)),
    });
    assert!(d.reply(retry_during_commit).is_none());
    assert!(d.reply(request).is_none());
    d.complete(); // summary ack, only now can C1 be planned
    assert_eq!(d.runtime.snapshot().state(), RuntimeState::Idle);
    assert!(d.reply(request).is_none());
    assert!(matches!(
        d.pending.as_ref().unwrap().commit.entries[0].payload,
        EntryPayload::UserMessage(_)
    ));
    d.settle();
    assert_eq!(d.accepted(request), d.accepted(retry));
    assert_eq!(d.accepted(request), d.accepted(retry_during_commit));
    assert_eq!(d.started.len(), 5);
    assert!(d.compaction.is_none());
    d.reopen();
    d.settle();
    checked(d);
}
#[test]
fn oversized_protected_content_rejects_after_one_compaction_attempt() {
    let mut d = before_overflow();
    let turns = d.runtime.snapshot().turns().count();
    let (request, _) = d.start(&"u".repeat(8000));
    assert!(d.reply(request).is_none());
    d.propose_summary();
    d.settle();
    assert!(matches!(
        d.rejected(request),
        Rejection::ContextBudgetExceeded { .. }
    ));
    assert_eq!(
        d.runtime.snapshot().turns().count(),
        turns,
        "no oversized user entry or turn is written"
    );
    assert!(d.compaction.is_none() && d.pending.is_none());
    assert_eq!(
        d.segment
            .effects
            .iter()
            .filter(|e| matches!(e, Effect::ProposeCompaction { .. }))
            .count(),
        1
    );
    checked(d);
}
#[test]
fn post_turn_trigger_is_exactly_850_permille_and_never_inside_turn() {
    for (last_bytes, expected) in [(1384, false), (1388, true)] {
        let mut d = Driver::with_configuration(
            budget_config(),
            br#"{"compaction":{"summary_model":{"provider_instance":"summary","model":"cheap"}}}"#,
        );
        for _ in 0..2 {
            plain_turn(&mut d, &"u".repeat(1000), "done");
        }
        let (request, _) = d.start(&"u".repeat(last_bytes));
        d.settle();
        d.accepted(request);
        assert!(d.compaction.is_none());
        d.provider(ProviderEvent::Started);
        d.provider(ProviderEvent::TextDelta {
            block_index: 0,
            text: "done".into(),
        });
        assert!(d.compaction.is_none());
        d.provider(ProviderEvent::Finished {
            outcome: ProviderOutcome::Completed,
            finish_reason: Some(FinishReason::Stop),
            error: None,
            continuation: None,
        });
        assert!(
            d.compaction.is_none(),
            "terminal response is not durable yet"
        );
        d.settle();
        assert_eq!(d.compaction.is_some(), expected);
        if expected {
            d.propose_summary();
            d.settle();
        }
        checked(d);
    }
}
#[test]
fn idle_and_recovery_guards_refuse_compaction_without_provider_work() {
    let mut d = Driver::new();
    let request = d.command(Command::PreviewCompaction);
    assert_eq!(
        d.rejected(request),
        Rejection::Compaction(CompactionError::NotEligible(NotEligible::Empty))
    );
    let (request, _) = d.start("active");
    d.settle();
    d.accepted(request);
    let request = d.command(Command::PreviewCompaction);
    assert_eq!(d.rejected(request), Rejection::Busy);
    d.reopen_unstarted();
    let request = d.command(Command::PreviewCompaction);
    assert_eq!(d.rejected(request), Rejection::RecoveryRequired);
    d.step(Input::Start);
    d.settle();
    assert!(d.compaction.is_none());
    checked(d);
}
#[test]
fn compaction_publication_failure_answers_held_start_and_faults() {
    let mut d = before_overflow();
    let (request, _) = d.start(&"u".repeat(800));
    d.propose_summary();
    d.fail_pending(PersistError::Io {
        message: "injected publication failure".into(),
    });
    assert_eq!(d.rejected(request), Rejection::Faulted);
    assert_eq!(d.runtime.phase(), Phase::Faulted);
    checked(d);
}

#[test]
fn storage_idle_rule_rejects_compaction_inside_turn() {
    let mut d = prepared();
    let (_, mut preview) = d.manual_preview();
    let (request, _) = d.start("active");
    d.settle();
    d.accepted(request);
    let snapshot = d.runtime.snapshot().clone();
    preview.entry.parent_id = snapshot.head().cloned();
    d.root.put_artifact(&preview.manifest_bytes).unwrap();
    let commit = Commit {
        command_id: None,
        command_fingerprint: None,
        head: HeadChange {
            from: snapshot.head().cloned(),
            to: Some(preview.entry.id.clone()),
            reason: HeadReason::Advance,
        },
        entries: vec![preview.entry],
        events: vec![],
    };
    assert!(matches!(
        d.store.as_mut().unwrap().append(commit, d.env.now()),
        Err(StoreError::Invalid("session_not_idle"))
    ));
    d.reopen();
    d.settle();
    checked(d);
}

#[test]
fn mid_turn_overflow_fails_without_compaction_inside_turn() {
    let mut d = before_overflow();
    let (request, _) = d.start("tool loop");
    d.settle();
    d.accepted(request);
    let turn = d.active_turn().unwrap();
    d.provider(ProviderEvent::Started);
    d.provider(ProviderEvent::ToolCallStarted {
        block_index: 0,
        name: "read".into(),
        provider_call_id: None,
    });
    d.provider(ProviderEvent::ToolArgumentsDelta {
        block_index: 0,
        fragment: serde_json::json!({"path":"x".repeat(2000)}).to_string(),
    });
    d.provider(ProviderEvent::ToolCallReady { block_index: 0 });
    let start = d.segment.effects.len();
    d.provider(ProviderEvent::Finished {
        outcome: ProviderOutcome::Completed,
        finish_reason: Some(FinishReason::ToolCalls),
        error: None,
        continuation: None,
    });
    d.settle();
    assert_eq!(
        d.runtime.snapshot().turn(&turn).unwrap().outcome,
        Some(TurnOutcome::Failed)
    );
    assert!(d.runtime.snapshot().events().any(|(_, _, e)| matches!(e, JournalEvent::TurnFinished(f) if f.turn_id == turn && f.code.as_deref() == Some("context_budget_exceeded"))));
    let effects = &d.segment.effects[start..];
    let finished = effects
        .iter()
        .position(|e| {
            matches!(
                e,
                Effect::Emit(RuntimeEvent {
                    kind: EventKind::TurnFinished { .. },
                    ..
                })
            )
        })
        .unwrap();
    assert!(
        !effects[..finished]
            .iter()
            .any(|e| matches!(e, Effect::ProposeCompaction { .. })),
        "no compaction while the tool turn is active"
    );
    if d.compaction.is_some() {
        let r = d.command(Command::CancelCompaction);
        d.accepted(r);
    }
    checked(d);
}

/// Compaction-specific seeded interleavings supplement the tool scheduler.
/// Called by every seed of the main randomized test, with the storage oracle on.
pub(super) fn randomized(seed: u64) {
    let mut d = prepared();
    let (id, _) = d.manual_preview();
    match seed % 4 {
        0 => {
            let r = d.command(Command::CommitCompaction {
                preview_id: id.clone(),
            });
            d.settle();
            d.accepted(r);
        }
        1 => {
            let head = d.runtime.snapshot().head().cloned();
            let r = d.command(Command::SelectHead { head: None });
            d.settle();
            d.accepted(r);
            let r = d.command(Command::SelectHead { head });
            d.settle();
            d.accepted(r);
        }
        2 => {
            d.command(Command::CommitCompaction {
                preview_id: id.clone(),
            });
            d.crash(match (seed / 4) % 3 {
                0 => Crash::Lost,
                1 => Crash::Durable,
                _ => Crash::Torn,
            });
        }
        _ => {
            d.reopen();
        }
    }
    let stale = d.command(Command::CommitCompaction { preview_id: id });
    assert!(matches!(d.rejected(stale), Rejection::StalePreview { .. }));
    checked(d);

    // Controls, the compaction that skips them, then controls again: the
    // skipped turn can be reset, the covered one cannot be pinned.
    let mut d = prepared();
    let mut turns: Vec<TurnId> = Vec::new();
    for stored in d
        .runtime
        .snapshot()
        .ancestry(d.runtime.snapshot().head())
        .unwrap()
    {
        if let Some(turn) = &stored.entry.turn_id
            && !turns.contains(turn)
        {
            turns.push(turn.clone());
        }
    }
    let controlled = turns[(seed as usize) % 2].clone();
    let covered = turns[(seed as usize + 1) % 2].clone();
    let mode = if seed.is_multiple_of(2) {
        ContextMode::Pinned
    } else {
        ContextMode::Excluded
    };
    let request = d.command(Command::SetContextControl {
        turn_id: controlled.clone(),
        mode,
    });
    d.settle();
    d.accepted(request);
    let (id, _) = d.manual_preview();
    let request = d.command(Command::CommitCompaction { preview_id: id });
    d.settle();
    d.accepted(request);
    let request = d.command(Command::SetContextControl {
        turn_id: covered,
        mode: ContextMode::Pinned,
    });
    d.settle();
    assert_eq!(
        d.rejected(request),
        Rejection::Invalid("context_control_covered")
    );
    let request = d.command(Command::SetContextControl {
        turn_id: controlled,
        mode: ContextMode::Default,
    });
    d.settle();
    d.accepted(request);
    d.reopen();
    d.settle();
    checked(d);
    let mut d = automatic();
    match (seed / 4) % 4 {
        0 => {
            d.propose_summary();
            d.settle();
        }
        1 => {
            let r = d.command(Command::CancelCompaction);
            d.accepted(r);
        }
        2 => {
            d.propose_summary();
            d.crash(match (seed / 16) % 3 {
                0 => Crash::Lost,
                1 => Crash::Durable,
                _ => Crash::Torn,
            });
        }
        _ => {
            let (token, _) = d.compaction.take().unwrap();
            d.step(Input::CompactionProposed {
                token,
                result: Err(CompactionError::Invalid("provider_failure")),
            });
        }
    }
    checked(d);
}
