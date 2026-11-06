//! Offline adapter tests: hand-written fixtures, a loopback fake server, the
//! public conformance suite, request/replay goldens and event-mapping units.
//! No test reads a real credential or reaches the network.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use harness_core::{
    context::{ContextItem, Instruction, InstructionLevel},
    model::{
        ArtifactSource, InferenceOptions, ModelInput, ModelProvider, ModelRequest, ReasoningEffort,
        conformance::{
            Case, CaseKind, Expected, ExpectedTool, Fixture, NativeMetadata, Observation,
            SubmittedRequest, SubmittedTool, run,
        },
    },
    session::{
        ArtifactRef, CallId, CallRef, ContentBlock, Digest, EffectClass, EffectSet, EntryId,
        ExecutionBackend, FinishReason, ModelError, ModelErrorCode, ModelRef, Provenance,
        ResponseId, ToolMetadata, ToolResult, ToolStatus, Usage,
    },
    tools::ToolDefinition,
};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use super::{
    AnthropicMessages, Profile, STATE_SCHEMA,
    events::Mapper,
    request::{self, Reads},
};
use crate::{
    credentials::{Credential, CredentialSource},
    sse::SseEvent,
    transport::{Transport, TransportConfig},
};

#[allow(dead_code, clippy::duplicate_mod)]
#[path = "../../../../tests/support/loopback.rs"]
mod loopback;

fn client_nonce() -> &'static str {
    static NONCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NONCE.get_or_init(loopback::nonce)
}

const SENTINEL_SECRET: &str = "SENTINEL-ANTHROPIC-KEY-TOKEN";

// ---------------------------------------------------------------- fake server

/// One canned HTTP response. `silent` sends no headers; `hold` keeps the
/// connection open until the client closes it (cancellation and drop tests).
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    silent: bool,
    hold: bool,
}

impl Reply {
    fn sse(text: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: text.as_bytes().to_vec(),
            hold: false,
            silent: false,
        }
    }

    fn holding(text: &str) -> Self {
        Self {
            hold: true,
            ..Self::sse(text)
        }
    }

    fn silent() -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: Vec::new(),
            silent: true,
            hold: true,
        }
    }
}

/// An attributed loopback server: unrelated probes never affect client state.
struct Server {
    url: String,
    submitted: Arc<Mutex<Option<Value>>>,
    headers: Arc<Mutex<Option<String>>>,
    released: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
    ignored: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
    _side_probe: loopback::SideProbe,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    fn start(reply: Reply) -> Self {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind fixture");
        listener.set_nonblocking(true).expect("nonblocking");
        let address = listener.local_addr().expect("addr");
        let side_probe = loopback::SideProbe::register(address);
        let path = format!("/v1/messages/{}", loopback::nonce());
        let url = format!("http://{address}{path}");
        let submitted = Arc::new(Mutex::new(None));
        let headers = Arc::new(Mutex::new(None));
        let released = Arc::new(AtomicBool::new(true));
        let connections = Arc::new(AtomicUsize::new(0));
        let ignored = Arc::new(AtomicUsize::new(0));
        let (record, recorded_headers, closed, count, unrelated) = (
            submitted.clone(),
            headers.clone(),
            released.clone(),
            connections.clone(),
            ignored.clone(),
        );
        let task = tokio::spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
            let mut handlers = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = tokio::select! {
                    accepted = listener.accept() => accepted.expect("accept"),
                    Some(result) = handlers.join_next(), if !handlers.is_empty() => {
                        result.expect("connection handler");
                        continue;
                    }
                };
                let (record, recorded_headers, closed, count, unrelated) = (
                    record.clone(),
                    recorded_headers.clone(),
                    closed.clone(),
                    count.clone(),
                    unrelated.clone(),
                );
                let path = path.clone();
                let reply = reply.clone();
                handlers.spawn(async move {
                    let body = tokio::time::timeout(
                        std::time::Duration::from_secs(1),
                        read_request(&mut socket, &path),
                    )
                    .await
                    .ok()
                    .flatten();
                    let Some((head, body)) = body else {
                        let ignored = unrelated.fetch_add(1, Ordering::SeqCst) + 1;
                        eprintln!("anthropic fixture: ignored unrelated connection #{ignored}");
                        return;
                    };
                    count.fetch_add(1, Ordering::SeqCst);
                    closed.store(false, Ordering::SeqCst);
                    *record.lock().expect("record") = Some(body);
                    *recorded_headers.lock().expect("headers") = Some(head);
                    if !reply.silent {
                        let mut response =
                            format!("HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
                        for (name, value) in &reply.headers {
                            response.push_str(&format!("{name}: {value}\r\n"));
                        }
                        // A held response stays open mid-body, so it is chunked
                        // and never terminated.
                        if reply.hold {
                            response.push_str("transfer-encoding: chunked\r\n\r\n");
                            let _ = socket.write_all(response.as_bytes()).await;
                            let chunk = format!("{:x}\r\n", reply.body.len());
                            let _ = socket.write_all(chunk.as_bytes()).await;
                            let _ = socket.write_all(&reply.body).await;
                            let _ = socket.write_all(b"\r\n").await;
                        } else {
                            response
                                .push_str(&format!("content-length: {}\r\n\r\n", reply.body.len()));
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.write_all(&reply.body).await;
                        }
                        let _ = socket.flush().await;
                    }
                    if reply.hold {
                        let mut buffer = [0u8; 1024];
                        loop {
                            match socket.read(&mut buffer).await {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                    }
                    let _ = socket.shutdown().await;
                    closed.store(true, Ordering::SeqCst);
                });
            }
        });
        Self {
            url,
            submitted,
            headers,
            released,
            connections,
            ignored,
            task,
            _side_probe: side_probe,
        }
    }

    fn observation(&self) -> Observation {
        Observation {
            submitted: self
                .submitted
                .lock()
                .expect("record")
                .as_ref()
                .map(decode_request),
            released: self.released.load(Ordering::SeqCst),
        }
    }

    fn header_block(&self) -> String {
        self.headers
            .lock()
            .expect("headers")
            .clone()
            .unwrap_or_default()
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        529 => "Overloaded",
        _ => "Internal Server Error",
    }
}

/// Read one attributed request, returning its lowercased header block and body.
async fn read_request(socket: &mut TcpStream, path: &str) -> Option<(String, Value)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut head_end = None;
    let mut length = 0usize;
    loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 || buffer.len() + read > 1024 * 1024 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if head_end.is_none()
            && let Some(position) = find(&buffer, b"\r\n\r\n")
        {
            head_end = Some(position + 4);
            let raw_head = String::from_utf8_lossy(&buffer[..position]).to_string();
            let head = raw_head.to_ascii_lowercase();
            if raw_head.lines().next() != Some(format!("POST {path} HTTP/1.1").as_str())
                || !head
                    .lines()
                    .any(|line| line == format!("x-harness-test: {}", client_nonce()))
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
                .map(|body| (head, body));
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ------------------------------------------------------------- fixture files

fn fixture_text(name: &str) -> &'static str {
    match name {
        "text" => include_str!("../../fixtures/anthropic/text.sse"),
        "request" => include_str!("../../fixtures/anthropic/request.sse"),
        "one_tool" => include_str!("../../fixtures/anthropic/one_tool.sse"),
        "interleaved_tools" => include_str!("../../fixtures/anthropic/interleaved_tools.sse"),
        "malformed_tool_json" => {
            include_str!("../../fixtures/anthropic/malformed_tool_json.sse")
        }
        "refusal" => include_str!("../../fixtures/anthropic/refusal.sse"),
        "output_limit" => include_str!("../../fixtures/anthropic/output_limit.sse"),
        "usage" => include_str!("../../fixtures/anthropic/usage.sse"),
        "native_state" => include_str!("../../fixtures/anthropic/native_state.sse"),
        "provider_error" => include_str!("../../fixtures/anthropic/provider_error.http"),
        "rate_limit" => include_str!("../../fixtures/anthropic/rate_limit.http"),
        "setup_failure" => include_str!("../../fixtures/anthropic/setup_failure.http"),
        other => panic!("unknown fixture {other}"),
    }
}

fn parse_fixture(text: &str) -> Reply {
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
    Reply {
        status,
        headers,
        body: body.as_bytes().to_vec(),
        silent: false,
        hold: false,
    }
}

/// Body for the streaming fixtures that must stay open while cancelling.
fn hold_stream() -> String {
    let mut text = String::new();
    for block in fixture_text("text").split("\n\n").take(2) {
        text.push_str(block);
        text.push_str("\n\n");
    }
    text
}

// -------------------------------------------------------------- credentials

struct Fixed(Credential);

impl CredentialSource for Fixed {
    fn credential(&self) -> Result<Credential, ModelError> {
        Ok(self.0.clone())
    }
}

struct Missing;

impl CredentialSource for Missing {
    fn credential(&self) -> Result<Credential, ModelError> {
        Err(ModelError {
            code: ModelErrorCode::Authentication,
            message: "missing or empty credential variable ANTHROPIC_API_KEY".into(),
            provider_code: None,
            retry_after_ms: None,
        })
    }
}

fn fixed(secret: &str) -> Fixed {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "x-harness-test",
        client_nonce().parse().expect("run header"),
    );
    Fixed(Credential::api_key(secret.to_owned(), headers, None).expect("credential"))
}

fn transport() -> Transport {
    Transport::new(TransportConfig {
        allow_loopback_http: true,
        ..TransportConfig::default()
    })
    .expect("transport")
}

fn adapter(url: String) -> AnthropicMessages {
    AnthropicMessages::with_transport(Arc::new(fixed(SENTINEL_SECRET)), transport())
        .with_endpoint(url)
}

// ------------------------------------------------------- request decoding

/// Decode the submitted wire request back into normalized evidence.
fn decode_request(body: &Value) -> SubmittedRequest {
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|tool| SubmittedTool {
                    name: tool["name"].as_str().unwrap_or_default().to_owned(),
                    description: tool["description"].as_str().unwrap_or_default().to_owned(),
                    input_schema: tool.get("input_schema").cloned().unwrap_or(Value::Null),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut anthropic = Map::new();
    if let Some(tier) = body.get("service_tier").and_then(Value::as_str) {
        anthropic.insert("service_tier".into(), json!(tier));
    }
    let mut provider_options = Map::new();
    if !anthropic.is_empty() {
        provider_options.insert("anthropic".into(), Value::Object(anthropic));
    }
    SubmittedRequest {
        tools,
        options: InferenceOptions {
            max_output_tokens: body
                .get("max_tokens")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0),
            reasoning_effort: body
                .pointer("/output_config/effort")
                .and_then(Value::as_str)
                .map(|effort| match effort {
                    "low" => ReasoningEffort::Low,
                    "medium" => ReasoningEffort::Medium,
                    _ => ReasoningEffort::High,
                }),
            provider_options,
        },
    }
}

// --------------------------------------------------------------- manifests

fn model() -> ModelRef {
    ModelRef {
        provider_instance: "anthropic".into(),
        model: "claude-sonnet-5-5".into(),
    }
}

fn tool(name: &str, description: &str, schema: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
        effects: EffectSet::new([EffectClass::Read]).expect("effects"),
        backend: ExecutionBackend::TrustedFs,
    }
}

fn tools() -> Vec<ToolDefinition> {
    vec![
        tool(
            "read",
            "Read a file from the workspace.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "offset_bytes": {"type": "integer"}},
                "required": ["path"],
                "additionalProperties": false,
            }),
        ),
        tool(
            "bash",
            "Run one command.",
            json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
                "additionalProperties": false,
            }),
        ),
    ]
}

fn user(text: &str) -> ContextItem {
    ContextItem::User {
        entry_id: EntryId::new(),
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

fn options(max_output_tokens: u32) -> InferenceOptions {
    InferenceOptions {
        max_output_tokens,
        reasoning_effort: None,
        provider_options: Map::new(),
    }
}

/// One manifest row for the Anthropic profile.
fn case(kind: CaseKind) -> Case {
    case_with(kind, model(), Profile::Anthropic)
}

/// One manifest row for `model` on `profile`.
///
/// Both profiles share the Messages wire format, so the manifest is built once
/// and the two profiles differ only in the model, the state schema, the
/// thinking block the fixture stores and one diagnostic. What each profile
/// accepts of those bytes is what the tests prove.
fn case_with(kind: CaseKind, model: ModelRef, profile: Profile) -> Case {
    let mut expected = Expected::default();
    let mut request = ModelRequest {
        response_id: ResponseId::new(),
        artifacts: ArtifactSource::default(),
        input: ModelInput {
            model: model.clone(),
            items: vec![user("Hello")],
            tools: Vec::new(),
            options: options(256),
            continuation: None,
        },
    };
    let fixture = match kind {
        CaseKind::Text => {
            expected.text.insert(0, "Hello world".into());
            "text"
        }
        CaseKind::OneTool => {
            expected.finish_reason = Some(FinishReason::ToolCalls);
            expected.tools.insert(
                0,
                ExpectedTool {
                    name: "read".into(),
                    provider_call_id: Some("toolu_fixture_1".into()),
                    arguments: json!({"path": "notes.txt"}),
                },
            );
            "one_tool"
        }
        CaseKind::InterleavedTools => {
            expected.finish_reason = Some(FinishReason::ToolCalls);
            expected.tools.insert(
                0,
                ExpectedTool {
                    name: "read".into(),
                    provider_call_id: Some("toolu_fixture_a".into()),
                    arguments: json!({"command": "echo a"}),
                },
            );
            expected.tools.insert(
                1,
                ExpectedTool {
                    name: "bash".into(),
                    provider_call_id: Some("toolu_fixture_b".into()),
                    arguments: json!({"command": "echo b"}),
                },
            );
            "interleaved_tools"
        }
        CaseKind::MalformedToolJson => {
            expected.outcome = harness_core::model::ProviderOutcome::Failed;
            expected.finish_reason = None;
            expected.error = Some(ModelError {
                code: ModelErrorCode::MalformedStream,
                message: "tool-call arguments are not one JSON object".into(),
                provider_code: None,
                retry_after_ms: None,
            });
            "malformed_tool_json"
        }
        CaseKind::Refusal => {
            expected.finish_reason = Some(FinishReason::Refusal);
            expected.text.insert(0, "I can't help with that.".into());
            "refusal"
        }
        CaseKind::OutputLimit => {
            expected.finish_reason = Some(FinishReason::OutputLimit);
            expected.text.insert(0, "Hello, I can".into());
            "output_limit"
        }
        CaseKind::ProviderError => {
            expected.outcome = harness_core::model::ProviderOutcome::Failed;
            expected.finish_reason = None;
            expected.error = Some(ModelError {
                code: ModelErrorCode::ProviderFailure,
                message: "the provider failed the response".into(),
                provider_code: Some("overloaded_error".into()),
                retry_after_ms: Some(250),
            });
            "provider_error"
        }
        CaseKind::RateLimit => {
            expected.outcome = harness_core::model::ProviderOutcome::Failed;
            expected.finish_reason = None;
            expected.error = Some(ModelError {
                code: ModelErrorCode::RateLimit,
                message: "provider rate limit reached; start a new turn".into(),
                provider_code: Some("rate_limit_error".into()),
                retry_after_ms: Some(2000),
            });
            "rate_limit"
        }
        CaseKind::SetupFailure => {
            expected.outcome = harness_core::model::ProviderOutcome::Failed;
            expected.finish_reason = None;
            expected.error = Some(ModelError {
                code: ModelErrorCode::Authentication,
                message: "provider authentication failed".into(),
                provider_code: Some("authentication_error".into()),
                retry_after_ms: None,
            });
            "setup_failure"
        }
        CaseKind::Usage => {
            expected.usage = vec![
                Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(1),
                    cached_input_tokens: Some(4),
                    reasoning_tokens: None,
                },
                Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(9),
                    cached_input_tokens: Some(4),
                    reasoning_tokens: None,
                },
            ];
            expected.text.insert(0, "Hi".into());
            "usage"
        }
        CaseKind::NativeState => {
            expected.native.insert(
                0,
                NativeMetadata {
                    schema: profile.state_schema().into(),
                    bytes: thinking_payload_bytes(profile),
                },
            );
            expected.text.insert(1, "pong".into());
            request.input.items = vec![
                user("Hello"),
                ContextItem::Assistant {
                    entry_id: EntryId::new(),
                    outcome: harness_core::session::ResponseOutcome::Completed,
                    content: vec![ContentBlock::ReasoningSummary {
                        text: "Earlier summary".into(),
                    }],
                },
            ];
            request.input.options.reasoning_effort = Some(ReasoningEffort::Medium);
            "native_state"
        }
        CaseKind::CancelSetup => "cancel_setup",
        CaseKind::CancelStreaming => "cancel_streaming",
        CaseKind::Drop => "drop",
        CaseKind::Deadline => "deadline",
        CaseKind::Request => {
            expected.text.insert(0, "Hello world".into());
            request.input.tools = tools();
            request.input.options = InferenceOptions {
                max_output_tokens: 321,
                reasoning_effort: Some(ReasoningEffort::High),
                provider_options: serde_json::from_value(json!({
                    "anthropic": {"service_tier": "auto"}
                }))
                .expect("options"),
            };
            "request"
        }
        CaseKind::UnsupportedOption => {
            expected.outcome = harness_core::model::ProviderOutcome::Failed;
            expected.finish_reason = None;
            expected.error = Some(ModelError {
                code: ModelErrorCode::UnsupportedCapability,
                message: format!(
                    "provider option namespace openai is not supported by the {} adapter",
                    profile.label()
                ),
                provider_code: None,
                retry_after_ms: None,
            });
            request.input.options.provider_options =
                serde_json::from_value(json!({"openai": {"reasoning_summary": "auto"}}))
                    .expect("options");
            "unsupported_option"
        }
    };
    Case {
        kind,
        fixture: fixture.into(),
        request,
        expected,
        unsupported_option: (kind == CaseKind::UnsupportedOption).then(|| "openai".into()),
    }
}

/// Exact bytes the mapper stores for the `native_state` thinking block of
/// `profile`: the same thought, with a signature only where the profile
/// documents one.
fn thinking_payload_bytes(profile: Profile) -> usize {
    let mut block = json!({
        "type": "thinking",
        "thinking": "Considering the greeting.",
    });
    if profile.requires_thinking_signature() {
        block["signature"] = json!("sig_fixture_thinking");
    }
    serde_json::to_vec(&block).expect("payload").len()
}

fn reply_for(fixture: &str) -> Reply {
    match fixture {
        "cancel_setup" | "deadline" | "unsupported_option" => Reply::silent(),
        "cancel_streaming" | "drop" => Reply::holding(&hold_stream()),
        other => parse_fixture(fixture_text(other)),
    }
}

fn factory() -> impl Fn(&str) -> Fixture {
    move |fixture: &str| {
        let server = Arc::new(Server::start(reply_for(fixture)));
        let url = server.url.clone();
        let observed = Arc::clone(&server);
        Fixture {
            provider: Arc::new(adapter(url)),
            observe: Arc::new(move || observed.observation()),
        }
    }
}

// ------------------------------------------------------------- conformance

#[tokio::test]
async fn every_conformance_case_passes_on_native_fixtures() {
    let manifest: Vec<Case> = CaseKind::ALL.into_iter().map(case).collect();
    run(&factory(), &manifest).await.expect("conformance");
}

// ------------------------------------------------------------ request golden

fn full_input() -> ModelInput {
    let call_id = CallId::new();
    ModelInput {
        model: model(),
        items: vec![
            ContextItem::Instructions {
                instructions: vec![
                    Instruction {
                        level: InstructionLevel::System,
                        text: "You are the harness.".into(),
                    },
                    Instruction {
                        level: InstructionLevel::Developer,
                        text: "Prefer small diffs.".into(),
                    },
                ],
            },
            user("Read notes.txt"),
            ContextItem::Assistant {
                entry_id: EntryId::new(),
                outcome: harness_core::session::ResponseOutcome::Completed,
                content: vec![
                    ContentBlock::ReasoningSummary {
                        text: "display only".into(),
                    },
                    ContentBlock::Text {
                        text: "Reading it.".into(),
                    },
                    ContentBlock::ToolCall {
                        call_id: call_id.clone(),
                        name: "read".into(),
                        arguments: serde_json::from_value(json!({"path": "notes.txt"}))
                            .expect("args"),
                        provider_call_id: Some("toolu_fixture_1".into()),
                    },
                ],
            },
            ContextItem::ToolResult {
                entry_id: EntryId::new(),
                provenance: Provenance::Executor,
                result: ToolResult {
                    call: CallRef {
                        response_entry_id: EntryId::new(),
                        call_id,
                    },
                    invocation_id: None,
                    status: ToolStatus::Succeeded,
                    content: vec![ContentBlock::Text {
                        text: "file contents".into(),
                    }],
                    metadata: ToolMetadata {
                        exit_code: Some(0),
                        duration_ms: Some(3),
                        output_truncated: false,
                        provenance: Provenance::Executor,
                        artifacts: Vec::new(),
                        code: None,
                        file_effect: None,
                        file_paths: Vec::new(),
                    },
                },
            },
            ContextItem::Summary {
                entry_id: EntryId::new(),
                summary: "Earlier.".into(),
            },
        ],
        tools: tools(),
        options: InferenceOptions {
            max_output_tokens: 512,
            reasoning_effort: Some(ReasoningEffort::Medium),
            provider_options: serde_json::from_value(json!({
                "anthropic": {"service_tier": "standard_only"}
            }))
            .expect("options"),
        },
        continuation: None,
    }
}

#[test]
fn request_body_is_one_correct_messages_body() {
    let body = request::build(Profile::Anthropic, &full_input(), &Reads::default()).expect("build");
    let expected = json!({
        "model": "claude-sonnet-5-5",
        "max_tokens": 512,
        "stream": true,
        "system": "You are the harness.\n\nPrefer small diffs.",
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "Read notes.txt"}]},
            {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Reading it."},
                    {"type": "tool_use", "id": "toolu_fixture_1", "name": "read", "input": {"path": "notes.txt"}},
                ],
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_fixture_1",
                        "content": "status: succeeded\nprovenance: executor\nexit_code: 0\noutput_truncated: false\n\nfile contents",
                        "is_error": false,
                    },
                    // Adjacent same-role items merge: the summary joins the
                    // tool-result turn, so roles strictly alternate.
                    {"type": "text", "text": "Summary of earlier conversation:\n\nEarlier."},
                ],
            },
        ],
        "tools": [
            {
                "name": "read",
                "description": "Read a file from the workspace.",
                "input_schema": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "offset_bytes": {"type": "integer"}},
                    "required": ["path"],
                    "additionalProperties": false,
                },
            },
            {
                "name": "bash",
                "description": "Run one command.",
                "input_schema": {
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"],
                    "additionalProperties": false,
                },
            },
        ],
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "medium"},
        "service_tier": "standard_only",
    });
    assert_eq!(body, expected);
}

#[test]
fn adjacent_same_role_items_merge_and_roles_alternate() {
    let mut input = full_input();
    input.items.push(user("one more"));
    input.items.push(user("and another"));
    let body = request::build(Profile::Anthropic, &input, &Reads::default()).expect("build");
    let messages = body["messages"].as_array().expect("messages");
    for pair in messages.windows(2) {
        assert_ne!(pair[0]["role"], pair[1]["role"], "roles must alternate");
    }
    let last = messages.last().expect("last");
    assert_eq!(last["role"], json!("user"));
    let texts: Vec<&str> = last["content"]
        .as_array()
        .expect("content")
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    assert!(texts.contains(&"one more"));
    assert!(texts.contains(&"and another"));
}

#[test]
fn image_input_needs_a_sourced_capability_and_serializes_as_base64() {
    let bytes = b"\x89PNG-fixture";
    let reference = ArtifactRef {
        sha256: Digest::of(bytes),
        bytes: bytes.len() as u64,
    };
    let input = ModelInput {
        items: vec![ContextItem::User {
            entry_id: EntryId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "What is this?".into(),
                },
                ContentBlock::Image {
                    artifact: reference.clone(),
                    mime_type: "image/png".into(),
                },
            ],
        }],
        ..full_input()
    };
    let reads = Reads::from_bytes(reference, bytes.to_vec());
    let body = request::build(Profile::Anthropic, &input, &reads).expect("build");
    let image = &body["messages"][0]["content"][1];
    assert_eq!(image["type"], json!("image"));
    assert_eq!(image["source"]["type"], json!("base64"));
    assert_eq!(image["source"]["media_type"], json!("image/png"));
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    assert_eq!(
        STANDARD
            .decode(image["source"]["data"].as_str().expect("data"))
            .expect("base64"),
        bytes
    );

    let mut unknown = input.clone();
    unknown.model.model = "claude-unknown".into();
    unknown.options.reasoning_effort = None;
    unknown.options.provider_options.clear();
    let error = request::build(Profile::Anthropic, &unknown, &reads).expect_err("capability");
    assert_eq!(error.code, ModelErrorCode::UnsupportedCapability);
    assert!(error.message.contains("image"), "{}", error.message);
}

#[test]
fn provider_options_are_validated_by_name() {
    let mut input = full_input();
    input.options.provider_options =
        serde_json::from_value(json!({"anthropic": {"nonsense": 1}})).expect("options");
    let error = request::build(Profile::Anthropic, &input, &Reads::default()).expect_err("unknown");
    assert_eq!(error.code, ModelErrorCode::InvalidRequest);
    assert!(error.message.contains("nonsense"));

    input.options.provider_options =
        serde_json::from_value(json!({"openai": {"reasoning_summary": "auto"}})).expect("options");
    let error = request::build(Profile::Anthropic, &input, &Reads::default()).expect_err("foreign");
    assert_eq!(error.code, ModelErrorCode::UnsupportedCapability);
    assert!(error.message.contains("openai"));

    input.options.provider_options =
        serde_json::from_value(json!({"anthropic": {"service_tier": "priority"}}))
            .expect("options");
    let error = request::build(Profile::Anthropic, &input, &Reads::default()).expect_err("tier");
    assert_eq!(error.code, ModelErrorCode::InvalidRequest);
    assert!(error.message.contains("service_tier"));
}

// ------------------------------------------------------------- replay golden

fn state_input(payload: &[u8], extra: Option<(&[u8], ModelRef)>) -> (ModelInput, Reads) {
    let reference = ArtifactRef {
        sha256: Digest::of(payload),
        bytes: payload.len() as u64,
    };
    let mut content = vec![ContentBlock::ProviderState {
        owner: model(),
        schema: STATE_SCHEMA.into(),
        artifact: reference.clone(),
    }];
    content.push(ContentBlock::Text {
        text: "answer".into(),
    });
    let extra = extra.map(|(bytes, owner)| {
        let clone = ArtifactRef {
            sha256: Digest::of(bytes),
            bytes: bytes.len() as u64,
        };
        content.push(ContentBlock::ProviderState {
            owner,
            schema: STATE_SCHEMA.into(),
            artifact: clone.clone(),
        });
        (clone, bytes)
    });
    let mut reads = Reads::from_bytes(reference, payload.to_vec());
    if let Some((clone, bytes)) = &extra {
        reads = reads.with(clone.clone(), (*bytes).to_vec());
    }
    (
        ModelInput {
            model: model(),
            items: vec![
                user("Hello"),
                ContextItem::Assistant {
                    entry_id: EntryId::new(),
                    outcome: harness_core::session::ResponseOutcome::Completed,
                    content,
                },
            ],
            tools: Vec::new(),
            options: options(256),
            continuation: None,
        },
        reads,
    )
}

#[test]
fn thinking_and_redacted_blocks_replay_verbatim_in_order() {
    let thinking = br#"{"type":"thinking","thinking":"step one","signature":"sig_fixture_replay"}"#;
    let redacted = br#"{"type":"redacted_thinking","data":"opaque-redacted-fixture"}"#;
    let (input, reads) = state_input(thinking, Some((redacted, model())));
    let body = request::build(Profile::Anthropic, &input, &reads).expect("replay");
    let content = body["messages"][1]["content"].as_array().expect("content");
    assert_eq!(
        content[0],
        json!({"type": "thinking", "thinking": "step one", "signature": "sig_fixture_replay"})
    );
    assert_eq!(content[1], json!({"type": "text", "text": "answer"}));
    assert_eq!(
        content[2],
        json!({"type": "redacted_thinking", "data": "opaque-redacted-fixture"})
    );

    // A foreign owner is skipped, never recovered.
    let (foreign_input, foreign_reads) = state_input(thinking, None);
    let mut foreign = foreign_input;
    if let ContextItem::Assistant { content, .. } = &mut foreign.items[1] {
        content[0] = ContentBlock::ProviderState {
            owner: ModelRef {
                provider_instance: "other".into(),
                model: "other".into(),
            },
            schema: STATE_SCHEMA.into(),
            artifact: ArtifactRef {
                sha256: Digest::of(thinking),
                bytes: thinking.len() as u64,
            },
        };
    }
    let body = request::build(Profile::Anthropic, &foreign, &foreign_reads).expect("skip");
    assert_eq!(
        body["messages"][1]["content"],
        json!([{"type": "text", "text": "answer"}])
    );
}

// ----------------------------------------------- foreign native call ids

/// One tool call and its result, with the native id the stored turn carries.
fn tool_exchange(provider_call_id: Option<&str>) -> (ModelInput, String) {
    let call_id = CallId::new();
    let harness = call_id.as_str().to_owned();
    let input = ModelInput {
        model: model(),
        items: vec![
            user("Read notes.txt"),
            ContextItem::Assistant {
                entry_id: EntryId::new(),
                outcome: harness_core::session::ResponseOutcome::Completed,
                content: vec![ContentBlock::ToolCall {
                    call_id: call_id.clone(),
                    name: "read".into(),
                    arguments: serde_json::from_value(json!({"path": "notes.txt"})).expect("args"),
                    provider_call_id: provider_call_id.map(str::to_owned),
                }],
            },
            ContextItem::ToolResult {
                entry_id: EntryId::new(),
                provenance: Provenance::Executor,
                result: ToolResult {
                    call: CallRef {
                        response_entry_id: EntryId::new(),
                        call_id,
                    },
                    invocation_id: None,
                    status: ToolStatus::Succeeded,
                    content: vec![ContentBlock::Text {
                        text: "file contents".into(),
                    }],
                    metadata: ToolMetadata {
                        exit_code: Some(0),
                        duration_ms: Some(3),
                        output_truncated: false,
                        provenance: Provenance::Executor,
                        artifacts: Vec::new(),
                        code: None,
                        file_effect: None,
                        file_paths: Vec::new(),
                    },
                },
            },
        ],
        tools: tools(),
        options: options(64),
        continuation: None,
    };
    (input, harness)
}

/// A tool call issued by the *other* provider: its id is the stored identity, so
/// it is replayed verbatim when Anthropic can take it, and replaced by one
/// deterministic, plain identifier when it cannot. Either way the `tool_use` and
/// its `tool_result` name the same id, on this request and on every later one,
/// which is what keeps a call paired across a switch without editing the
/// preserved-thinking prefix.
#[test]
fn a_foreign_call_id_stays_paired_across_a_provider_switch() {
    let (input, _) = tool_exchange(Some("call_fixture_1"));
    let body = request::build(Profile::Anthropic, &input, &Reads::default()).expect("foreign id");
    assert_eq!(body["messages"][1]["content"][0]["id"], "call_fixture_1");
    assert_eq!(
        body["messages"][2]["content"][0]["tool_use_id"],
        "call_fixture_1"
    );
    let (input, _) = tool_exchange(Some("toolu_fixture_1"));
    let body = request::build(Profile::Anthropic, &input, &Reads::default()).expect("own id");
    assert_eq!(body["messages"][1]["content"][0]["id"], "toolu_fixture_1");

    for foreign in [
        "call.1",
        "call:1",
        "",
        "id with space",
        "call/naïve",
        "x".repeat(65).as_str(),
    ] {
        let (input, harness) = tool_exchange(Some(foreign));
        let body =
            request::build(Profile::Anthropic, &input, &Reads::default()).expect("substituted id");
        let id = body["messages"][1]["content"][0]["id"]
            .as_str()
            .expect("id")
            .to_owned();
        assert!(
            !id.is_empty()
                && id.len() <= 64
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "unusable id {id:?} from {foreign:?}"
        );
        assert_eq!(
            body["messages"][2]["content"][0]["tool_use_id"],
            Value::String(id.clone())
        );
        // Derived from the harness call id, not from a counter or the request.
        assert_eq!(id, harness);
        assert_eq!(
            request::build(Profile::Anthropic, &input, &Reads::default()).expect("replay"),
            body
        );
    }
}

// --------------------------------------------------- preserved thinking

fn drop_block() -> Value {
    json!({"prefix_mismatch_behavior": "drop_block"})
}

/// Situation one: an untouched, append-only history replays one signed block.
/// Nothing has been edited, so the block passes the prefix check; the control
/// is still requested, because the same body must stay safe once the prefix
/// does change (the alternative is a 400 on enforcing accounts).
#[test]
fn preserved_thinking_is_requested_on_an_untouched_replayed_history() {
    let payload = br#"{"type":"thinking","thinking":"step one","signature":"sig_fixture_replay"}"#;
    let (input, reads) = state_input(payload, None);
    let body = request::build(Profile::Anthropic, &input, &reads).expect("replay");
    assert_eq!(
        body,
        json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": 256,
            "stream": true,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Hello"}]},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "step one", "signature": "sig_fixture_replay"},
                        {"type": "text", "text": "answer"},
                    ],
                },
            ],
            "thinking": {"type": "adaptive", "block_binding": drop_block()},
        })
    );
    assert!(request::replays_thinking(&body));
}

/// Situation two: a projection the harness already compacted and switched
/// through. The summary replaced the turns before the retained assistant turn,
/// so that turn's signed block can never pass the prefix check; a foreign
/// (switched-away) state block earlier in the projection is omitted as always.
#[test]
fn preserved_thinking_is_requested_behind_a_summary_and_a_switch() {
    let payload =
        br#"{"type":"thinking","thinking":"kept reasoning","signature":"sig_after_summary"}"#;
    let (mut input, reads) = state_input(payload, None);
    let foreign = br#"{"type":"thinking","thinking":"other account","signature":"sig_foreign"}"#;
    let foreign_ref = ArtifactRef {
        sha256: Digest::of(foreign),
        bytes: foreign.len() as u64,
    };
    input.items = vec![
        ContextItem::Instructions {
            instructions: vec![Instruction {
                level: InstructionLevel::System,
                text: "You are the harness.".into(),
            }],
        },
        // Compaction replaced every earlier turn with this summary.
        ContextItem::Summary {
            entry_id: EntryId::new(),
            summary: "Turns one to three.".into(),
        },
        user("Keep going"),
        // A turn produced before a provider switch; its state belongs to the
        // other provider instance and is never replayed here.
        ContextItem::Assistant {
            entry_id: EntryId::new(),
            outcome: harness_core::session::ResponseOutcome::Completed,
            content: vec![
                ContentBlock::ProviderState {
                    owner: ModelRef {
                        provider_instance: "openai-codex".into(),
                        model: "gpt-6.1-sol".into(),
                    },
                    schema: STATE_SCHEMA.into(),
                    artifact: foreign_ref,
                },
                ContentBlock::Text {
                    text: "from the other provider".into(),
                },
            ],
        },
        user("And now?"),
        ContextItem::Assistant {
            entry_id: EntryId::new(),
            outcome: harness_core::session::ResponseOutcome::Completed,
            content: vec![
                ContentBlock::ProviderState {
                    owner: model(),
                    schema: STATE_SCHEMA.into(),
                    artifact: ArtifactRef {
                        sha256: Digest::of(payload),
                        bytes: payload.len() as u64,
                    },
                },
                ContentBlock::Text {
                    text: "pong".into(),
                },
            ],
        },
    ];
    let body = request::build(Profile::Anthropic, &input, &reads).expect("replay");
    assert_eq!(
        body,
        json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": 256,
            "stream": true,
            "system": "You are the harness.",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Summary of earlier conversation:\n\nTurns one to three."},
                        {"type": "text", "text": "Keep going"},
                    ],
                },
                // The foreign block is gone; the provider's own text stays.
                {"role": "assistant", "content": [{"type": "text", "text": "from the other provider"}]},
                {"role": "user", "content": [{"type": "text", "text": "And now?"}]},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "kept reasoning", "signature": "sig_after_summary"},
                        {"type": "text", "text": "pong"},
                    ],
                },
            ],
            "thinking": {"type": "adaptive", "block_binding": drop_block()},
        })
    );
    assert!(request::replays_thinking(&body));
}

/// The beta header and the body control are chosen by one predicate, so a
/// request can never carry `block_binding` without the beta that permits it
/// (and vice versa).
#[test]
fn preserved_thinking_beta_header_follows_the_body_predicate() {
    let beta = AnthropicMessages::headers(Profile::Anthropic, true);
    assert_eq!(
        beta.get("anthropic-beta")
            .map(|value| value.to_str().unwrap()),
        Some(super::preserved::BETA)
    );
    assert_eq!(
        super::preserved::BETA,
        "thinking-binding-controls-2026-08-01"
    );
    let plain = AnthropicMessages::headers(Profile::Anthropic, false);
    assert!(plain.get("anthropic-beta").is_none());
    for headers in [&beta, &plain] {
        assert_eq!(
            headers.get("anthropic-version").unwrap().to_str().unwrap(),
            "2026-10-21"
        );
        assert_eq!(
            headers.get("accept").unwrap().to_str().unwrap(),
            "text/event-stream"
        );
    }
    // A body with no replayed block sends neither, which keeps the golden
    // request above (`request_body_is_one_correct_messages_body`) unchanged.
    let body = request::build(Profile::Anthropic, &full_input(), &Reads::default()).expect("build");
    assert!(!request::replays_thinking(&body));
    assert!(body.get("thinking").unwrap().get("block_binding").is_none());
}

#[test]
fn a_modified_thinking_payload_is_not_replayable() {
    for payload in [
        br#"{"type":"thinking","thinking":"step one"}"#.as_slice(),
        br#"{"type":"redacted_thinking"}"#.as_slice(),
        br#"{"type":"text","text":"not state"}"#.as_slice(),
    ] {
        let (input, reads) = state_input(payload, None);
        let error = request::build(Profile::Anthropic, &input, &reads).expect_err("invalid state");
        assert_eq!(error.code, ModelErrorCode::InvalidRequest);
    }
}

// ------------------------------------------------------------ event mapping

fn sse(kind: &str, data: Value) -> SseEvent {
    SseEvent {
        event: Some(kind.into()),
        data: data.to_string(),
        id: None,
        retry: None,
    }
}

fn new_mapper() -> (Mapper, Arc<super::Counters>) {
    let counters = Arc::new(super::Counters::default());
    (Mapper::new(counters.clone(), Profile::Anthropic), counters)
}

fn mapped(
    events: &[&str],
) -> (
    Vec<harness_core::model::ProviderEvent>,
    Arc<super::Counters>,
) {
    let (mut mapper, counters) = new_mapper();
    let mut out = Vec::new();
    for frame in events {
        let (name, data) = frame.split_once('\n').expect("frame");
        let name = name.strip_prefix("event: ").expect("name");
        let data = data
            .strip_prefix("data: ")
            .expect("data")
            .lines()
            .next()
            .expect("line");
        out.extend(
            mapper
                .push(&sse(
                    name,
                    serde_json::from_str(data).expect("valid JSON fixture frame"),
                ))
                .expect("maps"),
        );
    }
    (out, counters)
}

#[test]
fn thinking_deltas_are_ignored_pings_are_counted_and_unknown_is_counted() {
    let (events, counters) = mapped(&[
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"stop_reason\":null}}",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}",
        "event: ping\ndata: {\"type\":\"ping\"}",
        "event: mystery_event\ndata: {\"type\":\"mystery_event\"}",
    ]);
    assert_eq!(events, vec![harness_core::model::ProviderEvent::Started]);
    assert_eq!(counters.ignored_events(), 2);
    assert_eq!(counters.unknown_events(), 1);
}

#[test]
fn structural_violations_are_malformed_streams() {
    let frames = [
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}",
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"content\":[]}}",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":7,\"delta\":{\"type\":\"text_delta\",\"text\":\"x\"}}",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}",
    ];
    for frame in frames {
        let (name, data) = frame.split_once('\n').expect("frame");
        let name = name.strip_prefix("event: ").expect("name");
        let data = data.strip_prefix("data: ").expect("data");
        let (mut mapper, _) = new_mapper();
        let value = serde_json::from_str(data).expect("json");
        let result = if name == "content_block_start" {
            // A content block before `message_start` is malformed.
            mapper.push(&sse(name, value))
        } else if frame.contains("message_start") {
            // A user-role start message is not an assistant response.
            let mut value = serde_json::from_str::<Value>(data).expect("json");
            value["message"]["role"] = json!("user");
            mapper.push(&sse(name, value))
        } else {
            mapper.push(&sse(name, value))
        };
        let error = result.expect_err(frame);
        assert_eq!(error.code, ModelErrorCode::MalformedStream, "{frame}");
    }

    let (mut mapper, _) = new_mapper();
    assert_eq!(
        mapper
            .push(&SseEvent {
                event: None,
                data: "{not json".into(),
                id: None,
                retry: None,
            })
            .expect_err("invalid json")
            .code,
        ModelErrorCode::MalformedStream
    );
    let (mapper, _) = new_mapper();
    assert_eq!(
        mapper.finish_eof().expect_err("missing terminal").code,
        ModelErrorCode::MalformedStream
    );
}

#[test]
fn a_thinking_block_without_a_signature_is_malformed() {
    let (mut mapper, _) = new_mapper();
    let frames = [
        (
            "message_start",
            json!({"type":"message_start","message":{"role":"assistant","content":[]}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
    ];
    for (name, value) in frames {
        let result = mapper.push(&sse(name, value));
        if name == "content_block_stop" {
            let error = result.expect_err("signature required");
            assert_eq!(error.code, ModelErrorCode::MalformedStream);
        } else {
            result.expect("prior frames");
        }
    }
}

#[test]
fn error_events_are_classified_without_echoing_provider_text() {
    for (native, expected) in [
        ("overloaded_error", ModelErrorCode::ProviderFailure),
        ("rate_limit_error", ModelErrorCode::RateLimit),
        ("authentication_error", ModelErrorCode::Authentication),
        ("invalid_request_error", ModelErrorCode::InvalidRequest),
    ] {
        let (mut mapper, _) = new_mapper();
        let emitted = mapper
            .push(&sse(
                "error",
                json!({
                    "type": "error",
                    "error": {"type": native, "message": "SENTINEL-PROVIDER-CONTENT"},
                }),
            ))
            .expect("error event");
        let Some(harness_core::model::ProviderEvent::Finished { error, .. }) = emitted.last()
        else {
            panic!("terminal failure");
        };
        let error = error.as_ref().expect("error");
        assert_eq!(error.code, expected);
        assert_eq!(error.provider_code.as_deref(), Some(native));
        assert!(!error.message.contains("SENTINEL"));
    }
    let classified = super::classify(
        Some("request_too_large"),
        "prompt is too long: 210000 tokens > 200000 maximum",
    );
    assert_eq!(classified, ModelErrorCode::ContextOverflow);
    // Local, non-HTTP failures keep their diagnosis.
    let original =
        crate::error::TransportError::new(ModelErrorCode::MalformedStream, "truncated SSE event")
            .model_error();
    assert_eq!(super::refine_http(original.clone(), b""), original);
}

// ------------------------------------------------------------------ secrets

#[tokio::test]
async fn the_api_key_is_sent_as_x_api_key_and_never_rendered() {
    let server = Server::start(parse_fixture(fixture_text("text")));
    let adapter = adapter(server.url.clone());
    let rendered = format!("{adapter:?}");
    assert!(!rendered.contains(SENTINEL_SECRET), "{rendered}");
    assert!(rendered.contains("[redacted]"));
    let mut stream = adapter.stream(
        ModelRequest {
            response_id: ResponseId::new(),
            artifacts: ArtifactSource::default(),
            input: ModelInput {
                model: model(),
                items: vec![user("Hello")],
                tools: Vec::new(),
                options: options(128),
                continuation: None,
            },
        },
        Default::default(),
    );
    let mut events = Vec::new();
    while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
        events.push(format!("{event:?}"));
    }
    for event in &events {
        assert!(!event.contains(SENTINEL_SECRET), "{event}");
    }
    let headers = server.header_block();
    assert!(
        headers.contains(&format!("x-api-key: {}", SENTINEL_SECRET.to_lowercase())),
        "{headers}"
    );
    assert!(!headers.contains("authorization"), "{headers}");
    assert!(
        headers.contains("anthropic-version: 2026-10-21"),
        "{headers}"
    );
    assert!(headers.contains("accept: text/event-stream"), "{headers}");
    let body = server
        .submitted
        .lock()
        .expect("record")
        .clone()
        .expect("the adapter submitted a request");
    assert!(!body.to_string().contains(SENTINEL_SECRET));
    assert!(events.iter().any(|event| event.contains("Finished")));
    // An invalid key fails without echoing its bytes.
    let error = Credential::api_key(
        format!("{SENTINEL_SECRET}\r\n"),
        reqwest::header::HeaderMap::new(),
        None,
    )
    .expect_err("invalid credential");
    assert!(!format!("{error:?}").contains(SENTINEL_SECRET));
    assert!(!format!("{error:?}").contains(SENTINEL_SECRET));
}

#[tokio::test]
async fn a_missing_credential_fails_before_submission() {
    let adapter = AnthropicMessages::with_transport(Arc::new(Missing), transport());
    let mut stream = adapter.stream(
        ModelRequest {
            response_id: ResponseId::new(),
            artifacts: ArtifactSource::default(),
            input: ModelInput {
                model: model(),
                items: vec![user("Hello")],
                tools: Vec::new(),
                options: options(64),
                continuation: None,
            },
        },
        Default::default(),
    );
    let event = futures_util::StreamExt::next(&mut stream)
        .await
        .expect("terminal");
    let harness_core::model::ProviderEvent::Finished { error, .. } = event else {
        panic!("terminal failure");
    };
    let error = error.expect("error");
    assert_eq!(error.code, ModelErrorCode::Authentication);
    assert!(error.message.contains("ANTHROPIC_API_KEY"));
}

#[tokio::test]
async fn a_rate_limit_is_reported_once_without_a_retry() {
    let server = Arc::new(Server::start(parse_fixture(fixture_text("rate_limit"))));
    let mut stream =
        adapter(server.url.clone()).stream(case(CaseKind::RateLimit).request, Default::default());
    let mut terminal = 0;
    while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
        if matches!(event, harness_core::model::ProviderEvent::Finished { .. }) {
            terminal += 1;
        }
    }
    assert_eq!(terminal, 1);
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn finished_releases_the_connection_without_another_poll() {
    use harness_core::model::ProviderEvent;
    use std::time::Duration;

    let mut reply = parse_fixture(fixture_text("text"));
    reply.hold = true;
    let server = Server::start(reply);
    let mut stream =
        adapter(server.url.clone()).stream(case(CaseKind::Text).request, Default::default());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = futures_util::StreamExt::next(&mut stream)
                .await
                .expect("event");
            if matches!(event, ProviderEvent::Finished { .. }) {
                break;
            }
        }
    })
    .await
    .expect("terminal event");
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
    // Do not poll or drop the stream until after observing peer close.
    tokio::time::timeout(Duration::from_secs(2), async {
        while !server.released.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("Finished retained a live connection");
    drop(stream);
}

#[tokio::test]
async fn fixture_attribution_ignores_wrong_paths_and_missing_run_headers() {
    let server = Server::start(Reply::silent());
    let url = reqwest::Url::parse(&server.url).expect("url");
    let address = ("127.0.0.1", url.port().expect("port"));
    for request in [
        format!(
            "POST /wrong HTTP/1.1\r\nx-harness-test: {}\r\ncontent-length: 2\r\n\r\n{{}}",
            client_nonce()
        ),
        format!(
            "POST {} HTTP/1.1\r\ncontent-length: 2\r\n\r\n{{}}",
            url.path()
        ),
    ] {
        let mut socket = TcpStream::connect(address).await.expect("probe connect");
        socket
            .write_all(request.as_bytes())
            .await
            .expect("probe write");
        let mut byte = [0];
        tokio::time::timeout(std::time::Duration::from_secs(2), socket.read(&mut byte))
            .await
            .expect("unrelated connection closed")
            .expect("read");
    }
    assert!(server.ignored.load(Ordering::SeqCst) >= 2);
    assert!(server.submitted.lock().expect("record").is_none());
    assert_eq!(server.connections.load(Ordering::SeqCst), 0);
    assert!(server.released.load(Ordering::SeqCst));
}

// ------------------------------------------------------------------ catalog

#[test]
fn catalog_rows_match_the_official_models_overview() {
    use harness_core::context::InputMediaKind;
    for name in ["claude-haiku-5-5", "claude-sonnet-5-5", "claude-opus-5-5"] {
        let info = super::info(Profile::Anthropic, name).expect("catalogued model");
        assert_eq!(info.limits.context_tokens, Some(1_000_000), "{name}");
        assert_eq!(info.limits.max_output_tokens, Some(128_000), "{name}");
        assert!(
            info.capabilities
                .input_media
                .contains(&InputMediaKind::Image),
            "{name}"
        );
        assert!(info.capabilities.tools, "{name}");
        assert!(info.capabilities.reasoning_effort, "{name}");
    }
    assert_eq!(super::LIVE_CHECK_MODEL, "claude-haiku-5-5");
    assert!(super::info(Profile::Anthropic, "claude-unknown").is_none());
    assert!(!super::capabilities(Profile::Anthropic, "claude-unknown").reasoning_effort);
    let catalog = super::catalog(Profile::Anthropic, "anthropic");
    assert!(catalog.limits(&model()).is_some());
}

#[test]
fn env_anthropic_key_selects_the_api_key_header() {
    if std::env::var_os("HARNESS_ANTHROPIC_ENV_CHILD").is_some() {
        let credential = crate::credentials::EnvApiKey::with_scheme(
            "ANTHROPIC_API_KEY",
            crate::credentials::CredentialScheme::ApiKeyHeader,
        )
        .expect("env key")
        .credential()
        .expect("credential");
        let headers = credential.request_headers();
        assert_eq!(headers["x-api-key"], SENTINEL_SECRET);
        assert!(headers.get("authorization").is_none());
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "anthropic::tests::env_anthropic_key_selects_the_api_key_header",
            "--nocapture",
        ])
        .env("HARNESS_ANTHROPIC_ENV_CHILD", "1")
        .env("ANTHROPIC_API_KEY", SENTINEL_SECRET)
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains(SENTINEL_SECRET),
        "secret leaked in stdout"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(SENTINEL_SECRET),
        "secret leaked in stderr"
    );
    assert!(output.status.success(), "credential child failed");
}

// =========================================================================
// The DeepSeek profile
// =========================================================================

/// DeepSeek on the same adapter: its own endpoint, credential, catalog, state
/// schema and thinking rules.
///
/// The wire format is the same Messages API, so the manifest, the fake server
/// and the decoding helpers above are reused unchanged; only the streams whose
/// documented shape differs have their own fixture under `fixtures/deepseek/`.
/// Every assertion here is new: no Anthropic test changed.
mod deepseek {
    use super::*;

    use super::super::{
        DEEPSEEK_ENDPOINT, DEEPSEEK_LIVE_CHECK_MODEL, DEEPSEEK_STATE_SCHEMA,
        DEFAULT_MAX_OUTPUT_TOKENS, catalog, info,
    };

    const DEEPSEEK_SENTINEL: &str = "SENTINEL-DEEPSEEK-KEY-TOKEN";

    fn ds_model() -> ModelRef {
        ModelRef {
            provider_instance: "deepseek".into(),
            model: "deepseek-flash".into(),
        }
    }

    fn ds_adapter(url: String) -> AnthropicMessages {
        AnthropicMessages::with_profile(
            Profile::DeepSeek,
            Arc::new(fixed(DEEPSEEK_SENTINEL)),
            transport(),
        )
        .with_endpoint(url)
    }

    /// Fixture text for this profile: its own streams where DeepSeek differs,
    /// and the shared Messages fixtures where the vocabulary is identical.
    fn ds_fixture_text(name: &str) -> &'static str {
        match name {
            // A thinking block with no signature at all.
            "native_state" => include_str!("../../fixtures/deepseek/native_state.sse"),
            // `cache_read_input_tokens` reported, `cache_creation_input_tokens`
            // always 0, usage from the final `message_delta`.
            "usage" => include_str!("../../fixtures/deepseek/usage.sse"),
            // An unnamed `data` frame holding only an error object.
            "unnamed_error" => include_str!("../../fixtures/deepseek/unnamed_error.sse"),
            other => fixture_text(other),
        }
    }

    fn ds_reply_for(fixture: &str) -> Reply {
        match fixture {
            "cancel_setup" | "deadline" | "unsupported_option" => Reply::silent(),
            "cancel_streaming" | "drop" => Reply::holding(&hold_stream()),
            other => parse_fixture(ds_fixture_text(other)),
        }
    }

    fn ds_factory() -> impl Fn(&str) -> Fixture {
        move |fixture: &str| {
            let server = Arc::new(Server::start(ds_reply_for(fixture)));
            let url = server.url.clone();
            let observed = Arc::clone(&server);
            Fixture {
                provider: Arc::new(ds_adapter(url)),
                observe: Arc::new(move || observed.observation()),
            }
        }
    }

    /// Body frames of one fixture as (event name, data) pairs.
    fn ds_frames(fixture: &str) -> Vec<(Option<String>, String)> {
        let (_, body) = ds_fixture_text(fixture)
            .split_once("---\n")
            .expect("fixture separator");
        body.split("\n\n")
            .filter(|frame| !frame.trim().is_empty())
            .map(|frame| {
                let mut event = None;
                let mut data = String::new();
                for line in frame.lines() {
                    if let Some(name) = line.strip_prefix("event: ") {
                        event = Some(name.to_owned());
                    } else if let Some(value) = line.strip_prefix("data: ") {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(value);
                    }
                }
                (event, data)
            })
            .filter(|(_, data)| !data.is_empty())
            .collect()
    }

    fn ds_mapped(
        fixture: &str,
    ) -> (
        Vec<harness_core::model::ProviderEvent>,
        Arc<crate::anthropic::Counters>,
    ) {
        let counters = Arc::new(crate::anthropic::Counters::default());
        let mut mapper = Mapper::new(counters.clone(), Profile::DeepSeek);
        let mut out = Vec::new();
        for (event, data) in ds_frames(fixture) {
            out.extend(
                mapper
                    .push(&SseEvent {
                        event,
                        data,
                        id: None,
                        retry: None,
                    })
                    .expect("maps"),
            );
        }
        (out, counters)
    }

    // --------------------------------------------------------- conformance

    /// The complete public 17-case manifest, run against the DeepSeek profile
    /// exactly as it runs against the Anthropic one.
    #[tokio::test]
    async fn every_conformance_case_passes_on_deepseek_fixtures() {
        let manifest: Vec<Case> = CaseKind::ALL
            .into_iter()
            .map(|kind| case_with(kind, ds_model(), Profile::DeepSeek))
            .collect();
        run(&ds_factory(), &manifest).await.expect("conformance");
    }

    // ------------------------------------------------------ request bodies

    fn ds_full_input() -> ModelInput {
        ModelInput {
            model: ds_model(),
            ..full_input()
        }
    }

    /// A plain turn on the DeepSeek profile: its own model, `enabled` thinking
    /// instead of `adaptive`, and no Anthropic-only control.
    #[test]
    fn a_plain_deepseek_turn_is_one_messages_body() {
        let body =
            request::build(Profile::DeepSeek, &ds_full_input(), &Reads::default()).expect("build");
        let expected = json!({
            "model": "deepseek-flash",
            "max_tokens": 512,
            "stream": true,
            "system": "You are the harness.\n\nPrefer small diffs.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Read notes.txt"}]},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "Reading it."},
                        {"type": "tool_use", "id": "toolu_fixture_1", "name": "read", "input": {"path": "notes.txt"}},
                    ],
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_fixture_1",
                            "content": "status: succeeded\nprovenance: executor\nexit_code: 0\noutput_truncated: false\n\nfile contents",
                            "is_error": false,
                        },
                        {"type": "text", "text": "Summary of earlier conversation:\n\nEarlier."},
                    ],
                },
            ],
            "tools": [
                {
                    "name": "read",
                    "description": "Read a file from the workspace.",
                    "input_schema": {
                        "type": "object",
                        "properties": {"path": {"type": "string"}, "offset_bytes": {"type": "integer"}},
                        "required": ["path"],
                        "additionalProperties": false,
                    },
                },
                {
                    "name": "bash",
                    "description": "Run one command.",
                    "input_schema": {
                        "type": "object",
                        "properties": {"command": {"type": "string"}},
                        "required": ["command"],
                        "additionalProperties": false,
                    },
                },
            ],
            "thinking": {"type": "enabled"},
            "output_config": {"effort": "medium"},
            // Documented as ignored by DeepSeek; the option keeps one meaning.
            "service_tier": "standard_only",
        });
        assert_eq!(body, expected);
    }

    /// The tool loop DeepSeek documents: the assistant's complete content,
    /// thinking block included, comes back before the matching tool results.
    #[test]
    fn a_tool_loop_replays_the_unsigned_thinking_block_verbatim() {
        let payload = br#"{"type":"thinking","thinking":"The note holds the answer."}"#;
        let reference = ArtifactRef {
            sha256: Digest::of(payload),
            bytes: payload.len() as u64,
        };
        let reads = Reads::from_bytes(reference.clone(), payload.to_vec());
        let call_id = CallId::new();
        let mut input = ModelInput {
            model: ds_model(),
            items: vec![
                user("Read notes.txt"),
                ContextItem::Assistant {
                    entry_id: EntryId::new(),
                    outcome: harness_core::session::ResponseOutcome::Completed,
                    content: vec![
                        ContentBlock::ProviderState {
                            owner: ds_model(),
                            schema: DEEPSEEK_STATE_SCHEMA.into(),
                            artifact: reference,
                        },
                        ContentBlock::ToolCall {
                            call_id: call_id.clone(),
                            name: "read".into(),
                            arguments: serde_json::from_value(json!({"path": "notes.txt"}))
                                .expect("args"),
                            provider_call_id: Some("toolu_deepseek_1".into()),
                        },
                    ],
                },
            ],
            tools: tools(),
            options: options(256),
            continuation: None,
        };
        input.items.push(ContextItem::ToolResult {
            entry_id: EntryId::new(),
            provenance: Provenance::Executor,
            result: ToolResult {
                call: CallRef {
                    response_entry_id: EntryId::new(),
                    call_id,
                },
                invocation_id: None,
                status: ToolStatus::Succeeded,
                content: vec![ContentBlock::Text {
                    text: "file contents".into(),
                }],
                metadata: ToolMetadata {
                    exit_code: Some(0),
                    duration_ms: Some(3),
                    output_truncated: false,
                    provenance: Provenance::Executor,
                    artifacts: Vec::new(),
                    code: None,
                    file_effect: None,
                    file_paths: Vec::new(),
                },
            },
        });
        let body = request::build(Profile::DeepSeek, &input, &reads).expect("replay");
        let content = body["messages"][1]["content"]
            .as_array()
            .expect("assistant content");
        assert_eq!(
            content[0],
            json!({"type": "thinking", "thinking": "The note holds the answer."})
        );
        assert_eq!(content[1]["type"], json!("tool_use"));
        assert_eq!(
            body["messages"][2]["content"][0]["tool_use_id"],
            json!("toolu_deepseek_1")
        );
        // No effort asked for: no thinking object at all, because DeepSeek
        // already thinks by default, and no binding control in any case.
        assert!(body.get("thinking").is_none(), "{body}");
        assert!(!body.to_string().contains("block_binding"));
    }

    /// A narrowed summary source: one user message, an answered tool call, and
    /// a completed final assistant text response. That is the shape the first
    /// live matrix submitted as a summary request.
    fn summary_source() -> (ModelInput, CallId) {
        let call_id = CallId::new();
        let input = ModelInput {
            model: ds_model(),
            items: vec![
                user("Read notes.txt"),
                ContextItem::Assistant {
                    entry_id: EntryId::new(),
                    outcome: harness_core::session::ResponseOutcome::Completed,
                    content: vec![ContentBlock::ToolCall {
                        call_id: call_id.clone(),
                        name: "read".into(),
                        arguments: serde_json::from_value(json!({"path": "notes.txt"}))
                            .expect("args"),
                        provider_call_id: Some("toolu_summary_1".into()),
                    }],
                },
                ContextItem::ToolResult {
                    entry_id: EntryId::new(),
                    provenance: Provenance::Executor,
                    result: ToolResult {
                        call: CallRef {
                            response_entry_id: EntryId::new(),
                            call_id: call_id.clone(),
                        },
                        invocation_id: None,
                        status: ToolStatus::Succeeded,
                        content: vec![ContentBlock::Text {
                            text: "file contents".into(),
                        }],
                        metadata: ToolMetadata {
                            exit_code: Some(0),
                            duration_ms: Some(3),
                            output_truncated: false,
                            provenance: Provenance::Executor,
                            artifacts: Vec::new(),
                            code: None,
                            file_effect: None,
                            file_paths: Vec::new(),
                        },
                    },
                },
                // The completed final assistant response the matrix sent.
                ContextItem::Assistant {
                    entry_id: EntryId::new(),
                    outcome: harness_core::session::ResponseOutcome::Completed,
                    content: vec![ContentBlock::Text {
                        text: "The file says hello.".into(),
                    }],
                },
            ],
            tools: Vec::new(),
            options: options(256),
            continuation: None,
        };
        (input, call_id)
    }

    /// M6 decision 16 on the real adapter. The completed source ends with an
    /// assistant response, so without the shared request-only prompt the body is
    /// assistant prefill; with it, the adapter submits a standalone final user
    /// turn while the paired call/result and the final assistant response stay
    /// intact. Offline: loopback transport only.
    #[tokio::test]
    async fn a_summary_request_ends_in_a_user_turn_after_an_assistant_response() {
        use harness_core::{
            context::compaction::{SUMMARY_REQUEST_INSTRUCTION, summary_request_item},
            model::{ProviderEvent, ProviderOutcome},
        };
        let (input, _) = summary_source();

        // Without the request item the completed source really is prefill: the
        // body ends with the assistant's own final response.
        let without = request::build(Profile::DeepSeek, &input, &Reads::default()).expect("build");
        let messages = without["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 4, "{without}");
        assert_eq!(messages.last().expect("last")["role"], json!("assistant"));
        assert_eq!(
            messages.last().expect("last")["content"],
            json!([{"type": "text", "text": "The file says hello."}])
        );

        // With the shared request-only item the request closes in a user turn.
        let mut with_request = summary_source().0;
        with_request.items.push(summary_request_item());
        let body =
            request::build(Profile::DeepSeek, &with_request, &Reads::default()).expect("build");
        let messages = body["messages"].as_array().expect("messages");
        let roles: Vec<&str> = messages
            .iter()
            .map(|message| message["role"].as_str().expect("role"))
            .collect();
        assert_eq!(
            roles,
            ["user", "assistant", "user", "assistant", "user"],
            "{body}"
        );
        // The paired call and result survive, complete and in order.
        assert_eq!(
            messages[1]["content"][0],
            json!({"type": "tool_use", "id": "toolu_summary_1", "name": "read", "input": {"path": "notes.txt"}})
        );
        assert_eq!(
            messages[2]["content"][0]["tool_use_id"],
            json!("toolu_summary_1")
        );
        // The completed final assistant response is still there, verbatim...
        assert_eq!(
            messages[3]["content"],
            json!([{"type": "text", "text": "The file says hello."}])
        );
        // ...followed by one standalone user turn carrying only the fixed text.
        assert_eq!(
            messages[4]["content"],
            json!([{"type": "text", "text": SUMMARY_REQUEST_INSTRUCTION}]),
            "{body}"
        );

        // A source that ends at a tool result is already a user turn, so the
        // request joins that turn instead of opening a new one; the pair stays
        // complete and the prompt stays the last thing the model reads.
        let mut merging = summary_source().0;
        merging.items.pop();
        merging.items.push(summary_request_item());
        let merged = request::build(Profile::DeepSeek, &merging, &Reads::default()).expect("build");
        let messages = merged["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3, "{merged}");
        assert_eq!(messages[2]["role"], json!("user"));
        let blocks = messages[2]["content"].as_array().expect("content");
        assert_eq!(blocks.len(), 2, "{merged}");
        assert_eq!(blocks[0]["type"], json!("tool_result"));
        assert_eq!(
            blocks[1],
            json!({"type": "text", "text": SUMMARY_REQUEST_INSTRUCTION})
        );

        // And through the real adapter and transport: the submitted body is that
        // same shape, and the stream really completes successfully.
        let server = Server::start(parse_fixture(ds_fixture_text("text")));
        let mut stream = ds_adapter(server.url.clone()).stream(
            ModelRequest {
                response_id: ResponseId::new(),
                artifacts: ArtifactSource::default(),
                input: with_request,
            },
            Default::default(),
        );
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(event);
        }
        assert!(
            matches!(
                events.last(),
                Some(ProviderEvent::Finished {
                    outcome: ProviderOutcome::Completed,
                    finish_reason: Some(FinishReason::Stop),
                    ..
                })
            ),
            "expected a successful terminal event, got {:?}",
            events.last()
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                ProviderEvent::Finished {
                    outcome: ProviderOutcome::Failed,
                    ..
                }
            )),
            "the loopback fixture must not fail: {events:?}"
        );
        let submitted = server
            .submitted
            .lock()
            .expect("record")
            .clone()
            .expect("the adapter submitted a request");
        assert_eq!(submitted, body);
        let messages = submitted["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[3]["content"][0]["type"], json!("text"));
        assert_eq!(messages[4]["role"], json!("user"));
        assert_eq!(
            messages[4]["content"],
            json!([{"type": "text", "text": SUMMARY_REQUEST_INSTRUCTION}])
        );
    }

    /// The profile keeps images user-side only: `deepseek-flash` accepts them in
    /// user input, `deepseek-v4-pro` does not process them at all.
    #[test]
    fn images_follow_the_deepseek_catalog() {
        let bytes = b"\x89PNG-fixture";
        let reference = ArtifactRef {
            sha256: Digest::of(bytes),
            bytes: bytes.len() as u64,
        };
        let mut input = ModelInput {
            items: vec![ContextItem::User {
                entry_id: EntryId::new(),
                content: vec![
                    ContentBlock::Text {
                        text: "What is this?".into(),
                    },
                    ContentBlock::Image {
                        artifact: reference.clone(),
                        mime_type: "image/png".into(),
                    },
                ],
            }],
            ..ds_full_input()
        };
        let reads = Reads::from_bytes(reference, bytes.to_vec());
        let body = request::build(Profile::DeepSeek, &input, &reads).expect("build");
        assert_eq!(body["messages"][0]["content"][1]["type"], json!("image"));

        input.model.model = "deepseek-v4-pro".into();
        let error = request::build(Profile::DeepSeek, &input, &reads).expect_err("capability");
        assert_eq!(error.code, ModelErrorCode::UnsupportedCapability);
        assert!(error.message.contains("image"), "{}", error.message);
    }

    // -------------------------------------------------- dropped controls

    /// The controls DeepSeek does not document are absent, even on a request
    /// that replays thinking, while the Anthropic profile still sends them.
    #[test]
    fn the_profile_drops_the_anthropic_only_controls() {
        let payload = br#"{"type":"thinking","thinking":"step one"}"#;
        let (input, reads) = ds_state_input(payload, None);
        let body = request::build(Profile::DeepSeek, &input, &reads).expect("replay");
        assert!(request::replays_thinking(&body));
        assert!(body.get("thinking").is_none(), "{body}");
        assert!(!body.to_string().contains("block_binding"), "{body}");

        let headers = AnthropicMessages::headers(Profile::DeepSeek, true);
        assert!(headers.get("anthropic-beta").is_none());
        // Documented as ignored; the profile leaves it off rather than sending a
        // version the backend never reads.
        assert!(headers.get("anthropic-version").is_none());
        assert_eq!(headers["accept"], "text/event-stream");

        let anthropic = AnthropicMessages::headers(Profile::Anthropic, true);
        assert!(anthropic.get("anthropic-beta").is_some());
        assert!(anthropic.get("anthropic-version").is_some());
        assert_eq!(Profile::DeepSeek.endpoint(), DEEPSEEK_ENDPOINT);
        assert_eq!(Profile::DeepSeek.credential_variable(), "DEEPSEEK_API_KEY");
        assert_eq!(Profile::DeepSeek.state_schema(), DEEPSEEK_STATE_SCHEMA);
    }

    fn ds_state_input(payload: &[u8], extra: Option<(&[u8], ModelRef)>) -> (ModelInput, Reads) {
        let reference = ArtifactRef {
            sha256: Digest::of(payload),
            bytes: payload.len() as u64,
        };
        let mut content = vec![
            ContentBlock::ProviderState {
                owner: ds_model(),
                schema: DEEPSEEK_STATE_SCHEMA.into(),
                artifact: reference.clone(),
            },
            ContentBlock::Text {
                text: "answer".into(),
            },
        ];
        let extra = extra.map(|(bytes, owner)| {
            let clone = ArtifactRef {
                sha256: Digest::of(bytes),
                bytes: bytes.len() as u64,
            };
            content.push(ContentBlock::ProviderState {
                owner,
                schema: DEEPSEEK_STATE_SCHEMA.into(),
                artifact: clone.clone(),
            });
            (clone, bytes)
        });
        let mut reads = Reads::from_bytes(reference, payload.to_vec());
        if let Some((clone, bytes)) = &extra {
            reads = reads.with(clone.clone(), (*bytes).to_vec());
        }
        (
            ModelInput {
                model: ds_model(),
                items: vec![
                    user("Hello"),
                    ContextItem::Assistant {
                        entry_id: EntryId::new(),
                        outcome: harness_core::session::ResponseOutcome::Completed,
                        content,
                    },
                ],
                tools: Vec::new(),
                options: options(256),
                continuation: None,
            },
            reads,
        )
    }

    // ------------------------------------------------------ thinking rules

    /// An unsigned thinking block is a shape DeepSeek documents and Anthropic
    /// does not: the profile decides, and each keeps its own fixture.
    #[test]
    fn an_unsigned_thinking_block_maps_only_on_deepseek() {
        let frames = [
            json!({"type":"message_start","message":{"role":"assistant","content":[]}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}),
            json!({"type":"content_block_stop","index":0}),
        ];
        let names = [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
        ];
        let mut deepseek = Mapper::new(
            Arc::new(crate::anthropic::Counters::default()),
            Profile::DeepSeek,
        );
        let mut emitted = Vec::new();
        for (name, value) in names.iter().zip(frames.iter()) {
            emitted.extend(deepseek.push(&sse(name, value.clone())).expect("maps"));
        }
        let state = emitted
            .iter()
            .find_map(|event| match event {
                harness_core::model::ProviderEvent::ProviderState {
                    schema, payload, ..
                } => Some((schema.clone(), payload.clone())),
                _ => None,
            })
            .expect("unsigned state");
        assert_eq!(state.0, DEEPSEEK_STATE_SCHEMA);
        assert_eq!(
            serde_json::from_slice::<Value>(&state.1).expect("json"),
            json!({"type": "thinking", "thinking": "hmm"})
        );

        let mut anthropic = Mapper::new(
            Arc::new(crate::anthropic::Counters::default()),
            Profile::Anthropic,
        );
        let mut failure = None;
        for (name, value) in names.iter().zip(frames.iter()) {
            if let Err(error) = anthropic.push(&sse(name, value.clone())) {
                failure = Some(error);
                break;
            }
        }
        assert_eq!(
            failure
                .expect("the Anthropic profile requires a signature")
                .code,
            ModelErrorCode::MalformedStream
        );
    }

    /// Replay follows the same rule: DeepSeek accepts the unsigned payload the
    /// Anthropic profile refuses.
    #[test]
    fn an_unsigned_thinking_payload_replays_only_on_deepseek() {
        let payload = br#"{"type":"thinking","thinking":"step one"}"#;
        let (input, reads) = ds_state_input(payload, None);
        let body = request::build(Profile::DeepSeek, &input, &reads).expect("deepseek");
        assert_eq!(
            body["messages"][1]["content"][0],
            json!({"type": "thinking", "thinking": "step one"})
        );
        let mut anthropic_input = input.clone();
        anthropic_input.model = model();
        // Move the block to the Anthropic owner and schema, keeping the payload
        // exactly as DeepSeek wrote it.
        for item in &mut anthropic_input.items {
            if let ContextItem::Assistant { content, .. } = item {
                for block in content.iter_mut() {
                    if let ContentBlock::ProviderState { owner, schema, .. } = block {
                        *owner = model();
                        *schema = STATE_SCHEMA.into();
                    }
                }
            }
        }
        let error = request::build(Profile::Anthropic, &anthropic_input, &reads)
            .expect_err("signature required");
        assert_eq!(error.code, ModelErrorCode::InvalidRequest);
    }

    /// `redacted_thinking` is not part of DeepSeek's vocabulary: it is neither
    /// stored as state nor replayed.
    #[test]
    fn redacted_thinking_is_outside_the_deepseek_vocabulary() {
        let frames = [
            json!({"type":"message_start","message":{"role":"assistant","content":[]}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque"}}),
            json!({"type":"content_block_stop","index":0}),
        ];
        let names = ["message_start", "content_block_start", "content_block_stop"];
        let counters = Arc::new(crate::anthropic::Counters::default());
        let mut mapper = Mapper::new(counters.clone(), Profile::DeepSeek);
        let mut emitted = Vec::new();
        for (name, value) in names.iter().zip(frames.iter()) {
            emitted.extend(mapper.push(&sse(name, value.clone())).expect("maps"));
        }
        assert!(!emitted.iter().any(|event| matches!(
            event,
            harness_core::model::ProviderEvent::ProviderState { .. }
        )));
        assert_eq!(counters.unknown_events(), 1);

        let payload = br#"{"type":"redacted_thinking","data":"opaque"}"#;
        let (input, reads) = ds_state_input(payload, None);
        let error = request::build(Profile::DeepSeek, &input, &reads).expect_err("not replayable");
        assert_eq!(error.code, ModelErrorCode::InvalidRequest);
    }

    // -------------------------------------------------------- error frames

    /// An unnamed `data` frame carrying only an error is a coded failure, never
    /// a hang or a silent success, and its provider text is not echoed.
    #[test]
    fn the_unnamed_error_frame_is_a_coded_failure() {
        let (events, _) = ds_mapped("unnamed_error");
        assert_eq!(events.len(), 1, "{events:?}");
        let harness_core::model::ProviderEvent::Finished {
            outcome,
            error: Some(error),
            ..
        } = &events[0]
        else {
            panic!("terminal failure: {:?}", events);
        };
        assert_eq!(*outcome, harness_core::model::ProviderOutcome::Failed);
        assert_eq!(error.code, ModelErrorCode::ProviderFailure);
        assert_eq!(error.message, "DeepSeek request failed");
        assert!(error.provider_code.is_none());
        assert!(!error.message.contains("timed out"));

        // A frame with neither a type nor an error is still malformed.
        let mut mapper = Mapper::new(
            Arc::new(crate::anthropic::Counters::default()),
            Profile::DeepSeek,
        );
        let error = mapper
            .push(&SseEvent {
                event: None,
                data: r#"{"message":"no type"}"#.into(),
                id: None,
                retry: None,
            })
            .expect_err("malformed");
        assert_eq!(error.code, ModelErrorCode::MalformedStream);
    }

    // -------------------------------------------------------------- usage

    /// Usage is the final `message_delta` snapshot, not the `message_start`
    /// partial one, and never their sum.
    #[test]
    fn usage_comes_from_the_final_message_delta() {
        let (events, _) = ds_mapped("usage");
        let snapshots: Vec<Usage> = events
            .iter()
            .filter_map(|event| match event {
                harness_core::model::ProviderEvent::UsageUpdated { usage } => Some(usage.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(snapshots.len(), 2, "{snapshots:?}");
        assert_eq!(snapshots[0].output_tokens, Some(1));
        assert_eq!(
            snapshots[1],
            Usage {
                input_tokens: Some(12),
                output_tokens: Some(9),
                cached_input_tokens: Some(4),
                reasoning_tokens: None,
            }
        );
        assert!(matches!(
            events.last(),
            Some(harness_core::model::ProviderEvent::Finished {
                outcome: harness_core::model::ProviderOutcome::Completed,
                ..
            })
        ));
    }

    // ------------------------------------------------------ owner scoping

    /// State written by one profile is never replayed by the other, in either
    /// direction, even when the artifact is readable and the schema matches.
    #[test]
    fn state_never_crosses_the_profile_owner_or_schema() {
        let payload = br#"{"type":"thinking","thinking":"deepseek thought"}"#;
        let (input, reads) = ds_state_input(payload, None);

        // DeepSeek skips on the owner alone.
        let mut foreign_owner = input.clone();
        if let Some(ContextItem::Assistant { content, .. }) = foreign_owner.items.get_mut(1)
            && let ContentBlock::ProviderState { owner, .. } = &mut content[0]
        {
            *owner = model();
        }
        let body = request::build(Profile::DeepSeek, &foreign_owner, &reads).expect("skip");
        assert_eq!(
            body["messages"][1]["content"],
            json!([{"type": "text", "text": "answer"}])
        );

        // DeepSeek refuses the Anthropic schema even for its own owner.
        let mut wrong_schema = input.clone();
        if let Some(ContextItem::Assistant { content, .. }) = wrong_schema.items.get_mut(1)
            && let ContentBlock::ProviderState { schema, .. } = &mut content[0]
        {
            *schema = STATE_SCHEMA.into();
        }
        let error =
            request::build(Profile::DeepSeek, &wrong_schema, &reads).expect_err("foreign schema");
        assert_eq!(error.code, ModelErrorCode::UnsupportedCapability);
        assert!(error.message.contains(STATE_SCHEMA), "{}", error.message);

        // The reverse: an Anthropic request skips DeepSeek state by owner, and
        // refuses the DeepSeek schema for its own owner.
        let mut anthropic = input.clone();
        anthropic.model = model();
        let body = request::build(Profile::Anthropic, &anthropic, &reads).expect("skip");
        assert_eq!(
            body["messages"][1]["content"],
            json!([{"type": "text", "text": "answer"}])
        );
        if let Some(ContextItem::Assistant { content, .. }) = anthropic.items.get_mut(1)
            && let ContentBlock::ProviderState { owner, .. } = &mut content[0]
        {
            *owner = model();
        }
        let error =
            request::build(Profile::Anthropic, &anthropic, &reads).expect_err("foreign schema");
        assert_eq!(error.code, ModelErrorCode::UnsupportedCapability);
        assert!(
            error.message.contains(DEEPSEEK_STATE_SCHEMA),
            "{}",
            error.message
        );

        // The mapper writes the profile's own schema for the same stream shape.
        let (events, _) = ds_mapped("native_state");
        let schema = events
            .iter()
            .find_map(|event| match event {
                harness_core::model::ProviderEvent::ProviderState { schema, .. } => {
                    Some(schema.clone())
                }
                _ => None,
            })
            .expect("state");
        assert_eq!(schema, DEEPSEEK_STATE_SCHEMA);
    }

    // ------------------------------------------------------------ secrets

    #[tokio::test]
    async fn the_deepseek_key_is_sent_as_x_api_key_and_never_rendered() {
        let server = Server::start(parse_fixture(ds_fixture_text("text")));
        let adapter = ds_adapter(server.url.clone());
        let rendered = format!("{adapter:?}");
        assert!(!rendered.contains(DEEPSEEK_SENTINEL), "{rendered}");
        assert!(rendered.contains("[redacted]"));
        assert!(rendered.contains("profile: DeepSeek"), "{rendered}");
        let mut stream = adapter.stream(
            ModelRequest {
                response_id: ResponseId::new(),
                artifacts: ArtifactSource::default(),
                input: ModelInput {
                    model: ds_model(),
                    items: vec![user("Hello")],
                    tools: Vec::new(),
                    options: options(128),
                    continuation: None,
                },
            },
            Default::default(),
        );
        let mut events = Vec::new();
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            events.push(format!("{event:?}"));
        }
        for event in &events {
            assert!(!event.contains(DEEPSEEK_SENTINEL), "{event}");
        }
        let headers = server.header_block();
        assert!(
            headers.contains(&format!("x-api-key: {}", DEEPSEEK_SENTINEL.to_lowercase())),
            "{headers}"
        );
        assert!(!headers.contains("authorization"), "{headers}");
        // The two headers the profile drops stay dropped on the wire.
        assert!(!headers.contains("anthropic-version"), "{headers}");
        assert!(!headers.contains("anthropic-beta"), "{headers}");
        assert!(headers.contains("accept: text/event-stream"), "{headers}");
        let body = server
            .submitted
            .lock()
            .expect("record")
            .clone()
            .expect("the adapter submitted a request");
        assert!(!body.to_string().contains(DEEPSEEK_SENTINEL));
        assert!(events.iter().any(|event| event.contains("Finished")));
    }

    #[test]
    fn a_missing_deepseek_credential_fails_before_submission() {
        let adapter =
            AnthropicMessages::with_profile(Profile::DeepSeek, Arc::new(Missing), transport());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let error = runtime.block_on(async {
            let mut stream = adapter.stream(
                ModelRequest {
                    response_id: ResponseId::new(),
                    artifacts: ArtifactSource::default(),
                    input: ModelInput {
                        model: ds_model(),
                        items: vec![user("Hello")],
                        tools: Vec::new(),
                        options: options(64),
                        continuation: None,
                    },
                },
                Default::default(),
            );
            let event = futures_util::StreamExt::next(&mut stream)
                .await
                .expect("terminal");
            let harness_core::model::ProviderEvent::Finished { error, .. } = event else {
                panic!("terminal failure");
            };
            error.expect("error")
        });
        assert_eq!(error.code, ModelErrorCode::Authentication);
        // The shared fixture credential source names the Anthropic variable; the
        // profile's own variable is asserted by the child-process test below.
        assert!(error.message.contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn env_deepseek_key_selects_the_api_key_header() {
        if std::env::var_os("HARNESS_DEEPSEEK_ENV_CHILD").is_some() {
            let adapter = AnthropicMessages::deepseek_from_env().expect("env key");
            assert_eq!(adapter.profile(), Profile::DeepSeek);
            let rendered = format!("{adapter:?}");
            assert!(rendered.contains(DEEPSEEK_ENDPOINT), "{rendered}");
            assert!(!rendered.contains(DEEPSEEK_SENTINEL), "{rendered}");
            let credential = crate::credentials::EnvApiKey::with_scheme(
                "DEEPSEEK_API_KEY",
                crate::credentials::CredentialScheme::ApiKeyHeader,
            )
            .expect("env key")
            .credential()
            .expect("credential");
            let headers = credential.request_headers();
            assert_eq!(headers["x-api-key"], DEEPSEEK_SENTINEL);
            assert!(headers.get("authorization").is_none());
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "anthropic::tests::deepseek::env_deepseek_key_selects_the_api_key_header",
                "--nocapture",
            ])
            .env("HARNESS_DEEPSEEK_ENV_CHILD", "1")
            .env("DEEPSEEK_API_KEY", DEEPSEEK_SENTINEL)
            .env_remove("ANTHROPIC_API_KEY")
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains(DEEPSEEK_SENTINEL),
            "secret leaked in stdout"
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(DEEPSEEK_SENTINEL),
            "secret leaked in stderr"
        );
        assert!(output.status.success(), "credential child failed");
    }

    // ------------------------------------------------------------ catalog

    #[test]
    fn catalog_rows_match_the_deepseek_models_page() {
        use harness_core::context::InputMediaKind;
        for name in ["deepseek-flash", "deepseek-v4-pro"] {
            let info = info(Profile::DeepSeek, name).expect("catalogued model");
            assert_eq!(info.limits.context_tokens, Some(1_000_000), "{name}");
            assert_eq!(info.limits.max_output_tokens, Some(384_000), "{name}");
            assert!(info.capabilities.tools, "{name}");
            assert!(info.capabilities.reasoning_effort, "{name}");
        }
        let flash = info(Profile::DeepSeek, "deepseek-flash").expect("flash");
        assert!(
            flash
                .capabilities
                .input_media
                .contains(&InputMediaKind::Image)
        );
        let pro = info(Profile::DeepSeek, "deepseek-v4-pro").expect("pro");
        assert!(
            !pro.capabilities
                .input_media
                .contains(&InputMediaKind::Image)
        );
        assert_eq!(DEEPSEEK_LIVE_CHECK_MODEL, "deepseek-flash");
        assert_eq!(DEFAULT_MAX_OUTPUT_TOKENS, 4_096);
        assert!(info(Profile::DeepSeek, "claude-sonnet-5-5").is_none());
        let deepseek_catalog = catalog(Profile::DeepSeek, "deepseek");
        assert!(deepseek_catalog.limits(&ds_model()).is_some());
        // The two profiles' rows never mix, even under one instance name.
        assert!(
            catalog(Profile::DeepSeek, "anthropic")
                .limits(&model())
                .is_none()
        );
    }
}

#[test]
fn http_unknown_error_types_are_sensitive() {
    let sentinel = "SENTINEL-CREDENTIAL-ERROR-TYPE";
    let original = ModelError {
        code: ModelErrorCode::ProviderFailure,
        message: "safe".into(),
        provider_code: Some("http_500".into()),
        retry_after_ms: None,
    };
    let body = serde_json::to_vec(&json!({"error":{"type":sentinel,"message":sentinel}})).unwrap();
    let model = super::refine_http(original, &body);
    assert_eq!(model.provider_code.as_deref(), Some("http_500"));
    assert!(
        !serde_json::to_string(&model).unwrap().contains(sentinel),
        "credential leaked"
    );
}
