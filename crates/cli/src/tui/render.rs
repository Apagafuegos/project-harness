//! Layout and widgets for [`super::App`]. Backend-free: everything draws into
//! the supplied [`Frame`].

use harness_core::{
    runtime::{Phase, RuntimeView},
    session::{ModelRef, RuntimeState, Usage},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};

use super::{App, Modal, ResolutionMode};
use crate::text::bounded;

/// Height bound of the prompt editor, including wrapped lines.
const PROMPT_LINES: u16 = 6;

pub(crate) fn draw(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let width = area.width as usize;
    let banner = u16::from(app.last_error.is_some());
    let status = u16::from(area.height > banner);
    let prompt = prompt_height(app, width, area.height.saturating_sub(banner + status));
    let transcript = area.height.saturating_sub(banner + status + prompt);
    let chunks = Layout::vertical([
        Constraint::Length(transcript),
        Constraint::Length(prompt),
        Constraint::Length(banner),
        Constraint::Length(status),
    ])
    .split(area);

    draw_transcript(app, frame, chunks[0], width);
    draw_prompt(app, frame, chunks[1], width);
    if let Some(error) = &app.last_error
        && chunks[2].height > 0
    {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!(" error: {error}"),
                Style::new().fg(Color::Red),
            )),
            chunks[2],
        );
    }
    if chunks[3].height > 0 {
        let status = Line::styled(
            status_text(app),
            Style::new()
                .fg(Color::Black)
                .bg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        );
        frame.render_widget(Paragraph::new(status), chunks[3]);
    }
    if let Some((title, content)) = modal_content(app) {
        draw_modal(frame, area, &title, content);
    }
}

fn draw_transcript(app: &mut App, frame: &mut Frame, area: Rect, width: usize) {
    let physical = wrap_lines(app.transcript.clone(), width);
    let height = area.height as usize;
    let max_scroll = physical.len().saturating_sub(height);
    app.scroll = app.scroll.min(max_scroll);
    app.page = height.max(1);
    if height == 0 {
        return;
    }
    let end = physical.len().saturating_sub(app.scroll);
    let start = end.saturating_sub(height);
    frame.render_widget(
        Paragraph::new(Text::from(physical[start..end].to_vec())),
        area,
    );
}

fn draw_prompt(app: &App, frame: &mut Frame, area: Rect, width: usize) {
    let mut lines = wrapped_prompt(app, width);
    let height = area.height as usize;
    if height == 0 || lines.is_empty() {
        return;
    }
    if lines.len() > height {
        lines.drain(..lines.len() - height);
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn prompt_height(app: &App, width: usize, available: u16) -> u16 {
    if available == 0 {
        return 0;
    }
    let lines = wrapped_prompt(app, width).len() as u16;
    lines.clamp(1, PROMPT_LINES).min(available)
}

fn wrapped_prompt(app: &App, width: usize) -> Vec<Line<'static>> {
    // A visible cursor keeps the editor usable in a pure-buffer test.
    let mut chars: Vec<char> = app.prompt.chars().collect();
    if app.cursor <= chars.len() {
        chars.insert(app.cursor, '\u{258f}');
    }
    let display: String = chars.into_iter().collect();
    let mut lines = Vec::new();
    for (index, logical) in display.split('\n').enumerate() {
        let prefix = if index == 0 { "> " } else { "  " };
        lines.push(Line::styled(
            format!("{prefix}{}", crate::text::line(logical)),
            Style::new().fg(Color::Cyan),
        ));
    }
    wrap_lines(lines, width)
}

fn draw_modal(frame: &mut Frame, area: Rect, title: &str, content: Vec<Line<'static>>) {
    let width = area.width.saturating_sub(2).min(72);
    let height = area.height.saturating_sub(2).min(14);
    if width == 0 || height == 0 {
        return;
    }
    let rect = Rect::new(
        (area.width - width) / 2,
        (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Yellow))
        .title(format!(" {title} "));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let lines = wrap_lines(content, inner.width as usize);
    frame.render_widget(
        Paragraph::new(Text::from(
            lines
                .into_iter()
                .take(inner.height as usize)
                .collect::<Vec<_>>(),
        )),
        inner,
    );
}

fn modal_content(app: &App) -> Option<(String, Vec<Line<'static>>)> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    match &app.modal {
        Modal::None => None,
        Modal::Approval { approval_id } => {
            let view = app.view()?;
            let ticket = view
                .pending_approval
                .as_ref()
                .filter(|ticket| &ticket.approval_id == approval_id)?;
            let mut lines = Vec::new();
            match call_proposal(view, &ticket.call.call_id) {
                Some((name, arguments)) => {
                    lines.push(Line::raw(format!("tool: {}", bounded(&name, 80))));
                    lines.push(Line::raw(format!("args: {}", bounded(&arguments, 240))));
                }
                None => lines.push(Line::raw(format!(
                    "call: {}",
                    bounded(ticket.call.call_id.as_str(), 40)
                ))),
            }
            lines.push(Line::styled(
                format!(
                    "plan: {}",
                    bounded(
                        &ticket.plan_digest.as_str()[..12.min(ticket.plan_digest.as_str().len())],
                        12
                    )
                ),
                dim,
            ));
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "a allow · d deny · r allow and remember · Esc deny",
                Style::new().fg(Color::Yellow),
            ));
            Some(("approve tool call".into(), lines))
        }
        Modal::Uncertain { index, mode, input } => {
            let view = app.view()?;
            if view.awaiting.is_empty() {
                return None;
            }
            let mut lines = Vec::new();
            for (position, invocation) in view.awaiting.iter().enumerate() {
                let marker = if position == *index { ">" } else { " " };
                lines.push(Line::raw(format!(
                    "{marker} {} · {} · {}",
                    bounded(&invocation.tool, 40),
                    bounded(invocation.invocation_id.as_str(), 8),
                    hint(invocation.hint.as_ref())
                )));
            }
            lines.push(Line::raw(""));
            for choice in [
                ResolutionMode::AcknowledgedUnknown,
                ResolutionMode::ObservedSucceeded,
                ResolutionMode::ObservedFailed,
            ] {
                lines.push(Line::styled(
                    format!(
                        " ({}) {}",
                        if choice == *mode { "●" } else { " " },
                        choice.label()
                    ),
                    if choice == *mode {
                        Style::new().fg(Color::Yellow)
                    } else {
                        dim
                    },
                ));
            }
            lines.push(Line::raw(""));
            let observation = if input.is_empty() {
                "(type what you observed)".to_owned()
            } else {
                bounded(input, 160)
            };
            lines.push(Line::raw(format!("statement: {observation}")));
            lines.push(Line::styled(
                "↑↓ invocation · ←→ choice · Enter submit",
                dim,
            ));
            Some(("uncertain invocation".into(), lines))
        }
        Modal::ConfirmQuit => Some((
            "quit?".into(),
            vec![
                Line::raw("Quit harness?"),
                Line::styled(
                    "y or Enter quit · n or Esc cancel",
                    Style::new().fg(Color::Yellow),
                ),
            ],
        )),
    }
}

fn call_proposal(
    view: &RuntimeView,
    call: &harness_core::session::CallId,
) -> Option<(String, String)> {
    for (_, stored) in view.snapshot.entries() {
        if let harness_core::session::EntryPayload::AssistantResponse(response) =
            &stored.entry.payload
        {
            for block in &response.content {
                if let harness_core::session::ContentBlock::ToolCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } = block
                    && call_id == call
                {
                    return Some((
                        name.clone(),
                        serde_json::Value::Object(arguments.clone()).to_string(),
                    ));
                }
            }
        }
    }
    None
}

fn hint(hint: Option<&harness_core::tools::RecoveryHint>) -> String {
    use harness_core::tools::RecoveryHint;
    match hint {
        None => "no current-file evidence".into(),
        Some(RecoveryHint::MatchesPostImage) => "file matches the post-image".into(),
        Some(RecoveryHint::MatchesPreImage) => "file matches the pre-image".into(),
        Some(RecoveryHint::ChangedSince) => "file changed since".into(),
        Some(RecoveryHint::Missing) => "target missing".into(),
        Some(RecoveryHint::Unavailable { reason }) => {
            format!("evidence unavailable ({})", bounded(reason, 40))
        }
    }
}

fn status_text(app: &App) -> String {
    let model = current_model(app);
    // The sourced catalog limit is known only for the model the session was
    // opened with; a later switch needs a catalog the shell has not supplied.
    let limit = (model == app.info.model)
        .then_some(app.info.context_limit)
        .flatten();
    let phase = app
        .view()
        .map(|view| phase_name(view.phase))
        .unwrap_or("connecting");
    let usage = app
        .view()
        .and_then(last_usage)
        .map(|usage| {
            format!(
                "{}/{}",
                optional(usage.input_tokens),
                optional(usage.output_tokens)
            )
        })
        .unwrap_or_else(|| "—".into());
    let context = match (&app.context, limit) {
        (Some((totals, flags)), Some(limit)) if !flags.budget_unknown => {
            format!("ctx {}/{}", compact(totals.estimated_total), compact(limit))
        }
        (Some((totals, _)), _) => {
            format!("ctx {} (unknown budget)", compact(totals.estimated_total))
        }
        (None, Some(limit)) => format!("ctx —/{}", compact(limit)),
        (None, None) => "ctx —".into(),
    };
    format!(
        " {} · {} · {} · usage {} · {} ",
        bounded(
            app.info
                .workspace
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("workspace"),
            24
        ),
        model_name(&model),
        phase,
        usage,
        context
    )
}

fn current_model(app: &App) -> ModelRef {
    app.view()
        .and_then(|view| {
            view.snapshot
                .model_at(view.snapshot.head())
                .ok()
                .cloned()
                .or_else(|| view.partial.as_ref().map(|partial| partial.model.clone()))
        })
        .unwrap_or_else(|| app.info.model.clone())
}

fn last_usage(view: &RuntimeView) -> Option<Usage> {
    if let Some(usage) = view
        .partial
        .as_ref()
        .and_then(|partial| partial.usage.clone())
    {
        return Some(usage);
    }
    view.snapshot
        .ancestry(view.snapshot.head())
        .ok()?
        .iter()
        .rev()
        .find_map(|stored| {
            if let harness_core::session::EntryPayload::AssistantResponse(response) =
                &stored.entry.payload
            {
                response.usage.clone()
            } else {
                None
            }
        })
}

fn optional(value: Option<u64>) -> String {
    value.map_or_else(|| "?".into(), compact)
}

fn compact(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Durable(RuntimeState::Idle) => "idle",
        Phase::Durable(RuntimeState::Preparing) => "preparing",
        Phase::Durable(RuntimeState::Inferring) => "inferring",
        Phase::Durable(RuntimeState::Processing) => "processing",
        Phase::Durable(RuntimeState::Authorizing) => "authorizing",
        Phase::Durable(RuntimeState::Executing) => "executing",
        Phase::Durable(RuntimeState::Continuing) => "continuing",
        Phase::Durable(RuntimeState::Cancelling) => "cancelling",
        Phase::Durable(RuntimeState::Recovering) => "recovering",
        Phase::RecoveryRequired => "recovery required",
        Phase::AwaitingResolution => "resolution required",
        Phase::Faulted => "faulted",
    }
}

fn model_name(model: &ModelRef) -> String {
    format!(
        "{}/{}",
        bounded(&model.provider_instance, 32),
        bounded(&model.model, 48)
    )
}

/// Wrap every logical line to `width` display cells, preserving span styles.
fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for line in lines {
        out.extend(wrap_line(line, width));
    }
    out
}

fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::default()];
    }
    let mut out = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        let style = span.style;
        let mut buffer = String::new();
        for ch in span.content.chars() {
            let cells = char_width(ch);
            if used + cells > width && used > 0 {
                if !buffer.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buffer), style));
                }
                out.push(Line::from(std::mem::take(&mut spans)));
                used = 0;
            }
            buffer.push(ch);
            used += cells;
        }
        if !buffer.is_empty() {
            spans.push(Span::styled(buffer, style));
        }
    }
    if !spans.is_empty() || out.is_empty() {
        out.push(Line::from(spans));
    }
    out
}

fn char_width(ch: char) -> usize {
    if ch.is_ascii() {
        usize::from(!ch.is_control())
    } else {
        Span::raw(ch.to_string()).width()
    }
}
