//! Opt-in live check for mid-session switching. Run it through
//! `scripts/live-check.sh switch`; it is never part of `check.sh` or CI.
//!
//! With `ANTHROPIC_API_KEY` set it runs the cross-provider session: one short
//! OpenAI turn, a switch to Anthropic, a short Anthropic turn with one tool
//! round trip, a switch back, and a final OpenAI turn. Without the key it runs
//! the same-provider variant instead — the Codex model, another model the
//! account serves (read from `~/.codex/models_cache.json`), and back — and
//! reports the cross-provider part as skipped.
//!
//! It spends real quota, so it is deliberately small. It never prints
//! credentials, account IDs, session IDs or response IDs: only pass/fail lines,
//! model names and stable turn codes.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use harness_core::{
    context::{ModelCapabilities, ModelCatalog, ModelInfo, ModelLimits},
    runtime::*,
    session::*,
    tools::SandboxConfig,
};
use harness_providers::{
    anthropic::{self, AnthropicMessages},
    openai::{self, OpenAiResponses, Profile},
    router::Router,
};
use serde_json::{Value, json};

const OPENAI_INSTANCE: &str = "openai-codex";
const PLATFORM_INSTANCE: &str = "openai-api-key";
const ANTHROPIC_INSTANCE: &str = "anthropic";
const TURN_TIMEOUT: Duration = Duration::from_secs(180);
const NOTES: &str = "live-switch fixture notes";

fn main() -> std::process::ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            println!("live switch: fail (runtime)");
            return std::process::ExitCode::from(1);
        }
    };
    std::process::ExitCode::from(runtime.block_on(run()))
}

fn scratch() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "harness-live-switch-{}-{nanos}",
        std::process::id()
    ))
}

async fn run() -> u8 {
    let auth = std::env::args().nth(1).unwrap_or_else(|| "codex".into());
    let (instance, profile, adapter) = match auth.as_str() {
        "api-key" => (
            PLATFORM_INSTANCE,
            Profile::Platform,
            OpenAiResponses::platform_from_env(),
        ),
        _ => (
            OPENAI_INSTANCE,
            Profile::ChatGptCodex,
            OpenAiResponses::codex_login(),
        ),
    };
    let adapter = match adapter {
        Ok(adapter) => adapter,
        Err(_) => {
            println!("live switch: fail (no OpenAI credential; run `codex login`)");
            return 1;
        }
    };
    let anthropic = AnthropicMessages::from_env().ok();
    let root = scratch();
    if std::fs::create_dir_all(root.join("workspace")).is_err() {
        println!("live switch: fail (scratch directory)");
        return 1;
    }
    let _ = std::fs::write(root.join("workspace/notes.txt"), NOTES);
    let result = session(&root, instance, profile, adapter, anthropic).await;
    let _ = std::fs::remove_dir_all(&root);
    result
}

async fn session(
    root: &Path,
    instance: &'static str,
    profile: Profile,
    openai_adapter: OpenAiResponses,
    anthropic_adapter: Option<AnthropicMessages>,
) -> u8 {
    let workspace = root.join("workspace");
    let mut router = Router::new().with(instance, Arc::new(openai_adapter));
    let mut catalog = ModelCatalog::new();
    openai::extend_catalog(&mut catalog, profile, instance);
    // A second model the account serves, for the same-provider variant.
    let alternate = if profile == Profile::ChatGptCodex {
        alternate_model()
    } else {
        None
    };
    if let Some((model, context_tokens)) = &alternate {
        catalog.insert(
            ModelRef {
                provider_instance: instance.into(),
                model: model.clone(),
            },
            ModelInfo {
                limits: ModelLimits {
                    context_tokens: Some(*context_tokens),
                    max_output_tokens: None,
                },
                capabilities: ModelCapabilities {
                    tools: true,
                    reasoning_effort: true,
                    ..Default::default()
                },
            },
        );
    }
    if let Some(adapter) = anthropic_adapter {
        anthropic::extend_catalog(&mut catalog, ANTHROPIC_INSTANCE);
        router.register(ANTHROPIC_INSTANCE, Arc::new(adapter));
    }
    let cross_provider = router.contains(ANTHROPIC_INSTANCE);
    let root = match DataRoot::open(root.join("state"), true) {
        Ok(root) => Arc::new(root),
        Err(_) => {
            println!("live switch: fail (data root)");
            return 1;
        }
    };
    let policy = json!({
        "policy_revision": "m4-07-live",
        "enabled_tools": ["read"],
        "workspace_read": "allow",
        "workspace_mutation": "deny",
        "process": "deny",
        "network": "deny",
        "protected_paths": [],
        "allowances": [],
        "compaction": {"automatic": false},
    });
    let configuration = match root.put_artifact(&serde_json::to_vec(&policy).unwrap_or_default()) {
        Ok(artifact) => artifact,
        Err(_) => {
            println!("live switch: fail (configuration)");
            return 1;
        }
    };
    let header = SessionHeader {
        workspace: workspace.to_str().unwrap_or_default().into(),
        initial_model: ModelRef {
            provider_instance: instance.into(),
            model: openai::LIVE_CHECK_MODEL.into(),
        },
        policy_revision: "m4-07-live".into(),
        configuration,
    };
    let mut driver = DriverConfig::default();
    driver.runtime.context.catalog = catalog;
    driver.runtime.inference_deadline = Some(TURN_TIMEOUT);
    let host = ToolHostConfig {
        workspace_root: workspace.clone(),
        data_root: root.path().to_owned(),
        sandbox: None::<SandboxConfig>,
    };
    let handle = match SessionHandle::create_with_tools(
        root.clone(),
        SessionId::new(),
        header,
        Arc::new(router),
        driver,
        &host,
    )
    .await
    {
        Ok(handle) => handle,
        Err(_) => {
            println!("live switch: fail (session)");
            return 1;
        }
    };
    let mut failed = false;
    // Turn 1: the starting model.
    failed |= !turn(&handle, "Reply with the single word: pong", "openai turn 1").await;
    if cross_provider {
        let target = ModelRef {
            provider_instance: ANTHROPIC_INSTANCE.into(),
            model: anthropic::LIVE_CHECK_MODEL.into(),
        };
        failed |= !switch(&handle, target.clone(), "openai -> anthropic").await;
        failed |= !turn(
            &handle,
            "Reply with the single word: pong",
            "anthropic turn 1",
        )
        .await;
        // One tool round trip on the other provider, with the switch behind it.
        failed |= !turn(
            &handle,
            "Read the file notes.txt with the read tool, then answer with its contents only.",
            "anthropic tool round trip",
        )
        .await;
        failed |= !tools_used(&handle, "anthropic tool round trip").await;
        failed |= !switch(
            &handle,
            ModelRef {
                provider_instance: instance.into(),
                model: openai::LIVE_CHECK_MODEL.into(),
            },
            "anthropic -> openai",
        )
        .await;
        failed |= !turn(&handle, "Reply with the single word: pong", "openai turn 2").await;
        println!("cross-provider: {}", if failed { "fail" } else { "pass" });
    } else {
        println!("cross-provider: skipped (no ANTHROPIC_API_KEY)");
        match alternate {
            None => println!("same-provider: skipped (no alternate served model)"),
            Some((model, _)) => {
                let target = ModelRef {
                    provider_instance: instance.into(),
                    model: model.clone(),
                };
                let switched = switch(&handle, target, &format!("same provider -> {model}")).await;
                let there = turn(
                    &handle,
                    "Reply with the single word: pong",
                    "alternate model turn",
                )
                .await;
                let back = switch(
                    &handle,
                    ModelRef {
                        provider_instance: instance.into(),
                        model: openai::LIVE_CHECK_MODEL.into(),
                    },
                    "same provider back",
                )
                .await;
                let again = turn(
                    &handle,
                    "Reply with the single word: pong",
                    "original model turn",
                )
                .await;
                // One tool round trip here instead.
                let tool = turn(
                    &handle,
                    "Read the file notes.txt with the read tool, then answer with its contents only.",
                    "openai tool round trip",
                )
                .await;
                let used = tools_used(&handle, "openai tool round trip").await;
                let same_provider = switched && there && back && again && tool && used;
                println!(
                    "same-provider: {}",
                    if same_provider { "pass" } else { "fail" }
                );
                failed |= !same_provider;
            }
        }
    }
    handle.shutdown().await;
    if failed {
        println!("summary: fail");
        1
    } else {
        println!("summary: pass");
        0
    }
}

/// A second model from the account's fetched Codex cache: visible, usable in the
/// API, and not the live-check default. The cache carries no secrets.
fn alternate_model() -> Option<(String, u32)> {
    let path = std::env::var_os("CODEX_MODELS_CACHE")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".codex/models_cache.json"))
        })?;
    let bytes = std::fs::read(path).ok()?;
    let cache: Value = serde_json::from_slice(&bytes).ok()?;
    cache.get("models")?.as_array()?.iter().find_map(|model| {
        let slug = model.get("slug")?.as_str()?;
        let visible = model.get("visibility")?.as_str()? == "list";
        let api = model.get("supported_in_api")?.as_bool()?;
        let tokens = model.get("context_window")?.as_u64()?;
        (visible && api && slug != openai::LIVE_CHECK_MODEL)
            .then(|| (slug.to_owned(), u32::try_from(tokens).unwrap_or(u32::MAX)))
    })
}

async fn turn(handle: &SessionHandle, prompt: &str, label: &str) -> bool {
    let accepted = handle
        .command(Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![ContentBlock::Text {
                text: prompt.into(),
            }],
        })
        .await;
    let Ok(Accepted::TurnStarted { turn_id, .. }) = accepted else {
        println!("{label}: fail (start)");
        return false;
    };
    let deadline = tokio::time::Instant::now() + TURN_TIMEOUT;
    loop {
        if tokio::time::Instant::now() > deadline {
            println!("{label}: fail (timeout)");
            return false;
        }
        let Ok(view) = handle.view().await else {
            println!("{label}: fail (view)");
            return false;
        };
        if let Some(finished) = view
            .snapshot
            .events()
            .find_map(|(_, _, event)| match event {
                JournalEvent::TurnFinished(finish) if finish.turn_id == turn_id => Some(finish),
                _ => None,
            })
        {
            let code = finished.code.as_deref().unwrap_or("completed");
            let ok = finished.outcome == TurnOutcome::Completed;
            println!("{label}: {} ({code})", if ok { "pass" } else { "fail" });
            return ok;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn switch(handle: &SessionHandle, model: ModelRef, label: &str) -> bool {
    match handle
        .command(Command::SwitchModel {
            model,
            // Reasoning state from the other provider is optional: this check
            // proves the switch itself, not which policy the user would pick.
            policy: Portability::OmitOptional,
        })
        .await
    {
        Ok(Accepted::ModelSwitched { preview, .. }) => {
            println!(
                "{label}: pass (omitted {} entries)",
                preview.would_stop_being_portable.len()
            );
            true
        }
        Ok(other) => {
            println!("{label}: fail ({other:?})");
            false
        }
        Err(rejection) => {
            println!("{label}: fail ({rejection:?})");
            false
        }
    }
}

async fn tools_used(handle: &SessionHandle, label: &str) -> bool {
    let Ok(view) = handle.view().await else {
        return false;
    };
    // Tool results are journal entries, not audit events.
    let used = view
        .snapshot
        .entries()
        .any(|(_, stored)| match &stored.entry.payload {
            EntryPayload::ToolResult(result) => {
                matches!(result.result.status, ToolStatus::Succeeded)
            }
            _ => false,
        });
    println!(
        "{label}: tool result {}",
        if used { "pass" } else { "fail" }
    );
    used
}
