//! GFM projection of assistant text into width-independent rows.
//!
//! Every block is a plain [`Line`]; the renderer wraps it to the pane, so the
//! same projection serves an 80-column terminal and a 200-column one. Code
//! rows carry [`CODE_ROW`], the renderer's no-reflow hint, and horizontal rules
//! carry [`RULE_ROW`], which the renderer expands to the pane width.

use std::collections::BTreeMap;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::{
    highlight,
    render::{CODE_ROW, RULE_ROW},
    theme::{Role, Theme},
};
use crate::text::line;

/// Parse assistant Markdown with the GFM extensions the brief requires.
pub(super) fn rows(text: &str) -> Vec<Line<'static>> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_GFM;
    let mut builder = Builder::default();
    for event in Parser::new_ext(text, options) {
        builder.event(event);
    }
    builder.finish()
}

/// User text is plain: newlines and leading whitespace are kept, nothing is
/// interpreted, and only fenced blocks get the code treatment.
pub(super) fn user_rows(text: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut fence: Option<(String, Vec<String>)> = None;
    for text in text.split('\n') {
        let trimmed = text.trim_start();
        match &mut fence {
            Some((_, body)) => {
                if trimmed.starts_with("```") {
                    let (language, body) = fence.take().expect("inside a fence");
                    out.extend(code_block(language, body));
                } else {
                    body.push(line(text));
                }
            }
            None if trimmed.starts_with("```") => {
                let language = trimmed.trim_start_matches('`').trim().to_owned();
                fence = Some((language, Vec::new()));
            }
            None => out.push(Line::raw(line(text))),
        }
    }
    if let Some((language, body)) = fence {
        // An unclosed fence is still code, exactly as while streaming.
        out.extend(code_block(language, body));
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

/// A dim language label, then the body behind a dim gutter.
fn code_block(language: String, mut body: Vec<String>) -> Vec<Line<'static>> {
    while body.last().is_some_and(|line| line.is_empty()) {
        body.pop();
    }
    let label = if language.is_empty() {
        "code".to_owned()
    } else {
        language.clone()
    };
    let mut out = vec![Line::styled(label, dim())];
    let block = highlight::block(Theme::default(), &language, &body);
    for row in block.rows {
        out.push(code_row(row));
    }
    if let Some(note) = block.note {
        out.push(Line::styled(note, dim()));
    }
    out
}

fn code_row(row: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw(CODE_ROW), Span::styled("│ ", dim())];
    spans.extend(row.spans);
    Line::from(spans).style(row.style)
}

#[derive(Default)]
struct Builder {
    out: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    emphasis: usize,
    strong: usize,
    strike: usize,
    heading: Option<u8>,
    quote: usize,
    lists: Vec<List>,
    /// Indent and marker for the item's first line, consumed by `flush`.
    item: Option<Vec<Span<'static>>>,
    task: Option<bool>,
    code: Option<Code>,
    link: Option<Link>,
    table: Option<Table>,
    image: Option<String>,
    definitions: Vec<(String, Vec<Line<'static>>)>,
    definition: Option<usize>,
    numbers: BTreeMap<String, usize>,
}

#[derive(Default)]
struct List {
    next: Option<u64>,
}

struct Code {
    language: String,
    body: Vec<String>,
}

struct Link {
    destination: String,
    text: String,
}

#[derive(Default)]
struct Table {
    alignments: Vec<Alignment>,
    header: Vec<Vec<Span<'static>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    current: Vec<Vec<Span<'static>>>,
    cell: Option<Vec<Span<'static>>>,
    in_header: bool,
}

impl Builder {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(text) => {
                if let Some(cell) = &mut self.table.as_mut().and_then(|t| t.cell.as_mut()) {
                    cell.push(Span::styled(line(&text), inline_code()));
                    return;
                }
                if let Some(image) = &mut self.image {
                    image.push_str(&text);
                    return;
                }
                self.push_to_link(&text);
                self.spans.push(Span::styled(line(&text), inline_code()));
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.flush(),
            Event::Rule => {
                self.flush();
                self.separate();
                self.out.push(Line::styled(RULE_ROW, dim()));
            }
            Event::TaskListMarker(done) => self.task = Some(done),
            // Raw HTML is inert text, never interpreted.
            Event::Html(text) | Event::InlineHtml(text) => self.text(&text),
            Event::FootnoteReference(label) => {
                let next = self.numbers.len() + 1;
                let number = *self.numbers.entry(label.to_string()).or_insert(next);
                self.push_styled(&format!("[^{number}]"), link_style());
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush();
                self.separate();
                self.heading = Some(level as u8);
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.separate();
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.separate();
                self.code = Some(Code {
                    language: match kind {
                        CodeBlockKind::Fenced(language) => language
                            .split_whitespace()
                            .next()
                            .unwrap_or_default()
                            .to_owned(),
                        CodeBlockKind::Indented => String::new(),
                    },
                    body: Vec::new(),
                });
            }
            Tag::List(start) => {
                self.flush();
                if self.lists.is_empty() {
                    self.separate();
                }
                self.lists.push(List { next: start });
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.lists.last_mut().and_then(|list| list.next.as_mut()) {
                    Some(next) => {
                        let marker = format!("{next}. ");
                        *next += 1;
                        marker
                    }
                    None => {
                        let bullets = ["• ", "◦ ", "▪ "];
                        bullets[depth % bullets.len()].to_owned()
                    }
                };
                self.item = Some(vec![Span::styled(format!("{indent}{marker}"), dim())]);
            }
            Tag::Emphasis => self.emphasis += 1,
            Tag::Strong => self.strong += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                self.link = Some(Link {
                    destination: dest_url.to_string(),
                    text: String::new(),
                });
            }
            Tag::Image { .. } => self.image = Some(String::new()),
            Tag::FootnoteDefinition(label) => self.start_definition(&label),
            Tag::Table(alignments) => {
                self.flush();
                self.separate();
                self.table = Some(Table {
                    alignments,
                    ..Table::default()
                });
            }
            Tag::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_header = true;
                }
            }
            Tag::TableRow => {}
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.cell = Some(Vec::new());
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.separate();
            }
            TagEnd::Heading(_) => {
                self.flush();
                self.heading = None;
                self.separate();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
                self.separate();
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    for row in code_block(code.language, code.body) {
                        self.out.push(row);
                    }
                    self.separate();
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.separate();
                }
            }
            TagEnd::Item => self.flush(),
            TagEnd::Emphasis => self.emphasis = self.emphasis.saturating_sub(1),
            TagEnd::Strong => self.strong = self.strong.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                if let Some(link) = self.link.take()
                    && !link.destination.is_empty()
                    && link.text != link.destination
                {
                    self.spans.push(Span::styled(
                        format!(" ({})", line(&link.destination)),
                        dim(),
                    ));
                }
            }
            TagEnd::Image => {
                let alt = self.image.take().unwrap_or_default();
                let text = if alt.is_empty() {
                    "[image]".to_owned()
                } else {
                    format!("[image: {}]", line(&alt))
                };
                self.spans.push(Span::styled(text, dim()));
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.in_header = false;
                    table.header = std::mem::take(&mut table.current);
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.current);
                    table.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table
                    && let Some(cell) = table.cell.take()
                {
                    table.current.push(cell);
                }
            }
            TagEnd::FootnoteDefinition => self.end_definition(),
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    for row in table_rows(&table) {
                        self.out.push(row);
                    }
                    self.separate();
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(code) = &mut self.code {
            code.body.extend(text.split('\n').map(line));
            return;
        }
        if let Some(image) = &mut self.image {
            image.push_str(&line(text));
            return;
        }
        let flat = line(text).replace('\n', " ");
        let style = self.style();
        self.push_to_link(&flat);
        self.push_styled(&flat, style);
    }

    fn push_to_link(&mut self, text: &str) {
        if let Some(link) = &mut self.link {
            link.text.push_str(text);
        }
    }

    fn push_styled(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        if let Some(cell) = &mut self.table.as_mut().and_then(|table| table.cell.as_mut()) {
            cell.push(Span::styled(text.to_owned(), style));
            return;
        }
        self.spans.push(Span::styled(text.to_owned(), style));
    }

    fn style(&self) -> Style {
        let theme = Theme::default();
        let mut style = Style::new();
        if let Some(level) = self.heading {
            style = style.patch(match level {
                1 => theme.style(Role::Accent).add_modifier(Modifier::UNDERLINED),
                2 => theme.style(Role::Strong),
                _ => theme.style(Role::Strong).add_modifier(Modifier::DIM),
            });
        }
        if self.emphasis > 0 {
            style = style.patch(theme.style(Role::Emphasis));
        }
        if self.strong > 0 {
            style = style.patch(theme.style(Role::Strong));
        }
        if self.strike > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.link.is_some() {
            style = style.patch(theme.style(Role::Link));
        }
        style
    }

    fn flush(&mut self) {
        if self.spans.is_empty() && self.item.is_none() && self.task.is_none() {
            return;
        }
        let mut spans = Vec::new();
        if self.quote > 0 {
            spans.push(Span::styled("│ ".repeat(self.quote), dim()));
        }
        if let Some(item) = self.item.take() {
            spans.extend(item);
        }
        if let Some(done) = self.task.take() {
            spans.push(Span::styled(if done { "[x] " } else { "[ ] " }, dim()));
        }
        spans.append(&mut self.spans);
        // A trailing space would look like a stray cell in the transcript.
        if let Some(last) = spans.last_mut() {
            let trimmed = last.content.trim_end().to_owned();
            last.content = trimmed.into();
        }
        while spans.last().is_some_and(|span| span.content.is_empty()) {
            spans.pop();
        }
        self.out.push(Line::from(spans));
    }

    /// One blank row between blocks; repeated calls add none.
    fn separate(&mut self) {
        if self.out.last().is_some_and(|line| line.width() > 0) {
            self.out.push(Line::default());
        }
    }

    fn start_definition(&mut self, label: &str) {
        self.flush();
        self.separate();
        self.definition = Some(self.out.len());
        let number = self.numbers.len() + 1;
        let number = *self.numbers.entry(label.to_owned()).or_insert(number);
        // The number is attached when the definition is drained.
        self.definitions.push((format!("[^{number}]"), Vec::new()));
    }

    fn end_definition(&mut self) {
        self.flush();
        if let Some(start) = self.definition.take()
            && let Some((_, rows)) = self.definitions.last_mut()
        {
            rows.extend(self.out.drain(start..));
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        if let Some(code) = self.code.take() {
            for row in code_block(code.language, code.body) {
                self.out.push(row);
            }
        }
        if let Some(table) = self.table.take() {
            for row in table_rows(&table) {
                self.out.push(row);
            }
        }
        let definitions = std::mem::take(&mut self.definitions);
        let defined = !definitions.is_empty();
        for (label, rows) in definitions {
            self.separate();
            let mut first = true;
            for mut row in rows {
                if row.width() == 0 {
                    continue;
                }
                if first {
                    row.spans
                        .insert(0, Span::styled(format!("{label} "), link_style()));
                    first = false;
                }
                self.out.push(row);
            }
        }
        if defined {
            self.separate();
        }
        while self.out.last().is_some_and(|line| line.width() == 0) {
            self.out.pop();
        }
        self.out
    }
}

fn table_rows(table: &Table) -> Vec<Line<'static>> {
    let columns = table
        .header
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return Vec::new();
    }
    let cell_width = |cell: &[Span<'static>]| -> usize {
        cell.iter()
            .map(|span| Span::raw(span.content.clone()).width())
            .sum()
    };
    let mut widths = vec![1usize; columns];
    for row in std::iter::once(&table.header).chain(table.rows.iter()) {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell_width(cell));
        }
    }
    let alignments: Vec<Alignment> = (0..columns)
        .map(|index| {
            table
                .alignments
                .get(index)
                .copied()
                .unwrap_or(Alignment::None)
        })
        .collect();
    let mut out = Vec::new();
    if !table.header.is_empty() {
        out.push(table_line(&table.header, &widths, &alignments, true));
        let separator = widths
            .iter()
            .map(|width| "─".repeat(*width))
            .collect::<Vec<_>>()
            .join("─┼─");
        out.push(Line::styled(separator, dim()));
    }
    for row in &table.rows {
        out.push(table_line(row, &widths, &alignments, false));
    }
    out
}

fn table_line(
    cells: &[Vec<Span<'static>>],
    widths: &[usize],
    alignments: &[Alignment],
    header: bool,
) -> Line<'static> {
    let theme = Theme::default();
    let mut spans = Vec::new();
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" │ ", dim()));
        }
        let cell = cells.get(index).map(Vec::as_slice).unwrap_or_default();
        let used: usize = cell
            .iter()
            .map(|span| Span::raw(span.content.clone()).width())
            .sum();
        let padding = width.saturating_sub(used);
        let (left, right) = match alignments[index] {
            Alignment::Right => (padding, 0),
            Alignment::Center => (padding / 2, padding - padding / 2),
            _ => (0, padding),
        };
        if left > 0 {
            spans.push(Span::raw(" ".repeat(left)));
        }
        for span in cell {
            let mut span = span.clone();
            if header {
                span.style = span.style.patch(theme.style(Role::Strong));
            }
            spans.push(span);
        }
        if right > 0 {
            spans.push(Span::raw(" ".repeat(right)));
        }
    }
    Line::from(spans)
}

fn inline_code() -> Style {
    Theme::default().style(Role::InlineCode)
}

fn link_style() -> Style {
    Theme::default().style(Role::Link)
}

fn dim() -> Style {
    Theme::default().style(Role::Dim)
}
