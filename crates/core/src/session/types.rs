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
    /// Model.
    pub model: ModelRef,
    /// Configuration fingerprint.
    pub configuration_fingerprint: Digest,
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
}
impl ResultCode {
    /// The persisted spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolsUnavailable => "tools_unavailable",
            Self::Cancelled => "cancelled",
            Self::ProcessRestart => "process_restart",
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
