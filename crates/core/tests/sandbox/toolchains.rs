//! M8 S02 cases run by the existing sandbox target and exact-head CI gate.
use super::*;
use harness_core::tools::sandbox::ToolchainSettings;

fn configured_plan(f: &Fixture, settings: &ToolchainSettings, command: &str) -> Plan {
    let bytes = serde_json::to_vec(&json!({
        "policy_revision":"toolchains-v1", "enabled_tools":["bash"],
        "workspace_read":"ask", "workspace_mutation":"ask", "process":"ask", "network":"deny",
        "protected_paths":["private", "secret"], "allowances":[], "sandbox":settings
    }))
    .unwrap();
    let policy = PolicySnapshot::parse(
        ArtifactRef {
            sha256: Digest::of(&bytes),
            bytes: bytes.len() as u64,
        },
        &bytes,
    )
    .unwrap();
    let registry = Registry::builtin();
    let tool = registry.resolve("bash", &policy).unwrap();
    let args = tool
        .validate(&json!({"command":command, "timeout_ms":120000}))
        .unwrap();
    tool.plan(
        &args,
        &CallRef {
            response_entry_id: "019a0000-0000-7000-8000-00000000019a".parse().unwrap(),
            call_id: "019a0000-0000-7000-8000-00000000019b".parse().unwrap(),
        },
        &View(WorkspaceIdentity(f.root().to_str().unwrap().into())),
        &policy,
    )
    .unwrap()
}
fn configured_backend(f: &Fixture, settings: &ToolchainSettings) -> Sandbox {
    Sandbox::probe(SandboxConfig {
        workspace_root: f.root().into(),
        toolchains: settings.clone(),
        ..Default::default()
    })
    .expect("S02 toolchain sandbox must probe successfully")
}
fn run(f: &Fixture, b: &Sandbox, settings: &ToolchainSettings, command: &str) -> ExecutionOutcome {
    b.run(
        &configured_plan(f, settings, command),
        f.root(),
        &CancelToken::new(),
    )
}

#[test]
#[ignore = "S02 requires Linux user namespaces and Bubblewrap"]
fn s02_toolchain_credentials_present_and_absent_readonly_and_private_caches() {
    let f = Fixture::new();
    let mount = tempfile::tempdir().unwrap();
    let settings = ToolchainSettings {
        toolchain_paths: vec![mount.path().into()],
        env: [("BUILD_LABEL".into(), "ENV_SENTINEL_NOT_FOR_REPORTS".into())].into(),
        ..Default::default()
    };
    let leaves = ["credentials", "credentials.toml", ".git-credentials"];
    for leaf in leaves {
        fs::write(mount.path().join(leaf), "CREDENTIAL_SENTINEL").unwrap();
    }
    let b = configured_backend(&f, &settings);
    let mut command = String::from("set -eu\n");
    for leaf in leaves {
        command.push_str(&format!(
            "if cat '{}/{leaf}'; then exit 81; fi\n",
            mount.path().display()
        ));
    }
    command.push_str(&format!(
        "if echo bad > '{}/new-file'; then exit 82; fi\n",
        mount.path().display()
    ));
    command.push_str("if cat /workspace/private/token; then exit 83; fi\nif echo bad > /workspace/.git/config; then exit 84; fi\nif (echo bad > /dev/tcp/1.1.1.1/443); then exit 85; fi\ntest \"$(stat -c %a \"$CARGO_HOME\")\" = 700\ntest \"$(stat -c %u \"$CARGO_HOME\")\" = \"$(id -u)\"\ntest \"$(stat -c %a \"$CARGO_TARGET_DIR\")\" = 700\nprintf private > \"$CARGO_HOME/lock\"\nprintf target > \"$CARGO_TARGET_DIR/lock\"\ntest \"$BUILD_LABEL\" = ENV_SENTINEL_NOT_FOR_REPORTS\n");
    let outcome = run(&f, &b, &settings, &command);
    succeeded(&outcome);
    assert!(!text(&outcome).contains("CREDENTIAL_SENTINEL"));
    assert!(!mount.path().join("new-file").exists());
    let report = settings.report().to_string();
    assert!(!report.contains("ENV_SENTINEL_NOT_FOR_REPORTS"));
    assert_eq!(
        settings.report()["toolchain_paths"][0]["credential_masks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["status"] == "masked")
            .count(),
        3
    );
    for leaf in leaves {
        fs::remove_file(mount.path().join(leaf)).unwrap();
    }
    let b = configured_backend(&f, &settings); // Absence must not fail the probe.
    let mut command = String::from("set -eu\ntest ! -e \"$CARGO_HOME/lock\"\n");
    for leaf in leaves {
        command.push_str(&format!(
            "if echo bad > '{}/{leaf}'; then exit 86; fi\ntest ! -e '{}/{leaf}'\n",
            mount.path().display(),
            mount.path().display()
        ));
    }
    succeeded(&run(&f, &b, &settings, &command));
    for leaf in leaves {
        assert!(!mount.path().join(leaf).exists());
    }
    assert_eq!(
        settings.report()["toolchain_paths"][0]["credential_masks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["status"] == "absent")
            .count(),
        3
    );
    let mut changed = settings.clone();
    changed.env.insert("BUILD_LABEL".into(), "changed".into());
    failed(
        &run(&f, &b, &changed, ":"),
        ToolStatus::InfrastructureFailure,
        ResultCode::SandboxUnavailable,
    );
}

#[test]
#[ignore = "S02 requires Linux user namespaces, Bubblewrap and an installed Rust toolchain"]
fn s02_toolchain_cargo_test_offline_with_readonly_registry() {
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let rustup = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".rustup"));
    let toolchain = fs::read_dir(rustup.join("toolchains"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("1.99.1-"))
                && p.join("bin/cargo").is_file()
                && p.join("bin/rustc").is_file()
        });
    let Some(toolchain) = toolchain else {
        eprintln!("SKIPPED: toolchain recipe (Rust 1.99.1 not installed)");
        return;
    };
    let cargo = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cargo"));
    let registry = fs::canonicalize(cargo.join("registry"))
        .expect("dependencies must be fetched for the offline registry recipe");
    let itoa = fs::read_dir(registry.join("src"))
        .unwrap()
        .flatten()
        .flat_map(|e| fs::read_dir(e.path()).unwrap().flatten())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("itoa-1."))
        .expect("workspace dependencies fetch itoa before S02");
    let version = itoa.strip_prefix("itoa-").unwrap();
    let f = Fixture::new();
    fs::create_dir(f.root().join("src")).unwrap();
    fs::write(f.root().join("Cargo.toml"), format!("[package]\nname = 'sandbox-recipe'\nversion = '0.0.0'\nedition = '2024'\n[workspace]\n[dependencies]\nitoa = '={version}'\n")).unwrap();
    fs::write(f.root().join("src/lib.rs"), "#[test]\nfn offline_registry_test_ran() { assert_eq!(itoa::Buffer::new().format(42), \"42\"); println!(\"SANDBOX_RUST_TEST_BINARY_RAN\"); }\n").unwrap();
    let settings = ToolchainSettings {
        toolchain_paths: vec![fs::canonicalize(&toolchain).unwrap(), registry.clone()],
        path_prepend: vec![fs::canonicalize(toolchain.join("bin")).unwrap()],
        ..Default::default()
    };
    let b = configured_backend(&f, &settings);
    let command = format!(
        "set -eu\nln -s '{}' \"$CARGO_HOME/registry\"\ncargo test --offline -- --nocapture\ntest -f \"$CARGO_HOME/.package-cache\"\nif (echo bad > /dev/tcp/1.1.1.1/443); then exit 87; fi\n",
        registry.display()
    );
    let outcome = run(&f, &b, &settings, &command);
    succeeded(&outcome);
    let output = text(&outcome);
    assert!(output.contains("SANDBOX_RUST_TEST_BINARY_RAN"), "{output}");
    assert!(output.contains("1 passed"), "{output}");
    assert!(!f.root().join("target").exists());
    assert!(f.root().join("Cargo.lock").exists());
}
