//! Opt-in live check for the Anthropic Messages adapter. Run it through
//! `scripts/live-check.sh anthropic`; it is never part of `check.sh` or CI and
//! it skips (exit 77 from the script) when `ANTHROPIC_API_KEY` is unset.
//!
//! It spends the user's quota, so it is deliberately small: one streaming turn,
//! one tool invocation, usage from that turn, one invalid-model failure and one
//! cancellation. It never prints credentials, request IDs or response IDs.

use std::time::{Duration, Instant};

use harness_core::{
    context::ContextItem,
    model::{
        ArtifactSource, InferenceOptions, ModelInput, ModelProvider, ModelRequest,
        OperationControl, ProviderEvent, ProviderOutcome, ReasoningEffort,
    },
    session::{
        ContentBlock, EffectClass, EffectSet, ExecutionBackend, FinishReason, ModelRef, ResponseId,
        Usage,
    },
    tools::ToolDefinition,
};
use harness_providers::anthropic::{AnthropicMessages, LIVE_CHECK_MODEL, Profile, catalog, info};
use serde_json::json;

const TURN_TIMEOUT: Duration = Duration::from_secs(180);
const TOOL_NAME: &str = "live_echo";

fn instructions() -> &'static str {
    "You are a connectivity check. Follow instructions literally and keep answers short."
}

fn base_input(model: &str) -> ModelInput {
    ModelInput {
        model: ModelRef {
            provider_instance: "anthropic".into(),
            model: model.to_owned(),
        },
        items: vec![ContextItem::Instructions {
            instructions: vec![harness_core::context::Instruction {
                level: harness_core::context::InstructionLevel::System,
                text: instructions().into(),
            }],
        }],
        tools: Vec::new(),
        options: InferenceOptions {
            max_output_tokens: 512,
            reasoning_effort: Some(ReasoningEffort::Low),
            provider_options: Default::default(),
        },
        continuation: None,
    }
}

fn request(input: ModelInput) -> ModelRequest {
    ModelRequest {
        response_id: ResponseId::new(),
        input,
        artifacts: ArtifactSource::default(),
    }
}

fn user(text: &str) -> ContextItem {
    ContextItem::User {
        entry_id: harness_core::session::EntryId::new(),
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

fn echo_tool() -> ToolDefinition {
    ToolDefinition {
        name: TOOL_NAME.into(),
        description: "Echo one string back. Harmless; the live check never executes it.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": false,
        }),
        effects: EffectSet::new([EffectClass::Read]).expect("read effect"),
        backend: ExecutionBackend::TrustedFs,
    }
}

/// Consume one stream under a hard timeout; optionally cancel after `Started`.
async fn collect(
    adapter: &AnthropicMessages,
    request: ModelRequest,
    cancel_after_started: bool,
) -> Result<Vec<ProviderEvent>, String> {
    let control = OperationControl::default();
    let cancel = control.cancel.clone();
    let mut stream = adapter.stream(request, control);
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + TURN_TIMEOUT;
    loop {
        let next =
            tokio::time::timeout_at(deadline, futures_util::StreamExt::next(&mut stream)).await;
        match next {
            Err(_) => return Err("timed out".into()),
            Ok(None) => return Ok(events),
            Ok(Some(event)) => {
                if cancel_after_started
                    && matches!(event, ProviderEvent::Started)
                    && !events
                        .iter()
                        .any(|seen| matches!(seen, ProviderEvent::Started))
                {
                    cancel.cancel();
                }
                events.push(event);
            }
        }
    }
}

fn terminal(events: &[ProviderEvent]) -> Option<&ProviderEvent> {
    events
        .iter()
        .rev()
        .find(|event| matches!(event, ProviderEvent::Finished { .. }))
}

fn text_of(events: &[ProviderEvent]) -> String {
    let mut text = String::new();
    for event in events {
        if let ProviderEvent::TextDelta { text: delta, .. } = event {
            text.push_str(delta);
        }
    }
    text
}

fn last_usage(events: &[ProviderEvent]) -> Option<Usage> {
    events.iter().rev().find_map(|event| match event {
        ProviderEvent::UsageUpdated { usage } => Some(usage.clone()),
        _ => None,
    })
}

fn main() {
    let model = std::env::var("HARNESS_LIVE_ANTHROPIC_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| LIVE_CHECK_MODEL.to_owned());
    let limit = info(Profile::Anthropic, &model)
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
    // The catalog the application would configure, built from the same table.
    let _ = catalog(Profile::Anthropic, "anthropic");
    println!("live anthropic: model={model} {limit}");

    let adapter = match AnthropicMessages::from_env() {
        Ok(adapter) => adapter,
        Err(error) => {
            println!("setup: fail ({:?})", error.code);
            std::process::exit(1);
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let failures = runtime.block_on(run(&adapter, &model));
    if failures == 0 {
        println!("summary: pass");
    } else {
        println!("summary: fail ({failures} case(s))");
        std::process::exit(1);
    }
}

async fn run(adapter: &AnthropicMessages, model: &str) -> usize {
    let mut failures = 0;
    let started = Instant::now();

    // 1. Streaming text.
    let mut input = base_input(model);
    input.items.push(user("Reply with the single word: pong"));
    let text_turn = collect(adapter, request(input), false).await;
    let text_events = match &text_turn {
        Ok(events) => Some(events.clone()),
        Err(error) => {
            println!("text streaming: fail ({error})");
            failures += 1;
            None
        }
    };
    if let Some(events) = &text_events {
        let completed = terminal(events).is_some_and(|event| {
            matches!(
                event,
                ProviderEvent::Finished {
                    outcome: ProviderOutcome::Completed,
                    finish_reason: Some(FinishReason::Stop),
                    ..
                }
            )
        });
        let text = text_of(events);
        if completed && !text.trim().is_empty() {
            println!("text streaming: pass ({} chars)", text.chars().count());
        } else {
            println!(
                "text streaming: fail ({})",
                describe_terminal(terminal(events))
            );
            failures += 1;
        }
    }

    // 2. Usage from that turn.
    match text_events.as_deref().and_then(last_usage) {
        Some(usage) => println!(
            "usage: pass (input={} output={} cached={} reasoning={})",
            number(usage.input_tokens),
            number(usage.output_tokens),
            number(usage.cached_input_tokens),
            number(usage.reasoning_tokens),
        ),
        None => {
            println!("usage: fail (the provider reported no usage)");
            failures += 1;
        }
    }

    // 3. Tool invocation round trip.
    match tool_round_trip(adapter, model).await {
        Ok(detail) => println!("tool round trip: pass{detail}"),
        Err(error) => {
            println!("tool round trip: fail ({error})");
            failures += 1;
        }
    }

    // 4. One error case: an invalid model name. No reasoning control is asked
    // for, so the request reaches the API instead of failing locally on an
    // uncatalogued model's capabilities.
    let mut input = base_input("harness-live-invalid-model");
    input.options.reasoning_effort = None;
    input.items.push(user("This request must fail."));
    match collect(adapter, request(input), false).await {
        Ok(events) => match terminal(&events) {
            Some(ProviderEvent::Finished {
                outcome: ProviderOutcome::Failed,
                error: Some(error),
                ..
            }) => println!("invalid model: pass ({:?})", error.code),
            other => {
                println!("invalid model: fail ({})", describe_terminal(other));
                failures += 1;
            }
        },
        Err(error) => {
            println!("invalid model: fail ({error})");
            failures += 1;
        }
    }

    // 5. Cancellation mid-stream.
    let mut input = base_input(model);
    input
        .items
        .push(user("Count slowly from 1 to 200, one number per line."));
    match collect(adapter, request(input), true).await {
        Ok(events) => match terminal(&events) {
            Some(ProviderEvent::Finished {
                outcome: ProviderOutcome::Cancelled,
                ..
            })
            | None => println!("cancellation: pass"),
            other => {
                println!("cancellation: fail ({})", describe_terminal(other));
                failures += 1;
            }
        },
        Err(error) => {
            println!("cancellation: fail ({error})");
            failures += 1;
        }
    }

    println!("elapsed: {}s", started.elapsed().as_secs());
    failures
}

/// Ask for one call, then replay the call and its result for a final answer.
///
/// Signed thinking blocks are the one thing this example cannot materialize:
/// replaying them needs a stored artifact, which belongs to the runtime rather
/// than to a standalone example. When the response carries one, the check
/// verifies the call and states that the replay leg was not exercised instead
/// of sending a history the API is documented to reject.
async fn tool_round_trip(adapter: &AnthropicMessages, model: &str) -> Result<String, String> {
    let mut input = base_input(model);
    input.options.reasoning_effort = None;
    input.tools = vec![echo_tool()];
    input.items.push(user(&format!(
        "Call the {TOOL_NAME} tool with value \"ping\". Do not reply in text."
    )));
    let events = collect(adapter, request(input.clone()), false).await?;
    let call = events.iter().find_map(|event| match event {
        ProviderEvent::ToolCallStarted {
            block_index,
            name,
            provider_call_id,
        } => Some((*block_index, name.clone(), provider_call_id.clone())),
        _ => None,
    });
    let Some((index, name, call_id)) = call else {
        return Err("no tool call started".into());
    };
    if name != TOOL_NAME {
        return Err("the model selected an unexpected tool".into());
    }
    let ready = events.iter().any(
        |event| matches!(event, ProviderEvent::ToolCallReady { block_index } if *block_index == index),
    );
    if !ready
        || !matches!(
            terminal(&events),
            Some(ProviderEvent::Finished {
                outcome: ProviderOutcome::Completed,
                finish_reason: Some(FinishReason::ToolCalls),
                ..
            })
        )
    {
        return Err("no completed call".into());
    }
    let call_id = call_id.ok_or("the provider returned no call id")?;
    let mut fragments = String::new();
    for event in &events {
        if let ProviderEvent::ToolArgumentsDelta {
            block_index,
            fragment,
        } = event
            && *block_index == index
        {
            fragments.push_str(fragment);
        }
    }
    let arguments: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&fragments).map_err(|_| "invalid tool arguments")?;
    let echoed = arguments
        .get("value")
        .and_then(serde_json::Value::as_str)
        .filter(|value| *value == "ping")
        .ok_or("unexpected echo argument")?
        .to_owned();
    if events
        .iter()
        .any(|event| matches!(event, ProviderEvent::ProviderState { .. }))
    {
        return Ok(" (call ready; signed thinking needs runtime artifacts, so the replay leg was not sent)"
            .into());
    }

    // Scripted approval and pure echo execution, using the actual parsed call.
    let harness_call = harness_core::session::CallId::new();
    input.items.push(ContextItem::Assistant {
        entry_id: harness_core::session::EntryId::new(),
        outcome: harness_core::session::ResponseOutcome::Completed,
        content: vec![ContentBlock::ToolCall {
            call_id: harness_call.clone(),
            name: name.clone(),
            arguments,
            provider_call_id: Some(call_id),
        }],
    });
    input.items.push(ContextItem::ToolResult {
        entry_id: harness_core::session::EntryId::new(),
        provenance: harness_core::session::Provenance::Executor,
        result: harness_core::session::ToolResult {
            call: harness_core::session::CallRef {
                response_entry_id: harness_core::session::EntryId::new(),
                call_id: harness_call,
            },
            invocation_id: None,
            status: harness_core::session::ToolStatus::Succeeded,
            content: vec![ContentBlock::Text { text: echoed }],
            metadata: harness_core::session::ToolMetadata {
                exit_code: Some(0),
                duration_ms: Some(1),
                output_truncated: false,
                provenance: harness_core::session::Provenance::Executor,
                artifacts: Vec::new(),
                code: None,
                file_effect: None,
                file_paths: Vec::new(),
            },
        },
    });
    input
        .items
        .push(user("In one short sentence, say what the tool returned."));
    let final_events = collect(adapter, request(input), false).await?;
    if terminal(&final_events).is_some_and(|event| {
        matches!(
            event,
            ProviderEvent::Finished {
                outcome: ProviderOutcome::Completed,
                ..
            }
        ) && !text_of(&final_events).trim().is_empty()
    }) {
        Ok(String::new())
    } else {
        Err("no completed answer after the tool result".into())
    }
}

fn describe_terminal(event: Option<&ProviderEvent>) -> String {
    match event {
        Some(ProviderEvent::Finished {
            outcome,
            finish_reason,
            error,
            ..
        }) => {
            let code = error
                .as_ref()
                .map(|error| {
                    format!(
                        "{:?}/{:?}",
                        error.code,
                        error.provider_code.as_deref().unwrap_or("-")
                    )
                })
                .unwrap_or_else(|| "-".into());
            format!("outcome={outcome:?} finish={finish_reason:?} error={code}")
        }
        Some(_) => "no terminal event".into(),
        None => "stream ended with no terminal event".into(),
    }
}

fn number(value: Option<u64>) -> String {
    value.map_or_else(|| "-".into(), |value| value.to_string())
}
