//! Real CLI commands under isolated user directories; no API keys or network.
use std::{
    fs,
    process::{Command, Output},
};
fn run(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .output()
        .unwrap()
}
#[test]
fn config_path_show_missing_default_explicit_no_config_and_global_positions() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".config/harness/config.toml");
    let missing = run(temp.path(), &["config", "path"]);
    assert!(missing.status.success());
    assert_eq!(
        String::from_utf8_lossy(&missing.stdout),
        format!("{} (exists: false)\n", path.display())
    );
    assert!(run(temp.path(), &["config", "show"]).status.success());
    assert_eq!(
        run(
            temp.path(),
            &["--config", path.to_str().unwrap(), "config", "show"]
        )
        .status
        .code(),
        Some(2)
    );
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "provider = 'deepseek'\nmodel = 'configured'\napprove_read_only = true\nswitch_policy = 'omit-optional'\n").unwrap();
    let output = run(temp.path(), &["config", "show", "--model", "override"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("provider = \"deepseek\" [config]"));
    assert!(text.contains("model = \"override\" [flag]"));
    assert!(text.contains("approve_read_only = \"true\" [config] (ask only)"));
    assert!(text.contains("switch_policy = \"omit-optional\" [config] (ask only)"));
    assert!(text.contains("auth = \"codex\" [default]"));
    let output = run(temp.path(), &["config", "path"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("exists: true"));
    fs::write(&path, "unknown = 1").unwrap();
    assert_eq!(run(temp.path(), &["config", "path"]).status.code(), Some(2));
    for args in [
        vec!["--no-config", "config", "show"],
        vec!["config", "show", "--no-config"],
    ] {
        let output = run(temp.path(), &args);
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("provider = \"openai\" [default]")
        );
    }
    // A default malformed file reaches the same resolver before either client.
    for args in [vec![], vec!["ask", "--workspace", ".", "hello"]] {
        let output = run(temp.path(), &args);
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains(": unknown: unknown key"));
    }
}
#[test]
fn xdg_config_override_and_client_auth_behavior_use_effective_defaults() {
    let temp = tempfile::tempdir().unwrap();
    let xdg = temp.path().join("xdg");
    fs::create_dir_all(xdg.join("harness")).unwrap();
    let path = xdg.join("harness/config.toml");
    fs::write(&path, "provider = 'anthropic'\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["config", "path"])
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", &xdg)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(path.to_str().unwrap()));
    let out = run(
        temp.path(),
        &[
            "ask",
            "--workspace",
            ".",
            "--config",
            path.to_str().unwrap(),
            "hello",
        ],
    );
    assert_eq!(out.status.code(), Some(3));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("codex login"));
    let out = run(
        temp.path(),
        &[
            "ask",
            "--workspace",
            ".",
            "--config",
            path.to_str().unwrap(),
            "--provider",
            "openai",
            "hello",
        ],
    );
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("codex login"));
    fs::write(&path, "provider = 'anthropic'\nauth = 'api-key'\n").unwrap();
    let out = run(
        temp.path(),
        &[
            "ask",
            "--workspace",
            ".",
            "--config",
            path.to_str().unwrap(),
            "hello",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("auth_not_applicable"));
}
#[test]
fn config_errors_exit_two_and_never_echo_credential_or_invalid_value() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(".config/harness/config.toml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    for (text, expected) in [
        (
            "token = 'PRIVATE-CREDENTIAL-VALUE'",
            "config_credential_refused",
        ),
        ("provider = 'PRIVATE-CREDENTIAL-VALUE'", "config_invalid"),
        ("model = 42", "config_invalid"),
        ("secret = [", "config_invalid"),
    ] {
        fs::write(&path, text).unwrap();
        let output = run(temp.path(), &["config", "show"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("PRIVATE-CREDENTIAL-VALUE"),
            "credential leaked"
        );
        assert!(stderr.contains(expected));
    }
    fs::write(&path, vec![b'x'; 65537]).unwrap();
    assert_eq!(run(temp.path(), &["config", "show"]).status.code(), Some(2));
    fs::write(&path, [0xff]).unwrap();
    assert_eq!(run(temp.path(), &["config", "show"]).status.code(), Some(2));
}
