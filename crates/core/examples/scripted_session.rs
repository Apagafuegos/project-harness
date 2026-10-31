//! Milestone 2 end to end, offline and deterministic.
//!
//! A session with a configured context limit and an instruction runs a turn
//! that stops and a turn whose tool call is answered as `tools_unavailable`.
//! The process is killed during a third turn, while text is streaming. A new
//! process reopens the session, recovers it without being asked, and prints
//! the final context report. Identities and time come from
//! `DeterministicEnv`, and the provider is the scripted mock, so every run
//! prints the same bytes.
//!
//! ```text
//! cargo run --example scripted_session
//! ```

use std::{sync::Arc, time::Duration};

use harness_core::{
    context::ModelLimits,
    model::{
        ProviderEvent,
        scripted::{Script, ScriptedProvider, Step},
    },
    runtime::*,
    session::*,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const INSTRUCTIONS: &[u8] = br#"{"instructions":[{"level":"system","text":"You are a careful assistant. Say so when you cannot act."}]}"#;

fn main() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let root = Arc::new(DataRoot::open(temp.path().join("state"), true)?);
    let model = ModelRef {
        provider_instance: "mock".into(),
        model: "scripted".into(),
    };
    let header = SessionHeader {
        workspace: "/workspace".into(),
        initial_model: model.clone(),
        policy_revision: "initial".into(),
        configuration: root.put_artifact(INSTRUCTIONS)?,
    };
    let session: SessionId = DeterministicEnv::new(0).entry_id().to_string().parse()?;
    let mut config = DriverConfig::default();
    config.runtime.context.catalog.insert(
        model,
        ModelLimits {
            context_tokens: Some(32_000),
            max_output_tokens: Some(4_096),
        },
    );
    let provider = Arc::new(ScriptedProvider::new(scripts()));

    println!("== process 1: create session {session}");
    println!("model mock/scripted, context limit 32000 tokens, 1 system instruction");
    let process = runtime()?;
    let handle = process.block_on(async {
        let handle = SessionHandle::create_with_env(
            root.clone(),
            session.clone(),
            header,
            provider.clone(),
            config.clone(),
            DeterministicEnv::new(1),
        )
        .await?;
        for prompt in ["Hello!", "List the files in the workspace."] {
            let turn = start(&handle, prompt).await?;
            let view = until(&handle, idle).await?;
            print_turn(&view.snapshot, &turn)?;
        }
        let turn = start(&handle, "Write a short poem.").await?;
        let view = until(&handle, |view| {
            view.partial
                .as_ref()
                .is_some_and(|partial| !partial.blocks.is_empty())
        })
        .await?;
        println!("turn {turn}: \"Write a short poem.\"");
        if let Some(PartialBlock::Text(text)) = view.partial.as_ref().and_then(|p| p.blocks.get(&0))
        {
            println!("  streaming, not yet durable: {text:?}");
        }
        println!("  phase {:?}", view.phase);
        Ok::<_, Box<dyn std::error::Error>>(handle)
    })?;
    // Kill the process: dropping its Tokio runtime drops the driver at an
    // await point. Nothing shuts down, cancels or records the partial text.
    drop(process);
    drop(handle);
    println!("== process 1 killed while streaming");
    released(&root, &session)?;

    println!("== process 2: reopen");
    let streams_before = provider.streams().len();
    let process = runtime()?;
    process.block_on(async {
        let handle = SessionHandle::open_with_env(
            root.clone(),
            session.clone(),
            provider.clone(),
            config.clone(),
            DeterministicEnv::new(2),
        )
        .await?;
        // Register before requesting the first view, but recovery's commit
        // may win that request. Its events can already be covered by Reset.
        let mut subscription = handle.subscribe();
        println!(
            "{}",
            wait_for_recovery(async || subscription.next().await).await?
        );
        let view = handle.view().await.map_err(|r| format!("{r:?}"))?;
        println!(
            "streams opened by recovery: {}",
            provider.streams().len() - streams_before
        );
        println!("== journal, one line per record");
        print_journal(&view.snapshot);
        println!("== final context report");
        let report = handle.inspect_context(None, None).await?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        handle.shutdown().await;
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    Ok(())
}

/// One script per inference: a stop, a tool call and the answer after its
/// result, and a stream that stalls mid-text until the process dies.
fn scripts() -> Vec<Script> {
    vec![
        Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "Hello! How can I help?"),
            Step::completed(FinishReason::Stop),
        ]),
        Script::new([
            Step::Event(ProviderEvent::Started),
            Step::Event(ProviderEvent::ToolCallStarted {
                block_index: 0,
                name: "list_files".into(),
                provider_call_id: Some("call-1".into()),
            }),
            Step::Event(ProviderEvent::ToolArgumentsDelta {
                block_index: 0,
                fragment: r#"{"path":"."}"#.into(),
            }),
            Step::Event(ProviderEvent::ToolCallReady { block_index: 0 }),
            Step::completed(FinishReason::ToolCalls),
        ]),
        Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "I cannot list files: tools are unavailable here."),
            Step::completed(FinishReason::Stop),
        ]),
        Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "Roses are red,"),
            Step::Stall,
        ]),
    ]
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?)
}

fn idle(view: &RuntimeView) -> bool {
    view.phase == Phase::Durable(RuntimeState::Idle)
}

async fn start(handle: &SessionHandle, prompt: &str) -> Result<TurnId> {
    let command = Command::StartTurn {
        command_id: CommandId::new(),
        content: vec![ContentBlock::Text {
            text: prompt.into(),
        }],
    };
    match handle.command(command).await {
        Ok(Accepted::TurnStarted { turn_id, .. }) => Ok(turn_id),
        other => Err(format!("turn not started: {other:?}").into()),
    }
}

async fn until(
    handle: &SessionHandle,
    check: impl Fn(&RuntimeView) -> bool,
) -> Result<RuntimeView> {
    loop {
        let view = handle.view().await.map_err(|r| format!("{r:?}"))?;
        if check(&view) {
            return Ok(view);
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// The killed process's worker thread releases the journal lock on its own.
fn released(root: &DataRoot, session: &SessionId) -> Result<()> {
    loop {
        match JournalStore::inspect(root, session) {
            Ok(_) => return Ok(()),
            Err(StoreError::Locked) => std::thread::sleep(Duration::from_millis(1)),
            Err(error) => return Err(error.into()),
        }
    }
}

fn text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn print_turn(snapshot: &SessionSnapshot, turn: &TurnId) -> Result<()> {
    println!("turn {turn}:");
    for stored in snapshot.ancestry(snapshot.head())? {
        if stored.entry.turn_id.as_ref() != Some(turn) {
            continue;
        }
        match &stored.entry.payload {
            EntryPayload::UserMessage(message) => {
                println!("  user: {:?}", text(&message.content))
            }
            EntryPayload::AssistantResponse(response) => {
                let finish = response
                    .finish_reason
                    .map(|reason| format!("/{reason:?}"))
                    .unwrap_or_default();
                println!(
                    "  assistant ({:?}{finish}): {:?}",
                    response.outcome,
                    text(&response.content)
                );
                for block in &response.content {
                    if let ContentBlock::ToolCall {
                        name, arguments, ..
                    } = block
                    {
                        println!("  tool call {name} {}", serde_json::to_string(arguments)?);
                    }
                }
            }
            EntryPayload::ToolResult(result) => println!(
                "  tool result ({:?}, {:?}): {:?}",
                result.result.status,
                result.result.metadata.provenance,
                text(&result.result.content)
            ),
            _ => {}
        }
    }
    for (_, _, event) in snapshot.events() {
        if let JournalEvent::TurnFinished(finished) = event
            && finished.turn_id == *turn
        {
            println!(
                "  finished: {:?} {}",
                finished.outcome,
                finished.code.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(())
}

/// Report only the settled phase, not timing-dependent live recovery events.
/// The journal below reports recovery's durable records in either ordering.
async fn wait_for_recovery(
    mut next: impl AsyncFnMut() -> Option<SubscriptionItem>,
) -> Result<String> {
    loop {
        let done = match next().await {
            Some(SubscriptionItem::Reset(client)) => idle(client.view()),
            Some(SubscriptionItem::Event(event)) => matches!(
                event.kind,
                EventKind::PhaseChanged {
                    phase: Phase::Durable(RuntimeState::Idle),
                    ..
                }
            ),
            None => return Err("the session closed".into()),
        };
        if done {
            return Ok(format!(
                "view: phase {:?}",
                Phase::Durable(RuntimeState::Idle)
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::context::SessionConfiguration;
    use std::collections::VecDeque;

    // Execute only storage effects, explicitly controlling the boundary at
    // which the subscription takes its view. No OS scheduling or sleeps.
    fn settle(
        runtime: &mut Runtime,
        store: &mut JournalStore,
        env: &mut DeterministicEnv,
        effects: Vec<Effect>,
    ) -> Vec<RuntimeEvent> {
        let mut pending = VecDeque::from(effects);
        let mut events = Vec::new();
        while let Some(effect) = pending.pop_front() {
            match effect {
                Effect::Persist {
                    token,
                    mut commit,
                    command,
                    at,
                    ..
                } => {
                    if let Some((id, command)) = command {
                        store.bind_command(&mut commit, id, &command).unwrap();
                    }
                    let ack = store.append(commit, at).unwrap();
                    pending.extend(runtime.step(
                        Input::Persisted {
                            token,
                            ack,
                            snapshot: store.snapshot(),
                        },
                        env,
                    ));
                }
                Effect::Emit(event) => events.push(event),
                Effect::StartInference { .. } => {}
                Effect::Reply { result, .. } => {
                    result.unwrap();
                }
                Effect::SetTimer { .. } | Effect::CancelTimer { .. } => {}
                other => panic!("unexpected effect: {other:?}"),
            }
        }
        events
    }

    #[tokio::test]
    async fn recovery_reset_ordering() {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(DataRoot::open(temp.path().join("state"), true).unwrap());
        let model = ModelRef {
            provider_instance: "mock".into(),
            model: "scripted".into(),
        };
        let header = SessionHeader {
            workspace: "/workspace".into(),
            initial_model: model.clone(),
            policy_revision: "initial".into(),
            configuration: root.put_artifact(INSTRUCTIONS).unwrap(),
        };
        let decoded = SessionConfiguration::load(&root, &header.configuration).unwrap();
        let mut env = DeterministicEnv::new(1);
        let mut store = JournalStore::create(root, SessionId::new(), header, env.now()).unwrap();
        let mut config = RuntimeConfig::default();
        config.context.catalog.insert(
            model,
            ModelLimits {
                context_tokens: Some(32_000),
                max_output_tokens: Some(4_096),
            },
        );
        let mut runtime = Runtime::new(store.snapshot(), decoded.clone(), config.clone(), &mut env);
        let effects = runtime.step(
            Input::Command {
                request: RequestId(1),
                command: Command::StartTurn {
                    command_id: CommandId::new(),
                    content: vec![ContentBlock::Text {
                        text: "hello".into(),
                    }],
                },
            },
            &mut env,
        );
        settle(&mut runtime, &mut store, &mut env, effects);
        assert_eq!(runtime.phase(), Phase::Durable(RuntimeState::Inferring));
        // Process restart: recovery issues a commit, but we hold its answer.
        let mut env = DeterministicEnv::new(2);
        let mut runtime = Runtime::new(store.snapshot(), decoded, config, &mut env);
        let effects = runtime.step(Input::Start, &mut env);
        let early = runtime.view();
        assert_eq!(early.phase, Phase::RecoveryRequired);
        let events = settle(&mut runtime, &mut store, &mut env, effects);
        let late = runtime.view();
        assert!(idle(&late));
        assert!(store.snapshot().events().any(|(_, _, event)| matches!(
            event, JournalEvent::TurnFinished(f) if f.code.as_deref() == Some("process_restart")
        )));
        let mut outputs = Vec::new();
        for (commit_before_view, view) in [(false, early), (true, late)] {
            let mut client = ClientView::from(view);
            let mut items = VecDeque::from([SubscriptionItem::Reset(client.clone())]);
            // This is the subscription's gap-free reducer: the late reset
            // covers all recovery events, so none may be replayed to the client.
            for event in &events {
                match client.apply(event) {
                    ApplyOutcome::Applied => {
                        items.push_back(SubscriptionItem::Event(event.clone()))
                    }
                    ApplyOutcome::Duplicate => {}
                    ApplyOutcome::NeedsResync => panic!("unexpected gap"),
                }
            }
            if commit_before_view {
                assert_eq!(items.len(), 1, "late reset must cover every recovery event");
                assert!(matches!(&items[0], SubscriptionItem::Reset(c) if idle(c.view())));
            } else {
                assert!(
                    items.len() > 1,
                    "early reset must be followed by recovery events"
                );
            }
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                wait_for_recovery(async || match items.pop_front() {
                    Some(item) => Some(item),
                    None => std::future::pending().await,
                }),
            )
            .await
            .expect("recovery was already covered by the reset; do not wait for another event");
            outputs.push(result.unwrap());
        }
        assert_eq!(outputs[0], "view: phase Durable(Idle)");
        assert_eq!(outputs[0], outputs[1]);
    }
}

/// One line per record after the header: entries, then journal events.
fn print_journal(snapshot: &SessionSnapshot) {
    let mut lines = vec![Vec::new(); snapshot.sequence() as usize];
    let mut entries: Vec<_> = snapshot.entries().map(|(_, stored)| stored).collect();
    entries.sort_by_key(|stored| (stored.sequence, stored.entry_index));
    for stored in entries {
        let kind = match &stored.entry.payload {
            EntryPayload::UserMessage(_) => "user".to_owned(),
            EntryPayload::AssistantResponse(r) => format!("assistant({:?})", r.outcome),
            EntryPayload::ToolResult(r) => format!("result({:?})", r.result.status),
            EntryPayload::ModelChanged(_) => "model_changed".to_owned(),
            EntryPayload::Compaction(_) => "compaction".to_owned(),
            EntryPayload::ContextControl(c) => {
                format!("context_control({:?})", c.mode).to_lowercase()
            }
        };
        lines[stored.sequence as usize - 1].push(kind);
    }
    for (sequence, _, event) in snapshot.events() {
        let line = match event {
            JournalEvent::TurnStarted(_) => "turn_started".to_owned(),
            JournalEvent::StateChanged(c) => format!("{:?}>{:?}", c.from, c.to),
            JournalEvent::TurnFinished(f) => format!(
                "turn_finished({:?}, {})",
                f.outcome,
                f.code.as_deref().unwrap_or("-")
            ),
            other => format!("{other:?}"),
        };
        lines[*sequence as usize - 1].push(line);
    }
    for (index, line) in lines.iter().enumerate() {
        println!("{:>2}: {}", index + 1, line.join(" "));
    }
}
