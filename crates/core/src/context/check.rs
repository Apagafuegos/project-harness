//! Invariants checked after every build, in every build profile. A violation
//! is a builder bug and becomes `ContextError::Invariant`.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::{
    projection::ContextItem,
    report::{ContextReport, ExclusionReason, ItemKind, Part},
};
use crate::session::{CallId, ContentBlock, EntryId, EntryPayload, StoredEntry};

pub(super) fn check(
    branch: &[&StoredEntry],
    items: &[ContextItem],
    report: &ContextReport,
) -> Result<(), &'static str> {
    agrees(items, report)?;
    complete(branch, report)?;
    paired(items)
}

/// The report's included list describes exactly the projection's items.
fn agrees(items: &[ContextItem], report: &ContextReport) -> Result<(), &'static str> {
    if items.len() != report.included.len() {
        return Err("report_disagrees_with_projection");
    }
    for (item, record) in items.iter().zip(&report.included) {
        let kind = match item {
            ContextItem::Instructions { .. } => ItemKind::Instructions,
            ContextItem::User { .. } => ItemKind::User,
            ContextItem::Assistant { .. } => ItemKind::Assistant,
            ContextItem::ToolResult { .. } => ItemKind::ToolResult,
            ContextItem::Summary { .. } => ItemKind::Summary,
        };
        if kind != record.kind || item.entry_id() != record.entry_id.as_ref() {
            return Err("report_disagrees_with_projection");
        }
        if let ContextItem::Assistant {
            content,
            continuation,
            ..
        } = item
        {
            let blocks = record
                .blocks
                .iter()
                .filter(|b| matches!(b.part, Part::Block { .. }))
                .count();
            let continued = record.blocks.iter().any(|b| b.part == Part::Continuation);
            if blocks != content.len() || continued != continuation.is_some() {
                return Err("report_disagrees_with_projection");
            }
        } else if !record.blocks.is_empty() {
            return Err("report_disagrees_with_projection");
        }
    }
    Ok(())
}

/// Every branch entry has exactly one entry-level decision; every part of an
/// included assistant response has exactly one part-level decision; nothing
/// off the branch appears; exclusion reasons carry the right summary link.
fn complete(branch: &[&StoredEntry], report: &ContextReport) -> Result<(), &'static str> {
    let on_branch: HashMap<&EntryId, &StoredEntry> = branch
        .iter()
        .map(|stored| (&stored.entry.id, *stored))
        .collect();
    let mut decided: HashMap<&EntryId, usize> = HashMap::with_capacity(branch.len());
    let mut parts: HashMap<&EntryId, BTreeSet<Part>> = HashMap::new();
    let mut assistants: HashSet<&EntryId> = HashSet::new();
    fn part<'a>(
        parts: &mut HashMap<&'a EntryId, BTreeSet<Part>>,
        id: &'a EntryId,
        part: Part,
    ) -> Result<(), &'static str> {
        if parts.entry(id).or_default().insert(part) {
            Ok(())
        } else {
            Err("duplicate_decision")
        }
    }
    for record in &report.included {
        let Some(id) = &record.entry_id else {
            continue;
        };
        if !on_branch.contains_key(id) {
            return Err("entry_from_other_branch");
        }
        *decided.entry(id).or_default() += 1;
        if record.kind == ItemKind::Assistant {
            assistants.insert(id);
            parts.entry(id).or_default();
        }
        for block in &record.blocks {
            part(&mut parts, id, block.part)?;
        }
    }
    for exclusion in &report.excluded {
        let id = &exclusion.entry_id;
        if !on_branch.contains_key(id) {
            return Err("entry_from_other_branch");
        }
        match exclusion.part {
            Part::Entry => *decided.entry(id).or_default() += 1,
            other => part(&mut parts, id, other)?,
        }
        let linked = match exclusion.reason {
            ExclusionReason::CoveredBySummary | ExclusionReason::SupersededSummary => {
                exclusion.summary_id.as_ref().is_some_and(|summary| {
                    on_branch.get(summary).is_some_and(|stored| {
                        matches!(stored.entry.payload, EntryPayload::Compaction(_))
                    })
                })
            }
            _ => exclusion.summary_id.is_none(),
        };
        let entry_level = matches!(
            exclusion.reason,
            ExclusionReason::FailedResponse
                | ExclusionReason::ModelChangeMarker
                | ExclusionReason::SupersededSummary
                | ExclusionReason::CoveredBySummary
        );
        if !linked || entry_level != (exclusion.part == Part::Entry) {
            return Err("invalid_exclusion");
        }
    }
    for stored in branch {
        match decided.get(&stored.entry.id) {
            Some(1) => {}
            Some(_) => return Err("duplicate_decision"),
            None => return Err("entry_without_decision"),
        }
    }
    // Part decisions exist exactly for included assistant responses, and
    // cover every block and the continuation. Every entry with parts is on
    // the branch (checked above); walk it in order so the first error found
    // is deterministic.
    for stored in branch {
        let id = &stored.entry.id;
        let Some(decided) = parts.get(id) else {
            continue;
        };
        let EntryPayload::AssistantResponse(response) = &stored.entry.payload else {
            return Err("part_of_non_response");
        };
        let included = assistants.contains(id);
        let expected: BTreeSet<Part> = (0..response.content.len())
            .map(|index| Part::Block {
                index: index as u32,
            })
            .chain(response.continuation.as_ref().map(|_| Part::Continuation))
            .collect();
        if !included || *decided != expected {
            return Err("incomplete_part_decisions");
        }
    }
    Ok(())
}

/// Every included call has exactly one included result, right after the
/// response proposing it, and every included result has its call.
pub(super) fn paired(items: &[ContextItem]) -> Result<(), &'static str> {
    let mut proposed: HashSet<(&EntryId, &CallId)> = HashSet::new();
    let mut answered: HashSet<(&EntryId, &CallId)> = HashSet::new();
    // The response whose results may follow: the last item, skipping results.
    let mut proposer: Option<&EntryId> = None;
    for item in items {
        match item {
            ContextItem::Assistant {
                entry_id, content, ..
            } => {
                for block in content {
                    if let ContentBlock::ToolCall { call_id, .. } = block
                        && !proposed.insert((entry_id, call_id))
                    {
                        return Err("duplicate_tool_call");
                    }
                }
                proposer = Some(entry_id);
            }
            ContextItem::ToolResult { result, .. } => {
                let key = (&result.call.response_entry_id, &result.call.call_id);
                if !proposed.contains(&key) || proposer != Some(key.0) {
                    return Err("tool_result_without_call");
                }
                if !answered.insert(key) {
                    return Err("duplicate_tool_result");
                }
            }
            _ => proposer = None,
        }
    }
    if proposed != answered {
        return Err("tool_call_without_result");
    }
    Ok(())
}
