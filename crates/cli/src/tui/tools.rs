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
        "edit" => vec![Span::styled(
            bounded(string(arguments, "path").unwrap_or("?"), 120),
            plain,
        )],
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
    if raw || !matches!(name, "bash" | "edit" | "write" | "read") {
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
}
