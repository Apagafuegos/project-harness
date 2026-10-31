//! The pure builder. One pass over the branch finds summaries and answered
//! calls; a second makes exactly one decision per entry and per handled part,
//! producing the projection and its report together.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    BUILDER_VERSION, PROJECTION_VERSION, check,
    config::{ContextConfig, SessionConfiguration},
    estimate::{Estimate, EstimateMethod, MediaKind, TokenEstimator},
    projection::{ContextItem, Instruction, Projection, fingerprint},
    report::*,
};
use crate::{
    model::ModelInput,
    session::{
        ArtifactRef, AssistantResponse, CallId, Compaction, ContentBlock, ContextMode, EntryId,
        EntryPayload, ModelRef, ResponseOutcome, SessionSnapshot, StoredEntry, ToolResultEntry,
        TurnId, UserMessage, coverage,
    },
    tools::ToolDefinition,
};

/// Why no context could be built. None of these is an overflow: an
/// overflowing projection is built and flagged.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    /// The head is not an entry of the snapshot.
    #[error("unknown_head")]
    UnknownHead,
    /// The decoded configuration is not the snapshot's configuration artifact.
    #[error("configuration_mismatch")]
    ConfigurationMismatch,
    /// A configuration value is out of range.
    #[error("invalid_context_config: {0}")]
    InvalidConfig(&'static str),
    /// A builder invariant failed. This is a bug, never a property of history.
    #[error("context_invariant: {0}")]
    Invariant(&'static str),
}

impl ContextError {
    /// Stable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownHead => "unknown_head",
            Self::ConfigurationMismatch => "configuration_mismatch",
            Self::InvalidConfig(code) | Self::Invariant(code) => code,
        }
    }
}

/// A projection and the report built in the same walk.
#[derive(Clone, Debug, PartialEq)]
pub struct BuiltContext {
    /// What goes to the provider.
    pub projection: Projection,
    /// Where every entry on the branch went.
    pub report: ContextReport,
}

/// Builds projections from snapshots. Pure: it reads only the snapshot, the
/// head, its configuration and its estimator.
#[derive(Clone, Copy)]
pub struct ContextBuilder<'a> {
    config: &'a ContextConfig,
    configuration: &'a SessionConfiguration,
    estimator: &'a dyn TokenEstimator,
    tools: &'a [ToolDefinition],
}

impl std::fmt::Debug for ContextBuilder<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextBuilder")
            .field("config", self.config)
            .field("configuration", self.configuration)
            .field("estimator", &self.estimator.info())
            .finish()
    }
}

impl<'a> ContextBuilder<'a> {
    /// A builder using the configured heuristic estimator.
    pub fn new(config: &'a ContextConfig, configuration: &'a SessionConfiguration) -> Self {
        Self {
            config,
            configuration,
            estimator: &config.estimator,
            tools: &[],
        }
    }

    /// Enabled, available definitions used for both requests and fingerprints.
    pub fn with_tools(self, tools: &'a [ToolDefinition]) -> Self {
        Self { tools, ..self }
    }

    /// Replace the estimator.
    pub fn with_estimator(self, estimator: &'a dyn TokenEstimator) -> Self {
        Self { estimator, ..self }
    }

    /// Project the branch ending at `head` for the model at that head. With
    /// `model_override`, build a preview for that model instead: the report
    /// sets `preview` and shows what switching would change. A preview is
    /// never sent to a provider.
    pub fn build(
        &self,
        snapshot: &SessionSnapshot,
        head: Option<&EntryId>,
        model_override: Option<&ModelRef>,
    ) -> Result<BuiltContext, ContextError> {
        let branch = snapshot
            .ancestry(head)
            .map_err(|_| ContextError::UnknownHead)?;
        let model = model_override.unwrap_or(
            snapshot
                .model_at(head)
                .map_err(|_| ContextError::UnknownHead)?,
        );
        // Effective controls are folded once, over the selected head's
        // ancestry. A prefix rebuilt for a continuation uses the same fold: a
        // control appended after a response still changes what that response's
        // prefix projects to, so the existing prefix comparison sees it.
        let controls = Controls::fold(&branch);
        // Build required prefixes oldest first. Every continuation's parent
        // precedes it, so a fingerprint is computed once, without recursion or
        // an exponential re-walk of earlier continuation-only responses.
        let mut prefixes = BTreeMap::new();
        for stored in branch {
            if let EntryPayload::AssistantResponse(response) = &stored.entry.payload
                && response.continuation.is_some()
            {
                let parent = stored.entry.parent_id.clone();
                if !prefixes.contains_key(&parent) {
                    let prefix = self.build_inner(
                        snapshot,
                        parent.as_ref(),
                        Some(model),
                        &prefixes,
                        &controls,
                    )?;
                    prefixes.insert(parent, prefix.projection.fingerprint);
                }
            }
        }
        self.build_inner(snapshot, head, model_override, &prefixes, &controls)
    }

    fn build_inner(
        &self,
        snapshot: &SessionSnapshot,
        head: Option<&EntryId>,
        model_override: Option<&ModelRef>,
        prefixes: &BTreeMap<Option<EntryId>, crate::session::Digest>,
        controls: &Controls,
    ) -> Result<BuiltContext, ContextError> {
        if !(1..=1000).contains(&self.config.compaction_trigger_permille) {
            return Err(ContextError::InvalidConfig("compaction_trigger_permille"));
        }
        if self.configuration.artifact() != snapshot.configuration() {
            return Err(ContextError::ConfigurationMismatch);
        }
        let branch = snapshot
            .ancestry(head)
            .map_err(|_| ContextError::UnknownHead)?;
        let head_model = snapshot
            .model_at(head)
            .map_err(|_| ContextError::UnknownHead)?;
        let model = model_override.unwrap_or(head_model);
        let plan = Plan::new(&branch)?;

        let mut walk = Walk {
            model,
            head_model,
            preview: model_override.is_some(),
            estimator: self.estimator,
            method: self.estimator.method(),
            items: Vec::new(),
            included: Vec::new(),
            excluded: Vec::new(),
            counts: Counts::default(),
            portability: Vec::new(),
            stop_portable: Vec::new(),
            become_portable: Vec::new(),
            opaque_at_head: 0,
            pinned: false,
        };
        let instructions = match self.configuration.instructions() {
            None => InstructionsReport::Absent,
            Some([]) => InstructionsReport::Empty,
            Some(instructions) => walk.instructions(instructions)?,
        };
        let mut latest_completed = None;
        for stored in &branch {
            let entry = &stored.entry;
            let id = &entry.id;
            if let Some(summary) = plan.starts.get(id) {
                // A summary is not dialogue and belongs to no turn, even when
                // its first covered entry is in a pinned one.
                let pinned = walk.pinned;
                walk.pinned = false;
                walk.summary(summary)?;
                walk.pinned = pinned;
            }
            if let EntryPayload::AssistantResponse(response) = &entry.payload
                && response.outcome == ResponseOutcome::Completed
            {
                latest_completed = Some((id, response));
            }
            // Whole-turn controls are decided before summary replacement: an
            // excluded turn's entries are all reported as the user's choice.
            let mode = controls.mode(entry.turn_id.as_ref());
            walk.pinned = mode == ContextMode::Pinned;
            if mode == ContextMode::Excluded {
                walk.exclude(id, Part::Entry, ExclusionReason::UserExcluded, None);
                continue;
            }
            let replaced = plan
                .superseded_by
                .get(id)
                .map(|by| (ExclusionReason::SupersededSummary, by))
                .or_else(|| {
                    plan.covered_by
                        .get(id)
                        .map(|by| (ExclusionReason::CoveredBySummary, by))
                });
            if let Some((reason, by)) = replaced {
                let effective = plan.effective(by, branch.len())?;
                walk.excluded.push(Exclusion {
                    entry_id: id.clone(),
                    part: Part::Entry,
                    reason,
                    summary_id: Some((*by).clone()),
                    effective_summary: (effective != *by).then(|| effective.clone()),
                });
                continue;
            }
            match &entry.payload {
                EntryPayload::UserMessage(message) => walk.user(id, message)?,
                EntryPayload::AssistantResponse(response) => {
                    walk.assistant(id, response, &plan.answered)?
                }
                EntryPayload::ToolResult(result) => walk.tool_result(id, result)?,
                EntryPayload::ModelChanged(_) => {
                    walk.exclude(id, Part::Entry, ExclusionReason::ModelChangeMarker, None)
                }
                EntryPayload::ContextControl(_) => {
                    walk.exclude(id, Part::Entry, ExclusionReason::ContextControlMarker, None)
                }
                // An active summary that no other covers: its item was placed
                // at its first covered entry, which precedes it.
                EntryPayload::Compaction(_) => {}
            }
        }

        let by_id: BTreeMap<_, _> = branch.iter().map(|s| (&s.entry.id, *s)).collect();
        let mut invalid = BTreeSet::new();
        let selected = loop {
            let Some((index, id, empty)) =
                walk.items
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(index, item)| match item {
                        ContextItem::Assistant {
                            entry_id, content, ..
                        } => Some((index, entry_id.clone(), content.is_empty())),
                        _ => None,
                    })
            else {
                break None;
            };
            let stored = by_id[&id];
            let EntryPayload::AssistantResponse(response) = &stored.entry.payload else {
                unreachable!("assistant source")
            };
            let Some(native) = &response.continuation else {
                break None;
            };
            if native.owner != *model {
                break None;
            }
            let prefix = prefixes
                .get(&stored.entry.parent_id)
                .ok_or(ContextError::Invariant("missing_continuation_prefix"))?;
            if *prefix == native.input_fingerprint {
                break Some(id);
            }
            invalid.insert(id.clone());
            if !empty {
                break None;
            }
            // An invalid continuation cannot keep an empty assistant alive.
            walk.items.remove(index);
            walk.included.remove(index);
        };
        // Only the selected continuation may stand for an otherwise empty
        // assistant. Earlier continuation-only responses are part exclusions.
        let mut index = 0;
        while index < walk.items.len() {
            if matches!(&walk.items[index], ContextItem::Assistant { entry_id, content, .. }
                if content.is_empty() && selected.as_ref() != Some(entry_id))
            {
                walk.items.remove(index);
                walk.included.remove(index);
            } else {
                index += 1;
            }
        }
        let mut continuation = None;
        for stored in &branch {
            let EntryPayload::AssistantResponse(response) = &stored.entry.payload else {
                continue;
            };
            let Some(native) = &response.continuation else {
                continue;
            };
            let id = &stored.entry.id;
            // Entry-level exclusions already account for all parts.
            if walk
                .excluded
                .iter()
                .any(|e| &e.entry_id == id && e.part == Part::Entry)
            {
                continue;
            }
            let owned = walk.owned(
                id,
                Part::Continuation,
                &native.owner,
                &native.schema,
                &native.artifact,
            );
            if !owned {
                continue;
            }
            let reason = if invalid.contains(id) {
                Some(ExclusionReason::ContinuationPrefixMismatch)
            } else if selected.as_ref() != Some(id) {
                Some(ExclusionReason::ContinuationNotLatest)
            } else {
                None
            };
            if let Some(reason) = reason {
                if native.owner == *head_model {
                    walk.opaque_at_head = walk.opaque_at_head.saturating_sub(native.artifact.bytes);
                }
                walk.exclude(id, Part::Continuation, reason, None);
            } else {
                let record = walk
                    .included
                    .iter_mut()
                    .find(|r| r.entry_id.as_ref() == Some(id))
                    .ok_or(ContextError::Invariant("continuation_without_assistant"))?;
                record.blocks.push(IncludedBlock {
                    part: Part::Continuation,
                    kind: BlockKind::Continuation,
                    estimated_tokens: 0,
                    opaque_bytes: native.artifact.bytes,
                });
                record.opaque_bytes = record.opaque_bytes.saturating_add(native.artifact.bytes);
                walk.counts.opaque = walk.counts.opaque.saturating_add(native.artifact.bytes);
                continuation = Some(native.clone());
            }
        }

        // Continuation decisions are made after the content walk; restore
        // branch/part order so the report remains source-discoverable.
        let positions: BTreeMap<_, _> = branch
            .iter()
            .enumerate()
            .map(|(n, s)| (&s.entry.id, n))
            .collect();
        walk.excluded
            .sort_by_key(|e| (positions[&e.entry_id], e.part));
        walk.portability
            .sort_by_key(|e| (positions[&e.entry_id], e.part));
        walk.stop_portable
            .sort_by_key(|e| (positions[&e.entry_id], e.part));
        walk.become_portable
            .sort_by_key(|e| (positions[&e.entry_id], e.part));
        let counts = &walk.counts;
        let estimated = [
            counts.instructions,
            counts.user,
            counts.assistant_text,
            counts.tool_calls,
            counts.tool_results,
            counts.summaries,
            counts.media,
        ]
        .into_iter()
        .fold(0u64, u64::saturating_add);
        let totals = Totals {
            estimator: self.estimator.info(),
            estimated_instructions: counts.instructions,
            estimated_user: counts.user,
            estimated_assistant_text: counts.assistant_text,
            estimated_tool_calls: counts.tool_calls,
            estimated_tool_results: counts.tool_results,
            estimated_summaries: counts.summaries,
            estimated_media: counts.media,
            estimated_total: estimated,
            opaque_provider_state_bytes: counts.opaque,
        };
        let budget = budget_for(self.config, model, estimated, counts.opaque);
        let (overflow, compaction_recommended) = thresholds(&budget);
        let budget_unknown = matches!(budget, Budget::Unknown { .. });
        let preview = walk.preview.then(|| PreviewReport {
            required_incompatibilities: required_incompatibilities(
                &walk.items,
                &walk.included,
                &self.config.catalog.capabilities(model),
            ),
            model_at_head: head_model.clone(),
            preview_model: model.clone(),
            would_stop_being_portable: std::mem::take(&mut walk.stop_portable),
            would_become_portable: std::mem::take(&mut walk.become_portable),
            budget_at_head: budget_for(self.config, head_model, estimated, walk.opaque_at_head),
        });
        let reported_usage = latest_completed.and_then(|(id, response)| {
            response.usage.clone().map(|usage| ReportedUsage {
                basis: UsageBasis::ProviderReported,
                entry_id: id.clone(),
                response_id: response.response_id.clone(),
                model: response.model.clone(),
                usage,
            })
        });
        let input = ModelInput {
            model: model.clone(),
            items: walk.items,
            tools: self.tools.to_vec(),
            options: self
                .configuration
                .inference_options(self.config.output_reserve),
            continuation,
        };
        let fingerprint = fingerprint(&input);
        let report = ContextReport {
            identity: ReportIdentity {
                session_id: snapshot.session_id().clone(),
                journal_sequence: snapshot.sequence(),
                head: head.cloned(),
                model: model.clone(),
                configuration: snapshot.configuration().sha256.clone(),
                builder_version: BUILDER_VERSION,
                projection_version: PROJECTION_VERSION,
            },
            fingerprint: fingerprint.clone(),
            instructions,
            included: walk.included,
            excluded: walk.excluded,
            summaries: plan.summaries(self.estimator),
            controls: controls.report_on(&branch),
            totals,
            budget,
            reported_usage,
            portability: walk.portability,
            preview,
            flags: Flags {
                budget_unknown,
                overflow,
                compaction_recommended,
                preview: model_override.is_some(),
            },
        };
        check::check(&branch, &input, &report, controls).map_err(ContextError::Invariant)?;
        Ok(BuiltContext {
            projection: Projection {
                head: head.cloned(),
                input,
                fingerprint,
                preview: model_override.is_some(),
            },
            report,
        })
    }
}

/// Facts the decision pass needs before it reaches them.
struct Plan<'a> {
    /// Summaries listed in a later summary's `supersedes`, with the first
    /// summary on the branch that supersedes each.
    superseded_by: BTreeMap<&'a EntryId, &'a EntryId>,
    /// Entries covered by a summary that is not superseded, with that summary.
    covered_by: BTreeMap<&'a EntryId, &'a EntryId>,
    /// Projected summaries by their first covered entry.
    starts: BTreeMap<&'a EntryId, (&'a EntryId, &'a Compaction)>,
    /// Summaries that are not superseded, in branch order.
    active: Vec<(&'a EntryId, &'a Compaction)>,
    /// Calls with a result on the branch.
    answered: BTreeSet<(&'a EntryId, &'a CallId)>,
}

impl<'a> Plan<'a> {
    fn new(branch: &[&'a StoredEntry]) -> Result<Self, ContextError> {
        let mut superseded_by = BTreeMap::new();
        let mut answered = BTreeSet::new();
        let mut summaries = Vec::new();
        // Effective modes at the parent of each entry, folded as the branch is
        // walked. They are exactly what storage folded when it accepted the
        // compaction, so both apply the shared coverage rule to the same modes.
        let mut modes: BTreeMap<TurnId, ContextMode> = BTreeMap::new();
        for (index, stored) in branch.iter().enumerate() {
            match &stored.entry.payload {
                EntryPayload::Compaction(compaction) => {
                    // Decision 14: one shared rule, so storage and the builder
                    // cannot disagree about which gaps may be skipped.
                    coverage::error(&branch[..index], &compaction.covered_entry_ids, |turn| {
                        modes.get(turn).is_some_and(|mode| !mode.is_default())
                    })
                    .map_err(ContextError::Invariant)?;
                    for old in &compaction.supersedes {
                        superseded_by.entry(old).or_insert(&stored.entry.id);
                    }
                    summaries.push((&stored.entry.id, compaction));
                }
                EntryPayload::ContextControl(control) => {
                    if control.mode.is_default() {
                        modes.remove(&control.turn_id);
                    } else {
                        modes.insert(control.turn_id.clone(), control.mode);
                    }
                }
                EntryPayload::ToolResult(result) => {
                    let call = &result.result.call;
                    answered.insert((&call.response_entry_id, &call.call_id));
                }
                _ => {}
            }
        }
        let on_branch: BTreeSet<_> = branch.iter().map(|stored| &stored.entry.id).collect();
        let active: Vec<_> = summaries
            .into_iter()
            .filter(|(id, _)| !superseded_by.contains_key(id))
            .collect();
        let mut covered_by = BTreeMap::new();
        for (id, compaction) in &active {
            for covered in &compaction.covered_entry_ids {
                if !on_branch.contains(covered) {
                    return Err(ContextError::Invariant("summary_covers_other_branch"));
                }
                if covered_by.insert(covered, *id).is_some() {
                    return Err(ContextError::Invariant("overlapping_active_summaries"));
                }
            }
        }
        let mut starts = BTreeMap::new();
        for (id, compaction) in &active {
            if !covered_by.contains_key(id) {
                let first = compaction
                    .covered_entry_ids
                    .first()
                    .ok_or(ContextError::Invariant("empty_summary_coverage"))?;
                starts.insert(first, (*id, *compaction));
            }
        }
        Ok(Self {
            superseded_by,
            covered_by,
            starts,
            active,
            answered,
        })
    }

    /// The summary standing for `summary` in the projection: follow
    /// supersession, then coverage, until one is a projection item. Both
    /// links point later on the branch, so `limit` steps always suffice.
    fn effective(&self, summary: &'a EntryId, limit: usize) -> Result<&'a EntryId, ContextError> {
        let mut current = summary;
        for _ in 0..=limit {
            match self
                .superseded_by
                .get(current)
                .or_else(|| self.covered_by.get(current))
            {
                Some(next) => current = next,
                None => return Ok(current),
            }
        }
        Err(ContextError::Invariant("summary_cycle"))
    }

    fn summaries(&self, estimator: &dyn TokenEstimator) -> Vec<SummaryReport> {
        self.active
            .iter()
            .map(|(id, compaction)| SummaryReport {
                entry_id: (*id).clone(),
                included: !self.covered_by.contains_key(id),
                strategy: compaction.strategy.clone(),
                strategy_version: compaction.strategy_version.clone(),
                covered_entry_ids: compaction.covered_entry_ids.clone(),
                supersedes: compaction.supersedes.clone(),
                estimated_tokens: estimator.text(&compaction.summary).tokens,
            })
            .collect()
    }
}

/// Effective whole-turn controls, folded over the selected ancestry only. A
/// sibling branch's controls never apply. The last control for a turn wins, and
/// a reset removes it.
pub(super) struct Controls {
    /// Non-default mode per turn.
    modes: BTreeMap<TurnId, ContextMode>,
    /// The control entry setting each turn's mode, with its branch position.
    set_by: BTreeMap<TurnId, (EntryId, usize)>,
}

impl Controls {
    pub(super) fn fold(branch: &[&StoredEntry]) -> Self {
        let mut modes = BTreeMap::new();
        let mut set_by = BTreeMap::new();
        for (position, stored) in branch.iter().enumerate() {
            let EntryPayload::ContextControl(control) = &stored.entry.payload else {
                continue;
            };
            if control.mode.is_default() {
                modes.remove(&control.turn_id);
                set_by.remove(&control.turn_id);
            } else {
                modes.insert(control.turn_id.clone(), control.mode);
                set_by.insert(control.turn_id.clone(), (stored.entry.id.clone(), position));
            }
        }
        Self { modes, set_by }
    }

    pub(super) fn mode(&self, turn: Option<&TurnId>) -> ContextMode {
        turn.and_then(|turn| self.modes.get(turn).copied())
            .unwrap_or_default()
    }

    /// The control entry that sets `turn`'s applied mode, if it has one.
    pub(super) fn setter(&self, turn: &TurnId) -> Option<&EntryId> {
        self.modes.contains_key(turn).then(|| &self.set_by[turn].0)
    }

    /// One report per controlled turn whose effective control entry is on
    /// `branch`, ordered by that entry. A prefix build sees the head's modes
    /// but cannot name a control entry it does not contain, so those turns are
    /// omitted from its report; the head build always contains every setter.
    fn report_on(&self, branch: &[&StoredEntry]) -> Vec<ControlReport> {
        let positions: BTreeMap<&EntryId, usize> = branch
            .iter()
            .enumerate()
            .map(|(position, stored)| (&stored.entry.id, position))
            .collect();
        let mut reports: Vec<(usize, ControlReport)> = self
            .set_by
            .iter()
            .filter_map(|(turn, (entry, _))| {
                let position = *positions.get(entry)?;
                Some((
                    position,
                    ControlReport {
                        turn_id: turn.clone(),
                        mode: *self.modes.get(turn)?,
                        control_entry_id: entry.clone(),
                    },
                ))
            })
            .collect();
        reports.sort_by_key(|(position, _)| *position);
        reports.into_iter().map(|(_, report)| report).collect()
    }
}

#[derive(Default)]
struct Counts {
    instructions: u64,
    user: u64,
    assistant_text: u64,
    tool_calls: u64,
    tool_results: u64,
    summaries: u64,
    media: u64,
    opaque: u64,
}

#[derive(Clone, Copy)]
enum Category {
    Instructions,
    User,
    AssistantText,
    ToolCalls,
    ToolResults,
    Summaries,
    Media,
}

/// Decisions so far.
struct Walk<'a> {
    model: &'a ModelRef,
    head_model: &'a ModelRef,
    preview: bool,
    estimator: &'a dyn TokenEstimator,
    method: EstimateMethod,
    items: Vec<ContextItem>,
    included: Vec<IncludedItem>,
    excluded: Vec<Exclusion>,
    counts: Counts,
    portability: Vec<PortabilityLoss>,
    stop_portable: Vec<PortabilityLoss>,
    become_portable: Vec<PortabilityLoss>,
    /// Opaque bytes the projection would carry under the head's model.
    opaque_at_head: u64,
    /// Whether the entry being decided belongs to a pinned turn.
    pinned: bool,
}

impl Walk<'_> {
    fn tokens(&self, estimate: Estimate) -> Result<u64, ContextError> {
        if estimate.method == self.method {
            Ok(estimate.tokens)
        } else {
            Err(ContextError::Invariant("mixed_estimate_methods"))
        }
    }

    fn count(&mut self, category: Category, tokens: u64) {
        let counter = match category {
            Category::Instructions => &mut self.counts.instructions,
            Category::User => &mut self.counts.user,
            Category::AssistantText => &mut self.counts.assistant_text,
            Category::ToolCalls => &mut self.counts.tool_calls,
            Category::ToolResults => &mut self.counts.tool_results,
            Category::Summaries => &mut self.counts.summaries,
            Category::Media => &mut self.counts.media,
        };
        *counter = counter.saturating_add(tokens);
    }

    /// Estimate and count one text, media or tool-call block, attributing
    /// text to `text_category`. Other kinds are never estimated here.
    fn block(
        &mut self,
        block: &ContentBlock,
        text_category: Category,
    ) -> Result<(u64, BlockKind), ContextError> {
        let (category, estimate, kind) = match block {
            ContentBlock::Text { text } => {
                (text_category, self.estimator.text(text), BlockKind::Text)
            }
            ContentBlock::Image { .. } => (
                Category::Media,
                self.estimator.media(MediaKind::Image),
                BlockKind::Image,
            ),
            ContentBlock::Audio { .. } => (
                Category::Media,
                self.estimator.media(MediaKind::Audio),
                BlockKind::Audio,
            ),
            ContentBlock::File { .. } => (
                Category::Media,
                self.estimator.media(MediaKind::File),
                BlockKind::File,
            ),
            ContentBlock::ToolCall {
                name, arguments, ..
            } => {
                // Byte length is independent of the map's key order.
                let arguments = serde_json::to_string(arguments)
                    .map_err(|_| ContextError::Invariant("unserializable_arguments"))?;
                let name = self.tokens(self.estimator.text(name))?;
                let arguments = self.estimator.text(&arguments);
                let estimate = Estimate {
                    tokens: name.saturating_add(arguments.tokens),
                    method: arguments.method,
                };
                (Category::ToolCalls, estimate, BlockKind::ToolCall)
            }
            ContentBlock::ReasoningSummary { .. } | ContentBlock::ProviderState { .. } => {
                return Err(ContextError::Invariant("unestimable_block"));
            }
        };
        let tokens = self.tokens(estimate)?;
        self.count(category, tokens);
        Ok((tokens, kind))
    }

    fn push(&mut self, item: ContextItem, record: IncludedItem) {
        let mut record = record;
        record.pinned = self.pinned && record.entry_id.is_some();
        self.items.push(item);
        self.included.push(record);
    }

    fn exclude(
        &mut self,
        entry_id: &EntryId,
        part: Part,
        reason: ExclusionReason,
        summary_id: Option<&EntryId>,
    ) {
        self.excluded.push(Exclusion {
            entry_id: entry_id.clone(),
            part,
            reason,
            summary_id: summary_id.cloned(),
            effective_summary: None,
        });
    }

    fn instructions(
        &mut self,
        instructions: &[Instruction],
    ) -> Result<InstructionsReport, ContextError> {
        let mut tokens = 0u64;
        for instruction in instructions {
            tokens = tokens.saturating_add(self.tokens(self.estimator.text(&instruction.text))?);
        }
        self.count(Category::Instructions, tokens);
        self.push(
            ContextItem::Instructions {
                instructions: instructions.to_vec(),
            },
            IncludedItem {
                entry_id: None,
                kind: ItemKind::Instructions,
                estimated_tokens: tokens,
                opaque_bytes: 0,
                blocks: Vec::new(),
                provenance: None,
                pinned: false,
            },
        );
        Ok(InstructionsReport::Included {
            count: instructions.len() as u64,
            estimated_tokens: tokens,
        })
    }

    fn user(&mut self, id: &EntryId, message: &UserMessage) -> Result<(), ContextError> {
        let mut tokens = 0u64;
        for block in &message.content {
            if !matches!(
                block,
                ContentBlock::Text { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::Audio { .. }
                    | ContentBlock::File { .. }
            ) {
                return Err(ContextError::Invariant("invalid_user_block"));
            }
            tokens = tokens.saturating_add(self.block(block, Category::User)?.0);
        }
        self.push(
            ContextItem::User {
                entry_id: id.clone(),
                content: message.content.clone(),
            },
            IncludedItem {
                entry_id: Some(id.clone()),
                kind: ItemKind::User,
                estimated_tokens: tokens,
                opaque_bytes: 0,
                blocks: Vec::new(),
                provenance: None,
                pinned: false,
            },
        );
        Ok(())
    }

    fn assistant(
        &mut self,
        id: &EntryId,
        response: &AssistantResponse,
        answered: &BTreeSet<(&EntryId, &CallId)>,
    ) -> Result<(), ContextError> {
        if response.outcome == ResponseOutcome::Failed {
            self.exclude(id, Part::Entry, ExclusionReason::FailedResponse, None);
            return Ok(());
        }
        if response.content.is_empty() && response.continuation.is_none() {
            self.exclude(id, Part::Entry, ExclusionReason::EmptyResponse, None);
            return Ok(());
        }
        let mut content = Vec::new();
        let mut blocks = Vec::new();
        let (mut tokens, mut opaque) = (0u64, 0u64);
        for (index, block) in response.content.iter().enumerate() {
            let index =
                u32::try_from(index).map_err(|_| ContextError::Invariant("block_index_range"))?;
            let part = Part::Block { index };
            let (estimated, bytes, kind) = match block {
                ContentBlock::ReasoningSummary { .. } => {
                    self.exclude(id, part, ExclusionReason::ReasoningDisplayOnly, None);
                    continue;
                }
                ContentBlock::ToolCall { call_id, .. } if !answered.contains(&(id, call_id)) => {
                    self.exclude(id, part, ExclusionReason::OrphanToolCall, None);
                    continue;
                }
                ContentBlock::ProviderState {
                    owner,
                    schema,
                    artifact,
                } => {
                    if !self.owned(id, part, owner, schema, artifact) {
                        continue;
                    }
                    (0, artifact.bytes, BlockKind::ProviderState)
                }
                block => {
                    let (estimated, kind) = self.block(block, Category::AssistantText)?;
                    (estimated, 0, kind)
                }
            };
            tokens = tokens.saturating_add(estimated);
            opaque = opaque.saturating_add(bytes);
            content.push(block.clone());
            blocks.push(IncludedBlock {
                part,
                kind,
                estimated_tokens: estimated,
                opaque_bytes: bytes,
            });
        }
        // Nothing survived: the part exclusions already account for every
        // block, and an empty item would only cost the provider a turn.
        if blocks.is_empty()
            && !response
                .continuation
                .as_ref()
                .is_some_and(|c| c.owner == *self.model)
        {
            return Ok(());
        }
        self.counts.opaque = self.counts.opaque.saturating_add(opaque);
        self.push(
            ContextItem::Assistant {
                entry_id: id.clone(),
                outcome: response.outcome,
                content,
            },
            IncludedItem {
                entry_id: Some(id.clone()),
                kind: ItemKind::Assistant,
                estimated_tokens: tokens,
                opaque_bytes: opaque,
                blocks,
                provenance: None,
                pinned: false,
            },
        );
        Ok(())
    }

    /// Decide one piece of owner-scoped state: included only for its owner.
    /// Records portability losses and, in a preview, what switching changes.
    fn owned(
        &mut self,
        id: &EntryId,
        part: Part,
        owner: &ModelRef,
        schema: &str,
        artifact: &ArtifactRef,
    ) -> bool {
        let portable = owner == self.model;
        let at_head = owner == self.head_model;
        if at_head {
            self.opaque_at_head = self.opaque_at_head.saturating_add(artifact.bytes);
        }
        let loss = || PortabilityLoss {
            entry_id: id.clone(),
            part,
            owner: owner.clone(),
            schema: schema.to_owned(),
            bytes: artifact.bytes,
        };
        if self.preview && at_head && !portable {
            self.stop_portable.push(loss());
        }
        if self.preview && portable && !at_head {
            self.become_portable.push(loss());
        }
        if !portable {
            self.portability.push(loss());
            self.exclude(id, part, ExclusionReason::ProviderStateNotPortable, None);
        }
        portable
    }

    fn tool_result(&mut self, id: &EntryId, result: &ToolResultEntry) -> Result<(), ContextError> {
        let mut tokens = 0u64;
        for block in &result.result.content {
            tokens = tokens.saturating_add(self.block(block, Category::ToolResults)?.0);
        }
        // The model sees the text, which leads with any code; dropping the
        // v2 code keeps the item equal to what the fingerprint hashes.
        let mut projected = result.result.clone();
        projected.metadata.code = None;
        self.push(
            ContextItem::ToolResult {
                entry_id: id.clone(),
                provenance: result.result.metadata.provenance,
                result: projected,
            },
            IncludedItem {
                entry_id: Some(id.clone()),
                kind: ItemKind::ToolResult,
                estimated_tokens: tokens,
                opaque_bytes: 0,
                blocks: Vec::new(),
                provenance: Some(result.result.metadata.provenance),
                pinned: false,
            },
        );
        Ok(())
    }

    fn summary(&mut self, (id, compaction): &(&EntryId, &Compaction)) -> Result<(), ContextError> {
        let tokens = self.tokens(self.estimator.text(&compaction.summary))?;
        self.count(Category::Summaries, tokens);
        self.push(
            ContextItem::Summary {
                entry_id: (*id).clone(),
                summary: compaction.summary.clone(),
            },
            IncludedItem {
                entry_id: Some((*id).clone()),
                kind: ItemKind::Summary,
                estimated_tokens: tokens,
                opaque_bytes: 0,
                blocks: Vec::new(),
                provenance: None,
                pinned: false,
            },
        );
        Ok(())
    }
}

/// The budget of an estimate under `model`'s configured limits.
fn required_incompatibilities(
    items: &[ContextItem],
    included: &[IncludedItem],
    capabilities: &super::config::ModelCapabilities,
) -> Vec<RequiredIncompatibility> {
    use super::config::InputMediaKind;
    let mut required = Vec::new();
    for (item, source) in items.iter().zip(included) {
        match item {
            ContextItem::User { entry_id, content } => {
                for (index, block) in content.iter().enumerate() {
                    let media = match block {
                        ContentBlock::Image { .. } => {
                            Some((InputMediaKind::Image, RequiredCapability::Image))
                        }
                        ContentBlock::Audio { .. } => {
                            Some((InputMediaKind::Audio, RequiredCapability::Audio))
                        }
                        ContentBlock::File { .. } => {
                            Some((InputMediaKind::File, RequiredCapability::File))
                        }
                        _ => None,
                    };
                    if let Some((kind, capability)) = media
                        && !capabilities.input_media.contains(&kind)
                    {
                        required.push(RequiredIncompatibility {
                            entry_id: entry_id.clone(),
                            part: Part::Block {
                                index: index as u32,
                            },
                            capability,
                        });
                    }
                }
            }
            ContextItem::Assistant { entry_id, .. } if !capabilities.tools => {
                for block in &source.blocks {
                    if block.kind == BlockKind::ToolCall {
                        required.push(RequiredIncompatibility {
                            entry_id: entry_id.clone(),
                            part: block.part,
                            capability: RequiredCapability::Tools,
                        });
                    }
                }
            }
            ContextItem::ToolResult { entry_id, .. } if !capabilities.tools => {
                required.push(RequiredIncompatibility {
                    entry_id: entry_id.clone(),
                    part: Part::Entry,
                    capability: RequiredCapability::Tools,
                });
            }
            _ => {}
        }
    }
    required
}

fn budget_for(config: &ContextConfig, model: &ModelRef, estimated: u64, opaque: u64) -> Budget {
    let deductions = Deductions {
        output_reserve: config.output_reserve.into(),
        tool_headroom: config.tool_headroom.into(),
        safety_margin: config.safety_margin.into(),
    };
    let limits = config.catalog.limits(model).copied();
    let Some(context_tokens) = limits.and_then(|limits| limits.context_tokens) else {
        return Budget::Unknown {
            reason: if limits.is_none() {
                UnknownLimit::ModelNotInCatalog
            } else {
                UnknownLimit::ContextLimitNotConfigured
            },
            limits,
            deductions,
            estimated_tokens: estimated,
            unestimated_opaque_bytes: opaque,
        };
    };
    let context_tokens = u64::from(context_tokens);
    let usable = context_tokens.saturating_sub(
        deductions.output_reserve + deductions.tool_headroom + deductions.safety_margin,
    );
    let permille = config.compaction_trigger_permille;
    Budget::Known {
        limits: limits.unwrap_or_default(),
        context_tokens,
        deductions,
        usable_tokens: usable,
        estimated_tokens: estimated,
        headroom_tokens: usable.saturating_sub(estimated),
        overflow_tokens: estimated.saturating_sub(usable),
        compaction_trigger_permille: permille,
        // Both factors are below 2^32, so the product cannot overflow.
        compaction_threshold_tokens: (usable * u64::from(permille)).div_ceil(1000),
        unestimated_opaque_bytes: opaque,
    }
}

/// `(overflow, compaction_recommended)`. An unknown budget sets neither.
fn thresholds(budget: &Budget) -> (bool, bool) {
    match budget {
        Budget::Known {
            usable_tokens,
            estimated_tokens,
            compaction_threshold_tokens,
            ..
        } => (
            estimated_tokens > usable_tokens,
            estimated_tokens >= compaction_threshold_tokens,
        ),
        Budget::Unknown { .. } => (false, false),
    }
}
