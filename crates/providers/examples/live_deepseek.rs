//! Opt-in live check for the DeepSeek profile. Run it through
//! `scripts/live-check.sh deepseek`; it is never part of `check.sh` or CI and it
//! skips (exit 77 from the script) when `DEEPSEEK_API_KEY` is unset.
//!
//! It spends the user's quota, so it is deliberately small: one streamed turn
//! with a tool round trip, and one follow-up turn whose projection replays the
//! thinking block that turn stored. It never prints credentials, request IDs or
//! response IDs: only pass/fail lines, the model name and stable turn codes.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use harness_core::{runtime::*, session::*, tools::SandboxConfig};
use harness_providers::anthropic::{
    self, AnthropicMessages, DEEPSEEK_LIVE_CHECK_MODEL, DEEPSEEK_STATE_SCHEMA,
    DEFAULT_MAX_OUTPUT_TOKENS, Profile,
};
use harness_providers::router::Router;
use serde_json::json;

const INSTANCE: &str = "deepseek";
const TURN_TIMEOUT: Duration = Duration::from_secs(180);
const NOTES: &str = "live-deepseek fixture notes";

fn main() -> std::process::ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            println!("live deepseek: fail (runtime)");
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
        "harness-live-deepseek-{}-{nanos}",
        std::process::id()
    ))
}

async fn run() -> u8 {
    let model = std::env::var("HARNESS_LIVE_DEEPSEEK_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEEPSEEK_LIVE_CHECK_MODEL.to_owned());
    let limit = anthropic::info(Profile::DeepSeek, &model)
        .map(|metadata| {
            format!(
                "context={} max_output={}",
                metadata
                    .limits
                    .context_tokens
                    .map_or("unknown".into(), |value| value.to_string()),
                metadata
                    .limits
                    .max_output_tokens
                    .map_or("unknown".into(), |value| value.to_string()),
            )
        })
        .unwrap_or_else(|| "context=unknown max_output=unknown".into());
    println!("live deepseek: model={model} {limit} request_max_output={DEFAULT_MAX_OUTPUT_TOKENS}");
    let adapter = match AnthropicMessages::deepseek_from_env() {
        Ok(adapter) => adapter,
        Err(error) => {
            println!("setup: fail ({:?})", error.code);
            return 1;
        }
    };
    let root = scratch();
    if std::fs::create_dir_all(root.join("workspace")).is_err() {
        println!("live deepseek: fail (scratch directory)");
        return 1;
    }
    let _ = std::fs::write(root.join("workspace/notes.txt"), NOTES);
    let result = session(&root, &model, adapter).await;
    let _ = std::fs::remove_dir_all(&root);
    result
}

async fn session(root: &Path, model: &str, adapter: AnthropicMessages) -> u8 {
    let workspace = root.join("workspace");
    let mut router = Router::new();
    router.register(INSTANCE, Arc::new(adapter));
    let mut catalog = harness_core::context::ModelCatalog::new();
    // The same sourced table the application configures.
    anthropic::extend_catalog(&mut catalog, Profile::DeepSeek, INSTANCE);
    let root = match DataRoot::open(root.join("state"), true) {
        Ok(root) => Arc::new(root),
        Err(_) => {
            println!("live deepseek: fail (data root)");
            return 1;
        }
    };
    let policy = json!({
        "policy_revision": "m6-00-live",
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
            println!("live deepseek: fail (configuration)");
            return 1;
        }
    };
    let header = SessionHeader {
        workspace: workspace.to_str().unwrap_or_default().into(),
        initial_model: ModelRef {
            provider_instance: INSTANCE.into(),
            model: model.into(),
        },
        policy_revision: "m6-00-live".into(),
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
            println!("live deepseek: fail (session)");
            return 1;
        }
    };
    let mut failed = false;
    // Turn 1: one streamed turn with a tool round trip. Thinking is on by
    // default, so the response also stores replay state.
    let tool_turn = turn(
        &handle,
        "Read the file notes.txt with the read tool, then reply with its contents only.",
        "streamed tool turn",
    )
    .await;
    failed |= tool_turn != Some(TurnOutcome::Completed);
    failed |= !tools_used(&handle, "tool round trip").await;
    failed |= !usage_reported(&handle).await;
    let blocks = thinking_blocks(&handle).await;
    if blocks == 0 {
        println!("thinking replay: skipped (the model stored no thinking block)");
    }
    // Turn 2: the projection of this new turn replays the state turn 1 stored.
    // A profile that replayed an incompatible block would be refused here.
    let replay_turn = turn(
        &handle,
        "Reply with the single word: pong",
        "thinking replay turn",
    )
    .await;
    match (blocks, replay_turn) {
        (0, _) => {}
        (count, Some(TurnOutcome::Completed)) => {
            println!("thinking replay: pass ({count} block(s) replayed, continuation completed)")
        }
        (count, _) => {
            println!(
                "thinking replay: fail ({count} block(s) stored, continuation did not complete)"
            );
            failed = true;
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

/// Start one turn and wait for its durable outcome.
async fn turn(handle: &SessionHandle, prompt: &str, label: &str) -> Option<TurnOutcome> {
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
        return None;
    };
    let deadline = tokio::time::Instant::now() + TURN_TIMEOUT;
    loop {
        if tokio::time::Instant::now() > deadline {
            println!("{label}: fail (timeout)");
            return None;
        }
        let Ok(view) = handle.view().await else {
            println!("{label}: fail (view)");
            return None;
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
            return Some(finished.outcome);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn tools_used(handle: &SessionHandle, label: &str) -> bool {
    let Ok(view) = handle.view().await else {
        return false;
    };
    let used = view
        .snapshot
        .entries()
        .any(|(_, stored)| match &stored.entry.payload {
            EntryPayload::ToolResult(result) => {
                matches!(result.result.status, ToolStatus::Succeeded)
            }
            _ => false,
        });
    println!("{label}: {}", if used { "pass" } else { "fail" });
    used
}

/// DeepSeek documents usage in the closing `message_delta`; a stored turn that
/// carries output tokens is the evidence the mapper read it.
async fn usage_reported(handle: &SessionHandle) -> bool {
    let Ok(view) = handle.view().await else {
        return false;
    };
    let usage = view
        .snapshot
        .entries()
        .find_map(|(_, stored)| match &stored.entry.payload {
            EntryPayload::AssistantResponse(response) => response.usage.clone(),
            _ => None,
        });
    let ok = usage
        .as_ref()
        .is_some_and(|usage| usage.output_tokens.is_some_and(|tokens| tokens > 0));
    println!(
        "usage: {} (input={} output={})",
        if ok { "pass" } else { "fail" },
        usage
            .as_ref()
            .and_then(|usage| usage.input_tokens)
            .map_or("-".into(), |value| value.to_string()),
        usage
            .as_ref()
            .and_then(|usage| usage.output_tokens)
            .map_or("-".into(), |value| value.to_string()),
    );
    ok
}

/// Thinking state stored under this profile's schema and owner. Anthropic state
/// would be invisible here, which is exactly the owner-scoping guarantee.
async fn thinking_blocks(handle: &SessionHandle) -> usize {
    let Ok(view) = handle.view().await else {
        return 0;
    };
    let mut blocks = 0;
    for (_, stored) in view.snapshot.entries() {
        let EntryPayload::AssistantResponse(response) = &stored.entry.payload else {
            continue;
        };
        for block in &response.content {
            if let ContentBlock::ProviderState {
                owner,
                schema,
                artifact,
            } = block
                && schema == DEEPSEEK_STATE_SCHEMA
                && owner.provider_instance == INSTANCE
                && artifact.bytes > 0
            {
                blocks += 1;
            }
        }
    }
    blocks
}
