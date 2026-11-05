//! A durable, injected-provider headless client. Tools run only in the runtime.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read, Write},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use harness_core::{
    context::{Part, PreviewReport, RequiredCapability, StrategyId},
    model::ModelProvider,
    runtime::*,
    session::*,
    tools::{Registry, SandboxConfig},
};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::text::{bounded, line};

/// Invocation configuration. The caller supplies sourced model metadata in `driver`.
#[derive(Clone, Debug)]
pub struct Options {
    /// Absolute private storage root, outside the workspace.
    pub data_root: PathBuf,
    /// Explicitly authorized workspace.
    pub workspace: PathBuf,
    /// Continue this session, or create a new one.
    pub session: Option<SessionId>,
    /// Model for a new session; an explicit mismatch on continuation is refused.
    pub model: ModelRef,
    /// Whether the user supplied a model override.
    pub explicit_model: bool,
    /// Whether the user supplied a provider override.
    pub explicit_provider: bool,
    /// Explicit policy for a mid-session switch, applied only when the requested
    /// provider or model differs from the branch's current one.
    pub switch_policy: Portability,
    /// Provider instances the injected provider can actually serve, for an
    /// early refusal instead of a failed inference. Empty disables validation.
    pub known_instances: Vec<String>,
    /// Independent summary model, enabling automatic compaction only when set.
    pub summary_model: Option<ModelRef>,
    /// Compaction strategy for a new session.
    pub compaction_strategy: StrategyId,
    /// Whether the user supplied a strategy override; a differing explicit
    /// value is refused on continuation.
    pub explicit_strategy: bool,
    /// Literal prompt, or `-` to read bounded UTF-8 from the supplied stdin.
    pub prompt: String,
    /// Auto-approve registry-declared read-only tools, only with a terminal.
    pub approve_read_only: bool,
    /// Runtime defaults and sourced catalog. No caller-installed tool host.
    pub driver: DriverConfig,
    /// Real host assembly settings (tests can disable the shell).
    pub sandbox: Option<SandboxConfig>,
    /// Print the Codex authentication guidance on authentication failure.
    pub codex: bool,
}

/// Nonblocking terminal interface, independent of prompt stdin.
pub trait Terminal {
    /// Show the exact pending proposal and begin reading one human decision.
    fn begin(&mut self, proposal: &str) -> io::Result<()>;
    /// `None` means not ready; EOF and anything except a literal `y` deny.
    fn poll(&mut self) -> io::Result<Option<bool>>;
}

/// `/dev/tty`, opened nonblocking so cancellation never waits for terminal input.
pub struct ControllingTerminal {
    file: File,
    line: Vec<u8>,
}
impl ControllingTerminal {
    /// Missing controlling terminals are represented by `None`, never stdin.
    pub fn open() -> Option<Self> {
        let fd = rustix::fs::open(
            "/dev/tty",
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .ok()?;
        Some(Self {
            file: File::from(fd),
            line: Vec::new(),
        })
    }
}
impl Terminal for ControllingTerminal {
    fn begin(&mut self, proposal: &str) -> io::Result<()> {
        self.line.clear();
        writeln!(self.file, "{proposal}")?;
        write!(self.file, "approve? [y/N] ")?;
        self.file.flush()
    }
    fn poll(&mut self) -> io::Result<Option<bool>> {
        let mut byte = [0];
        loop {
            match self.file.read(&mut byte) {
                Ok(0) => return Ok(Some(false)),
                Ok(_) if byte[0] == b'\n' => {
                    return Ok(Some(self.line == b"y" || self.line == b"y\r"));
                }
                Ok(_) => {
                    // Bound input without accepting a truncated affirmative.
                    if self.line.len() < 256 {
                        self.line.push(byte[0]);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

/// Injectable client I/O. Interrupts represent Ctrl-C; a second one exits at once.
pub struct Io<'a, 't> {
    /// Prompt source only, never approval input.
    pub stdin: &'a mut dyn Read,
    /// Assistant text only; flushed after each update.
    pub stdout: &'a mut dyn Write,
    /// Single-line diagnostics and displayable reasoning summaries.
    pub stderr: &'a mut dyn Write,
    /// Whether stderr is a terminal (controls dimming only).
    pub stderr_is_tty: bool,
    /// Controlling terminal, or none in unattended use.
    pub terminal: Option<&'t mut dyn Terminal>,
    /// Injected signal channel, independent of terminal input.
    pub interrupts: mpsc::UnboundedReceiver<()>,
    /// Renderer-owned: the last byte written to stdout is not a newline, so the
    /// assistant's last line is still open. Clients never set this.
    pub stdout_open_line: bool,
}

impl Io<'_, '_> {
    /// Terminate an unterminated assistant line before a diagnostic or at exit.
    ///
    /// Stdout carries assistant text only, so the newline is written to stdout:
    /// a merged terminal then never glues a diagnostic to the answer. Nothing is
    /// written when stdout is empty or already ends with a newline, so stdout
    /// gains at most that single byte.
    fn end_stdout_line(&mut self) -> io::Result<()> {
        if self.stdout_open_line {
            self.stdout.write_all(b"\n")?;
            self.stdout.flush()?;
            self.stdout_open_line = false;
        }
        Ok(())
    }
}

/// Run one turn with an injected provider, returning the documented process code.
/// No provider error message or opaque state is formatted by this client.
pub async fn run(options: Options, provider: Arc<dyn ModelProvider>, mut io: Io<'_, '_>) -> u8 {
    let result = match execute(options, provider, &mut io).await {
        Ok(code) => code,
        Err((code, message)) => {
            // A diagnostic must never share stdout's last line (F05).
            let _ = io.end_stdout_line();
            let _ = writeln!(io.stderr, "error: {}", line(&message));
            code
        }
    };
    // At exit: assistant text that never ended in a newline still ends a line.
    let _ = io.end_stdout_line();
    result
}

type Failure = (u8, String);
fn configuration(code: impl Into<String>) -> Failure {
    (2, code.into())
}
fn infrastructure() -> Failure {
    (1, "client_io_failure".into())
}

async fn execute(
    mut o: Options,
    provider: Arc<dyn ModelProvider>,
    io: &mut Io<'_, '_>,
) -> Result<u8, Failure> {
    o.workspace =
        std::fs::canonicalize(&o.workspace).map_err(|_| configuration("invalid_workspace"))?;
    if !o.workspace.is_dir() || !o.data_root.is_absolute() {
        return Err(configuration("invalid_workspace_or_data_root"));
    }
    if o.prompt == "-" {
        let mut bytes = Vec::new();
        io.stdin
            .take(o.driver.runtime.max_user_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| configuration("prompt_read_failed"))?;
        o.prompt = String::from_utf8(bytes).map_err(|_| configuration("prompt_not_utf8"))?;
    }
    if o.prompt.is_empty() || o.prompt.len() > o.driver.runtime.max_user_bytes {
        return Err(configuration("invalid_prompt_size"));
    }
    let opened = crate::wiring::open_session(&session_options(&o), provider).await?;
    let handle = opened.handle;
    let session = opened.session;
    for warning in opened.warnings {
        if writeln!(io.stderr, "warning: {}", line(&warning)).is_err() {
            handle.shutdown().await;
            return Err(infrastructure());
        }
    }
    // Always drain and release the journal, including output and command errors.
    let result = drive(&handle, &session, &o, io).await;
    handle.shutdown().await;
    result
}

/// Print the portability report on stderr: every affected entry and part, the
/// owner that would lose it, its schema and byte count, and every required
/// incompatibility. Native payloads are never printed — the journal keeps them,
/// and a report only classifies them.
fn switch_report(
    io: &mut Io<'_, '_>,
    verdict: &str,
    preview: &PreviewReport,
) -> Result<(), Failure> {
    writeln!(
        io.stderr,
        "switch: {verdict} from {} to {}",
        model(&preview.model_at_head),
        model(&preview.preview_model)
    )
    .map_err(|_| infrastructure())?;
    for loss in &preview.would_stop_being_portable {
        writeln!(
            io.stderr,
            "switch: omits entry={} part={} owner={} schema={} bytes={}",
            loss.entry_id,
            part(&loss.part),
            model(&loss.owner),
            line(&loss.schema),
            loss.bytes
        )
        .map_err(|_| infrastructure())?;
    }
    for required in &preview.required_incompatibilities {
        writeln!(
            io.stderr,
            "switch: requires entry={} part={} capability={}",
            required.entry_id,
            part(&required.part),
            capability(required.capability)
        )
        .map_err(|_| infrastructure())?;
    }
    Ok(())
}

fn model(model: &ModelRef) -> String {
    format!("{}/{}", line(&model.provider_instance), line(&model.model))
}

fn part(part: &Part) -> String {
    match part {
        Part::Entry => "entry".into(),
        Part::Block { index } => format!("block[{index}]"),
        Part::Continuation => "continuation".into(),
    }
}

fn capability(capability: RequiredCapability) -> &'static str {
    match capability {
        RequiredCapability::Image => "image",
        RequiredCapability::Audio => "audio",
        RequiredCapability::File => "file",
        RequiredCapability::Tools => "tools",
    }
}

fn rejection(error: Rejection) -> Failure {
    match error {
        Rejection::ContextBudgetExceeded { .. } => (1, "context_budget_exceeded".into()),
        Rejection::Compaction(e) => (1, e.code().into()),
        Rejection::Invalid(code) => configuration(code),
        other => configuration(format!("command_rejected: {other:?}")),
    }
}

async fn drive(
    handle: &SessionHandle,
    session: &SessionId,
    o: &Options,
    io: &mut Io<'_, '_>,
) -> Result<u8, Failure> {
    let auto_read = o.approve_read_only;
    let codex = o.codex;
    writeln!(io.stderr, "session: {session}").map_err(|_| infrastructure())?;
    if let Some(availability) = handle.tool_availability() {
        for (name, backend) in [
            ("shell", &availability.shell),
            ("file_tools", &availability.file_tools),
        ] {
            if let BackendAvailability::Unavailable { reasons } = backend {
                writeln!(
                    io.stderr,
                    "{name} unavailable: {}",
                    line(&reasons.join(", "))
                )
                .map_err(|_| infrastructure())?;
            }
        }
    }
    let mut subscription = handle.subscribe();
    // The subscription protocol yields a reset first; it is the only accepted
    // starting point, because events buffered before it cannot be reduced
    // without a view to reduce them into.
    let mut client = match subscription.next().await {
        Some(SubscriptionItem::Reset(client)) => client,
        _ => return Err((1, "subscription_closed".into())),
    };
    while client.view().phase == Phase::RecoveryRequired {
        update(&mut client, subscription.next().await, io)?;
    }
    if client.view().phase != Phase::Durable(RuntimeState::Idle) {
        return Err(configuration("session_requires_resolution"));
    }
    // A continuation that names another provider or model switches explicitly,
    // while idle and before the turn opens. The report is printed either way; a
    // refusal exits 2 having written nothing. Naming the branch's current model
    // is not a switch, so no `model_changed` entry is appended for it.
    match crate::wiring::switch_session(handle, &session_options(o)).await {
        Ok(Some(preview)) => switch_report(io, "switched", &preview)?,
        Ok(None) => {}
        Err(Rejection::PortabilityLoss(preview)) => {
            switch_report(io, "refused", &preview)?;
            return Err(configuration("portability_loss"));
        }
        Err(e) => return Err(rejection(e)),
    }
    let baseline = client.view().snapshot.sequence();
    let start = handle.command(Command::StartTurn {
        command_id: CommandId::new(),
        content: vec![ContentBlock::Text {
            text: o.prompt.clone(),
        }],
    });
    tokio::pin!(start);
    let mut cancelled = false;
    let turn = loop {
        tokio::select! {
            biased;
            Some(()) = io.interrupts.recv() => {
                if cancelled { return Ok(130); }
                cancelled = true;
                let _ = handle.command(Command::CancelCompaction).await;
            }
            result = &mut start => {
                match result {
                    Ok(Accepted::TurnStarted { turn_id, .. }) => break turn_id,
                    Err(_) if cancelled => return Ok(130),
                    Err(e) => return Err(rejection(e)),
                    _ => return Err((1, "unexpected_start_reply".into())),
                }
            }
        }
    };
    if cancelled {
        let _ = handle
            .command(Command::Cancel {
                turn_id: turn.clone(),
            })
            .await;
    }
    let mut render = Renderer::default();
    let mut asked = BTreeSet::new();
    let mut pending: Option<PendingApproval> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    loop {
        // Reconcile accumulated partial and durable output after every update,
        // including lag resets. Never replay an already-written byte.
        render.view(client.view(), baseline, io)?;
        if let Some(finished) =
            client
                .view()
                .snapshot
                .events()
                .find_map(|(_, _, event)| match event {
                    JournalEvent::TurnFinished(f) if f.turn_id == turn => Some(f),
                    _ => None,
                })
        {
            let outcome = finished.outcome;
            let code = finished.code.as_deref().unwrap_or("completed");
            let usage: Vec<_> = render.usage.values().collect();
            // The usage line is the first diagnostic after the answer (F05).
            io.end_stdout_line().map_err(|_| infrastructure())?;
            writeln!(
                io.stderr,
                "usage: {}",
                serde_json::to_string(&usage).map_err(|_| infrastructure())?
            )
            .map_err(|_| infrastructure())?;
            writeln!(io.stderr, "turn: {}", line(code)).map_err(|_| infrastructure())?;
            if code == "authentication" && codex {
                writeln!(io.stderr, "run `codex login`").map_err(|_| infrastructure())?;
            }
            return Ok(if cancelled || outcome == TurnOutcome::Cancelled {
                130
            } else if code == "authentication" {
                3
            } else if outcome == TurnOutcome::Completed {
                0
            } else {
                1
            });
        }
        if client.view().phase == Phase::Faulted {
            return Err((1, "runtime_fault".into()));
        }
        if let Some(ticket) = &client.view().pending_approval
            && asked.insert(ticket.approval_id.clone())
            && !cancelled
        {
            let (name, arguments) = proposal(client.view(), &ticket.call)?;
            let mut summary = format!(
                "tool: {} {}",
                bounded(&name, 80),
                bounded(&arguments.to_string(), 320)
            );
            for row in crate::wiring::sandbox_summary(&ticket.plan) {
                summary.push('\n');
                summary.push_str(&row);
            }
            let decision = if io.terminal.is_none() {
                writeln!(io.stderr, "approval denied: no controlling terminal")
                    .map_err(|_| infrastructure())?;
                Some(false)
            } else if auto_read && read_only(&name) {
                writeln!(io.stderr, "approval: read-only auto-approved")
                    .map_err(|_| infrastructure())?;
                Some(true)
            } else if io
                .terminal
                .as_deref_mut()
                .expect("checked")
                .begin(&summary)
                .is_err()
            {
                writeln!(io.stderr, "approval denied: terminal unavailable")
                    .map_err(|_| infrastructure())?;
                Some(false)
            } else {
                None
            };
            if let Some(allow) = decision {
                submit(handle, ticket, allow).await?;
            } else {
                pending = Some(ticket.clone());
            }
        }
        if pending.as_ref().is_some_and(|p| {
            client
                .view()
                .pending_approval
                .as_ref()
                .is_none_or(|c| c.approval_id != p.approval_id)
        }) {
            pending = None;
        }
        tokio::select! {
            biased;
            Some(()) = io.interrupts.recv() => {
                if cancelled { return Ok(130); }
                cancelled = true;
                pending = None;
                let _ = handle.command(Command::Cancel { turn_id: turn.clone() }).await;
            }
            item = subscription.next() => update(&mut client, item, io)?,
            _ = tick.tick(), if pending.is_some() => {
                let decision = io.terminal.as_deref_mut().expect("pending terminal").poll().unwrap_or(Some(false));
                if let Some(allow) = decision {
                    let ticket = pending.take().expect("pending");
                    submit(handle, &ticket, allow).await?;
                }
            }
        }
    }
}
fn update(
    client: &mut ClientView,
    item: Option<SubscriptionItem>,
    io: &mut Io<'_, '_>,
) -> Result<(), Failure> {
    match item {
        Some(SubscriptionItem::Reset(c)) => *client = c,
        Some(SubscriptionItem::Event(e)) => {
            match &e.kind {
                EventKind::CompactionFailed { code } => {
                    writeln!(io.stderr, "compaction failed: {code}")
                        .map_err(|_| infrastructure())?
                }
                EventKind::CompactionCommitted { .. } => {
                    writeln!(io.stderr, "compaction: committed").map_err(|_| infrastructure())?
                }
                EventKind::Fault(cause) => {
                    let _ = io.end_stdout_line();
                    writeln!(
                        io.stderr,
                        "error: runtime_fault {}",
                        line(&format!("{cause:?}"))
                    )
                    .map_err(|_| infrastructure())?
                }
                _ => {}
            }
            client.apply(&e);
        }
        None => return Err((1, "subscription_closed".into())),
    }
    Ok(())
}
async fn submit(handle: &SessionHandle, p: &PendingApproval, allow: bool) -> Result<(), Failure> {
    match handle
        .command(Command::SubmitApproval {
            approval_id: p.approval_id.clone(),
            plan_digest: p.plan_digest.clone(),
            decision: if allow {
                Decision::Allow
            } else {
                Decision::Deny
            },
            remember: false,
        })
        .await
    {
        Ok(_) | Err(Rejection::NotActive) => Ok(()),
        Err(e) => Err(rejection(e)),
    }
}
fn proposal(view: &RuntimeView, call: &CallRef) -> Result<(String, Value), Failure> {
    for (_, stored) in view.snapshot.entries() {
        if let EntryPayload::AssistantResponse(response) = &stored.entry.payload {
            for block in &response.content {
                if let ContentBlock::ToolCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } = block
                    && call_id == &call.call_id
                {
                    return Ok((name.clone(), Value::Object(arguments.clone())));
                }
            }
        }
    }
    Err((1, "approval_proposal_missing".into()))
}
/// The one read-only classifier both clients use for `--approve-read-only`.
///
/// Only a registry tool whose effects are exactly `[Read]` on the trusted
/// filesystem qualifies, so an unknown name or any process/mutation effect is
/// never auto-approved. Today that is `read`, `list`, `glob` and `grep`.
pub(crate) fn read_only(name: &str) -> bool {
    Registry::builtin().definitions().any(|d| {
        d.name == name
            && d.effects.as_slice() == [EffectClass::Read]
            && d.backend == ExecutionBackend::TrustedFs
    })
}

#[derive(Default)]
struct Renderer {
    written: BTreeMap<(String, usize, bool), usize>,
    results: BTreeSet<String>,
    proposals: BTreeSet<String>,
    usage: BTreeMap<String, Usage>,
}
impl Renderer {
    fn text(
        &mut self,
        response: &str,
        index: usize,
        reasoning: bool,
        text: &str,
        io: &mut Io<'_, '_>,
    ) -> Result<(), Failure> {
        let offset = self
            .written
            .entry((response.into(), index, reasoning))
            .or_default();
        if text.len() <= *offset {
            return Ok(());
        }
        let delta = text.get(*offset..).ok_or_else(infrastructure)?;
        if reasoning {
            if io.stderr_is_tty {
                writeln!(io.stderr, "\x1b[2mreasoning: {}\x1b[0m", line(delta))
            } else {
                writeln!(io.stderr, "reasoning: {}", line(delta))
            }
            .map_err(|_| infrastructure())?;
        } else {
            io.stdout
                .write_all(delta.as_bytes())
                .and_then(|()| io.stdout.flush())
                .map_err(|_| infrastructure())?;
            io.stdout_open_line = !delta.ends_with('\n');
        }
        *offset = text.len();
        Ok(())
    }
    fn view(
        &mut self,
        view: &RuntimeView,
        baseline: u64,
        io: &mut Io<'_, '_>,
    ) -> Result<(), Failure> {
        let mut entries: Vec<_> = view
            .snapshot
            .entries()
            .map(|(_, s)| s)
            .filter(|s| s.sequence > baseline)
            .collect();
        entries.sort_by_key(|s| (s.sequence, s.entry_index));
        for stored in entries {
            match &stored.entry.payload {
                EntryPayload::AssistantResponse(r) => {
                    let id = r.response_id.to_string();
                    // Calls/state can disappear on cancellation. Text/reasoning
                    // offsets are indexed by their own kind, not journal indices.
                    let mut indices = [0, 0];
                    for block in &r.content {
                        let (reasoning, text) = match block {
                            ContentBlock::Text { text } => (false, text),
                            ContentBlock::ReasoningSummary { text } => (true, text),
                            ContentBlock::ToolCall {
                                call_id,
                                name,
                                arguments,
                                ..
                            } => {
                                if self.proposals.insert(call_id.to_string()) {
                                    writeln!(
                                        io.stderr,
                                        "tool: {} {}",
                                        bounded(name, 80),
                                        bounded(&Value::Object(arguments.clone()).to_string(), 320)
                                    )
                                    .map_err(|_| infrastructure())?;
                                }
                                continue;
                            }
                            _ => continue,
                        };
                        self.text(&id, indices[reasoning as usize], reasoning, text, io)?;
                        indices[reasoning as usize] += 1;
                    }
                    if let Some(usage) = &r.usage {
                        self.usage.insert(id, usage.clone());
                    }
                }
                EntryPayload::ToolResult(r)
                    if self.results.insert(r.result.call.call_id.to_string()) =>
                {
                    writeln!(
                        io.stderr,
                        "tool result: {:?} {}",
                        r.result.status,
                        r.result.metadata.code.as_deref().unwrap_or("succeeded")
                    )
                    .map_err(|_| infrastructure())?;
                }
                _ => {}
            }
        }
        if let Some(partial) = &view.partial {
            let mut indices = [0, 0];
            for block in partial.blocks.values() {
                let (reasoning, text) = match block {
                    PartialBlock::Text(t) => (false, t),
                    PartialBlock::ReasoningSummary(t) => (true, t),
                    _ => continue,
                };
                self.text(
                    &partial.response_id.to_string(),
                    indices[reasoning as usize],
                    reasoning,
                    text,
                    io,
                )?;
                indices[reasoning as usize] += 1;
            }
        }
        Ok(())
    }
}
/// Backwards-compatible location of the shared default resolver.
pub use crate::wiring::default_data_root;

fn session_options(o: &Options) -> crate::wiring::SessionOptions {
    crate::wiring::SessionOptions {
        data_root: o.data_root.clone(),
        workspace: o.workspace.clone(),
        session: o.session.clone(),
        model: o.model.clone(),
        explicit_model: o.explicit_model,
        explicit_provider: o.explicit_provider,
        switch_policy: o.switch_policy,
        known_instances: o.known_instances.clone(),
        summary_model: o.summary_model.clone(),
        compaction_strategy: o.compaction_strategy,
        explicit_strategy: o.explicit_strategy,
        driver: o.driver.clone(),
        sandbox: o.sandbox.clone(),
    }
}

/// `--approve-read-only` covers exactly the registry's read-only trusted tools.
#[cfg(test)]
mod read_only_tests {
    use super::read_only;

    #[test]
    fn only_registry_read_only_trusted_tools_are_auto_approvable() {
        for name in ["read", "list", "glob", "grep"] {
            assert!(read_only(name), "{name} must stay auto-approvable");
        }
        for name in ["write", "edit", "bash", "unknown-tool", "", "Read", "cargo"] {
            assert!(!read_only(name), "{name} must never be auto-approved");
        }
    }
}
