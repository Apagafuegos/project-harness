//! Persisted v1 wire types. Storage validates these against the bundled schema.

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
