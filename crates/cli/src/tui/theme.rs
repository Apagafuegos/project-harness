//! One terminal-default-friendly ANSI theme. Environment detection belongs to the shell.
use ratatui::{
    style::{Color, Modifier, Style},
    text::Line,
};
use std::borrow::Cow;

/// Semantic roles shared by transcript, chrome and dialogs.
#[derive(Clone, Copy, Debug)]
pub(super) enum Role {
    User,
    Assistant,
    Tool,
    Dim,
    Accent,
    Warning,
    Error,
    Status,
    Selection,
    Code,
    Emphasis,
    Strong,
    Link,
    InlineCode,
    /// Code with no highlighting rule: the terminal's own foreground.
    Plain,
    /// A highlighted keyword or storage class.
    CodeKeyword,
    /// A highlighted string or character literal.
    CodeString,
    /// A highlighted number, constant or builtin variable.
    CodeNumber,
    /// A highlighted callable name.
    CodeFunction,
    /// A highlighted type, class or tag name.
    CodeType,
}

/// The ANSI colours used by the highlighter. Every one is a named 16-colour
/// variant; no RGB value is ever handed to Ratatui.
pub(super) const CODE_PALETTE: &[(u8, u8, u8, Role)] = &[
    (205, 49, 49, Role::Error),
    (13, 188, 121, Role::CodeString),
    (229, 229, 16, Role::CodeType),
    (36, 114, 200, Role::CodeFunction),
    (188, 63, 188, Role::CodeKeyword),
    (17, 168, 205, Role::CodeNumber),
];

/// Computed once by the shell and injected into the pure model.
#[derive(Clone, Copy, Debug, Default)]
pub struct Theme {
    no_color: bool,
    pub(super) ascii: bool,
}
impl Theme {
    /// Pure construction, including an explicit ASCII fallback.
    pub fn new(no_color: bool, ascii: bool) -> Self {
        Self { no_color, ascii }
    }
    /// `NO_COLOR` disables colours for any nonempty value. `TERM=dumb` uses ASCII activity.
    pub fn from_environment() -> Self {
        Self::new(
            std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
            std::env::var_os("TERM").is_some_and(|v| v == "dumb"),
        )
    }
    /// False when `NO_COLOR` stripped colour; syntax highlighting is off then.
    pub(super) fn colours(self) -> bool {
        !self.no_color
    }
    pub(super) fn style(self, role: Role) -> Style {
        let (color, modifier) = match role {
            Role::User => (Some(Color::Cyan), Modifier::BOLD),
            Role::Assistant => (Some(Color::Green), Modifier::BOLD),
            Role::Tool => (Some(Color::Yellow), Modifier::BOLD),
            Role::Dim => (None, Modifier::DIM),
            Role::Accent => (Some(Color::Magenta), Modifier::BOLD),
            Role::Warning => (Some(Color::Yellow), Modifier::BOLD),
            Role::Error => (Some(Color::Red), Modifier::BOLD),
            Role::Status => (None, Modifier::REVERSED),
            Role::Selection => (None, Modifier::REVERSED | Modifier::BOLD),
            Role::Code => (Some(Color::Green), Modifier::empty()),
            Role::Emphasis => (None, Modifier::ITALIC),
            Role::Strong => (None, Modifier::BOLD),
            Role::Link => (None, Modifier::UNDERLINED),
            Role::InlineCode => (Some(Color::Yellow), Modifier::DIM),
            Role::Plain => (None, Modifier::empty()),
            Role::CodeKeyword => (Some(Color::Magenta), Modifier::empty()),
            Role::CodeString => (Some(Color::Green), Modifier::empty()),
            Role::CodeNumber => (Some(Color::Cyan), Modifier::empty()),
            Role::CodeFunction => (Some(Color::Blue), Modifier::empty()),
            Role::CodeType => (Some(Color::Yellow), Modifier::empty()),
        };
        let mut style = Style::new().add_modifier(modifier);
        if !self.no_color
            && let Some(color) = color
        {
            style = style.fg(color);
        }
        style
    }
    /// Chrome strings the projection does not own (titles, hints, status) get
    /// the same ASCII fallbacks so `TERM=dumb` never draws a wide glyph.
    pub(super) fn ascii_ready(self, text: impl Into<String>) -> String {
        let text = text.into();
        if self.ascii {
            return ascii_glyphs(&text).unwrap_or(text);
        }
        text
    }
    /// The width-independent projection uses semantic theme styles. Strip all
    /// colour channels at the App boundary without losing Markdown modifiers,
    /// and translate every decorative glyph when the theme is ASCII-only.
    pub(super) fn apply(self, rows: &mut [Line<'static>]) {
        for row in rows {
            if self.no_color {
                row.style.fg = None;
                row.style.bg = None;
            }
            for span in &mut row.spans {
                if self.no_color {
                    span.style.fg = None;
                    span.style.bg = None;
                }
                if self.ascii
                    && let Some(ascii) = ascii_glyphs(&span.content)
                {
                    span.content = Cow::Owned(ascii);
                }
            }
        }
    }
}

/// Canonical rows use single Unicode glyphs so the projection stays readable.
/// `TERM=dumb` maps each to an ASCII stand-in; two-cell entries come first so
/// the trailing space of a gutter marker is kept.
const ASCII_GLYPHS: &[(&str, &str)] = &[
    ("▌ ", "| "),
    ("│ ", "| "),
    ("• ", "- "),
    ("◦ ", "* "),
    ("▪ ", "+ "),
    ("…", "..."),
    ("✓", "ok"),
    ("✗", "x"),
    ("⊘", "-"),
    ("↪", ">"),
    ("▌", "|"),
    ("│", "|"),
    ("─", "-"),
    ("┼", "+"),
    ("●", "*"),
    ("↑", "^"),
    ("↓", "v"),
    ("·", "-"),
    ("–", "-"),
    ("—", "-"),
];

fn ascii_glyphs(text: &str) -> Option<String> {
    if text.is_ascii() {
        return None;
    }
    let mut out = text.to_owned();
    for (glyph, replacement) in ASCII_GLYPHS {
        if out.contains(glyph) {
            out = out.replace(glyph, replacement);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ascii_glyphs_replace_every_decorative_glyph() {
        let mut rows = vec![Line::raw("▌ ✓ read │ a ─ b • c ◦ d ▪ e … ↪ ⊘ ● ↑ ↓ · ┼")];
        Theme::new(false, true).apply(&mut rows);
        let text: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.is_ascii(), "{text}");
        assert_eq!(text, "| ok read | a - b - c * d + e ... > - * ^ v - +");
    }
}
