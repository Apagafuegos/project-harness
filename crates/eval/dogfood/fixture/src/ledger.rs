//! An append-only stock ledger. Days are integer business-day indices.
use std::collections::BTreeMap;

/// One stock movement; a negative quantity removes stock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Movement {
    pub item: String,
    pub day: u32,
    pub quantity: i64,
}

/// Stock movements retain insertion order, including zero quantities.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    movements: Vec<Movement>,
}

impl Ledger {
    /// Construct an empty notebook.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a movement without aggregating or sorting history.
    pub fn record(&mut self, item: &str, day: u32, quantity: i64) {
        self.movements.push(Movement {
            item: item.to_owned(),
            day,
            quantity,
        });
    }

    /// Inspect insertion-ordered history.
    pub fn movements(&self) -> &[Movement] {
        &self.movements
    }

    /// Number of recorded movements, not distinct item names.
    pub fn len(&self) -> usize {
        self.movements.len()
    }

    /// Whether history is empty.
    pub fn is_empty(&self) -> bool {
        self.movements.is_empty()
    }

    /// Current stock by item, sorted lexicographically, retaining zero stock.
    pub fn balances(&self) -> BTreeMap<String, i64> {
        let mut stock = BTreeMap::new();
        for movement in &self.movements {
            *stock.entry(movement.item.clone()).or_default() += movement.quantity;
        }
        stock
    }

    /// Stock at the end of `cutoff`, including movements on that day.
    pub fn balance_through(&self, cutoff: u32) -> BTreeMap<String, i64> {
        let mut stock = BTreeMap::new();
        for movement in &self.movements {
            if movement.day < cutoff {
                *stock.entry(movement.item.clone()).or_default() += movement.quantity;
            }
        }
        stock
    }

    /// Distinct item names in lexicographic order.
    pub fn items(&self) -> Vec<String> {
        self.balances().into_keys().collect()
    }

    /// Copy only the movements for a single item, preserving insertion order.
    pub fn select(&self, item: &str) -> Self {
        Self {
            movements: self
                .movements
                .iter()
                .filter(|m| m.item == item)
                .cloned()
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_history() {
        let ledger = Ledger::new();
        assert!(ledger.is_empty());
        assert_eq!(ledger.len(), 0);
        assert!(ledger.balances().is_empty());
    }

    #[test]
    fn insertion_order_is_history_order() {
        let mut ledger = Ledger::new();
        ledger.record("pear", 9, 2);
        ledger.record("apple", 1, 4);
        assert_eq!(ledger.movements()[0].day, 9);
        assert_eq!(ledger.movements()[1].day, 1);
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn balances_are_sorted_and_keep_zero() {
        let mut ledger = Ledger::new();
        ledger.record("pear", 1, 2);
        ledger.record("apple", 1, 4);
        ledger.record("pear", 2, -2);
        assert_eq!(ledger.items(), vec!["apple", "pear"]);
        assert_eq!(ledger.balances()["pear"], 0);
        assert_eq!(ledger.balances()["apple"], 4);
    }

    #[test]
    fn balance_includes_cutoff() {
        let mut ledger = Ledger::new();
        ledger.record("pear", 1, 2);
        ledger.record("pear", 2, 3);
        ledger.record("pear", 3, 100);
        assert_eq!(ledger.balance_through(2)["pear"], 5);
    }

    #[test]
    fn select_retains_only_one_item() {
        let mut ledger = Ledger::new();
        ledger.record("pear", 2, 3);
        ledger.record("apple", 1, 4);
        ledger.record("pear", 1, -1);
        let selected = ledger.select("pear");
        assert_eq!(selected.len(), 2);
        assert_eq!(selected.movements()[0].day, 2);
        assert_eq!(selected.movements()[1].day, 1);
        assert!(ledger.select("missing").is_empty());
    }
}
