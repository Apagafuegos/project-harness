//! Pure session and branch navigation. No storage or terminal access.
use super::{Action, App, Modal};
use crate::text::bounded;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harness_core::{runtime::Command, session::*};
use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph},
};

#[derive(Default)]
pub(super) struct Navigation {
    screen: Option<Screen>,
    sessions: Vec<SessionCatalogEntry>,
    index: usize,
    edit_parent: Option<Option<EntryId>>,
    report: Option<Box<harness_core::context::PreviewReport>>,
    waiting: bool,
}
#[derive(Clone, Copy)]
enum Screen {
    Sessions,
    Branches,
    Models,
    SwitchReport,
}

struct Branch {
    user: EntryId,
    parent: Option<EntryId>,
    head: EntryId,
    turn: TurnId,
    depth: usize,
    text: String,
    model: ModelRef,
    current: bool,
    /// Effective mode of this turn on the selected ancestry; `Default` when
    /// no control applies. Toggling follows this value, never one press alone.
    mode: ContextMode,
}

impl App {
    /// Supply a completed read-only catalog scan; late results never reopen a dialog.
    pub fn on_sessions(&mut self, sessions: Vec<SessionCatalogEntry>) {
        self.navigation.sessions = sessions;
        self.navigation.index = 0;
    }

    pub(super) fn navigation_captures_input(&self) -> bool {
        matches!(self.modal, Modal::None) && self.navigation.screen.is_some()
    }

    /// One picker row. The session this client owns is locked by its own
    /// engine, so the catalog cannot read it; its live view supplies the title,
    /// model and state instead (ADR 0021), and opening it is meaningless.
    fn session_row(&self, entry: &SessionCatalogEntry) -> String {
        let live = self
            .view()
            .filter(|view| view.snapshot.session_id() == &entry.session_id);
        let (title, model, state, status) = match live {
            Some(view) => (
                title_of(&view.snapshot),
                view.snapshot
                    .model_at(view.snapshot.head())
                    .map(model_name)
                    .unwrap_or_else(|_| "unknown model".into()),
                format!("{:?}", view.snapshot.state()),
                "current".into(),
            ),
            None => (
                entry.title.clone().unwrap_or_else(|| "(no message)".into()),
                entry
                    .model
                    .as_ref()
                    .map(model_name)
                    .unwrap_or_else(|| "unknown model".into()),
                format!("{:?}", entry.state),
                format!("{:?}", entry.status),
            ),
        };
        format!(
            "{} | {} | {} {}\n  {} | {}",
            bounded(&title, 45),
            model,
            state,
            status,
            entry
                .last_activity
                .clone()
                .map(String::from)
                .unwrap_or_else(|| "unknown activity".into()),
            entry.reason.as_deref().unwrap_or("ok")
        )
    }

    fn is_current_session(&self, id: &SessionId) -> bool {
        self.view()
            .is_some_and(|view| view.snapshot.session_id() == id)
    }

    pub(super) fn navigation_key(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        if !matches!(self.modal, Modal::None) {
            return None;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')) {
            return None;
        }
        if self.navigation.waiting {
            return Some(vec![]);
        }
        // Keys the tree shares with a later dialog are only active while the
        // tree is the open screen, so decision 9's map is unchanged elsewhere.
        let tree = self
            .navigation
            .screen
            .is_some_and(|s| matches!(s, Screen::Branches));
        if control {
            let screen = match key.code {
                KeyCode::Char('o') => Some(Screen::Sessions),
                KeyCode::Char('t') => Some(Screen::Branches),
                KeyCode::Char('l') => Some(Screen::Models),
                _ => None,
            };
            if let Some(screen) = screen {
                self.navigation.screen = Some(screen);
                self.navigation.index = 0;
                self.clear_error();
                return Some(if matches!(screen, Screen::Sessions) {
                    vec![Action::ListSessions]
                } else {
                    vec![]
                });
            }
        }
        let screen = self.navigation.screen?;
        let branches = self.branches();
        let models: Vec<_> = self
            .catalog
            .iter()
            .map(|(model, _)| model.clone())
            .collect();
        let count = match screen {
            Screen::Sessions => self.navigation.sessions.len(),
            Screen::Branches => branches.len(),
            Screen::Models => models.len(),
            Screen::SwitchReport => self.navigation.report.as_ref().map_or(0, |r| {
                r.would_stop_being_portable.len() + r.required_incompatibilities.len()
            }),
        };
        match key.code {
            KeyCode::Esc => self.navigation.screen = None,
            KeyCode::Up => self.navigation.index = self.navigation.index.saturating_sub(1),
            KeyCode::Down => {
                self.navigation.index = (self.navigation.index + 1).min(count.saturating_sub(1))
            }
            KeyCode::Char('n') if matches!(screen, Screen::Sessions) && !control => {
                self.navigation.waiting = true;
                return Some(vec![Action::NewSession]);
            }
            KeyCode::Enter if matches!(screen, Screen::Models) => {
                if let Some(model) = models.get(self.navigation.index) {
                    self.navigation.waiting = true;
                    return Some(vec![Action::Command(Command::SwitchModel {
                        model: model.clone(),
                        policy: Portability::Reject,
                    })]);
                }
            }
            KeyCode::Char('o') if matches!(screen, Screen::SwitchReport) && !control => {
                if let Some(report) = &self.navigation.report {
                    if report.required_incompatibilities.is_empty() {
                        self.navigation.waiting = true;
                        return Some(vec![Action::Command(Command::SwitchModel {
                            model: report.preview_model.clone(),
                            policy: Portability::OmitOptional,
                        })]);
                    }
                    self.set_error("required incompatibilities cannot be omitted");
                }
            }
            KeyCode::Enter if matches!(screen, Screen::Sessions) => {
                if let Some(entry) = self.navigation.sessions.get(self.navigation.index) {
                    if self.is_current_session(&entry.session_id) {
                        self.set_error("already open");
                        return Some(vec![]);
                    }
                    let reason = match entry.status {
                        CatalogStatus::Busy => Some("busy: session_locked"),
                        CatalogStatus::NeedsRepair => {
                            Some("needs repair: run harness journal repair")
                        }
                        CatalogStatus::Unreadable => Some("unreadable session"),
                        CatalogStatus::Ok
                            if entry.workspace.as_deref() != self.info.workspace.to_str() =>
                        {
                            Some("different workspace: open refused")
                        }
                        CatalogStatus::Ok => None,
                    };
                    if let Some(reason) = reason {
                        self.set_error(reason);
                    } else {
                        let id = entry.session_id.clone();
                        self.navigation.waiting = true;
                        return Some(vec![Action::OpenSession(id)]);
                    }
                }
            }
            KeyCode::Enter if matches!(screen, Screen::Branches) => {
                if let Some(branch) = branches.get(self.navigation.index) {
                    // Re-selecting the current head would be `head_unchanged`;
                    // navigation never submits a command the core must refuse.
                    if self.view().and_then(|view| view.snapshot.head()) == Some(&branch.head) {
                        return Some(vec![]);
                    }
                    self.navigation.waiting = true;
                    return Some(vec![Action::Command(Command::SelectHead {
                        head: Some(branch.head.clone()),
                    })]);
                }
            }
            KeyCode::Char('e') if matches!(screen, Screen::Branches) && !control => {
                if let Some(branch) = branches.get(self.navigation.index) {
                    self.prompt = branch.text.clone();
                    self.cursor = self.prompt.chars().count();
                    self.navigation.edit_parent = Some(branch.parent.clone());
                    self.navigation.screen = None;
                }
            }
            // `p` and `x` are whole-turn controls: pin (or reset a pinned turn)
            // and exclude (or reset an excluded one). A control that the app can
            // already rule out sends nothing and shows the engine's own refusal.
            KeyCode::Char(pressed @ ('p' | 'x')) if tree && !control => {
                if let Some(branch) = branches.get(self.navigation.index) {
                    let turn = branch.turn.clone();
                    let refusal = self
                        .view()
                        .and_then(|view| super::control_refusal(view, &turn));
                    if let Some(refusal) = refusal {
                        self.set_error(super::runtime::rejection(refusal));
                        return Some(vec![]);
                    }
                    let mode = match (pressed, branch.mode) {
                        ('p', ContextMode::Pinned) | ('x', ContextMode::Excluded) => {
                            ContextMode::Default
                        }
                        ('p', _) => ContextMode::Pinned,
                        _ => ContextMode::Excluded,
                    };
                    self.navigation.waiting = true;
                    return Some(vec![Action::Command(Command::SetContextControl {
                        turn_id: turn,
                        mode,
                    })]);
                }
            }
            _ => {}
        }
        Some(vec![])
    }

    pub(super) fn navigation_send(&mut self) -> Vec<Action> {
        self.navigation
            .edit_parent
            .take()
            .filter(|head| {
                self.view()
                    .is_none_or(|view| view.snapshot.head() != head.as_ref())
            })
            .map(|head| Action::Command(Command::SelectHead { head }))
            .into_iter()
            .collect()
    }

    pub(super) fn navigation_rejected(&mut self, error: harness_core::runtime::Rejection) {
        self.navigation.waiting = false;
        if let harness_core::runtime::Rejection::PortabilityLoss(report) = error {
            self.navigation.report = Some(report);
            self.navigation.screen = Some(Screen::SwitchReport);
            self.navigation.index = 0;
            self.set_error("portability_loss: switch refused; saved model retained");
        } else {
            self.set_error(super::runtime::rejection(error));
        }
    }

    pub(super) fn navigation_accepted(&mut self, accepted: &harness_core::runtime::Accepted) {
        use harness_core::runtime::Accepted;
        self.navigation.waiting = false;
        match accepted {
            Accepted::HeadSelected { .. } | Accepted::ModelSwitched { .. } => {
                self.navigation.screen = None;
                self.clear_error();
            }
            // A cancellation acknowledgement proves nothing about an earlier
            // refusal: the turn ended, the reason did not. Keep the banner.
            Accepted::Cancelling { .. } => {}
            _ => self.clear_error(),
        }
    }

    pub(super) fn navigation_finished(&mut self) {
        self.navigation.waiting = false;
    }

    pub(super) fn navigation_reset(&mut self) {
        self.navigation = Navigation::default();
        self.prompt.clear();
        self.cursor = 0;
        self.clear_error();
    }

    fn branches(&self) -> Vec<Branch> {
        let Some(view) = self.view() else {
            return vec![];
        };
        let s = &view.snapshot;
        let modes = super::turn_modes(&s.ancestry(s.head()).unwrap_or_default());
        let current_user = s
            .ancestry(s.head())
            .unwrap_or_default()
            .into_iter()
            .rev()
            .find(|e| matches!(e.entry.payload, EntryPayload::UserMessage(_)))
            .map(|e| &e.entry.id);
        let mut rows = Vec::new();
        for (turn_id, turn) in s.turns() {
            let Some(user) = s.entry(&turn.user_entry_id) else {
                continue;
            };
            let EntryPayload::UserMessage(message) = &user.entry.payload else {
                continue;
            };
            let ancestors = s
                .ancestry(user.entry.parent_id.as_ref())
                .unwrap_or_default();
            let depth = ancestors
                .iter()
                .filter(|e| matches!(e.entry.payload, EntryPayload::UserMessage(_)))
                .count();
            let head = s
                .entries()
                .filter(|(_, e)| e.entry.turn_id.as_ref() == Some(turn_id))
                .max_by_key(|(_, e)| (e.sequence, e.entry_index))
                .map(|(id, _)| id.clone())
                .unwrap_or_else(|| user.entry.id.clone());
            rows.push(Branch {
                user: user.entry.id.clone(),
                parent: user.entry.parent_id.clone(),
                head,
                turn: turn_id.clone(),
                depth,
                text: message
                    .content
                    .iter()
                    .filter_map(|b| {
                        if let ContentBlock::Text { text } = b {
                            Some(text.as_str())
                        } else {
                            None
                        }
                    })
                    .collect(),
                model: s
                    .model_at(Some(&user.entry.id))
                    .unwrap_or(&self.info.model)
                    .clone(),
                current: current_user == Some(&user.entry.id),
                mode: modes.get(turn_id).copied().unwrap_or_default(),
            });
        }
        // Lexicographic ancestry paths put every descendant beside its parent,
        // while preserving journal identity order among sibling branches.
        rows.sort_by_key(|row| {
            s.ancestry(Some(&row.user))
                .unwrap_or_default()
                .iter()
                .filter(|e| matches!(e.entry.payload, EntryPayload::UserMessage(_)))
                .map(|e| e.entry.id.clone())
                .collect::<Vec<_>>()
        });
        rows
    }

    pub(super) fn render_navigation(&self, frame: &mut Frame) {
        if !matches!(self.modal, Modal::None) {
            return;
        }
        let Some(screen) = self.navigation.screen else {
            return;
        };
        let (title, rows) = match screen {
            Screen::Sessions => (
                "sessions · Enter open · n new · Esc close",
                self.navigation
                    .sessions
                    .iter()
                    .map(|e| self.session_row(e))
                    .collect::<Vec<_>>(),
            ),
            Screen::Models => (
                "models · Enter switch (Reject first) · Esc close",
                self.catalog
                    .iter()
                    .map(|(model, info)| {
                        format!(
                            "{} | context {:?} output {:?}\n  media {:?} tools={} reasoning={}",
                            model_name(model),
                            info.limits.context_tokens,
                            info.limits.max_output_tokens,
                            info.capabilities.input_media,
                            info.capabilities.tools,
                            info.capabilities.reasoning_effort
                        )
                    })
                    .collect(),
            ),
            Screen::SwitchReport => {
                let Some(report) = &self.navigation.report else {
                    return;
                };
                let mut rows: Vec<String> = report
                    .would_stop_being_portable
                    .iter()
                    .map(|loss| {
                        format!(
                            "optional entry={} part={}\n  owner={} schema={} bytes={}",
                            loss.entry_id,
                            part(&loss.part),
                            model_name(&loss.owner),
                            bounded(&loss.schema, 100),
                            loss.bytes
                        )
                    })
                    .collect();
                rows.extend(report.required_incompatibilities.iter().map(|required| {
                    format!(
                        "REQUIRED entry={} part={}\n  capability={} — cannot omit",
                        required.entry_id,
                        part(&required.part),
                        capability(required.capability)
                    )
                }));
                (
                    if report.required_incompatibilities.is_empty() {
                        "portability report · o confirm omit optional · Esc cancel"
                    } else {
                        "portability report · REQUIRED cannot omit · Esc cancel"
                    },
                    rows,
                )
            }
            Screen::Branches => (
                "branches · Enter select · e edit sibling · p pin · x exclude · Esc close",
                self.branches()
                    .iter()
                    .map(|b| {
                        format!(
                            "{}{}{}{}{}\n  {}",
                            "  ".repeat(b.depth.min(20)),
                            if b.depth == 0 { "●" } else { "└─" },
                            if b.current { "*" } else { " " },
                            mode_marker(b.mode),
                            bounded(&b.text, 44),
                            model_name(&b.model)
                        )
                    })
                    .collect(),
            ),
        };
        draw_navigation(
            frame,
            title,
            rows,
            self.navigation.index,
            self.last_error.as_deref(),
        );
    }
}

/// The marker shown before a tree row's title, padded to one column so titles
/// stay aligned. ASCII brackets are chosen over an emoji: they are one cell per
/// character in every terminal and stay visible at 20×5 (ADR 0023 Part 5).
fn mode_marker(mode: ContextMode) -> &'static str {
    match mode {
        ContextMode::Pinned => "[pin] ",
        ContextMode::Excluded => "[excl] ",
        ContextMode::Default => "      ",
    }
}

fn model_name(m: &ModelRef) -> String {
    format!(
        "{}/{}",
        bounded(&m.provider_instance, 32),
        bounded(&m.model, 48)
    )
}

/// A portability report names the part, never its payload.
fn part(part: &harness_core::context::Part) -> String {
    use harness_core::context::Part;
    match part {
        Part::Entry => "entry".into(),
        Part::Block { index } => format!("block[{index}]"),
        Part::Continuation => "continuation".into(),
    }
}

fn capability(capability: harness_core::context::RequiredCapability) -> &'static str {
    use harness_core::context::RequiredCapability;
    match capability {
        RequiredCapability::Image => "image",
        RequiredCapability::Audio => "audio",
        RequiredCapability::File => "file",
        RequiredCapability::Tools => "tools",
    }
}

/// The catalog's title rule, applied to a live snapshot (ADR 0021): the first
/// user message in journal order, first line, at most 120 scalars.
fn title_of(snapshot: &SessionSnapshot) -> String {
    let first = snapshot
        .entries()
        .filter(|(_, stored)| matches!(stored.entry.payload, EntryPayload::UserMessage(_)))
        .min_by_key(|(_, stored)| (stored.sequence, stored.entry_index));
    let Some((_, stored)) = first else {
        return "(no message)".into();
    };
    let EntryPayload::UserMessage(message) = &stored.entry.payload else {
        unreachable!()
    };
    let text: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let title: String = text
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(120)
        .collect();
    if title.is_empty() {
        "(no text)".into()
    } else {
        title
    }
}

fn draw_navigation(
    frame: &mut Frame,
    title: &str,
    rows: Vec<String>,
    selected: usize,
    error: Option<&str>,
) {
    let a = frame.area();
    let rect = Rect::new(a.x, a.y, a.width, a.height.saturating_sub(2));
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    let mut lines = Vec::new();
    if rows.is_empty() {
        lines.push(Line::raw("(empty)"));
    }
    // Start at the selected row when necessary; wrap metadata rather than
    // clipping owners, schemas or byte counts at the terminal edge.
    let start = if selected < 3 { 0 } else { selected };
    for (i, row) in rows.iter().enumerate().skip(start) {
        for (n, text) in row.lines().enumerate() {
            let prefix = if i == selected && n == 0 { "> " } else { "  " };
            let text = format!("{prefix}{text}");
            let width = usize::from(inner.width.max(1));
            let chars: Vec<char> = text.chars().collect();
            lines.extend(
                chars
                    .chunks(width)
                    .map(|chunk| Line::raw(chunk.iter().collect::<String>())),
            );
        }
    }
    if let Some(error) = error {
        lines.push(Line::raw(format!("error: {error}")));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
