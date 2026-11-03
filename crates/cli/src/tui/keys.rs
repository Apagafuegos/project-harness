//! The input allowlist is also the generated key reference. Handlers can only
//! receive a documented chord; context-sensitive meanings stay in their owners.
use super::theme::{Role, Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
use ratatui::text::Line;

struct Binding {
    area: &'static str,
    label: &'static str,
    description: &'static str,
    chords: &'static [(KeyCode, M)],
}
macro_rules! binding { ($area:literal, $label:literal, $description:literal, $(($code:expr,$mods:expr)),+ $(,)?) => { Binding {area:$area,label:$label,description:$description,chords:&[$(($code,$mods)),+] } }; }
use KeyCode::*;
const KEYS: &[Binding] = &[
    binding!(
        "Editing",
        "Enter",
        "send when idle; confirm dialog choice",
        (Enter, M::NONE)
    ),
    binding!(
        "Editing",
        "Shift-Enter / Alt-Enter",
        "insert newline",
        (Enter, M::SHIFT),
        (Enter, M::ALT)
    ),
    binding!(
        "Editing",
        "Left / Right",
        "move cursor; recovery choice",
        (Left, M::NONE),
        (Right, M::NONE)
    ),
    binding!(
        "Editing",
        "Home / End",
        "line start/end; empty End follows output",
        (Home, M::NONE),
        (End, M::NONE)
    ),
    binding!(
        "Editing",
        "Up / Down",
        "line movement; first/last line recalls history; dialog selection/scroll",
        (Up, M::NONE),
        (Down, M::NONE)
    ),
    binding!(
        "Editing",
        "Backspace / Delete",
        "delete character",
        (Backspace, M::NONE),
        (Delete, M::NONE)
    ),
    binding!(
        "Editing",
        "Ctrl-Left / Ctrl-Right",
        "move by word",
        (Left, M::CONTROL),
        (Right, M::CONTROL)
    ),
    binding!(
        "Editing",
        "Ctrl-W",
        "delete previous word",
        (Char('w'), M::CONTROL)
    ),
    binding!(
        "Editing",
        "Ctrl-U",
        "delete to start of line",
        (Char('u'), M::CONTROL)
    ),
    binding!(
        "Turn",
        "Esc",
        "cancel active turn; idle dismiss error; close dialog; approval DENIES",
        (Esc, M::NONE)
    ),
    binding!(
        "Turn",
        "Ctrl-C",
        "cancel; second press quits durably; idle asks",
        (Char('c'), M::CONTROL)
    ),
    binding!(
        "Turn",
        "Ctrl-D",
        "empty prompt quits durably (never deletes)",
        (Char('d'), M::CONTROL)
    ),
    binding!(
        "Navigation",
        "PgUp / PgDn",
        "scroll transcript or dialog",
        (PageUp, M::NONE),
        (PageDown, M::NONE)
    ),
    binding!(
        "Navigation",
        "Ctrl-Up / Ctrl-Down",
        "scroll transcript one line",
        (Up, M::CONTROL),
        (Down, M::CONTROL)
    ),
    binding!(
        "Navigation",
        "Ctrl-End",
        "return to follow mode, even with a draft",
        (End, M::CONTROL)
    ),
    binding!(
        "Navigation",
        "Ctrl-O",
        "sessions: Up/Down select, Enter open, n new",
        (Char('o'), M::CONTROL)
    ),
    binding!(
        "Navigation",
        "Ctrl-T",
        "branches: Enter select, e edit sibling, p pin, x exclude",
        (Char('t'), M::CONTROL)
    ),
    binding!(
        "Navigation",
        "Ctrl-L",
        "models: Enter tries Reject, o confirms optional omission",
        (Char('l'), M::CONTROL)
    ),
    binding!(
        "Context",
        "Ctrl-R",
        "toggle reasoning summaries",
        (Char('r'), M::CONTROL)
    ),
    binding!(
        "Context",
        "Ctrl-K",
        "inspector: m preview model, c preview compaction, Enter commit shown preview",
        (Char('k'), M::CONTROL)
    ),
    binding!(
        "Dialogs",
        "F1 / ?",
        "key reference (question mark only with empty prompt)",
        (F(1), M::NONE)
    ),
    binding!(
        "Dialogs",
        "Tab / Shift-Tab",
        "recovery: cycle resolution",
        (Tab, M::NONE),
        (BackTab, M::SHIFT),
        (BackTab, M::NONE)
    ),
    // Printable characters are a documented family, not an unlisted shortcut.
    Binding {
        area: "Dialogs",
        label: "Text / paste",
        description: "edit only; approval a allow, d deny, r remember; quit y/Enter or n/Esc; help Esc/F1 closes",
        chords: &[],
    },
];

pub(super) fn documented(key: KeyEvent) -> bool {
    let mods = key.modifiers - M::SHIFT;
    if matches!(key.code, Char(c) if !c.is_control()) && mods.is_empty() {
        return true;
    }
    KEYS.iter().any(|b| {
        b.chords.iter().any(|&(code, m)| {
            key.code == code && (key.modifiers == m || (m == M::NONE && key.modifiers == M::SHIFT))
        })
    })
}
pub(super) fn help(theme: Theme) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut previous = "";
    for b in KEYS {
        if b.area != previous {
            if !rows.is_empty() {
                rows.push(Line::raw(""));
            }
            rows.push(Line::styled(b.area, theme.style(Role::Accent)));
            previous = b.area;
        }
        rows.push(Line::styled(b.label, theme.style(Role::Strong)));
        rows.push(Line::raw(format!("  {}", b.description)));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn help_table_covers_every_handled_key() {
        // Independent contract: deleting any table row must fail, including
        // rows whose printable letters are also ordinary editor input.
        let labels = [
            "Enter",
            "Shift-Enter / Alt-Enter",
            "Left / Right",
            "Home / End",
            "Up / Down",
            "Backspace / Delete",
            "Ctrl-Left / Ctrl-Right",
            "Ctrl-W",
            "Ctrl-U",
            "Esc",
            "Ctrl-C",
            "Ctrl-D",
            "PgUp / PgDn",
            "Ctrl-Up / Ctrl-Down",
            "Ctrl-End",
            "Ctrl-O",
            "Ctrl-T",
            "Ctrl-L",
            "Ctrl-R",
            "Ctrl-K",
            "F1 / ?",
            "Tab / Shift-Tab",
            "Text / paste",
        ];
        for label in labels {
            assert!(
                KEYS.iter().any(|b| b.label == label),
                "undocumented key: {label}"
            );
        }
        for c in ['c', 'd', 'r', 'k', 'o', 't', 'l', 'u', 'w'] {
            assert!(documented(KeyEvent::new(Char(c), M::CONTROL)));
        }
    }
}
