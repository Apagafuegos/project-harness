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
        ArtifactRef, AssistantResponse, CallId, Compaction, ContentBlock, EntryId, EntryPayload,
        ModelRef, ResponseOutcome, SessionSnapshot, StoredEntry, ToolResultEntry, UserMessage,
    },
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
        }
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
                walk.summary(summary)?;
            }
            if let EntryPayload::AssistantResponse(response) = &entry.payload
                && response.outcome == ResponseOutcome::Completed
            {
                latest_completed = Some((id, response));
            }
            if let Some(by) = plan.superseded_by.get(id) {
                walk.exclude(
                    id,
                    Part::Entry,
                    ExclusionReason::SupersededSummary,
                    Some(by),
                );
                continue;
            }
            if let Some(by) = plan.covered_by.get(id) {
                walk.exclude(id, Part::Entry, ExclusionReason::CoveredBySummary, Some(by));
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
                // An active summary that no other covers: its item was placed
                // at its first covered entry, which precedes it.
                EntryPayload::Compaction(_) => {}
            }
        }

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
        let preview = walk.preview.then(|| PreviewReport {
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
        let fingerprint = fingerprint(model, &walk.items);
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
            totals,
            budget,
            reported_usage,
            portability: walk.portability,
            preview,
            flags: Flags {
                overflow,
                compaction_recommended,
                preview: model_override.is_some(),
            },
        };
        check::check(&branch, &walk.items, &report).map_err(ContextError::Invariant)?;
        Ok(BuiltContext {
            projection: Projection {
                head: head.cloned(),
                input: ModelInput {
                    model: model.clone(),
                    items: walk.items,
                },
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
        for stored in branch {
            match &stored.entry.payload {
                EntryPayload::Compaction(compaction) => {
                    for old in &compaction.supersedes {
                        superseded_by.entry(old).or_insert(&stored.entry.id);
                    }
                    summaries.push((&stored.entry.id, compaction));
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
        let continuation = match &response.continuation {
            Some(continuation)
                if self.owned(
                    id,
                    Part::Continuation,
                    &continuation.owner,
                    &continuation.schema,
                    &continuation.artifact,
                ) =>
            {
                opaque = opaque.saturating_add(continuation.artifact.bytes);
                blocks.push(IncludedBlock {
                    part: Part::Continuation,
                    kind: BlockKind::Continuation,
                    estimated_tokens: 0,
                    opaque_bytes: continuation.artifact.bytes,
                });
                Some(continuation.clone())
            }
            _ => None,
        };
        self.counts.opaque = self.counts.opaque.saturating_add(opaque);
        self.push(
            ContextItem::Assistant {
                entry_id: id.clone(),
                outcome: response.outcome,
                content,
                continuation,
            },
            IncludedItem {
                entry_id: Some(id.clone()),
                kind: ItemKind::Assistant,
                estimated_tokens: tokens,
                opaque_bytes: opaque,
                blocks,
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
        self.push(
            ContextItem::ToolResult {
                entry_id: id.clone(),
                result: result.result.clone(),
            },
            IncludedItem {
                entry_id: Some(id.clone()),
                kind: ItemKind::ToolResult,
                estimated_tokens: tokens,
                opaque_bytes: 0,
                blocks: Vec::new(),
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
            },
        );
        Ok(())
    }
}

/// The budget of an estimate under `model`'s configured limits.
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
