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

mod highlight;
mod inspector;
mod inspector_shell;
mod keys;
mod markdown;
mod navigation;
mod render;
pub mod runtime;
pub mod runtime_nav;
pub mod theme;
mod tools;
mod transcript;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use harness_core::{
    context::{Flags, Totals},
    runtime::{
        Accepted, ApplyOutcome, ClientView, Command, Phase, Rejection, RuntimeDiagnostic,
        RuntimeView, SubscriptionItem, UncertainInvocation, UndoPreview, UndoResolution,
        UserResolution,
    },
    session::{
        ApprovalId, CommandId, ContentBlock, ContextMode, Decision, EntryPayload, FileOutcome,
        InvocationId, ModelRef, RuntimeState, StoredEntry, ToolStatus, TurnId, UndoFileOutcome,
    },
};
use ratatui::{Frame, text::Line};

use self::transcript::Notice;
use theme::Theme;

/// TUI-local input, carrying a monotonic clock supplied by the shell.
#[derive(Clone, Copy, Debug)]
pub enum UiAction {
    /// Monotonic time since shell startup, delivered only during active turns.
    Tick(Duration),
}
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

/// One remembered grant a continued session resumed, as the transcript lists
/// it. Only displayed: the recorded allowances are never changed or dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResumedGrant {
    /// A trusted file tool's grant for one path, or everything below it.
    File {
        /// Tool name, such as `write`.
        tool: String,
        /// The recorded path spelling.
        path: String,
        /// Whether the grant covers everything below `path`.
        prefix: bool,
    },
    /// One exact shell command in one cwd.
    Bash {
        /// The exact recorded command line.
        command: String,
        /// The recorded cwd.
        cwd: String,
    },
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
    /// Delete one other session's journal from the shell. Artifacts are never
    /// deleted: they are shared and content-addressed (decision 16).
    DeleteSession {
        /// Session whose journal is removed.
        id: harness_core::session::SessionId,
    },
    /// Write a Markdown export of one session. The shell refuses an existing
    /// path instead of overwriting it.
    ExportSession {
        /// Session to export, read-only.
        id: harness_core::session::SessionId,
        /// Destination file inside an existing directory.
        path: PathBuf,
    },
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
        /// Show the raw JSON arguments instead of the readable preview.
        raw: bool,
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
    /// The user asked to delete a session; name it and state the consequences.
    ConfirmDelete {
        /// Session that would lose its journal.
        id: harness_core::session::SessionId,
        /// Display title, which the catalog may not know.
        title: String,
    },
    /// The user asked to export a session as Markdown; edit the destination.
    ExportPath {
        /// Session that would be read.
        id: harness_core::session::SessionId,
        /// Editable destination path, prefilled from the session ID.
        input: String,
    },
    /// The read-only undo preview, waiting for a y/N confirmation.
    UndoConfirm {
        /// Exactly what the durable undo would do.
        preview: Box<UndoPreview>,
    },
    /// An interrupted undo waits for the user's statement about every target.
    UndoResolve {
        /// Selected target row.
        index: usize,
        /// Acknowledge unknown, or state what was observed per file.
        mode: UndoResolveMode,
        /// One observed outcome per target, in target order.
        files: Vec<FileOutcome>,
        /// The user's note; required when observing.
        input: String,
    },
    /// A refusal with no choice to make, shown in the same dialog frame.
    Notice {
        /// Dialog title.
        title: String,
        /// Short explanation lines.
        message: String,
    },
}

/// How the user answers an interrupted undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UndoResolveMode {
    /// Accept that which files changed is unknown.
    AcknowledgedUnknown,
    /// State what each file shows now.
    Observed,
}

impl UndoResolveMode {
    fn next(self) -> Self {
        match self {
            Self::AcknowledgedUnknown => Self::Observed,
            Self::Observed => Self::AcknowledgedUnknown,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::AcknowledgedUnknown => "accept that which files changed is unknown",
            Self::Observed => "observed: state each file now",
        }
    }
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
    theme: Theme,
    help: bool,
    modal_scroll: usize,
    modal_page: usize,
    history: Vec<String>,
    process_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    pasted_lines: usize,
    physical_lines: usize,
    new_lines: usize,
    now: Duration,
    turn_started: Option<Duration>,
    timed_turn: Option<TurnId>,
    durations: BTreeMap<TurnId, Duration>,
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
    /// Show full tool output instead of the first three lines.
    output: bool,
    /// Render completed entries once and reuse the rows.
    renderer: transcript::Renderer,
    /// Latest built-context estimate, absent after a reset.
    context: Option<(Totals, Flags)>,
    /// Opt-in: answer read-only approval tickets without a key press (F02).
    read_only_approval: bool,
    /// Tickets already answered as read-only, so one ticket is submitted once.
    read_only_approved: BTreeSet<ApprovalId>,
    /// Commands produced without a key press, taken by the shell and submitted
    /// through the same command path as a key press.
    undecided: Vec<Action>,
    /// Grants this session's journal resumed, listed before any turn.
    resumed: Vec<ResumedGrant>,
}

impl App {
    /// Create an app for a session. The first subscription item must be a
    /// `Reset`; until then the screen shows an empty, connecting state.
    pub fn new(info: SessionInfo) -> Self {
        Self::with_theme(info, Theme::default())
    }

    /// Inject the shell-computed theme; construction never reads environment.
    pub fn with_theme(info: SessionInfo, theme: Theme) -> Self {
        Self {
            theme,
            help: false,
            modal_scroll: 0,
            modal_page: 1,
            history: Vec::new(),
            process_history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            pasted_lines: 0,
            physical_lines: 0,
            new_lines: 0,
            now: Duration::ZERO,
            turn_started: None,
            timed_turn: None,
            durations: BTreeMap::new(),
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
            output: false,
            renderer: transcript::Renderer::default(),
            context: None,
            read_only_approval: false,
            read_only_approved: BTreeSet::new(),
            undecided: Vec::new(),
            resumed: Vec::new(),
        }
    }

    /// Record the grants the opened session's journal resumed (decision 18).
    /// The shell sets this at open and on every durable handover; the app only
    /// lists them and never alters the recorded allowances.
    pub fn set_resumed_grants(&mut self, resumed: Vec<ResumedGrant>) {
        if self.resumed == resumed {
            return;
        }
        self.resumed = resumed;
        self.rebuild();
    }

    /// The grants this app lists as resumed. A view of the shell's decisions.
    pub fn resumed_grants(&self) -> &[ResumedGrant] {
        &self.resumed
    }

    /// Opt in to answering registry-declared read-only tickets automatically.
    /// The shell sets this from the resolved `approve_read_only` value.
    pub fn set_read_only_approval(&mut self, on: bool) {
        self.read_only_approval = on;
    }

    /// Whether this app auto-approves read-only tool calls.
    pub fn read_only_approval(&self) -> bool {
        self.read_only_approval
    }

    /// Decisions this app made without a key press. The shell dispatches them
    /// exactly like a key press: one command, never retried, journaled as an
    /// ordinary allow.
    pub fn take_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.undecided)
    }

    /// Supply the merged, sourced catalog for the current and switched models.
    pub fn set_catalog(&mut self, catalog: harness_core::context::ModelCatalog) {
        self.catalog = catalog;
    }

    /// Insert bracketed paste atomically; modal input never becomes a command.
    pub fn on_paste(&mut self, text: &str) {
        if self.help || self.navigation_captures_input() || self.inspector_captures() {
            return;
        }
        let text: String = text
            .chars()
            .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
            .collect();
        match &mut self.modal {
            Modal::None => {
                self.pasted_lines = text.split('\n').count();
                let at = self.byte_index(self.cursor);
                self.prompt.insert_str(at, &text);
                self.cursor += text.chars().count();
            }
            Modal::Uncertain { input, .. } | Modal::UndoResolve { input, .. } => {
                input.push_str(&text)
            }
            Modal::ExportPath { input, .. } => {
                input.extend(text.chars().filter(|c| !c.is_control() || *c == '\n'));
            }
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
                self.seed_history();
                self.reset_inspector();
                self.notices.clear();
                self.context = None;
                self.scroll = 0;
                self.physical_lines = 0;
                self.new_lines = 0;
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
            }
        }
        if self
            .view
            .as_ref()
            .is_none_or(|client| client.view().active_turn.is_none())
        {
            self.interrupt_armed = false;
        }
        self.seed_history();
        self.approve_read_only();
        self.sync_modal();
        let active = self.active_turn();
        if active != self.timed_turn {
            if let (Some(id), Some(start)) = (&self.timed_turn, self.turn_started) {
                self.durations
                    .insert(id.clone(), self.now.saturating_sub(start));
            }
            self.timed_turn = active;
            self.turn_started = None;
        }
        self.rebuild();
    }

    /// Turn one key press into state changes and shell actions. Never performs
    /// I/O, and never builds a provider request or decides an approval.
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if key.kind == KeyEventKind::Release || !keys::documented(key) {
            return Vec::new();
        }
        if self.help {
            match key.code {
                KeyCode::Esc | KeyCode::F(1) => {
                    self.help = false;
                    self.modal_scroll = 0;
                }
                KeyCode::Up => self.modal_scroll = self.modal_scroll.saturating_sub(1),
                KeyCode::Down => self.modal_scroll = self.modal_scroll.saturating_add(1),
                KeyCode::PageUp => {
                    self.modal_scroll = self.modal_scroll.saturating_sub(self.modal_page)
                }
                KeyCode::PageDown => {
                    self.modal_scroll = self.modal_scroll.saturating_add(self.modal_page)
                }
                _ => {}
            }
            return Vec::new();
        }
        if key.code == KeyCode::F(1)
            || (matches!(self.modal, Modal::None)
                && key.code == KeyCode::Char('?')
                && self.prompt.is_empty()
                && !self.navigation_captures_input()
                && !self.inspector_captures())
        {
            self.help = true;
            self.modal_scroll = 0;
            return Vec::new();
        }
        if !matches!(self.modal, Modal::None) {
            let scroll = match key.code {
                KeyCode::PageUp => Some(false),
                KeyCode::PageDown => Some(true),
                KeyCode::Up if !matches!(self.modal, Modal::Uncertain { .. }) => Some(false),
                KeyCode::Down if !matches!(self.modal, Modal::Uncertain { .. }) => Some(true),
                _ => None,
            };
            if let Some(down) = scroll {
                let n = if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                    self.modal_page
                } else {
                    1
                };
                self.modal_scroll = if down {
                    self.modal_scroll.saturating_add(n)
                } else {
                    self.modal_scroll.saturating_sub(n)
                };
                return Vec::new();
            }
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
            Modal::UndoConfirm { .. } => self.undo_confirm_key(key),
            Modal::UndoResolve { .. } => self.undo_resolve_key(key),
            Modal::ConfirmQuit => self.confirm_quit_key(key),
            Modal::ConfirmDelete { .. } => self.confirm_delete_key(key),
            Modal::ExportPath { .. } => self.export_path_key(key),
            Modal::Notice { .. } => self.notice_key(key),
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
        self.render_session_maintenance(frame);
        self.render_inspector(frame);
        if self.help {
            let mut rows = keys::help(self.theme);
            self.theme.apply(&mut rows);
            let viewport = render::draw_modal(
                frame,
                self.theme,
                "key reference",
                "↑↓ / PgUp / PgDn scroll · Esc / F1 close",
                rows,
                None,
                &mut self.modal_scroll,
            );
            self.modal_page = viewport.height.max(1) as usize;
        }
    }

    fn rebuild(&mut self) {
        self.transcript = match &self.view {
            Some(client) => self.renderer.project(
                client.view(),
                transcript::Toggles {
                    reasoning: self.reasoning,
                    output: self.output,
                },
                &self.notices,
                &self.durations,
                &self.resumed,
            ),
            None => Vec::new(),
        };
        self.theme.apply(&mut self.transcript);
    }

    /// How many transcript entries were rendered from scratch. A view of the
    /// render cache for tests; it is not part of any client protocol.
    pub fn transcript_parses(&self) -> u64 {
        self.renderer.parses()
    }

    /// True when full tool output is expanded (Ctrl-E).
    pub fn full_output(&self) -> bool {
        self.output
    }

    /// F02: answer a pending read-only ticket without a key press.
    ///
    /// Only the shared [`crate::ask::read_only`] classifier qualifies, so shell
    /// commands, mutations, unknown and non-registry tools always open the
    /// modal. The submitted decision is an ordinary, unremembered allow bound
    /// to the ticket's approval id and plan digest; turning the setting on can
    /// never create a grant or change the recorded session policy.
    fn approve_read_only(&mut self) {
        if !self.read_only_approval {
            return;
        }
        let Some(ticket) = self.view().and_then(|view| view.pending_approval.clone()) else {
            return;
        };
        if self.read_only_approved.contains(&ticket.approval_id) {
            return;
        }
        let Some((name, _)) = self
            .view()
            .and_then(|view| render::call_proposal(view, &ticket.call.call_id))
        else {
            return;
        };
        if !crate::ask::read_only(&name) {
            return;
        }
        self.read_only_approved.insert(ticket.approval_id.clone());
        self.notices.push(Notice::Info(format!(
            "approval: {name} auto-approved as read-only (no remembered grant)"
        )));
        self.undecided
            .push(Action::Command(Command::SubmitApproval {
                approval_id: ticket.approval_id,
                plan_digest: ticket.plan_digest,
                decision: Decision::Allow,
                remember: false,
            }));
    }

    /// Keep the modal in step with the view. Approval takes precedence over
    /// resolution, and a confirm-quit dialog only survives while it is still
    /// the only thing asking for input.
    fn sync_modal(&mut self) {
        let Some(view) = self.view.as_ref().map(ClientView::view) else {
            return;
        };
        if view.phase == Phase::AwaitingResolution
            && let Some(undo) = view.undo.clone()
        {
            // An interrupted undo is resolved as a whole set, never by retrying.
            let targets = view
                .snapshot
                .unresolved_undo()
                .filter(|pending| pending.intent.undo_id == undo.undo_id)
                .map(|pending| pending.intent.targets.len())
                .unwrap_or(0);
            match &mut self.modal {
                Modal::UndoResolve {
                    index, files, mode, ..
                } if files.len() == targets => {
                    *index = (*index).min(targets.saturating_sub(1));
                    let _ = mode;
                }
                _ => {
                    self.modal = Modal::UndoResolve {
                        index: 0,
                        mode: UndoResolveMode::AcknowledgedUnknown,
                        files: vec![FileOutcome::Unknown; targets],
                        input: String::new(),
                    }
                }
            }
            return;
        }
        if let Some(ticket) = &view.pending_approval {
            if self.read_only_approved.contains(&ticket.approval_id) {
                // Answered without a key press: the modal never opens for it.
                self.modal = Modal::None;
                return;
            }
            match &self.modal {
                Modal::Approval { approval_id, .. } if *approval_id == ticket.approval_id => {}
                _ => {
                    self.modal = Modal::Approval {
                        approval_id: ticket.approval_id.clone(),
                        raw: false,
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
        } else if !self.modal_survives_sync() {
            self.modal = Modal::None;
        }
    }

    /// A confirmation or path dialog is the user's own question and outlives an
    /// engine reset; an approval or resolution dialog belongs to the view.
    fn modal_survives_sync(&self) -> bool {
        matches!(
            self.modal,
            Modal::ConfirmQuit
                | Modal::ConfirmDelete { .. }
                | Modal::ExportPath { .. }
                | Modal::Notice { .. }
                | Modal::UndoConfirm { .. }
        )
    }

    /// An informational dialog only closes; it never sends anything.
    fn notice_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('n')) {
            self.modal = Modal::None;
        }
        Vec::new()
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

    /// Reduce a command reply the shell could not attribute to an inspection or
    /// a navigation. Returns false when it is an ordinary reply.
    pub fn undo_reply(&mut self, reply: &Result<Accepted, Rejection>) -> bool {
        match reply {
            // Any candidate reaches the confirmation, including a newest turn
            // whose every mutation is `not_applied`: it has no targets but can
            // still be consumed so earlier turns are reachable. A turn that is
            // not a candidate is refused with `undo_nothing`, never previewed.
            Ok(Accepted::UndoPreviewed { preview }) => {
                self.modal = Modal::UndoConfirm {
                    preview: preview.clone(),
                };
                true
            }
            Ok(Accepted::UndoCompleted { outcome, files, .. }) => {
                self.modal = Modal::None;
                self.notices.push(Notice::Info(format!(
                    "undo {}: {}",
                    outcome.as_str(),
                    files
                        .iter()
                        .map(|row| format!("{} {}", row.outcome.as_str(), row.path))
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
                true
            }
            Ok(Accepted::UndoResolved { .. }) => {
                self.modal = Modal::None;
                self.notices
                    .push(Notice::Info("interrupted undo resolved".into()));
                true
            }
            Err(rejection @ Rejection::Undo { .. }) => {
                self.set_error(crate::tui::runtime::rejection(rejection.clone()));
                self.modal = Modal::None;
                true
            }
            // A refused resolution keeps its dialog and the note draft: the
            // session is still awaiting resolution, and the user corrects the
            // answer instead of starting over.
            Err(rejection @ Rejection::Invalid(_))
                if matches!(self.modal, Modal::UndoResolve { .. }) =>
            {
                self.set_error(crate::tui::runtime::rejection(rejection.clone()));
                true
            }
            _ => false,
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
            KeyCode::Char('e') if control => {
                self.output = !self.output;
                self.rebuild();
                Vec::new()
            }
            // Never an OS job control: the raw-mode loop turns Ctrl-Z into a
            // read-only undo preview, and only y records anything.
            KeyCode::Char('z') if control => {
                if !self.is_idle() {
                    return Vec::new();
                }
                self.clear_error();
                vec![Action::Command(Command::PreviewUndo)]
            }
            KeyCode::Esc => match self.active_turn() {
                Some(turn_id) => vec![Action::Command(Command::Cancel { turn_id })],
                None => {
                    self.clear_error();
                    Vec::new()
                }
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
                if self.row_start() == 0 {
                    self.recall(false);
                } else {
                    self.move_vertical(false);
                }
                Vec::new()
            }
            KeyCode::Down => {
                if self.row_end() == self.prompt.chars().count() {
                    self.recall(true);
                } else {
                    self.move_vertical(true);
                }
                Vec::new()
            }
            KeyCode::Left if control => {
                self.cursor = self.previous_word();
                Vec::new()
            }
            KeyCode::Right if control => {
                let chars: Vec<_> = self.prompt.chars().collect();
                while self.cursor < chars.len() && !chars[self.cursor].is_whitespace() {
                    self.cursor += 1;
                }
                while self.cursor < chars.len() && chars[self.cursor].is_whitespace() {
                    self.cursor += 1;
                }
                Vec::new()
            }
            KeyCode::Char('w') if control => {
                self.erase_to(self.previous_word());
                Vec::new()
            }
            KeyCode::Char('u') if control => {
                self.erase_to(self.row_start());
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
            KeyCode::End if control || self.prompt.is_empty() => {
                self.follow();
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
        self.follow();
        self.clear_error();
        self.pasted_lines = 0;
        self.history_index = None;
        self.process_history.push(text.clone());
        self.history.push(text.clone());
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
        if key.code == KeyCode::Char('v') && !control && !alt {
            // The raw-arguments switch never answers the ticket.
            if let Modal::Approval { raw, .. } = &mut self.modal {
                *raw = !*raw;
                self.modal_scroll = 0;
            }
            return Vec::new();
        }
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
                            // A terminal cannot classify a captured rename; the
                            // dialog refuses that case rather than guessing.
                            file_effect: None,
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

    /// The preview is the user's decision point. Enter is the default answer,
    /// and the default answer is no: only `y` records an undo.
    fn undo_confirm_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control {
            return Vec::new();
        }
        let Modal::UndoConfirm { preview } = &self.modal else {
            return Vec::new();
        };
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let command = Command::Undo {
                    command_id: CommandId::new(),
                    expected_sequence: preview.sequence,
                    expected_head: preview.head.clone(),
                    expected_configuration: preview.configuration.clone(),
                    target_turn_id: preview.target_turn_id.clone(),
                };
                self.modal = Modal::None;
                vec![Action::Command(command)]
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {
                // Cancellation records nothing and keeps the prompt draft.
                self.modal = Modal::None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn undo_resolve_key(&mut self, key: KeyEvent) -> Vec<Action> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let Some(view) = self.view().cloned() else {
            return Vec::new();
        };
        let Some(undo) = view.undo.clone() else {
            return Vec::new();
        };
        let paths: Vec<String> = view
            .snapshot
            .unresolved_undo()
            .filter(|pending| pending.intent.undo_id == undo.undo_id)
            .map(|pending| {
                pending
                    .intent
                    .targets
                    .iter()
                    .map(|target| target.path.clone())
                    .collect()
            })
            .unwrap_or_default();
        let Modal::UndoResolve {
            index,
            mode,
            files,
            input,
        } = &mut self.modal
        else {
            return Vec::new();
        };
        if paths.len() != files.len() || paths.is_empty() {
            return Vec::new();
        }
        *index = (*index).min(paths.len() - 1);
        match key.code {
            KeyCode::Up => *index = index.saturating_sub(1),
            KeyCode::Down => *index = (*index + 1).min(paths.len() - 1),
            KeyCode::Left | KeyCode::BackTab | KeyCode::Right | KeyCode::Tab => {
                *mode = mode.next();
            }
            KeyCode::Char('r') if !control && !alt => {
                *mode = UndoResolveMode::Observed;
                files[*index] = FileOutcome::ObservedRestored;
            }
            KeyCode::Char('x') if !control && !alt => {
                *mode = UndoResolveMode::Observed;
                files[*index] = FileOutcome::ObservedRemoved;
            }
            KeyCode::Char('o') if !control && !alt => {
                *mode = UndoResolveMode::Observed;
                files[*index] = FileOutcome::ObservedOther;
            }
            KeyCode::Char('u') if !control && !alt => {
                *mode = UndoResolveMode::Observed;
                files[*index] = FileOutcome::Unknown;
            }
            KeyCode::Char('a') if !control && !alt => *mode = UndoResolveMode::AcknowledgedUnknown,
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c) if !control && !alt => {
                if !c.is_control() {
                    input.push(c);
                }
            }
            KeyCode::Enter => {
                if input.is_empty() {
                    return Vec::new();
                }
                let resolution = match *mode {
                    UndoResolveMode::AcknowledgedUnknown => UndoResolution::AcknowledgedUnknown {
                        note: input.clone(),
                    },
                    UndoResolveMode::Observed => UndoResolution::Observed {
                        files: paths
                            .iter()
                            .zip(files.iter())
                            .map(|(path, outcome)| UndoFileOutcome {
                                path: path.clone(),
                                outcome: *outcome,
                            })
                            .collect(),
                        note: input.clone(),
                    },
                };
                let undo_id = undo.undo_id.clone();
                return vec![Action::Command(Command::ResolveUndo {
                    undo_id,
                    resolution,
                })];
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

    /// Reduce an injected tick. No clock, timer, key or engine action is created.
    pub fn on_action(&mut self, action: UiAction) {
        let UiAction::Tick(now) = action;
        self.now = now;
        if self.active_turn().is_some() && self.turn_started.is_none() {
            self.turn_started = Some(now);
        }
    }

    fn follow(&mut self) {
        self.scroll = 0;
        self.new_lines = 0;
    }

    fn seed_history(&mut self) {
        let previous = self.history.clone();
        self.history = self
            .view()
            .and_then(|v| v.snapshot.ancestry(v.snapshot.head()).ok())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| {
                if let EntryPayload::UserMessage(m) = &e.entry.payload {
                    Some(
                        m.content
                            .iter()
                            .filter_map(|b| {
                                if let ContentBlock::Text { text } = b {
                                    Some(text.as_str())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                } else {
                    None
                }
            })
            .filter(|s| !s.is_empty())
            .collect();
        // Keep uncommitted/rejected prompts sent in this process without adding
        // another copy of those already present in the authoritative ancestry.
        for text in &self.process_history {
            if !self.history.contains(text) {
                self.history.push(text.clone());
            }
        }
        if previous != self.history {
            self.history_index = None;
        }
    }
    fn recall(&mut self, down: bool) {
        if self.history.is_empty() {
            return;
        }
        let index = match (self.history_index, down) {
            (None, true) => return,
            (None, false) => {
                self.history_draft = self.prompt.clone();
                self.history.len() - 1
            }
            (Some(i), false) => i.saturating_sub(1),
            (Some(i), true) if i + 1 >= self.history.len() => {
                self.prompt = self.history_draft.clone();
                self.cursor = self.prompt.chars().count();
                self.history_index = None;
                return;
            }
            (Some(i), true) => i + 1,
        };
        self.history_index = Some(index);
        self.prompt = self.history[index].clone();
        self.cursor = self.prompt.chars().count();
    }
    fn previous_word(&self) -> usize {
        let chars: Vec<_> = self.prompt.chars().collect();
        let mut at = self.cursor;
        while at > 0 && chars[at - 1].is_whitespace() {
            at -= 1;
        }
        while at > 0 && !chars[at - 1].is_whitespace() {
            at -= 1;
        }
        at
    }
    fn erase_to(&mut self, start: usize) {
        let range = self.byte_index(start)..self.byte_index(self.cursor);
        self.prompt.replace_range(range, "");
        self.cursor = start;
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
