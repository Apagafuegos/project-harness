//! Offline M3 walkthrough with real files and a process crash, no shell backend.
//! Run with `cargo run --example tool_session`. Output deliberately reports
//! context membership/provenance, not random IDs, host paths or elapsed times.
use harness_core::{
    context::ModelLimits,
    model::{
        ProviderEvent,
        scripted::{Script, ScriptedProvider, Step},
    },
    runtime::*,
    session::*,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command as Process, Stdio},
    sync::Arc,
    time::Duration,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn model() -> ModelRef {
    ModelRef {
        provider_instance: "mock".into(),
        model: "scripted".into(),
    }
}
fn config() -> DriverConfig {
    let mut c = DriverConfig::default();
    c.runtime.context.catalog.insert(
        model(),
        ModelLimits {
            context_tokens: Some(128_000),
            max_output_tokens: None,
        },
    );
    c
}
fn host(base: &Path) -> ToolHostConfig {
    ToolHostConfig {
        workspace_root: base.join("workspace"),
        data_root: base.join("state"),
        sandbox: None,
    }
}
fn proposal(name: &str, args: Value) -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::Event(ProviderEvent::ToolCallStarted {
            block_index: 0,
            name: name.into(),
            provider_call_id: None,
        }),
        Step::Event(ProviderEvent::ToolArgumentsDelta {
            block_index: 0,
            fragment: args.to_string(),
        }),
        Step::Event(ProviderEvent::ToolCallReady { block_index: 0 }),
        Step::completed(FinishReason::ToolCalls),
    ])
}
fn reply() -> Script {
    Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, "done"),
        Step::completed(FinishReason::Stop),
    ])
}
fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?)
}
async fn wait(
    handle: &SessionHandle,
    predicate: impl Fn(&RuntimeView) -> bool,
) -> Result<RuntimeView> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let view = handle.view().await.map_err(|e| format!("{e:?}"))?;
            if view.phase == Phase::Faulted {
                return Err::<RuntimeView, Box<dyn std::error::Error>>(
                    "unexpected runtime fault".into(),
                );
            }
            if predicate(&view) {
                return Ok(view);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await?
}
async fn start(handle: &SessionHandle) -> Result<()> {
    handle
        .command(Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![ContentBlock::Text {
                text: "use the tool".into(),
            }],
        })
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}
async fn approve(handle: &SessionHandle) -> Result<()> {
    let p = wait(handle, |v| v.pending_approval.is_some())
        .await?
        .pending_approval
        .unwrap();
    handle
        .command(Command::SubmitApproval {
            approval_id: p.approval_id,
            plan_digest: p.plan_digest,
            decision: Decision::Allow,
            remember: false,
        })
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}
fn idle(v: &RuntimeView) -> bool {
    v.phase == Phase::Durable(RuntimeState::Idle)
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() == 4 && args[1] == "--crash-child" {
        return crash_child(
            Path::new(&args[2]),
            args[3].to_str().ok_or("session")?.parse()?,
        );
    }
    let scratch =
        Scratch(std::env::temp_dir().join(format!("harness-tool-session-{}", SessionId::new())));
    fs::create_dir(&scratch.0)?;
    fs::create_dir(scratch.0.join("workspace"))?;
    fs::write(scratch.0.join("workspace/note"), "hello")?;
    let root = Arc::new(DataRoot::open(scratch.0.join("state"), true)?);
    let session = SessionId::new();
    let policy = json!({"policy_revision":"p1", "enabled_tools":["read","write","edit"], "workspace_read":"allow", "workspace_mutation":"ask", "process":"ask", "network":"deny", "protected_paths":[], "allowances":[]});
    let header = SessionHeader {
        workspace: host(&scratch.0)
            .workspace_root
            .to_str()
            .ok_or("workspace")?
            .into(),
        initial_model: model(),
        policy_revision: "p1".into(),
        configuration: root.put_artifact(&serde_json::to_vec(&policy)?)?,
    };
    runtime()?.block_on(async {
        let provider = Arc::new(ScriptedProvider::new(vec![
            proposal("read", json!({"path":"note"})),
            reply(),
            proposal(
                "edit",
                json!({"path":"note","old_text":"hello","new_text":"hi"}),
            ),
            reply(),
        ]));
        let h = SessionHandle::create_with_tools(
            root.clone(),
            session.clone(),
            header,
            provider,
            config(),
            &host(&scratch.0),
        )
        .await?;
        start(&h).await?;
        wait(&h, idle).await?;
        println!("read: succeeded");
        start(&h).await?;
        approve(&h).await?;
        wait(&h, idle).await?;
        assert_eq!(fs::read_to_string(scratch.0.join("workspace/note"))?, "hi");
        println!("edit: approval granted; succeeded");
        h.shutdown().await;
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    let status = Process::new(std::env::current_exe()?)
        .arg("--crash-child")
        .arg(&scratch.0)
        .arg(session.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(25),
        "child must die from SIGXFSZ, not graceful shutdown"
    );
    // The OS deliberately interrupts the result append. Repair only the exact
    // inspected unterminated suffix; this cannot invent a result.
    let inspection = JournalStore::inspect(&root, &session)?;
    assert!(inspection.file_bytes > inspection.valid_bytes);
    JournalStore::repair_tail(root.clone(), &session, &inspection, Timestamp::now())?;
    assert_eq!(
        fs::read_to_string(scratch.0.join("workspace/new"))?,
        "written"
    );
    println!("write: process crashed after mutation, before result");
    runtime()?.block_on(async {
        let provider = Arc::new(ScriptedProvider::new(vec![]));
        let h = SessionHandle::open_with_tools(
            root.clone(),
            session,
            provider.clone(),
            config(),
            &host(&scratch.0),
        )
        .await?;
        let v = wait(&h, |v| {
            v.phase == Phase::AwaitingResolution
                && v.awaiting.first().is_some_and(|i| i.hint.is_some())
        })
        .await?;
        let invocation = &v.awaiting[0];
        assert_eq!(
            invocation.hint,
            Some(harness_core::tools::RecoveryHint::MatchesPostImage)
        );
        println!("reopen: uncertain write; advisory hint matches_post_image");
        h.command(Command::ResolveUncertain {
            invocation_id: invocation.invocation_id.clone(),
            resolution: UserResolution::Observed {
                status: ToolStatus::Succeeded,
                text: "Checked new: written".into(),
            },
        })
        .await
        .map_err(|e| format!("{e:?}"))?;
        wait(&h, idle).await?;
        assert!(provider.streams().is_empty());
        println!("user resolution: observed succeeded; no re-execution");
        let report = h.inspect_context(None, None).await?;
        println!("== final context report (membership and provenance)");
        for item in report.included {
            println!("{:?} {:?}", item.kind, item.provenance);
        }
        println!("flags: {}", serde_json::to_string(&report.flags)?);
        h.shutdown().await;
        Ok::<_, Box<dyn std::error::Error>>(())
    })?;
    Ok(())
}

fn crash_child(base: &Path, session: SessionId) -> Result<()> {
    let root = Arc::new(DataRoot::open(base.join("state"), false)?);
    runtime()?.block_on(async {
        let provider = Arc::new(ScriptedProvider::new(vec![
            proposal("write", json!({"path":"new","text":"written"})),
            reply(),
        ]));
        let h = SessionHandle::open_with_tools(
            root.clone(),
            session.clone(),
            provider,
            config(),
            &host(base),
        )
        .await?;
        start(&h).await?;
        wait(&h, |v| v.pending_approval.is_some()).await?;
        // Bound the *next* append using the prior edit's actual intent record.
        // Both use the same authorization/intent shape and UUID lengths; 128
        // bytes accommodates timestamp/size differences, but not a result.
        // SIGXFSZ kills the process during its result append, with no Drop or
        // cancellation. This is an example-local OS fault, not a product hook
        // or a race against a fast filesystem. The parent verifies the crash,
        // torn tail, post-image and unresolved intent before reporting success.
        let journal = base.join("state/sessions").join(format!("{session}.jsonl"));
        let bytes = fs::read(&journal)?;
        let intent_size = bytes
            .split_inclusive(|b| *b == b'\n')
            .filter(|line| String::from_utf8_lossy(line).contains("execution_intent"))
            .map(|line| line.len())
            .max()
            .ok_or("missing previous intent")?;
        let limit = (bytes.len() + intent_size + 128) as u64;
        // Do not leave a host-dependent core dump from the deliberate crash.
        rustix::process::setrlimit(
            rustix::process::Resource::Core,
            rustix::process::Rlimit {
                current: Some(0),
                maximum: Some(0),
            },
        )?;
        rustix::process::setrlimit(
            rustix::process::Resource::Fsize,
            rustix::process::Rlimit {
                current: Some(limit),
                maximum: Some(limit),
            },
        )?;
        approve(&h).await?;
        wait(&h, idle).await?;
        Err::<(), Box<dyn std::error::Error>>("result unexpectedly fit the crash limit".into())
    })
}
