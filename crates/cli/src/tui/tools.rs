//! Compact tool-call blocks and human-readable approval previews.
//!
//! The client only formats what the view already carries: a call's name and
//! arguments from the assistant response, and the result's status, code and
//! metadata. Nothing here authorizes, plans or executes anything.

use std::path::Path;

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use serde_json::{Map, Value};

use super::{
    render::CODE_ROW,
    theme::{Role, Theme},
};
use crate::text::{bounded, line};

/// Lines a transcript diff may occupy before it is summarised.
pub(super) const DIFF_LINES: usize = 12;
/// Lines of a write preview shown in the approval modal.
pub(super) const WRITE_PREVIEW_LINES: usize = 10;

/// The status marks one call header can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mark {
    /// A call whose result has not arrived yet.
    Running,
    /// A result the tool reported as successful.
    Ok,
    /// A failed, invalid, timed-out or infrastructure result.
    Failed,
    /// A call the user denied or the turn cancelled.
    Blocked,
    /// A result whose real outcome is unknown.
    Uncertain,
}

impl Mark {
    pub(super) fn of(status: Option<harness_core::session::ToolStatus>) -> Self {
        use harness_core::session::ToolStatus;
        match status {
            None => Self::Running,
            Some(ToolStatus::Succeeded) => Self::Ok,
            Some(ToolStatus::Failed | ToolStatus::InvalidArguments) => Self::Failed,
            Some(ToolStatus::TimedOut | ToolStatus::InfrastructureFailure) => Self::Failed,
            Some(ToolStatus::Denied | ToolStatus::Cancelled) => Self::Blocked,
            Some(ToolStatus::Uncertain) => Self::Uncertain,
        }
    }

    pub(super) fn glyph(self) -> &'static str {
        match self {
            Self::Running => "…",
            Self::Ok => "✓",
            Self::Failed => "✗",
            Self::Blocked => "⊘",
            Self::Uncertain => "?",
        }
    }

    pub(super) fn style(self) -> Style {
        let theme = Theme::default();
        match self {
            Self::Running => theme.style(Role::Dim),
            Self::Ok => theme.style(Role::Tool),
            Self::Failed => theme.style(Role::Error),
            Self::Blocked => theme.style(Role::Warning),
            Self::Uncertain => theme.style(Role::Warning),
        }
    }
}

/// A streaming call: its arguments may still be incomplete JSON.
pub(super) fn partial(name: &str, arguments: &str, ready: bool) -> Vec<Line<'static>> {
    match serde_json::from_str::<Map<String, Value>>(arguments) {
        Ok(map) if !map.is_empty() => {
            let mut rows = call(name, &map, Facts::default());
            if !ready {
                rows[0]
                    .spans
                    .push(Span::styled(" …", Theme::default().style(Role::Dim)));
            }
            rows
        }
        _ => {
            let theme = Theme::default();
            let mut spans = vec![
                Span::styled(Mark::Running.glyph(), Mark::Running.style()),
                Span::raw(" "),
                Span::styled(bounded(name, 40), theme.style(Role::Strong)),
                Span::raw(" "),
                Span::styled(bounded(&line(arguments), 200), theme.style(Role::Dim)),
            ];
            if ready {
                spans.push(Span::raw(" …"));
            }
            vec![Line::from(spans)]
        }
    }
}

/// The status, code and exit facts a header must always show.
#[derive(Clone, Copy, Default)]
pub(super) struct Facts<'a> {
    pub(super) status: Option<harness_core::session::ToolStatus>,
    pub(super) code: Option<&'a str>,
    pub(super) exit_code: Option<i32>,
    pub(super) search: Option<(u64, bool)>,
}

/// One compact call header, plus the diff an `edit` call carries.
pub(super) fn call(
    name: &str,
    arguments: &Map<String, Value>,
    facts: Facts<'_>,
) -> Vec<Line<'static>> {
    let mark = Mark::of(facts.status);
    let theme = Theme::default();
    let mut spans = vec![
        Span::styled(mark.glyph(), mark.style()),
        Span::raw(" "),
        Span::styled(bounded(name, 40), theme.style(Role::Strong)),
        Span::raw(" "),
    ];
    spans.extend(summary(name, arguments));
    if let Some((count, truncated)) = facts.search {
        spans.push(Span::styled(
            format!(
                " ({count} results{})",
                if truncated { "; truncated" } else { "" }
            ),
            theme.style(Role::Dim),
        ));
    }
    let suffix = match (facts.exit_code, facts.code) {
        (Some(exit), _) => Some(format!(" (exit {exit})")),
        (None, Some(code)) => Some(format!(" ({})", bounded(code, 60))),
        (None, None) => None,
    };
    if let Some(suffix) = suffix {
        spans.push(Span::styled(suffix, mark.style()));
    }
    let mut rows = vec![Line::from(spans)];
    if name == "edit" {
        rows.extend(diff(
            string(arguments, "old_text").unwrap_or_default(),
            string(arguments, "new_text").unwrap_or_default(),
            DIFF_LINES,
        ));
    } else if name == "multi_edit" {
        rows.extend(multi_edit_preview(arguments));
    }
    rows
}

/// Per-tool argument summary: the part a human needs, and nothing else.
fn summary(name: &str, arguments: &Map<String, Value>) -> Vec<Span<'static>> {
    let theme = Theme::default();
    let code = theme.style(Role::InlineCode);
    let dim = theme.style(Role::Dim);
    let plain = Style::new();
    match name {
        "bash" => vec![Span::styled(
            line(&bounded(
                string(arguments, "command").unwrap_or_default(),
                400,
            )),
            code,
        )],
        "read" => {
            let mut spans = vec![Span::styled(
                bounded(string(arguments, "path").unwrap_or("?"), 120),
                plain,
            )];
            let offset = number(arguments, "offset_bytes").unwrap_or(0);
            let max = number(arguments, "max_bytes").unwrap_or(1024 * 1024);
            spans.push(Span::styled(
                format!(" (bytes {offset}-{})", offset.saturating_add(max)),
                dim,
            ));
            spans
        }
        "write" => {
            let bytes = string(arguments, "text").map_or(0, str::len);
            vec![
                Span::styled(
                    bounded(string(arguments, "path").unwrap_or("?"), 120),
                    plain,
                ),
                Span::styled(format!(" ({bytes} B)"), dim),
            ]
        }
        "list" => vec![Span::raw(bounded(
            string(arguments, "path").unwrap_or("."),
            120,
        ))],
        "glob" => vec![Span::styled(
            line(&bounded(string(arguments, "pattern").unwrap_or("?"), 200)),
            code,
        )],
        "grep" => vec![Span::styled(
            format!(
                "/{}/ {}",
                line(&bounded(string(arguments, "pattern").unwrap_or("?"), 200)),
                line(&bounded(string(arguments, "path").unwrap_or("."), 120))
            ),
            code,
        )],
        "edit" => vec![Span::styled(
            bounded(string(arguments, "path").unwrap_or("?"), 120),
            plain,
        )],
        "multi_edit" => {
            let edits = arguments
                .get("edits")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let paths = arguments
                .get("edits")
                .and_then(Value::as_array)
                .map(|edits| {
                    edits
                        .iter()
                        .filter_map(|edit| edit.get("path").and_then(Value::as_str))
                        .map(display_path)
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                })
                .unwrap_or(0);
            vec![Span::styled(
                format!("{edits} replacements across {paths} files"),
                plain,
            )]
        }
        _ => vec![Span::styled(code_style_json(arguments), dim)],
    }
}

fn code_style_json(arguments: &Map<String, Value>) -> String {
    bounded(&Value::Object(arguments.clone()).to_string(), 200)
}

/// A `-`/`+` diff of the two strings, bounded to `budget` rows plus a count.
pub(super) fn diff(old: &str, new: &str, budget: usize) -> Vec<Line<'static>> {
    let theme = Theme::default();
    let mut rows: Vec<(char, Style, String)> = Vec::new();
    // A text that ends with a newline has no extra empty last line.
    for line in old.strip_suffix('\n').unwrap_or(old).split('\n') {
        rows.push(('-', theme.style(Role::Error), line.to_owned()));
    }
    for line in new.strip_suffix('\n').unwrap_or(new).split('\n') {
        rows.push(('+', theme.style(Role::Code), line.to_owned()));
    }
    let total = rows.len();
    let shown = rows.len().min(budget);
    let mut out = Vec::with_capacity(shown + 1);
    for (sign, style, text) in rows.drain(..shown) {
        out.push(Line::from(vec![
            Span::raw(CODE_ROW),
            Span::styled(format!("{sign} "), style),
            Span::styled(line(&text), style),
        ]));
    }
    if total > shown {
        out.push(Line::styled(
            format!("… +{} lines", total - shown),
            theme.style(Role::Dim),
        ));
    }
    out
}

/// The approval modal body: what the user is actually being asked to allow.
pub(super) fn approval(
    workspace: &Path,
    name: &str,
    arguments: &Map<String, Value>,
    raw: bool,
) -> Vec<Line<'static>> {
    let theme = Theme::default();
    let dim = theme.style(Role::Dim);
    let label = theme.style(Role::Strong);
    let code = theme.style(Role::InlineCode);
    if !raw && matches!(name, "list" | "glob" | "grep") {
        let defaults = match name {
            "list" => serde_json::json!({"path":"."}),
            "glob" => serde_json::json!({"path":".","pattern":"","include_ignored":false}),
            _ => {
                serde_json::json!({"pattern":"","path":".","glob":null,"case_insensitive":false,"output":"matches","context_lines":0,"max_results":200,"include_ignored":false})
            }
        };
        let mut values = defaults.as_object().expect("defaults").clone();
        values.extend(arguments.clone());
        let mut rows = vec![Line::styled("read-only", dim)];
        for (key, value) in values {
            rows.push(Line::from(vec![
                Span::styled(format!("{key}: "), label),
                Span::raw(line(&value.to_string())),
            ]));
        }
        return rows;
    }
    if raw || !matches!(name, "bash" | "edit" | "multi_edit" | "write" | "read") {
        let mut rows = Vec::new();
        if !matches!(name, "bash" | "edit" | "write" | "read") {
            rows.push(Line::from(vec![
                Span::styled("tool ", label),
                Span::raw(bounded(name, 60)),
            ]));
        }
        let pretty = serde_json::to_string_pretty(&Value::Object(arguments.clone()))
            .unwrap_or_else(|_| "{}".into());
        for text in pretty.lines() {
            rows.push(Line::raw(line(text)));
        }
        return rows;
    }
    match name {
        "bash" => {
            let cwd = string(arguments, "cwd").unwrap_or(".");
            let mut rows = vec![Line::from(vec![
                Span::styled("command", label),
                Span::raw(":"),
            ])];
            for text in string(arguments, "command").unwrap_or_default().split('\n') {
                rows.push(Line::from(vec![
                    Span::raw(CODE_ROW),
                    Span::styled("│ ", dim),
                    Span::styled(line(text), code),
                ]));
            }
            let directory = match cwd {
                "." => workspace.display().to_string(),
                other => workspace.join(other).display().to_string(),
            };
            rows.push(Line::raw(format!("cwd: {directory}")));
            let timeout = number(arguments, "timeout_ms").unwrap_or(120_000);
            rows.push(Line::styled(format!("timeout: {}s", timeout / 1000), dim));
            rows.push(Line::styled("network: denied", dim));
            rows
        }
        "edit" => {
            let mut rows = vec![Line::from(vec![
                Span::styled("path ", label),
                Span::raw(bounded(string(arguments, "path").unwrap_or("?"), 120)),
            ])];
            rows.extend(diff(
                string(arguments, "old_text").unwrap_or_default(),
                string(arguments, "new_text").unwrap_or_default(),
                usize::MAX,
            ));
            rows
        }
        "multi_edit" => multi_edit_preview(arguments),
        "write" => {
            let text = string(arguments, "text").unwrap_or_default();
            let mut rows = vec![
                Line::from(vec![
                    Span::styled("path ", label),
                    Span::raw(bounded(string(arguments, "path").unwrap_or("?"), 120)),
                ]),
                Line::from(vec![
                    Span::styled("size ", label),
                    Span::raw(format!("{} B", text.len())),
                    Span::styled(
                        if boolean(arguments, "overwrite") {
                            " · overwrite"
                        } else {
                            " · new file"
                        },
                        dim,
                    ),
                ]),
            ];
            for text in text.lines().take(WRITE_PREVIEW_LINES) {
                rows.push(Line::from(vec![
                    Span::raw(CODE_ROW),
                    Span::styled("│ ", dim),
                    Span::styled(line(text), code),
                ]));
            }
            if text.lines().count() > WRITE_PREVIEW_LINES {
                rows.push(Line::styled(
                    format!("… +{} lines", text.lines().count() - WRITE_PREVIEW_LINES),
                    dim,
                ));
            }
            rows
        }
        _ => {
            let offset = number(arguments, "offset_bytes").unwrap_or(0);
            let max = number(arguments, "max_bytes").unwrap_or(1024 * 1024);
            vec![
                Line::from(vec![
                    Span::styled("path ", label),
                    Span::raw(bounded(string(arguments, "path").unwrap_or("?"), 120)),
                ]),
                Line::from(vec![
                    Span::styled("range ", label),
                    Span::raw(format!("bytes {offset}-{}", offset.saturating_add(max))),
                ]),
                Line::styled("read-only", dim),
            ]
        }
    }
}

// Display grouping only; validation and authorization remain in the engine.
fn display_path(path: &str) -> String {
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|part| part == "..") {
        return path.to_owned();
    }
    let path = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/");
    if path.is_empty() { ".".into() } else { path }
}

fn multi_edit_preview(arguments: &Map<String, Value>) -> Vec<Line<'static>> {
    let edits = arguments
        .get("edits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut grouped = std::collections::BTreeMap::<String, Vec<(String, String)>>::new();
    for edit in edits {
        let Some(path) = edit.get("path").and_then(Value::as_str) else {
            continue;
        };
        let old = edit
            .get("old_text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let new = edit
            .get("new_text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        grouped
            .entry(display_path(path))
            .or_default()
            .push((old.to_owned(), new.to_owned()));
    }
    let theme = Theme::default();
    let label = theme.style(Role::Strong);
    let mut rows = vec![Line::styled(
        format!("{} files · exact replacements", grouped.len()),
        theme.style(Role::Dim),
    )];
    for (path, replacements) in grouped {
        rows.push(Line::from(vec![
            Span::styled("path ", label),
            Span::raw(bounded(&path, 120)),
        ]));
        for (index, (old, new)) in replacements.iter().enumerate() {
            rows.push(Line::styled(
                format!("replacement {} of {}", index + 1, replacements.len()),
                theme.style(Role::Dim),
            ));
            rows.extend(diff(old, new, usize::MAX));
        }
    }
    rows
}

/// Toolchain scope is taken from the exact plan, not client configuration.
pub(super) fn sandbox_summary(plan: &[u8]) -> Vec<Line<'static>> {
    crate::wiring::sandbox_summary(plan)
        .into_iter()
        .map(note)
        .collect()
}

/// The plan digest, always visible and dimmed.
pub(super) fn digest(digest: &str) -> Line<'static> {
    Line::styled(
        format!("plan {}", bounded(digest, 12)),
        Theme::default().style(Role::Dim),
    )
}

fn string<'a>(arguments: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(Value::as_str)
}

fn number(arguments: &Map<String, Value>, key: &str) -> Option<u64> {
    arguments.get(key).and_then(Value::as_u64)
}

fn boolean(arguments: &Map<String, Value>, key: &str) -> bool {
    arguments.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Search metadata comes only from the recorded model-facing result.
pub(super) fn search_facts(text: &str) -> Option<(u64, bool)> {
    let value: Value = serde_json::from_str(text).ok()?;
    Some((value["count"].as_u64()?, value["truncated"].as_bool()?))
}

/// Decode search JSON for display only; malformed/non-search text stays literal.
pub(super) fn result_lines(name: &str, text: &str) -> Vec<String> {
    if !matches!(name, "list" | "glob" | "grep") {
        return text.lines().map(str::to_owned).collect();
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return text.lines().map(str::to_owned).collect();
    };
    let mut rows = Vec::new();
    if let Some(entries) = value["entries"].as_array() {
        for entry in entries {
            let name = entry["name"].as_str().unwrap_or("?");
            rows.push(match entry["type"].as_str() {
                Some("file") => format!("file {name} ({} B)", entry["size"]),
                Some("dir") => format!("dir {name}/"),
                _ => format!("symlink {name} [not followed]"),
            });
        }
    } else if let Some(paths) = value["paths"].as_array() {
        rows.extend(paths.iter().filter_map(Value::as_str).map(str::to_owned));
    } else if let Some(matches) = value["matches"].as_array() {
        for found in matches {
            let path = found["path"].as_str().unwrap_or("?");
            for context in found["before"].as_array().into_iter().flatten() {
                rows.push(format!(
                    "{path}-{}- {}",
                    context["line"],
                    context["text"].as_str().unwrap_or("")
                ));
            }
            rows.push(format!(
                "{path}:{}: {}",
                found["line"],
                found["text"].as_str().unwrap_or("")
            ));
            for context in found["after"].as_array().into_iter().flatten() {
                rows.push(format!(
                    "{path}-{}- {}",
                    context["line"],
                    context["text"].as_str().unwrap_or("")
                ));
            }
        }
    } else {
        return text.lines().map(str::to_owned).collect();
    }
    if rows.is_empty() {
        rows.push("no results".into());
    }
    if let Some(skipped) = value["skipped"].as_object() {
        for (reason, count) in skipped {
            if count.as_u64().is_some_and(|count| count > 0) {
                rows.push(format!("skipped {reason}: {count}"));
            }
        }
    }
    for limit in value["limits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        rows.push(format!("limit: {limit}"));
    }
    for warning in value["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        rows.push(format!("warning: {warning}"));
    }
    if let Some(omitted) = value["inline_omitted"].as_u64() {
        rows.push(format!(
            "{omitted} results omitted inline; full output in artifact"
        ));
    }
    rows
}

/// A code block's dim gutter, for tool output rows.
pub(super) fn output_gutter() -> Span<'static> {
    Span::styled("│ ", Theme::default().style(Role::Dim))
}

/// One row of tool output, hard-wrapped by the renderer.
pub(super) fn output_row(text: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw(CODE_ROW),
        output_gutter(),
        Span::styled(line(text), Theme::default().style(Role::Plain)),
    ])
}

/// Dim rows that never reflow: notes and counts under a call.
pub(super) fn note(text: impl Into<String>) -> Line<'static> {
    Line::styled(
        text.into(),
        Theme::default()
            .style(Role::Dim)
            .add_modifier(Modifier::DIM),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::session::ToolStatus;

    fn arguments(value: serde_json::Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => panic!("object arguments"),
        }
    }

    fn text(rows: &[Line<'static>]) -> String {
        rows.iter()
            .map(|row| {
                row.spans
                    .iter()
                    .filter(|span| !span.content.starts_with('\u{0}'))
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_tool_status_has_a_header_mark_and_keeps_failure_facts() {
        for (status, glyph) in [
            (ToolStatus::Succeeded, "✓"),
            (ToolStatus::Failed, "✗"),
            (ToolStatus::InvalidArguments, "✗"),
            (ToolStatus::Denied, "⊘"),
            (ToolStatus::TimedOut, "✗"),
            (ToolStatus::Cancelled, "⊘"),
            (ToolStatus::InfrastructureFailure, "✗"),
            (ToolStatus::Uncertain, "?"),
        ] {
            let rows = call(
                "read",
                &arguments(serde_json::json!({"path": "a.txt", "max_bytes": 10})),
                Facts {
                    status: Some(status),
                    code: Some("tools_unavailable"),
                    exit_code: None,
                    search: None,
                },
            );
            let header = text(&rows);
            assert!(header.starts_with(glyph), "{status:?}: {header}");
            assert!(header.contains("read a.txt (bytes 0-10)"), "{header}");
            assert!(header.contains("(tools_unavailable)"), "{header}");
        }
        // A call with no result yet is running.
        let running = text(&call(
            "write",
            &arguments(serde_json::json!({"path": "notes.md", "text": "abc"})),
            Facts::default(),
        ));
        assert!(running.starts_with("… write notes.md (3 B)"), "{running}");
        // An exit code wins over the machine code, and bash shows its command.
        let failed = text(&call(
            "bash",
            &arguments(serde_json::json!({"command": "cargo test"})),
            Facts {
                status: Some(ToolStatus::Failed),
                code: Some("nonzero_exit"),
                search: None,
                exit_code: Some(101),
            },
        ));
        assert_eq!(failed, "✗ bash cargo test (exit 101)");
        // Unknown tools summarise their compact arguments.
        let unknown = text(&call(
            "web_search",
            &arguments(serde_json::json!({"q": "rust"})),
            Facts::default(),
        ));
        assert!(
            unknown.contains("web_search") && unknown.contains('q'),
            "{unknown}"
        );
    }

    #[test]
    fn search_blocks_at_80_columns_no_color_and_ascii() {
        use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};
        for (ascii, expected) in [
            (
                false,
                "✓ list . (2 results)\n│ file a.rs (3 B)\n│ dir src/\n✓ glob **/*.rs (1 results)\n│ a.rs\n✓ grep /hit/ . (1 results; truncated)\n│ a.rs:1: hit\n│ limit: max_results:matches",
            ),
            (
                true,
                "ok list . (2 results)\n| file a.rs (3 B)\n| dir src/\nok glob **/*.rs (1 results)\n| a.rs\nok grep /hit/ . (1 results; truncated)\n| a.rs:1: hit\n| limit: max_results:matches",
            ),
        ] {
            let mut rows = Vec::new();
            for (name, args, result) in [
                (
                    "list",
                    serde_json::json!({}),
                    serde_json::json!({"count":2,"truncated":false,"entries":[{"name":"a.rs","type":"file","size":3},{"name":"src","type":"dir"}]}),
                ),
                (
                    "glob",
                    serde_json::json!({"pattern":"**/*.rs"}),
                    serde_json::json!({"count":1,"truncated":false,"paths":["a.rs"]}),
                ),
                (
                    "grep",
                    serde_json::json!({"pattern":"hit"}),
                    serde_json::json!({"count":1,"truncated":true,"matches":[{"path":"a.rs","line":1,"text":"hit"}],"limits":["max_results:matches"]}),
                ),
            ] {
                let result = result.to_string();
                rows.extend(call(
                    name,
                    &arguments(args),
                    Facts {
                        status: Some(ToolStatus::Succeeded),
                        search: search_facts(&result),
                        ..Facts::default()
                    },
                ));
                rows.extend(
                    result_lines(name, &result)
                        .iter()
                        .map(|line| output_row(line)),
                );
            }
            let theme = Theme::new(true, ascii);
            theme.apply(&mut rows);
            let rows = super::super::render::wrap_lines(rows, 80, ascii);
            let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
            terminal
                .draw(|frame| frame.render_widget(Paragraph::new(rows), frame.area()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let actual = (0..8)
                .map(|y| {
                    (0..80)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(actual, expected);
            assert!(
                buffer
                    .content
                    .iter()
                    .all(|cell| cell.fg == ratatui::style::Color::Reset
                        && cell.bg == ratatui::style::Color::Reset)
            );
        }
    }

    #[test]
    fn search_approval_previews_show_every_default_and_argument() {
        for (name, args, fields) in [
            ("list", serde_json::json!({}), vec!["path: \".\""]),
            (
                "glob",
                serde_json::json!({"pattern":"*.rs","include_ignored":true}),
                vec!["pattern: \"*.rs\"", "path: \".\"", "include_ignored: true"],
            ),
            (
                "grep",
                serde_json::json!({"pattern":"hit","glob":"*.rs","context_lines":5,"max_results":12,"case_insensitive":true,"include_ignored":true,"output":"files","path":"src"}),
                vec![
                    "pattern: \"hit\"",
                    "path: \"src\"",
                    "glob: \"*.rs\"",
                    "context_lines: 5",
                    "max_results: 12",
                    "case_insensitive: true",
                    "include_ignored: true",
                    "output: \"files\"",
                ],
            ),
        ] {
            let preview = text(&approval(Path::new("/work"), name, &arguments(args), false));
            for field in fields {
                assert!(preview.contains(field), "{preview}: missing {field}");
            }
        }
        let preview = text(&approval(
            Path::new("/work"),
            "grep",
            &arguments(serde_json::json!({"pattern":"hit"})),
            false,
        ));
        for field in [
            "case_insensitive: false",
            "output: \"matches\"",
            "context_lines: 0",
            "max_results: 200",
            "include_ignored: false",
            "glob: null",
        ] {
            assert!(preview.contains(field), "{preview}");
        }
    }

    #[test]
    fn approval_preview_shows_the_bound_sandbox_scope() {
        let plan = serde_json::to_vec(&serde_json::json!({
            "name":"bash", "sandbox":{
                "toolchain_paths":["/opt/rust"], "path_prepend":["/opt/rust/bin"],
                "cargo_registry":"/opt/registry",
                "env":{"RUSTUP_TOOLCHAIN":"1.99.1"}
            }
        }))
        .unwrap();
        let rows = text(&sandbox_summary(&plan));
        assert!(rows.contains("toolchain (read-only): /opt/rust"), "{rows}");
        // Decision 10: every bound mount is in the preview, the registry too.
        assert_eq!(
            crate::wiring::sandbox_summary(&plan)[2],
            "cargo registry (read-only): /opt/registry"
        );
        assert!(
            rows.contains("sandbox PATH: /opt/rust/bin:/usr/bin:/bin"),
            "{rows}"
        );
        assert!(
            rows.contains("sandbox env keys: RUSTUP_TOOLCHAIN"),
            "{rows}"
        );
        assert!(!rows.contains("1.99.1"), "values are never rendered");
        assert!(sandbox_summary(b"{\"name\":\"read\"}").is_empty());
        assert_eq!(crate::wiring::sandbox_summary(b"{}"), Vec::<String>::new());
    }

    #[test]
    fn edit_diff_is_coloured_and_bounded() {
        let old: String = (0..10).map(|n| format!("old {n}\n")).collect();
        let new: String = (0..10).map(|n| format!("new {n}\n")).collect();
        let rows = diff(&old, &new, DIFF_LINES);
        assert_eq!(rows.len(), DIFF_LINES + 1);
        assert!(
            text(&rows).starts_with("- old 0\n- old 1"),
            "{}",
            text(&rows)
        );
        let last = rows.last().unwrap();
        assert_eq!(last.spans[0].content, "… +8 lines");
        assert_eq!(rows[..DIFF_LINES].len(), DIFF_LINES);
        let signs: Vec<char> = rows[..DIFF_LINES]
            .iter()
            .map(|row| row.spans[1].content.chars().next().unwrap())
            .collect();
        assert_eq!(signs.iter().filter(|sign| **sign == '-').count(), 10);
        assert_eq!(signs.iter().filter(|sign| **sign == '+').count(), 2);
    }

    #[test]
    fn approval_previews_are_human_readable_and_switch_to_raw() {
        let bash = approval(
            Path::new("/work"),
            "bash",
            &arguments(
                serde_json::json!({"command": "cargo test", "cwd": "crates", "timeout_ms": 5000}),
            ),
            false,
        );
        let readable = text(&bash);
        assert!(readable.contains("command:"), "{readable}");
        assert!(readable.contains("│ cargo test"), "{readable}");
        assert!(readable.contains("cwd: /work/crates"), "{readable}");
        assert!(readable.contains("timeout: 5s"), "{readable}");
        assert!(readable.contains("network: denied"), "{readable}");

        let edit = text(&approval(
            Path::new("/work"),
            "edit",
            &arguments(serde_json::json!({"path": "a.txt", "old_text": "one", "new_text": "two"})),
            false,
        ));
        assert!(edit.contains("path a.txt"), "{edit}");
        assert!(edit.contains("- one") && edit.contains("+ two"), "{edit}");

        let body: String = (0..11).map(|n| format!("line {n}\n")).collect();
        let write = text(&approval(
            Path::new("/work"),
            "write",
            &arguments(serde_json::json!({"path": "b.txt", "text": body})),
            false,
        ));
        assert!(write.contains("path b.txt"), "{write}");
        assert!(write.contains("size 78 B · new file"), "{write}");
        assert!(write.contains("│ line 0"), "{write}");
        assert!(write.contains("│ line 9"), "{write}");
        assert!(!write.contains("│ line 10"), "{write}");
        assert!(write.contains("… +1 lines"), "{write}");

        let read = text(&approval(
            Path::new("/work"),
            "read",
            &arguments(serde_json::json!({"path": "c.txt", "offset_bytes": 10, "max_bytes": 5})),
            false,
        ));
        assert!(read.contains("range bytes 10-15"), "{read}");

        let raw = text(&approval(
            Path::new("/work"),
            "bash",
            &arguments(serde_json::json!({"command": "cargo test"})),
            true,
        ));
        assert!(raw.contains("\"command\": \"cargo test\""), "{raw}");
        assert!(!raw.contains("network: denied"), "{raw}");
    }

    #[test]
    fn multi_edit_approval_shows_each_path_and_replacement() {
        let arguments = arguments(serde_json::json!({"edits":[
            {"path":"z.txt","old_text":"one","new_text":"two"},
            {"path":"a.txt","old_text":"left","new_text":"right"},
            {"path":"./z.txt","old_text":"two","new_text":"three"}
        ]}));
        let preview = text(&approval(
            Path::new("/work"),
            "multi_edit",
            &arguments,
            false,
        ));
        assert!(
            preview.contains("2 files · exact replacements"),
            "{preview}"
        );
        assert!(preview.find("path a.txt").unwrap() < preview.find("path z.txt").unwrap());
        assert!(preview.contains("- left\n+ right"), "{preview}");
        assert!(
            preview.contains("replacement 2 of 2\n- two\n+ three"),
            "{preview}"
        );
    }
}
