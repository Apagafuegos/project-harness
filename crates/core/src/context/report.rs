//! The context report: plain data with stable Serde serialization.
//!
//! Field names say what kind of number they hold: `estimated_*` values come
//! from the estimator named in `totals.estimator`, `*_bytes` values are sizes
//! of opaque provider state that no estimate covers, and `reported_usage` is
//! what a provider said about an earlier request. They are never added up.

use serde::{Deserialize, Serialize};

use super::{config::ModelLimits, estimate::EstimatorInfo};
use crate::session::{
    ContextMode, Digest, EntryId, ModelRef, Provenance, ResponseId, SessionId, TurnId, UndoId,
    UndoOutcome, Usage,
};

/// Accounts for every entry on one branch for one projection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextReport {
    /// What was built, from what.
    pub identity: ReportIdentity,
    /// The projection's fingerprint.
    pub fingerprint: Digest,
    /// Whether the configuration supplied instructions.
    pub instructions: InstructionsReport,
    /// One record per projection item, in projection order.
    pub included: Vec<IncludedItem>,
    /// Every excluded entry, block or continuation, in branch order.
    pub excluded: Vec<Exclusion>,
    /// Every compaction summary on the branch that no later one supersedes.
    pub summaries: Vec<SummaryReport>,
    /// Effective non-default controls on the branch, in ancestral order, by
    /// the position of the control entry that sets each turn's mode. Omitted
    /// when no control applies, so a history without controls keeps its bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controls: Vec<ControlReport>,
    /// Recorded undo notes on the branch, in ancestral order. Omitted when the
    /// history has none. Controls stay about pins and exclusions: a filesystem
    /// undo is reported here instead.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undos: Vec<UndoReport>,
    /// Estimated tokens by category, and opaque bytes.
    pub totals: Totals,
    /// The budget under the projection model's limits.
    pub budget: Budget,
    /// Usage reported by the provider for the latest completed response.
    pub reported_usage: Option<ReportedUsage>,
    /// Provider state and continuations excluded for belonging to another model.
    pub portability: Vec<PortabilityLoss>,
    /// What building for another model would change; present only in a preview.
    pub preview: Option<PreviewReport>,
    /// Summary flags.
    pub flags: Flags,
}

/// Identity of the inputs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportIdentity {
    /// Session.
    pub session_id: SessionId,
    /// Journal sequence of the snapshot.
    pub journal_sequence: u64,
    /// Head whose branch was projected; `null` is the empty root.
    pub head: Option<EntryId>,
    /// Model the projection is for.
    pub model: ModelRef,
    /// Configuration artifact the instructions came from.
    pub configuration: Digest,
    /// Version of the builder's report and checks.
    pub builder_version: u32,
    /// Version of the inclusion rules, hashed into the fingerprint.
    pub projection_version: u32,
}

/// Whether instructions were configured.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum InstructionsReport {
    /// The configuration has no `instructions` field; no item was added.
    Absent,
    /// The field is an empty array; no item was added.
    Empty,
    /// The first projection item holds them.
    Included {
        /// Number of instructions.
        count: u64,
        /// Their estimated tokens.
        estimated_tokens: u64,
    },
}

/// Kinds of projection items.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// Configured instructions.
    Instructions,
    /// User message.
    User,
    /// Assistant response.
    Assistant,
    /// Tool result.
    ToolResult,
    /// Compaction summary.
    Summary,
}

/// One projection item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncludedItem {
    /// Source entry; `null` for instructions.
    pub entry_id: Option<EntryId>,
    /// Item kind.
    pub kind: ItemKind,
    /// Estimated tokens of the whole item.
    pub estimated_tokens: u64,
    /// Bytes of opaque provider state in the item, not estimated.
    pub opaque_bytes: u64,
    /// For assistant items, each included block and continuation. Their
    /// excluded siblings are in `excluded`.
    pub blocks: Vec<IncludedBlock>,
    /// For tool results only: `executor`, `runtime`, `user_observed` or
    /// `user_acknowledged_unknown`. Omitted for other items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    /// The item belongs to a turn whose effective mode is `pinned`. Omitted
    /// when false, so a projection without pins keeps its bytes. The
    /// projection itself is unchanged: pinning changes no item, so it changes
    /// no fingerprint.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// Parts included as deterministic text under explicit ConvertSupported.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversions: Vec<Conversion>,
}

/// Which part of an entry a decision is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "part", rename_all = "snake_case")]
pub enum Part {
    /// The whole entry.
    Entry,
    /// One content block, by its index in the entry.
    Block {
        /// Block index.
        index: u32,
    },
    /// An assistant response's provider continuation.
    Continuation,
}

/// Kinds of included assistant blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// Text.
    Text,
    /// Image.
    Image,
    /// Audio.
    Audio,
    /// File.
    File,
    /// Tool call with its result included.
    ToolCall,
    /// Provider state owned by the projection's model.
    ProviderState,
    /// Continuation owned by the projection's model.
    Continuation,
}

/// One included part of an assistant item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IncludedBlock {
    /// Block or continuation.
    #[serde(flatten)]
    pub part: Part,
    /// Kind.
    pub kind: BlockKind,
    /// Estimated tokens; zero for opaque state.
    pub estimated_tokens: u64,
    /// Opaque bytes; zero for everything else.
    pub opaque_bytes: u64,
}

/// The closed set of reasons for leaving something out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionReason {
    /// A tool call with no result on this branch: from a cancelled or
    /// interrupted response, or still unanswered at the selected head.
    OrphanToolCall,
    /// A failed response.
    FailedResponse,
    /// A reasoning summary; display-only, never replayed.
    ReasoningDisplayOnly,
    /// Provider state or a continuation owned by another model.
    ProviderStateNotPortable,
    /// A continuation from a response other than the last included assistant.
    ContinuationNotLatest,
    /// The current rebuilt input at the response parent no longer matches.
    ContinuationPrefixMismatch,
    /// A `model_changed` entry; it selects the model and is not dialogue.
    ModelChangeMarker,
    /// A response with no blocks and no continuation: nothing to replay.
    EmptyResponse,
    /// A summary superseded by a later summary on the branch.
    SupersededSummary,
    /// An entry replaced by a summary.
    CoveredBySummary,
    /// Every entry of a turn the user excluded, by a `context_control`.
    UserExcluded,
    /// A `context_control` entry; it selects a turn's mode, not dialogue.
    ContextControlMarker,
}

/// One turn whose effective mode is not `default`, on this branch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlReport {
    /// The controlled turn.
    pub turn_id: TurnId,
    /// Its effective mode: `pinned` or `excluded`.
    pub mode: ContextMode,
    /// The control entry that sets it, the last one for this turn before the
    /// head.
    pub control_entry_id: EntryId,
}

/// One recorded undo note on the branch, as the report lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndoReport {
    /// The undo identity.
    pub undo_id: UndoId,
    /// The turn whose trusted file edits were reverted, or not.
    pub target_turn_id: TurnId,
    /// The `undo_note` entry the projection includes.
    pub note_entry_id: EntryId,
    /// How the undo ended.
    pub outcome: UndoOutcome,
    /// Canonical paths in the note's own order.
    pub paths: Vec<String>,
}

/// One excluded entry, block or continuation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Exclusion {
    /// Source entry.
    pub entry_id: EntryId,
    /// Which part.
    #[serde(flatten)]
    pub part: Part,
    /// Why.
    pub reason: ExclusionReason,
    /// The covering summary for `covered_by_summary`, or the superseding one
    /// for `superseded_summary`; otherwise `null`.
    pub summary_id: Option<EntryId>,
    /// Present only when `summary_id` is itself covered or superseded: the
    /// summary that stands for this entry in the projection, found by
    /// following those links until one is a projection item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_summary: Option<EntryId>,
}

/// One summary on the branch that no later summary supersedes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryReport {
    /// The compaction entry.
    pub entry_id: EntryId,
    /// Whether it is a projection item. False only when a later summary
    /// covers this summary's entry.
    pub included: bool,
    /// Strategy.
    pub strategy: String,
    /// Strategy version.
    pub strategy_version: String,
    /// Entries it replaces, in branch order.
    pub covered_entry_ids: Vec<EntryId>,
    /// Summaries it supersedes.
    pub supersedes: Vec<EntryId>,
    /// Estimated tokens of its text.
    pub estimated_tokens: u64,
}

/// Estimated tokens of the projection by category.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Totals {
    /// The estimator that made every figure here.
    pub estimator: EstimatorInfo,
    /// Configured instructions.
    pub estimated_instructions: u64,
    /// User text.
    pub estimated_user: u64,
    /// Assistant text.
    pub estimated_assistant_text: u64,
    /// Tool-call names and arguments.
    pub estimated_tool_calls: u64,
    /// Tool-result text.
    pub estimated_tool_results: u64,
    /// Summary text.
    pub estimated_summaries: u64,
    /// Image, audio and file blocks anywhere.
    pub estimated_media: u64,
    /// Sum of the categories above.
    pub estimated_total: u64,
    /// Included provider state and continuations, in bytes. Not estimated.
    pub opaque_provider_state_bytes: u64,
}

/// Why a budget is unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownLimit {
    /// The model has no catalog entry.
    ModelNotInCatalog,
    /// The catalog entry has no context limit.
    ContextLimitNotConfigured,
}

/// The configured deductions, shown in every budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deductions {
    /// Output reserve.
    pub output_reserve: u64,
    /// Tool headroom.
    pub tool_headroom: u64,
    /// Safety margin.
    pub safety_margin: u64,
}

/// `usable = context_tokens - output_reserve - tool_headroom - safety_margin`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Budget {
    /// The context limit is configured.
    Known {
        /// The model's catalog entry.
        limits: ModelLimits,
        /// Configured context limit.
        context_tokens: u64,
        /// Deductions.
        deductions: Deductions,
        /// The limit minus deductions, or zero if they exceed it.
        usable_tokens: u64,
        /// Estimated tokens of the projection.
        estimated_tokens: u64,
        /// `usable - estimated`, or zero.
        headroom_tokens: u64,
        /// `estimated - usable`, or zero. Nonzero means overflow.
        overflow_tokens: u64,
        /// Configured trigger, in thousandths of usable.
        compaction_trigger_permille: u32,
        /// `ceil(usable * trigger / 1000)`.
        compaction_threshold_tokens: u64,
        /// Opaque provider-state bytes that no figure above accounts for.
        unestimated_opaque_bytes: u64,
    },
    /// No context limit is configured: no overflow check, no compaction flag.
    /// The core fails a turn in this state with `context_budget_unknown`.
    Unknown {
        /// Why.
        reason: UnknownLimit,
        /// The catalog entry, if the model has one.
        limits: Option<ModelLimits>,
        /// Deductions that would apply.
        deductions: Deductions,
        /// Estimated tokens of the projection.
        estimated_tokens: u64,
        /// Opaque provider-state bytes that no figure above accounts for.
        unestimated_opaque_bytes: u64,
    },
}

/// What the provider reported for an earlier request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportedUsage {
    /// Always `provider_reported`: these figures are not estimates and
    /// describe the request that produced `entry_id`, not this projection.
    pub basis: UsageBasis,
    /// The latest completed response on the branch.
    pub entry_id: EntryId,
    /// Its response identity.
    pub response_id: ResponseId,
    /// Its model.
    pub model: ModelRef,
    /// The recorded usage.
    pub usage: Usage,
}

/// Label of reported usage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageBasis {
    /// Reported by the provider.
    ProviderReported,
}

/// One piece of provider state or continuation and its owner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PortabilityLoss {
    /// Source response.
    pub entry_id: EntryId,
    /// Block or continuation.
    #[serde(flatten)]
    pub part: Part,
    /// Owning model.
    pub owner: ModelRef,
    /// Native schema name.
    pub schema: String,
    /// Artifact size. The payload is never disclosed.
    pub bytes: u64,
}

/// A required projected part that cannot be omitted by a switch policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredIncompatibility {
    /// Source entry.
    pub entry_id: EntryId,
    /// Source part (tool results name the whole entry).
    #[serde(flatten)]
    pub part: Part,
    /// Unsupported feature, never content or native payload.
    pub capability: RequiredCapability,
}

/// Features required by projected dialogue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredCapability {
    /// User image.
    Image,
    /// User audio.
    Audio,
    /// User file.
    File,
    /// Tool call or result.
    Tools,
}

/// A displayable part included as text, or proposed for conversion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Conversion {
    /// Original journal entry.
    pub entry_id: EntryId,
    /// Original journal part.
    pub part: Part,
    /// Target capability that is missing.
    pub capability: RequiredCapability,
    /// Deterministic rendering used instead.
    pub kind: ConversionKind,
}

/// Closed conversion rules; opaque native state has no conversion kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversionKind {
    /// Stored media metadata only.
    MediaReference,
    /// Bounded compact JSON call.
    ToolCallText,
    /// Bounded result text with name and status.
    ToolResultText,
}

/// What switching from the head's model to the preview model would change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewReport {
    /// Required dialogue the target cannot accept. Neither policy may omit it.
    pub required_incompatibilities: Vec<RequiredIncompatibility>,
    /// Displayable parts convertible under explicit ConvertSupported.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub would_convert: Vec<Conversion>,
    /// The model in effect at the head.
    pub model_at_head: ModelRef,
    /// The model the projection was built for.
    pub preview_model: ModelRef,
    /// Portable under the head's model, excluded under the preview model.
    pub would_stop_being_portable: Vec<PortabilityLoss>,
    /// Excluded under the head's model, portable under the preview model.
    pub would_become_portable: Vec<PortabilityLoss>,
    /// The same projection estimate under the head model's limits.
    pub budget_at_head: Budget,
}

/// Summary flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flags {
    /// The budget is unknown. Inspection reports it; the core refuses to
    /// infer with it and fails the turn with `context_budget_unknown`.
    pub budget_unknown: bool,
    /// The estimate exceeds the usable budget. Never set for an unknown budget.
    pub overflow: bool,
    /// The estimate is at or above the compaction threshold. Nothing acts on
    /// it until summaries are generated.
    pub compaction_recommended: bool,
    /// Built under a model override; never sent to a provider.
    pub preview: bool,
}
