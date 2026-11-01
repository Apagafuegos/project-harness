//! A pure terminal-UI model: state, keys, actions and rendering.
//! The separate [`runtime`] shell owns terminal I/O and Tokio; [`App`] does not.
//!
//! The shell moves [`SubscriptionItem`]s and [`KeyEvent`]s in and [`Action`]s
//! out. [`App::render`] draws into a Ratatui [`Frame`] without touching a
//! backend, so tests can assert on a [`ratatui::backend::TestBackend`] buffer.
//!
//! The app never decides an approval, builds a provider request or runs a
//! tool. It submits the user's decision and renders what the runtime reports.
//!
//! See `docs/decisions/0023-tui-architecture.md`.

mod inspector;
mod inspector_shell;
mod navigation;
mod render;
pub mod runtime;
pub mod runtime_nav;
mod transcript;

use std::{collections::BTreeMap, path::PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use harness_core::{
    context::{Flags, Totals},
    runtime::{
        ApplyOutcome, ClientView, Command, Phase, Rejection, RuntimeDiagnostic, RuntimeView,
        SubscriptionItem, UncertainInvocation, UserResolution,
    },
    session::{
        ApprovalId, CommandId, ContentBlock, ContextMode, Decision, EntryPayload, InvocationId,
        ModelRef, RuntimeState, StoredEntry, ToolStatus, TurnId,
    },
};
use ratatui::{Frame, text::Line};

use self::transcript::Notice;
use crate::text::{bounded, line};

/// Static session facts the view does not carry.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    /// Authorized workspace, shown in the status bar.
    pub workspace: PathBuf,
    /// Model the session was opened with; the branch's model wins once known.
    pub model: ModelRef,
    /// Sourced context limit for [`Self::model`], when the catalog knows one.
    pub context_limit: Option<u64>,
}

/// One instruction for the shell that owns the terminal and the session handle.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Submit exactly one core command. The shell reports a rejection in the
    /// app's error banner; the app never retries.
    Command(Command),
    /// Scan the read-only session catalog.
    ListSessions,
    /// Durably hand over to another session in this workspace.
    OpenSession(harness_core::session::SessionId),
    /// Create a session with the current workspace and flags.
    NewSession,
    /// Read-only projection request, executed asynchronously by the shell.
    InspectContext {
        /// Historical head, or the current head.
        head: Option<harness_core::session::EntryId>,
        /// Preview only; never switches the session model.
        model_override: Option<ModelRef>,
    },
    /// Read an existing ephemeral compaction proposal by identity, without
    /// regenerating it. Reserved for clients that retain an ID across resets;
    /// manual proposals already arrive in the `CompactionPreviewed` reply.
    InspectPreview(harness_core::session::EntryId),
    /// Leave the client. When `cancel` names a turn, the shell must submit
    /// `Cancel` for it and wait for durable cancellation before restoring the
    /// terminal and exiting.
    Quit {
        /// Turn whose durable cancellation must precede exit.
        cancel: Option<TurnId>,
    },
}

/// Which input the app is currently capturing.
#[derive(Clone, Debug, Default)]
enum Modal {
    /// No modal; normal editing.
    #[default]
    None,
    /// A durable approval ticket is pending and captures every key.
    Approval {
        /// Ticket the answer is bound to.
        approval_id: ApprovalId,
    },
    /// The runtime awaits a statement about an uncertain invocation.
    Uncertain {
        /// Selected entry in the view's `awaiting` list.
        index: usize,
        /// Selected resolution choice.
        mode: ResolutionMode,
        /// The user's account of what happened.
        input: String,
    },
    /// The user asked to quit while idle; confirm before leaving.
    ConfirmQuit,
}

/// The core's user-statement choices, restricted to what a terminal can offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolutionMode {
    /// Accept not knowing; recorded as `uncertain`.
    AcknowledgedUnknown,
    /// The user checked and the call succeeded.
    ObservedSucceeded,
    /// The user checked and the call failed.
    ObservedFailed,
}

impl ResolutionMode {
    fn next(self) -> Self {
        match self {
            Self::AcknowledgedUnknown => Self::ObservedSucceeded,
            Self::ObservedSucceeded => Self::ObservedFailed,
            Self::ObservedFailed => Self::AcknowledgedUnknown,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::AcknowledgedUnknown => Self::ObservedFailed,
            Self::ObservedSucceeded => Self::AcknowledgedUnknown,
            Self::ObservedFailed => Self::ObservedSucceeded,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::AcknowledgedUnknown => "accept that the outcome is unknown",
            Self::ObservedSucceeded => "observed: succeeded",
            Self::ObservedFailed => "observed: failed",
        }
    }
}

/// The pure client model. One instance owns one session's screen.
pub struct App {
    navigation: navigation::Navigation,
    inspector: inspector::Inspector,
    info: SessionInfo,
    catalog: harness_core::context::ModelCatalog,
    view: Option<ClientView>,
    transcript: Vec<Line<'static>>,
    notices: Vec<Notice>,
    prompt: String,
    /// Cursor position in `prompt`, counted in characters.
    cursor: usize,
    /// Transcript lines hidden below the viewport; `0` follows the tail.
    scroll: usize,
    /// Lines one `PageUp`/`PageDown` moves; refreshed from the last layout.
    page: usize,
    modal: Modal,
    last_error: Option<String>,
    /// A Ctrl-C already cancelled the current busy period.
    interrupt_armed: bool,
    /// Show full reasoning summaries instead of one line.
    reasoning: bool,
    /// Latest built-context estimate, absent after a reset.
    context: Option<(Totals, Flags)>,
}

impl App {
    /// Create an app for a session. The first subscription item must be a
    /// `Reset`; until then the screen shows an empty, connecting state.
    pub fn new(info: SessionInfo) -> Self {
        Self {
            navigation: Default::default(),
            inspector: Default::default(),
            info,
            catalog: Default::default(),
            view: None,
            transcript: Vec::new(),
            notices: Vec::new(),
            prompt: String::new(),
            cursor: 0,
            scroll: 0,
            page: 10,
            modal: Modal::None,
            last_error: None,
            interrupt_armed: false,
            reasoning: false,
            context: None,
        }
    }

    /// Supply the merged, sourced catalog for the current and switched models.
    pub fn set_catalog(&mut self, catalog: harness_core::context::ModelCatalog) {
        self.catalog = catalog;
    }

    /// Insert bracketed paste atomically; modal input never becomes a command.
    pub fn on_paste(&mut self, text: &str) {
        if self.navigation_captures_input() || self.inspector_captures() {
            return;
        }
        let text: String = text
            .chars()
            .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
            .collect();
        match &mut self.modal {
            Modal::None => {
                let at = self.byte_index(self.cursor);
                self.prompt.insert_str(at, &text);
                self.cursor += text.chars().count();
            }
            Modal::Uncertain { input, .. } => input.push_str(&text),
            _ => {}
        }
    }

    /// The current view, once a reset has arrived.
    pub fn view(&self) -> Option<&RuntimeView> {
        self.view.as_ref().map(ClientView::view)
    }

    /// Replace the error banner. The shell calls this with a sanitized
    /// rejection, never with a provider message or a secret.
    pub fn set_error(&mut self, error: impl Into<String>) {
        self.last_error = Some(bounded(&error.into(), 240));
    }

    /// Clear the error banner.
    pub fn clear_error(&mut self) {
        self.last_error = None;
    }

    /// Reduce one subscription item into state.
    ///
    /// A `Reset` replaces the whole view and recomputes the transcript from the
    /// snapshot branch plus the live partial response. It never appends to the
    /// previous transcript, so lag, epoch changes and gaps cannot duplicate
    /// rows. An `Event` is applied to the current view; an event that does not
    /// fit (a gap the shell has not yet resynced) is ignored until the next
    /// `Reset`.
    pub fn on_subscription(&mut self, item: SubscriptionItem) {
        match item {
            SubscriptionItem::Reset(client) => {
                if client.view().phase == Phase::Faulted {
                    self.set_error("runtime fault");
                }
                self.view = Some(client);
                self.reset_inspector();
                self.notices.clear();
                self.context = None;
                self.scroll = 0;
                self.rebuild();
            }
            SubscriptionItem::Event(event) => {
                let Some(client) = self.view.as_mut() else {
                    return;
                };
                if client.apply(&event) != ApplyOutcome::Applied {
                    return;
                }
                self.observe_inspector(&event.kind);
                self.observe(&event.kind);
                self.rebuild();
            }
        }
        if self
            .view
            .as_ref()
            .is_none_or(|client| client.view().active_turn.is_none())
        {
            self.interrupt_armed = false;
        }
        self.sync_modal();
    }

    /// Turn one key press into state changes and shell actions. Never performs
    /// I/O, and never builds a provider request or decides an approval.
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        if let Some(actions) = self.navigation_key(key) {
            return actions;
        }
        if let Some(actions) = self.inspector_key(key) {
            return actions;
        }
        match self.modal {
            Modal::Approval { .. } => self.approval_key(key),
            Modal::Uncertain { .. } => self.uncertain_key(key),
            Modal::ConfirmQuit => self.confirm_quit_key(key),
            Modal::None => self.editor_key(key),
        }
    }

    /// Draw the transcript, prompt, error banner, status bar and any modal.
    ///
    /// Rendering never panics on a small area: panes are laid out with
    /// saturating heights and a zero-height pane draws nothing.
    pub fn render(&mut self, frame: &mut Frame) {
        render::draw(self, frame);
        self.render_navigation(frame);
        self.render_inspector(frame);
    }

    fn rebuild(&mut self) {
        self.transcript = match &self.view {
            Some(client) => transcript::project(client.view(), self.reasoning, &self.notices),
            None => Vec::new(),
        };
    }

    /// Keep the modal in step with the view. Approval takes precedence over
    /// resolution, and a confirm-quit dialog only survives while it is still
    /// the only thing asking for input.
    fn sync_modal(&mut self) {
        let Some(view) = self.view.as_ref().map(ClientView::view) else {
            return;
        };
        if let Some(ticket) = &view.pending_approval {
            match &self.modal {
                Modal::Approval { approval_id } if *approval_id == ticket.approval_id => {}
                _ => {
                    self.modal = Modal::Approval {
                        approval_id: ticket.approval_id.clone(),
                    }
                }
            }
        } else if view.phase == Phase::AwaitingResolution && !view.awaiting.is_empty() {
            match &mut self.modal {
                Modal::Uncertain { index, .. } => {
                    *index = (*index).min(view.awaiting.len() - 1);
                }
                _ => {
                    self.modal = Modal::Uncertain {
                        index: 0,
                        mode: ResolutionMode::AcknowledgedUnknown,
                        input: String::new(),
                    }
                }
            }
        } else if !matches!(self.modal, Modal::ConfirmQuit) {
            self.modal = Modal::None;
        }
    }

    fn observe(&mut self, kind: &harness_core::runtime::EventKind) {
        use harness_core::runtime::EventKind;
        match kind {
            EventKind::Diagnostic(diagnostic) => {
                self.notices
                    .push(Notice::Diagnostic(diagnostic_text(diagnostic)));
            }
            EventKind::Fault(cause) => {
                let text = fault_text(cause);
                self.notices.push(Notice::Fault(text.clone()));
                self.last_error = Some(text);
            }
            EventKind::CompactionFailed { code } => self
                .notices
                .push(Notice::CompactionFailed((*code).to_owned())),
            EventKind::CompactionCommitted { .. } => {}
            EventKind::CompactionPreviewed { automatic, .. } => {
                if *automatic {
                    self.notices
                        .push(Notice::Info("compaction previewed".into()));
                }
            }
            EventKind::Recovered { code, .. } => self.notices.push(Notice::Recovered(format!(
                "recovery closed a turn ({code})"
            ))),
            EventKind::ContextBuilt { totals, flags, .. } => {
                self.context = Some((totals.clone(), *flags));
            }
            _ => {}
        }
    }

    fn active_turn(&self) -> Option<TurnId> {
        self.view
            .as_ref()
            .and_then(|client| client.view().active_turn.clone())
    }

    fn is_idle(&self) -> bool {
        self.view.as_ref().is_some_and(|client| {
            let view = client.view();
            view.phase == Phase::Durable(RuntimeState::Idle) && view.active_turn.is_none()
        })
    }

    fn editor_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter if shift || alt => {
                self.insert('\n');
                Vec::new()
            }
            KeyCode::Enter if !control => self.send(),
            KeyCode::Char('c') if control => self.interrupt(),
            KeyCode::Char('d') if control => {
                if self.prompt.is_empty() {
                    vec![Action::Quit {
                        cancel: self.active_turn(),
                    }]
                } else {
                    Vec::new()
                }
            }
            KeyCode::Char('r') if control => {
                self.reasoning = !self.reasoning;
                self.rebuild();
                Vec::new()
            }
            KeyCode::Esc => match self.active_turn() {
                Some(turn_id) => vec![Action::Command(Command::Cancel { turn_id })],
                None => Vec::new(),
            },
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_add(self.page.max(1));
                Vec::new()
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(self.page.max(1));
                Vec::new()
            }
            KeyCode::Up if control => {
                self.scroll = self.scroll.saturating_add(1);
                Vec::new()
            }
            KeyCode::Down if control => {
                self.scroll = self.scroll.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Up => {
                self.move_vertical(false);
                Vec::new()
            }
            KeyCode::Down => {
                self.move_vertical(true);
                Vec::new()
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.prompt.chars().count());
                Vec::new()
            }
            KeyCode::Home => {
                self.cursor = self.row_start();
                Vec::new()
            }
            KeyCode::End => {
                self.cursor = self.row_end();
                Vec::new()
            }
            KeyCode::Backspace => {
                self.backspace();
                Vec::new()
            }
            KeyCode::Delete => {
                self.delete();
                Vec::new()
            }
            KeyCode::Char(c) if !control && !alt => {
                if !c.is_control() {
                    self.insert(c);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn send(&mut self) -> Vec<Action> {
        if !self.is_idle() || self.prompt.is_empty() {
            return Vec::new();
        }
        let text = std::mem::take(&mut self.prompt);
        self.cursor = 0;
        self.scroll = 0;
        let mut actions = self.navigation_send();
        actions.push(Action::Command(Command::StartTurn {
            command_id: CommandId::new(),
            content: vec![ContentBlock::Text { text }],
        }));
        actions
    }

    fn interrupt(&mut self) -> Vec<Action> {
        match self.active_turn() {
            Some(turn_id) if self.interrupt_armed => vec![Action::Quit {
                cancel: Some(turn_id),
            }],
            Some(turn_id) => {
                self.interrupt_armed = true;
                vec![Action::Command(Command::Cancel { turn_id })]
            }
            // Nothing is running; ask before leaving so a stray Ctrl-C does not
            // discard a typed prompt.
            None => {
                self.modal = Modal::ConfirmQuit;
                Vec::new()
            }
        }
    }

    fn approval_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let Some(ticket) = self
            .view
            .as_ref()
            .and_then(|client| client.view().pending_approval.clone())
        else {
            return Vec::new();
        };
        let (decision, remember) = match key.code {
            KeyCode::Char('a') if !control && !alt => (Decision::Allow, false),
            KeyCode::Char('r') if !control && !alt => (Decision::Allow, true),
            KeyCode::Char('d') if !control && !alt => (Decision::Deny, false),
            // Dismissing an approval denies it; it never silently allows.
            KeyCode::Esc => (Decision::Deny, false),
            _ => return Vec::new(),
        };
        vec![Action::Command(Command::SubmitApproval {
            approval_id: ticket.approval_id,
            plan_digest: ticket.plan_digest,
            decision,
            remember,
        })]
    }

    fn uncertain_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let Modal::Uncertain { index, mode, input } = &mut self.modal else {
            return Vec::new();
        };
        let awaiting: Vec<UncertainInvocation> = self
            .view
            .as_ref()
            .map(|client| client.view().awaiting.clone())
            .unwrap_or_default();
        if awaiting.is_empty() {
            return Vec::new();
        }
        *index = (*index).min(awaiting.len() - 1);
        match key.code {
            KeyCode::Up => *index = index.saturating_sub(1),
            KeyCode::Down => *index = (*index + 1).min(awaiting.len() - 1),
            KeyCode::Left | KeyCode::BackTab => *mode = mode.previous(),
            KeyCode::Right | KeyCode::Tab => *mode = mode.next(),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) if !control && !alt => {
                if !c.is_control() {
                    input.push(c);
                }
            }
            KeyCode::Enter => {
                let resolution = match *mode {
                    ResolutionMode::AcknowledgedUnknown => {
                        Some(UserResolution::AcknowledgedUnknown)
                    }
                    ResolutionMode::ObservedSucceeded | ResolutionMode::ObservedFailed
                        if !input.is_empty() =>
                    {
                        Some(UserResolution::Observed {
                            status: if *mode == ResolutionMode::ObservedSucceeded {
                                ToolStatus::Succeeded
                            } else {
                                ToolStatus::Failed
                            },
                            text: input.clone(),
                        })
                    }
                    _ => None,
                };
                if let Some(resolution) = resolution {
                    let invocation_id: InvocationId = awaiting[*index].invocation_id.clone();
                    return vec![Action::Command(Command::ResolveUncertain {
                        invocation_id,
                        resolution,
                    })];
                }
            }
            // There is no safe default resolution; Esc keeps the dialog open.
            KeyCode::Esc => {}
            _ => {}
        }
        Vec::new()
    }

    fn confirm_quit_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') if !control => {
                self.modal = Modal::None;
                vec![Action::Quit {
                    cancel: self.active_turn(),
                }]
            }
            KeyCode::Char('c') if control => {
                self.modal = Modal::None;
                vec![Action::Quit {
                    cancel: self.active_turn(),
                }]
            }
            KeyCode::Char('n') if !control => {
                self.modal = Modal::None;
                Vec::new()
            }
            KeyCode::Esc => {
                self.modal = Modal::None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.prompt
            .char_indices()
            .nth(char_index)
            .map_or(self.prompt.len(), |(index, _)| index)
    }

    fn insert(&mut self, c: char) {
        let at = self.byte_index(self.cursor);
        self.prompt.insert(at, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let at = self.byte_index(self.cursor);
        self.prompt.remove(at);
    }

    fn delete(&mut self) {
        if self.cursor >= self.prompt.chars().count() {
            return;
        }
        let at = self.byte_index(self.cursor);
        self.prompt.remove(at);
    }

    /// Character offset of the first character of the cursor's line.
    fn row_start(&self) -> usize {
        let before: Vec<char> = self.prompt.chars().take(self.cursor).collect();
        match before.iter().rposition(|c| *c == '\n') {
            Some(index) => index + 1,
            None => 0,
        }
    }

    /// Character offset one past the cursor's line.
    fn row_end(&self) -> usize {
        self.prompt
            .chars()
            .enumerate()
            .skip(self.cursor)
            .find(|(_, c)| *c == '\n')
            .map_or_else(|| self.prompt.chars().count(), |(index, _)| index)
    }

    fn move_vertical(&mut self, down: bool) {
        let row = self.row_start();
        let column = self.cursor - row;
        let line_end = self.row_end();
        if down {
            if line_end >= self.prompt.chars().count() {
                return;
            }
            let next = line_end + 1;
            let next_end = self
                .prompt
                .chars()
                .enumerate()
                .skip(next)
                .find(|(_, c)| *c == '\n')
                .map_or_else(|| self.prompt.chars().count(), |(index, _)| index);
            self.cursor = next + column.min(next_end.saturating_sub(next));
        } else {
            if row == 0 {
                return;
            }
            let previous = self.row_start() - 1;
            let previous_start = self.prompt.chars().take(previous).collect::<Vec<_>>();
            let previous_start = match previous_start.iter().rposition(|c| *c == '\n') {
                Some(index) => index + 1,
                None => 0,
            };
            self.cursor = previous_start + column.min(previous - previous_start);
        }
    }
}

/// The effective mode of every turn on an ancestry, folded from its
/// `context_control` entries: the last control for a turn on this ancestry wins
/// (decision 5). A turn with no control in effect has `ContextMode::Default`.
///
/// Core folds the same map for storage and the builder; the client repeats the
/// fold over the entries it already has because entries are immutable and no
/// payload is read. Controls on sibling branches are never on this ancestry, so
/// they never apply here either.
pub(crate) fn turn_modes(ancestry: &[&StoredEntry]) -> BTreeMap<TurnId, ContextMode> {
    let mut modes = BTreeMap::new();
    for stored in ancestry {
        if let EntryPayload::ContextControl(control) = &stored.entry.payload {
            modes.insert(control.turn_id.clone(), control.mode);
        }
    }
    modes
}

/// The refusal a `SetContextControl` for this turn would meet, from state the
/// app already has, so a key that cannot work sends nothing. The order is the
/// engine's own: a control is idle-only first (`busy`), and only then does the
/// target turn have to be finished (`context_control_unfinished_turn`). `None`
/// means the client cannot decide and the core must: every other semantic rule
/// stays the engine's, and its code is shown verbatim.
pub(crate) fn control_refusal(view: &RuntimeView, turn: &TurnId) -> Option<Rejection> {
    if view.phase != Phase::Durable(RuntimeState::Idle) || view.active_turn.is_some() {
        return Some(Rejection::Busy);
    }
    match view.snapshot.turn(turn).map(|info| info.outcome.is_some()) {
        Some(true) => None,
        _ => Some(Rejection::Invalid("context_control_unfinished_turn")),
    }
}

fn diagnostic_text(diagnostic: &RuntimeDiagnostic) -> String {
    match diagnostic {
        RuntimeDiagnostic::StaleProviderEvent { dropped, .. } => {
            format!("stale provider events dropped: {dropped}")
        }
        RuntimeDiagnostic::StaleTimer { .. } => "stale timer fired".into(),
        RuntimeDiagnostic::MalformedStream { reason, .. } => {
            format!("malformed provider stream ({reason})")
        }
    }
}

fn fault_text(cause: &harness_core::runtime::FaultCause) -> String {
    use harness_core::runtime::{FaultCause, PersistError};
    let code = match cause {
        FaultCause::Persist(error) => match error {
            PersistError::WorkerLost => "storage_worker_lost",
            PersistError::Poisoned => "writer_poisoned",
            PersistError::TornTail(_) => "torn_journal_tail",
            PersistError::Rejected { code } => code.as_str(),
            PersistError::Io { .. } => "storage_io",
        },
        FaultCause::UnexpectedAnswer(_) => "unexpected_commit_answer",
        FaultCause::Internal(code) => code,
    };
    format!("fault: {}", line(code))
}

/// A bounded, single-line view of a tool call, shared by the transcript and
/// the approval dialog.
pub(crate) fn call_summary(
    name: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let arguments = serde_json::Value::Object(arguments.clone()).to_string();
    format!("{} {}", bounded(name, 80), bounded(&arguments, 320))
}
