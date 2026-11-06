//! `harness journal inspect` and `harness journal repair` on a torn journal.

use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use harness_core::session::{DataRoot, Digest, SessionId};
use serde_json::Value;

const SESSION: &str = "019a0000-0000-7000-8000-000000000003";
/// The unterminated start of a record a crash cut short.
const TAIL: &[u8] = b"{\"record\":{\"version\":1,\"seq\":";

/// A private data root holding the compaction fixture, with `suffix`
/// appended to its journal.
fn torn(suffix: &[u8]) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/rfcs/001/examples");
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let root = DataRoot::open(&path, true).unwrap();
    for entry in fs::read_dir(examples.join("artifacts")).unwrap() {
        root.put_artifact(&fs::read(entry.unwrap().path()).unwrap())
            .unwrap();
    }
    let mut journal = fs::read(examples.join("compaction.jsonl")).unwrap();
    journal.extend_from_slice(suffix);
    let file = path.join("sessions").join(format!("{SESSION}.jsonl"));
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&file)
        .unwrap()
        .write_all(&journal)
        .unwrap();
    (temp, path, file)
}

fn harness(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(arguments)
        .output()
        .unwrap()
}

fn inspect(root: &str) -> Value {
    let output = harness(&["journal", "inspect", root, SESSION]);
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn repair(root: &str, fingerprint: &str, size: &str) -> Output {
    harness(&[
        "journal",
        "repair",
        root,
        SESSION,
        "--fingerprint",
        fingerprint,
        "--size",
        size,
    ])
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

#[test]
fn inspect_then_repair_only_the_inspected_torn_tail() {
    let (_temp, path, file) = torn(TAIL);
    let root = path.to_str().unwrap();
    let bytes = fs::read(&file).unwrap();
    let inspection = inspect(root);
    assert_eq!(inspection["session_id"], SESSION);
    assert_eq!(inspection["file_bytes"], bytes.len());
    assert_eq!(inspection["valid_bytes"], bytes.len() - TAIL.len());
    assert_eq!(inspection["fingerprint"], Digest::of(&bytes).as_str());
    assert_eq!(inspection["diagnostic"]["kind"], "torn_tail");
    assert_eq!(inspection["diagnostic"]["code"], "unterminated_tail");
    assert_eq!(inspection["repairable"], true);
    assert_eq!(inspection["quarantine_bytes"], TAIL.len());
    // The fixture's eleven v1 records; the torn tail is not counted.
    assert_eq!(inspection["versions"], serde_json::json!({"1": 11}));
    let prefix = &inspection["valid_prefix"];
    assert_eq!(prefix["state"], "idle");
    assert_eq!(prefix["requires_recovery"], false);
    let fingerprint = inspection["fingerprint"].as_str().unwrap();
    let size = inspection["file_bytes"].to_string();

    // Neither value is chosen for the user, and a mismatch changes nothing.
    let usage = harness(&["journal", "repair", root, SESSION]);
    assert!(!usage.status.success());
    assert!(
        stderr(&usage).contains("--fingerprint"),
        "{}",
        stderr(&usage)
    );
    for (fingerprint, size) in [
        (Digest::of(b"other").to_string(), size.clone()),
        (fingerprint.to_owned(), (bytes.len() - 1).to_string()),
    ] {
        let stale = repair(root, &fingerprint, &size);
        assert!(!stale.status.success());
        assert!(
            stderr(&stale).contains("stale_repair"),
            "{}",
            stderr(&stale)
        );
        assert_eq!(fs::read(&file).unwrap(), bytes);
    }

    let repaired = repair(root, fingerprint, &size);
    assert!(repaired.status.success(), "{repaired:?}");
    let report: Value = serde_json::from_slice(&repaired.stdout).unwrap();
    assert_eq!(report["original_size"], bytes.len());
    assert_eq!(report["valid_bytes"], bytes.len() - TAIL.len());
    let artifact = &report["quarantine_artifact"];
    assert_eq!(artifact["bytes"], TAIL.len());
    let quarantined = path
        .join("artifacts")
        .join(artifact["sha256"].as_str().unwrap());
    assert_eq!(fs::read(quarantined).unwrap(), TAIL);

    let after = inspect(root);
    assert_eq!(after["diagnostic"], Value::Null);
    assert_eq!(after["repairable"], false);
    // The repair commit continues the v1 journal as the current version.
    assert_eq!(after["versions"], serde_json::json!({"1": 11, "5": 1}));
    assert_eq!(after["valid_bytes"], after["file_bytes"]);
    // A repaired journal has nothing left to repair.
    let again = repair(
        root,
        after["fingerprint"].as_str().unwrap(),
        &after["file_bytes"].to_string(),
    );
    assert!(!again.status.success());
    assert!(stderr(&again).contains("tail_not_repairable"));
}

#[test]
fn a_complete_corrupt_record_is_never_repaired() {
    let (_temp, path, file) = torn(b"{\"not\":\"a record\"}\n");
    let root = path.to_str().unwrap();
    let bytes = fs::read(&file).unwrap();
    let inspection = inspect(root);
    assert_eq!(inspection["diagnostic"]["kind"], "corrupt");
    assert_eq!(inspection["repairable"], false);
    assert_eq!(inspection["quarantine_bytes"], Value::Null);
    let refused = repair(
        root,
        inspection["fingerprint"].as_str().unwrap(),
        &inspection["file_bytes"].to_string(),
    );
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("tail_not_repairable"));
    assert_eq!(fs::read(&file).unwrap(), bytes);

    let missing = harness(&["journal", "inspect", root, &SessionId::new().to_string()]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("not found"));
}
