//! Offline M4 acceptance tour. Native fixtures, real adapters, one durable session.
use std::{sync::Arc, time::Duration};

use harness_core::{context::Part, model::ModelProvider, runtime::*, session::*};
use harness_providers::{
    anthropic::{self, AnthropicMessages},
    credentials::{Credential, CredentialSource},
    openai::{self, OpenAiResponses, Profile},
    router::Router,
    transport::{Transport, TransportConfig},
};
use serde_json::json;

#[path = "../tests/support/fixture_server.rs"]
mod fixture_server;
use fixture_server::Server;

struct Fixed(Credential);
impl CredentialSource for Fixed {
    fn credential(&self) -> Result<Credential, ModelError> {
        Ok(self.0.clone())
    }
}

fn credential(server: &Server, api_key: bool) -> Arc<Fixed> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-harness-test", server.nonce.parse().expect("nonce"));
    let value = if api_key {
        Credential::api_key("offline-fixture".into(), headers, None)
    } else {
        Credential::bearer("offline-fixture".into(), headers, None)
    };
    Arc::new(Fixed(value.expect("credential")))
}

async fn turn(handle: &SessionHandle, prompt: &str) {
    let Accepted::TurnStarted { turn_id, .. } = handle
        .command(Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![ContentBlock::Text {
                text: prompt.into(),
            }],
        })
        .await
        .expect("start")
    else {
        panic!("turn not started")
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let view = handle.view().await.expect("view");
            if let Some(finish) = view
                .snapshot
                .events()
                .find_map(|(_, _, event)| match event {
                    JournalEvent::TurnFinished(finish) if finish.turn_id == turn_id => Some(finish),
                    _ => None,
                })
            {
                assert_eq!(finish.outcome, TurnOutcome::Completed);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded turn");
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let openai_server = Server::start().await;
    let anthropic_server = Server::start().await;
    let transport = Transport::new(TransportConfig {
        allow_loopback_http: true,
        ..TransportConfig::default()
    })
    .expect("transport");
    let provider: Arc<dyn ModelProvider> = Arc::new(
        Router::new()
            .with(
                "openai",
                Arc::new(
                    OpenAiResponses::with_transport(
                        Profile::Platform,
                        credential(&openai_server, false),
                        transport.clone(),
                    )
                    .with_endpoint(openai_server.url.clone()),
                ),
            )
            .with(
                "anthropic",
                Arc::new(
                    AnthropicMessages::with_transport(
                        credential(&anthropic_server, true),
                        transport,
                    )
                    .with_endpoint(anthropic_server.url.clone()),
                ),
            ),
    );
    let temp = tempfile::tempdir().expect("scratch");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let root = Arc::new(DataRoot::open(temp.path().join("state"), true).expect("root"));
    let model = ModelRef {
        provider_instance: "openai".into(),
        model: "gpt-5.5".into(),
    };
    let target = ModelRef {
        provider_instance: "anthropic".into(),
        model: "claude-sonnet-5-5".into(),
    };
    let configuration = root
        .put_artifact(
            &serde_json::to_vec(&json!({
                "compaction": { "summary_model": target, "automatic": false }
            }))
            .expect("json"),
        )
        .expect("configuration");
    let mut config = DriverConfig::default();
    openai::extend_catalog(
        &mut config.runtime.context.catalog,
        Profile::Platform,
        "openai",
    );
    anthropic::extend_catalog(
        &mut config.runtime.context.catalog,
        anthropic::Profile::Anthropic,
        "anthropic",
    );
    let handle = SessionHandle::create(
        root,
        SessionId::new(),
        SessionHeader {
            workspace: workspace.to_str().expect("path").into(),
            initial_model: model,
            policy_revision: "offline".into(),
            configuration,
        },
        provider,
        config,
    )
    .await
    .expect("session");

    openai_server.reply(include_str!("../fixtures/openai/one_tool.sse"));
    openai_server.reply(include_str!("../fixtures/openai/text.sse"));
    turn(&handle, "read notes.txt").await;
    println!(
        "OpenAI tool turn: completed; tool result: tools_unavailable (no authority installed)"
    );
    // Three completed turns are required to compact while retaining the newest two.
    openai_server.reply(include_str!("../fixtures/openai/text.sse"));
    turn(&handle, "retain this turn").await;
    openai_server.reply(include_str!("../fixtures/openai/native_state.sse"));
    turn(&handle, "retain owner-scoped reasoning").await;

    let summary = "## Task and constraints\nRead notes.\n## Decisions\nNo tool authority.\n## Files and actions\nNone.\n## Outcomes\nCall denied.\n## Unresolved work\nNone.\n";
    anthropic_server.reply_text(summary);
    let before = handle.view().await.expect("view").snapshot.sequence();
    let Accepted::CompactionPreviewed {
        preview_id,
        preview,
    } = handle
        .command(Command::PreviewCompaction)
        .await
        .expect("preview")
    else {
        panic!("not preview")
    };
    assert_eq!(
        handle.view().await.expect("view").snapshot.sequence(),
        before
    );
    println!(
        "manual preview: read-only; covered={}; fits={}",
        preview.diff.newly_covered.len(),
        preview.diff.fits_budget
    );
    handle
        .command(Command::CommitCompaction { preview_id })
        .await
        .expect("commit");
    println!("manual compaction: committed; recent turns retained=2");
    let rejection = handle
        .command(Command::SwitchModel {
            model: target.clone(),
            policy: Portability::Reject,
        })
        .await
        .expect_err("state loss must reject");
    let Rejection::PortabilityLoss(report) = rejection else {
        panic!("not portability loss")
    };
    println!(
        "switch Reject: refused; required={}",
        report.required_incompatibilities.len()
    );
    for loss in &report.would_stop_being_portable {
        let part = match loss.part {
            Part::Block { index } => format!("block[{index}]"),
            _ => "continuation".into(),
        };
        // Entry UUIDs are intentionally normalized; owner/part/schema/bytes are real report data.
        println!(
            "portability: entry=retained-response part={part} owner={}/{} schema={} bytes={}",
            loss.owner.provider_instance, loss.owner.model, loss.schema, loss.bytes
        );
    }
    handle
        .command(Command::SwitchModel {
            model: target,
            policy: Portability::OmitOptional,
        })
        .await
        .expect("explicit omit");
    println!("switch OmitOptional: accepted");
    anthropic_server.reply(include_str!("../fixtures/anthropic/text.sse"));
    turn(&handle, "finish on Anthropic").await;
    println!("Anthropic turn: completed");
    let report = handle.inspect_context(None, None).await.expect("report");
    println!(
        "final context report: model={}/{} included={} excluded={} summaries={} estimated_tokens={} portability_losses={} flags={:?}",
        report.identity.model.provider_instance,
        report.identity.model.model,
        report.included.len(),
        report.excluded.len(),
        report.summaries.len(),
        report.totals.estimated_total,
        report.portability.len(),
        report.flags
    );
    // Report membership/provenance without random journal identities, paths or
    // the identity-dependent fingerprint. Token estimates remain labelled.
    for (index, item) in report.included.iter().enumerate() {
        println!(
            "included[{index}]: {:?} provenance={:?} estimated_tokens={} opaque_bytes={}",
            item.kind, item.provenance, item.estimated_tokens, item.opaque_bytes
        );
    }
    for (index, item) in report.excluded.iter().enumerate() {
        println!("excluded[{index}]: {:?} {:?}", item.part, item.reason);
    }
    println!("budget: {:?}", report.budget);
    handle.shutdown().await;
}
