//! Execution boundary. Only the runtime constructs an authorized operation.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use super::{Plan, PolicySnapshot, Registry, ToolDefinition};
use crate::session::{
    CallRef, ContentBlock, DecisionId, ExecutionBackend, FileEffect, FileImage, InvocationId,
    ResultCode, ToolStatus, UndoConflict, UndoFileOutcome,
};

/// A plan whose write-ahead intent the core has committed before dispatch.
/// There is no public constructor or deserializer.
#[derive(Clone, Debug)]
pub struct AuthorizedOperation {
    plan: Plan,
    invocation_id: InvocationId,
    call: CallRef,
    decision_id: DecisionId,
    policy_revision: String,
    image: Option<FileImage>,
}

impl AuthorizedOperation {
    pub(crate) fn new(
        plan: Plan,
        invocation_id: InvocationId,
        decision_id: DecisionId,
        image: Option<FileImage>,
    ) -> Self {
        Self {
            call: plan.call().clone(),
            policy_revision: plan.policy_revision().into(),
            plan,
            invocation_id,
            decision_id,
            image,
        }
    }
    /// The captured pre-image and post-image binding, for a trusted write/edit.
    pub fn image(&self) -> Option<&FileImage> {
        self.image.as_ref()
    }
    /// The exact immutable approved plan.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }
    /// Unique invocation recorded in the intent.
    pub fn invocation_id(&self) -> &InvocationId {
        &self.invocation_id
    }
    /// The proposal being answered.
    pub fn call(&self) -> &CallRef {
        &self.call
    }
    /// The recorded authorization.
    pub fn decision_id(&self) -> &DecisionId {
        &self.decision_id
    }
    /// Revision that authorized this operation (before a remembered grant).
    pub fn policy_revision(&self) -> &str {
        &self.policy_revision
    }
}

/// Cooperative executor cancellation. Cancellation never claims rollback.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);
impl CancelToken {
    /// An initially uncancelled token.
    pub fn new() -> Self {
        Self::default()
    }
    /// Request cancellation; idempotent and visible to every clone.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// One executor's terminal observation. Status is set explicitly by the
/// executor, never inferred from a planning refusal. Inline content is bounded
/// to 32 KiB; full bounded captures are published as artifacts before the result.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionOutcome {
    /// Succeeded, failed, timed out, cancelled, or infrastructure failure.
    pub status: ToolStatus,
    /// None exactly when succeeded.
    pub code: Option<ResultCode>,
    /// Model-facing output (at most 32 KiB of compact JSON).
    pub content: Vec<ContentBlock>,
    /// Process exit status, when applicable.
    pub exit_code: Option<i32>,
    /// Observed elapsed time, in milliseconds.
    pub duration_ms: Option<u64>,
    /// Whether the capture was truncated.
    pub output_truncated: bool,
    /// Full bounded captures, published through the existing artifact store.
    pub artifacts: Vec<Vec<u8>>,
    /// For a trusted write/edit, what the executor observed about the rename
    /// commit point. `None` for every other call. Never inferred from `status`.
    pub file_effect: Option<FileEffect>,
}

/// What one undo restoration did. A `Refused` outcome changed no target; a
/// `Done` outcome that is not `complete` may have changed some.
#[derive(Clone, Debug, PartialEq)]
pub enum UndoExecution {
    /// The whole-set preflight refused with every conflicting path and reason.
    Refused(Vec<UndoConflict>),
    /// Restoration ran; `complete` is false when a later target failed and the
    /// remaining ones were not attempted.
    Done {
        /// One row per attempted target, in target order.
        files: Vec<UndoFileOutcome>,
        /// Whether every target reached its desired state.
        complete: bool,
    },
}

/// Synchronous because the production driver will use a dedicated executor
/// thread (step 4b). No executor is installed by defining a tool.
pub trait Executor: Send + Sync {
    /// The backend this executor enforces.
    fn backend(&self) -> ExecutionBackend;
    /// Execute exactly this authorized plan, observing cooperative cancellation.
    fn execute(&self, operation: &AuthorizedOperation, cancel: &CancelToken) -> ExecutionOutcome;
}

/// Fixed registry plus the backends available to this runtime incarnation.
#[derive(Clone, Debug)]
pub struct ToolSet {
    registry: Registry,
    backends: Vec<ExecutionBackend>,
}
impl Default for ToolSet {
    fn default() -> Self {
        Self::new([])
    }
}
impl ToolSet {
    /// Construct once, from the backends with installed executors.
    pub fn new(backends: impl IntoIterator<Item = ExecutionBackend>) -> Self {
        let mut available = Vec::new();
        for backend in backends {
            if !available.contains(&backend) {
                available.push(backend);
            }
        }
        Self {
            registry: Registry::builtin(),
            backends: available,
        }
    }
    /// The immutable compiled-in registry.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }
    /// Whether an executor is available for this backend.
    pub fn available(&self, backend: ExecutionBackend) -> bool {
        self.backends.contains(&backend)
    }
    /// Whether this incarnation has no executors (the Milestone 2 behavior).
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }
    /// Enabled definitions with an executor, always in registry order.
    pub fn definitions(&self, policy: &PolicySnapshot) -> Vec<ToolDefinition> {
        self.registry
            .definitions()
            .filter(|d| policy.enabled(&d.name) && self.available(d.backend))
            .cloned()
            .collect()
    }
}
