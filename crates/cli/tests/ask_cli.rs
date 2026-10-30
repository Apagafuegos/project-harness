//! Process-spawning checks stay separate from concurrent journal reopen tests
//! (ADR 0012's fork/exec lock inheritance gap).
use std::{fs, path::Path, process::Command};

fn scan_no_secret(path: &Path, secret: &[u8]) {
    for item in fs::read_dir(path).unwrap() {
        let path = item.unwrap().path();
        if path.is_dir() {
            scan_no_secret(&path, secret);
        } else {
            assert!(
                !fs::read(&path)
                    .unwrap()
                    .windows(secret.len())
                    .any(|w| w == secret),
                "secret in {}",
                path.display()
            );
        }
    }
}
#[test]
fn sentinel_credential_never_printed_on_real_auth_failure() {
    let sentinel = "SENTINEL-CREDENTIAL-ASK";
    // The subscription login is absent in this temporary HOME, so the adapter's
    // own credential read fails. Nothing may echo the API key in the environment.
    for (auth, expected) in [
        ("codex", "run `codex login`"),
        ("api-key", "authentication"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("workspace")).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(["ask", "--workspace"])
            .arg(temp.path().join("workspace"))
            .arg("--data-root")
            .arg(temp.path().join("state"))
            .args(["--auth", auth, "hello"])
            .env("HOME", temp.path())
            .env("OPENAI_API_KEY", sentinel)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(3), "{auth}: {stderr}");
        assert!(out.stdout.is_empty());
        assert!(!stderr.contains(sentinel), "{auth} leaked the credential");
        assert!(stderr.contains(expected), "{auth}: {stderr}");
        scan_no_secret(temp.path(), sentinel.as_bytes());
    }
}
#[test]
fn ask_help_and_usage_errors() {
    let out = Command::new(env!("CARGO_BIN_EXE_harness"))
        .args(["ask", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("--summary-model"));
    for args in [
        vec!["ask", "hello"],
        vec![
            "ask",
            "--workspace",
            ".",
            "--provider",
            "anthropic",
            "hello",
        ],
        vec!["ask", "--workspace", ".", "--auth", "invalid", "hello"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty());
    }
}
