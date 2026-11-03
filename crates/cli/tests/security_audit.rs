//! Offline sentinel from the real credential/header boundary to durable exports
//! and the TUI. A loopback fixture deliberately echoes the header in error codes.
use harness::{
    tui::{App, SessionInfo},
    wiring::{self, SessionOptions},
};
use harness_core::{model::ModelProvider, runtime::*, session::*};
use harness_providers::{
    anthropic::AnthropicMessages,
    credentials::{Credential, CredentialSource},
    openai::{OpenAiResponses, Profile},
    transport::{Transport, TransportConfig},
};
use ratatui::{Terminal, backend::TestBackend};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

const SENTINEL: &str = "SENTINEL-CREDENTIAL-AUDIT";
struct Key(Credential);
impl CredentialSource for Key {
    fn credential(&self) -> Result<Credential, ModelError> {
        Ok(self.0.clone())
    }
}
fn no_secret(bytes: &[u8]) {
    assert!(
        !bytes
            .windows(SENTINEL.len())
            .any(|v| v == SENTINEL.as_bytes()),
        "credential escaped redaction"
    );
}
fn scan(path: &Path) {
    for item in fs::read_dir(path).unwrap() {
        let p = item.unwrap().path();
        if p.is_dir() {
            scan(&p);
        } else {
            no_secret(&fs::read(p).unwrap());
        }
    }
}
fn private(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o7777,
        mode
    );
}

#[tokio::test]
async fn sentinel_credentials_stay_out_of_journals_exports_config_and_tui() {
    // The fixture reads the actual header, then echoes it in both message and
    // code/type. This catches raw provider-code copying, not just message leaks.
    for mode in ["openai-http", "openai-sse", "anthropic-http"] {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let nonce = SessionId::new().to_string();
        let expected = format!("POST /{nonce} HTTP/1.1");
        let server = std::thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(15);
            loop {
                assert!(
                    Instant::now() < end,
                    "fixture received no attributed request"
                );
                let (mut socket, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => panic!("fixture accept failed"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 4096];
                loop {
                    let n = match socket.read(&mut buf) {
                        Ok(n) if n > 0 => n,
                        _ => break,
                    };
                    request.extend_from_slice(&buf[..n]);
                    assert!(request.len() < 128 * 1024, "fixture request bound");
                    if let Some(pos) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..pos]);
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|s| s.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= pos + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8_lossy(&request);
                if !request.starts_with(&expected) {
                    continue;
                } // unrelated listener probes
                let header = request
                    .lines()
                    .find(|l| {
                        l.to_ascii_lowercase()
                            .starts_with(if mode == "anthropic-http" {
                                "x-api-key:"
                            } else {
                                "authorization:"
                            })
                    })
                    .expect("credential header missing");
                assert!(
                    header.contains(SENTINEL),
                    "fixture did not receive the sentinel credential"
                );
                let body = if mode == "openai-sse" {
                    format!(
                        "event: error\ndata: {{\"type\":\"error\",\"code\":\"{SENTINEL}\",\"message\":\"{SENTINEL}\"}}\n\n"
                    )
                } else {
                    format!(
                        "{{\"error\":{{\"code\":\"{SENTINEL}\",\"type\":\"{SENTINEL}\",\"message\":\"{SENTINEL}\"}}}}"
                    )
                };
                let status = if mode == "openai-sse" {
                    "200 OK"
                } else {
                    "500 Internal Server Error"
                };
                let content_type = if mode == "openai-sse" {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                write!(socket, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                return;
            }
        });
        let transport = Transport::new(TransportConfig {
            allow_loopback_http: true,
            ..Default::default()
        })
        .unwrap();
        let key = if mode == "anthropic-http" {
            Credential::api_key
        } else {
            Credential::bearer
        };
        let credential = Arc::new(Key(key(SENTINEL.into(), Default::default(), None).unwrap()));
        let endpoint = format!("http://{address}/{nonce}");
        let (provider, model): (Arc<dyn ModelProvider>, ModelRef) = if mode == "anthropic-http" {
            (
                Arc::new(
                    AnthropicMessages::with_transport(credential, transport)
                        .with_endpoint(endpoint),
                ),
                ModelRef {
                    provider_instance: "anthropic".into(),
                    model: "claude-sonnet-5-5".into(),
                },
            )
        } else {
            (
                Arc::new(
                    OpenAiResponses::with_transport(Profile::Platform, credential, transport)
                        .with_endpoint(endpoint),
                ),
                ModelRef {
                    provider_instance: "openai".into(),
                    model: "gpt-5.5".into(),
                },
            )
        };
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("work");
        fs::create_dir(&workspace).unwrap();
        let mut driver = DriverConfig::default();
        driver.runtime.context.catalog.insert(
            model.clone(),
            harness_core::context::ModelLimits {
                context_tokens: Some(128_000),
                max_output_tokens: Some(8192),
            },
        );
        let options = SessionOptions {
            workspace: workspace.clone(),
            data_root: temp.path().join("state"),
            session: None,
            model: model.clone(),
            explicit_model: false,
            explicit_provider: false,
            switch_policy: Portability::Reject,
            known_instances: vec![],
            summary_model: None,
            compaction_strategy: wiring::creation_compaction_strategy(),
            explicit_strategy: false,
            driver,
            sandbox: None,
        };
        let wired = wiring::open_session(&options, provider).await.unwrap();
        wired
            .handle
            .command(Command::StartTurn {
                command_id: CommandId::new(),
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
            })
            .await
            .unwrap();
        let view = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let view = wired.handle.view().await.unwrap();
                if view
                    .snapshot
                    .events()
                    .any(|(_, _, e)| matches!(e, JournalEvent::TurnFinished(_)))
                {
                    break view;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("turn deadline");
        server.join().unwrap();
        let error = view
            .snapshot
            .entries()
            .find_map(|(_, e)| match &e.entry.payload {
                EntryPayload::AssistantResponse(r) => r.error.clone(),
                _ => None,
            })
            .expect("provider error was not journaled");
        no_secret(&serde_json::to_vec(&error).unwrap());
        assert_eq!(error.code, ModelErrorCode::ProviderFailure);
        let mut app = App::new(SessionInfo {
            workspace,
            model,
            context_limit: Some(128_000),
        });
        app.on_subscription(SubscriptionItem::Reset(ClientView::from(view)));
        // Exercise the actual sanitized provider diagnostic on the banner surface.
        app.set_error(format!("provider failure: {}", error.message));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| app.render(f)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        no_secret(rendered.as_bytes());
        assert!(rendered.contains("provider failure"));
        wired.handle.shutdown().await;
        let root = DataRoot::open(&options.data_root, false).unwrap();
        let markdown = temp.path().join("export.md");
        root.export_markdown_to(&wired.session, &markdown, true)
            .unwrap();
        let journal = temp.path().join("export-journal");
        root.export_journal(&wired.session, &journal).unwrap();
        no_secret(&fs::read(&markdown).unwrap());
        scan(&journal);
        scan(root.path());
        for path in [
            root.path(),
            &root.path().join("sessions"),
            &root.path().join("artifacts"),
            &journal,
            &journal.join("sessions"),
            &journal.join("artifacts"),
        ] {
            private(path, 0o700);
        }
        for dir in [
            root.path().join("sessions"),
            root.path().join("artifacts"),
            journal.join("sessions"),
            journal.join("artifacts"),
        ] {
            for entry in fs::read_dir(dir).unwrap() {
                private(&entry.unwrap().path(), 0o600);
            }
        }
        private(&markdown, 0o600);
        let config = temp.path().join("config.toml");
        fs::write(&config, format!("api_key = '{SENTINEL}'\n")).unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_harness"))
            .args(["--config"])
            .arg(config)
            .args(["config", "show"])
            .env("OPENAI_API_KEY", SENTINEL)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        no_secret(&output.stdout);
        no_secret(&output.stderr);
        assert!(String::from_utf8_lossy(&output.stderr).contains("config_credential_refused"));
    }
}
