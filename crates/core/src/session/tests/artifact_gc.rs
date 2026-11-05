//! Gate the exact selection/unlink gap while another thread reuses old bytes.
use super::*;
use crate::session::io::Gate;
use std::{
    sync::mpsc,
    time::{Duration, SystemTime},
};

fn gate(root: &DataRoot, point: Point) -> (tokio::sync::oneshot::Receiver<()>, mpsc::Sender<()>) {
    let (entered, arrived) = tokio::sync::oneshot::channel();
    let (release, wait) = mpsc::channel();
    root.io.test.lock().unwrap().gate = Some(Gate {
        point,
        entered,
        release: wait,
    });
    (arrived, release)
}

#[test]
fn artifact_gc_serializes_selection_and_unlink_with_dedupe_reuse() {
    let (_temp, root) = environment();
    let bytes = br#"{"policy":"reused configuration"}"#;
    let reference = root.put_artifact(bytes).unwrap();
    let path = root
        .path()
        .join("artifacts")
        .join(reference.sha256.as_str());
    fs::File::open(&path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(48 * 3600))
        .unwrap();
    let (selected, finish_gc) = gate(&root, Point::GcSelected);
    let collector = {
        let root = root.clone();
        std::thread::spawn(move || root.collect_artifacts(&GcOptions::default()).unwrap())
    };
    selected.blocking_recv().unwrap();

    // A separately opened DataRoot must contend too. Its synced temp contains
    // the same digest, but publication/reuse must wait for the collector.
    let writer_root = Arc::new(DataRoot::open(root.path(), false).unwrap());
    let (synced, publish) = gate(&writer_root, Point::ArtifactSynced);
    let (finished, completion) = mpsc::channel();
    let writer = {
        let writer_root = writer_root.clone();
        std::thread::spawn(move || {
            let result = writer_root.put_artifact(bytes);
            finished.send(()).unwrap();
            result
        })
    };
    synced.blocking_recv().unwrap();
    // If the shared writer lock is removed, trap publication here until gc has
    // unlinked the old digest. That broken ordering deterministically loses it.
    let (mut renamed, finish_writer) = gate(&writer_root, Point::ArtifactRenamed);
    publish.send(()).unwrap();
    let published_while_gc_paused = completion.recv_timeout(Duration::from_millis(300)).is_ok();
    let reached_rename_while_gc_paused = renamed.try_recv().is_ok();
    finish_gc.send(()).unwrap();
    let report = collector.join().unwrap();
    // Release the post-publication gate in either ordering; never strand a worker
    // when a mutation is under test.
    finish_writer.send(()).unwrap();
    let reused = writer.join().unwrap().unwrap();
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(reused, reference);
    root.verify_artifact(&reused).unwrap();
    assert!(!published_while_gc_paused);
    assert!(
        !reached_rename_while_gc_paused,
        "publication bypassed gc's exclusive lock"
    );

    let id = SessionId::new();
    let store = JournalStore::create(
        root.clone(),
        id.clone(),
        SessionHeader {
            workspace: "/workspace".into(),
            initial_model: ModelRef {
                provider_instance: "mock".into(),
                model: "scripted".into(),
            },
            policy_revision: "initial".into(),
            configuration: reused,
        },
        at(),
    )
    .unwrap();
    drop(store);
    let reopened = JournalStore::open(root.clone(), &id).unwrap();
    assert_eq!(reopened.snapshot().configuration(), &reference);
    drop(reopened);

    // The reverse ordering: completed dedupe refresh precedes gc's lock, so
    // even an unreferenced artifact is now young and must be retained.
    root.delete_session(&id).unwrap();
    fs::File::open(&path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(48 * 3600))
        .unwrap();
    writer_root.put_artifact(bytes).unwrap();
    assert!(
        root.collect_artifacts(&GcOptions::default())
            .unwrap()
            .candidates
            .is_empty()
    );
    root.verify_artifact(&reference).unwrap();
}

#[test]
fn journal_export_checks_the_copied_bytes_not_only_the_verified_source() {
    let (temp, root, store) = fresh();
    let id = store.snapshot().session_id().clone();
    let artifact = store.snapshot().configuration().clone();
    drop(store);
    let path = root.path().join("artifacts").join(artifact.sha256.as_str());
    let original = fs::read(&path).unwrap();
    let (verified, finish_copy) = gate(&root, Point::ExportArtifactVerified);
    let destination = temp.path().join("copy");
    let exporter = {
        let root = root.clone();
        let destination = destination.clone();
        std::thread::spawn(move || root.export_journal(&id, &destination))
    };
    verified.blocking_recv().unwrap();
    let mut corrupt = original.clone();
    corrupt[0] ^= 1;
    fs::write(&path, &corrupt).unwrap();
    finish_copy.send(()).unwrap();
    assert!(matches!(
        exporter.join().unwrap(),
        Err(MaintenanceError::ArtifactInvalid(_))
    ));
    assert!(!destination.join("sessions").exists());
    assert!(!destination.join("artifacts").exists());
}

/// Audit follow-up: a failed journal export removes only what it created.
#[test]
fn failed_journal_export_removes_the_empty_directory_it_created() {
    let (temp, root, store) = fresh();
    let id = store.snapshot().session_id().clone();
    drop(store);
    // Injected after the destination and its children were created.
    root.io.test.lock().unwrap().fail = Some(Point::ExportArtifactVerified);
    let destination = temp.path().join("exported");
    assert!(root.export_journal(&id, &destination).is_err());
    assert!(
        !destination.exists(),
        "the destination directory this export created must be gone"
    );
    assert!(
        temp.path().is_dir(),
        "the parent directory it did not create must survive"
    );
}

#[test]
fn a_failed_export_never_removes_a_preexisting_empty_destination() {
    let (temp, root, store) = fresh();
    let id = store.snapshot().session_id().clone();
    drop(store);
    let destination = temp.path().join("mine");
    fs::create_dir(&destination).unwrap();
    root.io.test.lock().unwrap().fail = Some(Point::ExportArtifactVerified);
    assert!(matches!(
        root.export_journal(&id, &destination),
        Err(MaintenanceError::Exists)
    ));
    assert!(
        destination.is_dir(),
        "a pre-existing directory is untouched"
    );
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
}

#[test]
fn a_failed_export_never_touches_a_preexisting_nonempty_destination() {
    let (temp, root, store) = fresh();
    let id = store.snapshot().session_id().clone();
    drop(store);
    let destination = temp.path().join("mine");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("keep"), b"user data").unwrap();
    root.io.test.lock().unwrap().fail = Some(Point::ExportArtifactVerified);
    assert!(matches!(
        root.export_journal(&id, &destination),
        Err(MaintenanceError::Exists)
    ));
    assert_eq!(fs::read(destination.join("keep")).unwrap(), b"user data");
}
