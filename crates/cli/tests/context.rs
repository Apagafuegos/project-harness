//! `harness context` against the RFC-001 compaction fixture.

use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::Arc,
    time::{Duration, Instant},
};

use harness_core::session::{DataRoot, JournalStore, SessionId};

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A private data root holding the compaction fixture and its artifacts.
fn fixture() -> (tempfile::TempDir, PathBuf, SessionId) {
    let examples = repository().join("docs/rfcs/001/examples");
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let root = DataRoot::open(&path, true).unwrap();
    for entry in fs::read_dir(examples.join("artifacts")).unwrap() {
        root.put_artifact(&fs::read(entry.unwrap().path()).unwrap())
            .unwrap();
    }
    let journal = fs::read(examples.join("compaction.jsonl")).unwrap();
    let session: SessionId = "019a0000-0000-7000-8000-000000000003".parse().unwrap();
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path.join("sessions").join(format!("{session}.jsonl")))
        .unwrap()
        .write_all(&journal)
        .unwrap();
    (temp, path, session)
}

fn harness(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(arguments)
        .output()
        .unwrap()
}

#[test]
fn prints_the_golden_report_of_the_compaction_fixture() {
    let (_temp, root, session) = fixture();
    let root = root.to_str().unwrap();
    let session = session.to_string();
    let output = harness(&["context", root, &session, "--context-tokens", "128000"]);
    assert!(output.status.success(), "{output:?}");
    let golden =
        fs::read_to_string(repository().join("crates/core/src/context/golden/compaction.json"))
            .unwrap();
    assert_eq!(String::from_utf8(output.stdout).unwrap(), golden);

    // Without a limit the budget is unknown.
    let output = harness(&["context", root, &session]);
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["budget"]["status"], "unknown");
    assert_eq!(report["budget"]["reason"], "model_not_in_catalog");

    // A preview for another model, from an earlier head.
    let output = harness(&[
        "context",
        root,
        &session,
        "--head",
        "019a0000-0000-7000-8000-000000000259",
        "--model",
        "other/model",
        "--context-tokens",
        "1000",
    ]);
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["flags"]["preview"], true);
    assert_eq!(report["identity"]["model"]["provider_instance"], "other");
    assert_eq!(report["budget"]["context_tokens"], 1000);
    assert_eq!(report["included"].as_array().unwrap().len(), 3);
}

#[test]
fn refuses_a_locked_session_at_once_and_reports_bad_input() {
    let (_temp, path, session) = fixture();
    let root = path.to_str().unwrap();
    let id = session.to_string();
    let store =
        JournalStore::open(Arc::new(DataRoot::open(&path, false).unwrap()), &session).unwrap();
    let started = Instant::now();
    let output = harness(&["context", root, &id]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let message = String::from_utf8(output.stderr).unwrap();
    assert!(
        message.contains("is locked by a running engine") && message.contains("does not wait"),
        "{message}"
    );
    assert!(started.elapsed() < Duration::from_secs(30), "it waited");
    drop(store);

    let unknown = harness(&[
        "context",
        root,
        &id,
        "--head",
        &SessionId::new().to_string(),
    ]);
    assert!(!unknown.status.success());
    assert!(
        String::from_utf8(unknown.stderr)
            .unwrap()
            .contains("unknown_head")
    );

    let missing = harness(&["context", root, &SessionId::new().to_string()]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8(missing.stderr)
            .unwrap()
            .contains("not found")
    );

    let usage = harness(&["context", root, &id, "--model", "no-slash"]);
    assert!(!usage.status.success());
    assert!(
        String::from_utf8(usage.stderr)
            .unwrap()
            .contains("PROVIDER/MODEL")
    );
}
