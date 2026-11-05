//! Offline resolution and production option-assembly tests for both clients.
#[path = "config_sandbox_tests.rs"]
mod sandbox;
use super::*;
use clap::Command;
use harness_core::{
    context::{ModelLimits, StrategyId},
    runtime::DriverConfig,
    session::{DataRoot, FinishReason, JournalStore, ModelRef, Portability},
};
use std::{collections::BTreeMap, fs, sync::Arc};

fn matches(tui: bool, flags: &[&str]) -> clap::ArgMatches {
    let command = if tui {
        Command::new("harness").args(crate::tui_cli::args())
    } else {
        crate::ask_cli::command()
    };
    command
        .args(globals())
        .try_get_matches_from(
            std::iter::once("harness")
                .chain(if tui {
                    vec![]
                } else {
                    vec!["--workspace", "/unused-workspace", "hello"]
                })
                .chain(flags.iter().copied()),
        )
        .unwrap()
}
fn configured(values: &[(&str, &str)]) -> BTreeMap<String, String> {
    values
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn precedence_every_applicable_key_both_clients_and_ask_only_scope() {
    for tui in [false, true] {
        for (key, flag, value, override_value, default) in [
            (
                "provider",
                "--provider",
                "anthropic",
                "openai",
                Some("openai"),
            ),
            ("auth", "--auth", "api-key", "codex", Some("codex")),
            (
                "model",
                "--model",
                "configured-model",
                "flag-model",
                Some("gpt-6.1-sol"),
            ),
            (
                "summary_model",
                "--summary-model",
                "configured-summary",
                "flag-summary",
                None,
            ),
            (
                "compaction_strategy",
                "--compaction-strategy",
                "rolling_summary",
                "structured_state",
                Some("observation_masking"),
            ),
            (
                "data_root",
                "--data-root",
                "/configured-state",
                "/flag-state",
                None,
            ),
            (
                "switch_policy",
                "--switch-policy",
                "omit-optional",
                "reject",
                Some("reject"),
            ),
            (
                "approve_read_only",
                "--approve-read-only",
                "false",
                "true",
                Some("false"),
            ),
        ] {
            let base = resolve_values(&matches(tui, &[]), BTreeMap::new()).unwrap();
            assert_eq!(base.sources[key], "default");
            if key != "data_root" {
                assert_eq!(base.text(key), default);
            }
            let config = configured(&[(key, value)]);
            let settings = resolve_values(&matches(tui, &[]), config.clone()).unwrap();
            assert_eq!(
                settings.text(key),
                Some(value),
                "clap default masked config: {tui}/{key}"
            );
            assert!(settings.given(key));
            assert_eq!(settings.sources[key], "config");
            if tui && key == "switch_policy" {
                let options = crate::tui_cli::options(
                    &matches(tui, &[]),
                    &settings,
                    "/unused-workspace".into(),
                    vec![],
                    DriverConfig::default(),
                );
                assert_eq!(options.switch_policy, Portability::Reject);
                continue;
            }
            let flags = if key == "approve_read_only" {
                vec![flag]
            } else {
                vec![flag, override_value]
            };
            let args = matches(tui, &flags);
            let settings = resolve_values(&args, config).unwrap();
            assert_eq!(settings.text(key), Some(override_value));
            assert_eq!(settings.sources[key], "flag");
            // Exercise the exact assembly functions called by each real client.
            let (
                model,
                summary,
                strategy,
                root,
                explicit_model,
                explicit_provider,
                explicit_strategy,
            ) = if tui {
                let o = crate::tui_cli::options(
                    &args,
                    &settings,
                    "/unused-workspace".into(),
                    vec![],
                    DriverConfig::default(),
                );
                (
                    o.model,
                    o.summary_model,
                    o.compaction_strategy,
                    o.data_root,
                    o.explicit_model,
                    o.explicit_provider,
                    o.explicit_strategy,
                )
            } else {
                let o = crate::ask_cli::options(&args, &settings, vec![], DriverConfig::default());
                if key == "approve_read_only" {
                    assert!(o.approve_read_only);
                }
                if key == "switch_policy" {
                    assert_eq!(o.switch_policy, Portability::Reject);
                }
                (
                    o.model,
                    o.summary_model,
                    o.compaction_strategy,
                    o.data_root,
                    o.explicit_model,
                    o.explicit_provider,
                    o.explicit_strategy,
                )
            };
            if key == "model" {
                assert_eq!(model.model, override_value);
                assert!(explicit_model);
            }
            if key == "provider" {
                assert_eq!(model.provider_instance, "openai-codex");
                assert!(explicit_provider);
            }
            if key == "auth" {
                assert_eq!(model.provider_instance, "openai-codex");
            }
            if key == "summary_model" {
                assert_eq!(summary.unwrap().model, override_value);
            }
            if key == "compaction_strategy" {
                assert_eq!(strategy, StrategyId::StructuredState);
                assert!(explicit_strategy);
            }
            if key == "data_root" {
                assert_eq!(root.to_str(), Some(override_value));
            }
            if key == "approve_read_only" && tui {
                // The TUI takes the same resolved value into its pure model.
                let mut app = harness::tui::App::new(harness::tui::SessionInfo {
                    workspace: "/unused-workspace".into(),
                    model: ModelRef {
                        provider_instance: "openai-codex".into(),
                        model: "gpt-6.1-sol".into(),
                    },
                    context_limit: None,
                });
                app.set_read_only_approval(settings.approve_read_only());
                assert!(app.read_only_approval());
            }
        }
    }
}

#[test]
fn typed_config_rejects_unknown_types_values_utf8_size_and_missing_explicit() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    assert!(load(&path, false).unwrap().is_empty());
    assert!(load(&path, true).unwrap_err().contains("config_invalid:"));
    for (text, key) in [
        ("unknown = 1", "unknown"),
        ("model = 2", "model"),
        ("approve_read_only = 'true'", "approve_read_only"),
        ("provider = 'invalid'", "provider"),
        ("auth = 'invalid'", "auth"),
        ("compaction_strategy = 'invalid'", "compaction_strategy"),
        ("switch_policy = 'invalid'", "switch_policy"),
        ("data_root = false", "data_root"),
        ("model = [", "file"),
    ] {
        fs::write(&path, text).unwrap();
        let error = load(&path, true).unwrap_err();
        assert!(error.starts_with("config_invalid:"));
        assert!(error.contains(&format!(": {key}:")));
    }
    fs::write(&path, [0xff]).unwrap();
    assert!(load(&path, true).unwrap_err().contains("UTF-8"));
    fs::write(&path, "#".repeat(65537)).unwrap();
    assert!(load(&path, true).unwrap_err().contains("64 KiB"));
    fs::write(&path, "#".repeat(65536)).unwrap();
    assert!(load(&path, true).unwrap().is_empty());
    fs::write(&path, "approve_read_only = true\nmodel = 'valid-model'\n").unwrap();
    assert_eq!(load(&path, true).unwrap()["approve_read_only"], "true");
}

#[test]
fn credential_refusal_never_echoes_value_or_source() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    for key in [
        "api_key",
        "openai_key",
        "token",
        "secret",
        "password",
        "API_KEY",
        "nested.token",
    ] {
        fs::write(&path, format!("{key} = 'PRIVATE-CREDENTIAL-VALUE'\n")).unwrap();
        let error = load(&path, true).unwrap_err();
        // No assertion prints a potentially leaked value, even on mutation.
        assert!(
            !error.contains("PRIVATE-CREDENTIAL-VALUE"),
            "credential leaked"
        );
        assert_eq!(
            error,
            "config_credential_refused: put credentials in environment variables"
        );
    }
}

#[test]
fn no_config_ignores_even_explicit_bad_file_and_show_has_sources() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, "unknown = 1").unwrap();
    let args = matches(false, &["--no-config"]);
    let settings = resolve(&args).unwrap();
    assert!(!settings.given("provider"));
    fs::write(&path, "provider = 'deepseek'\n").unwrap();
    let args = matches(
        false,
        &["--config", path.to_str().unwrap(), "--model", "flag-model"],
    );
    let show = resolve(&args).unwrap().show();
    assert!(show.contains("provider = \"deepseek\" [config]"));
    assert!(show.contains("model = \"flag-model\" [flag]"));
    assert!(show.contains("auth = \"codex\" [default]"));
    assert!(show.contains("switch_policy = \"reject\" [default] (ask only)"));
    assert!(show.contains("approve_read_only = \"false\" [default] (ask and TUI)"));
}

fn provider() -> Arc<harness_core::model::scripted::ScriptedProvider> {
    use harness_core::model::scripted::{Script, ScriptedProvider, Step};
    Arc::new(ScriptedProvider::new([Script::new([
        Step::Event(harness_core::model::ProviderEvent::Started),
        Step::text(0, "done"),
        Step::completed(FinishReason::Stop),
    ])]))
}
fn driver(settings: &Settings) -> DriverConfig {
    let mut driver = DriverConfig::default();
    for model in [settings.text("model"), settings.text("summary_model")]
        .into_iter()
        .flatten()
    {
        driver.runtime.context.catalog.insert(
            ModelRef {
                provider_instance: harness::wiring::provider_instance(
                    settings.text("provider").unwrap(),
                    settings.text("auth") == Some("codex"),
                )
                .into(),
                model: model.into(),
            },
            ModelLimits {
                context_tokens: Some(128000),
                max_output_tokens: None,
            },
        );
    }
    driver
}

#[tokio::test]
async fn resolved_config_reaches_both_clients_recorded_fields_and_continuation_checks() {
    for tui in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let args = matches(tui, &[]);
        let settings = resolve_values(
            &args,
            configured(&[
                ("auth", "api-key"),
                ("model", "configured-model"),
                ("summary_model", "configured-summary"),
                ("compaction_strategy", "rolling_summary"),
                ("data_root", state.to_str().unwrap()),
                ("approve_read_only", "true"),
                ("switch_policy", "omit-optional"),
            ]),
        )
        .unwrap();
        let session = if tui {
            let mut options = crate::tui_cli::options(
                &args,
                &settings,
                workspace.clone(),
                vec![],
                driver(&settings),
            );
            options.sandbox = None;
            assert_eq!(options.switch_policy, Portability::Reject);
            let opened = harness::wiring::open_session(&options, provider())
                .await
                .unwrap();
            let session = opened.session.clone();
            opened.handle.shutdown().await;
            options.session = Some(session.clone());
            options.compaction_strategy = StrategyId::StructuredState;
            assert_eq!(
                harness::wiring::open_session(&options, provider())
                    .await
                    .err()
                    .unwrap()
                    .1,
                "session_policy_mismatch"
            );
            options.compaction_strategy = StrategyId::RollingSummary;
            options.summary_model = None;
            assert_eq!(
                harness::wiring::open_session(&options, provider())
                    .await
                    .err()
                    .unwrap()
                    .1,
                "session_policy_mismatch"
            );
            session
        } else {
            let mut options = crate::ask_cli::options(&args, &settings, vec![], driver(&settings));
            options.workspace = workspace.clone();
            options.sandbox = None;
            assert!(options.approve_read_only);
            assert_eq!(options.switch_policy, Portability::OmitOptional);
            let (_, rx) = tokio::sync::mpsc::unbounded_channel();
            let mut err = Vec::new();
            let code = harness::ask::run(
                options,
                provider(),
                harness::ask::Io {
                    stdin: &mut std::io::empty(),
                    stdout: &mut Vec::new(),
                    stderr: &mut err,
                    stderr_is_tty: false,
                    terminal: None,
                    interrupts: rx,
                    stdout_open_line: false,
                },
            )
            .await;
            assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
            DataRoot::open(&state, false).unwrap().sessions().unwrap()[0]
                .session_id
                .clone()
        };
        let root = DataRoot::open(&state, false).unwrap();
        let snapshot = JournalStore::inspect(&root, &session)
            .unwrap()
            .snapshot
            .unwrap();
        assert_eq!(
            snapshot.model_at(snapshot.head()).unwrap().model,
            "configured-model"
        );
        assert_eq!(
            snapshot
                .model_at(snapshot.head())
                .unwrap()
                .provider_instance,
            harness::wiring::API_KEY
        );
        let configuration: serde_json::Value = serde_json::from_slice(
            &root
                .read_artifact(snapshot.configuration(), 1024 * 1024)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(configuration["compaction"]["strategy"], "rolling_summary");
        assert_eq!(
            configuration["compaction"]["summary_model"]["model"],
            "configured-summary"
        );
        assert!(configuration.get("approve_read_only").is_none());
        assert!(configuration.get("switch_policy").is_none());
    }
}
