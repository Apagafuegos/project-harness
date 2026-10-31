//! Thin asynchronous terminal shell. The engine remains the only session owner.
use super::{Action, App, inspector_shell};
use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, EnableBracketedPaste, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::{Stream, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use harness_core::runtime::{Accepted, Command, Phase, Rejection, SessionHandle, SubscriptionItem};
use harness_core::{
    context::{ContextReport, compaction::CompactionPreview},
    runtime::InspectError,
    session::{EntryId, ModelRef},
};
use ratatui::{Terminal, backend::Backend};
use std::{
    io::{self, IsTerminal, Write},
    sync::{
        Once,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static HOOK: Once = Once::new();

/// Owns the process terminal, including partial setup and panic restoration.
/// Only one guard can be active. No terminal mutation happens on a non-TTY.
pub struct TerminalGuard;
impl TerminalGuard {
    /// Enter raw mode, the alternate screen and bracketed paste.
    pub fn enter() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::other("TUI requires a terminal; use harness ask"));
        }
        HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                Self::restore_terminal();
                previous(info);
            }));
        });
        if ACTIVE.swap(true, Ordering::SeqCst) {
            return Err(io::Error::other("terminal already owned"));
        }
        let guard = Self;
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        Ok(guard)
    }

    /// Idempotent restoration, also used immediately on SIGTERM/SIGHUP.
    pub fn restore(&mut self) {
        Self::restore_terminal();
    }

    fn restore_terminal() {
        if ACTIVE.swap(false, Ordering::SeqCst) {
            // Restore termios before any diagnostic (including the panic hook).
            let _ = disable_raw_mode();
            let mut out = io::stdout();
            let _ = execute!(out, DisableBracketedPaste, LeaveAlternateScreen, Show);
            let _ = out.flush();
        }
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Injectable command endpoint. Futures are polled by the loop, never awaited
/// inline; storage latency cannot stop keys, subscription reduction or ticks.
pub trait Commands {
    /// Submit one typed command without retries.
    fn command(&self, command: Command) -> BoxFuture<'static, Result<Accepted, Rejection>>;
    /// Execute an edit's SelectHead before its StartTurn, stopping on refusal.
    fn sequence(&self, commands: Vec<Command>) -> BoxFuture<'static, Result<Accepted, Rejection>> {
        super::runtime_nav::sequence(
            commands
                .into_iter()
                .map(|command| self.command(command))
                .collect(),
        )
    }
    /// Shell-only session navigation, unsupported by a bare engine endpoint.
    fn navigation(&self, _action: Action) -> super::runtime_nav::NavigationFuture {
        Box::pin(async { Err("session navigation unavailable".into()) })
    }
    /// Read a context projection without blocking the terminal loop.
    fn inspect_context(
        &self,
        _head: Option<EntryId>,
        _model_override: Option<ModelRef>,
    ) -> BoxFuture<'static, Result<ContextReport, InspectError>> {
        Box::pin(async { Err(InspectError::Closed) })
    }
    /// Read a live compaction proposal, never regenerate it.
    fn inspect_preview(
        &self,
        _id: EntryId,
    ) -> BoxFuture<'static, Result<CompactionPreview, Rejection>> {
        Box::pin(async {
            Err(Rejection::StalePreview {
                field: "preview_id",
            })
        })
    }
    /// Optional continuation switch, submitted once recovery reaches idle.
    fn startup(&self) -> Option<Command> {
        None
    }
}
impl Commands for SessionHandle {
    fn inspect_context(
        &self,
        head: Option<EntryId>,
        model_override: Option<ModelRef>,
    ) -> BoxFuture<'static, Result<ContextReport, InspectError>> {
        let handle = self.clone();
        Box::pin(async move { handle.inspect_context(head, model_override).await })
    }
    fn inspect_preview(
        &self,
        id: EntryId,
    ) -> BoxFuture<'static, Result<CompactionPreview, Rejection>> {
        let handle = self.clone();
        Box::pin(async move { handle.inspect_context_preview(id).await })
    }
    fn command(&self, command: Command) -> BoxFuture<'static, Result<Accepted, Rejection>> {
        let handle = self.clone();
        Box::pin(async move { handle.command(command).await })
    }
}

/// External signal, independent of raw-mode keyboard Ctrl-C.
#[derive(Clone, Copy, Debug)]
pub enum Signal {
    /// Equivalent to a Ctrl-C key.
    Interrupt,
    /// Restore immediately, then cancel durably and exit.
    Terminate,
}

/// Observable loop counters, useful for deterministic nonblocking tests.
#[derive(Default, Debug)]
pub struct Stats {
    /// Render ticks serviced (including clean ticks which draw nothing).
    pub ticks: usize,
    /// Ticks serviced while command replies were outstanding.
    pub pending_ticks: usize,
    /// Frames actually rendered.
    pub frames: usize,
}

/// Drive one app with injectable terminal, input, subscription and commands.
/// The caller always shuts down the handle after return (including errors).
/// A quit drains pending commands and observes durable turn closure first.
pub async fn drive<B, E, S, G, H>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    events: E,
    subscriptions: S,
    signals: G,
    handle: &H,
    mut restore: impl FnMut(),
) -> io::Result<Stats>
where
    B: Backend,
    E: Stream<Item = io::Result<Event>>,
    S: Stream<Item = SubscriptionItem>,
    G: Stream<Item = Signal>,
    H: Commands,
{
    futures_util::pin_mut!(events, subscriptions, signals);
    let mut pending = FuturesUnordered::new();
    let mut navigation = super::runtime_nav::PendingNavigation::new();
    let mut inspections = inspector_shell::Inspections::new();
    let mut tick = tokio::time::interval(Duration::from_millis(16));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut quitting = false;
    let mut restored = false;
    let mut stats = Stats::default();
    let mut startup = handle.startup();
    let mut events_open = true;
    let mut signals_open = true;
    loop {
        if !quitting {
            for action in app.inspector_refresh() {
                inspector_shell::dispatch(action, handle, &mut inspections);
            }
        }
        if !quitting
            && app.view().is_some_and(|v| {
                v.phase == Phase::Durable(harness_core::session::RuntimeState::Idle)
            })
            && let Some(command) = startup.take()
        {
            let unchanged = matches!(&command, Command::SwitchModel { model, .. }
                if app.view().is_some_and(|v| v.snapshot.model_at(v.snapshot.head()).ok() == Some(model)));
            if !unchanged {
                pending.push(handle.command(command));
            }
        }
        if quitting
            && pending.is_empty()
            && navigation.is_empty()
            && app.view().is_some_and(|v| {
                v.active_turn.is_none()
                    || v.phase == Phase::Faulted
                    || v.phase == Phase::AwaitingResolution
            })
        {
            return Ok(stats);
        }
        tokio::select! {
            item = subscriptions.next() => {
                match item {
                    Some(item) => app.on_subscription(item),
                    None => return Err(io::Error::other("subscription_closed")),
                }
                dirty = true;
            }
            inspection = inspections.next(), if !inspections.is_empty() => {
                inspector_shell::deliver(app, inspection.expect("pending inspection"));
                dirty = true;
            }
            reply = pending.next(), if !pending.is_empty() => {
                let reply = reply.expect("pending command");
                app.inspector_reply(&reply);
                match reply {
                    Ok(Accepted::TurnStarted { turn_id, .. }) if quitting => {
                        pending.push(handle.command(Command::Cancel { turn_id }));
                    }
                    Ok(_) if quitting || app.view().is_some_and(|v| v.phase == Phase::Faulted) => {},
                    Ok(accepted) => app.navigation_accepted(&accepted),
                    // Quit's best-effort CancelCompaction finds nothing to cancel
                    // when no compaction is running; that must not replace a
                    // refusal the user still needs to read.
                    Err(Rejection::NotActive) if quitting => {},
                    Err(error) => if !app.inspector_rejection(&error) { app.navigation_rejected(error); },
                }
                dirty = true;
            }
            result = navigation.next(), if !navigation.is_empty() => {
                if let Some(replacement) = super::runtime_nav::apply(app, result.expect("pending navigation")) { startup = replacement; }
                dirty = true;
            }
            signal = signals.next(), if signals_open => {
                match signal {
                    Some(Signal::Terminate) => {
                        restore();
                        restored = true;
                        quitting = true;
                        cancel_pending_compaction(handle, &mut pending);
                        if let Some(turn_id) = app.view().and_then(|v| v.active_turn.clone()) {
                            pending.push(handle.command(Command::Cancel { turn_id }));
                        }
                    }
                    Some(Signal::Interrupt) if !quitting => {
                        let actions = app.on_key(crossterm::event::KeyEvent::new(
                            crossterm::event::KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL));
                        dispatch(actions, handle, &mut pending, &mut navigation, &mut inspections, &mut quitting);
                    }
                    Some(_) => {}
                    None => signals_open = false,
                }
                dirty = true;
            }
            event = events.next(), if events_open && app.view().is_some() => {
                match event {
                    Some(Ok(Event::Key(key))) if !quitting => {
                        dispatch(app.on_key(key), handle, &mut pending, &mut navigation, &mut inspections, &mut quitting);
                    }
                    Some(Ok(Event::Paste(text))) if !quitting => app.on_paste(&text),
                    Some(Ok(Event::Resize(width, height))) => {
                        terminal.resize(ratatui::layout::Rect::new(0, 0, width, height)).map_err(|_| io::Error::other("terminal resize failed"))?;
                        if !restored {
                            terminal.draw(|frame| app.render(frame)).map_err(|_| io::Error::other("terminal draw failed"))?;
                            stats.frames += 1;
                        }
                    }
                    Some(Err(error)) => return Err(error),
                    None => {
                        events_open = false;
                        quitting = true;
                        cancel_pending_compaction(handle, &mut pending);
                        if let Some(turn_id) = app.view().and_then(|v| v.active_turn.clone()) {
                            pending.push(handle.command(Command::Cancel { turn_id }));
                        }
                    }
                    _ => {}
                }
                dirty = true;
            }
            _ = tick.tick() => {
                stats.ticks += 1;
                if !pending.is_empty() || !navigation.is_empty() { stats.pending_ticks += 1; }
                if dirty && !restored {
                    terminal.draw(|frame| app.render(frame)).map_err(|_| io::Error::other("terminal draw failed"))?;
                    stats.frames += 1;
                    dirty = false;
                }
            }
        }
    }
}

type Pending = FuturesUnordered<BoxFuture<'static, Result<Accepted, Rejection>>>;
fn cancel_pending_compaction(handle: &impl Commands, pending: &mut Pending) {
    // A pending manual preview (or budget-held start) must be cancelled before
    // draining replies. Otherwise quit would wait for a stalled summarizer.
    if !pending.is_empty() {
        pending.push(handle.command(Command::CancelCompaction));
    }
}
fn dispatch(
    actions: Vec<Action>,
    handle: &impl Commands,
    pending: &mut Pending,
    navigation: &mut super::runtime_nav::PendingNavigation,
    inspections: &mut inspector_shell::Inspections,
    quitting: &mut bool,
) {
    let mut commands = Vec::new();
    for action in actions {
        match action {
            Action::Command(command) => commands.push(command),
            action @ (Action::ListSessions | Action::OpenSession(_) | Action::NewSession) => {
                super::runtime_nav::dispatch(action, handle, navigation)
            }
            action @ (Action::InspectContext { .. } | Action::InspectPreview(_)) => {
                inspector_shell::dispatch(action, handle, inspections)
            }
            Action::Quit { cancel } => {
                *quitting = true;
                cancel_pending_compaction(handle, pending);
                if let Some(turn_id) = cancel {
                    pending.push(handle.command(Command::Cancel { turn_id }));
                }
            }
        }
    }
    if !commands.is_empty() {
        pending.push(handle.sequence(commands));
    }
}

pub(super) fn rejection(error: Rejection) -> String {
    // Never Debug-format arbitrary provider data or the portability report.
    match error {
        Rejection::Invalid(code) => format!("command rejected: {code}"),
        Rejection::Busy => "command rejected: busy".into(),
        Rejection::RecoveryRequired => "command rejected: recovery_required".into(),
        Rejection::ResolutionRequired => "command rejected: resolution_required".into(),
        Rejection::NotActive => "command rejected: not active".into(),
        Rejection::Faulted => "command rejected: runtime fault".into(),
        Rejection::ContextBudgetExceeded { .. } => "context_budget_exceeded".into(),
        Rejection::PortabilityLoss(_) => {
            "portability_loss: switch refused; saved model retained".into()
        }
        _ => "command rejected".into(),
    }
}
