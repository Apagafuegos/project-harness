use super::*;
use harness_core::tools::sandbox::ToolchainSettings;

#[test]
fn sandbox_table_strict_types_keys_redaction_and_sources() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    for text in [
        "[sandbox]\nunknown=1",
        "[sandbox]\nenv=[]",
        "[sandbox]\ntoolchain_paths='wrong'",
        "[sandbox]\npath_prepend=['/outside']",
    ] {
        fs::write(&path, text).unwrap();
        let error = load(&path, true).unwrap_err();
        assert!(error.contains("sandbox"), "{error}");
    }
    for key in [
        "api_key",
        "CUSTOM_KEY",
        "token",
        "PASSWORD",
        "PATH",
        "CARGO_HOME",
        "LD_PRELOAD",
        "OPENAI_API_KEY",
    ] {
        fs::write(
            &path,
            format!("[sandbox.env]\n{key}='DO_NOT_PRINT_ENV_SENTINEL'\n"),
        )
        .unwrap();
        let error = load(&path, true).unwrap_err();
        assert!(error.contains(key), "missing key diagnostic");
        assert!(!error.contains("DO_NOT_PRINT_ENV_SENTINEL"));
    }
    fs::write(&path, "[sandbox]\ntoolchain_paths=['/tools']\npath_prepend=['/tools/bin']\n[sandbox.env]\nBUILD_LABEL='DO_NOT_PRINT_ENV_SENTINEL'\n").unwrap();
    let settings = resolve_values(&matches(false, &[]), load(&path, true).unwrap()).unwrap();
    let show = settings.show();
    assert!(show.contains("sandbox.toolchain_paths = [\"/tools\"] [config]"));
    assert!(show.contains("sandbox.path_prepend = [\"/tools/bin\"] [config]"));
    assert!(show.contains("BUILD_LABEL"));
    assert!(!show.contains("DO_NOT_PRINT_ENV_SENTINEL"));
    let settings = resolve_values(&matches(false, &[]), BTreeMap::new()).unwrap();
    assert!(
        settings
            .show()
            .contains("sandbox.toolchain_paths = [] [default]")
    );
    assert!(settings.sandbox_config().toolchains.is_empty());
    fs::write(&path, "[sandbox]\ntoolchain_paths=['/']\n").unwrap();
    assert!(
        resolve(&matches(false, &["--config", path.to_str().unwrap()]))
            .unwrap_err()
            .contains("refused toolchain path: /")
    );
    resolve(&matches(false, &["--no-config"])).unwrap();
}

#[tokio::test]
async fn sandbox_both_clients_record_settings_and_refuse_continuation_changes() {
    for tui in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let mount = temp.path().join("tools");
        fs::create_dir(&mount).unwrap();
        let sandbox = ToolchainSettings {
            toolchain_paths: vec![mount.clone()],
            path_prepend: vec![mount.clone()],
            env: [("BUILD_LABEL".into(), "recorded".into())].into(),
        };
        let args = matches(tui, &[]);
        let mut values = configured(&[("auth", "api-key"), ("data_root", state.to_str().unwrap())]);
        values.insert("sandbox".into(), serde_json::to_string(&sandbox).unwrap());
        let settings = resolve_values(&args, values).unwrap();
        let mut options = if tui {
            crate::tui_cli::options(
                &args,
                &settings,
                workspace.clone(),
                vec![],
                driver(&settings),
            )
        } else {
            let ask = crate::ask_cli::options(&args, &settings, vec![], driver(&settings));
            assert_eq!(ask.sandbox.as_ref().unwrap().toolchains, sandbox);
            harness::wiring::SessionOptions {
                data_root: ask.data_root,
                workspace: workspace.clone(),
                session: None,
                model: ask.model,
                explicit_model: ask.explicit_model,
                explicit_provider: ask.explicit_provider,
                switch_policy: ask.switch_policy,
                known_instances: ask.known_instances,
                summary_model: ask.summary_model,
                compaction_strategy: ask.compaction_strategy,
                explicit_strategy: ask.explicit_strategy,
                driver: ask.driver,
                sandbox: ask.sandbox,
            }
        };
        assert_eq!(options.sandbox.as_ref().unwrap().toolchains, sandbox);
        options.sandbox.as_mut().unwrap().bwrap = "/nonexistent-m8-bwrap".into();
        let opened = harness::wiring::open_session(&options, provider())
            .await
            .unwrap();
        let id = opened.session.clone();
        opened.handle.shutdown().await;
        let root = DataRoot::open(&state, false).unwrap();
        let snapshot = JournalStore::inspect(&root, &id).unwrap().snapshot.unwrap();
        let bytes = root
            .read_artifact(snapshot.configuration(), 1024 * 1024)
            .unwrap();
        let recorded: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(recorded["sandbox"], serde_json::to_value(&sandbox).unwrap());
        options.session = Some(id.clone());
        let original = options.sandbox.clone();
        let journal = state.join("sessions").join(format!("{id}.jsonl"));
        let before = fs::read(&journal).unwrap();
        for change in ["mount", "prepend", "env", "removed"] {
            options.sandbox = original.clone();
            let settings = &mut options.sandbox.as_mut().unwrap().toolchains;
            match change {
                "mount" => settings.toolchain_paths.push(temp.path().join("different")),
                "prepend" => settings.path_prepend.clear(),
                "env" => {
                    settings.env.insert("BUILD_LABEL".into(), "changed".into());
                }
                _ => *settings = ToolchainSettings::default(),
            }
            let error = harness::wiring::open_session(&options, provider())
                .await
                .err()
                .unwrap();
            assert_eq!(error, (2, "session_policy_mismatch".into()), "{change}");
            assert_eq!(fs::read(&journal).unwrap(), before);
        }
        options.sandbox = original;
        harness::wiring::open_session(&options, provider())
            .await
            .unwrap()
            .handle
            .shutdown()
            .await;
    }
}
