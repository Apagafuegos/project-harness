//! Bounded, pure-Rust syntax highlighting mapped onto the theme's ANSI roles.
//!
//! Grammars are bundled in `syntect`'s default syntax set and the regex engine
//! is the pure-Rust `fancy-regex` backend, so nothing is fetched at build or run
//! time and no C library is linked (decision 10). The syntect theme carries
//! *probe* colours that are mapped back to [`Role`]s here: the only colours that
//! reach Ratatui are the theme's 16 ANSI variants, and `NO_COLOR` renders plain.

use std::sync::OnceLock;

use ratatui::text::{Line, Span};
use syntect::{
    easy::HighlightLines,
    highlighting::{
        Color as Probe, FontStyle, StyleModifier, Theme as SyntectTheme, ThemeItem, ThemeSettings,
    },
    parsing::SyntaxSet,
};

use super::theme::{CODE_PALETTE, Role, Theme};

/// Per-block caps: past either bound the block renders plain with one note.
pub(super) const MAX_LINES: usize = 2_000;
/// See [`MAX_LINES`].
pub(super) const MAX_BYTES: usize = 256 * 1024;

/// One highlighted code body, without its label or gutter.
pub(super) struct Block {
    pub(super) rows: Vec<Line<'static>>,
    pub(super) note: Option<String>,
}

/// Highlight `lines` as `language`; unknown or untagged languages stay plain.
pub(super) fn block(theme: Theme, language: &str, lines: &[String]) -> Block {
    let bytes: usize = lines.iter().map(String::len).sum();
    let capped = lines.len() > MAX_LINES || bytes > MAX_BYTES;
    if capped {
        let note = format!(
            "highlighting skipped: {} lines, {} KiB (cap {MAX_LINES} lines / {} KiB)",
            lines.len(),
            bytes / 1024,
            MAX_BYTES / 1024
        );
        return Block {
            rows: plain(theme, lines),
            note: Some(note),
        };
    }
    if !theme.colours() {
        return Block {
            rows: plain(theme, lines),
            note: None,
        };
    }
    let Some(syntax) = syntax(language) else {
        return Block {
            rows: plain(theme, lines),
            note: None,
        };
    };
    let theme_syntax = syntect_theme();
    let mut highlighter = HighlightLines::new(syntax, theme_syntax);
    let mut rows = Vec::with_capacity(lines.len());
    for line in lines {
        let mut source = line.clone();
        source.push('\n');
        let styled = highlighter.highlight_line(&source, syntaxes());
        let Ok(styled) = styled else {
            // A grammar that cannot parse this text is not worth a client error.
            return Block {
                rows: plain(theme, lines),
                note: None,
            };
        };
        let mut spans = Vec::new();
        for (style, text) in styled {
            let text = text.trim_end_matches('\n');
            if text.is_empty() {
                continue;
            }
            spans.push(Span::styled(
                text.to_owned(),
                theme.style(role(style.font_style, style.foreground)),
            ));
        }
        rows.push(Line::from(spans));
    }
    Block { rows, note: None }
}

fn plain(theme: Theme, lines: &[String]) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|line| Line::styled(line.clone(), theme.style(Role::Plain)))
        .collect()
}

fn role(font: FontStyle, foreground: Probe) -> Role {
    // Comments are the only rule expressed with an italic modifier.
    if font.contains(FontStyle::ITALIC) {
        return Role::Dim;
    }
    if font.contains(FontStyle::BOLD) {
        return Role::Strong;
    }
    let rgb = (foreground.r, foreground.g, foreground.b);
    CODE_PALETTE
        .iter()
        .find(|(r, g, b, _)| (*r, *g, *b) == rgb)
        .map(|(_, _, _, role)| *role)
        .unwrap_or(Role::Plain)
}

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Resolve a fence info string. A few common aliases are folded to the
/// extension the bundled grammar is registered under.
fn syntax(language: &str) -> Option<&'static syntect::parsing::SyntaxReference> {
    let language = language.trim().to_ascii_lowercase();
    let token = match language.as_str() {
        "" | "text" | "txt" | "plain" | "output" | "console" | "log" => return None,
        "bash" | "zsh" | "shell" | "sh" => "sh",
        "javascript" | "jsx" | "node" => "js",
        "typescript" | "tsx" => "ts",
        "python" | "py3" => "py",
        "c++" | "cc" | "hpp" => "cpp",
        "markdown" | "mdx" => "md",
        "yml" | "yaml" => "yml",
        "dockerfile" | "docker" => "Dockerfile",
        "makefile" | "make" => "Makefile",
        other => other,
    };
    syntaxes().find_syntax_by_token(token)
}

/// The probe theme. Each rule sets one of [`CODE_PALETTE`]'s colours, so
/// [`role`] can map a syntect style back to the semantic ANSI role.
fn syntect_theme() -> &'static SyntectTheme {
    static THEME: OnceLock<SyntectTheme> = OnceLock::new();
    THEME.get_or_init(|| SyntectTheme {
        name: Some("harness-ansi".into()),
        author: None,
        settings: ThemeSettings {
            foreground: Some(probe(Role::Plain)),
            ..ThemeSettings::default()
        },
        scopes: vec![
            item("comment", Role::Dim, Some(FontStyle::ITALIC)),
            item("string", Role::CodeString, None),
            item("constant", Role::CodeNumber, None),
            item("keyword", Role::CodeKeyword, None),
            item("storage", Role::CodeKeyword, None),
            item("entity.name.function", Role::CodeFunction, None),
            item("support.function", Role::CodeFunction, None),
            item("variable.function", Role::CodeFunction, None),
            item("entity.name.type", Role::CodeType, None),
            item("entity.name.class", Role::CodeType, None),
            item("entity.name.tag", Role::CodeType, None),
            item("support.type", Role::CodeType, None),
            item("support.class", Role::CodeType, None),
            item("invalid", Role::Error, None),
            item("markup.bold", Role::Strong, Some(FontStyle::BOLD)),
            item("markup.italic", Role::Emphasis, Some(FontStyle::ITALIC)),
            item("markup.heading", Role::CodeType, Some(FontStyle::BOLD)),
        ],
    })
}

fn item(selector: &str, role: Role, font: Option<FontStyle>) -> ThemeItem {
    ThemeItem {
        scope: selector
            .parse()
            .expect("the built-in highlight selectors are valid"),
        style: StyleModifier {
            foreground: Some(probe(role)),
            background: None,
            font_style: font,
        },
    }
}

fn probe(role: Role) -> Probe {
    CODE_PALETTE
        .iter()
        .find(|(_, _, _, candidate)| *candidate as u8 == role as u8)
        .map(|(r, g, b, _)| Probe {
            r: *r,
            g: *g,
            b: *b,
            a: 0xff,
        })
        .unwrap_or(Probe {
            r: 0,
            g: 0,
            b: 0,
            a: 0xff,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn rows(theme: Theme, language: &str, lines: &[&str]) -> Block {
        let lines: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
        block(theme, language, &lines)
    }

    #[test]
    fn rust_keywords_strings_and_comments_map_to_ansi_roles_only() {
        let block = rows(
            Theme::default(),
            "rust",
            &["// note", "fn main() {", "    let s = \"text\";", "}"],
        );
        let colors: Vec<Option<Color>> = block
            .rows
            .iter()
            .flat_map(|row| row.spans.iter().map(|span| span.style.fg))
            .collect();
        assert!(colors.contains(&Some(Color::Magenta)), "{colors:?}");
        assert!(colors.contains(&Some(Color::Green)), "{colors:?}");
        assert!(
            colors.iter().all(|color| matches!(
                color,
                None | Some(Color::Magenta)
                    | Some(Color::Green)
                    | Some(Color::Cyan)
                    | Some(Color::Blue)
                    | Some(Color::Yellow)
                    | Some(Color::Red)
            )),
            "{colors:?}"
        );
        let text: String = block.rows[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text, "// note");
        assert!(block.rows[0].spans[0].style.add_modifier == ratatui::style::Modifier::DIM);
    }

    #[test]
    fn untagged_unknown_and_no_color_blocks_are_plain() {
        let tagged = rows(Theme::default(), "", &["plain text"]);
        assert!(tagged.note.is_none());
        assert!(
            tagged.rows[0]
                .spans
                .iter()
                .all(|span| span.style.fg.is_none())
        );
        let unknown = rows(Theme::default(), "no-such-language", &["x"]);
        assert!(
            unknown.rows[0]
                .spans
                .iter()
                .all(|span| span.style.fg.is_none())
        );
        let colorless = rows(Theme::new(true, false), "rust", &["fn main() {}"]);
        assert!(
            colorless
                .rows
                .iter()
                .all(|row| row.spans.iter().all(|span| span.style.fg.is_none()))
        );
    }

    #[test]
    fn blocks_over_the_line_or_byte_cap_render_plain_with_one_note() {
        let long: Vec<String> = (0..=MAX_LINES).map(|n| format!("let x = {n};")).collect();
        let block = block(Theme::default(), "rust", &long);
        assert!(block.note.is_some());
        assert!(
            block
                .rows
                .iter()
                .all(|row| row.spans.iter().all(|span| span.style.fg.is_none()))
        );
        let wide = vec!["x".repeat(MAX_BYTES + 1)];
        assert!(block_bytes(Theme::default(), "rust", &wide).note.is_some());
    }

    fn block_bytes(theme: Theme, language: &str, lines: &[String]) -> Block {
        block(theme, language, lines)
    }
}
