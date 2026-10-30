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
    // own credential read fails. The API-key case uses a forbidden header byte,
    // so real adapter construction fails locally, never by contacting OpenAI.
    // A valid-looking sentinel would make this an accidental live test.
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
            .env("OPENAI_API_KEY", format!("{sentinel}\n"))
            .env_remove("ANTHROPIC_API_KEY")
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
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--summary-model"));
    assert!(help.contains("--switch-policy"));
    assert!(help.contains("anthropic"));
    for args in [
        vec!["ask", "hello"],
        vec!["ask", "--workspace", ".", "--provider", "google", "hello"],
        vec!["ask", "--workspace", ".", "--auth", "invalid", "hello"],
        vec![
            "ask",
            "--workspace",
            ".",
            "--switch-policy",
            "convert-supported",
            "hello",
        ],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty());
    }
}

/// `--provider anthropic` without a key is an authentication failure with no
/// provider text, and `--auth` does not silently apply to it.
#[test]
fn anthropic_provider_requires_its_own_key_and_no_openai_auth_flag() {
    let sentinel = "SENTINEL-ANTHROPIC-ASK";
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("workspace")).unwrap();
    for (args, expected) in [
        (vec!["--provider", "anthropic"], 3),
        (vec!["--provider", "anthropic", "--auth", "codex"], 2),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(["ask", "--workspace"])
            .arg(temp.path().join("workspace"))
            .arg("--data-root")
            .arg(temp.path().join("state"))
            .args(args)
            .arg("hello")
            .env("HOME", temp.path())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(expected), "{expected}");
        assert!(out.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains(sentinel), "{stderr}");
        if expected == 3 {
            assert!(stderr.contains("error: authentication"), "{stderr}");
            assert!(!stderr.contains("codex login"), "{stderr}");
        } else {
            assert!(stderr.contains("auth_not_applicable"), "{stderr}");
        }
    }
    assert!(!temp.path().join("state").exists(), "nothing was opened");
}
