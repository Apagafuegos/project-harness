//! Offline end-to-end mid-session switching: a real durable session, the
//! router, and the two real adapters against attributed loopback fixture
//! servers. No credential is read and nothing reaches the network.
//!
//! Every test drives the public `SessionHandle` API and inspects three kinds of
//! evidence: what the runtime asked a provider for (a recording wrapper in
//! front of each adapter), exactly what that adapter submitted (the loopback
//! server's decoded body and header block), and what became durable (the
//! published snapshot).

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::StreamExt;
use harness_core::{
    context::{ModelCapabilities, ModelCatalog, ModelInfo, ModelLimits, Part, RequiredCapability},
    model::{ModelEventStream, ModelProvider, ModelRequest, OperationControl, ProviderEvent},
    runtime::*,
    session::*,
    tools::SandboxConfig,
};
use harness_providers::{
    anthropic::{self, AnthropicMessages},
    credentials::{Credential, CredentialSource},
    openai::{self, OpenAiResponses, Profile},
    router::Router,
    transport::{Transport, TransportConfig},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[allow(dead_code, clippy::duplicate_mod)]
#[path = "../../../tests/support/loopback.rs"]
mod loopback;

/// Provider instances, distinct because ownership of native state and the
/// account behind it are per instance, never per adapter type.
const OPENAI: &str = "openai-codex";
const ANTHROPIC: &str = "anthropic";
const OPENAI_MODEL: &str = "gpt-6.1-sol";
const ANTHROPIC_MODEL: &str = "claude-sonnet-5-5";
const SUMMARY_MODEL: &str = "claude-haiku-5-5";
/// Catalogued for this test only: no image input.
const TEXT_ONLY_MODEL: &str = "claude-text-only-fixture";

const SUMMARY: &str = "## Task and constraints\nRead files only.\n## Decisions\nNone.\n## Files and actions\nRead notes.txt.\n## Outcomes\nEvery turn completed.\n## Unresolved work\nNone.\n";

fn run_header() -> &'static str {
    static NONCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NONCE.get_or_init(loopback::nonce)
}

// --------------------------------------------------------------- fake server

/// One canned HTTP response, in the fixtures' own `status:/header:/---` shape.
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn fixture(text: &str) -> Self {
        let (head, body) = text.split_once("---\n").expect("fixture separator");
        let mut status = 200;
        let mut headers = Vec::new();
        for line in head.lines() {
            if let Some(value) = line.strip_prefix("status:") {
                status = value.trim().parse().expect("status");
            } else if let Some(value) = line.strip_prefix("header:")
                && let Some((name, value)) = value.trim().split_once(':')
            {
                headers.push((name.trim().to_owned(), value.trim().to_owned()));
            }
        }
        Self {
            status,
            headers,
            body: body.as_bytes().to_vec(),
        }
    }

    /// An SSE reply from event/data pairs, as both providers stream them.
    fn sse(events: &[(&str, Value)]) -> Self {
        let mut body = String::new();
        for (event, data) in events {
            body.push_str(&format!("event: {event}\ndata: {data}\n\n"));
        }
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: body.into_bytes(),
        }
    }

    fn summary(text: &str) -> Self {
        Self::sse(&[
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"msg_summary","type":"message",
                    "role":"assistant","model":SUMMARY_MODEL,"content":[],"stop_reason":null,
                    "stop_sequence":null}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,
                    "content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,
                    "delta":{"type":"text_delta","text":text}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ])
    }
}

/// One attributed request, exactly as it arrived.
struct Received {
    header_block: String,
    body: Value,
}

impl Received {
    fn model(&self) -> &str {
        self.body["model"].as_str().unwrap_or_default()
    }

    /// Whether any submitted message content block has one of `types`.
    fn has_block(&self, types: &[&str]) -> bool {
        self.body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .any(|block| {
                block["type"]
                    .as_str()
                    .is_some_and(|kind| types.contains(&kind))
            })
    }
}

/// A loopback endpoint with its own random path and run header. Only requests
/// carrying both are counted or answered; anything else is closed untouched.
struct Server {
    url: String,
    log: Arc<Mutex<Vec<Received>>>,
    queues: Arc<Mutex<BTreeMap<String, VecDeque<Reply>>>>,
    task: tokio::task::JoinHandle<()>,
    _side_probe: loopback::SideProbe,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Bodies are attributed by path; this adapter adds the run header.
fn credential(secret: &str, anthropic: bool) -> Credential {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-harness-test", run_header().parse().expect("header"));
    if anthropic {
        Credential::api_key(secret.to_owned(), headers, None).expect("credential")
    } else {
        Credential::bearer(secret.to_owned(), headers, None).expect("credential")
    }
}

struct Fixed(Credential);

impl CredentialSource for Fixed {
    fn credential(&self) -> Result<Credential, ModelError> {
        Ok(self.0.clone())
    }
}

impl Server {
    async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let side_probe = loopback::SideProbe::register(address);
        let path = format!("/endpoint/{}", loopback::nonce());
        let url = format!("http://{address}{path}");
        let log = Arc::new(Mutex::new(Vec::new()));
        let queues: Arc<Mutex<BTreeMap<String, VecDeque<Reply>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let (task_log, task_queues) = (log.clone(), queues.clone());
        let task_listener = listener;
        let task = tokio::spawn(async move {
            let mut handlers = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = tokio::select! {
                    accepted = task_listener.accept() => accepted.expect("accept"),
                    Some(result) = handlers.join_next(), if !handlers.is_empty() => {
                        result.expect("handler");
                        continue;
                    }
                };
                let (log, queues) = (task_log.clone(), task_queues.clone());
                let path = path.clone();
                handlers.spawn(async move {
                    let request = tokio::time::timeout(
                        Duration::from_secs(2),
                        read_request(&mut socket, &path),
                    )
                    .await
                    .ok()
                    .flatten();
                    // Unattributed peers (host services probing the port)
                    // never reach the recorded evidence.
                    let Some(request) = request else {
                        return;
                    };
                    let model = request.body["model"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    log.lock().expect("log").push(request);
                    let reply = queues
                        .lock()
                        .expect("queues")
                        .get_mut(&model)
                        .and_then(VecDeque::pop_front);
                    let reply = reply.unwrap_or_else(|| Reply {
                        status: 500,
                        headers: vec![("content-type".into(), "application/json".into())],
                        body: br#"{"error":{"type":"api_error","message":"no fixture reply"}}"#
                            .to_vec(),
                    });
                    let mut response = format!("HTTP/1.1 {} Response\r\n", reply.status);
                    for (name, value) in &reply.headers {
                        response.push_str(&format!("{name}: {value}\r\n"));
                    }
                    response.push_str(&format!(
                        "content-length: {}\r\nconnection: close\r\n\r\n",
                        reply.body.len()
                    ));
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&reply.body).await;
                    let _ = socket.flush().await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self {
            url,
            log,
            queues,
            task,
            _side_probe: side_probe,
        }
    }

    /// Queue one reply for the model name in the request body.
    fn reply(&self, model: &str, reply: Reply) {
        self.queues
            .lock()
            .expect("queues")
            .entry(model.to_owned())
            .or_default()
            .push_back(reply);
    }

    fn received(&self) -> Vec<Received> {
        self.log
            .lock()
            .expect("log")
            .iter()
            .map(|received| Received {
                header_block: received.header_block.clone(),
                body: received.body.clone(),
            })
            .collect()
    }
}

/// Read one attributed request; `None` for anything that is not ours.
async fn read_request(socket: &mut TcpStream, path: &str) -> Option<Received> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut head_end = None;
    let mut length = 0usize;
    loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 || buffer.len() + read > 2 * 1024 * 1024 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if head_end.is_none()
            && let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
        {
            head_end = Some(position + 4);
            let raw_head = String::from_utf8_lossy(&buffer[..position]).to_string();
            let head = raw_head.to_ascii_lowercase();
            if raw_head.lines().next() != Some(format!("POST {path} HTTP/1.1").as_str())
                || !head
                    .lines()
                    .any(|line| line == format!("x-harness-test: {}", run_header()))
            {
                return None;
            }
            for line in head.lines() {
                if let Some(value) = line.strip_prefix("content-length:") {
                    length = value.trim().parse().ok()?;
                }
            }
        }
        if let Some(end) = head_end
            && buffer.len() >= end + length
        {
            let head = String::from_utf8_lossy(&buffer[..end - 4]).to_ascii_lowercase();
            return serde_json::from_slice(&buffer[end..end + length])
                .ok()
                .map(|body| Received {
                    header_block: head,
                    body,
                });
        }
    }
}

// ------------------------------------------------------- recorded provider

/// Records every logical request handed to one adapter, then delegates
/// unchanged: the same request, the same control, no wrapper task.
struct Recording {
    inner: Arc<dyn ModelProvider>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    terminal_gate: Arc<TerminalGate>,
}

#[derive(Default)]
struct TerminalGate {
    armed: std::sync::atomic::AtomicBool,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl ModelProvider for Recording {
    fn stream(&self, request: ModelRequest, control: OperationControl) -> ModelEventStream {
        self.requests
            .lock()
            .expect("requests")
            .push(request.clone());
        let mut stream = self.inner.stream(request, control);
        let gate = self.terminal_gate.clone();
        Box::pin(async_stream::stream! {
            while let Some(event) = stream.next().await {
                if matches!(event, ProviderEvent::Finished { .. })
                    && gate.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    gate.reached.notify_one();
                    gate.release.notified().await;
                }
                yield event;
            }
        })
    }
}

// ------------------------------------------------------------------ fixture

struct Fixture {
    /// Keeps the private data root and workspace alive for the session.
    _temp: tempfile::TempDir,
    handle: SessionHandle,
    openai: Arc<Server>,
    anthropic: Arc<Server>,
    openai_requests: Arc<Mutex<Vec<ModelRequest>>>,
    anthropic_requests: Arc<Mutex<Vec<ModelRequest>>>,
    text_only: ModelRef,
    root: Arc<DataRoot>,
    provider: Arc<dyn ModelProvider>,
    driver: DriverConfig,
    host: ToolHostConfig,
    session: SessionId,
    terminal_gate: Arc<TerminalGate>,
}

impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        std::fs::write(workspace.join("notes.txt"), "fixture notes").expect("notes");
        let root = Arc::new(DataRoot::open(temp.path().join("state"), true).expect("data root"));
        let openai = Arc::new(Server::start().await);
        let anthropic = Arc::new(Server::start().await);
        let transport = Transport::new(TransportConfig {
            allow_loopback_http: true,
            ..TransportConfig::default()
        })
        .expect("transport");
        let openai_requests = Arc::new(Mutex::new(Vec::new()));
        let anthropic_requests = Arc::new(Mutex::new(Vec::new()));
        let openai_adapter = OpenAiResponses::with_transport(
            Profile::ChatGptCodex,
            Arc::new(Fixed(credential("fixture-openai", false))),
            transport.clone(),
        )
        .with_endpoint(openai.url.clone());
        let anthropic_adapter = AnthropicMessages::with_transport(
            Arc::new(Fixed(credential("fixture-anthropic", true))),
            transport,
        )
        .with_endpoint(anthropic.url.clone());
        let terminal_gate = Arc::new(TerminalGate::default());
        let provider: Arc<dyn ModelProvider> = Arc::new(
            Router::new()
                .with(
                    OPENAI,
                    Arc::new(Recording {
                        inner: Arc::new(openai_adapter),
                        requests: openai_requests.clone(),
                        terminal_gate: terminal_gate.clone(),
                    }),
                )
                .with(
                    ANTHROPIC,
                    Arc::new(Recording {
                        inner: Arc::new(anthropic_adapter),
                        requests: anthropic_requests.clone(),
                        terminal_gate: terminal_gate.clone(),
                    }),
                ),
        );
        let text_only = ModelRef {
            provider_instance: ANTHROPIC.into(),
            model: TEXT_ONLY_MODEL.into(),
        };
        let mut catalog = ModelCatalog::new();
        openai::extend_catalog(&mut catalog, Profile::ChatGptCodex, OPENAI);
        anthropic::extend_catalog(&mut catalog, ANTHROPIC);
        catalog.insert(
            text_only.clone(),
            ModelInfo {
                limits: ModelLimits {
                    context_tokens: Some(200_000),
                    max_output_tokens: None,
                },
                capabilities: ModelCapabilities {
                    input_media: Default::default(),
                    tools: true,
                    reasoning_effort: false,
                },
            },
        );
        let configuration = json!({
            "policy_revision": "m4-07",
            "enabled_tools": ["read"],
            "workspace_read": "allow",
            "workspace_mutation": "deny",
            "process": "deny",
            "network": "deny",
            "protected_paths": [],
            "allowances": [],
            "compaction": {
                "summary_model": {
                    "provider_instance": ANTHROPIC,
                    "model": SUMMARY_MODEL,
                },
                "retain_recent_turns": 2,
                "automatic": false,
            },
        });
        let artifact = root
            .put_artifact(&serde_json::to_vec(&configuration).expect("configuration"))
            .expect("artifact");
        let session = SessionId::new();
        let header = SessionHeader {
            workspace: workspace.to_str().expect("workspace path").into(),
            initial_model: ModelRef {
                provider_instance: OPENAI.into(),
                model: OPENAI_MODEL.into(),
            },
            policy_revision: "m4-07".into(),
            configuration: artifact,
        };
        let mut driver = DriverConfig::default();
        driver.runtime.context.catalog = catalog;
        let host = ToolHostConfig {
            workspace_root: workspace.clone(),
            data_root: root.path().to_owned(),
            sandbox: None::<SandboxConfig>,
        };
        let handle = SessionHandle::create_with_tools(
            root.clone(),
            session.clone(),
            header,
            provider.clone(),
            driver.clone(),
            &host,
        )
        .await
        .expect("session");
        Self {
            _temp: temp,
            handle,
            openai,
            anthropic,
            openai_requests,
            anthropic_requests,
            text_only,
            root,
            provider,
            driver,
            host,
            session,
            terminal_gate,
        }
    }

    async fn reopen(&mut self) {
        self.handle.shutdown().await;
        self.handle = SessionHandle::open_with_tools(
            self.root.clone(),
            self.session.clone(),
            self.provider.clone(),
            self.driver.clone(),
            &self.host,
        )
        .await
        .expect("reopen real storage");
    }

    fn openai_model() -> ModelRef {
        ModelRef {
            provider_instance: OPENAI.into(),
            model: OPENAI_MODEL.into(),
        }
    }

    fn anthropic_model() -> ModelRef {
        ModelRef {
            provider_instance: ANTHROPIC.into(),
            model: ANTHROPIC_MODEL.into(),
        }
    }

    async fn sequence(&self) -> u64 {
        self.handle.view().await.expect("view").snapshot.sequence()
    }

    async fn turn(&self, text: &str) -> RuntimeView {
        self.start(Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![ContentBlock::Text { text: text.into() }],
        })
        .await
    }

    async fn start(&self, command: Command) -> RuntimeView {
        let accepted = self.handle.command(command).await.expect("command");
        let Accepted::TurnStarted { turn_id, .. } = accepted else {
            panic!("expected a started turn, got {accepted:?}")
        };
        self.finish(turn_id).await
    }

    async fn finish(&self, turn: TurnId) -> RuntimeView {
        for _ in 0..20_000 {
            let view = self.handle.view().await.expect("view");
            let finished = view.snapshot.events().any(
                |(_, _, event)| matches!(event, JournalEvent::TurnFinished(f) if f.turn_id == turn),
            );
            if finished && view.phase == Phase::Durable(RuntimeState::Idle) {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("turn {turn} did not finish");
    }

    async fn switch(&self, model: ModelRef, policy: Portability) -> Result<Accepted, Rejection> {
        self.handle
            .command(Command::SwitchModel { model, policy })
            .await
    }

    fn recorded(&self, provider: &str) -> Vec<ModelRequest> {
        let requests = match provider {
            OPENAI => &self.openai_requests,
            _ => &self.anthropic_requests,
        };
        requests.lock().expect("requests").clone()
    }
}

/// The last logical request recorded for one provider instance.
fn last(fixture: &Fixture, provider: &str) -> ModelRequest {
    fixture
        .recorded(provider)
        .pop()
        .expect("the provider was asked at least once")
}

/// Every assistant state block in a recorded request, as (owner, schema).
fn states(request: &ModelRequest) -> Vec<(ModelRef, String)> {
    let mut found = Vec::new();
    for item in &request.input.items {
        if let harness_core::context::ContextItem::Assistant { content, .. } = item {
            for block in content {
                if let ContentBlock::ProviderState { owner, schema, .. } = block {
                    found.push((owner.clone(), schema.clone()));
                }
            }
        }
    }
    found
}

/// The text of the newest completed response only: earlier turns stay in the
/// projection, so an accumulated string would mix them.
fn last_response_text(view: &RuntimeView) -> String {
    let mut text = String::new();
    for (_, stored) in view.snapshot.entries() {
        if let EntryPayload::AssistantResponse(response) = &stored.entry.payload {
            let mut current = String::new();
            for block in &response.content {
                if let ContentBlock::Text { text: body } = block {
                    current.push_str(body);
                }
            }
            if !current.is_empty() {
                text = current;
            }
        }
    }
    text
}

// -------------------------------------------------------------- reject/omit

#[tokio::test]
async fn openai_to_anthropic_is_rejected_by_default_and_omits_every_entry() {
    let fixture = Fixture::new().await;
    fixture.openai.reply(
        OPENAI_MODEL,
        Reply::fixture(include_str!("../fixtures/openai/native_state.sse")),
    );
    let view = fixture.turn("hello").await;
    assert_eq!(last_response_text(&view), "pong");
    let before = fixture.sequence().await;
    let head_response = view
        .snapshot
        .entries()
        .filter_map(|(_, stored)| match &stored.entry.payload {
            EntryPayload::AssistantResponse(response)
                if matches!(response.outcome, ResponseOutcome::Completed) =>
            {
                Some(stored.entry.id.clone())
            }
            _ => None,
        })
        .next()
        .expect("one completed response");

    // Reject is the default policy: encrypted OpenAI reasoning cannot be carried
    // to Anthropic, so the switch is refused with the classified preview and
    // nothing is written.
    let rejected = fixture
        .switch(Fixture::anthropic_model(), Portability::Reject)
        .await
        .expect_err("encrypted reasoning is not portable");
    let Rejection::PortabilityLoss(preview) = &rejected else {
        panic!("expected portability_loss, got {rejected:?}")
    };
    assert_eq!(fixture.sequence().await, before, "a refusal writes nothing");
    assert!(preview.required_incompatibilities.is_empty());
    assert_eq!(preview.model_at_head, Fixture::openai_model());
    assert_eq!(preview.preview_model, Fixture::anthropic_model());
    assert_eq!(preview.would_stop_being_portable.len(), 1);
    let loss = &preview.would_stop_being_portable[0];
    assert_eq!(loss.entry_id, head_response);
    assert_eq!(loss.owner, Fixture::openai_model());
    assert_eq!(loss.schema, openai::STATE_SCHEMA);
    assert_eq!(loss.part, Part::Block { index: 1 });
    assert!(
        loss.bytes > 0,
        "the report states a byte count, never the payload"
    );

    // OmitOptional accepts exactly that preview and switches.
    let accepted = fixture
        .switch(Fixture::anthropic_model(), Portability::OmitOptional)
        .await
        .expect("explicit omission");
    let Accepted::ModelSwitched {
        preview: accepted_preview,
        ..
    } = accepted
    else {
        panic!("expected model_switched, got {accepted:?}")
    };
    assert_eq!(accepted_preview.as_ref(), preview.as_ref());
    assert!(fixture.sequence().await > before);

    // The next request is entirely Anthropic: no OpenAI state, no continuation,
    // no artifact reads at all.
    fixture.anthropic.reply(
        ANTHROPIC_MODEL,
        Reply::fixture(include_str!("../fixtures/anthropic/text.sse")),
    );
    let view = fixture.turn("next").await;
    assert_eq!(last_response_text(&view), "Hello world");
    let request = last(&fixture, ANTHROPIC);
    assert_eq!(request.input.model, Fixture::anthropic_model());
    assert!(request.input.continuation.is_none());
    assert!(states(&request).is_empty());
    assert!(request.artifacts.references().is_empty());
    let submitted = fixture.anthropic.received();
    assert_eq!(submitted.len(), 1, "one Anthropic request");
    assert_eq!(submitted[0].model(), ANTHROPIC_MODEL);
    assert!(!submitted[0].has_block(&["thinking", "redacted_thinking"]));
    // No replayed state means no preserved-thinking controls at all.
    assert!(submitted[0].body.get("thinking").is_none());
    assert!(!submitted[0].header_block.contains("anthropic-beta"));
    // Nothing was sent back to OpenAI.
    assert_eq!(fixture.openai.received().len(), 1);
    fixture.handle.shutdown().await;
}

#[tokio::test]
async fn anthropic_to_openai_is_rejected_by_default_for_signed_thinking() {
    let fixture = Fixture::new().await;
    // Start the session on Anthropic by switching before any turn: nothing is
    // stored yet, so no optional state is lost.
    fixture
        .switch(Fixture::anthropic_model(), Portability::Reject)
        .await
        .expect("a clean switch");
    fixture.anthropic.reply(
        ANTHROPIC_MODEL,
        Reply::fixture(include_str!("../fixtures/anthropic/native_state.sse")),
    );
    let view = fixture.turn("hello").await;
    assert_eq!(last_response_text(&view), "pong");
    let before = fixture.sequence().await;
    let rejected = fixture
        .switch(Fixture::openai_model(), Portability::Reject)
        .await
        .expect_err("signed thinking is not portable");
    let Rejection::PortabilityLoss(preview) = &rejected else {
        panic!("expected portability_loss, got {rejected:?}")
    };
    assert_eq!(fixture.sequence().await, before);
    assert_eq!(preview.would_stop_being_portable.len(), 1);
    assert_eq!(
        preview.would_stop_being_portable[0].schema,
        anthropic::STATE_SCHEMA
    );
    assert_eq!(
        preview.would_stop_being_portable[0].part,
        Part::Block { index: 0 }
    );
    let accepted = fixture
        .switch(Fixture::openai_model(), Portability::OmitOptional)
        .await
        .expect("explicit omission");
    assert!(matches!(accepted, Accepted::ModelSwitched { .. }));

    fixture.openai.reply(
        OPENAI_MODEL,
        Reply::fixture(include_str!("../fixtures/openai/text.sse")),
    );
    let view = fixture.turn("next").await;
    assert_eq!(last_response_text(&view), "Hello world");
    let request = last(&fixture, OPENAI);
    assert!(request.input.continuation.is_none());
    assert!(states(&request).is_empty());
    assert!(request.artifacts.references().is_empty());
    let submitted = fixture.openai.received();
    assert_eq!(submitted.len(), 1);
    assert_eq!(submitted[0].model(), OPENAI_MODEL);
    // No replayed reasoning item and no encrypted payload anywhere.
    assert!(
        !submitted[0]
            .body
            .to_string()
            .contains("encrypted_content\":\"")
    );
    fixture.handle.shutdown().await;
}

// ------------------------------------------------------------- switch back

#[tokio::test]
async fn switching_back_replays_only_the_owning_providers_state_and_no_continuation() {
    let mut fixture = Fixture::new().await;
    fixture.openai.reply(
        OPENAI_MODEL,
        Reply::fixture(include_str!("../fixtures/openai/native_state.sse")),
    );
    fixture.turn("hello").await;
    fixture.reopen().await;
    fixture
        .switch(Fixture::anthropic_model(), Portability::OmitOptional)
        .await
        .expect("omit OpenAI reasoning");
    fixture.anthropic.reply(
        ANTHROPIC_MODEL,
        Reply::fixture(include_str!("../fixtures/anthropic/native_state.sse")),
    );
    fixture.turn("middle").await;
    fixture.reopen().await;

    // Back to OpenAI: Anthropic's signed block is lost, OpenAI's own encrypted
    // reasoning is replayed again, and no continuation is carried across.
    fixture
        .switch(Fixture::openai_model(), Portability::OmitOptional)
        .await
        .expect("omit Anthropic thinking");
    fixture.openai.reply(
        OPENAI_MODEL,
        Reply::fixture(include_str!("../fixtures/openai/text.sse")),
    );
    let view = fixture.turn("back").await;
    assert_eq!(last_response_text(&view), "Hello world");
    let request = last(&fixture, OPENAI);
    assert_eq!(request.input.model, Fixture::openai_model());
    assert!(request.input.continuation.is_none());
    let replayed = states(&request);
    assert_eq!(replayed.len(), 1, "only OpenAI's own reasoning item");
    assert_eq!(replayed[0].0, Fixture::openai_model());
    assert_eq!(replayed[0].1, openai::STATE_SCHEMA);
    let submitted = fixture.openai.received();
    let last_body = &submitted.last().expect("second OpenAI request").body;
    let reasoning = last_body["input"]
        .as_array()
        .expect("input")
        .iter()
        .find(|item| item["type"] == "reasoning")
        .expect("OpenAI replays its own encrypted reasoning");
    assert_eq!(
        reasoning["encrypted_content"],
        "gAAAA-fixture-encrypted-reasoning"
    );
    assert!(!last_body.to_string().contains("signature"));

    // And back to Anthropic: its own thinking block returns, OpenAI's does not.
    fixture
        .switch(Fixture::anthropic_model(), Portability::OmitOptional)
        .await
        .expect("omit OpenAI reasoning again");
    fixture.anthropic.reply(
        ANTHROPIC_MODEL,
        Reply::fixture(include_str!("../fixtures/anthropic/text.sse")),
    );
    fixture.turn("and back").await;
    let request = last(&fixture, ANTHROPIC);
    assert!(request.input.continuation.is_none());
    let replayed = states(&request);
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].0, Fixture::anthropic_model());
    assert_eq!(replayed[0].1, anthropic::STATE_SCHEMA);
    let bodies = fixture.anthropic.received();
    let last_body = &bodies.last().expect("second Anthropic request").body;
    assert!(last_body.to_string().contains("sig_fixture_thinking"));
    assert!(
        !last_body
            .to_string()
            .contains("gAAAA-fixture-encrypted-reasoning")
    );
    assert_eq!(
        last_body["thinking"]["block_binding"]["prefix_mismatch_behavior"], "drop_block",
        "a replayed block always carries the preserved-thinking control"
    );
    fixture.handle.shutdown().await;
}

// ---------------------------------------------------------------- compaction

#[tokio::test]
async fn a_summary_from_the_other_provider_is_plain_text_for_the_next_one() {
    let fixture = Fixture::new().await;
    fixture.openai.reply(
        OPENAI_MODEL,
        Reply::fixture(include_str!("../fixtures/openai/text.sse")),
    );
    fixture.turn("one").await;
    fixture
        .switch(Fixture::anthropic_model(), Portability::Reject)
        .await
        .expect("nothing to lose yet");
    fixture.anthropic.reply(
        ANTHROPIC_MODEL,
        Reply::fixture(include_str!("../fixtures/anthropic/native_state.sse")),
    );
    fixture.turn("two").await;
    fixture
        .switch(Fixture::openai_model(), Portability::OmitOptional)
        .await
        .expect("omit the signed block");
    fixture.openai.reply(
        OPENAI_MODEL,
        Reply::fixture(include_str!("../fixtures/openai/text.sse")),
    );
    fixture.turn("three").await;

    // Compaction runs on the independently configured summary model, which
    // belongs to the *other* provider instance: the router sends it there.
    fixture
        .anthropic
        .reply(SUMMARY_MODEL, Reply::summary(SUMMARY));
    let accepted = fixture
        .handle
        .command(Command::PreviewCompaction)
        .await
        .expect("preview");
    let Accepted::CompactionPreviewed { preview_id, .. } = accepted else {
        panic!("expected a preview, got {accepted:?}")
    };
    let summary_requests = fixture.recorded(ANTHROPIC);
    let summary_request = summary_requests
        .iter()
        .find(|request| request.input.model.model == SUMMARY_MODEL)
        .expect("the summary model was asked through the router");
    assert_eq!(
        summary_request.input.model.provider_instance, ANTHROPIC,
        "the summary model's own instance received the request"
    );
    assert!(
        fixture
            .anthropic
            .received()
            .iter()
            .any(|request| request.model() == SUMMARY_MODEL),
        "the summary generation reached the Anthropic endpoint"
    );
    let committed = fixture
        .handle
        .command(Command::CommitCompaction { preview_id })
        .await
        .expect("commit");
    assert!(matches!(committed, Accepted::CompactionCommitted { .. }));

    // The same summary is ordinary text for the other provider, and the
    // retained signed block still replays (now behind a compaction boundary).
    fixture
        .switch(Fixture::anthropic_model(), Portability::Reject)
        .await
        .expect("the covered reasoning is no longer projected");
    fixture.anthropic.reply(
        ANTHROPIC_MODEL,
        Reply::fixture(include_str!("../fixtures/anthropic/text.sse")),
    );
    let view = fixture.turn("four").await;
    assert!(last_response_text(&view).ends_with("Hello world"));
    let request = last(&fixture, ANTHROPIC);
    assert!(request.input.continuation.is_none());
    let summary_item = request
        .input
        .items
        .iter()
        .find_map(|item| match item {
            harness_core::context::ContextItem::Summary { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .expect("the summary is a projection item");
    assert_eq!(summary_item, SUMMARY);
    let submitted = fixture.anthropic.received();
    let last_body = &submitted.last().expect("post-compaction request").body;
    assert_eq!(
        last_body["messages"][0]["content"][0],
        json!({"type": "text",
            "text": format!("Summary of earlier conversation:\n\n{SUMMARY}")}),
        "the summary travels as plain text, not as provider state"
    );
    assert!(last_body.to_string().contains("sig_fixture_thinking"));
    // Preserved thinking: the retained block rides with the drop_block control
    // and the beta header that permits it.
    assert_eq!(
        last_body["thinking"]["block_binding"]["prefix_mismatch_behavior"],
        "drop_block"
    );
    assert!(
        submitted
            .last()
            .expect("request")
            .header_block
            .contains("anthropic-beta: thinking-binding-controls-2026-08-01"),
        "{:?}",
        submitted.last().expect("request").header_block
    );
    fixture.handle.shutdown().await;
}

// --------------------------------------------------- required incompatibility

#[tokio::test]
async fn a_text_only_target_is_refused_under_both_policies_across_providers() {
    let fixture = Fixture::new().await;
    let image = fixture
        .handle
        .put_artifact(b"\x89PNG-fixture".to_vec())
        .await
        .expect("artifact");
    fixture.openai.reply(
        OPENAI_MODEL,
        Reply::fixture(include_str!("../fixtures/openai/text.sse")),
    );
    fixture
        .start(Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "look".into(),
                },
                ContentBlock::Image {
                    artifact: image,
                    mime_type: "image/png".into(),
                },
            ],
        })
        .await;
    let before = fixture.sequence().await;
    for policy in [Portability::Reject, Portability::OmitOptional] {
        let rejected = fixture
            .switch(fixture.text_only.clone(), policy)
            .await
            .expect_err("a text-only model cannot take the image");
        let Rejection::PortabilityLoss(preview) = rejected else {
            panic!("expected portability_loss")
        };
        assert_eq!(fixture.sequence().await, before, "a refusal writes nothing");
        assert_eq!(preview.required_incompatibilities.len(), 1);
        assert_eq!(
            preview.required_incompatibilities[0].capability,
            RequiredCapability::Image
        );
        assert_eq!(
            preview.required_incompatibilities[0].part,
            Part::Block { index: 1 }
        );
        assert!(preview.would_stop_being_portable.is_empty());
    }
    fixture.handle.shutdown().await;
}

// --------------------------------------------------------------- tool calls

#[tokio::test]
async fn native_tool_call_ids_stay_paired_across_a_switch_in_both_directions() {
    let fixture = Fixture::new().await;
    // OpenAI proposes `read` with its own call id; the runtime executes it under
    // the session policy and continues the same turn.
    fixture.openai.reply(
        OPENAI_MODEL,
        Reply::fixture(include_str!("../fixtures/openai/one_tool.sse")),
    );
    fixture.openai.reply(
        OPENAI_MODEL,
        Reply::fixture(include_str!("../fixtures/openai/text.sse")),
    );
    fixture.turn("read notes.txt").await;
    assert_eq!(fixture.openai.received().len(), 2, "call then continuation");
    // The second request already pairs the result with OpenAI's own id.
    let second = &fixture.openai.received()[1].body;
    assert_eq!(second["input"][1]["type"], "function_call");
    assert_eq!(second["input"][2]["type"], "function_call_output");
    assert_eq!(second["input"][1]["call_id"], "call_fixture_1");
    assert_eq!(second["input"][2]["call_id"], "call_fixture_1");
    assert!(
        second["input"][2]["output"]
            .as_str()
            .expect("output")
            .contains("fixture notes")
    );

    // Switching to Anthropic keeps the call paired: the foreign id is sent
    // verbatim as `tool_use.id` and named again by `tool_result.tool_use_id`.
    fixture
        .switch(Fixture::anthropic_model(), Portability::Reject)
        .await
        .expect("no optional state yet");
    fixture.anthropic.reply(
        ANTHROPIC_MODEL,
        Reply::fixture(include_str!("../fixtures/anthropic/text.sse")),
    );
    fixture.turn("summarize").await;
    let body = &fixture.anthropic.received()[0].body;
    let blocks: Vec<&Value> = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .collect();
    let call = blocks
        .iter()
        .find(|block| block["type"] == "tool_use")
        .expect("tool_use");
    let result = blocks
        .iter()
        .find(|block| block["type"] == "tool_result")
        .expect("tool_result");
    assert_eq!(call["id"], "call_fixture_1");
    assert_eq!(result["tool_use_id"], "call_fixture_1");
    assert!(
        result["content"]
            .as_str()
            .expect("content")
            .contains("fixture notes")
    );

    // And the other direction: Anthropic's `toolu_…` id survives to OpenAI.
    fixture.anthropic.reply(
        ANTHROPIC_MODEL,
        Reply::fixture(include_str!("../fixtures/anthropic/one_tool.sse")),
    );
    fixture.anthropic.reply(
        ANTHROPIC_MODEL,
        Reply::fixture(include_str!("../fixtures/anthropic/text.sse")),
    );
    fixture.turn("read again").await;
    fixture
        .switch(Fixture::openai_model(), Portability::Reject)
        .await
        .expect("Anthropic state is absent here");
    fixture.openai.reply(
        OPENAI_MODEL,
        Reply::fixture(include_str!("../fixtures/openai/text.sse")),
    );
    fixture.turn("final").await;
    let received = fixture.openai.received();
    let body = &received.last().expect("openai request").body;
    let calls: Vec<&Value> = body["input"]
        .as_array()
        .expect("input")
        .iter()
        .filter(|item| item["type"] == "function_call")
        .collect();
    let outputs: Vec<&Value> = body["input"]
        .as_array()
        .expect("input")
        .iter()
        .filter(|item| item["type"] == "function_call_output")
        .collect();
    let foreign = calls
        .iter()
        .find(|call| call["call_id"] == "toolu_fixture_1")
        .expect("the Anthropic call id travelled verbatim");
    assert!(
        outputs
            .iter()
            .any(|output| output["call_id"] == foreign["call_id"]),
        "the result names the same id"
    );
    fixture.handle.shutdown().await;
}

// M01: an actual adapter has assembled a ready call, but the runtime has not
// received the terminal event. No proposal, intent or execution may be durable.
async fn terminal_gates_dispatch(anthropic: bool) {
    let fixture = Fixture::new().await;
    let (server, model, call, text) = if anthropic {
        fixture
            .switch(Fixture::anthropic_model(), Portability::Reject)
            .await
            .expect("switch");
        (
            &fixture.anthropic,
            ANTHROPIC_MODEL,
            include_str!("../fixtures/anthropic/one_tool.sse"),
            include_str!("../fixtures/anthropic/text.sse"),
        )
    } else {
        (
            &fixture.openai,
            OPENAI_MODEL,
            include_str!("../fixtures/openai/one_tool.sse"),
            include_str!("../fixtures/openai/text.sse"),
        )
    };
    server.reply(model, Reply::fixture(call));
    server.reply(model, Reply::fixture(text));
    fixture
        .terminal_gate
        .armed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let Accepted::TurnStarted { turn_id, .. } = fixture
        .handle
        .command(Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![ContentBlock::Text {
                text: "read notes.txt".into(),
            }],
        })
        .await
        .expect("start")
    else {
        panic!("not a turn")
    };
    tokio::time::timeout(
        Duration::from_secs(10),
        fixture.terminal_gate.reached.notified(),
    )
    .await
    .expect("adapter reached terminal");
    let view = fixture.handle.view().await.expect("view");
    assert_eq!(view.phase, Phase::Durable(RuntimeState::Inferring));
    assert!(view.pending_approval.is_none());
    assert!(
        !view.snapshot.entries().any(|(_, stored)| matches!(
            stored.entry.payload,
            EntryPayload::AssistantResponse(_) | EntryPayload::ToolResult(_)
        )),
        "no proposal or execution result before terminal success"
    );
    assert_eq!(server.received().len(), 1, "no dependent inference");
    fixture.terminal_gate.release.notify_one();
    let view = fixture.finish(turn_id).await;
    assert!(
        view.snapshot
            .entries()
            .any(|(_, stored)| matches!(&stored.entry.payload,
        EntryPayload::ToolResult(result) if result.result.status == ToolStatus::Succeeded)),
        "the valid call must really execute after terminal success"
    );
    fixture.handle.shutdown().await;
}

#[tokio::test]
async fn m01_openai_ready_call_waits_for_terminal_success() {
    terminal_gates_dispatch(false).await;
}
#[tokio::test]
async fn m01_anthropic_ready_call_waits_for_terminal_success() {
    terminal_gates_dispatch(true).await;
}

async fn cumulative_usage_replaces(anthropic: bool) {
    let fixture = Fixture::new().await;
    let (server, model, usage) = if anthropic {
        fixture
            .switch(Fixture::anthropic_model(), Portability::Reject)
            .await
            .expect("switch");
        (
            &fixture.anthropic,
            ANTHROPIC_MODEL,
            include_str!("../fixtures/anthropic/usage.sse"),
        )
    } else {
        (
            &fixture.openai,
            OPENAI_MODEL,
            include_str!("../fixtures/openai/usage.sse"),
        )
    };
    server.reply(model, Reply::fixture(usage));
    let view = fixture.turn("usage").await;
    let usage = view
        .snapshot
        .entries()
        .find_map(|(_, stored)| match &stored.entry.payload {
            EntryPayload::AssistantResponse(response) => response.usage.clone(),
            _ => None,
        })
        .expect("final usage");
    assert_eq!(usage.input_tokens, Some(12));
    assert_eq!(usage.output_tokens, Some(9), "snapshots replace, never sum");
    assert_eq!(usage.cached_input_tokens, Some(4));
    fixture.handle.shutdown().await;
}
#[tokio::test]
async fn m03_openai_cumulative_usage_is_not_summed() {
    cumulative_usage_replaces(false).await;
}
#[tokio::test]
async fn m03_anthropic_cumulative_usage_is_not_summed() {
    cumulative_usage_replaces(true).await;
}

async fn media_and_native_state_reopen(anthropic: bool) {
    let mut fixture = Fixture::new().await;
    let (model, native, text) = if anthropic {
        fixture
            .switch(Fixture::anthropic_model(), Portability::Reject)
            .await
            .expect("switch");
        (
            ANTHROPIC_MODEL,
            include_str!("../fixtures/anthropic/native_state.sse"),
            include_str!("../fixtures/anthropic/text.sse"),
        )
    } else {
        (
            OPENAI_MODEL,
            include_str!("../fixtures/openai/native_state.sse"),
            include_str!("../fixtures/openai/text.sse"),
        )
    };
    let bytes = b"\x89PNG-fixture";
    let image = fixture
        .handle
        .put_artifact(bytes.to_vec())
        .await
        .expect("image");
    let server = if anthropic {
        fixture.anthropic.clone()
    } else {
        fixture.openai.clone()
    };
    server.reply(model, Reply::fixture(native));
    fixture
        .start(Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "look".into(),
                },
                ContentBlock::Image {
                    artifact: image.clone(),
                    mime_type: "image/png".into(),
                },
            ],
        })
        .await;
    let before = fixture
        .handle
        .inspect_context(None, None)
        .await
        .expect("context");
    fixture.reopen().await;
    let after = fixture
        .handle
        .inspect_context(None, None)
        .await
        .expect("context");
    assert_eq!(before, after, "immutable context survives reopen");
    server.reply(model, Reply::fixture(text));
    fixture.turn("replay").await;
    let owner = if anthropic { ANTHROPIC } else { OPENAI };
    let request = last(&fixture, owner);
    assert_eq!(states(&request).len(), 1);
    assert_eq!(states(&request)[0].0, request.input.model);
    assert_eq!(
        request.artifacts.read(&image).await.expect("image bytes"),
        bytes
    );
    assert!(request.input.continuation.is_none());
    let submitted = server.received();
    let body = &submitted.last().expect("replayed request").body;
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    assert!(
        body.to_string().contains(&encoded),
        "actual native image round trip"
    );
    let expected = if anthropic {
        "sig_fixture_thinking"
    } else {
        "gAAAA-fixture-encrypted-reasoning"
    };
    assert!(
        body.to_string().contains(expected),
        "exact native state replayed after reopen"
    );
    // Switch after reopen: both policies are checked against retained state.
    let target = if anthropic {
        Fixture::openai_model()
    } else {
        Fixture::anthropic_model()
    };
    assert!(matches!(
        fixture.switch(target.clone(), Portability::Reject).await,
        Err(Rejection::PortabilityLoss(_))
    ));
    fixture
        .switch(target, Portability::OmitOptional)
        .await
        .expect("omit");
    let (other, other_model, other_text) = if anthropic {
        (
            &fixture.openai,
            OPENAI_MODEL,
            include_str!("../fixtures/openai/text.sse"),
        )
    } else {
        (
            &fixture.anthropic,
            ANTHROPIC_MODEL,
            include_str!("../fixtures/anthropic/text.sse"),
        )
    };
    other.reply(other_model, Reply::fixture(other_text));
    fixture.turn("foreign owner").await;
    let request = last(&fixture, if anthropic { OPENAI } else { ANTHROPIC });
    assert!(states(&request).is_empty());
    assert!(request.input.continuation.is_none());
    assert!(
        !other
            .received()
            .last()
            .expect("foreign request")
            .body
            .to_string()
            .contains(expected)
    );
    fixture.handle.shutdown().await;
}
#[tokio::test]
async fn m04_openai_multimodal_and_native_replay_survive_reopen() {
    media_and_native_state_reopen(false).await;
}
#[tokio::test]
async fn m04_anthropic_multimodal_and_native_replay_survive_reopen() {
    media_and_native_state_reopen(true).await;
}
