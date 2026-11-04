//! Persisted wire types for every supported record version. Storage validates
//! each record against its own version's bundled schema; fields a version lacks
//! read as `None`.

use super::identity::*;
use serde::{Deserialize, Serialize};

/// Persisted artifact fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    /// Sha256.
    pub sha256: Digest,
    /// Bytes.
    pub bytes: u64,
}

/// Persisted model fields. Ordered so it can key a model catalog.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRef {
    /// Provider instance.
    pub provider_instance: String,
    /// Model.
    pub model: String,
}

/// Persisted call ref fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallRef {
    /// Response entry id.
    pub response_entry_id: EntryId,
    /// Call id.
    pub call_id: CallId,
}

/// Persisted continuation fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderContinuation {
    /// Owner.
    pub owner: ModelRef,
    /// Schema.
    pub schema: String,
    /// Artifact.
    pub artifact: ArtifactRef,
    /// Input fingerprint.
    pub input_fingerprint: Digest,
}

/// Persisted usage fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: Option<u64>,
    /// Output tokens.
    pub output_tokens: Option<u64>,
    /// Cached input tokens.
    pub cached_input_tokens: Option<u64>,
    /// Reasoning tokens.
    pub reasoning_tokens: Option<u64>,
}

/// Persisted model error fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelError {
    /// Code.
    pub code: ModelErrorCode,
    /// Message.
    pub message: String,
    /// Provider code.
    pub provider_code: Option<String>,
    /// Retry after ms.
    pub retry_after_ms: Option<u64>,
}

/// Persisted tool metadata fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolMetadata {
    /// Exit code.
    pub exit_code: Option<i32>,
    /// Duration ms.
    pub duration_ms: Option<u64>,
    /// Output truncated.
    pub output_truncated: bool,
    /// Provenance.
    pub provenance: Provenance,
    /// Artifacts.
    pub artifacts: Vec<ArtifactRef>,
    /// Machine-readable reason, such as `tools_unavailable`; v2 only. `None`
    /// on a succeeded or user-provided result, and on every v1 result. Readers
    /// keep any code the schema accepts; writers take one from [`ResultCode`].
    #[serde(default)]
    pub code: Option<String>,
    /// What the executor observed about the rename commit point of a trusted
    /// write/edit; v4 only. `None` on every other call, and on every result of
    /// an earlier version, where the effect is unavailable rather than absent.
    #[serde(default)]
    pub file_effect: Option<FileEffect>,
}

/// Persisted tool result value fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    /// Call.
    pub call: CallRef,
    /// Invocation id.
    pub invocation_id: Option<InvocationId>,
    /// Status.
    pub status: ToolStatus,
    /// Content.
    pub content: Vec<ContentBlock>,
    /// Metadata.
    pub metadata: ToolMetadata,
}

/// Persisted user message fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserMessage {
    /// Content.
    pub content: Vec<ContentBlock>,
}

/// Persisted assistant response fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantResponse {
    /// Response id.
    pub response_id: ResponseId,
    /// Model.
    pub model: ModelRef,
    /// Outcome.
    pub outcome: ResponseOutcome,
    /// Finish reason.
    pub finish_reason: Option<FinishReason>,
    /// Content.
    pub content: Vec<ContentBlock>,
    /// Usage.
    pub usage: Option<Usage>,
    /// Continuation.
    pub continuation: Option<ProviderContinuation>,
    /// Error.
    pub error: Option<ModelError>,
    /// Fingerprint of the context projection the inference was sent; v2 only.
    /// `None` on every v1 response, and on a response not produced from a
    /// local projection.
    #[serde(default)]
    pub projection_fingerprint: Option<Digest>,
}

/// Persisted tool result fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultEntry {
    /// Result.
    pub result: ToolResult,
}

/// Persisted model changed fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChanged {
    /// Model.
    pub model: ModelRef,
    /// Portability.
    pub portability: Portability,
}

/// Persisted compaction fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compaction {
    /// Strategy.
    pub strategy: String,
    /// Strategy version.
    pub strategy_version: String,
    /// Covered entry ids.
    pub covered_entry_ids: Vec<EntryId>,
    /// Source fingerprint.
    pub source_fingerprint: Digest,
    /// Source manifest.
    pub source_manifest: ArtifactRef,
    /// Supersedes.
    pub supersedes: Vec<EntryId>,
    /// Summary.
    pub summary: String,
    /// Model that generated the summary; v3 only. `None` means no model was
    /// used, as for a deterministic strategy, and is encoded by omitting the
    /// field rather than writing null. Readers of v1 and v2 always see `Some`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// Configuration fingerprint.
    pub configuration_fingerprint: Digest,
}

/// Persisted context-control fields: a durable, append-only statement of the
/// mode one finished turn has on the branch it is appended to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextControl {
    /// The finished turn this control applies to. The entry's own `turn_id`
    /// stays `None`: a control is not dialogue.
    pub turn_id: TurnId,
    /// The effective mode this control sets for that turn.
    pub mode: ContextMode,
}

/// Persisted entry fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Id.
    pub id: EntryId,
    /// Parent id.
    pub parent_id: Option<EntryId>,
    /// Turn id.
    pub turn_id: Option<TurnId>,
    /// Payload.
    pub payload: EntryPayload,
}

/// Persisted head change fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadChange {
    /// From.
    pub from: Option<EntryId>,
    /// To.
    pub to: Option<EntryId>,
    /// Reason.
    pub reason: HeadReason,
}

/// Persisted turn started fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnStarted {
    /// Turn id.
    pub turn_id: TurnId,
    /// User entry id.
    pub user_entry_id: EntryId,
    /// Configuration.
    pub configuration: ArtifactRef,
}

/// Persisted state changed fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateChanged {
    /// Turn id.
    pub turn_id: TurnId,
    /// From.
    pub from: RuntimeState,
    /// To.
    pub to: RuntimeState,
}

/// Persisted approval requested fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequested {
    /// Turn id.
    pub turn_id: TurnId,
    /// Approval id.
    pub approval_id: ApprovalId,
    /// Call.
    pub call: CallRef,
    /// Plan digest.
    pub plan_digest: Digest,
    /// Plan.
    pub plan: ArtifactRef,
    /// Policy revision.
    pub policy_revision: String,
    /// Expires at.
    pub expires_at: Timestamp,
}

/// Persisted authorization decided fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationDecided {
    /// Turn id.
    pub turn_id: TurnId,
    /// Decision id.
    pub decision_id: DecisionId,
    /// Call.
    pub call: CallRef,
    /// Approval id.
    pub approval_id: Option<ApprovalId>,
    /// Plan digest.
    pub plan_digest: Digest,
    /// Plan.
    pub plan: ArtifactRef,
    /// Policy revision.
    pub policy_revision: String,
    /// Decision.
    pub decision: Decision,
    /// Reason.
    pub reason: String,
}

/// Persisted execution intent fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionIntent {
    /// Turn id.
    pub turn_id: TurnId,
    /// Invocation id.
    pub invocation_id: InvocationId,
    /// Call.
    pub call: CallRef,
    /// Decision id.
    pub decision_id: DecisionId,
    /// Plan digest.
    pub plan_digest: Digest,
    /// Backend.
    pub backend: ExecutionBackend,
    /// Timeout ms.
    pub timeout_ms: u64,
    /// Effects the plan declares; v2 only. `None` means the intent was
    /// recorded before v2 and its effects are unknown, so it must never be
    /// treated as read-only. Writing an intent requires a set.
    #[serde(default)]
    pub effects: Option<EffectSet>,
    /// The bound pre-image capture of a trusted write/edit; v4 only. `None`
    /// on every other call, and on every intent of an earlier version, which
    /// has no stored pre-image and so cannot be undone.
    #[serde(default)]
    pub file_image: Option<FileImage>,
}

/// One trusted write/edit target captured before dispatch. Every key is
/// present in v4; `pre_image` and `pre_mode` are null exactly when the target
/// was absent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileImage {
    /// Canonical workspace path from the bound plan.
    pub path: String,
    /// Exact pre-image bytes, or null for a created file.
    pub pre_image: Option<ArtifactRef>,
    /// Ordinary permission bits of the pre-image, or null when absent.
    pub pre_mode: Option<u32>,
    /// SHA-256 of the exact replacement bytes this execution will write.
    pub post_image: Digest,
}

/// One applied trusted file mutation the user asked to undo. `pre_image` and
/// `pre_mode` come from the first mutation of `path` in the turn and
/// `post_image` from the last; `invocation_ids` is nonempty and in execution
/// order, so adjacent images must chain exactly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndoTarget {
    /// Canonical workspace path of every mutation in this target.
    pub path: String,
    /// Applied trusted mutations of `path` in the undon turn, in order.
    pub invocation_ids: Vec<InvocationId>,
    /// Pre-image of the first mutation, or null when it created the file.
    pub pre_image: Option<ArtifactRef>,
    /// Ordinary permission bits to restore, or null when it created the file.
    pub pre_mode: Option<u32>,
    /// Post-image of the last mutation, which the current file must still have.
    pub post_image: Digest,
}

/// The durable intent of one undo, committed before any target is touched.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndoIntent {
    /// Undo identity; unique in the journal's identity namespace.
    pub undo_id: UndoId,
    /// Finished turn whose trusted file mutations are reverted.
    pub target_turn_id: TurnId,
    /// Head the undo was selected and confirmed at.
    pub anchor_head: Option<EntryId>,
    /// Session configuration in effect, which may not change until it resolves.
    pub configuration: ArtifactRef,
    /// Authorization policy revision in effect.
    pub policy_revision: String,
    /// Targets to restore, unique by path and sorted bytewise.
    pub targets: Vec<UndoTarget>,
    /// Calls the undo does not reverse, in ancestral proposal order.
    pub omitted_calls: Vec<CallRef>,
}

/// Recorded when an undo may have changed some target but cannot prove what.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndoUncertain {
    /// The interrupted undo.
    pub undo_id: UndoId,
    /// Why the outcome is unknown, bounded to 1 KiB.
    pub reason: String,
}

/// The terminal result of one undo, always committed with its note entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndoResult {
    /// The resolved undo.
    pub undo_id: UndoId,
    /// How the undo ended.
    pub outcome: UndoOutcome,
    /// Exactly one row per intent target, in target order.
    pub file_outcomes: Vec<UndoFileOutcome>,
    /// The `undo_note` entry committed with this result.
    pub note_entry_id: EntryId,
    /// The user's statement for an observed or acknowledged-unknown outcome.
    pub resolution_note: Option<String>,
}

/// One target's outcome, by its canonical path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndoFileOutcome {
    /// Canonical workspace path of the intent target.
    pub path: String,
    /// What happened to it.
    pub outcome: FileOutcome,
}

/// The recorded user-visible note of one undo, projected as a user message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UndoNote {
    /// The resolved undo.
    pub undo_id: UndoId,
    /// The turn that was the subject of the undo.
    pub target_turn_id: TurnId,
    /// How the undo ended; equals its result's outcome.
    pub outcome: UndoOutcome,
    /// Deterministic text, exactly as the builder projects it.
    pub text: String,
}

/// Persisted execution uncertain fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionUncertain {
    /// Turn id.
    pub turn_id: TurnId,
    /// Invocation id.
    pub invocation_id: InvocationId,
    /// Reason.
    pub reason: String,
}

/// Persisted recovery resolved fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryResolved {
    /// Turn id.
    pub turn_id: TurnId,
    /// Invocation id.
    pub invocation_id: InvocationId,
    /// Result entry id.
    pub result_entry_id: EntryId,
    /// Resolution.
    pub resolution: Resolution,
    /// Note.
    pub note: String,
}

/// Persisted turn finished fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnFinished {
    /// Turn id.
    pub turn_id: TurnId,
    /// Outcome.
    pub outcome: TurnOutcome,
    /// Code.
    pub code: Option<String>,
}

/// Persisted configuration changed fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationChanged {
    /// Configuration.
    pub configuration: ArtifactRef,
    /// Policy revision.
    pub policy_revision: String,
}

/// Persisted tail repaired fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TailRepaired {
    /// Original size.
    pub original_size: u64,
    /// Valid bytes.
    pub valid_bytes: u64,
    /// Quarantine artifact.
    pub quarantine_artifact: ArtifactRef,
}

/// Persisted session created fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionHeader {
    /// Workspace.
    pub workspace: String,
    /// Initial model.
    pub initial_model: ModelRef,
    /// Policy revision.
    pub policy_revision: String,
    /// Configuration.
    pub configuration: ArtifactRef,
}

/// Persisted commit fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commit {
    /// Command id.
    pub command_id: Option<CommandId>,
    /// Command fingerprint.
    pub command_fingerprint: Option<Digest>,
    /// Head.
    pub head: HeadChange,
    /// Entries.
    pub entries: Vec<Entry>,
    /// Events.
    pub events: Vec<JournalEvent>,
}

/// Persisted record fields.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// Version.
    pub version: u32,
    /// Seq.
    pub seq: u64,
    /// Record id.
    pub record_id: RecordId,
    /// Session id.
    pub session_id: SessionId,
    /// At.
    pub at: Timestamp,
    /// Previous checksum.
    pub previous_checksum: Option<Digest>,
    /// Payload.
    pub payload: RecordPayload,
}

/// Ordered block variants.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlock {
    /// Text content.
    Text {
        /// Text.
        text: String,
    },
    /// Image content.
    Image {
        /// Artifact.
        artifact: ArtifactRef,
        /// Mime type.
        mime_type: String,
    },
    /// Audio content.
    Audio {
        /// Artifact.
        artifact: ArtifactRef,
        /// Mime type.
        mime_type: String,
    },
    /// File content.
    File {
        /// Artifact.
        artifact: ArtifactRef,
        /// Mime type.
        mime_type: String,
        /// Name.
        name: String,
    },
    /// Reasoning summary content.
    ReasoningSummary {
        /// Text.
        text: String,
    },
    /// Tool call content.
    ToolCall {
        /// Call id.
        call_id: CallId,
        /// Name.
        name: String,
        /// Arguments.
        arguments: serde_json::Map<String, serde_json::Value>,
        /// Provider call id.
        provider_call_id: Option<String>,
    },
    /// Provider state content.
    ProviderState {
        /// Owner.
        owner: ModelRef,
        /// Schema.
        schema: String,
        /// Artifact.
        artifact: ArtifactRef,
    },
}

/// Ordered journal event variants.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum JournalEvent {
    /// Turn started audit event.
    TurnStarted(TurnStarted),
    /// State changed audit event.
    StateChanged(StateChanged),
    /// Approval requested audit event.
    ApprovalRequested(ApprovalRequested),
    /// Authorization decided audit event.
    AuthorizationDecided(AuthorizationDecided),
    /// Execution intent audit event.
    ExecutionIntent(ExecutionIntent),
    /// Execution uncertain audit event.
    ExecutionUncertain(ExecutionUncertain),
    /// Recovery resolved audit event.
    RecoveryResolved(RecoveryResolved),
    /// Turn finished audit event.
    TurnFinished(TurnFinished),
    /// Configuration changed audit event.
    ConfigurationChanged(ConfigurationChanged),
    /// Tail repaired audit event.
    TailRepaired(TailRepaired),
    /// Undo intent audit event.
    UndoIntent(UndoIntent),
    /// Undo uncertainty audit event.
    UndoUncertain(UndoUncertain),
    /// Undo result audit event.
    UndoResult(UndoResult),
}

/// Persisted EntryPayload variants.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryPayload {
    /// User message.
    UserMessage(UserMessage),
    /// Assistant response.
    AssistantResponse(AssistantResponse),
    /// Tool result.
    ToolResult(ToolResultEntry),
    /// Model changed.
    ModelChanged(ModelChanged),
    /// Compaction.
    Compaction(Compaction),
    /// Context control.
    ContextControl(ContextControl),
    /// A recorded undo note: one user message that corrects the model's
    /// assumptions without rewriting the reverted history.
    UndoNote(UndoNote),
}

/// Persisted RecordPayload variants.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordPayload {
    /// Session created.
    SessionCreated(SessionHeader),
    /// Commit.
    Commit(Commit),
}

/// Persisted ModelErrorCode values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorCode {
    /// Invalid request.
    InvalidRequest,
    /// Unsupported capability.
    UnsupportedCapability,
    /// Authentication.
    Authentication,
    /// Rate limit.
    RateLimit,
    /// Transport.
    Transport,
    /// Provider failure.
    ProviderFailure,
    /// Malformed stream.
    MalformedStream,
    /// Limit exceeded.
    LimitExceeded,
    /// Context overflow.
    ContextOverflow,
}

/// Persisted Provenance values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Executor.
    Executor,
    /// Runtime.
    Runtime,
    /// User observed.
    UserObserved,
    /// User acknowledged unknown.
    UserAcknowledgedUnknown,
}

/// Persisted ToolStatus values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Succeeded.
    Succeeded,
    /// Failed.
    Failed,
    /// Invalid arguments.
    InvalidArguments,
    /// Denied.
    Denied,
    /// Timed out.
    TimedOut,
    /// Cancelled.
    Cancelled,
    /// Infrastructure failure.
    InfrastructureFailure,
    /// Uncertain.
    Uncertain,
}

/// Persisted ResponseOutcome values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseOutcome {
    /// Completed.
    Completed,
    /// Failed.
    Failed,
    /// Cancelled.
    Cancelled,
    /// Interrupted.
    Interrupted,
}

/// Persisted FinishReason values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Stop.
    Stop,
    /// Tool calls.
    ToolCalls,
    /// Output limit.
    OutputLimit,
    /// Refusal.
    Refusal,
}

/// One turn's effective context mode on a branch. The last control for a turn
/// in the selected ancestry wins; `default` is the absence of a control.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    /// Included by the normal inclusion rules.
    #[default]
    Default,
    /// Kept verbatim: included by the normal rules and never summarized.
    Pinned,
    /// Left out of the request; still recorded, reported and displayed.
    Excluded,
}

impl ContextMode {
    /// Whether this is the mode a turn has with no control in effect.
    pub fn is_default(self) -> bool {
        self == Self::Default
    }
}

/// Persisted Portability values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Portability {
    /// Reject.
    Reject,
    /// Omit optional.
    OmitOptional,
    /// Convert supported.
    ConvertSupported,
}

/// Persisted HeadReason values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadReason {
    /// Advance.
    Advance,
    /// Navigate.
    Navigate,
    /// Retain.
    Retain,
}

/// Persisted Decision values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Allow.
    Allow,
    /// Deny.
    Deny,
}

/// A declared effect class, ordered as the tools contract lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    /// Reads workspace files.
    Read,
    /// Creates, changes or removes workspace files.
    FilesystemMutation,
    /// Runs a process.
    Process,
    /// Uses the network.
    Network,
    /// Has an effect outside the workspace and the sandbox.
    ExternalSideEffect,
}

/// A nonempty set of effect classes. [`EffectSet::new`] orders it canonically,
/// so one set has one byte form. A decoded set keeps its stored order, and
/// storage rejects one that is not canonical.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EffectSet(Vec<EffectClass>);
impl EffectSet {
    /// The canonical set of `effects`, or `None` if there are none.
    pub fn new(effects: impl IntoIterator<Item = EffectClass>) -> Option<Self> {
        let mut effects: Vec<_> = effects.into_iter().collect();
        effects.sort_unstable();
        effects.dedup();
        (!effects.is_empty()).then_some(Self(effects))
    }
    /// The classes, in stored order.
    pub fn as_slice(&self) -> &[EffectClass] {
        &self.0
    }
    /// Whether the set declares `effect`.
    pub fn contains(&self, effect: EffectClass) -> bool {
        self.0.contains(&effect)
    }
    /// Strictly increasing in the contract's order: sorted, without duplicates.
    pub(crate) fn is_canonical(&self) -> bool {
        self.0.windows(2).all(|pair| pair[0] < pair[1])
    }
}

/// The machine-readable result codes this build writes. The schema accepts
/// any code matching `^[a-z][a-z0-9_]{0,63}$`, so a later code needs no new
/// schema version; [`ResultCode::as_str`] is the only way a writer names one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultCode {
    /// No tool runs in this build, so the runtime denied the call.
    ToolsUnavailable,
    /// The turn was cancelled before the call started.
    Cancelled,
    /// The process stopped before the call started.
    ProcessRestart,
    /// No compiled-in definition has this name.
    UnknownTool,
    /// Session configuration disables this tool.
    ToolDisabled,
    /// Schema or semantic validation failed.
    InvalidArguments,
    /// The configured policy prohibits the operation.
    PolicyDenied,
    /// A protected target or directory prohibits the operation.
    ProtectedPath,
    /// Subprocess networking is unsupported.
    NetworkUnsupported,
    /// A required workspace object is absent.
    NotFound,
    /// A file target is not regular.
    NotRegularFile,
    /// Multiply linked files are unsupported.
    HardLinkRejected,
    /// A no-overwrite target exists.
    AlreadyExists,
    /// Literal edit matched nothing.
    NoMatch,
    /// Literal edit matched more than once.
    MultipleMatches,
    /// File contents are not UTF-8.
    NotUtf8,
    /// Arguments or workspace contents exceed limits.
    TooLarge,
    /// A trusted infrastructure component violated its contract.
    InfrastructureFailure,
    /// The target changed after the plan was approved.
    StalePlan,
    /// A path component is a symbolic link.
    SymlinkRejected,
    /// The operation exceeded its deadline.
    Timeout,
    /// The process exited unsuccessfully or was killed by a signal.
    NonzeroExit,
    /// The process exceeded the output capture limit.
    OutputLimit,
    /// The required sandbox could not be established.
    SandboxUnavailable,
    /// The user denied the approval.
    ApprovalDenied,
    /// The approval request expired unanswered.
    ApprovalExpired,
    /// A policy revision invalidated the pending approval.
    ApprovalInvalidated,
}
// One list generates both the slice and its exhaustiveness check. Adding an
// enum variant without adding it here is a compile error, not a lookup panic.
const fn all_result_codes(code: ResultCode) -> &'static [ResultCode] {
    macro_rules! codes {
        ($($variant:ident),+ $(,)?) => {
            match code {
                $(ResultCode::$variant)|+ => &[$(ResultCode::$variant),+],
            }
        };
    }
    codes! {
        ToolsUnavailable, Cancelled, ProcessRestart, UnknownTool, ToolDisabled,
        InvalidArguments, PolicyDenied, ProtectedPath, NetworkUnsupported,
        NotFound, NotRegularFile, HardLinkRejected, AlreadyExists, NoMatch,
        MultipleMatches, NotUtf8, TooLarge, InfrastructureFailure, StalePlan,
        SymlinkRejected, Timeout, NonzeroExit, OutputLimit, SandboxUnavailable,
        ApprovalDenied, ApprovalExpired, ApprovalInvalidated,
    }
}

impl ResultCode {
    /// Every result code this build writes, complete by construction.
    pub const ALL: &'static [Self] = all_result_codes(Self::ToolsUnavailable);

    /// Parse an exact persisted spelling; unknown or future codes stay unknown.
    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|value| value.as_str() == code)
    }

    /// The persisted spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolsUnavailable => "tools_unavailable",
            Self::Cancelled => "cancelled",
            Self::ProcessRestart => "process_restart",
            Self::UnknownTool => "unknown_tool",
            Self::ToolDisabled => "tool_disabled",
            Self::InvalidArguments => "invalid_arguments",
            Self::PolicyDenied => "policy_denied",
            Self::ProtectedPath => "protected_path",
            Self::NetworkUnsupported => "network_unsupported",
            Self::NotFound => "not_found",
            Self::NotRegularFile => "not_regular_file",
            Self::HardLinkRejected => "hard_link_rejected",
            Self::AlreadyExists => "already_exists",
            Self::NoMatch => "no_match",
            Self::MultipleMatches => "multiple_matches",
            Self::NotUtf8 => "not_utf8",
            Self::TooLarge => "too_large",
            Self::InfrastructureFailure => "infrastructure_failure",
            Self::StalePlan => "stale_plan",
            Self::SymlinkRejected => "symlink_rejected",
            Self::Timeout => "timeout",
            Self::NonzeroExit => "nonzero_exit",
            Self::OutputLimit => "output_limit",
            Self::SandboxUnavailable => "sandbox_unavailable",
            Self::ApprovalDenied => "approval_denied",
            Self::ApprovalExpired => "approval_expired",
            Self::ApprovalInvalidated => "approval_invalidated",
        }
    }
}

/// Persisted ExecutionBackend values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionBackend {
    /// Trusted fs.
    TrustedFs,
    /// Bubblewrap.
    Bubblewrap,
}

/// Persisted FileEffect values: what an executor observed about a mutation's
/// rename commit point. Never inferred from a result's status or text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileEffect {
    /// The commit point was provably not reached.
    NotApplied,
    /// The rename committed, even if a later step failed.
    Applied,
    /// The outcome could not be determined.
    Unknown,
}

/// Persisted UndoOutcome values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UndoOutcome {
    /// Every target was restored or removed.
    Succeeded,
    /// Provably nothing was restored, so nothing was consumed.
    NotApplied,
    /// The user stated what an interrupted undo left behind.
    Observed,
    /// The user accepted that an interrupted undo's effects are unknown.
    AcknowledgedUnknown,
}
impl UndoOutcome {
    /// Whether this outcome consumes the target turn: every outcome but a
    /// provable no-op means the user will not be offered that turn again.
    pub fn consumes(self) -> bool {
        self != Self::NotApplied
    }
    /// The persisted spelling, which the note text also uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::NotApplied => "not_applied",
            Self::Observed => "observed",
            Self::AcknowledgedUnknown => "acknowledged_unknown",
        }
    }
}

/// Persisted FileOutcome values: one restored, removed, unchanged or unknown
/// target of an undo, in the executor's or the user's observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOutcome {
    /// The pre-image bytes were written back.
    Restored,
    /// A created file was removed.
    Removed,
    /// The target was left exactly as it was.
    Unchanged,
    /// The user observed the pre-image restored.
    ObservedRestored,
    /// The user observed the created file gone.
    ObservedRemoved,
    /// The user observed something else.
    ObservedOther,
    /// Neither the user nor the executor could tell.
    Unknown,
}
impl FileOutcome {
    /// The persisted spelling, which the note text also uses.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Restored => "restored",
            Self::Removed => "removed",
            Self::Unchanged => "unchanged",
            Self::ObservedRestored => "observed_restored",
            Self::ObservedRemoved => "observed_removed",
            Self::ObservedOther => "observed_other",
            Self::Unknown => "unknown",
        }
    }
}

/// Persisted Resolution values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Observed.
    Observed,
    /// Acknowledged unknown.
    AcknowledgedUnknown,
}

/// Persisted TurnOutcome values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    /// Completed.
    Completed,
    /// Failed.
    Failed,
    /// Cancelled.
    Cancelled,
    /// Interrupted.
    Interrupted,
}

/// Persisted RuntimeState values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    /// Idle.
    Idle,
    /// Preparing.
    Preparing,
    /// Inferring.
    Inferring,
    /// Processing.
    Processing,
    /// Authorizing.
    Authorizing,
    /// Executing.
    Executing,
    /// Continuing.
    Continuing,
    /// Cancelling.
    Cancelling,
    /// Recovering.
    Recovering,
}
