use super::*;
use std::{collections::BTreeMap, fs, io::Write, path::Path, sync::Arc};

fn at(n: u8) -> Timestamp {
    format!("2026-10-31T12:00:{n:02}Z").parse().unwrap()
}
fn root() -> (tempfile::TempDir, Arc<DataRoot>) {
    let temp = tempfile::tempdir().unwrap();
    let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
    (temp, root)
}
fn session(root: &Arc<DataRoot>, time: u8, title: Option<&str>) -> SessionId {
    let id = SessionId::new();
    let configuration = root.put_artifact(b"{}").unwrap();
    let mut store = JournalStore::create(
        root.clone(),
        id.clone(),
        SessionHeader {
            workspace: "/workspace".into(),
            initial_model: ModelRef {
                provider_instance: "mock".into(),
                model: "first".into(),
            },
            policy_revision: "initial".into(),
            configuration,
        },
        at(0),
    )
    .unwrap();
    if let Some(title) = title {
        let snapshot = store.snapshot();
        let turn = TurnId::new();
        let entry = EntryId::new();
        store
            .append(
                Commit {
                    command_id: None,
                    command_fingerprint: None,
                    head: HeadChange {
                        from: None,
                        to: Some(entry.clone()),
                        reason: HeadReason::Advance,
                    },
                    entries: vec![Entry {
                        id: entry.clone(),
                        parent_id: None,
                        turn_id: Some(turn.clone()),
                        payload: EntryPayload::UserMessage(UserMessage {
                            content: vec![ContentBlock::Text { text: title.into() }],
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
                },
                at(time),
            )
            .unwrap();
    } else {
        let entry = EntryId::new();
        store
            .append(
                Commit {
                    command_id: None,
                    command_fingerprint: None,
                    head: HeadChange {
                        from: None,
                        to: Some(entry.clone()),
                        reason: HeadReason::Advance,
                    },
                    entries: vec![Entry {
                        id: entry,
                        parent_id: None,
                        turn_id: None,
                        payload: EntryPayload::ModelChanged(ModelChanged {
                            model: ModelRef {
                                provider_instance: "mock".into(),
                                model: "changed".into(),
                            },
                            portability: Portability::Reject,
                        }),
                    }],
                    events: vec![],
                },
                at(time),
            )
            .unwrap();
    }
    id
}
fn files(path: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    for item in fs::read_dir(path).unwrap() {
        let path = item.unwrap().path();
        if path.is_dir() {
            out.extend(files(&path));
        } else {
            out.insert(path.clone(), fs::read(path).unwrap());
        }
    }
    out
}
#[test]
fn empty_catalog_and_strays_are_ignored() {
    let (_temp, root) = root();
    assert!(root.sessions().unwrap().is_empty());
    for name in [
        "notes",
        "invalid.jsonl",
        "019a0000-0000-7000-8000-000000000001.txt",
    ] {
        fs::write(root.path().join("sessions").join(name), b"not a journal").unwrap();
    }
    assert!(root.sessions().unwrap().is_empty());
}
#[test]
fn orders_committed_activity_and_titles_deterministically() {
    let (_temp, root) = root();
    let a = session(&root, 5, Some("first line\nnot the title"));
    let b = session(&root, 7, Some(&"é".repeat(140)));
    let c = session(&root, 7, None);
    let entries = root.sessions().unwrap();
    let mut tied = [b.clone(), c.clone()];
    tied.sort();
    assert_eq!(
        entries.iter().map(|e| &e.session_id).collect::<Vec<_>>(),
        vec![&tied[0], &tied[1], &a]
    );
    for e in &entries {
        assert_eq!(e.status, CatalogStatus::Ok);
        assert_eq!(e.workspace.as_deref(), Some("/workspace"));
        assert_eq!(e.created_at, Some(at(0)));
        if e.session_id == a {
            assert_eq!(e.title.as_deref(), Some("first line"));
            assert_eq!(e.last_activity, Some(at(5)));
            assert_eq!(e.state, Some(RuntimeState::Preparing));
        }
        if e.session_id == b {
            assert_eq!(e.title.as_deref(), Some("é".repeat(120).as_str()));
        }
        if e.session_id == c {
            assert!(e.title.is_none());
            assert_eq!(e.model.as_ref().unwrap().model, "changed");
            assert_eq!(e.state, Some(RuntimeState::Idle));
        }
    }
}
#[test]
fn live_writer_is_busy_and_listing_never_takes_exclusive_lock_or_changes_bytes() {
    let (_temp, root) = root();
    let id = session(&root, 1, None);
    let writer = JournalStore::open(root.clone(), &id).unwrap();
    let before = files(root.path());
    assert_eq!(root.sessions().unwrap()[0].status, CatalogStatus::Busy);
    assert_eq!(files(root.path()), before);
    drop(writer);
    let shared = root.journal(&id, false, false).unwrap();
    rustix::fs::flock(&shared, FlockOperation::NonBlockingLockShared).unwrap();
    assert_eq!(
        root.sessions().unwrap()[0].status,
        CatalogStatus::Ok,
        "catalog must coexist with shared readers"
    );
    assert_eq!(files(root.path()), before);
}
#[test]
fn torn_corrupt_unsupported_and_unsafe_sessions_stay_listed_without_changes() {
    let (_temp, root) = root();
    let torn = session(&root, 2, Some("repair me"));
    let corrupt = session(&root, 3, None);
    let unsupported = SessionId::new();
    for (id, suffix) in [
        (&torn, b"{\"torn\":".as_slice()),
        (&corrupt, b"{\"bad\":true}\n".as_slice()),
    ] {
        root.journal(id, false, true)
            .unwrap()
            .write_all(suffix)
            .unwrap();
    }
    root.journal(&unsupported, true, true)
        .unwrap()
        .write_all(b"{\"record\":{\"version\":99},\"sha256\":\"bad\"}\n")
        .unwrap();
    let unsafe_id = SessionId::new();
    std::os::unix::fs::symlink(
        "/etc/passwd",
        root.path()
            .join("sessions")
            .join(format!("{unsafe_id}.jsonl")),
    )
    .unwrap();
    let before = files(root.path());
    let entries = root.sessions().unwrap();
    assert_eq!(entries.len(), 4);
    for e in entries {
        if e.session_id == torn {
            assert_eq!(e.status, CatalogStatus::NeedsRepair);
            assert_eq!(e.title.as_deref(), Some("repair me"));
            assert_eq!(e.last_activity, Some(at(2)));
        } else {
            assert_eq!(e.status, CatalogStatus::Unreadable);
            assert!(e.reason.is_some());
        }
    }
    assert_eq!(files(root.path()), before);
}
