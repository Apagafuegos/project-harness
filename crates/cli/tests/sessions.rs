//! Process-level catalog/context checks stay separate from concurrent ask reopens.
use harness::wiring::{self, SessionOptions};
use harness_core::{model::scripted::ScriptedProvider, runtime::DriverConfig, session::*};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

fn files(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
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
fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn sessions_help_empty_missing_and_relative_root() {
    let help = run(&["sessions", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--data-root <DIR>"));
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("absent");
    let output = run(&["sessions", "--data-root", path.to_str().unwrap()]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "[]\n");
    assert!(!path.exists());
    let output = run(&["sessions", "--data-root", "relative"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}
#[test]
fn catalog_json_and_context_include_new_session_instructions_without_modification() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(workspace.join("AGENTS.md"), "project rules").unwrap();
    let path = temp.path().join("state");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let opened = rt
        .block_on(wiring::open_session(
            &SessionOptions {
                data_root: path.clone(),
                workspace,
                session: None,
                model: ModelRef {
                    provider_instance: "mock".into(),
                    model: "scripted".into(),
                },
                explicit_model: false,
                explicit_provider: false,
                switch_policy: Portability::Reject,
                known_instances: vec!["mock".into()],
                summary_model: None,
                compaction_strategy: harness::wiring::creation_compaction_strategy(),
                explicit_strategy: false,
                driver: DriverConfig::default(),
                sandbox: None,
            },
            Arc::new(ScriptedProvider::new([])),
        ))
        .unwrap();
    assert!(opened.warnings.is_empty());
    let id = opened.session.to_string();
    rt.block_on(opened.handle.shutdown());
    let before = files(&path);
    let output = run(&["sessions", "--data-root", path.to_str().unwrap()]);
    assert!(output.status.success(), "{output:?}");
    let entries: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(entries.as_array().unwrap().len(), 1);
    let entry = &entries[0];
    assert_eq!(entry["session_id"], id);
    assert_eq!(entry["status"], "ok");
    assert_eq!(entry["state"], "idle");
    assert_eq!(entry["model"]["model"], "scripted");
    assert!(entry["title"].is_null());
    assert_eq!(entry["created_at"], entry["last_activity"]);
    assert_eq!(files(&path), before);
    let output = run(&[
        "context",
        path.to_str().unwrap(),
        &id,
        "--context-tokens",
        "128000",
    ]);
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["instructions"]["status"], "included");
    assert_eq!(report["instructions"]["count"], 2);
    assert!(report["instructions"]["estimated_tokens"].as_u64().unwrap() > 0);
    assert_eq!(
        report["instructions"]["estimated_tokens"],
        report["totals"]["estimated_instructions"]
    );
    assert_eq!(files(&path), before);
    let root = Arc::new(DataRoot::open(&path, false).unwrap());
    let _writer = JournalStore::open(root, &opened.session).unwrap();
    let output = run(&["sessions", "--data-root", path.to_str().unwrap()]);
    assert!(output.status.success(), "{output:?}");
    let entries: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(entries[0]["status"], "busy");
    assert_eq!(entries[0]["reason"], "session_locked");
    assert_eq!(files(&path), before);
}
