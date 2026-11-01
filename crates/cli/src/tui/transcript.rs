//! Pure projection from a runtime view to transcript rows.
//!
//! Rows are width-independent Ratatui [`Line`]s. The renderer wraps them to the
//! pane, so the projection can be tested without a terminal and a `Reset`
//! always rebuilds exactly the same rows from the same view.

use std::collections::{BTreeMap, BTreeSet};

use harness_core::{
    runtime::{PartialBlock, RuntimeView},
    session::{
        ApprovalId, CallId, ContentBlock, ContextMode, Decision, EntryId, EntryPayload,
        JournalEvent, ModelRef, Portability, ToolStatus, TurnId, TurnOutcome,
    },
};
use pulldown_cmark::{CodeBlockKind, Event as MarkdownEvent, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::text::{bounded, line};

/// An observed non-durable fact that belongs in the log but not in a snapshot.
#[derive(Clone, Debug)]
pub(crate) enum Notice {
    /// A non-fatal runtime observation.
    Diagnostic(String),
    /// The runtime stopped.
    Fault(String),
    /// A compaction attempt failed with its stable code.
    CompactionFailed(String),
    /// Recovery closed a turn.
    Recovered(String),
    /// Informational status.
    Info(String),
}

/// Project the durable branch and the live partial response into rows.
pub(crate) fn project(
    view: &RuntimeView,
    reasoning: bool,
    notices: &[Notice],
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let ancestry = match view.snapshot.ancestry(view.snapshot.head()) {
        Ok(ancestry) => ancestry,
        Err(_) => return rows,
    };
    let branch: BTreeSet<EntryId> = ancestry
        .iter()
        .map(|stored| stored.entry.id.clone())
        .collect();
    let (outcomes, approvals) = audit_rows(view, &branch);

    let mut emitted: BTreeSet<TurnId> = BTreeSet::new();
    // An excluded turn stays in history, in this transcript and in the report;
    // it is only kept out of the model's request (decision 7). Dimming and one
    // tag row say so, while the text stays readable.
    let modes = super::turn_modes(&ancestry);
    let mut tagged: BTreeSet<TurnId> = BTreeSet::new();
    for (position, stored) in ancestry.iter().enumerate() {
        let mode = stored
            .entry
            .turn_id
            .as_ref()
            .and_then(|turn| modes.get(turn))
            .copied()
            .unwrap_or_default();
        let start = rows.len();
        if mode == ContextMode::Excluded
            && stored
                .entry
                .turn_id
                .as_ref()
                .is_some_and(|turn| tagged.insert(turn.clone()))
        {
            rows.push(Line::styled(
                "── excluded turn (kept in history; never sent to the model) ──",
                dim_style(),
            ));
        }
        match &stored.entry.payload {
            EntryPayload::UserMessage(message) => {
                push_label(&mut rows, "you");
                for block in &message.content {
                    media_or_text(&mut rows, block, "  ");
                }
                rows.push(Line::default());
            }
            EntryPayload::AssistantResponse(response) => {
                push_label(&mut rows, "assistant");
                if let Some(error) = &response.error {
                    rows.push(Line::styled(
                        format!("  error: {}", model_error_code(error.code)),
                        error_style(),
                    ));
                }
                for block in &response.content {
                    match block {
                        ContentBlock::Text { text } => {
                            rows.extend(markdown(text));
                        }
                        ContentBlock::ReasoningSummary { text } => {
                            rows.extend(reasoning_rows(text, reasoning));
                        }
                        ContentBlock::ToolCall {
                            call_id,
                            name,
                            arguments,
                            ..
                        } => {
                            rows.push(Line::styled(
                                format!("  tool: {}", super::call_summary(name, arguments)),
                                tool_style(),
                            ));
                            approval_rows(&mut rows, &approvals, call_id);
                        }
                        ContentBlock::ProviderState {
                            owner,
                            schema,
                            artifact,
                        } => rows.push(Line::styled(
                            format!(
                                "  provider state: {} ({} bytes, owner {}, not shown)",
                                bounded(schema, 80),
                                artifact.bytes,
                                model_name(owner)
                            ),
                            dim_style(),
                        )),
                        other => media_or_text(&mut rows, other, "  "),
                    }
                }
                rows.push(Line::default());
            }
            EntryPayload::ToolResult(result) => {
                let status = tool_status(result.result.status);
                let code = result
                    .result
                    .metadata
                    .code
                    .as_deref()
                    .map(|code| format!(" ({})", bounded(code, 80)))
                    .unwrap_or_default();
                rows.push(Line::styled(
                    format!("  tool result: {status}{code}"),
                    tool_style(),
                ));
                let mut lines = 0usize;
                for block in &result.result.content {
                    if let ContentBlock::Text { text } = block {
                        for text in text.lines() {
                            if lines == 6 {
                                rows.push(Line::styled("  …", dim_style()));
                                break;
                            }
                            rows.push(Line::raw(format!("  {}", bounded(text, 200))));
                            lines += 1;
                        }
                    }
                }
                if result.result.metadata.output_truncated {
                    rows.push(Line::styled("  output truncated", dim_style()));
                }
                if !result.result.metadata.artifacts.is_empty() {
                    rows.push(Line::styled(
                        format!(
                            "  artifacts captured: {}",
                            result.result.metadata.artifacts.len()
                        ),
                        dim_style(),
                    ));
                }
                rows.push(Line::default());
            }
            EntryPayload::ModelChanged(changed) => {
                rows.push(Line::styled(
                    format!(
                        "model switched: {} (portability: {})",
                        model_name(&changed.model),
                        portability(changed.portability)
                    ),
                    accent_style(),
                ));
                rows.push(Line::default());
            }
            EntryPayload::ContextControl(control) => {
                let action = match control.mode {
                    ContextMode::Pinned => "Pinned",
                    ContextMode::Excluded => "Excluded",
                    ContextMode::Default => "Reset",
                };
                rows.push(Line::styled(
                    format!("{action} turn {}", turn_label(&control.turn_id)),
                    accent_style(),
                ));
                rows.push(Line::default());
            }
            EntryPayload::Compaction(compaction) => {
                rows.push(Line::styled(
                    "── compaction summary ──────────────────────",
                    accent_style(),
                ));
                let model = compaction
                    .model
                    .as_ref()
                    .map(model_name)
                    .unwrap_or_else(|| "no model".into());
                rows.push(Line::styled(
                    format!(
                        "  {} · {} entries covered · {}",
                        bounded(&compaction.strategy, 40),
                        compaction.covered_entry_ids.len(),
                        model
                    ),
                    dim_style(),
                ));
                for text in compaction.summary.lines() {
                    rows.push(Line::raw(format!("  {}", bounded(text, 200))));
                }
                rows.push(Line::styled(
                    "────────────────────────────────────────────",
                    accent_style(),
                ));
                rows.push(Line::default());
            }
        }
        let last_in_commit = ancestry
            .get(position + 1)
            .is_none_or(|next| next.sequence != stored.sequence);
        if last_in_commit {
            for (turn_id, finish) in &outcomes {
                if finish.2 == stored.sequence && !emitted.contains(turn_id) {
                    rows.push(outcome_row(finish.0, finish.1.as_deref()));
                    rows.push(Line::default());
                    emitted.insert(turn_id.clone());
                }
            }
        }
        if mode == ContextMode::Excluded {
            dim(&mut rows[start..]);
        }
    }
    // A turn may finish in a commit that carries no entry (cancellation or a
    // failure before any response). Show those outcomes after the branch.
    for (turn_id, finish) in &outcomes {
        if emitted.insert(turn_id.clone()) {
            rows.push(outcome_row(finish.0, finish.1.as_deref()));
            rows.push(Line::default());
        }
    }

    if let Some(partial) = &view.partial {
        rows.push(Line::styled(
            format!("assistant (streaming · {})", model_name(&partial.model)),
            label_style(),
        ));
        for block in partial.blocks.values() {
            match block {
                PartialBlock::Text(text) => rows.extend(markdown(text)),
                PartialBlock::ReasoningSummary(text) => {
                    rows.extend(reasoning_rows(text, reasoning));
                }
                PartialBlock::ProviderState { schema, bytes } => rows.push(Line::styled(
                    format!(
                        "  provider state: {} ({bytes} bytes, not shown)",
                        bounded(schema, 80)
                    ),
                    dim_style(),
                )),
                PartialBlock::ToolCall {
                    name,
                    arguments,
                    ready,
                    ..
                } => rows.push(Line::styled(
                    format!(
                        "  tool: {} {}{}",
                        bounded(name, 80),
                        bounded(arguments, 320),
                        if *ready { "" } else { " …" }
                    ),
                    tool_style(),
                )),
            }
        }
        rows.push(Line::default());
    } else if view.phase == harness_core::runtime::Phase::AwaitingResolution
        && !view.awaiting.is_empty()
    {
        rows.push(Line::styled(
            "waiting for a statement about an uncertain invocation",
            error_style(),
        ));
        rows.push(Line::default());
    }

    for notice in notices {
        match notice {
            Notice::Diagnostic(text) => {
                rows.push(Line::styled(format!("diagnostic: {text}"), dim_style()))
            }
            Notice::Fault(text) => rows.push(Line::styled(text.clone(), error_style())),
            Notice::CompactionFailed(code) => rows.push(Line::styled(
                format!("compaction failed: {}", bounded(code, 80)),
                error_style(),
            )),
            Notice::Recovered(text) => rows.push(Line::styled(text.clone(), dim_style())),
            Notice::Info(text) => rows.push(Line::styled(text.clone(), dim_style())),
        }
    }
    if view.phase == harness_core::runtime::Phase::Faulted
        && !notices.iter().any(|n| matches!(n, Notice::Fault(_)))
    {
        rows.push(Line::styled("fault: runtime stopped", error_style()));
    }
    if rows.is_empty() {
        rows.push(Line::styled(
            "no messages yet — type a prompt and press Enter",
            dim_style(),
        ));
    }
    rows
}

type Outcomes = BTreeMap<TurnId, (Option<TurnOutcome>, Option<String>, u64)>;
type Approvals = BTreeMap<CallId, (Option<ApprovalId>, Option<Decision>)>;

fn audit_rows(view: &RuntimeView, branch: &BTreeSet<EntryId>) -> (Outcomes, Approvals) {
    let mut starts = BTreeSet::new();
    let mut outcomes = BTreeMap::new();
    let mut approvals = Approvals::new();
    for (sequence, _, event) in view.snapshot.events() {
        match event {
            JournalEvent::TurnStarted(started) if branch.contains(&started.user_entry_id) => {
                starts.insert(started.turn_id.clone());
            }
            JournalEvent::TurnFinished(finished) if starts.contains(&finished.turn_id) => {
                outcomes.insert(
                    finished.turn_id.clone(),
                    (Some(finished.outcome), finished.code.clone(), *sequence),
                );
            }
            JournalEvent::ApprovalRequested(requested) => {
                approvals
                    .entry(requested.call.call_id.clone())
                    .or_default()
                    .0 = Some(requested.approval_id.clone());
            }
            JournalEvent::AuthorizationDecided(decided) => {
                approvals.entry(decided.call.call_id.clone()).or_default().1 =
                    Some(decided.decision);
            }
            _ => {}
        }
    }
    (outcomes, approvals)
}

fn outcome_row(outcome: Option<TurnOutcome>, code: Option<&str>) -> Line<'static> {
    let outcome = outcome.unwrap_or(TurnOutcome::Completed);
    let label = match outcome {
        TurnOutcome::Completed => "turn: completed",
        TurnOutcome::Cancelled => "turn: cancelled",
        TurnOutcome::Interrupted => "turn: interrupted",
        TurnOutcome::Failed => "turn: failed",
    };
    let text = match code {
        Some(code) => format!("{label} ({})", bounded(code, 80)),
        None => label.to_owned(),
    };
    Line::styled(
        text,
        if outcome == TurnOutcome::Completed {
            dim_style()
        } else {
            error_style()
        },
    )
}

fn approval_rows(rows: &mut Vec<Line<'static>>, approvals: &Approvals, call: &CallId) {
    let Some((requested, decided)) = approvals.get(call) else {
        return;
    };
    if let Some(approval_id) = requested {
        rows.push(Line::styled(
            format!(
                "  approval requested: {} (a allow · d deny · r allow and remember)",
                short_id(approval_id.as_str())
            ),
            accent_style(),
        ));
    }
    if let Some(decision) = decided {
        rows.push(Line::styled(
            format!(
                "  approval: {}",
                match decision {
                    Decision::Allow => "allowed",
                    Decision::Deny => "denied",
                }
            ),
            tool_style(),
        ));
    }
}

/// Dim rows an excluded turn owns. Wrapping keeps span styles and the renderer
/// patches them over the line style, so both are patched: the turn is visibly
/// dimmed without hiding or truncating one character of it.
fn dim(rows: &mut [Line<'static>]) {
    for row in rows.iter_mut() {
        row.style = row.style.patch(dim_style());
        for span in &mut row.spans {
            span.style = span.style.patch(dim_style());
        }
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn push_label(rows: &mut Vec<Line<'static>>, label: &str) {
    rows.push(Line::styled(label.to_owned(), label_style()));
}

fn media_or_text(rows: &mut Vec<Line<'static>>, block: &ContentBlock, prefix: &str) {
    match block {
        ContentBlock::Text { text } => {
            if text.is_empty() {
                rows.push(Line::raw(prefix.to_owned()));
            }
            for text in text.lines() {
                rows.push(Line::raw(format!("{prefix}{}", bounded(text, 400))));
            }
        }
        ContentBlock::Image { mime_type, .. } => rows.push(Line::styled(
            format!("{prefix}[image {}]", bounded(mime_type, 60)),
            dim_style(),
        )),
        ContentBlock::Audio { mime_type, .. } => rows.push(Line::styled(
            format!("{prefix}[audio {}]", bounded(mime_type, 60)),
            dim_style(),
        )),
        ContentBlock::File {
            mime_type, name, ..
        } => rows.push(Line::styled(
            format!(
                "{prefix}[file {} {}]",
                bounded(name, 60),
                bounded(mime_type, 60)
            ),
            dim_style(),
        )),
        ContentBlock::ReasoningSummary { text } => {
            rows.extend(reasoning_rows(text, false));
        }
        // Tool calls and native state are rendered by their own owners.
        ContentBlock::ToolCall { .. } | ContentBlock::ProviderState { .. } => {}
    }
}

/// Collapse a reasoning summary to one bounded line unless expanded.
fn reasoning_rows(text: &str, expanded: bool) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    if expanded {
        return text
            .lines()
            .map(|text| Line::styled(format!("  {text}"), dim_style()))
            .collect();
    }
    let first = text.lines().next().unwrap_or_default();
    let more = text.lines().count().saturating_sub(1);
    let suffix = if more > 0 {
        format!(" … (+{more} lines, Ctrl-R)")
    } else {
        " (Ctrl-R)".into()
    };
    vec![Line::styled(
        format!("  reasoning: {}{suffix}", bounded(first, 160)),
        dim_style(),
    )]
}

fn tool_status(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Succeeded => "succeeded",
        ToolStatus::Failed => "failed",
        ToolStatus::InvalidArguments => "invalid arguments",
        ToolStatus::Denied => "denied",
        ToolStatus::TimedOut => "timed out",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::InfrastructureFailure => "infrastructure failure",
        ToolStatus::Uncertain => "uncertain",
    }
}

fn model_error_code(code: harness_core::session::ModelErrorCode) -> &'static str {
    use harness_core::session::ModelErrorCode;
    match code {
        ModelErrorCode::InvalidRequest => "invalid_request",
        ModelErrorCode::UnsupportedCapability => "unsupported_capability",
        ModelErrorCode::Authentication => "authentication",
        ModelErrorCode::RateLimit => "rate_limit",
        ModelErrorCode::Transport => "transport",
        ModelErrorCode::ProviderFailure => "provider_failure",
        ModelErrorCode::MalformedStream => "malformed_stream",
        ModelErrorCode::LimitExceeded => "limit_exceeded",
        ModelErrorCode::ContextOverflow => "context_overflow",
    }
}

fn portability(portability: Portability) -> &'static str {
    match portability {
        Portability::Reject => "reject",
        Portability::OmitOptional => "omit optional",
        Portability::ConvertSupported => "convert supported",
    }
}

fn model_name(model: &ModelRef) -> String {
    format!(
        "{}/{}",
        bounded(&model.provider_instance, 40),
        bounded(&model.model, 60)
    )
}

/// The last eight characters of a turn's identity, enough to tell turns apart
/// in one line without a full UUID.
fn turn_label(turn: &TurnId) -> String {
    turn.to_string()
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn label_style() -> Style {
    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
}

fn dim_style() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn tool_style() -> Style {
    Style::new().fg(Color::Yellow)
}

fn error_style() -> Style {
    Style::new().fg(Color::Red)
}

fn accent_style() -> Style {
    Style::new().fg(Color::Magenta)
}

fn code_style() -> Style {
    Style::new().fg(Color::Green)
}

/// Minimal Markdown: headings, emphasis, lists, quotes, inline code and fenced
/// code as a distinct block. No syntax highlighting and no HTML execution.
fn markdown(text: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut builder = MarkdownBuilder::default();
    for event in Parser::new_ext(text, Options::empty()) {
        builder.event(event, &mut out);
    }
    builder.flush(&mut out);
    if !out.is_empty() && out.last().is_some_and(|line| line.width() > 0) {
        out.push(Line::default());
    }
    out
}

#[derive(Default)]
struct MarkdownBuilder {
    spans: Vec<Span<'static>>,
    emphasis: usize,
    strong: usize,
    heading: bool,
    quote: usize,
    lists: Vec<Option<u64>>,
    item: Option<String>,
    code: Option<Vec<String>>,
    code_language: String,
    link: bool,
}

impl MarkdownBuilder {
    fn event(&mut self, event: MarkdownEvent<'_>, out: &mut Vec<Line<'static>>) {
        match event {
            MarkdownEvent::Start(tag) => self.start(tag, out),
            MarkdownEvent::End(tag) => self.end(tag, out),
            MarkdownEvent::Text(text) => {
                if let Some(code) = &mut self.code {
                    code.extend(text.split('\n').map(str::to_owned));
                } else {
                    self.push(&text);
                }
            }
            MarkdownEvent::Code(text) => self.push_styled(&text, inline_code_style()),
            MarkdownEvent::SoftBreak => self.push(" "),
            MarkdownEvent::HardBreak => self.flush(out),
            MarkdownEvent::Rule => {
                self.flush(out);
                self.separate(out);
                out.push(Line::styled("────", dim_style()));
            }
            MarkdownEvent::TaskListMarker(done) => {
                self.push(if done { "[x] " } else { "[ ] " });
            }
            // Raw HTML is shown as inert text, never interpreted.
            MarkdownEvent::Html(text) | MarkdownEvent::InlineHtml(text) => self.push(&text),
            MarkdownEvent::FootnoteReference(label) => self.push(&format!("[{label}]")),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>, out: &mut Vec<Line<'static>>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush(out);
                self.separate(out);
                let hashes = usize::from(level as u8).clamp(1, 6);
                self.item = Some(format!("{} ", "#".repeat(hashes)));
                self.heading = true;
            }
            Tag::BlockQuote(_) => {
                self.flush(out);
                self.separate(out);
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush(out);
                self.code = Some(Vec::new());
                self.code_language = match kind {
                    CodeBlockKind::Fenced(language) => bounded(&language, 40),
                    CodeBlockKind::Indented => String::new(),
                };
            }
            Tag::List(start) => {
                self.flush(out);
                self.separate(out);
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush(out);
                let depth = self.lists.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.lists.last_mut() {
                    Some(Some(next)) => {
                        let marker = format!("{next}. ");
                        *next += 1;
                        marker
                    }
                    _ => "• ".to_owned(),
                };
                self.item = Some(format!("{indent}{marker}"));
            }
            Tag::Emphasis => self.emphasis += 1,
            Tag::Strong => self.strong += 1,
            Tag::Link { .. } => self.link = true,
            Tag::Image { .. } => self.push("[image]"),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd, out: &mut Vec<Line<'static>>) {
        match tag {
            TagEnd::Paragraph => {
                self.flush(out);
                self.separate(out);
            }
            TagEnd::Heading(_) => {
                self.flush(out);
                self.heading = false;
                self.separate(out);
            }
            TagEnd::BlockQuote(_) => {
                self.flush(out);
                self.quote = self.quote.saturating_sub(1);
                self.separate(out);
            }
            TagEnd::CodeBlock => {
                if let Some(lines) = self.code.take() {
                    let language = std::mem::take(&mut self.code_language);
                    let title = if language.is_empty() {
                        "┌─ code".to_owned()
                    } else {
                        format!("┌─ code {language}")
                    };
                    let mut lines = lines;
                    while lines.last().is_some_and(|text| text.is_empty()) {
                        lines.pop();
                    }
                    out.push(Line::styled(title, code_style()));
                    for text in lines {
                        out.push(Line::styled(format!("│ {}", line(&text)), code_style()));
                    }
                    out.push(Line::styled("└─", code_style()));
                    self.separate(out);
                }
            }
            TagEnd::List(_) => {
                self.flush(out);
                self.lists.pop();
                self.separate(out);
            }
            TagEnd::Item => self.flush(out),
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Link => self.link = false,
            _ => {}
        }
    }

    fn push(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.push_styled(text, self.style());
    }

    fn push_styled(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        self.spans.push(Span::styled(line(text), style));
    }

    fn style(&self) -> Style {
        let mut style = if self.heading {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        if self.emphasis > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strong > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.link {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        style
    }

    fn flush(&mut self, out: &mut Vec<Line<'static>>) {
        if self.spans.is_empty() && self.item.is_none() {
            return;
        }
        let mut spans = Vec::new();
        if self.quote > 0 {
            spans.push(Span::styled("│ ".repeat(self.quote), dim_style()));
        }
        if let Some(item) = self.item.take() {
            spans.push(Span::styled(item, label_style()));
        }
        spans.append(&mut self.spans);
        out.push(Line::from(spans));
    }

    /// One blank row between blocks; repeated calls add none.
    fn separate(&self, out: &mut Vec<Line<'static>>) {
        if out.last().is_some_and(|line| line.width() > 0) {
            out.push(Line::default());
        }
    }
}

fn inline_code_style() -> Style {
    Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM)
}
