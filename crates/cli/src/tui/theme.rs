//! One terminal-default-friendly ANSI theme. Environment detection belongs to the shell.
use ratatui::{
    style::{Color, Modifier, Style},
    text::Line,
};

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
}

/// Computed once by the shell and injected into the pure model.
#[derive(Clone, Copy, Debug, Default)]
pub struct Theme {
    no_color: bool,
    pub(super) ascii: bool,
}
impl Theme {
    /// Pure construction, including an explicit ASCII spinner fallback.
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
        };
        let mut style = Style::new().add_modifier(modifier);
        if !self.no_color
            && let Some(color) = color
        {
            style = style.fg(color);
        }
        style
    }
    /// The width-independent projection uses semantic theme styles. Strip all
    /// colour channels at the App boundary without losing Markdown modifiers.
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
                if self.ascii && span.content == "▌ " {
                    span.content = "| ".into();
                }
            }
        }
    }
}
