//! The approval prompt the headless client writes: bound plan scope, never values.
use super::*;

/// Decision 10: the `ask` prompt lists every mount the approved plan binds,
/// the read-only registry included, with environment key names only.
#[test]
fn approval_text_names_every_bound_sandbox_mount() {
    let plan = serde_json::to_vec(&serde_json::json!({
        "name": "bash",
        "sandbox": {
            "toolchain_paths": ["/opt/rust"],
            "path_prepend": ["/opt/rust/bin"],
            "cargo_registry": "/opt/registry",
            "env": {"RUSTUP_TOOLCHAIN": "1.99.1"}
        }
    }))
    .unwrap();
    let arguments = serde_json::json!({"command": "cargo test --offline"});
    let text = approval_text("bash", &arguments, &plan);
    assert!(text.starts_with("tool: bash "), "{text}");
    assert!(text.contains("toolchain (read-only): /opt/rust"), "{text}");
    assert!(
        text.contains("cargo registry (read-only): /opt/registry"),
        "{text}"
    );
    assert!(
        text.contains("sandbox PATH: /opt/rust/bin:/usr/bin:/bin"),
        "{text}"
    );
    assert!(
        text.contains("sandbox env keys: RUSTUP_TOOLCHAIN"),
        "{text}"
    );
    assert!(
        !text.contains("1.99.1"),
        "values are never rendered: {text}"
    );
    // A bash plan with no `[sandbox]` table keeps its v0.1 text exactly.
    let plain = serde_json::to_vec(&serde_json::json!({"name": "bash"})).unwrap();
    let text = approval_text("bash", &arguments, &plain);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(!text.contains("cargo registry"), "{text}");
}

#[test]
fn approval_text_shows_bound_shell_options_without_toolchains() {
    let arguments = serde_json::json!({"command":"false | cat"});
    for (pipefail, label) in [(true, "enabled"), (false, "disabled")] {
        let plan =
            serde_json::to_vec(&serde_json::json!({"name":"bash","shell":{"pipefail":pipefail}}))
                .unwrap();
        assert!(
            approval_text("bash", &arguments, &plan).contains(&format!("bash pipefail: {label}"))
        );
    }
}
