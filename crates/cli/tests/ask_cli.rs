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
    assert!(!include_str!("../src/base-instruction.txt").contains(sentinel));
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
    assert!(help.contains("--compaction-strategy"));
    assert!(
        help.contains("default: observation_masking"),
        "the new-session default is not shown in --help: {help}"
    );
    assert!(help.contains("--switch-policy"));
    assert!(help.contains("anthropic"));
    assert!(help.contains("deepseek"));
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
        vec![
            "ask",
            "--workspace",
            ".",
            "--compaction-strategy",
            "masking",
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

/// `--provider deepseek` without a key is the same authentication failure as
/// `anthropic`: exit 3, no provider text, no Codex hint, nothing opened, and
/// `--auth` is not silently applied to it.
#[test]
fn deepseek_provider_requires_its_own_key_and_no_openai_auth_flag() {
    let sentinel = "SENTINEL-DEEPSEEK-ASK";
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("workspace")).unwrap();
    for (args, expected) in [
        (vec!["--provider", "deepseek"], 3),
        (vec!["--provider", "deepseek", "--auth", "codex"], 2),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(["ask", "--workspace"])
            .arg(temp.path().join("workspace"))
            .arg("--data-root")
            .arg(temp.path().join("state"))
            .args(args)
            .arg("hello")
            .env("HOME", temp.path())
            .env_remove("DEEPSEEK_API_KEY")
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

/// The `deepseek` instance is registered only when its credential exists, and a
/// missing credential is fatal only when `deepseek` is the selected provider.
///
/// The real adapters are constructed in a child process, because credential
/// discovery is process-wide: one test may not change another's environment.
#[test]
fn deepseek_is_registered_with_its_key_and_skipped_without_it() {
    const CASE: &str = "HARNESS_DEEPSEEK_ENV_CASE";
    let deepseek = "SENTINEL-DEEPSEEK-WIRING";
    let anthropic = "SENTINEL-ANTHROPIC-WIRING";
    if let Ok(case) = std::env::var(CASE) {
        use harness::wiring;
        use harness_core::session::ModelRef;
        match case.as_str() {
            "with" => {
                let wired = wiring::providers("deepseek", false).expect("deepseek registered");
                assert!(wired.instances.iter().any(|name| name == "deepseek"));
                assert!(wired.instances.iter().any(|name| name == "anthropic"));
                let flash = ModelRef {
                    provider_instance: "deepseek".into(),
                    model: "deepseek-flash".into(),
                };
                let pro = ModelRef {
                    provider_instance: "deepseek".into(),
                    model: "deepseek-v4-pro".into(),
                };
                assert_eq!(
                    wired
                        .driver
                        .runtime
                        .context
                        .catalog
                        .limits(&flash)
                        .and_then(|limits| limits.context_tokens),
                    Some(1_000_000)
                );
                assert!(wired.driver.runtime.context.catalog.limits(&pro).is_some());
                assert_eq!(wiring::provider_instance("deepseek", true), "deepseek");
                assert_eq!(wiring::default_model("deepseek", false), "deepseek-flash");
            }
            // Skipped, not fatal: another provider is selected and this one has
            // no credential.
            "without" => {
                let wired = wiring::providers("anthropic", false).expect("anthropic registered");
                assert!(wired.instances.iter().any(|name| name == "anthropic"));
                // Skipped, never fatal: no `DEEPSEEK_API_KEY`, so no instance.
                assert!(!wired.instances.iter().any(|name| name == "deepseek"));
            }
            // Selected without a credential: an authentication failure, and no
            // Codex hint (the client only asks for one on the OpenAI path, which
            // the binary-level test above also asserts).
            "missing" => {
                let error = match wiring::providers("deepseek", false) {
                    Ok(_) => panic!("a missing credential was not fatal"),
                    Err(error) => error,
                };
                assert_eq!(error.code, 3);
                assert_eq!(error.diagnostic, "authentication");
                assert!(!error.codex_hint);
            }
            other => panic!("unknown case {other}"),
        }
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    for (case, env) in [
        (
            "with",
            vec![
                ("DEEPSEEK_API_KEY", deepseek),
                ("ANTHROPIC_API_KEY", anthropic),
            ],
        ),
        ("without", vec![("ANTHROPIC_API_KEY", anthropic)]),
        ("missing", Vec::new()),
    ] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "deepseek_is_registered_with_its_key_and_skipped_without_it",
                "--nocapture",
            ])
            .env(CASE, case)
            .env("HOME", temp.path())
            .env_remove("DEEPSEEK_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("XDG_DATA_HOME");
        for (name, value) in env {
            command.env(name, value);
        }
        let out = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{case}: {stdout}\n{stderr}");
        assert!(
            !stdout.contains(deepseek) && !stdout.contains(anthropic),
            "{case} leaked a key"
        );
        assert!(
            !stderr.contains(deepseek) && !stderr.contains(anthropic),
            "{case} leaked a key"
        );
        // Provider setup never touches storage.
        assert!(!temp.path().join("state").exists());
    }
}
