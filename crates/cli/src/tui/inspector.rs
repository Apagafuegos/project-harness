//! Pure context inspection and preview-first compaction UI. No artifacts are read.
use super::{Action, App, Modal, transcript::Notice};
use crate::text::{bounded, line};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use harness_core::{
    context::{
        BlockKind, Budget, ContextReport, ExclusionReason,
        compaction::{CompactionError, CompactionPreview},
    },
    runtime::{Accepted, EventKind, InspectError, Rejection},
    session::{ContentBlock, EntryId, EntryPayload, ModelRef, SessionId},
};
use ratatui::{
    Frame,
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph},
};

type Identity = (SessionId, u64, Option<EntryId>, ModelRef);

#[derive(Default)]
pub(super) struct Inspector {
    open: bool,
    report: Option<ContextReport>,
    requested: Option<Identity>,
    model_override: Option<ModelRef>,
    models: Option<usize>,
    scroll: usize,
    page: usize,
    generating: bool,
    preview: Option<(EntryId, Box<CompactionPreview>)>,
    shown: bool,
    committing: bool,
    cancelling: bool,
    message: Option<String>,
    automatic: Option<EntryId>,
}

impl App {
    pub(super) fn inspector_captures(&self) -> bool {
        self.inspector.open && matches!(self.modal, Modal::None)
    }

    fn context_identity(&self) -> Option<Identity> {
        let view = self.view()?;
        Some((
            view.snapshot.session_id().clone(),
            view.snapshot.sequence(),
            view.snapshot.head().cloned(),
            self.inspector
                .model_override
                .clone()
                .or_else(|| view.snapshot.model_at(view.snapshot.head()).ok().cloned())?,
        ))
    }

    /// The shell checks this after reducing input/events. Only changed inputs
    /// schedule work; streaming deltas cannot create an inspection flood.
    pub(super) fn inspector_refresh(&mut self) -> Vec<Action> {
        if !self.inspector.open {
            return Vec::new();
        }
        let identity = self.context_identity();
        if identity.is_none() || self.inspector.requested == identity {
            return Vec::new();
        }
        self.inspector.requested = identity;
        self.inspector.report = None;
        vec![Action::InspectContext {
            head: None,
            model_override: self.inspector.model_override.clone(),
        }]
    }

    /// Deliver an asynchronous read-only report. Replies from older heads,
    /// sessions or model overrides are discarded, never painted over a new view.
    pub fn on_context_report(&mut self, result: Result<ContextReport, InspectError>) {
        if !self.inspector.open {
            return;
        }
        match result {
            Ok(report) => {
                let actual = Some((
                    report.identity.session_id.clone(),
                    report.identity.journal_sequence,
                    report.identity.head.clone(),
                    report.identity.model.clone(),
                ));
                if actual == self.context_identity()
                    && report.flags.preview == self.inspector.model_override.is_some()
                {
                    self.inspector.report = Some(report);
                }
            }
            Err(_) => self.inspector.message = Some("context inspection unavailable".into()),
        }
    }

    /// Deliver an explicitly inspected live preview; it never grants implicit
    /// commitment. A cancelled/closed request cannot resurrect a proposal.
    pub fn on_compaction_preview(
        &mut self,
        id: EntryId,
        result: Result<CompactionPreview, Rejection>,
    ) {
        match result {
            Ok(preview) if self.inspector.open && self.inspector.generating => {
                self.inspector.generating = false;
                self.inspector.preview = Some((id, Box::new(preview)));
                self.inspector.shown = false;
                self.inspector.scroll = 0;
            }
            Err(error) => {
                self.inspector_rejection(&error);
            }
            _ => {}
        }
    }

    pub(super) fn inspector_reply(&mut self, reply: &Result<Accepted, Rejection>) {
        match reply {
            Ok(Accepted::CompactionPreviewed {
                preview_id,
                preview,
            }) => {
                self.on_compaction_preview(preview_id.clone(), Ok((**preview).clone()));
            }
            Ok(Accepted::CompactionCommitted { .. }) => {
                self.inspector.preview = None;
                self.inspector.committing = false;
                self.inspector.message = Some("Compaction committed".into());
            }
            Ok(Accepted::CompactionCancelled) => self.inspector.cancelling = false,
            _ => {}
        }
    }

    pub(super) fn inspector_rejection(&mut self, error: &Rejection) -> bool {
        let message = match error {
            // Cancelling generation is a normal outcome, never an error banner.
            Rejection::Compaction(CompactionError::Cancelled) => {
                self.inspector.generating = false;
                self.inspector.committing = false;
                self.inspector.preview = None;
                self.inspector.shown = false;
                self.inspector.cancelling = false;
                self.inspector.message = Some("Compaction cancelled; nothing committed".into());
                return true;
            }
            Rejection::StalePreview { field } => {
                self.inspector.preview = None;
                format!("stale_preview: {field} changed; press c for a new preview")
            }
            Rejection::Compaction(error) => format!("compaction: {}", error.code()),
            Rejection::ContextBudgetExceeded { reason } => match reason {
                Some(reason) => format!("context_budget_exceeded: {reason}"),
                None => "context_budget_exceeded: open Ctrl-K, then c to preview compaction".into(),
            },
            _ => {
                self.inspector.generating = false;
                self.inspector.committing = false;
                return false;
            }
        };
        self.inspector.generating = false;
        self.inspector.committing = false;
        self.inspector.message = Some(message.clone());
        self.set_error(message);
        true
    }

    pub(super) fn observe_inspector(&mut self, event: &EventKind) {
        match event {
            EventKind::CompactionPreviewed {
                preview_id,
                automatic: true,
                ..
            } => {
                self.inspector.automatic = Some(preview_id.clone());
                self.inspector.message = Some("Automatic compaction previewed".into());
            }
            // The manual command reply supplies the preview. Automatic proposals
            // can commit before a user sees them, so never offer Enter for those.
            EventKind::CompactionCommitted { preview_id, .. } => {
                let message = if self.inspector.automatic.as_ref() == Some(preview_id) {
                    "automatic compaction committed"
                } else {
                    "compaction committed"
                };
                self.inspector.automatic = None;
                self.inspector.message = Some(message.into());
                self.notices.push(Notice::Info(message.into()));
            }
            EventKind::CompactionFailed { code: "cancelled" } => {}
            EventKind::CompactionFailed {
                code: "context_budget_exceeded",
            } => {
                if self
                    .last_error
                    .as_ref()
                    .is_none_or(|text| !text.starts_with("context_budget_exceeded:"))
                {
                    self.set_error("context_budget_exceeded: Ctrl-K to inspect budget and compaction eligibility");
                }
                self.inspector.message = self.last_error.clone();
            }
            EventKind::CompactionFailed { code } => {
                self.inspector.generating = false;
                self.inspector.message = Some(format!("compaction failed: {code}"));
                self.set_error(format!("compaction failed: {code}"));
            }
            _ => {}
        }
    }

    pub(super) fn inspector_key(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        if !matches!(self.modal, Modal::None) {
            return None;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Char('k') && control {
            if self.inspector.open {
                return Some(self.close_inspector());
            }
            self.inspector.open = true;
            self.inspector.requested = None;
            self.inspector.scroll = 0;
            return Some(Vec::new());
        }
        if !self.inspector.open {
            return None;
        }
        // Global cancellation/quit still belongs to the existing editor.
        if control && matches!(key.code, KeyCode::Char('c' | 'd')) {
            return None;
        }
        if control || key.modifiers.contains(KeyModifiers::ALT) {
            return Some(Vec::new());
        }
        if let Some(index) = self.inspector.models {
            let count = self.catalog.iter().count();
            match key.code {
                KeyCode::Up => self.inspector.models = Some(index.saturating_sub(1)),
                KeyCode::Down => self.inspector.models = Some((index + 1).min(count)),
                KeyCode::Enter => {
                    self.inspector.model_override = if index == 0 {
                        None
                    } else {
                        self.catalog
                            .iter()
                            .nth(index - 1)
                            .map(|(model, _)| model.clone())
                    };
                    self.inspector.models = None;
                    self.inspector.requested = None;
                    self.inspector.scroll = 0;
                }
                KeyCode::Esc => self.inspector.models = None,
                _ => {}
            }
            return Some(Vec::new());
        }
        let actions = match key.code {
            KeyCode::Char('m')
                if !self.inspector.generating && self.inspector.preview.is_none() =>
            {
                self.inspector.models = Some(0);
                self.inspector.scroll = 0;
                Vec::new()
            }
            KeyCode::Esc if self.inspector.generating || self.inspector.preview.is_some() => {
                self.discard_compaction()
            }
            KeyCode::Esc => self.close_inspector(),
            KeyCode::PageUp => {
                self.inspector.scroll = self.inspector.scroll.saturating_sub(self.inspector.page);
                Vec::new()
            }
            KeyCode::PageDown => {
                self.inspector.scroll = self.inspector.scroll.saturating_add(self.inspector.page);
                Vec::new()
            }
            KeyCode::Char('c')
                if !self.inspector.generating
                    && !self.inspector.committing
                    && !self.inspector.cancelling =>
            {
                self.inspector.generating = true;
                self.inspector.preview = None;
                self.inspector.shown = false;
                self.inspector.message = None;
                self.inspector.scroll = 0;
                vec![Action::Command(
                    harness_core::runtime::Command::PreviewCompaction,
                )]
            }
            KeyCode::Enter if self.inspector.shown && !self.inspector.committing => {
                if let Some((id, _)) = &self.inspector.preview {
                    self.inspector.committing = true;
                    vec![Action::Command(
                        harness_core::runtime::Command::CommitCompaction {
                            preview_id: id.clone(),
                        },
                    )]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        };
        Some(actions)
    }

    fn discard_compaction(&mut self) -> Vec<Action> {
        if self.inspector.committing {
            return Vec::new();
        }
        self.inspector.cancelling = true;
        self.inspector.generating = false;
        self.inspector.preview = None;
        self.inspector.shown = false;
        self.inspector.message = Some("Compaction cancelled; nothing committed".into());
        self.inspector.scroll = 0;
        vec![Action::Command(
            harness_core::runtime::Command::CancelCompaction,
        )]
    }

    fn close_inspector(&mut self) -> Vec<Action> {
        let actions = if self.inspector.generating || self.inspector.preview.is_some() {
            self.discard_compaction()
        } else {
            Vec::new()
        };
        self.inspector.open = false;
        actions
    }

    pub(super) fn render_inspector(&mut self, frame: &mut Frame) {
        if !self.inspector_captures() {
            return;
        }
        let area = frame.area();
        let mut rows = vec![Line::raw(
            "Ctrl-K close | m model preview | c compact | PgUp/PgDn | Esc cancel",
        )];
        if let Some(message) = &self.inspector.message {
            rows.push(Line::raw(bounded(message, 240)));
        }
        if self.inspector.generating {
            rows.push(Line::raw("Generating compaction preview… Esc cancels"));
        }
        if let Some(index) = self.inspector.models {
            rows.push(Line::raw(
                "Preview model — nothing switches (Up/Down, Enter, Esc)",
            ));
            rows.push(Line::raw(format!(
                "{} Session model (clear override)",
                if index == 0 { ">" } else { " " }
            )));
            rows.extend(self.catalog.iter().enumerate().map(|(i, (model, _))| {
                Line::raw(format!(
                    "{} {}",
                    if i + 1 == index { ">" } else { " " },
                    model_name(model)
                ))
            }));
        } else if let Some((_, preview)) = &self.inspector.preview {
            rows.extend(preview_rows(preview));
            rows.push(Line::raw(if self.inspector.committing {
                "Committing…"
            } else {
                "Enter commits this shown preview | Esc discards"
            }));
        } else if let Some(report) = &self.inspector.report {
            rows.extend(self.report_rows(report));
        } else {
            rows.push(Line::raw("Inspecting context…"));
        }
        let block = Block::default()
            .title(" Context inspector (read-only) ")
            .title_bottom(if self.inspector.preview.is_some() {
                " Enter commit | Esc discard | PgUp/PgDn "
            } else {
                " PgUp/PgDn scroll "
            })
            .borders(Borders::ALL);
        let inner = block.inner(area);
        let physical = super::render::wrap_lines(rows, inner.width as usize);
        self.inspector.page = (inner.height as usize).max(1);
        if let Some(index) = self.inspector.models {
            self.inspector.scroll = index.saturating_sub(self.inspector.page.saturating_sub(4));
        }
        self.inspector.scroll = self
            .inspector
            .scroll
            .min(physical.len().saturating_sub(inner.height as usize));
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(
                physical
                    .into_iter()
                    .skip(self.inspector.scroll)
                    .take(inner.height as usize)
                    .collect::<Vec<_>>(),
            ),
            inner,
        );
        // Arrival alone is not enough: a frame must actually expose the proposal
        // before Enter can authorize commitment.
        if inner.height >= 4 && inner.width >= 20 && self.inspector.preview.is_some() {
            self.inspector.shown = true;
        }
    }

    fn report_rows(&self, report: &ContextReport) -> Vec<Line<'static>> {
        let mut rows = vec![Line::raw(format!(
            "Model: {}",
            model_name(&report.identity.model)
        ))];
        if report.flags.preview {
            rows.push(Line::raw("MODEL OVERRIDE PREVIEW — nothing switched"));
        }
        match &report.budget {
            Budget::Known {
                context_tokens,
                deductions,
                estimated_tokens,
                headroom_tokens,
                usable_tokens,
                ..
            } => {
                rows.push(Line::raw(format!(
                    "Limit {context_tokens} | output reserve {} | usable {usable_tokens}",
                    deductions.output_reserve
                )));
                rows.push(Line::raw(format!(
                    "Used estimate {estimated_tokens} | remaining estimate {headroom_tokens}"
                )));
                rows.push(Line::raw(format!(
                    "Tool headroom {} | safety margin {}",
                    deductions.tool_headroom, deductions.safety_margin
                )));
            }
            Budget::Unknown {
                reason,
                deductions,
                estimated_tokens,
                ..
            } => {
                rows.push(Line::raw(format!(
                    "Limit unknown ({reason:?}) | output reserve {}",
                    deductions.output_reserve
                )));
                rows.push(Line::raw(format!(
                    "Used estimate {estimated_tokens} | remaining estimate unknown"
                )));
            }
        }
        rows.push(Line::raw(format!(
            "Flags: overflow={} compaction_recommended={} budget_unknown={}",
            report.flags.overflow, report.flags.compaction_recommended, report.flags.budget_unknown
        )));
        rows.push(Line::raw(format!(
            "Estimates: {:?} / {} (opaque bytes {}, NOT tokens)",
            report.totals.estimator.method,
            report.totals.estimator.version,
            report.totals.opaque_provider_state_bytes
        )));
        rows.push(Line::raw("Included items (projection order):"));
        for item in &report.included {
            let id = item
                .entry_id
                .as_ref()
                .map(short)
                .unwrap_or_else(|| "config".into());
            let text = item
                .entry_id
                .as_ref()
                .and_then(|id| self.view()?.snapshot.entry(id))
                .map(|stored| entry_preview(&stored.entry.payload))
                .unwrap_or_else(|| "recorded instructions".into());
            rows.push(Line::raw(format!(
                " {:?} {id} ~{} tokens: {text}",
                item.kind, item.estimated_tokens
            )));
            for block in &item.blocks {
                if block.kind == BlockKind::ToolCall {
                    rows.push(Line::raw(format!(
                        "   tool call {:?} ~{} tokens",
                        block.part, block.estimated_tokens
                    )));
                }
            }
        }
        rows.push(Line::raw("Excluded entries / parts:"));
        for item in &report.excluded {
            rows.push(Line::raw(format!(
                " {} {:?}: {}",
                short(&item.entry_id),
                item.part,
                code(&item.reason)
            )));
        }
        rows.push(Line::raw(format!("Fingerprint: {}", report.fingerprint)));
        rows.push(Line::raw(format!(
            "Configuration: {}",
            report.identity.configuration
        )));
        rows.push(Line::raw(format!(
            "Builder v{} | projection v{} | sequence {}",
            report.identity.builder_version,
            report.identity.projection_version,
            report.identity.journal_sequence
        )));
        rows
    }
}

fn model_name(model: &ModelRef) -> String {
    bounded(&format!("{}/{}", model.provider_instance, model.model), 120)
}
fn short(id: &EntryId) -> String {
    id.to_string()
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}
fn code(value: &ExclusionReason) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

// Explicit content allowlist. Never format a whole entry, block, portability
// report, continuation or ProviderState (not even its opaque schema).
fn block_preview(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text { text } => Some(bounded(text, 100)),
        ContentBlock::ToolCall { name, .. } => Some(format!("tool call {}", bounded(name, 60))),
        ContentBlock::Image { .. } => Some("[image]".into()),
        ContentBlock::Audio { .. } => Some("[audio]".into()),
        ContentBlock::File { .. } => Some("[file]".into()),
        _ => None,
    }
}
fn entry_preview(payload: &EntryPayload) -> String {
    let blocks = match payload {
        EntryPayload::UserMessage(message) => &message.content,
        EntryPayload::AssistantResponse(response) => &response.content,
        EntryPayload::ToolResult(result) => &result.result.content,
        EntryPayload::Compaction(summary) => return bounded(&summary.summary, 100),
        _ => return String::new(),
    };
    blocks
        .iter()
        .find_map(block_preview)
        .unwrap_or_else(|| "[no displayable content]".into())
}

fn preview_rows(preview: &CompactionPreview) -> Vec<Line<'static>> {
    let mut rows = vec![
        Line::raw("COMPACTION PREVIEW — not committed"),
        Line::raw("Proposed summary:"),
    ];
    if let EntryPayload::Compaction(summary) = &preview.entry.payload {
        rows.extend(summary.summary.lines().map(|text| Line::raw(line(text))));
        rows.push(Line::raw("Source manifest — covered entries:"));
        rows.extend(
            summary
                .covered_entry_ids
                .iter()
                .map(|id| Line::raw(format!(" covered {id}"))),
        );
        rows.push(Line::raw("Source manifest — superseded summaries:"));
        rows.extend(
            summary
                .supersedes
                .iter()
                .map(|id| Line::raw(format!(" supersedes {id}"))),
        );
        rows.push(Line::raw(format!(
            "Source fingerprint: {}",
            summary.source_fingerprint
        )));
        rows.push(Line::raw(format!(
            "Manifest configuration: {}",
            summary.configuration_fingerprint
        )));
        // Display provenance fields from the exact manifest, not reconstructed
        // journal references. Only the fixed manifest keys can be rendered.
        if let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&preview.manifest_bytes) {
            for field in ["covered", "supersedes"] {
                if let Some(entries) = manifest[field].as_array() {
                    for entry in entries {
                        rows.push(Line::raw(format!(
                            " {field} checksum {} index {}",
                            entry["record_checksum"].as_str().unwrap_or("?"),
                            entry["entry_index"]
                        )));
                    }
                }
            }
        }
    }
    let diff = &preview.diff;
    rows.push(Line::raw(format!(
        "CompactionDiff: newly covered {} | superseded {} | fits budget {}",
        diff.newly_covered.len(),
        diff.superseded.len(),
        diff.fits_budget
    )));
    rows.extend(
        diff.newly_covered
            .iter()
            .map(|id| Line::raw(format!(" newly covered {id}"))),
    );
    rows.extend(
        diff.superseded
            .iter()
            .map(|id| Line::raw(format!(" newly superseded {id}"))),
    );
    rows.push(Line::raw("Token estimates by category (before -> after):"));
    let before = &diff.before;
    let after = &diff.after;
    for (label, a, b) in [
        (
            "instructions",
            before.estimated_instructions,
            after.estimated_instructions,
        ),
        ("user", before.estimated_user, after.estimated_user),
        (
            "assistant",
            before.estimated_assistant_text,
            after.estimated_assistant_text,
        ),
        (
            "tool calls",
            before.estimated_tool_calls,
            after.estimated_tool_calls,
        ),
        (
            "tool results",
            before.estimated_tool_results,
            after.estimated_tool_results,
        ),
        (
            "summaries",
            before.estimated_summaries,
            after.estimated_summaries,
        ),
        ("media", before.estimated_media, after.estimated_media),
        ("TOTAL", before.estimated_total, after.estimated_total),
    ] {
        rows.push(Line::raw(format!(" {label}: {a} -> {b}")));
    }
    rows
}
