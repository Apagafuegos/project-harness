//! Stable text output: one `item: quantity` line per item.
use std::collections::BTreeMap;

/// Format sorted balances, always ending each row with a newline.
pub fn render_balance(stock: &BTreeMap<String, i64>) -> String {
    let mut text = String::new();
    for (item, quantity) in stock {
        text.push_str(&format!("{item}: {quantity}\n"));
    }
    text
}

/// Parse the display format. Blank lines are ignored; duplicate items add up.
/// Invalid quantities and missing separators are rejected with a line number.
pub fn parse_balance(text: &str) -> Result<BTreeMap<String, i64>, usize> {
    let mut stock = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (item, quantity) = line.split_once(':').ok_or(index + 1)?;
        let item = item.trim();
        if item.is_empty() {
            return Err(index + 1);
        }
        let quantity: i64 = quantity.trim().parse().map_err(|_| index + 1)?;
        *stock.entry(item.to_owned()).or_default() += quantity;
    }
    Ok(stock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_display() {
        assert_eq!(render_balance(&BTreeMap::new()), "");
        assert!(parse_balance("").unwrap().is_empty());
    }

    #[test]
    fn sorted_display_with_newlines() {
        let stock = BTreeMap::from([("pear".into(), -2), ("apple".into(), 4)]);
        assert_eq!(render_balance(&stock), "apple: 4\npear: -2\n");
    }

    #[test]
    fn parse_combines_duplicates() {
        let stock = parse_balance("pear: 3\n\npear: -1\n").unwrap();
        assert_eq!(stock["pear"], 2);
        assert_eq!(stock.len(), 1);
    }

    #[test]
    fn parse_trims_fields() {
        assert_eq!(parse_balance(" apple : 2 ").unwrap()["apple"], 2);
    }

    #[test]
    fn parse_reports_line_number() {
        assert_eq!(parse_balance("apple: 2\npear: no"), Err(2));
        assert_eq!(parse_balance("apple 2"), Err(1));
        assert_eq!(parse_balance(": 2"), Err(1));
    }

    #[test]
    fn display_round_trip() {
        let stock = BTreeMap::from([("apple".into(), 0), ("pear".into(), 10)]);
        assert_eq!(parse_balance(&render_balance(&stock)).unwrap(), stock);
    }
}
