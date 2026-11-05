use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use credentials::{Credential, CredentialSource};
use futures_util::StreamExt;
use harness_core::{
    model::{
        ArtifactSource, InferenceOptions, ModelInput, ModelProvider, ModelRequest,
        OperationControl, ProviderEvent, ProviderOutcome,
        conformance::{
            self, Case, CaseKind, Expected, Fixture, Observation, SubmittedRequest, SubmittedTool,
        },
    },
    session::{FinishReason, ModelErrorCode, ModelRef, ResponseId},
};
use reqwest::header::HeaderMap;
use router::Router;
use serde_json::{Value, json};
use sse::SseConfig;
use std::{
    collections::BTreeMap,
    future::poll_fn,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use transport::{Transport, TransportConfig};

#[allow(dead_code)]
#[path = "../../../tests/support/loopback.rs"]
mod loopback;

fn client_nonce() -> &'static str {
    static NONCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NONCE.get_or_init(loopback::nonce)
}

const SECRET: &str = "SENTINEL-DO-NOT-DISCLOSE-987654321";
fn credential() -> Credential {
    let mut headers = HeaderMap::new();
    headers.insert("x-harness-test", client_nonce().parse().unwrap());
    Credential::bearer(SECRET.into(), headers, None).unwrap()
}
fn transport() -> Transport {
    Transport::new(TransportConfig {
        allow_loopback_http: true,
        ..Default::default()
    })
    .unwrap()
}
fn request() -> ModelRequest {
    ModelRequest {
        response_id: ResponseId::try_from("019a0000-0000-7000-8000-000000000001".to_owned())
            .unwrap(),
        input: ModelInput {
            model: ModelRef {
                provider_instance: "echo".into(),
                model: "echo".into(),
            },
            items: vec![],
            tools: vec![],
            options: InferenceOptions {
                max_output_tokens: 100,
                reasoning_effort: None,
                provider_options: Default::default(),
            },
            continuation: None,
        },
        artifacts: ArtifactSource::default(),
    }
}
// A random run header identifies our client; each listener also has its own
// random path. The header is shared so redirected POSTs remain attributable at
// the target (a target-specific header would hide a redirect regression).
async fn matching_request(socket: &mut tokio::net::TcpStream, path: &str, probe: &Probe) -> bool {
    let mut input = vec![];
    let mut buf = [0; 4096];
    loop {
        let n = socket.read(&mut buf).await.unwrap_or(0);
        if n == 0 || input.len() + n > 1024 * 1024 {
            return false;
        }
        input.extend_from_slice(&buf[..n]);
        let Some(end) = input.windows(4).position(|v| v == b"\r\n\r\n") else {
            continue;
        };
        let raw_header = String::from_utf8_lossy(&input[..end]);
        let header = raw_header.to_ascii_lowercase();
        if raw_header.lines().next() != Some(format!("POST {path} HTTP/1.1").as_str())
            || !header
                .lines()
                .any(|line| line == format!("x-harness-test: {}", client_nonce()))
        {
            return false;
        }
        let length: usize = header
            .lines()
            .find_map(|line| line.strip_prefix("content-length: ")?.parse().ok())
            .unwrap_or(0);
        if length > 1024 * 1024 {
            return false;
        }
        if input.len() < end + 4 + length {
            continue;
        }
        // The transport sends `harness/<crate version>` (transport.rs); derive it
        // so a version bump cannot silently weaken this assertion.
        let user_agent = concat!("user-agent: harness/", env!("CARGO_PKG_VERSION"));
        probe.headers_ok.store(
            header.contains("authorization: bearer sentinel-do-not-disclose-987654321")
                && header.contains(user_agent),
            Ordering::SeqCst,
        );
        *probe.submitted.lock().unwrap() =
            serde_json::from_slice(&input[end + 4..end + 4 + length]).ok();
        return true;
    }
}

#[derive(Default)]
struct Probe {
    ignored: AtomicUsize,
    active: AtomicUsize,
    accepted: AtomicUsize,
    closed: AtomicBool,
    submitted: Mutex<Option<Value>>,
    headers_ok: AtomicBool,
}
struct Fake {
    endpoint: String,
    probe: Arc<Probe>,
    task: tokio::task::JoinHandle<()>,
    _side_probe: loopback::SideProbe,
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fake {
    async fn new(status: u16, headers: &str, body: Vec<u8>, hold: bool, setup: bool) -> Self {
        Self::at(
            "127.0.0.1:0",
            status,
            headers,
            body,
            hold,
            setup,
            "text/event-stream",
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn at(
        address: &str,
        status: u16,
        headers: &str,
        body: Vec<u8>,
        hold: bool,
        setup: bool,
        content_type: &str,
    ) -> Self {
        let listener = TcpListener::bind(address).await.unwrap();
        let side_probe = loopback::SideProbe::register(listener.local_addr().unwrap());
        let path = format!("/echo/{}", loopback::nonce());
        let endpoint = format!("http://{}{path}", listener.local_addr().unwrap());
        let content_type = content_type.to_owned();
        let probe = Arc::new(Probe::default());
        let p = probe.clone();
        let headers = headers.to_string();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = tokio::select! {
                    accepted = listener.accept() => accepted.unwrap(),
                    Some(result) = connections.join_next(), if !connections.is_empty() => {
                        result.unwrap();
                        continue;
                    }
                };
                let p = p.clone();
                let path = path.clone();
                let headers = headers.clone();
                let body = body.clone();
                let content_type = content_type.clone();
                connections.spawn(async move {
                let mut buf = [0; 4096];
                let matched = tokio::time::timeout(Duration::from_secs(1), matching_request(&mut socket, &path, &p)).await.unwrap_or(false);
                if !matched {
                    let ignored = p.ignored.fetch_add(1, Ordering::SeqCst) + 1;
                    eprintln!("fake: ignored unrelated connection #{ignored}");
                    return;
                }
                p.accepted.fetch_add(1, Ordering::SeqCst);
                p.active.fetch_add(1, Ordering::SeqCst);
                if !setup && status != 0 {
                    let response = format!(
                        "HTTP/1.1 {status} Fake\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n{headers}\r\n"
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    if !body.is_empty() {
                        let _ = socket
                            .write_all(format!("{:x}\r\n", body.len()).as_bytes())
                            .await;
                        let _ = socket.write_all(&body).await;
                        let _ = socket.write_all(b"\r\n").await;
                    }
                    if !hold {
                        let _ = socket.write_all(b"0\r\n\r\n").await;
                    }
                }
                if hold || setup {
                    loop {
                        if socket.read(&mut buf).await.unwrap_or(0) == 0 {
                            break;
                        }
                    }
                }
                drop(socket);
                p.active.fetch_sub(1, Ordering::SeqCst);
                p.closed.store(true, Ordering::SeqCst);
                });
            }
        });
        Self {
            endpoint,
            probe,
            task,
            _side_probe: side_probe,
        }
    }
    async fn sse(body: &str) -> Self {
        Self::new(200, "", body.as_bytes().to_vec(), false, false).await
    }
    async fn closed(&self) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !self.probe.closed.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("connection was not closed");
        assert_eq!(self.probe.active.load(Ordering::SeqCst), 0);
    }
}

/// Test-only adapter: a deliberately trivial vocabulary, not any provider API.
struct EchoSseProvider {
    transport: Transport,
    endpoint: String,
    bounds: SseConfig,
}
impl ModelProvider for EchoSseProvider {
    fn stream(
        &self,
        request: ModelRequest,
        control: OperationControl,
    ) -> harness_core::model::ModelEventStream {
        let t = self.transport.clone();
        let endpoint = self.endpoint.clone();
        let tools: Vec<_> = request.input.tools.iter().map(|t| json!({"name":t.name,"description":t.description,"input_schema":t.input_schema})).collect();
        let body = json!({"tools":tools,"options":request.input.options});
        let c = control.clone();
        let mut stream = sse::read(
            async move { t.post(&endpoint, &body, credential(), c).await },
            self.bounds.clone(),
            control,
        );
        Box::pin(async_stream::stream! {
            let mut started = false;
            while let Some(event) = stream.next().await {
                match event {
                    Err(e) => { drop(stream); yield terminal(if e.is_stopped() { ProviderOutcome::Cancelled } else { ProviderOutcome::Failed }, None, (!e.is_stopped()).then(|| e.model_error())); return; }
                    Ok(event) => match event.event.as_deref() {
                        Some("start") if !started => { started = true; yield ProviderEvent::Started; }
                        Some("text") if started => yield ProviderEvent::TextDelta { block_index: 0, text: event.data },
                        Some("finish") if started => { drop(stream); yield terminal(ProviderOutcome::Completed, Some(match event.data.as_str() { "refusal" => FinishReason::Refusal, "output_limit" => FinishReason::OutputLimit, _ => FinishReason::Stop }), None); return; }
                        _ => { drop(stream); yield terminal(ProviderOutcome::Failed, None, Some(error::TransportError::new(ModelErrorCode::MalformedStream, "invalid echo event").model_error())); return; }
                    }
                }
            }
            drop(stream);
            yield terminal(ProviderOutcome::Failed, None, Some(error::TransportError::new(ModelErrorCode::MalformedStream, "truncated echo response").model_error()));
        })
    }
}
fn terminal(
    outcome: ProviderOutcome,
    finish_reason: Option<FinishReason>,
    error: Option<harness_core::session::ModelError>,
) -> ProviderEvent {
    ProviderEvent::Finished {
        outcome,
        finish_reason,
        error,
        continuation: None,
    }
}
fn echo(fake: &Fake) -> EchoSseProvider {
    EchoSseProvider {
        transport: transport(),
        endpoint: fake.endpoint.clone(),
        bounds: Default::default(),
    }
}
fn read(fake: &Fake, config: SseConfig, control: OperationControl) -> sse::SseStream {
    let t = transport();
    let endpoint = fake.endpoint.clone();
    let c = control.clone();
    sse::read(
        async move { t.post(&endpoint, &json!({}), credential(), c).await },
        config,
        control,
    )
}
async fn code(fake: &Fake) -> error::TransportError {
    transport()
        .post(
            &fake.endpoint,
            &json!({}),
            credential(),
            OperationControl::default(),
        )
        .await
        .err()
        .expect("expected HTTP failure")
}
#[tokio::test]
async fn unrelated_connections_do_not_touch_client_probes() {
    let fake = Fake::sse("data: yes\n\n").await;
    let url = reqwest::Url::parse(&fake.endpoint).unwrap();
    let address = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
    // Isolate each part of the matcher, plus a silent connection. None may
    // set accepted, active, closed, submitted, or headers_ok.
    let mut idle = tokio::net::TcpStream::connect(&address).await.unwrap();
    for (method, path, marker) in [
        ("GET", url.path(), client_nonce()),
        ("POST", "/", client_nonce()),
        ("POST", url.path(), "wrong"),
    ] {
        let mut socket = tokio::net::TcpStream::connect(&address).await.unwrap();
        socket.write_all(format!("{method} {path} HTTP/1.1\r\nx-harness-test: {marker}\r\nContent-Length: 2\r\n\r\n{{}}").as_bytes()).await.unwrap();
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
    assert_eq!(fake.probe.accepted.load(Ordering::SeqCst), 0);
    assert_eq!(fake.probe.active.load(Ordering::SeqCst), 0);
    assert!(!fake.probe.closed.load(Ordering::SeqCst));
    assert!(!fake.probe.headers_ok.load(Ordering::SeqCst));
    assert!(fake.probe.submitted.lock().unwrap().is_none());
    assert!(fake.probe.ignored.load(Ordering::SeqCst) >= 3);
    // The idle peer must not block a real request, and is bounded itself.
    let mut stream = transport()
        .post(&fake.endpoint, &json!({}), credential(), Default::default())
        .await
        .unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap(), b"data: yes\n\n");
    fake.closed().await;
    assert_eq!(fake.probe.accepted.load(Ordering::SeqCst), 1);
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), idle.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn invalid_bounds_never_open_transport() {
    let fake = Fake::sse("data: unused\n\n").await;
    for config in [
        SseConfig {
            max_event_bytes: 0,
            ..Default::default()
        },
        SseConfig {
            max_queue_bytes: 1,
            ..Default::default()
        },
        SseConfig {
            max_queue_events: 0,
            ..Default::default()
        },
        SseConfig {
            max_queue_events: usize::MAX,
            ..Default::default()
        },
        SseConfig {
            max_queue_bytes: usize::MAX,
            ..Default::default()
        },
    ] {
        let error = read(&fake, config, Default::default())
            .next()
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.model_error().code, ModelErrorCode::InvalidRequest);
    }
    assert_eq!(fake.probe.accepted.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn wrong_content_type_and_interrupted_echo() {
    let fake = Fake::at(
        "127.0.0.1:0",
        200,
        "",
        b"{}".to_vec(),
        false,
        false,
        "application/json",
    )
    .await;
    let error = transport()
        .post(&fake.endpoint, &json!({}), credential(), Default::default())
        .await
        .err()
        .unwrap();
    assert_eq!(error.model_error().code, ModelErrorCode::MalformedStream);
    fake.closed().await;
    let fake = Fake::sse("event: start\ndata: yes\n\nevent: text\ndata: partial\n\n").await;
    let events: Vec<_> = echo(&fake)
        .stream(request(), Default::default())
        .collect()
        .await;
    assert!(
        matches!(events.last(), Some(ProviderEvent::Finished { outcome: ProviderOutcome::Failed, error: Some(e), .. }) if e.code == ModelErrorCode::MalformedStream)
    );
    let fake = Fake::new(
        200,
        "",
        format!(
            "event: start\ndata: yes\n\n{}",
            "event: text\ndata: x\n\n".repeat(20)
        )
        .into_bytes(),
        true,
        false,
    )
    .await;
    let mut provider = echo(&fake);
    provider.bounds.max_response_bytes = 80;
    let events: Vec<_> = provider
        .stream(request(), Default::default())
        .collect()
        .await;
    assert!(
        matches!(events.last(), Some(ProviderEvent::Finished { error: Some(e), .. }) if e.code == ModelErrorCode::LimitExceeded)
    );
    fake.closed().await;
}
#[test]
fn codex_bounded_files_and_no_symlink_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    login_fixture(&path, u32::MAX as u64);
    let symlink = dir.path().join("link.json");
    std::os::unix::fs::symlink(&path, &symlink).unwrap();
    no_secret(
        credentials::CodexLogin::from_path(symlink)
            .credential()
            .unwrap_err(),
    );
    std::fs::write(&path, vec![b'x'; 65537]).unwrap();
    no_secret(
        credentials::CodexLogin::from_path(&path)
            .credential()
            .unwrap_err(),
    );
    for value in [
        json!({"tokens":{"access_token":"a.!!!!.c","account_id":"fixture"}}),
        json!({"tokens":{"access_token":"a.e30.c"}}),
    ] {
        std::fs::write(&path, value.to_string()).unwrap();
        no_secret(
            credentials::CodexLogin::from_path(&path)
                .credential()
                .unwrap_err(),
        );
    }
}
fn no_secret(value: impl std::fmt::Debug) {
    let captured = format!("{value:?}");
    assert!(
        !captured.contains(SECRET),
        "secret leaked in captured diagnostics"
    );
}

// ------------------------------------------------------------------- router

fn routed(request: ModelRequest) -> ModelRequest {
    request
}

/// Dispatch is by `provider_instance`, never by the model name or by which
/// adapter happens to be first: the same model string reaches two different
/// servers, and a model name that equals another instance's name does not
/// change where the request goes.
#[tokio::test]
async fn router_dispatches_by_provider_instance_not_model_name() {
    let first = Fake::sse(
        "event: start\ndata: yes\n\nevent: text\ndata: one\n\nevent: finish\ndata: stop\n\n",
    )
    .await;
    let second = Fake::sse(
        "event: start\ndata: yes\n\nevent: text\ndata: two\n\nevent: finish\ndata: stop\n\n",
    )
    .await;
    let router = Router::new()
        .with("echo", Arc::new(echo(&first)))
        .with("other", Arc::new(echo(&second)));
    assert!(router.contains("echo") && router.contains("other"));
    assert!(!router.contains("missing"));
    assert_eq!(router.instances().collect::<Vec<_>>(), ["echo", "other"]);

    let mut to_first = routed(request());
    to_first.input.model.model = "other".into();
    let text: Vec<_> = router
        .stream(to_first, OperationControl::default())
        .filter_map(|event| async move {
            match event {
                ProviderEvent::TextDelta { text, .. } => Some(text),
                _ => None,
            }
        })
        .collect()
        .await;
    assert_eq!(text, ["one"]);
    assert_eq!(first.probe.accepted.load(Ordering::SeqCst), 1);
    assert_eq!(second.probe.accepted.load(Ordering::SeqCst), 0);

    let mut to_second = routed(request());
    to_second.input.model.provider_instance = "other".into();
    to_second.input.model.model = "echo".into();
    let text: Vec<_> = router
        .stream(to_second, OperationControl::default())
        .filter_map(|event| async move {
            match event {
                ProviderEvent::TextDelta { text, .. } => Some(text),
                _ => None,
            }
        })
        .collect()
        .await;
    assert_eq!(text, ["two"]);
    assert_eq!(first.probe.accepted.load(Ordering::SeqCst), 1);
    assert_eq!(second.probe.accepted.load(Ordering::SeqCst), 1);
    first.closed().await;
    second.closed().await;
}

/// An unregistered instance ends in one terminal `invalid_request` event and
/// never reaches any registered adapter's transport.
#[tokio::test]
async fn router_unknown_instance_fails_without_a_network_call() {
    let fake = Fake::sse("event: start\ndata: yes\n\nevent: finish\ndata: stop\n\n").await;
    let router = Router::new().with("echo", Arc::new(echo(&fake)));
    let mut input = routed(request());
    input.input.model.provider_instance = "missing-instance".into();
    let events: Vec<_> = router
        .stream(input, OperationControl::default())
        .collect()
        .await;
    assert_eq!(events.len(), 1, "one terminal event, no start");
    let ProviderEvent::Finished {
        outcome,
        finish_reason,
        error: Some(error),
        continuation,
    } = &events[0]
    else {
        panic!("expected one terminal failure")
    };
    assert_eq!(*outcome, ProviderOutcome::Failed);
    assert_eq!(*finish_reason, None);
    assert_eq!(continuation, &None);
    assert_eq!(error.code, ModelErrorCode::InvalidRequest);
    assert!(
        !error.message.contains("missing-instance"),
        "the diagnostic names no request detail"
    );
    assert_eq!(fake.probe.accepted.load(Ordering::SeqCst), 0);
    assert_eq!(fake.probe.ignored.load(Ordering::SeqCst), 0);
    // An empty instance name can never be registered or reached.
    let router = Router::new().with("", Arc::new(echo(&fake)));
    assert!(!router.contains(""));
    assert_eq!(router.instances().count(), 0);
}

/// Cancellation, deadlines and drop reach the delegate unchanged: the router
/// adds no wrapper task that could outlive the adapter's own stream.
#[tokio::test]
async fn router_passes_cancel_deadline_and_drop_through_unchanged() {
    for mode in ["cancel", "deadline", "drop"] {
        let fake = Fake::new(
            200,
            "",
            b"event: start\ndata: yes\n\nevent: text\ndata: x\n\n".to_vec(),
            true,
            false,
        )
        .await;
        let router = Router::new().with("echo", Arc::new(echo(&fake)));
        let control = OperationControl {
            deadline: (mode == "deadline")
                .then(|| tokio::time::Instant::now() + Duration::from_millis(40)),
            ..Default::default()
        };
        let mut stream = router.stream(request(), control.clone());
        assert_eq!(stream.next().await, Some(ProviderEvent::Started));
        if mode == "cancel" {
            control.cancel.cancel();
            let terminal = loop {
                match stream.next().await {
                    Some(event) => {
                        if matches!(event, ProviderEvent::Finished { .. }) {
                            break event;
                        }
                    }
                    None => panic!("stream ended without a terminal event"),
                }
            };
            assert!(matches!(
                terminal,
                ProviderEvent::Finished {
                    outcome: ProviderOutcome::Cancelled,
                    ..
                }
            ));
        }
        if mode == "deadline" {
            let terminal = loop {
                match stream.next().await {
                    Some(event) if matches!(event, ProviderEvent::Finished { .. }) => break event,
                    Some(_) => {}
                    None => panic!("no terminal event"),
                }
            };
            assert!(matches!(
                terminal,
                ProviderEvent::Finished {
                    outcome: ProviderOutcome::Cancelled,
                    ..
                }
            ));
        }
        // Both the cooperative paths and drop must close the connection: the
        // router returned the adapter's own stream, so its release is intact.
        drop(stream);
        fake.closed().await;
    }
}

#[tokio::test]
async fn sse_fields_and_fragmented_line_endings() {
    let body = "\u{feff}:comment\r\nevent: token\rid: one\nretry: 15\r\ndata: α\r\ndata: β\r\n\r\n";
    // Exercise every byte boundary, including UTF-8, BOM and CRLF splits.
    for split in 0..=body.len() {
        let bytes = body.as_bytes().to_vec();
        let stream: transport::ByteStream = Box::pin(
            async_stream::stream! { yield Ok(bytes[..split].to_vec()); yield Ok(bytes[split..].to_vec()); },
        );
        let mut reader = sse::read(async { Ok(stream) }, Default::default(), Default::default());
        let event = reader.next().await.unwrap().unwrap();
        assert_eq!(
            event,
            sse::SseEvent {
                event: Some("token".into()),
                id: Some("one".into()),
                retry: Some(15),
                data: "α\nβ".into()
            }
        );
        assert!(reader.next().await.is_none());
    }
}
#[tokio::test]
async fn sse_invalid_utf8_truncation_and_metadata() {
    for body in [
        b"data: \xff\n\n".to_vec(),
        b"data: unfinished".to_vec(),
        b"data: unfinished\n".to_vec(),
        b":comment".to_vec(),
    ] {
        let fake = Fake::new(200, "", body, false, false).await;
        let error = read(&fake, Default::default(), Default::default())
            .next()
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.model_error().code, ModelErrorCode::MalformedStream);
    }
    let fake = Fake::sse(":ignore\nunknown: x\nid: bad\0id\nretry: -5\ndata\n\n").await;
    let event = read(&fake, Default::default(), Default::default())
        .next()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.data, "");
    assert_eq!(event.id, None);
    assert_eq!(event.retry, None);
}
#[tokio::test]
async fn oversized_event() {
    let fake = Fake::new(
        200,
        "",
        format!("event: start\ndata: {}\n\n", "x".repeat(256 * 1024)).into_bytes(),
        true,
        false,
    )
    .await;
    let mut stream = echo(&fake).stream(request(), Default::default());
    assert!(
        matches!(stream.next().await, Some(ProviderEvent::Finished { error: Some(e), .. }) if e.code == ModelErrorCode::LimitExceeded)
    );
    fake.closed().await;
}
#[tokio::test]
async fn response_bound_and_conservative_queue_backpressure() {
    let fake = Fake::sse(&"data: x\n\n".repeat(20)).await;
    let mut stream = read(
        &fake,
        SseConfig {
            max_response_bytes: 64,
            ..Default::default()
        },
        Default::default(),
    );
    let mut count = 0;
    while let Some(event) = stream.next().await {
        match event {
            Ok(_) => count += 1,
            Err(e) => {
                assert_eq!(e.model_error().code, ModelErrorCode::LimitExceeded);
                break;
            }
        }
    }
    assert!(count > 0 && count < 20);
    // A custom source records pulls. With one reserved slot and one event budget,
    // the worker cannot read a third chunk while the second event is queued.
    let pulls = Arc::new(AtomicUsize::new(0));
    let p = pulls.clone();
    let bytes: transport::ByteStream = Box::pin(
        async_stream::stream! { for _ in 0..20 { p.fetch_add(1, Ordering::SeqCst); yield Ok(b"data: x\n\n".to_vec()); } },
    );
    let mut reader = sse::read(
        async { Ok(bytes) },
        SseConfig {
            max_event_bytes: 32,
            max_queue_bytes: 32,
            max_queue_events: 1,
            ..Default::default()
        },
        Default::default(),
    );
    assert!(reader.next().await.unwrap().is_ok());
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(pulls.load(Ordering::SeqCst), 2);
    drop(reader);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(pulls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn cancel_deadline_drop_close_connection() {
    for mode in [
        "cancel",
        "deadline",
        "drop",
        "cancel_paused",
        "deadline_paused",
    ] {
        let fake = Fake::new(
            200,
            "",
            b"event: start\ndata: yes\n\n".to_vec(),
            true,
            false,
        )
        .await;
        let control = OperationControl {
            deadline: mode
                .starts_with("deadline")
                .then(|| tokio::time::Instant::now() + Duration::from_millis(100)),
            ..Default::default()
        };
        let mut stream = echo(&fake).stream(request(), control.clone());
        assert_eq!(stream.next().await, Some(ProviderEvent::Started));
        match mode {
            "drop" => drop(stream),
            "cancel_paused" => {
                control.cancel.cancel();
                fake.closed().await;
                drop(stream);
            }
            "deadline_paused" => {
                fake.closed().await;
                drop(stream);
            }
            _ => {
                if mode == "cancel" {
                    control.cancel.cancel();
                }
                assert!(matches!(
                    stream.next().await,
                    Some(ProviderEvent::Finished {
                        outcome: ProviderOutcome::Cancelled,
                        ..
                    })
                ));
                assert!(stream.next().await.is_none());
            }
        }
        fake.closed().await;
    }
}
#[tokio::test]
async fn setup_cancel_deadline_first_byte_timeout() {
    for mode in ["cancel", "deadline", "drop", "timeout", "timeout_headers"] {
        let fake = Fake::new(200, "", vec![], true, mode != "timeout_headers").await;
        let mut t = transport();
        if mode.starts_with("timeout") {
            t = Transport::new(TransportConfig {
                allow_loopback_http: true,
                first_byte_timeout: Duration::from_millis(60),
                ..Default::default()
            })
            .unwrap();
        }
        let control = OperationControl {
            deadline: (mode == "deadline")
                .then(|| tokio::time::Instant::now() + Duration::from_millis(60)),
            ..Default::default()
        };
        let endpoint = fake.endpoint.clone();
        let c = control.clone();
        let mut stream = sse::read(
            async move { t.post(&endpoint, &json!({}), credential(), c).await },
            Default::default(),
            control.clone(),
        );
        assert!(
            poll_fn(|cx| std::task::Poll::Ready(stream.as_mut().poll_next(cx)))
                .await
                .is_pending()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while fake.probe.submitted.lock().unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if mode == "drop" {
            drop(stream);
            fake.closed().await;
            continue;
        }
        if mode == "cancel" {
            control.cancel.cancel();
        }
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.is_stopped(), !mode.starts_with("timeout"));
        fake.closed().await;
    }
}
#[tokio::test]
async fn http_error_taxonomy_retry_and_redaction() {
    for (status, body, expected) in [
        (401, "", ModelErrorCode::Authentication),
        (403, "", ModelErrorCode::Authentication),
        (400, "", ModelErrorCode::InvalidRequest),
        (404, "", ModelErrorCode::InvalidRequest),
        (422, "", ModelErrorCode::InvalidRequest),
        (413, "", ModelErrorCode::ContextOverflow),
        (
            400,
            "context_length_exceeded",
            ModelErrorCode::ContextOverflow,
        ),
        (429, "", ModelErrorCode::RateLimit),
        (500, "", ModelErrorCode::ProviderFailure),
        (502, "", ModelErrorCode::ProviderFailure),
        (503, "", ModelErrorCode::ProviderFailure),
        (504, "", ModelErrorCode::ProviderFailure),
        (418, "overloaded", ModelErrorCode::ProviderFailure),
    ] {
        let fake = Fake::new(
            status,
            "retry-after-ms: 123\r\n",
            format!("{SECRET} {body}").into_bytes(),
            false,
            false,
        )
        .await;
        let e = code(&fake).await;
        assert_eq!(e.model_error().code, expected);
        assert_eq!(e.model_error().retry_after_ms, Some(123));
        assert_eq!(
            e.model_error().provider_code,
            Some(format!("http_{status}"))
        );
        no_secret(&e);
        no_secret(e.to_string());
        no_secret(e.model_error());
        assert!(fake.probe.headers_ok.load(Ordering::SeqCst));
        assert_eq!(fake.probe.accepted.load(Ordering::SeqCst), 1);
        let mut stream = echo(&fake).stream(request(), Default::default());
        assert!(
            matches!(stream.next().await, Some(ProviderEvent::Finished { error: Some(e), .. }) if e.code == expected)
        );
        assert!(stream.next().await.is_none());
        assert_eq!(fake.probe.accepted.load(Ordering::SeqCst), 2);
    }
    for (header, expected) in [
        ("Retry-After: 2\r\n".to_string(), Some(2000)),
        (
            format!(
                "Retry-After: {}\r\n",
                httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(60))
            ),
            None,
        ),
    ] {
        let fake = Fake::new(429, &header, vec![], false, false).await;
        let delay = code(&fake).await.model_error().retry_after_ms.unwrap();
        if let Some(expected) = expected {
            assert_eq!(delay, expected);
        } else {
            assert!((58000..=60000).contains(&delay));
        }
    }
    let fake = Fake::new(500, "", vec![b'x'; 100_000], false, false).await;
    assert_eq!(code(&fake).await.body().len(), 65536);
}
#[tokio::test]
async fn no_redirect() {
    let target = Fake::sse("data: leaked\n\n").await;
    let fake = Fake::new(
        307,
        &format!("Location: {}\r\n", target.endpoint),
        vec![],
        false,
        false,
    )
    .await;
    assert_eq!(
        code(&fake).await.model_error().provider_code.as_deref(),
        Some("http_307")
    );
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(target.probe.accepted.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn url_policy_and_connection_failures() {
    for endpoint in [
        "http://example.com",
        "http://localhost",
        "http://127.0.0.2",
        "ftp://127.0.0.1",
        "https://user:password@example.com",
    ] {
        let e = transport()
            .post(endpoint, &json!({}), credential(), Default::default())
            .await
            .err()
            .unwrap();
        assert_eq!(e.model_error().code, ModelErrorCode::InvalidRequest);
        no_secret(e);
    }
    let fake = Fake::sse("data: x\n\n").await;
    let t = Transport::new(Default::default()).unwrap();
    assert_eq!(
        t.post(&fake.endpoint, &json!({}), credential(), Default::default())
            .await
            .err()
            .unwrap()
            .model_error()
            .code,
        ModelErrorCode::InvalidRequest
    );
    // Keep the port reserved by a bound, non-listening socket. No other
    // process can acquire it between bind and connect (HTTP or TLS).
    let reserved = tokio::net::TcpSocket::new_v4().unwrap();
    reserved.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    for scheme in ["http", "https"] {
        let endpoint = format!("{scheme}://{}", reserved.local_addr().unwrap());
        let e = transport()
            .post(&endpoint, &json!({}), credential(), Default::default())
            .await
            .err()
            .unwrap();
        assert!(!e.is_stopped());
        assert_eq!(e.model_error().code, ModelErrorCode::Transport);
        no_secret(e);
    }
    // A matching HTTP request whose peer closes without any response is
    // also transport failure. TLS setup uses the reserved refusal above:
    // an encrypted connection cannot be attributed by an HTTP header.
    let reset = Fake::new(0, "", vec![], false, false).await;
    let e = code(&reset).await;
    assert!(!e.is_stopped());
    assert_eq!(e.model_error().code, ModelErrorCode::Transport);
    reset.closed().await;
}

#[tokio::test]
async fn ipv6_loopback_opt_in() {
    let fake = Fake::at(
        "[::1]:0",
        200,
        "",
        b"data: yes\n\n".to_vec(),
        false,
        false,
        "text/event-stream",
    )
    .await;
    let mut bytes = transport()
        .post(&fake.endpoint, &json!({}), credential(), Default::default())
        .await
        .unwrap();
    assert_eq!(bytes.next().await.unwrap().unwrap(), b"data: yes\n\n");
    fake.closed().await;
}
fn login_fixture(path: &std::path::Path, expiry: u64) -> String {
    let token = format!(
        "header.{}.{}",
        URL_SAFE_NO_PAD.encode(json!({"exp":expiry}).to_string()),
        SECRET
    );
    std::fs::write(
        path,
        json!({"tokens":{"access_token":token,"account_id":"fixture-account"}}).to_string(),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    token
}
#[test]
fn codex_read_only_rereads_expiry_and_redaction() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let source = credentials::CodexLogin::from_path(&path);
    let e = source.credential().unwrap_err();
    assert!(e.message.contains("run `codex login`"));
    no_secret(e);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let token = login_fixture(&path, now + 3600);
    let original = std::fs::read(&path).unwrap();
    let c = source.credential().unwrap();
    assert!(c.expiry().unwrap() > SystemTime::now());
    assert!(
        c.request_headers()["authorization"] == format!("Bearer {token}"),
        "wrong bearer header"
    );
    assert_eq!(c.request_headers()["chatgpt-account-id"], "fixture-account");
    assert_eq!(std::fs::read(&path).unwrap(), original);
    no_secret(&c);
    no_secret(c.to_string());
    no_secret(&source);
    no_secret(source.to_string());
    no_secret(c.request_headers());
    login_fixture(&path, now - 1);
    let e = source.credential().unwrap_err();
    assert_eq!(e.code, ModelErrorCode::Authentication);
    assert!(e.message.contains("run `codex login`"));
    no_secret(e);
    login_fixture(&path, now + 7200);
    assert!(source.credential().unwrap().expiry().unwrap() > c.expiry().unwrap());
    for value in [
        json!({}),
        json!({"tokens":{"access_token":SECRET,"account_id":"fixture"}}),
        json!({"tokens":{"access_token":"a.e30.c","account_id":"fixture"}}),
    ] {
        std::fs::write(&path, value.to_string()).unwrap();
        no_secret(source.credential().unwrap_err());
    }
    std::fs::write(&path, SECRET).unwrap();
    no_secret(source.credential().unwrap_err());
    no_secret(Credential::bearer(format!("{SECRET}\r\n"), HeaderMap::new(), None).unwrap_err());
}
#[test]
fn insecure_credential_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    login_fixture(&path, u32::MAX as u64);
    for mode in [0o644, 0o640, 0o604] {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let e = credentials::CodexLogin::from_path(&path)
            .credential()
            .unwrap_err();
        assert_eq!(e.message, "insecure credential file");
    }
    // Ownership predicate is exercised on a real opened fixture's metadata,
    // including the mismatch branch on unprivileged CI runners.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let metadata = std::fs::File::open(&path).unwrap().metadata().unwrap();
    let owner = rustix::process::getuid().as_raw();
    assert!(credentials::secure_file(&metadata, owner));
    assert!(!credentials::secure_file(&metadata, owner.wrapping_add(1)));
    // When privileged, also exercise the complete mismatched-owner file read.
    if rustix::process::getuid().is_root() {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        rustix::fs::chown(&path, Some(rustix::process::Uid::from_raw(65534)), None).unwrap();
        assert_eq!(
            credentials::CodexLogin::from_path(&path)
                .credential()
                .unwrap_err()
                .message,
            "insecure credential file"
        );
    }
}
#[tokio::test]
async fn backpressured_cancel_deadline_drop_release() {
    for mode in ["cancel", "deadline", "drop"] {
        let fake = Fake::new(
            200,
            "",
            format!(
                "event: start\ndata: yes\n\n{}",
                "event: text\ndata: x\n\n".repeat(20)
            )
            .into_bytes(),
            true,
            false,
        )
        .await;
        let mut provider = echo(&fake);
        provider.bounds = SseConfig {
            max_event_bytes: 64,
            max_queue_bytes: 64,
            max_queue_events: 1,
            ..Default::default()
        };
        let control = OperationControl {
            deadline: (mode == "deadline")
                .then(|| tokio::time::Instant::now() + Duration::from_millis(80)),
            ..Default::default()
        };
        let mut stream = provider.stream(request(), control.clone());
        assert_eq!(stream.next().await, Some(ProviderEvent::Started));
        tokio::time::sleep(Duration::from_millis(20)).await;
        if mode == "drop" {
            drop(stream);
        } else {
            if mode == "cancel" {
                control.cancel.cancel();
            }
            fake.closed().await;
            drop(stream);
        }
        fake.closed().await;
    }
}
#[test]
fn captured_secret_outputs() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["_redaction", "--nocapture"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains(SECRET),
        "secret leaked in stdout or panic"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(SECRET),
        "secret leaked in stderr or log"
    );
    assert!(output.status.success(), "redaction subprocess failed");
    no_secret(TransportConfig {
        proxy: Some(format!("http://user:{SECRET}@127.0.0.1")),
        ..Default::default()
    });
}
#[tokio::test]
async fn proxy_environment_ignored() {
    if std::env::var_os("HARNESS_PROXY_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::proxy_environment_ignored"])
            .env("HARNESS_PROXY_CHILD", "1")
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("ALL_PROXY", "http://127.0.0.1:1")
            .env("http_proxy", "http://127.0.0.1:1")
            .env("https_proxy", "http://127.0.0.1:1")
            .env("all_proxy", "http://127.0.0.1:1")
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ambient proxy changed direct transport"
        );
        return;
    }
    let fake = Fake::sse("data: yes\n\n").await;
    let mut stream = read(&fake, Default::default(), Default::default());
    assert_eq!(stream.next().await.unwrap().unwrap().data, "yes");
    assert_eq!(fake.probe.accepted.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn echo_conformance() {
    echo_cases_conform(|provider| provider).await;
}

/// The same public conformance cases with the fixture adapter only reachable
/// through the router. Dispatch by `provider_instance` is the only difference:
/// every other observation (submitted request, release, terminal outcome) must
/// be unchanged.
#[tokio::test]
async fn conformance_cases_pass_through_the_router() {
    echo_cases_conform(|provider| Arc::new(Router::new().with("echo", provider))).await;
}

async fn echo_cases_conform(wrap: fn(Arc<dyn ModelProvider>) -> Arc<dyn ModelProvider>) {
    for kind in [
        CaseKind::Text,
        CaseKind::Refusal,
        CaseKind::OutputLimit,
        CaseKind::ProviderError,
        CaseKind::RateLimit,
        CaseKind::SetupFailure,
        CaseKind::CancelSetup,
        CaseKind::CancelStreaming,
        CaseKind::Drop,
        CaseKind::Deadline,
        CaseKind::Request,
    ] {
        let fake = echo_fake(kind).await;
        let case = echo_case(kind);
        let factory = |_: &str| {
            let p = fake.probe.clone();
            Fixture {
                provider: wrap(Arc::new(echo(&fake))),
                observe: Arc::new(move || Observation {
                    submitted: submitted(&p),
                    released: p.active.load(Ordering::SeqCst) == 0,
                }),
            }
        };
        conformance::run_case(&factory, &case).await.unwrap();
    }
}

async fn echo_fake(kind: CaseKind) -> Fake {
    let (status, body, hold, setup) = match kind {
        CaseKind::ProviderError => (503, "error", false, false),
        CaseKind::RateLimit => (429, "error", false, false),
        CaseKind::SetupFailure => (401, "error", false, false),
        CaseKind::CancelSetup | CaseKind::Drop => (200, "", true, true),
        CaseKind::CancelStreaming => (200, "event: start\ndata: yes\n\n", true, false),
        CaseKind::Refusal => (
            200,
            "event: start\ndata: yes\n\nevent: finish\ndata: refusal\n\n",
            false,
            false,
        ),
        CaseKind::OutputLimit => (
            200,
            "event: start\ndata: yes\n\nevent: finish\ndata: output_limit\n\n",
            false,
            false,
        ),
        _ => (
            200,
            "event: start\ndata: yes\n\nevent: text\ndata: hello\n\nevent: finish\ndata: stop\n\n",
            false,
            false,
        ),
    };
    Fake::new(
        status,
        "retry-after-ms: 75\r\n",
        body.as_bytes().to_vec(),
        hold,
        setup,
    )
    .await
}

fn submitted(probe: &Probe) -> Option<SubmittedRequest> {
    let body = probe.submitted.lock().unwrap();
    let body = body.as_ref()?;
    let options = serde_json::from_value(body.get("options")?.clone()).ok()?;
    let tools = body
        .get("tools")?
        .as_array()?
        .iter()
        .map(|t| {
            Some(SubmittedTool {
                name: t.get("name")?.as_str()?.into(),
                description: t.get("description")?.as_str()?.into(),
                input_schema: t.get("input_schema")?.clone(),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(SubmittedRequest { tools, options })
}

fn echo_case(kind: CaseKind) -> Case {
    use harness_core::{
        model::ReasoningEffort,
        session::{EffectClass, EffectSet, ExecutionBackend},
        tools::ToolDefinition,
    };
    let status = match kind {
        CaseKind::ProviderError => 503,
        CaseKind::RateLimit => 429,
        CaseKind::SetupFailure => 401,
        _ => 200,
    };
    let mut expected = Expected::default();
    match kind {
        CaseKind::Text | CaseKind::Request => {
            expected.text = BTreeMap::from([(0, "hello".into())]);
        }
        CaseKind::Refusal => expected.finish_reason = Some(FinishReason::Refusal),
        CaseKind::OutputLimit => expected.finish_reason = Some(FinishReason::OutputLimit),
        CaseKind::ProviderError | CaseKind::RateLimit | CaseKind::SetupFailure => {
            expected.outcome = ProviderOutcome::Failed;
            expected.finish_reason = None;
            expected.error = Some(harness_core::session::ModelError {
                code: match kind {
                    CaseKind::ProviderError => ModelErrorCode::ProviderFailure,
                    CaseKind::RateLimit => ModelErrorCode::RateLimit,
                    _ => ModelErrorCode::Authentication,
                },
                message: "provider HTTP request failed".into(),
                provider_code: Some(format!("http_{status}")),
                retry_after_ms: Some(75),
            });
        }
        _ => {}
    }
    let mut input = request();
    if kind == CaseKind::Request {
        input.input.tools = ["read", "other"]
            .into_iter()
            .map(|name| ToolDefinition {
                name: name.into(),
                description: format!("definition {name}"),
                input_schema: json!({"type":"object"}),
                effects: EffectSet::new([EffectClass::Read]).unwrap(),
                backend: ExecutionBackend::TrustedFs,
            })
            .collect();
        input.input.options.reasoning_effort = Some(ReasoningEffort::High);
        input.input.options.provider_options =
            serde_json::from_value(json!({"echo":{"strict":true}})).unwrap();
    }
    Case {
        kind,
        fixture: format!("{kind:?}"),
        request: input,
        expected,
        unsupported_option: None,
    }
}

#[test]
fn env_key_read_once_in_child() {
    // Re-exec avoids unsafe mutation of the environment in a threaded test runner.
    if std::env::var_os("HARNESS_ENV_CHILD").is_some() {
        let c = credentials::EnvApiKey::new("HARNESS_FIXTURE_KEY").unwrap();
        no_secret(&c);
        no_secret(c.to_string());
        no_secret(c.credential().unwrap());
        let error = credentials::EnvApiKey::new("HARNESS_MISSING_KEY").unwrap_err();
        assert!(error.message.contains("HARNESS_MISSING_KEY"));
        no_secret(error);
        let error = credentials::EnvApiKey::new("HARNESS_EMPTY_KEY").unwrap_err();
        no_secret(error);
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::env_key_read_once_in_child",
            "--nocapture",
        ])
        .env("HARNESS_ENV_CHILD", "1")
        .env("HARNESS_FIXTURE_KEY", SECRET)
        .env("HARNESS_EMPTY_KEY", "")
        .env_remove("HARNESS_MISSING_KEY")
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));
    assert!(output.status.success(), "credential child failed");
}
