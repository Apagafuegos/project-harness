//! Headless approval scope; sandbox environment values stay private.
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

#[test]
fn batch_approval_uses_bound_scope_beyond_the_truncated_argument_line() {
    let arguments = serde_json::json!({"edits":[{"path":"first.rs","old_text":"x".repeat(600),"new_text":"y"}]});
    let plan = serde_json::to_vec(
        &serde_json::json!({"name":"multi_edit","arguments":{"files":[
            {"path":"first.rs","edits":[{"old_text":"old","new_text":"new"}]},
            {"path":"last.rs","edits":[{"old_text":"a\n\u{1b}\u{9b}","new_text":"b"}]}
        ]}}),
    )
    .unwrap();
    let text = approval_text("multi_edit", &arguments, &plan);
    assert!(text.contains("file (mutation): \"last.rs\""), "{text}");
    assert!(text.contains("replacement 1:"), "{text}");
    assert!(!text.contains('\u{1b}'));
    assert!(!text.contains('\u{9b}'));
    let read = serde_json::to_vec(&serde_json::json!({"name":"read","arguments":{"paths":[
        {"path":"a.rs","range":{"mode":"lines","start_line":2,"line_count":3,"max_bytes":100}},
        {"path":"z.rs","range":{"mode":"bytes","offset_bytes":7,"max_bytes":9}}
    ]}}))
    .unwrap();
    let text = approval_text("read", &serde_json::json!({}), &read);
    for expected in ["a.rs", "start_line", "z.rs", "offset_bytes"] {
        assert!(text.contains(expected), "{text}");
    }
}
