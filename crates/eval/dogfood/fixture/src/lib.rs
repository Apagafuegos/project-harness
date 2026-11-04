//! Stock notebook: insertion-ordered movements and deterministic summaries.
//!
//! This fixture has no external dependencies and no persistence or command line.
//! Quantities are small enough that overflow is outside this exercise's scope.
pub mod display;
pub mod ledger;

use ledger::Ledger;

/// A current, sorted summary of every item, retaining zero balances.
pub fn summary(ledger: &Ledger) -> String {
    display::render_balance(&ledger.balances())
}

/// A summary at the end of a day, using the ledger's inclusive cutoff API.
pub fn summary_through(ledger: &Ledger, day: u32) -> String {
    display::render_balance(&ledger.balance_through(day))
}

/// Create a ledger from one snapshot on the supplied day.
/// Parsing is all-or-nothing: malformed input returns its original line number.
pub fn from_snapshot(text: &str, day: u32) -> Result<Ledger, usize> {
    let balances = display::parse_balance(text)?;
    let mut ledger = Ledger::new();
    for (item, quantity) in balances {
        ledger.record(&item, day, quantity);
    }
    Ok(ledger)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_summary() {
        let mut ledger = Ledger::new();
        ledger.record("pear", 1, 3);
        ledger.record("apple", 2, 1);
        assert_eq!(summary(&ledger), "apple: 1\npear: 3\n");
    }

    #[test]
    fn empty_summary() {
        assert_eq!(summary(&Ledger::new()), "");
    }

    #[test]
    fn snapshot_uses_supplied_day() {
        let ledger = from_snapshot("pear: 3\napple: 1", 7).unwrap();
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger.movements()[0].item, "apple");
        assert!(ledger.movements().iter().all(|m| m.day == 7));
    }

    #[test]
    fn malformed_snapshot_is_rejected() {
        assert_eq!(from_snapshot("pear: 3\nbroken", 7), Err(2));
    }

    #[test]
    fn summary_before_any_movement() {
        let mut ledger = Ledger::new();
        ledger.record("pear", 7, 3);
        assert_eq!(summary_through(&ledger, 1), "");
    }
}
