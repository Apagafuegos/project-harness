//! Layout and widgets for [`super::App`]. Backend-free: everything draws into
//! the supplied [`Frame`].

use harness_core::{
    runtime::{Phase, RuntimeView},
    session::{ModelRef, RuntimeState},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};

use super::{
    App, Modal, ResolutionMode,
    theme::{Role, Theme},
    tools,
};
use crate::text::bounded;
use unicode_segmentation::UnicodeSegmentation;

/// Height bound of the prompt editor, including wrapped lines.
const PROMPT_LINES: u16 = 6;

/// A projection marker: a code body never word-reflows, it hard-wraps.
pub(super) const CODE_ROW: &str = "\u{0}code";
/// A projection marker: a horizontal rule expands to the pane width.
pub(super) const RULE_ROW: &str = "\u{0}rule";

pub(crate) fn draw(app: &mut App, frame: &mut Frame) {
    let area = frame.area();
    let width = area.width as usize;
    let mut errors = app
        .last_error
        .as_ref()
        .map(|error| {
            wrap_lines(
                vec![Line::styled(
                    format!(" error: {error}"),
                    app.theme.style(Role::Error),
                )],
                width,
                app.theme.ascii,
            )
        })
        .unwrap_or_default();
    if errors.len() > 3 {
        errors.truncate(3);
        errors.push(Line::styled("…", app.theme.style(Role::Error)));
    }
    let banner = (errors.len() as u16).min(area.height.saturating_sub(3));
    let status = u16::from(area.height > 0);
    let hints = u16::from(area.height > 6);
    let framed = area.height > 5;
    let available = area.height.saturating_sub(banner + status + hints);
    let prompt = (wrapped_prompt(app, width).len() as u16)
        .clamp(1, PROMPT_LINES)
        .saturating_add(u16::from(framed))
        .min(available.saturating_sub(1));
    let header = u16::from(area.height >= 12);
    let transcript = area
        .height
        .saturating_sub(banner + status + hints + prompt + header);
    let panes = Layout::vertical([Constraint::Length(header), Constraint::Min(0)]).split(area);
    frame.render_widget(
        Paragraph::new(app.theme.ascii_ready(header_text(app))).style(app.theme.style(Role::Dim)),
        panes[0],
    );
    let chunks = Layout::vertical([
        Constraint::Length(transcript),
        Constraint::Length(prompt),
        Constraint::Length(banner),
        Constraint::Length(hints),
        Constraint::Length(status),
    ])
    .split(panes[1]);
    draw_transcript(app, frame, chunks[0], width);
    draw_prompt(app, frame, chunks[1], width, framed);
    frame.render_widget(Paragraph::new(Text::from(errors)), chunks[2]);
    frame.render_widget(
        Paragraph::new(Line::styled(
            app.theme.ascii_ready(hint_text(app)),
            app.theme.style(Role::Dim),
        )),
        chunks[3],
    );
    frame.render_widget(
        Paragraph::new(app.theme.ascii_ready(status_text(app, width)))
            .style(app.theme.style(Role::Status)),
        chunks[4],
    );
    if let Some((title, content)) = modal_content(app) {
        let hints = match app.modal {
            Modal::Approval { .. } => "v raw · a allow · d deny · r remember · Esc deny",
            Modal::Uncertain { .. } => "↑↓ invocation · ←→ choice · Enter submit · PgUp/PgDn",
            _ => "y / Enter quit · n / Esc cancel",
        };
        let inner = draw_modal(
            frame,
            app.theme,
            &title,
            hints,
            content,
            None,
            &mut app.modal_scroll,
        );
        app.modal_page = inner.height.max(1) as usize;
    }
}

fn hint_text(app: &App) -> &'static str {
    if app.help {
        "↑↓ scroll · PgUp/PgDn page · Esc close"
    } else if matches!(app.modal, Modal::Approval { .. }) {
        "v raw args · a allow · d deny · r remember · Esc deny"
    } else if !matches!(app.modal, Modal::None)
        || app.navigation_captures_input()
        || app.inspector_captures()
    {
        "↑↓ select/scroll · Enter confirm · Esc close · F1 help"
    } else if app.active_turn().is_some() {
        "Esc cancel · Ctrl-C cancel/quit · PgUp scroll · F1 help"
    } else {
        "Enter send · Shift+Enter newline · Ctrl-K context · Ctrl-O sessions · F1 help"
    }
}

fn draw_transcript(app: &mut App, frame: &mut Frame, area: Rect, width: usize) {
    let empty = app.view().is_none_or(|v| {
        !v.snapshot
            .ancestry(v.snapshot.head())
            .unwrap_or_default()
            .iter()
            .any(|e| {
                matches!(
                    e.entry.payload,
                    harness_core::session::EntryPayload::UserMessage(_)
                )
            })
    });
    let mut rows = if empty {
        vec![
            Line::styled(
                app.theme.ascii_ready("Welcome to harness"),
                app.theme.style(Role::Accent),
            ),
            Line::raw(
                app.theme
                    .ascii_ready(format!("Workspace: {}", app.info.workspace.display())),
            ),
            Line::raw(
                app.theme
                    .ascii_ready(format!("Model: {}", model_name(&current_model(app)))),
            ),
            Line::styled(
                app.theme
                    .ascii_ready("Enter send · Alt-Enter newline · Ctrl-O sessions · F1 help"),
                app.theme.style(Role::Dim),
            ),
        ]
    } else {
        app.transcript.clone()
    };
    // Spacing after the final message must not consume the tiny tail viewport.
    while rows.last().is_some_and(|row| row.width() == 0) {
        rows.pop();
    }
    let physical = wrap_lines(rows, width, app.theme.ascii);
    let height = area.height as usize;
    let overflowing = physical.len() > height;
    let content_height = height.saturating_sub(usize::from(overflowing));
    if app.scroll > 0 {
        let added = physical.len().saturating_sub(app.physical_lines);
        app.scroll = app.scroll.saturating_add(added);
        app.new_lines = app.new_lines.saturating_add(added);
    } else {
        app.new_lines = 0;
    }
    app.physical_lines = physical.len();
    let max_scroll = physical.len().saturating_sub(content_height);
    app.scroll = app.scroll.min(max_scroll);
    app.page = content_height.max(1);
    if height == 0 {
        return;
    }
    let end = physical.len().saturating_sub(app.scroll);
    let start = end.saturating_sub(content_height);
    frame.render_widget(
        Paragraph::new(Text::from(physical[start..end].to_vec())),
        Rect {
            height: content_height as u16,
            ..area
        },
    );
    if overflowing {
        let percent = (end * 100) / physical.len().max(1);
        let marker = if app.scroll > 0 {
            format!("↓ {} new lines · End to follow · {percent}%", app.new_lines)
        } else {
            "100% · following".into()
        };
        frame.render_widget(
            Paragraph::new(Line::styled(
                app.theme.ascii_ready(marker),
                app.theme.style(Role::Dim),
            )),
            Rect::new(area.x, area.y + area.height - 1, area.width, 1),
        );
    }
}

fn draw_prompt(app: &App, frame: &mut Frame, mut area: Rect, width: usize, framed: bool) {
    if framed && area.height > 0 {
        let mode = if matches!(app.modal, Modal::Approval { .. }) {
            "approval"
        } else if app.active_turn().is_some() {
            "streaming…"
        } else {
            "message"
        };
        let title = if app.pasted_lines > 20 {
            format!(" {mode} [pasted {} lines] ", app.pasted_lines)
        } else {
            format!(" {mode} ")
        };
        let mut block = Block::new()
            .borders(Borders::TOP)
            .title(app.theme.ascii_ready(title))
            .border_style(app.theme.style(if app.active_turn().is_some() {
                Role::Dim
            } else {
                Role::Accent
            }));
        if app.theme.ascii {
            block = block.border_set(ratatui::symbols::border::Set {
                horizontal_top: "-",
                ..ratatui::symbols::border::Set::default()
            });
        }
        let inner = block.inner(area);
        frame.render_widget(block, area);
        area = inner;
    }
    if area.height == 0 || width < 3 {
        return;
    }
    let (mut lines, (x, y)) = prompt_layout(app, width);
    let start = y.saturating_add(1).saturating_sub(area.height as usize);
    lines = lines
        .into_iter()
        .skip(start)
        .take(area.height as usize)
        .collect();
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
    if !app.help
        && matches!(app.modal, Modal::None)
        && !app.navigation_captures_input()
        && !app.inspector_captures()
    {
        frame.set_cursor_position((area.x + x as u16, area.y + (y - start) as u16));
    }
}

fn wrapped_prompt(app: &App, width: usize) -> Vec<Line<'static>> {
    prompt_layout(app, width).0
}

/// The editor hard-wraps graphemes, so cursor geometry and text share one path.
fn prompt_layout(app: &App, width: usize) -> (Vec<Line<'static>>, (usize, usize)) {
    let mut rows = vec![String::from("> ")];
    let (mut x, mut y, mut chars) = (2usize, 0usize, 0usize);
    let mut cursor = (2, 0);
    for g in app.prompt.graphemes(true) {
        let display = crate::text::line(g);
        let n = Span::raw(&display).width();
        if g == "\n" {
            if chars == app.cursor {
                cursor = (x.min(width.saturating_sub(1)), y);
            }
            rows.push("  ".into());
            x = 2;
            y += 1;
            chars += 1;
            continue;
        }
        if x + n > width.max(3) {
            rows.push("  ".into());
            x = 2;
            y += 1;
        }
        if (chars..chars + g.chars().count()).contains(&app.cursor) {
            cursor = (x, y);
        }
        rows[y].push_str(&display);
        x += n;
        chars += g.chars().count();
    }
    if x >= width.max(3) {
        rows.push("  ".into());
        x = 2;
        y += 1;
    }
    if chars == app.cursor {
        cursor = (x, y);
    }
    let mut lines: Vec<_> = rows.into_iter().map(Line::raw).collect();
    if app.prompt.is_empty() {
        lines[0] = Line::from(vec![
            Span::raw("> "),
            Span::styled(
                app.theme.ascii_ready("Ask anything · F1 help"),
                app.theme.style(Role::Dim),
            ),
        ]);
    }
    (lines, cursor)
}

/// Shared content-sized, scrollable dialog frame. Returns the exposed content
/// rectangle (used by the inspector's existing preview-first safety guard).
pub(super) fn draw_modal(
    frame: &mut Frame,
    theme: Theme,
    title: &str,
    hints: &str,
    content: Vec<Line<'static>>,
    focus: Option<usize>,
    scroll: &mut usize,
) -> Rect {
    let area = frame.area();
    // Modal bodies are projection rows too: `NO_COLOR` strips their colour and
    // ASCII mode gets the same glyph fallbacks as the transcript.
    let mut content = content;
    theme.apply(&mut content);
    let width = (content
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(1)
        .max(title.len() + 2)
        .max(hints.len().min(72))
        + 2)
    .min(area.width.saturating_sub(2) as usize) as u16;
    let focused = focus.map(|index| {
        wrap_lines(
            content.iter().take(index).cloned().collect(),
            width.saturating_sub(2) as usize,
            theme.ascii,
        )
        .len()
    });
    let lines = wrap_lines(content, width.saturating_sub(2) as usize, theme.ascii);
    let height = (lines.len() + 2).min(area.height.saturating_sub(2) as usize) as u16;
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(theme.style(Role::Warning))
        .title(theme.ascii_ready(format!(" {title} ")));
    let mut inner = block.inner(rect);
    // At tiny sizes the frame collapses before content, but its title remains
    // present whenever there is room for chrome.
    if inner.height == 0 && rect.height > 0 {
        inner = Rect::new(rect.x, rect.y, rect.width, rect.height);
    }
    if let Some(focus) = focused {
        if focus < *scroll {
            *scroll = focus;
        } else if focus >= scroll.saturating_add(inner.height as usize) {
            *scroll = focus.saturating_sub(inner.height.saturating_sub(1) as usize);
        }
    }
    *scroll = (*scroll).min(lines.len().saturating_sub(inner.height as usize));
    let more = *scroll + (inner.height as usize) < lines.len();
    let footer = theme.ascii_ready(format!(
        " {}{}{} ",
        if *scroll > 0 { "more ↑ · " } else { "" },
        if more { "more ↓ · " } else { "" },
        hints
    ));
    frame.render_widget(Clear, rect);
    frame.render_widget(block.title_bottom(footer), rect);
    frame.render_widget(
        Paragraph::new(Text::from(
            lines
                .into_iter()
                .skip(*scroll)
                .take(inner.height as usize)
                .collect::<Vec<_>>(),
        )),
        inner,
    );
    inner
}

fn modal_content(app: &App) -> Option<(String, Vec<Line<'static>>)> {
    let dim = app.theme.style(Role::Dim);
    match &app.modal {
        Modal::None => None,
        Modal::Approval { approval_id, raw } => {
            let view = app.view()?;
            let ticket = view
                .pending_approval
                .as_ref()
                .filter(|ticket| &ticket.approval_id == approval_id)?;
            let mut lines = Vec::new();
            match call_proposal(view, &ticket.call.call_id) {
                Some((name, arguments)) => {
                    lines.extend(tools::approval(
                        &app.info.workspace,
                        &name,
                        &arguments,
                        *raw,
                    ));
                }
                None => lines.push(Line::raw(format!(
                    "call: {}",
                    bounded(ticket.call.call_id.as_str(), 40)
                ))),
            }
            lines.push(Line::raw(""));
            lines.push(tools::digest(ticket.plan_digest.as_str()));
            // The key hints live in the frame and footer, never in the body.
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
                        app.theme.style(Role::Warning)
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
        // The session screen owns these dialogs and draws them itself.
        Modal::ConfirmDelete { .. } | Modal::ExportPath { .. } | Modal::Notice { .. } => None,
        Modal::ConfirmQuit => Some((
            "quit?".into(),
            vec![
                Line::raw("Quit harness?"),
                Line::styled(
                    "y or Enter quit · n or Esc cancel",
                    app.theme.style(Role::Warning),
                ),
            ],
        )),
    }
}

fn call_proposal(
    view: &RuntimeView,
    call: &harness_core::session::CallId,
) -> Option<(String, serde_json::Map<String, serde_json::Value>)> {
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
                    return Some((name.clone(), arguments.clone()));
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

fn header_text(app: &App) -> String {
    let workspace = app
        .info
        .workspace
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("workspace");
    let Some(view) = app.view() else {
        return format!(" harness · {workspace} · connecting");
    };
    let id = view.snapshot.session_id().as_str();
    let siblings = view
        .snapshot
        .entries()
        .filter(|(_, e)| {
            matches!(
                e.entry.payload,
                harness_core::session::EntryPayload::UserMessage(_)
            )
        })
        .fold(std::collections::BTreeMap::new(), |mut counts, (_, e)| {
            *counts.entry(e.entry.parent_id.clone()).or_insert(0usize) += 1;
            counts
        })
        .values()
        .map(|n| n.saturating_sub(1))
        .sum::<usize>();
    format!(
        " harness · {workspace} · session {}{}",
        &id[..8.min(id.len())],
        if siblings > 0 {
            format!(" · branch: {siblings} siblings")
        } else {
            String::new()
        }
    )
}

fn status_text(app: &App, width: usize) -> String {
    let model = current_model(app);
    let limit = app
        .catalog
        .limits(&model)
        .and_then(|limits| limits.context_tokens.map(u64::from))
        .or_else(|| {
            (model == app.info.model)
                .then_some(app.info.context_limit)
                .flatten()
        });
    let phase = app
        .view()
        .map(|view| phase_name(view.phase))
        .unwrap_or("connecting");
    let context = match (&app.context, limit) {
        (Some((totals, flags)), Some(limit)) if !flags.budget_unknown => {
            format!(
                "context {} / {} ({}%)",
                compact(totals.estimated_total),
                compact(limit),
                totals.estimated_total.saturating_mul(100) / limit.max(1)
            )
        }
        (Some((totals, _)), _) => {
            format!(
                "context {} (unknown budget)",
                compact(totals.estimated_total)
            )
        }
        (None, Some(limit)) => format!("context — / {}", compact(limit)),
        (None, None) => "context —".into(),
    };
    let activity = if app.active_turn().is_some()
        || app
            .view()
            .is_some_and(|v| v.phase != Phase::Durable(RuntimeState::Idle))
    {
        let elapsed = app
            .turn_started
            .map(|start| app.now.saturating_sub(start).as_secs())
            .unwrap_or(0);
        let spinner = if app.theme.ascii {
            ["|", "/", "-", "\\"][(app.now.as_millis() / 100 % 4) as usize]
        } else {
            ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
                [(app.now.as_millis() / 100 % 10) as usize]
        };
        let bytes = app.view().and_then(|v| v.partial.as_ref()).map(|p| {
            p.blocks
                .values()
                .map(|b| match b {
                    harness_core::runtime::PartialBlock::Text(t)
                    | harness_core::runtime::PartialBlock::ReasoningSummary(t) => t.len(),
                    harness_core::runtime::PartialBlock::ToolCall { arguments, .. } => {
                        arguments.len()
                    }
                    _ => 0,
                })
                .sum::<usize>()
        });
        format!(
            "{spinner} {phase} {elapsed}s{}",
            bytes.map(|n| format!(" · {n} B")).unwrap_or_default()
        )
    } else {
        phase.to_owned()
    };
    let left = format!(
        " {} · {} ",
        bounded(
            app.info
                .workspace
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("workspace"),
            24
        ),
        model_name(&model),
    );
    let mut right = format!("{activity} · {context} ");
    if Span::raw(&left).width() + Span::raw(&right).width() > width {
        right = format!("{activity} ");
    }
    if Span::raw(&left).width() + Span::raw(&right).width() > width {
        right.clear();
    }
    let padding = width.saturating_sub(Span::raw(&left).width() + Span::raw(&right).width());
    format!("{left}{}{right}", " ".repeat(padding))
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

fn compact(value: u64) -> String {
    if value >= 1_000_000 && value.is_multiple_of(1_000_000) {
        format!("{}m", value / 1_000_000)
    } else if value >= 1_000 && value.is_multiple_of(1_000) {
        format!("{}k", value / 1_000)
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

/// Wrap logical rows by words and display cells. A row carrying [`CODE_ROW`]
/// hard-wraps and a row carrying [`RULE_ROW`] becomes a full-width rule; both
/// are width-independent projection hints, not a Markdown reparse.
pub(super) fn wrap_lines(
    lines: Vec<Line<'static>>,
    width: usize,
    ascii: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for mut line in lines {
        let mut code = false;
        let mut rule = false;
        line.spans.retain(|span| match span.content.as_ref() {
            CODE_ROW => {
                code = true;
                false
            }
            RULE_ROW => {
                rule = true;
                false
            }
            _ => true,
        });
        if rule {
            out.push(Line::styled(
                if ascii { "-" } else { "─" }.repeat(width),
                line.style,
            ));
            continue;
        }
        let gutter = line
            .spans
            .first()
            .filter(|s| s.content == "▌ " || s.content == "| ")
            .cloned();
        if gutter.is_some() {
            line.spans.remove(0);
        }
        if line.spans.first().is_some_and(|s| s.content == "assistant") && line.spans.len() == 2 {
            let available = width.saturating_sub(usize::from(gutter.is_some()) * 2);
            let padding = available.saturating_sub(line.width());
            if padding == 0 {
                line.spans.truncate(1);
            } else {
                line.spans.insert(1, Span::raw(" ".repeat(padding)));
            }
        }
        let wrapped = wrap_line(
            line,
            width.saturating_sub(usize::from(gutter.is_some()) * 2),
            code,
            ascii,
        );
        out.extend(wrapped.into_iter().map(|mut row| {
            if let Some(gutter) = &gutter {
                row.spans.insert(0, gutter.clone());
            }
            row
        }));
    }
    out
}

fn hanging_indent(text: &str) -> usize {
    let leading = text.chars().take_while(|c| *c == ' ').count();
    let rest = &text[leading..];
    let marker = if rest.starts_with("• ")
        || rest.starts_with("◦ ")
        || rest.starts_with("▪ ")
        || rest.starts_with("- ")
        || rest.starts_with("* ")
        || rest.starts_with("+ ")
        || rest.starts_with("> ")
        || rest.starts_with("│ ")
    {
        2
    } else if rest.starts_with("[^") {
        rest.find("] ").map_or(0, |end| end + 2)
    } else {
        rest.chars().take_while(char::is_ascii_digit).count()
    };
    let marker = if marker > 0 && rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        if rest[marker..].starts_with(". ") {
            marker + 2
        } else {
            0
        }
    } else {
        marker
    };
    leading + marker
}

fn wrap_line(line: Line<'static>, width: usize, code: bool, ascii: bool) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![Line::default()];
    }
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let mut offsets = Vec::new();
    let mut byte = 0;
    for span in &line.spans {
        offsets.push((
            byte,
            byte + span.content.len(),
            line.style.patch(span.style),
        ));
        byte += span.content.len();
    }
    let cells: Vec<(String, Style)> = text
        .grapheme_indices(true)
        .map(|(i, g)| {
            (
                g.to_owned(),
                offsets
                    .iter()
                    .find(|(a, b, _)| *a <= i && i < *b)
                    .map(|(_, _, s)| *s)
                    .unwrap_or(line.style),
            )
        })
        .collect();
    let indent = if code {
        0
    } else {
        hanging_indent(&text).min(width.saturating_sub(2))
    };
    let mut rows = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0;
    let mut i = 0;
    while i < cells.len() {
        let whitespace = cells[i].0.chars().all(char::is_whitespace);
        let mut end = i + 1;
        if !code {
            while end < cells.len() && cells[end].0.chars().all(char::is_whitespace) == whitespace {
                end += 1;
            }
        }
        let token_width: usize = cells[i..end]
            .iter()
            .map(|(g, _)| Span::raw(g.clone()).width())
            .sum();
        if !code && !whitespace && used > indent && used + token_width > width {
            // Whitespace at a word boundary is not carried to either edge.
            while spans
                .last()
                .is_some_and(|s: &Span<'_>| s.content.chars().all(char::is_whitespace))
            {
                spans.pop();
            }
            rows.push(Line::from(std::mem::take(&mut spans)).style(line.style));
            used = indent;
            if indent > 0 {
                spans.push(Span::styled(" ".repeat(indent), line.style));
            }
        }
        for (g, style) in &cells[i..end] {
            let n = Span::raw(g.clone()).width();
            if code && used + n > width.saturating_sub(1).max(1) && used > 0 {
                if used < width {
                    spans.push(Span::styled(if ascii { ">" } else { "↪" }, line.style));
                }
                rows.push(Line::from(std::mem::take(&mut spans)).style(line.style));
                used = 0;
            }
            if used + n > width {
                if !code && whitespace {
                    continue;
                }
                if used > 0 {
                    rows.push(Line::from(std::mem::take(&mut spans)).style(line.style));
                }
                used = indent;
                if indent > 0 {
                    spans.push(Span::styled(" ".repeat(indent), line.style));
                }
            }
            if n > width.saturating_sub(used) {
                continue;
            } // impossible-wide grapheme in a one-cell pane
            spans.push(Span::styled(g.clone(), *style));
            used += n;
        }
        i = end;
    }
    if !spans.is_empty() || rows.is_empty() {
        rows.push(Line::from(spans).style(line.style));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    fn strings(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }
    #[test]
    fn word_wrap_keeps_words_and_hanging_indent() {
        let rows = wrap_lines(vec![Line::raw("  • alpha beta gamma delta")], 14, false);
        assert_eq!(strings(&rows), ["  • alpha beta", "    gamma", "    delta"]);
        assert!(rows.iter().all(|r| r.width() <= 14));
        let rows = wrap_lines(vec![Line::raw("one two three four")], 9, false);
        assert_eq!(strings(&rows), ["one two", "three", "four"]);
    }
    #[test]
    fn wrap_long_tokens_display_cells_and_span_styles() {
        let accent = Theme::default().style(Role::Accent);
        let strong = Theme::default().style(Role::Strong);
        let rows = wrap_lines(
            vec![Line::from(vec![
                Span::styled("abc", accent),
                Span::styled("defghijk", strong),
            ])],
            4,
            false,
        );
        assert_eq!(strings(&rows), ["abcd", "efgh", "ijk"]);
        assert_eq!(rows[0].spans[0].style, accent);
        assert_eq!(rows[0].spans[3].style, strong);
        let rows = wrap_lines(vec![Line::raw("界界 👩‍💻👩‍💻 e\u{301}lan")], 6, false);
        assert!(rows.iter().all(|r| r.width() <= 6), "{rows:?}");
        let all = strings(&rows).join("");
        assert!(all.contains("👩‍💻👩‍💻") && all.contains("e\u{301}lan"));
    }
    #[test]
    fn code_rows_hard_wrap_without_word_reflow() {
        let marked = |text: &str| {
            Line::from(vec![
                Span::raw(CODE_ROW),
                Span::raw("│ "),
                Span::raw(text.to_owned()),
            ])
        };
        let rows = wrap_lines(
            vec![
                Line::raw("rust"),
                marked("alpha beta gamma"),
                marked("delta"),
            ],
            10,
            false,
        );
        let lines = strings(&rows);
        assert!(lines.contains(&"│ alpha b↪".to_owned()), "{lines:?}");
        assert!(lines.contains(&"eta gamma".to_owned()), "{lines:?}");
        assert!(rows.iter().all(|r| r.width() <= 10));
        // Without the marker the same text reflows by words.
        let plain = wrap_lines(vec![Line::raw("│ alpha beta gamma")], 10, false);
        assert!(strings(&plain).contains(&"│ alpha".to_owned()), "{plain:?}");
        // ASCII mode uses an ASCII continuation glyph and rule.
        let rows = wrap_lines(
            vec![
                marked("alpha beta gamma"),
                Line::styled(RULE_ROW, Style::new()),
            ],
            10,
            true,
        );
        assert!(strings(&rows).contains(&"│ alpha b>".to_owned()));
        assert_eq!(strings(&rows).last().unwrap(), &"-".repeat(10));
    }
}

#[cfg(test)]
mod modal_tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    #[test]
    fn modal_content_is_sized_bounded_and_scrolls_to_the_last_line() {
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let rows: Vec<_> = (0..40)
            .map(|n| Line::raw(format!("content row {n:02}")))
            .collect();
        let mut scroll = 0;
        let mut inner = Rect::default();
        terminal
            .draw(|f| {
                inner = draw_modal(
                    f,
                    Theme::default(),
                    "title",
                    "↑↓ / PgUp/PgDn",
                    rows.clone(),
                    None,
                    &mut scroll,
                );
            })
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            screen.contains("more ↓") && screen.contains("content row 00"),
            "{screen}"
        );
        assert!(inner.height <= 8 && inner.width <= 36);
        scroll = usize::MAX;
        terminal
            .draw(|f| {
                draw_modal(
                    f,
                    Theme::default(),
                    "title",
                    "↑↓ / PgUp/PgDn",
                    rows.clone(),
                    None,
                    &mut scroll,
                );
            })
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            screen.contains("content row 39") && screen.contains("more ↑"),
            "{screen}"
        );
        assert!(!screen.contains("more ↓"));
    }
}
