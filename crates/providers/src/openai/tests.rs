//! Offline adapter tests: hand-written fixtures, a loopback fake server, the
//! public conformance suite and request/event unit tests. No test reads the
//! user's real Codex login or reaches the network.

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
    OpenAiResponses, Profile, STATE_SCHEMA,
    events::{self, Mapper},
    request::{self, Reads},
};
use crate::{
    credentials::Credential, sse::SseEvent, transport::Transport, transport::TransportConfig,
};

// Other provider tests include this private test helper independently; keep
// this adapter's fixtures self-contained without editing the shared tests.
#[allow(dead_code, clippy::duplicate_mod)]
#[path = "../../../../tests/support/loopback.rs"]
mod loopback;

fn client_nonce() -> &'static str {
    static NONCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NONCE.get_or_init(loopback::nonce)
}

// ---------------------------------------------------------------- fake server

/// One canned HTTP response. `silent` sends no headers at all; `hold` keeps the
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
            silent: false,
            hold: false,
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
        let path = format!("/responses/{}", loopback::nonce());
        let url = format!("http://{address}{path}");
        let submitted = Arc::new(Mutex::new(None));
        let released = Arc::new(AtomicBool::new(true));
        let connections = Arc::new(AtomicUsize::new(0));
        let ignored = Arc::new(AtomicUsize::new(0));
        let (record, closed, count, unrelated) = (
            submitted.clone(),
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
                let (record, closed, count, unrelated) = (
                    record.clone(),
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
                    let Some(body) = body else {
                        let ignored = unrelated.fetch_add(1, Ordering::SeqCst) + 1;
                        eprintln!("openai fixture: ignored unrelated connection #{ignored}");
                        return;
                    };
                    count.fetch_add(1, Ordering::SeqCst);
                    closed.store(false, Ordering::SeqCst);
                    *record.lock().expect("record") = Some(body);
                    if !reply.silent {
                        let mut head =
                            format!("HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
                        for (name, value) in &reply.headers {
                            head.push_str(&format!("{name}: {value}\r\n"));
                        }
                        // A held response must stay open mid-body, so it is chunked and
                        // never terminated; `content-length` would end the body early
                        // and the client would see a clean EOF instead of pending.
                        if reply.hold {
                            head.push_str("transfer-encoding: chunked\r\n\r\n");
                            let _ = socket.write_all(head.as_bytes()).await;
                            let chunk = format!("{:x}\r\n", reply.body.len());
                            let _ = socket.write_all(chunk.as_bytes()).await;
                            let _ = socket.write_all(&reply.body).await;
                            let _ = socket.write_all(b"\r\n").await;
                        } else {
                            head.push_str(&format!("content-length: {}\r\n\r\n", reply.body.len()));
                            let _ = socket.write_all(head.as_bytes()).await;
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
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        _ => "Internal Server Error",
    }
}

async fn read_request(socket: &mut TcpStream, path: &str) -> Option<Value> {
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
            let raw_head = String::from_utf8_lossy(&buffer[..position]);
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
            return serde_json::from_slice(&buffer[end..end + length]).ok();
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
        "text" | "request" => include_str!("../../fixtures/openai/text.sse"),
        "one_tool" => include_str!("../../fixtures/openai/one_tool.sse"),
        "interleaved_tools" => include_str!("../../fixtures/openai/interleaved_tools.sse"),
        "malformed_tool_json" => include_str!("../../fixtures/openai/malformed_tool_json.sse"),
        "refusal" => include_str!("../../fixtures/openai/refusal.sse"),
        "output_limit" => include_str!("../../fixtures/openai/output_limit.sse"),
        "usage" => include_str!("../../fixtures/openai/usage.sse"),
        "native_state" => include_str!("../../fixtures/openai/native_state.sse"),
        "provider_error" => include_str!("../../fixtures/openai/provider_error.http"),
        "rate_limit" => include_str!("../../fixtures/openai/rate_limit.http"),
        "setup_failure" => include_str!("../../fixtures/openai/setup_failure.http"),
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
    for block in fixture_text("text").split("\n\n").take(3) {
        text.push_str(block);
        text.push_str("\n\n");
    }
    text
}

// -------------------------------------------------------------- credentials

struct Fixed(Credential);

impl crate::credentials::CredentialSource for Fixed {
    fn credential(&self) -> Result<Credential, ModelError> {
        Ok(self.0.clone())
    }
}

struct Missing;

impl crate::credentials::CredentialSource for Missing {
    fn credential(&self) -> Result<Credential, ModelError> {
        Err(ModelError {
            code: ModelErrorCode::Authentication,
            message: "missing, invalid or expired Codex login; run `codex login`".into(),
            provider_code: None,
            retry_after_ms: None,
        })
    }
}

const SENTINEL_SECRET: &str = "SENTINEL-SECRET-TOKEN";

fn fixed() -> Fixed {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "x-harness-test",
        client_nonce().parse().expect("run header"),
    );
    Fixed(Credential::bearer(SENTINEL_SECRET.to_owned(), headers, None).expect("credential"))
}

fn transport() -> Transport {
    Transport::new(TransportConfig {
        allow_loopback_http: true,
        ..TransportConfig::default()
    })
    .expect("transport")
}

fn adapter(profile: Profile, url: String) -> OpenAiResponses {
    OpenAiResponses::with_transport(profile, Arc::new(fixed()), transport()).with_endpoint(url)
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
                    input_schema: tool.get("parameters").cloned().unwrap_or(Value::Null),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut openai = Map::new();
    if let Some(summary) = body.pointer("/reasoning/summary").and_then(Value::as_str) {
        openai.insert("reasoning_summary".into(), json!(summary));
    }
    if let Some(key) = body.get("prompt_cache_key").and_then(Value::as_str) {
        openai.insert("prompt_cache_key".into(), json!(key));
    }
    if let Some(tier) = body.get("service_tier").and_then(Value::as_str) {
        openai.insert("service_tier".into(), json!(tier));
    }
    let mut provider_options = Map::new();
    if !openai.is_empty() {
        provider_options.insert("openai".into(), Value::Object(openai));
    }
    SubmittedRequest {
        tools,
        options: InferenceOptions {
            max_output_tokens: body
                .get("max_output_tokens")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0),
            reasoning_effort: body
                .pointer("/reasoning/effort")
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
        provider_instance: "openai".into(),
        model: "gpt-5.5".into(),
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

fn case(kind: CaseKind, profile: Profile) -> Case {
    let mut expected = Expected::default();
    let mut request = ModelRequest {
        response_id: ResponseId::new(),
        artifacts: ArtifactSource::default(),
        input: ModelInput {
            model: model(),
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
                    provider_call_id: Some("call_fixture_1".into()),
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
                    provider_call_id: Some("call_fixture_a".into()),
                    arguments: json!({"command": "echo a"}),
                },
            );
            expected.tools.insert(
                1,
                ExpectedTool {
                    name: "bash".into(),
                    provider_call_id: Some("call_fixture_b".into()),
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
                provider_code: Some("server_error".into()),
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
                provider_code: Some("rate_limit_exceeded".into()),
                retry_after_ms: Some(2000),
            });
            "rate_limit"
        }
        CaseKind::SetupFailure => {
            expected.outcome = harness_core::model::ProviderOutcome::Failed;
            expected.finish_reason = None;
            expected.error = Some(ModelError {
                code: ModelErrorCode::Authentication,
                message: events::safe_message(ModelErrorCode::Authentication, profile),
                provider_code: Some("invalid_api_key".into()),
                retry_after_ms: None,
            });
            "setup_failure"
        }
        CaseKind::Usage => {
            expected.usage = vec![
                Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(2),
                    cached_input_tokens: Some(4),
                    reasoning_tokens: Some(0),
                },
                Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(9),
                    cached_input_tokens: Some(4),
                    reasoning_tokens: Some(3),
                },
            ];
            expected.text.insert(0, "Hi".into());
            "usage"
        }
        CaseKind::NativeState => {
            expected
                .reasoning
                .insert(0, "Considering the greeting.".into());
            expected.native.insert(
                1,
                NativeMetadata {
                    schema: STATE_SCHEMA.into(),
                    bytes: reasoning_item_bytes(),
                },
            );
            expected.text.insert(2, "pong".into());
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
            request.input.options.provider_options =
                serde_json::from_value(json!({"openai": {"reasoning_summary": "auto"}}))
                    .expect("options");
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
                    "openai": {
                        "reasoning_summary": "concise",
                        "prompt_cache_key": "fixture-key",
                        "service_tier": "auto",
                    }
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
                message:
                    "provider option namespace anthropic is not supported by the OpenAI adapter"
                        .into(),
                provider_code: None,
                retry_after_ms: None,
            });
            request.input.options.provider_options =
                serde_json::from_value(json!({"anthropic": {"thinking": true}})).expect("options");
            "unsupported_option"
        }
    };
    if profile == Profile::ChatGptCodex {
        request.input.model.model = super::LIVE_CHECK_MODEL.into();
        // `ChatGptCodex` sends no `max_output_tokens`, so the decoded evidence
        // reports 0; the row carries that value so the round-trip check stays
        // meaningful for every field the profile does serialize.
        request.input.options.max_output_tokens = 0;
    }
    Case {
        kind,
        fixture: fixture.into(),
        request,
        expected,
        unsupported_option: (kind == CaseKind::UnsupportedOption).then(|| "anthropic".into()),
    }
}

fn reasoning_item_bytes() -> usize {
    for block in fixture_text("native_state").split("\n\n") {
        if !block.contains("gAAAA") {
            continue;
        }
        let data = block
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("data line");
        let value: Value = serde_json::from_str(data).expect("event");
        return serde_json::to_vec(&value["item"]).expect("item").len();
    }
    panic!("no encrypted reasoning item in the fixture");
}

fn reply_for(fixture: &str) -> Reply {
    match fixture {
        "cancel_setup" => Reply::silent(),
        "cancel_streaming" | "drop" => Reply::holding(&hold_stream()),
        "deadline" => Reply::silent(),
        "unsupported_option" => Reply::silent(),
        "request" => parse_fixture(fixture_text("request")),
        other => parse_fixture(fixture_text(other)),
    }
}

fn factory(profile: Profile) -> impl Fn(&str) -> Fixture {
    move |fixture: &str| {
        let server = Arc::new(Server::start(reply_for(fixture)));
        let url = server.url.clone();
        let observed = Arc::clone(&server);
        Fixture {
            provider: Arc::new(adapter(profile, url)),
            observe: Arc::new(move || observed.observation()),
        }
    }
}

// ------------------------------------------------------------- conformance

#[tokio::test]
async fn platform_passes_every_conformance_case() {
    let manifest: Vec<Case> = CaseKind::ALL
        .into_iter()
        .map(|kind| case(kind, Profile::Platform))
        .collect();
    run(&factory(Profile::Platform), &manifest)
        .await
        .expect("platform conformance");
}

#[tokio::test]
async fn chatgpt_codex_passes_every_conformance_case() {
    let manifest: Vec<Case> = CaseKind::ALL
        .into_iter()
        .map(|kind| case(kind, Profile::ChatGptCodex))
        .collect();
    run(&factory(Profile::ChatGptCodex), &manifest)
        .await
        .expect("chatgpt codex conformance");
}

// ------------------------------------------------------------ request shape

fn full_input() -> ModelInput {
    let call_id = CallId::new();
    let mut provider_options = Map::new();
    provider_options.insert("openai".into(), json!({"reasoning_summary": "concise"}));
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
                        provider_call_id: Some("call_fixture_1".into()),
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
        ],
        tools: tools(),
        options: InferenceOptions {
            max_output_tokens: 512,
            reasoning_effort: Some(ReasoningEffort::Medium),
            provider_options,
        },
        continuation: None,
    }
}

#[test]
fn platform_request_is_one_correct_stateless_body() {
    let input = full_input();
    let body = request::build(&input, Profile::Platform, &Reads::default()).expect("build");
    let expected = json!({
        "model": "gpt-5.5",
        "instructions": "You are the harness.\n\nPrefer small diffs.",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Read notes.txt"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Reading it."}]},
            {"type": "function_call", "call_id": "call_fixture_1", "name": "read", "arguments": "{\"path\":\"notes.txt\"}"},
            {
                "type": "function_call_output",
                "call_id": "call_fixture_1",
                "output": "status: succeeded\nprovenance: executor\nexit_code: 0\noutput_truncated: false\n\nfile contents",
            },
        ],
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "max_output_tokens": 512,
        "tools": [
            {
                "type": "function",
                "name": "read",
                "description": "Read a file from the workspace.",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}, "offset_bytes": {"type": "integer"}},
                    "required": ["path"],
                    "additionalProperties": false,
                },
            },
            {
                "type": "function",
                "name": "bash",
                "description": "Run one command.",
                "parameters": {
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"],
                    "additionalProperties": false,
                },
            },
        ],
        "tool_choice": "auto",
        "reasoning": {"effort": "medium", "summary": "concise"},
    });
    assert_eq!(body, expected);
}

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

/// The mirror image of the Anthropic case: a `toolu_…` id from the other
/// adapter travels verbatim, and anything OpenAI cannot take is replaced by one
/// deterministic identifier used by both the call and its output, unchanged on
/// every later request.
#[test]
fn a_foreign_call_id_stays_paired_across_a_provider_switch() {
    let (input, _) = tool_exchange(Some("toolu_fixture_1"));
    let body = request::build(&input, Profile::Platform, &Reads::default()).expect("foreign id");
    let call = body["input"]
        .as_array()
        .expect("input")
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("call");
    assert_eq!(call["call_id"], "toolu_fixture_1");
    let output = body["input"]
        .as_array()
        .expect("input")
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("output");
    assert_eq!(output["call_id"], "toolu_fixture_1");

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
            request::build(&input, Profile::Platform, &Reads::default()).expect("substituted id");
        let call = body["input"]
            .as_array()
            .expect("input")
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("call");
        let id = call["call_id"].as_str().expect("call_id").to_owned();
        assert!(
            !id.is_empty()
                && id.len() <= 64
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "unusable id {id:?} from {foreign:?}"
        );
        let output = body["input"]
            .as_array()
            .expect("input")
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .expect("output");
        assert_eq!(output["call_id"], call["call_id"], "result must pair");
        assert_eq!(id, harness);
        assert_eq!(
            request::build(&input, Profile::Platform, &Reads::default()).expect("replay"),
            body
        );
    }
}

#[test]
fn chatgpt_request_moves_instructions_into_input() {
    let mut input = full_input();
    input.model.model = super::LIVE_CHECK_MODEL.into();
    let body = request::build(&input, Profile::ChatGptCodex, &Reads::default()).expect("build");
    // The Codex client has no output-budget field; it always sends tool_choice
    // and parallel_tool_calls instead.
    assert!(body.get("max_output_tokens").is_none());
    assert_eq!(body["tool_choice"], json!("auto"));
    assert_eq!(body["parallel_tool_calls"], json!(true));
    assert!(body.get("instructions").is_none());
    assert_eq!(body["input"][0]["role"], json!("developer"));
    assert_eq!(
        body["input"][0]["content"][0]["text"],
        json!("You are the harness.\n\nPrefer small diffs.")
    );
    assert_eq!(body["store"], json!(false));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    // Everything after the leading instruction item is unchanged.
    assert_eq!(
        body["input"][1]["content"][0]["text"],
        json!("Read notes.txt")
    );
}

#[test]
fn image_input_needs_a_sourced_capability_and_serializes_as_a_data_url() {
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
    let body = request::build(&input, Profile::Platform, &reads).expect("build");
    let image = &body["input"][0]["content"][1];
    assert_eq!(image["type"], json!("input_image"));
    assert!(
        image["image_url"]
            .as_str()
            .expect("url")
            .starts_with("data:image/png;base64,")
    );

    let mut unknown = input.clone();
    unknown.model.model = "gpt-unknown".into();
    // Isolate the capability under test: drop controls an uncatalogued model
    // would also refuse, so the image check is what fails.
    unknown.options.reasoning_effort = None;
    unknown.options.provider_options.clear();
    let error = request::build(&unknown, Profile::Platform, &reads).expect_err("capability");
    assert_eq!(error.code, ModelErrorCode::UnsupportedCapability);
    assert!(error.message.contains("image"), "{}", error.message);
}

#[test]
fn provider_options_are_validated_by_name() {
    let mut input = full_input();
    input.options.provider_options =
        serde_json::from_value(json!({"openai": {"nonsense": 1}})).expect("options");
    let error = request::build(&input, Profile::Platform, &Reads::default()).expect_err("unknown");
    assert_eq!(error.code, ModelErrorCode::InvalidRequest);
    assert!(error.message.contains("nonsense"));

    input.options.provider_options =
        serde_json::from_value(json!({"anthropic": {"thinking": true}})).expect("options");
    let error = request::build(&input, Profile::Platform, &Reads::default()).expect_err("foreign");
    assert_eq!(error.code, ModelErrorCode::UnsupportedCapability);
    assert!(error.message.contains("anthropic"));

    input.options.provider_options =
        serde_json::from_value(json!({"openai": {"reasoning_summary": "verbose"}}))
            .expect("options");
    let error = request::build(&input, Profile::Platform, &Reads::default()).expect_err("mode");
    assert_eq!(error.code, ModelErrorCode::InvalidRequest);
}

#[test]
fn same_owner_reasoning_state_is_replayed_and_a_foreign_owner_is_not() {
    let payload =
        br#"{"id":"rs_fixture_1","type":"reasoning","encrypted_content":"gAAAA-fixture"}"#;
    let reference = ArtifactRef {
        sha256: Digest::of(payload),
        bytes: payload.len() as u64,
    };
    let mut input = full_input();
    input.items.push(ContextItem::Assistant {
        entry_id: EntryId::new(),
        outcome: harness_core::session::ResponseOutcome::Completed,
        content: vec![ContentBlock::ProviderState {
            owner: model(),
            schema: STATE_SCHEMA.into(),
            artifact: reference.clone(),
        }],
    });
    let reads = Reads::from_bytes(reference.clone(), payload.to_vec());
    let body = request::build(&input, Profile::Platform, &reads).expect("replay");
    let replayed = body["input"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["type"] == json!("reasoning"))
        .expect("replayed reasoning");
    assert_eq!(replayed["encrypted_content"], json!("gAAAA-fixture"));

    let mut foreign = input.clone();
    if let ContextItem::Assistant { content, .. } = foreign.items.last_mut().expect("assistant") {
        content[0] = ContentBlock::ProviderState {
            owner: ModelRef {
                provider_instance: "other".into(),
                model: "other".into(),
            },
            schema: STATE_SCHEMA.into(),
            artifact: reference,
        };
    }
    let body = request::build(&foreign, Profile::Platform, &reads).expect("skip");
    assert!(
        !body["input"]
            .as_array()
            .expect("items")
            .iter()
            .any(|item| item["type"] == json!("reasoning"))
    );
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

fn new_mapper(profile: Profile) -> (Mapper, Arc<super::Counters>) {
    let counters = Arc::new(super::Counters::default());
    (Mapper::new(profile, counters.clone()), counters)
}

#[test]
fn unknown_events_are_counted_and_never_fatal() {
    let (mut mapper, counters) = new_mapper(Profile::Platform);
    let emitted = mapper
        .push(&sse(
            "response.something_new",
            json!({"type": "response.something_new"}),
        ))
        .expect("ignored");
    assert!(emitted.is_empty());
    assert_eq!(counters.unknown_events(), 1);
    mapper
        .push(&sse(
            "response.output_text.done",
            json!({"type": "response.output_text.done", "item_id": "x", "text": "y"}),
        ))
        .expect("ignored");
    assert_eq!(counters.ignored_events(), 1);
}

#[test]
fn out_of_order_and_invalid_data_are_malformed_streams() {
    let (mut mapper, _) = new_mapper(Profile::Platform);
    assert_eq!(
        mapper
            .push(&sse(
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "item_id": "missing", "delta": "x"}),
            ))
            .expect_err("unknown item")
            .code,
        ModelErrorCode::MalformedStream
    );
    let (mut mapper, _) = new_mapper(Profile::Platform);
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
    let (mapper, _) = new_mapper(Profile::Platform);
    assert_eq!(
        mapper.finish_eof().expect_err("missing terminal").code,
        ModelErrorCode::MalformedStream
    );
}

#[test]
fn error_event_codes_are_classified_without_echoing_provider_text() {
    let (mut mapper, _) = new_mapper(Profile::Platform);
    let emitted = mapper
        .push(&sse(
            "error",
            json!({"type": "error", "code": "context_length_exceeded", "message": "secret body"}),
        ))
        .expect("error event");
    let Some(harness_core::model::ProviderEvent::Finished { error, .. }) = emitted.last() else {
        panic!("terminal failure");
    };
    let error = error.as_ref().expect("error");
    assert_eq!(error.code, ModelErrorCode::ContextOverflow);
    assert_eq!(
        error.provider_code.as_deref(),
        Some("context_length_exceeded")
    );
    assert!(!error.message.contains("secret body"));

    let (mut mapper, _) = new_mapper(Profile::Platform);
    let emitted = mapper
        .push(&sse(
            "response.failed",
            json!({
                "type": "response.failed",
                "response": {"error": {"code": "rate_limit_exceeded", "message": "slow down"}},
            }),
        ))
        .expect("failed");
    let Some(harness_core::model::ProviderEvent::Finished { error, .. }) = emitted.last() else {
        panic!("terminal failure");
    };
    assert_eq!(
        error.as_ref().expect("error").code,
        ModelErrorCode::RateLimit
    );
}

#[test]
fn content_filter_incompletion_is_a_refusal_outcome() {
    let (mut mapper, _) = new_mapper(Profile::Platform);
    let emitted = mapper
        .push(&sse(
            "response.incomplete",
            json!({
                "type": "response.incomplete",
                "response": {"incomplete_details": {"reason": "content_filter"}},
            }),
        ))
        .expect("incomplete");
    let Some(harness_core::model::ProviderEvent::Finished { finish_reason, .. }) = emitted.last()
    else {
        panic!("terminal");
    };
    assert_eq!(*finish_reason, Some(FinishReason::Refusal));
}

#[test]
fn chatgpt_authentication_failure_keeps_the_codex_login_hint() {
    let model_error = ModelError {
        code: ModelErrorCode::Authentication,
        message: "provider authentication failed".into(),
        provider_code: Some("http_401".into()),
        retry_after_ms: None,
    };
    let refined = super::refine_http(
        Profile::ChatGptCodex,
        model_error.clone(),
        br#"{"error":{"code":"token_expired","message":"expired"}}"#,
    );
    assert_eq!(refined.code, ModelErrorCode::Authentication);
    assert!(refined.message.contains("run `codex login`"));
    assert_eq!(refined.provider_code.as_deref(), Some("token_expired"));

    let platform = super::refine_http(Profile::Platform, model_error, b"{}");
    assert!(!platform.message.contains("codex login"));
}

// ------------------------------------------------------------- secrets

#[tokio::test]
async fn no_secret_reaches_errors_debug_or_fixtures() {
    let server = Server::start(parse_fixture(fixture_text("rate_limit")));
    let adapter = adapter(Profile::Platform, server.url.clone());
    let rendered = format!("{adapter:?}");
    assert!(!rendered.contains(SENTINEL_SECRET), "{rendered}");
    assert!(rendered.contains("[redacted]"));
    let request = tests_support::simple_request();
    let mut stream = adapter.stream(request, Default::default());
    let mut events = Vec::new();
    while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
        events.push(format!("{event:?}"));
    }
    for event in &events {
        assert!(!event.contains(SENTINEL_SECRET), "{event}");
    }
    let Some(body) = server.submitted.lock().expect("record").clone() else {
        panic!("the adapter submitted no request");
    };
    assert!(!body.to_string().contains(SENTINEL_SECRET));
    assert!(events.iter().any(|event| event.contains("RateLimit")));
}

#[tokio::test]
async fn a_missing_subscription_login_fails_with_the_codex_hint() {
    let adapter =
        OpenAiResponses::with_transport(Profile::ChatGptCodex, Arc::new(Missing), transport());
    let mut stream = adapter.stream(tests_support::simple_request(), Default::default());
    let event = futures_util::StreamExt::next(&mut stream)
        .await
        .expect("terminal");
    let harness_core::model::ProviderEvent::Finished { error, .. } = event else {
        panic!("terminal failure");
    };
    let error = error.expect("error");
    assert_eq!(error.code, ModelErrorCode::Authentication);
    assert!(error.message.contains("codex login"));
}

#[tokio::test]
async fn a_rate_limit_is_reported_once_without_a_retry() {
    let server = Arc::new(Server::start(parse_fixture(fixture_text("rate_limit"))));
    let adapter = adapter(Profile::Platform, server.url.clone());
    let mut stream = adapter.stream(tests_support::simple_request(), Default::default());
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
async fn finished_releases_connection_without_another_poll() {
    use harness_core::model::ProviderEvent;
    use std::time::Duration;

    for profile in [Profile::Platform, Profile::ChatGptCodex] {
        // Keep HTTP pending after the native terminal event: neither server
        // EOF nor a second consumer poll may mask a retained SSE reader.
        let mut reply = parse_fixture(fixture_text("text"));
        reply.hold = true;
        let server = Server::start(reply);
        let adapter = adapter(profile, server.url.clone());
        let row = case(CaseKind::Text, profile);
        let mut stream = adapter.stream(row.request, Default::default());
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
        // Do not poll or drop `stream` until after observing peer close.
        tokio::time::timeout(Duration::from_secs(2), async {
            while !server.released.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("Finished retained a live connection");
        drop(stream);
    }
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

#[test]
fn account_catalog_and_portable_reasoning_levels_are_sourced() {
    use harness_core::context::InputMediaKind;
    let info = super::info(Profile::ChatGptCodex, super::LIVE_CHECK_MODEL).expect("account model");
    assert_eq!(info.limits.context_tokens, Some(500_000));
    assert_eq!(info.limits.max_output_tokens, None);
    assert!(
        info.capabilities
            .input_media
            .contains(&InputMediaKind::Image)
    );
    assert!(info.capabilities.tools);
    assert!(super::info(Profile::ChatGptCodex, "gpt-5.5").is_none());
    assert_eq!(
        super::info(Profile::Platform, "gpt-5.5")
            .expect("platform")
            .limits
            .max_output_tokens,
        Some(128_000)
    );
    for (portable, wire) in [
        (ReasoningEffort::Low, "low"),
        (ReasoningEffort::Medium, "medium"),
        (ReasoningEffort::High, "high"),
    ] {
        let mut input = full_input();
        input.model.model = super::LIVE_CHECK_MODEL.into();
        input.options.reasoning_effort = Some(portable);
        let body = request::build(&input, Profile::ChatGptCodex, &Reads::default()).expect("body");
        assert_eq!(body["reasoning"]["effort"], json!(wire));
        assert!(body.get("max_output_tokens").is_none());
    }
}

#[tokio::test]
async fn expired_deadline_wins_before_ready_work_is_polled() {
    use harness_core::model::OperationControl;
    for _ in 0..64 {
        let polled = AtomicBool::new(false);
        let control = OperationControl {
            deadline: Some(tokio::time::Instant::now()),
            ..Default::default()
        };
        let result = crate::transport::controlled(&control, async {
            polled.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(result.expect_err("expired operation ran").is_stopped());
        assert!(!polled.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn missing_content_type_streams_successfully() {
    let mut reply = parse_fixture(fixture_text("text"));
    reply.headers.clear();
    let server = Server::start(reply);
    let adapter = adapter(Profile::ChatGptCodex, server.url.clone());
    let mut stream = adapter.stream(
        case(CaseKind::Text, Profile::ChatGptCodex).request,
        Default::default(),
    );
    let mut text = String::new();
    let mut completed = false;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(event) = futures_util::StreamExt::next(&mut stream).await {
            match event {
                harness_core::model::ProviderEvent::TextDelta { text: delta, .. } => {
                    text.push_str(&delta)
                }
                harness_core::model::ProviderEvent::Finished {
                    outcome,
                    finish_reason,
                    ..
                } => {
                    assert_eq!(outcome, harness_core::model::ProviderOutcome::Completed);
                    assert_eq!(finish_reason, Some(FinishReason::Stop));
                    completed = true;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("stream completed");
    assert!(completed);
    assert_eq!(text, "Hello world");
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn explicit_wrong_content_type_reaches_finished_as_malformed_stream() {
    for content_type in ["application/json", "text/html"] {
        let mut reply = parse_fixture(fixture_text("text"));
        reply.headers = vec![("content-type".into(), content_type.into())];
        let server = Server::start(reply);
        let adapter = adapter(Profile::ChatGptCodex, server.url.clone());
        let mut stream = adapter.stream(
            case(CaseKind::Text, Profile::ChatGptCodex).request,
            Default::default(),
        );
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            futures_util::StreamExt::next(&mut stream),
        )
        .await
        .expect("terminal event")
        .expect("event");
        let harness_core::model::ProviderEvent::Finished {
            outcome,
            error: Some(error),
            ..
        } = event
        else {
            panic!("expected terminal error");
        };
        assert_eq!(outcome, harness_core::model::ProviderOutcome::Failed);
        assert_eq!(error.code, ModelErrorCode::MalformedStream);
        assert_eq!(error.message, "expected text/event-stream");
        assert_eq!(error.provider_code, None);
        assert!(futures_util::StreamExt::next(&mut stream).await.is_none());
    }
}

#[test]
fn non_http_transport_errors_keep_their_original_code_and_message() {
    for profile in [Profile::Platform, Profile::ChatGptCodex] {
        for (code, message) in [
            (ModelErrorCode::MalformedStream, "truncated SSE event"),
            (ModelErrorCode::LimitExceeded, "SSE byte limit exceeded"),
            (ModelErrorCode::Transport, "HTTP transport failed"),
        ] {
            let transport_error = crate::error::TransportError::new(code, message);
            let original = transport_error.model_error();
            let event = super::transport_terminal(profile, transport_error);
            let harness_core::model::ProviderEvent::Finished {
                outcome,
                error: Some(error),
                ..
            } = event
            else {
                panic!("expected terminal error");
            };
            assert_eq!(outcome, harness_core::model::ProviderOutcome::Failed);
            assert_eq!(error, original);
        }
    }
}

#[test]
fn sanitized_recorded_codex_stream_maps_to_completed_text_and_usage() {
    use harness_core::model::{ProviderEvent, ProviderOutcome};
    let fixture = include_str!("../../fixtures/openai/recorded-codex-text.sse");
    assert!(fixture.starts_with("# RECORDED:"));
    let reply = parse_fixture(fixture);
    let body = std::str::from_utf8(&reply.body).expect("fixture UTF-8");
    let (mut mapper, _) = new_mapper(Profile::ChatGptCodex);
    let mut emitted = Vec::new();
    for frame in body.split("\n\n").filter(|frame| !frame.is_empty()) {
        let data = frame
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("native event data");
        emitted.extend(
            mapper
                .push(&SseEvent {
                    data: data.into(),
                    ..Default::default()
                })
                .expect("recorded event maps"),
        );
    }
    assert!(mapper.finished());
    assert!(
        mapper
            .finish_eof()
            .expect("native terminal event")
            .is_empty()
    );
    assert_eq!(
        emitted,
        vec![
            ProviderEvent::Started,
            ProviderEvent::TextDelta {
                block_index: 0,
                text: "[recorded text]".into()
            },
            // Synthetic counts in the sanitized fixture preserve only availability.
            ProviderEvent::UsageUpdated {
                usage: Usage {
                    input_tokens: Some(1),
                    output_tokens: Some(1),
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                }
            },
            ProviderEvent::Finished {
                outcome: ProviderOutcome::Completed,
                finish_reason: Some(FinishReason::Stop),
                error: None,
                continuation: None,
            },
        ]
    );
}

// --------------------------------------------------------- test-only helpers

/// Helpers used by the tests above; kept here so the module stays self-contained.
pub(crate) mod tests_support {
    use super::*;

    /// A minimal valid request over the catalogued live-check model.
    pub(crate) fn simple_request() -> harness_core::model::ModelRequest {
        harness_core::model::ModelRequest {
            response_id: ResponseId::new(),
            artifacts: ArtifactSource::default(),
            input: ModelInput {
                model: model(),
                items: vec![user("Hello")],
                tools: Vec::new(),
                options: options(128),
                continuation: None,
            },
        }
    }
}

#[test]
fn unknown_error_codes_are_sensitive_in_http_and_streams() {
    let sentinel = "SENTINEL-CREDENTIAL-ERROR-CODE";
    let original = ModelError {
        code: ModelErrorCode::ProviderFailure,
        message: "safe".into(),
        provider_code: Some("http_500".into()),
        retry_after_ms: None,
    };
    let body = serde_json::to_vec(&json!({"error":{"code":sentinel,"message":sentinel}})).unwrap();
    for profile in [Profile::Platform, Profile::ChatGptCodex] {
        let model = super::refine_http(profile, original.clone(), &body);
        assert_eq!(model.provider_code.as_deref(), Some("http_500"));
        assert!(
            !serde_json::to_string(&model).unwrap().contains(sentinel),
            "credential leaked"
        );
    }
}
