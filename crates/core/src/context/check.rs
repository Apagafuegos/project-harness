//! Invariants checked after every build, in every build profile. A violation
//! is a builder bug and becomes `ContextError::Invariant`.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::{
    projection::ContextItem,
    report::{ContextReport, ExclusionReason, ItemKind, Part},
};
use crate::session::{CallId, ContentBlock, ContextMode, EntryId, EntryPayload, StoredEntry};

pub(super) fn check(
    branch: &[&StoredEntry],
    input: &crate::model::ModelInput,
    report: &ContextReport,
    applied: &super::builder::Controls,
) -> Result<(), &'static str> {
    let items = &input.items;
    agrees(items, report)?;
    if report
        .included
        .iter()
        .flat_map(|r| &r.blocks)
        .filter(|b| b.part == Part::Continuation)
        .count()
        != usize::from(input.continuation.is_some())
    {
        return Err("continuation_disagrees_with_projection");
    }
    complete(branch, report)?;
    controls(branch, report, applied)?;
    ordered(branch, report)?;
    paired(items)
}

/// Included items follow ancestral order, and a summary stands where its first
/// covered entry is. Decision 14 projects a skipped pinned turn in place, so
/// this catches one moved after the retained window, or ahead of the entries a
/// summary spans around it.
fn ordered(branch: &[&StoredEntry], report: &ContextReport) -> Result<(), &'static str> {
    let positions: HashMap<&EntryId, usize> = branch
        .iter()
        .enumerate()
        .map(|(position, stored)| (&stored.entry.id, position))
        .collect();
    let starts: HashMap<&EntryId, usize> = report
        .summaries
        .iter()
        .filter_map(|summary| {
            let first = summary.covered_entry_ids.first()?;
            Some((&summary.entry_id, *positions.get(first)?))
        })
        .collect();
    let mut last = None;
    for record in &report.included {
        let Some(id) = &record.entry_id else {
            continue;
        };
        let position = if record.kind == ItemKind::Summary {
            *starts.get(id).ok_or("summary_without_coverage")?
        } else {
            *positions.get(id).ok_or("entry_from_other_branch")?
        };
        if last.is_some_and(|last| last >= position) {
            return Err("items_out_of_branch_order");
        }
        last = Some(position);
    }
    Ok(())
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
            // A request-only prompt has no entry and no report record, so a
            // projection that contains one is a builder bug, not a report
            // mismatch to be explained away.
            ContextItem::Request { .. } => return Err("request_item_in_projection"),
        };
        if kind != record.kind || item.entry_id() != record.entry_id.as_ref() {
            return Err("report_disagrees_with_projection");
        }
        let provenance = match item {
            ContextItem::ToolResult {
                provenance, result, ..
            } => {
                if *provenance != result.metadata.provenance {
                    return Err("tool_result_provenance_mismatch");
                }
                Some(*provenance)
            }
            _ => None,
        };
        if provenance != record.provenance {
            return Err("report_disagrees_with_projection");
        }
        if let ContextItem::Assistant { content, .. } = item {
            let blocks = record
                .blocks
                .iter()
                .filter(|b| matches!(b.part, Part::Block { .. }))
                .count();
            if blocks != content.len() {
                return Err("report_disagrees_with_projection");
            }
            if record.blocks.is_empty() {
                return Err("empty_assistant_item");
            }
        } else if !record.blocks.is_empty() {
            return Err("report_disagrees_with_projection");
        }
    }
    Ok(())
}

/// Every branch entry is decided exactly once: by one entry-level decision,
/// or, for a response none of whose parts is included, by part exclusions
/// alone. Every part of a response decided by parts, included or not, has
/// exactly one part-level decision; nothing off the branch appears; exclusion
/// reasons carry the right summary links.
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
        let summary = |id: &EntryId| {
            on_branch
                .get(id)
                .is_some_and(|stored| matches!(stored.entry.payload, EntryPayload::Compaction(_)))
        };
        let linked = match exclusion.reason {
            ExclusionReason::CoveredBySummary | ExclusionReason::SupersededSummary => {
                exclusion.summary_id.as_ref().is_some_and(summary)
                    && exclusion
                        .effective_summary
                        .as_ref()
                        .is_none_or(|effective| {
                            summary(effective) && Some(effective) != exclusion.summary_id.as_ref()
                        })
            }
            _ => exclusion.summary_id.is_none() && exclusion.effective_summary.is_none(),
        };
        let entry_level = matches!(
            exclusion.reason,
            ExclusionReason::FailedResponse
                | ExclusionReason::EmptyResponse
                | ExclusionReason::ModelChangeMarker
                | ExclusionReason::ContextControlMarker
                | ExclusionReason::UserExcluded
                | ExclusionReason::SupersededSummary
                | ExclusionReason::CoveredBySummary
        );
        if !linked || entry_level != (exclusion.part == Part::Entry) {
            return Err("invalid_exclusion");
        }
    }
    // Part decisions cover every block and the continuation of the response
    // they belong to. An included response also has its entry-level
    // inclusion; an excluded one has nothing else. Walk the branch in order
    // so the first error found is deterministic.
    for stored in branch {
        let id = &stored.entry.id;
        let entry_decisions = decided.get(id).copied().unwrap_or(0);
        let Some(decided) = parts.get(id) else {
            match entry_decisions {
                1 => continue,
                0 => return Err("entry_without_decision"),
                _ => return Err("duplicate_decision"),
            }
        };
        let EntryPayload::AssistantResponse(response) = &stored.entry.payload else {
            return Err("part_of_non_response");
        };
        let expected: BTreeSet<Part> = (0..response.content.len())
            .map(|index| Part::Block {
                index: index as u32,
            })
            .chain(response.continuation.as_ref().map(|_| Part::Continuation))
            .collect();
        if *decided != expected {
            return Err("incomplete_part_decisions");
        }
        let included = assistants.contains(id);
        match (included, entry_decisions) {
            (true, 1) | (false, 0) => {}
            _ => return Err("duplicate_decision"),
        }
    }
    Ok(())
}

/// Every effective control, every pin marker and every excluded turn agrees
/// with the modes applied to this build; control entries are never dialogue.
fn controls(
    branch: &[&StoredEntry],
    report: &ContextReport,
    applied: &super::builder::Controls,
) -> Result<(), &'static str> {
    let positions: HashMap<&EntryId, usize> = branch
        .iter()
        .enumerate()
        .map(|(position, stored)| (&stored.entry.id, position))
        .collect();
    // The reported summary names the applied control entry, in ancestral order.
    let mut last = None;
    let mut turns = HashSet::new();
    for item in &report.controls {
        let stored = branch
            .iter()
            .find(|stored| stored.entry.id == item.control_entry_id)
            .ok_or("control_entry_not_on_branch")?;
        let EntryPayload::ContextControl(control) = &stored.entry.payload else {
            return Err("control_entry_not_a_control");
        };
        if control.turn_id != item.turn_id || control.mode != item.mode {
            return Err("control_report_disagrees_with_entry");
        }
        if applied.mode(Some(&item.turn_id)) != item.mode
            || applied.setter(&item.turn_id) != Some(&item.control_entry_id)
        {
            return Err("control_report_disagrees_with_fold");
        }
        if item.mode.is_default() || !turns.insert(&item.turn_id) {
            return Err("control_report_disagrees_with_fold");
        }
        let position = positions[&item.control_entry_id];
        if last.is_some_and(|last| last >= position) {
            return Err("controls_out_of_ancestral_order");
        }
        last = Some(position);
    }
    // Every applied setter on this branch is reported.
    for stored in branch {
        let EntryPayload::ContextControl(control) = &stored.entry.payload else {
            continue;
        };
        let applied_mode = applied.mode(Some(&control.turn_id));
        if !applied_mode.is_default()
            && applied.setter(&control.turn_id) == Some(&stored.entry.id)
            && !report
                .controls
                .iter()
                .any(|item| item.control_entry_id == stored.entry.id)
        {
            return Err("unreported_effective_control");
        }
    }
    let sources: HashMap<&EntryId, &StoredEntry> = branch
        .iter()
        .map(|stored| (&stored.entry.id, *stored))
        .collect();
    for record in &report.included {
        let Some(id) = &record.entry_id else {
            if record.pinned {
                return Err("pinned_without_turn");
            }
            continue;
        };
        let stored = sources[id];
        if matches!(stored.entry.payload, EntryPayload::ContextControl(_)) {
            return Err("control_entry_included");
        }
        let mode = applied.mode(stored.entry.turn_id.as_ref());
        if record.pinned != (mode == ContextMode::Pinned) {
            return Err("pin_marker_disagrees_with_controls");
        }
        if mode == ContextMode::Excluded {
            return Err("excluded_turn_included");
        }
    }
    for stored in branch {
        let mode = applied.mode(stored.entry.turn_id.as_ref());
        let excluded_decision = |reason: ExclusionReason| {
            report.excluded.iter().any(|exclusion| {
                exclusion.entry_id == stored.entry.id
                    && exclusion.part == Part::Entry
                    && exclusion.reason == reason
            })
        };
        if mode == ContextMode::Excluded && !excluded_decision(ExclusionReason::UserExcluded) {
            return Err("excluded_entry_without_decision");
        }
        // A control entry is a marker, unless an active summary covers it: the
        // summary then stands for it as for any other source entry.
        let covered = excluded_decision(ExclusionReason::CoveredBySummary)
            || excluded_decision(ExclusionReason::SupersededSummary);
        if matches!(stored.entry.payload, EntryPayload::ContextControl(_))
            && !excluded_decision(ExclusionReason::ContextControlMarker)
            && !covered
        {
            return Err("undecided_control_entry");
        }
        if excluded_decision(ExclusionReason::ContextControlMarker)
            && !matches!(stored.entry.payload, EntryPayload::ContextControl(_))
        {
            return Err("invalid_exclusion");
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
