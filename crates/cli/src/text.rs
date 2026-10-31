//! Single-line text helpers shared by the headless and terminal clients.
//!
//! Both clients print model- or tool-controlled text into a terminal. Control
//! characters are replaced so a value cannot move the cursor or emit an escape
//! sequence, and bounded values say when they were cut.

/// Replace every control character with a space, keeping one line.
pub(crate) fn line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// One line of at most `max` characters, with an ellipsis when cut.
pub(crate) fn bounded(text: &str, max: usize) -> String {
    let mut s: String = text
        .chars()
        .take(max)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if text.chars().count() > max {
        s.push('…');
    }
    s
}
