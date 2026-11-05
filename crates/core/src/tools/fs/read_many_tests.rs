//! M9 line-addressed and batched reads through the trusted filesystem executor.

use super::{FsProtectedPaths, FsWorkspace};
use crate::{
    session::{ArtifactRef, CallRef, ContentBlock, Digest, ResultCode, ToolStatus},
    tools::{CancelToken, ExecutionOutcome, Plan, PolicySnapshot, Registry},
};
use serde_json::{Value, json};
use std::fs;
use tempfile::TempDir;

fn workspace(root: &TempDir) -> FsWorkspace {
    FsWorkspace::open(root.path(), &FsProtectedPaths::default()).unwrap()
}

fn policy(protected: &[&str]) -> PolicySnapshot {
    let bytes = serde_json::to_vec(&json!({
        "policy_revision":"read-many-v1",
        "enabled_tools":["read","write","edit","bash","list","glob","grep"],
        "workspace_read":"allow",
        "workspace_mutation":"ask",
        "process":"ask",
        "network":"deny",
        "protected_paths":protected
    }))
    .unwrap();
    PolicySnapshot::parse(
        ArtifactRef {
            sha256: Digest::of(&bytes),
            bytes: bytes.len() as u64,
        },
        &bytes,
    )
    .unwrap()
}

fn plan_with_policy(
    view: &FsWorkspace,
    args: Value,
    policy: &PolicySnapshot,
) -> Result<Plan, crate::tools::Refusal> {
    let tool = Registry::builtin().resolve("read", policy).unwrap();
    tool.plan(
        &tool.validate(&args)?,
        &CallRef {
            response_entry_id: "019a0000-0000-7000-8000-00000000019a".parse().unwrap(),
            call_id: "019a0000-0000-7000-8000-00000000019b".parse().unwrap(),
        },
        view,
        policy,
    )
}

fn plan(view: &FsWorkspace, args: Value) -> Plan {
    plan_with_policy(view, args, &policy(&[])).unwrap()
}

fn execute(view: &FsWorkspace, args: Value) -> ExecutionOutcome {
    view.execute(&plan(view, args), &CancelToken::new())
}

fn text(outcome: &ExecutionOutcome) -> &str {
    assert_eq!(outcome.status, ToolStatus::Succeeded, "{outcome:?}");
    let ContentBlock::Text { text } = &outcome.content[0] else {
        panic!("text output expected");
    };
    text
}

#[test]
fn line_ranges_preserve_crlf_utf8_and_eof_rules() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("lines"), "one\r\ntwø\nlast").unwrap();
    let view = workspace(&root);

    let crlf = execute(
        &view,
        json!({"path":"lines", "start_line":1, "line_count":1}),
    );
    assert!(text(&crlf).ends_with("one\r\n"));

    let result = execute(
        &view,
        json!({"path":"lines", "start_line":2, "line_count":2}),
    );
    assert!(text(&result).contains("twø\nlast"));
    assert!(text(&result).contains("lines 2+2"));
    assert!(!result.output_truncated);

    let trailing_root = TempDir::new().unwrap();
    fs::write(trailing_root.path().join("lines"), "one\n").unwrap();
    let trailing = workspace(&trailing_root);
    let no_phantom = execute(
        &trailing,
        json!({"path":"lines", "start_line":2, "line_count":1}),
    );
    assert!(text(&no_phantom).contains("lines 2+0"));
    assert!(text(&no_phantom).ends_with('\n'));
}

#[test]
fn line_output_caps_long_lines_at_utf8_boundaries_and_reports_truncation() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("long"), "aé€z\n".repeat(1000)).unwrap();
    let view = workspace(&root);
    let result = execute(
        &view,
        json!({"path":"long", "start_line":1, "line_count":1, "max_bytes":4}),
    );
    assert!(text(&result).contains("\naé"));
    assert!(text(&result).contains("output_truncated=true"));
    assert!(result.output_truncated);
    assert!(text(&result).len() <= 32 * 1024);
}

#[test]
fn eight_paths_are_ordered_bounded_and_explicitly_truncated_per_file() {
    let root = TempDir::new().unwrap();
    let mut paths = Vec::new();
    for index in 0..8 {
        let name = format!("f{index}");
        fs::write(root.path().join(&name), "x\n".repeat(10_000)).unwrap();
        paths.push(json!({"path":name, "start_line":1, "line_count":10_000}));
    }
    let view = workspace(&root);
    let args = json!({"paths":paths});
    let result = execute(&view, args.clone());
    let rendered = text(&result);
    assert!(rendered.len() <= 32 * 1024);
    assert!(result.output_truncated);
    assert_eq!(rendered.matches("output_truncated=").count(), 8);
    assert!(rendered.contains("[0] lines 1+10000"));
    assert!(rendered.contains("[7] lines 1+10000"));
    assert_eq!(rendered, text(&execute(&view, args)));
}

#[test]
fn schema_and_planning_reject_mixed_paths_and_protected_hardlinked_or_symlinked_targets() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("ok"), "safe").unwrap();
    fs::write(root.path().join("secret"), "secret").unwrap();
    fs::hard_link(root.path().join("secret"), root.path().join("hard")).unwrap();
    std::os::unix::fs::symlink("ok", root.path().join("alias")).unwrap();
    let view = workspace(&root);
    let policy = policy(&["secret"]);

    for (args, code) in [
        (
            json!({"path":"ok", "paths":[{"path":"ok"}]}),
            ResultCode::InvalidArguments,
        ),
        (
            json!({"paths":[{"path":"ok"}, {"path":"secret"}]}),
            ResultCode::ProtectedPath,
        ),
        (
            json!({"paths":[{"path":"ok"}, {"path":"hard"}]}),
            ResultCode::HardLinkRejected,
        ),
        (
            json!({"paths":[{"path":"ok"}, {"path":"alias"}]}),
            ResultCode::SymlinkRejected,
        ),
    ] {
        assert_eq!(
            plan_with_policy(&view, args, &policy).unwrap_err().code,
            code
        );
    }
}

#[test]
fn changed_ranges_and_paths_change_the_bound_plan() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("a"), "a\nb\n").unwrap();
    fs::write(root.path().join("b"), "x\ny\n").unwrap();
    let view = workspace(&root);
    let first = plan(
        &view,
        json!({"paths":[{"path":"a", "start_line":1, "line_count":1}]}),
    );
    let changed_range = plan(
        &view,
        json!({"paths":[{"path":"a", "start_line":2, "line_count":1}]}),
    );
    let changed_path = plan(
        &view,
        json!({"paths":[{"path":"b", "start_line":1, "line_count":1}]}),
    );
    assert_ne!(first.digest(), changed_range.digest());
    assert_ne!(first.digest(), changed_path.digest());
}
