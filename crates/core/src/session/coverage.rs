//! The compaction coverage shape, shared by storage validation and the builder.
//!
//! Since decision 14 (m6-02) a compaction's coverage is the selected span minus
//! every turn a control pinned or excluded. Coverage is therefore an ancestral,
//! ordered subsequence of the branch whose gaps are exactly the skipped turns.
//! Storage is the oracle and the builder revalidates the same rule on the
//! history it projects, both through this function, so the two cannot drift.

use super::{EntryId, StoredEntry, TurnId};

/// `Ok(())` when `covered` names entries of `ancestry` in ancestral order, and
/// every entry between its first and last covered entry that it does not cover
/// belongs to a turn `skipped` accepts.
///
/// `skipped` decides one turn, and each caller answers with the effective mode
/// at the compaction's own parent: a turn skipped by a compaction can be reset
/// later, and that must never retroactively invalidate recorded coverage.
///
/// Whole-turn grouping is not this function's rule: a covered entry whose turn
/// is not covered entirely is `compaction_splits_turn`, checked separately.
/// Given that rule, a gap can only be made of turns no covered entry belongs to.
pub(crate) fn error(
    ancestry: &[&StoredEntry],
    covered: &[EntryId],
    mut skipped: impl FnMut(&TurnId) -> bool,
) -> Result<(), &'static str> {
    let Some(first) = covered.first() else {
        return Err("compaction_not_ancestral");
    };
    let position = |id: &EntryId| ancestry.iter().position(|stored| &stored.entry.id == id);
    let start = position(first).ok_or("compaction_not_ancestral")?;
    let last = position(covered.last().ok_or("compaction_not_ancestral")?)
        .ok_or("compaction_not_ancestral")?;
    // Covered IDs out of ancestral order end before they begin.
    if last < start {
        return Err("compaction_not_contiguous");
    }
    let mut next = 0;
    for stored in &ancestry[start..=last] {
        if covered.get(next).is_some_and(|id| id == &stored.entry.id) {
            next += 1;
            continue;
        }
        let skippable = stored.entry.turn_id.as_ref().is_some_and(&mut skipped);
        if !skippable {
            return Err("compaction_not_contiguous");
        }
    }
    if next != covered.len() {
        return Err("compaction_not_contiguous");
    }
    Ok(())
}
