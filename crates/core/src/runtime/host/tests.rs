use super::*;
use crate::{
    session::{ArtifactRef, Digest, ResultCode},
    tools::{Registry, ToolSet},
};
use serde_json::json;

fn policy(paths: &[&str]) -> PolicySnapshot {
    let bytes = serde_json::to_vec(
        &json!({"policy_revision":"p1", "enabled_tools":["read","write","edit","bash"],
        "workspace_read":"allow", "workspace_mutation":"ask", "process":"ask", "network":"deny",
        "protected_paths":paths}),
    )
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
fn config(root: &Path) -> ToolHostConfig {
    ToolHostConfig {
        workspace_root: root.join("workspace"),
        data_root: root.join("data"),
        sandbox: None,
    }
}
#[test]
fn file_host_and_disabled_shell_have_stable_json() {
    let temp = tempfile::tempdir().unwrap();
    let config = config(temp.path());
    std::fs::create_dir(&config.workspace_root).unwrap();
    let (host, availability) = ToolHost::assemble(&config, &policy(&[]));
    assert_eq!(
        serde_json::to_value(&availability).unwrap(),
        json!({
            "file_tools":{"status":"available"}, "shell":{"status":"unavailable","reasons":["shell_disabled"]}
        })
    );
    let roundtrip: ToolAvailability =
        serde_json::from_value(serde_json::to_value(availability).unwrap()).unwrap();
    assert!(roundtrip.file_tools.is_available());
    let names: Vec<_> = ToolSet::new(host.executors.iter().map(|e| e.backend()))
        .definitions(&policy(&[]))
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(names, ["read", "write", "edit"]);
}
#[test]
fn data_root_requires_policy_mask_including_equal_root() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = config(temp.path());
    std::fs::create_dir(&config.workspace_root).unwrap();
    for suffix in ["", "private", "private/state"] {
        config.data_root = config.workspace_root.join(suffix);
        let (host, report) = ToolHost::assemble(&config, &policy(&[]));
        assert!(host.executors.is_empty());
        assert_eq!(
            report,
            ToolAvailability::unavailable("data_root_unprotected")
        );
        let (_, report) = ToolHost::assemble(
            &config,
            &policy(&[if suffix.is_empty() { "." } else { "private" }]),
        );
        assert!(report.file_tools.is_available(), "{report:?}");
    }
}
#[test]
fn data_root_alias_and_component_boundaries_do_not_bypass_protection() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let mut config = config(temp.path());
    std::fs::create_dir(&config.workspace_root).unwrap();
    symlink(&config.workspace_root, temp.path().join("alias")).unwrap();
    config.data_root = temp.path().join("alias/new-state");
    assert_eq!(
        ToolHost::assemble(&config, &policy(&[])).1,
        ToolAvailability::unavailable("data_root_unprotected")
    );
    config.data_root = config.workspace_root.join("private-other");
    assert_eq!(
        ToolHost::assemble(&config, &policy(&["private"])).1,
        ToolAvailability::unavailable("data_root_unprotected")
    );
    std::fs::create_dir(config.workspace_root.join("private")).unwrap();
    symlink(
        config.workspace_root.join("private"),
        temp.path().join("inner-alias"),
    )
    .unwrap();
    for path in [
        config.workspace_root.join("../data"),
        temp.path().join("inner-alias/../new-state"),
    ] {
        config.data_root = path;
        assert_eq!(
            ToolHost::assemble(&config, &policy(&[])).1,
            ToolAvailability::unavailable("root_resolution_failed")
        );
    }
    symlink(
        config.workspace_root.join("missing"),
        temp.path().join("dangling-alias"),
    )
    .unwrap();
    config.data_root = temp.path().join("dangling-alias/new-state");
    assert_eq!(
        ToolHost::assemble(&config, &policy(&[])).1,
        ToolAvailability::unavailable("root_resolution_failed")
    );
}
#[test]
fn configured_protection_follows_renamed_object_not_just_spelling() {
    let temp = tempfile::tempdir().unwrap();
    let config = config(temp.path());
    std::fs::create_dir(&config.workspace_root).unwrap();
    std::fs::create_dir(config.workspace_root.join("private")).unwrap();
    std::fs::write(config.workspace_root.join("private/token"), "secret").unwrap();
    let p = policy(&["private"]);
    let (host, availability) = ToolHost::assemble(&config, &p);
    assert!(availability.file_tools.is_available());
    std::fs::rename(
        config.workspace_root.join("private"),
        config.workspace_root.join("renamed"),
    )
    .unwrap();
    // The planner's string-based policy masks remain in the plan. Only startup
    // descriptor protection detects this different spelling of the same object.
    let registry = Registry::builtin();
    let tool = registry.resolve("read", &p).unwrap();
    let operation = tool.validate(&json!({"path":"renamed/token"})).unwrap();
    let call = crate::session::CallRef {
        response_entry_id: crate::session::EntryId::new(),
        call_id: crate::session::CallId::new(),
    };
    let refusal = tool
        .plan(&operation, &call, host.workspace.as_ref(), &p)
        .unwrap_err();
    assert_eq!(refusal.code, ResultCode::ProtectedPath);
}
#[test]
fn relative_config_paths_fail_closed() {
    let config = ToolHostConfig {
        workspace_root: "relative".into(),
        data_root: "/data".into(),
        sandbox: None,
    };
    assert!(
        ToolHost::assemble(&config, &policy(&[]))
            .0
            .executors
            .is_empty()
    );
}
#[test]
fn remember_preserves_protected_paths() {
    let p = policy(&["private", ".harness"]);
    let allowance =
        crate::tools::Allowance::parse(&json!({"tool":"bash","command":"true","cwd":"."})).unwrap();
    let bytes = p.grant(allowance, "p2");
    let remembered = PolicySnapshot::parse(
        ArtifactRef {
            sha256: Digest::of(&bytes),
            bytes: bytes.len() as u64,
        },
        &bytes,
    )
    .unwrap();
    assert_eq!(remembered.protected_paths(), p.protected_paths());
}

#[test]
fn probe_reason_categories_never_publish_raw_diagnostics() {
    for reason in [
        "configured sandbox probe failed: SECRET_CAPTURE_SENTINEL SECRET_PATH_SENTINEL",
        "descriptor audit: SECRET_TARGET_SENTINEL",
        "environment leaked: SECRET_ENV_SENTINEL",
        "setup failed: SECRET_ENV_SENTINEL",
    ] {
        assert!(!sandbox_reason(reason).contains("SECRET_"));
    }
}
