//! Deterministic, staged semantic replay. No executor or provider is called here.

use super::{error::Result, *};
use imbl::{OrdMap, OrdSet, Vector};
use std::{collections::BTreeSet, sync::Arc};

/// An immutable tree entry and its exact journal provenance.
#[derive(Clone, Debug)]
pub struct StoredEntry {
    /// Validated immutable entry.
    pub entry: Entry,
    /// Journal sequence containing the entry.
    pub sequence: u64,
    /// Exact-byte checksum of its containing record.
    pub record_checksum: Digest,
    /// Index within that commit's entry array.
    pub entry_index: usize,
}

/// A durable command receipt, reconstructed without an in-memory reply cache.
#[derive(Clone, Debug)]
pub struct CommandReceipt {
    /// Digest of the stored normalized submitted command.
    pub fingerprint: Digest,
    /// Original durable acceptance, including its original head.
    pub acknowledgement: CommitAck,
    /// Accepted entries/events identify associated turns, approvals and recovery.
    pub commit: Commit,
}

/// One known turn and its durable outcome.
#[derive(Clone, Debug)]
pub struct TurnInfo {
    /// Initial user entry.
    pub user_entry_id: EntryId,
    /// None while unfinished; terminal outcomes are retained across navigation.
    pub outcome: Option<TurnOutcome>,
}

/// An execution intent that has no durable result; opening never retries it.
#[derive(Clone, Debug)]
pub struct UnresolvedInvocation {
    /// Persisted intent and its binding.
    pub intent: ExecutionIntent,
    /// Sequence at which dispatch first became possible.
    pub sequence: u64,
    /// Record time of the intent.
    pub at: Timestamp,
    /// Whether an explicit uncertainty event has been recorded.
    pub uncertainty_recorded: bool,
}

/// A read-only point-in-time view sharing unchanged history across commits.
/// Cloning it is cheap and never exposes staged changes.
#[derive(Clone, Debug)]
pub struct SessionSnapshot(pub(crate) Arc<State>);
impl SessionSnapshot {
    /// Immutable session identity.
    pub fn session_id(&self) -> &SessionId {
        &self.0.session_id
    }
    /// Last accepted record sequence.
    pub fn sequence(&self) -> u64 {
        self.0.sequence
    }
    /// Last accepted exact-byte record checksum.
    pub fn checksum(&self) -> &Digest {
        &self.0.checksum
    }
    /// Durably selected head, independent of physical entry order.
    pub fn head(&self) -> Option<&EntryId> {
        self.0.head.as_ref()
    }
    /// Initial session configuration and workspace.
    pub fn header(&self) -> &SessionHeader {
        &self.0.header
    }
    /// Current session-wide configuration, unaffected by navigation.
    pub fn configuration(&self) -> &ArtifactRef {
        &self.0.configuration
    }
    /// Current session-wide authorization policy revision.
    pub fn policy_revision(&self) -> &str {
        &self.0.policy_revision
    }
    /// Last committed operational state; opening does not synthesize audit events.
    pub fn state(&self) -> RuntimeState {
        self.0.runtime_state
    }
    /// Current unfinished turn, if any.
    pub fn active_turn(&self) -> Option<&TurnId> {
        self.0.active_turn.as_ref()
    }
    /// Look up an immutable entry on any branch.
    pub fn entry(&self, id: &EntryId) -> Option<&StoredEntry> {
        self.entry_arc(id).map(Arc::as_ref)
    }
    pub(crate) fn entry_arc(&self, id: &EntryId) -> Option<&Arc<StoredEntry>> {
        self.0.entries.get(id)
    }
    /// All branches, in stable identity order. Use ancestry for conversation order.
    pub fn entries(
        &self,
    ) -> impl ExactSizeIterator<Item = (&EntryId, &StoredEntry)> + DoubleEndedIterator {
        self.0
            .entries
            .iter()
            .map(|(id, entry)| (id, entry.as_ref()))
    }
    /// Look up a known turn, including one on a sibling branch.
    pub fn turn(&self, id: &TurnId) -> Option<&TurnInfo> {
        self.0.turns.get(id)
    }
    /// Known turn outcomes in stable identity order, including sibling branches.
    pub fn turns(
        &self,
    ) -> impl ExactSizeIterator<Item = (&TurnId, &TurnInfo)> + DoubleEndedIterator {
        self.0.turns.iter()
    }
    /// Globally ordered audit events `(record sequence, event index, event)`.
    pub fn events(
        &self,
    ) -> impl ExactSizeIterator<Item = &(u64, usize, JournalEvent)> + DoubleEndedIterator {
        self.0.events.iter().map(Arc::as_ref)
    }
    /// Persisted command acceptance, including after an acknowledgement was lost.
    pub fn command_receipt(&self, id: &CommandId) -> Option<&CommandReceipt> {
        self.0.commands.get(id).map(Arc::as_ref)
    }
    /// Return ancestors in conversation order, rejecting unknown heads.
    pub fn ancestry(&self, head: Option<&EntryId>) -> Result<Vec<&StoredEntry>> {
        self.0.ancestry(head)
    }
    /// Branch-derived model selection at the supplied head.
    pub fn model_at(&self, head: Option<&EntryId>) -> Result<&ModelRef> {
        self.0.model_at(head)
    }
    /// All intents lacking a committed result, across every branch.
    pub fn unresolved_invocations(&self) -> Vec<UnresolvedInvocation> {
        self.0
            .unresolved
            .iter()
            .map(|id| &self.0.invocations[id])
            .filter(|v| v.result.is_none())
            .map(|v| UnresolvedInvocation {
                intent: v.intent.clone(),
                sequence: v.sequence,
                at: v.at.clone(),
                uncertainty_recorded: v.uncertain,
            })
            .collect()
    }
    /// Whether any journal identity (session, record, entry, turn, response,
    /// call, invocation, approval, decision or command) already uses `id`.
    /// Storage rejects a commit that reuses one with `duplicate_id`.
    pub(crate) fn has_identity(&self, id: &str) -> bool {
        self.0.ids.contains(id)
    }
    /// A reopened engine must explicitly terminate unfinished work and resolve
    /// uncertain effects before accepting new work. No tool may be replayed.
    pub fn requires_recovery(&self) -> bool {
        self.0.active_turn.is_some() || self.0.has_unresolved()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Invocation {
    intent: ExecutionIntent,
    sequence: u64,
    at: Timestamp,
    uncertain: bool,
    result: Option<EntryId>,
    recovery_resolved: bool,
}
impl Invocation {
    /// Lacks a result, or is uncertain without an explicit recovery resolution.
    fn unresolved(&self) -> bool {
        self.result.is_none() || (self.uncertain && !self.recovery_resolved)
    }
}
/// Facts that depend only on an entry's ancestry. Entries are immutable, so
/// these are computed once from the parent's facts when the entry is inserted.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BranchFacts {
    /// Calls still awaiting results after this entry, in required order.
    pending: Arc<[CallRef]>,
    /// Model in effect at this entry.
    model: ModelRef,
}
/// Internal ordered key for a call; the public `CallRef` is not `Ord`.
type CallKey = (EntryId, CallId);
fn key(call: &CallRef) -> CallKey {
    (call.response_entry_id.clone(), call.call_id.clone())
}
#[derive(Clone, Debug)]
struct Approval {
    request: ApprovalRequested,
    used: bool,
}
#[derive(Clone, Debug)]
struct Authorization {
    decision: AuthorizationDecided,
    used: bool,
}
#[derive(Clone, Debug)]
pub(crate) struct State {
    pub session_id: SessionId,
    pub header: SessionHeader,
    pub sequence: u64,
    pub checksum: Digest,
    pub head: Option<EntryId>,
    pub configuration: ArtifactRef,
    pub policy_revision: String,
    pub runtime_state: RuntimeState,
    pub active_turn: Option<TurnId>,
    pub entries: OrdMap<EntryId, Arc<StoredEntry>>,
    pub turns: OrdMap<TurnId, TurnInfo>,
    pub events: Vector<Arc<(u64, usize, JournalEvent)>>,
    pub commands: OrdMap<CommandId, Arc<CommandReceipt>>,
    pub invocations: OrdMap<InvocationId, Invocation>,
    ids: OrdSet<String>,
    approvals: OrdMap<ApprovalId, Approval>,
    decisions: OrdMap<DecisionId, Authorization>,
    inference_response: Option<EntryId>,
    // Derived state below is staged with the records it indexes and rebuilt by
    // replay, never persisted. It keeps a commit's validation cost independent
    // of history length.
    /// Branch facts of every entry in `entries`.
    pub(crate) branch: OrdMap<EntryId, Arc<BranchFacts>>,
    /// Branch facts at the null root.
    root: Arc<BranchFacts>,
    /// Invocations for which `Invocation::unresolved` holds.
    unresolved: OrdSet<InvocationId>,
    /// The only invocation of each call that has one.
    invocation_by_call: OrdMap<CallKey, InvocationId>,
    /// The only unused approval of each call that has one.
    unused_approvals: OrdMap<CallKey, ApprovalId>,
    /// Decisions not yet used by an intent or the end of their turn.
    unused_decisions: OrdSet<DecisionId>,
}

fn require(condition: bool, code: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(StoreError::Invalid(code))
    }
}
impl State {
    pub fn start(record: &Record, checksum: Digest) -> Result<Self> {
        let RecordPayload::SessionCreated(header) = &record.payload else {
            return Err(StoreError::Invalid("missing_header"));
        };
        require(
            record.seq == 0 && record.previous_checksum.is_none(),
            "invalid_header_sequence",
        )?;
        let mut state = Self {
            session_id: record.session_id.clone(),
            header: header.clone(),
            sequence: 0,
            checksum,
            head: None,
            configuration: header.configuration.clone(),
            policy_revision: header.policy_revision.clone(),
            runtime_state: RuntimeState::Idle,
            active_turn: None,
            entries: OrdMap::new(),
            turns: OrdMap::new(),
            events: Vector::new(),
            commands: OrdMap::new(),
            invocations: OrdMap::new(),
            ids: OrdSet::new(),
            approvals: OrdMap::new(),
            decisions: OrdMap::new(),
            inference_response: None,
            branch: OrdMap::new(),
            root: Arc::new(BranchFacts {
                pending: Arc::from([]),
                model: header.initial_model.clone(),
            }),
            unresolved: OrdSet::new(),
            invocation_by_call: OrdMap::new(),
            unused_approvals: OrdMap::new(),
            unused_decisions: OrdSet::new(),
        };
        state.unique(record.session_id.as_str())?;
        state.unique(record.record_id.as_str())?;
        Ok(state)
    }
    fn unique(&mut self, id: &str) -> Result<()> {
        require(self.ids.insert(id.to_owned()).is_none(), "duplicate_id")
    }
    fn has_unresolved(&self) -> bool {
        !self.unresolved.is_empty()
    }
    /// Recompute one invocation's membership of `unresolved` from its record.
    /// Call after every change to that record.
    fn track(&mut self, id: &InvocationId) {
        if self.invocations.get(id).is_some_and(Invocation::unresolved) {
            self.unresolved.insert(id.clone());
        } else {
            self.unresolved.remove(id);
        }
    }
    fn idle(&self) -> Result<()> {
        require(
            self.active_turn.is_none()
                && self.runtime_state == RuntimeState::Idle
                && !self.has_unresolved(),
            "session_not_idle",
        )
    }
    fn active(&self, turn: &TurnId) -> Result<()> {
        require(self.active_turn.as_ref() == Some(turn), "wrong_active_turn")
    }
    fn ancestry(&self, head: Option<&EntryId>) -> Result<Vec<&StoredEntry>> {
        let mut next = head;
        let mut entries = Vec::new();
        while let Some(id) = next {
            let stored = self
                .entries
                .get(id)
                .ok_or(StoreError::Invalid("unknown_head_or_parent"))?;
            entries.push(stored.as_ref());
            next = stored.entry.parent_id.as_ref();
        }
        entries.reverse();
        Ok(entries)
    }
    fn facts(&self, head: Option<&EntryId>) -> Result<&Arc<BranchFacts>> {
        match head {
            None => Ok(&self.root),
            Some(id) => self
                .branch
                .get(id)
                .ok_or(StoreError::Invalid("unknown_head_or_parent")),
        }
    }
    fn model_at(&self, head: Option<&EntryId>) -> Result<&ModelRef> {
        Ok(&self.facts(head)?.model)
    }
    pub(crate) fn pending(&self, head: Option<&EntryId>) -> Result<&[CallRef]> {
        Ok(&self.facts(head)?.pending)
    }
    fn call(&self, call: &CallRef, sequence: u64, turn: &TurnId) -> Result<()> {
        let response = self
            .entries
            .get(&call.response_entry_id)
            .ok_or(StoreError::Invalid("unknown_response"))?;
        require(
            response.sequence < sequence && response.entry.turn_id.as_ref() == Some(turn),
            "proposal_not_prior_durable",
        )?;
        let EntryPayload::AssistantResponse(payload) = &response.entry.payload else {
            return Err(StoreError::Invalid("call_not_response"));
        };
        require(
            payload.outcome == ResponseOutcome::Completed
                && payload.finish_reason == Some(FinishReason::ToolCalls),
            "ineligible_proposal",
        )?;
        require(payload.content.iter().any(|block| matches!(block, ContentBlock::ToolCall {call_id, ..} if call_id == &call.call_id)), "unknown_call")?;
        Ok(())
    }
    fn next_call(&self, call: &CallRef, sequence: u64, turn: &TurnId) -> Result<()> {
        self.call(call, sequence, turn)?;
        require(
            self.pending(self.head.as_ref())?.first() == Some(call),
            "call_not_next_on_branch",
        )
    }

    pub fn apply(&mut self, record: &Record, checksum: Digest) -> Result<()> {
        self.apply_record(record, checksum)?;
        #[cfg(test)]
        if ORACLE.get() {
            self.check_derived(record);
        }
        Ok(())
    }
    fn apply_record(&mut self, record: &Record, checksum: Digest) -> Result<()> {
        require(
            record.seq == self.sequence + 1
                && record.session_id == self.session_id
                && record.previous_checksum.as_ref() == Some(&self.checksum),
            "broken_record_chain",
        )?;
        self.unique(record.record_id.as_str())?;
        let RecordPayload::Commit(commit) = &record.payload else {
            return Err(StoreError::Invalid("repeated_header"));
        };
        if commit.head.from != self.head {
            return Err(StoreError::StaleHead);
        }
        require(
            !commit.entries.is_empty()
                || !commit.events.is_empty()
                || commit.head.from != commit.head.to,
            "empty_commit",
        )?;
        require(
            commit.command_id.is_some() == commit.command_fingerprint.is_some(),
            "command_binding",
        )?;
        if let Some(id) = &commit.command_id {
            self.unique(id.as_str())?;
        }
        match commit.head.reason {
            HeadReason::Advance => {
                require(!commit.entries.is_empty(), "advance_without_entries")?;
                let mut parent = commit.head.from.as_ref();
                for entry in &commit.entries {
                    require(entry.parent_id.as_ref() == parent, "nonlinear_advance")?;
                    parent = Some(&entry.id);
                }
                require(parent == commit.head.to.as_ref(), "invalid_advance_head")?;
            }
            HeadReason::Navigate => {
                self.idle()?;
                require(commit.entries.is_empty(), "navigation_has_entries")?;
                require(
                    self.pending(commit.head.to.as_ref())?.is_empty(),
                    "head_inside_tool_exchange",
                )?;
            }
            HeadReason::Retain => require(
                commit.entries.is_empty() && commit.head.to == self.head,
                "invalid_retained_head",
            )?,
        }
        let starts: Vec<_> = commit
            .events
            .iter()
            .filter_map(|e| {
                if let JournalEvent::TurnStarted(v) = e {
                    Some(v)
                } else {
                    None
                }
            })
            .collect();
        require(starts.len() <= 1, "multiple_turns")?;
        for start in &starts {
            self.idle()?;
            self.unique(start.turn_id.as_str())?;
            require(
                start.configuration == self.configuration,
                "turn_configuration_mismatch",
            )?;
            self.turns.insert(
                start.turn_id.clone(),
                TurnInfo {
                    user_entry_id: start.user_entry_id.clone(),
                    outcome: None,
                },
            );
        }
        for (index, entry) in commit.entries.iter().enumerate() {
            self.unique(entry.id.as_str())?;
            if let Some(parent) = &entry.parent_id {
                require(self.entries.contains_key(parent), "unknown_parent")?;
            }
            self.entry(entry, record.seq, &starts)?;
            let facts = step(self.facts(entry.parent_id.as_ref())?, entry)?;
            self.branch.insert(entry.id.clone(), facts);
            self.entries.insert(
                entry.id.clone(),
                Arc::new(StoredEntry {
                    entry: entry.clone(),
                    sequence: record.seq,
                    record_checksum: checksum.clone(),
                    entry_index: index,
                }),
            );
        }
        self.head = commit.head.to.clone();
        // Entries and events are atomic, but event order remains semantically significant.
        for (index, event) in commit.events.iter().enumerate() {
            self.event(event, record)?;
            self.events
                .push_back(Arc::new((record.seq, index, event.clone())));
        }
        // Every new entry's branch rules were checked by `step`; this guards
        // that the selected head exists.
        self.facts(self.head.as_ref())?;
        require(
            self.active_turn.is_none() || self.runtime_state != RuntimeState::Idle,
            "unfinished_idle_turn",
        )?;
        for entry in &commit.entries {
            if let EntryPayload::ToolResult(result) = &entry.payload
                && matches!(
                    result.result.metadata.provenance,
                    Provenance::UserObserved | Provenance::UserAcknowledgedUnknown
                )
            {
                require(commit.events.iter().any(|e| matches!(e, JournalEvent::RecoveryResolved(r) if r.result_entry_id == entry.id)), "recovery_result_without_resolution")?;
            }
        }
        self.sequence = record.seq;
        self.checksum = checksum.clone();
        if let (Some(id), Some(fingerprint)) = (&commit.command_id, &commit.command_fingerprint) {
            self.commands.insert(
                id.clone(),
                Arc::new(CommandReceipt {
                    fingerprint: fingerprint.clone(),
                    acknowledgement: CommitAck {
                        sequence: record.seq,
                        checksum,
                        head: self.head.clone(),
                    },
                    commit: commit.clone(),
                }),
            );
        }
        Ok(())
    }

    fn entry(&mut self, entry: &Entry, sequence: u64, starts: &[&TurnStarted]) -> Result<()> {
        match &entry.payload {
            EntryPayload::UserMessage(message) => {
                require(
                    starts.iter().any(|s| {
                        Some(&s.turn_id) == entry.turn_id.as_ref() && s.user_entry_id == entry.id
                    }),
                    "user_without_turn_start",
                )?;
                require(
                    self.pending(entry.parent_id.as_ref())?.is_empty(),
                    "head_inside_tool_exchange",
                )?;
                valid_content(&message.content)?;
            }
            EntryPayload::AssistantResponse(response) => {
                let turn = entry
                    .turn_id
                    .as_ref()
                    .ok_or(StoreError::Invalid("response_without_turn"))?;
                self.active(turn)?;
                require(
                    matches!(
                        self.runtime_state,
                        RuntimeState::Inferring
                            | RuntimeState::Cancelling
                            | RuntimeState::Recovering
                    ),
                    "response_outside_inference",
                )?;
                require(
                    self.pending(entry.parent_id.as_ref())?.is_empty(),
                    "inference_inside_tool_exchange",
                )?;
                require(
                    response.model == *self.model_at(entry.parent_id.as_ref())?,
                    "response_model_mismatch",
                )?;
                require(
                    self.inference_response.is_none(),
                    "duplicate_inference_response",
                )?;
                self.unique(response.response_id.as_str())?;
                let mut calls = 0;
                for block in &response.content {
                    if let ContentBlock::ToolCall { call_id, .. } = block {
                        self.unique(call_id.as_str())?;
                        calls += 1;
                    }
                    if let ContentBlock::ProviderState { owner, .. } = block {
                        require(owner == &response.model, "provider_state_owner")?;
                    }
                }
                match response.outcome {
                    ResponseOutcome::Completed => {
                        require(
                            response.finish_reason.is_some() && response.error.is_none(),
                            "invalid_completed_response",
                        )?;
                        require(
                            (response.finish_reason == Some(FinishReason::ToolCalls))
                                == (calls > 0),
                            "inconsistent_tool_finish",
                        )?;
                    }
                    ResponseOutcome::Failed => require(
                        response.error.is_some()
                            && response.finish_reason.is_none()
                            && response.continuation.is_none(),
                        "invalid_failed_response",
                    )?,
                    ResponseOutcome::Cancelled | ResponseOutcome::Interrupted => require(
                        response.error.is_none()
                            && response.finish_reason.is_none()
                            && response.continuation.is_none(),
                        "invalid_partial_response",
                    )?,
                }
                if let Some(continuation) = &response.continuation {
                    require(continuation.owner == response.model, "continuation_owner")?;
                }
                self.inference_response = Some(entry.id.clone());
            }
            EntryPayload::ToolResult(result) => {
                let result = &result.result;
                let turn = entry
                    .turn_id
                    .as_ref()
                    .ok_or(StoreError::Invalid("result_without_turn"))?;
                self.call(&result.call, sequence, turn)?;
                require(
                    self.pending(entry.parent_id.as_ref())?.first() == Some(&result.call),
                    "orphan_duplicate_or_reordered_result",
                )?;
                valid_content(&result.content)?;
                let recovering = matches!(
                    result.metadata.provenance,
                    Provenance::UserObserved | Provenance::UserAcknowledgedUnknown
                );
                if !recovering {
                    self.active(turn)?;
                } else {
                    require(
                        self.turns
                            .get(turn)
                            .is_some_and(|t| t.outcome == Some(TurnOutcome::Interrupted))
                            || self.runtime_state == RuntimeState::Recovering,
                        "not_recovering",
                    )?;
                }
                if let Some(id) = &result.invocation_id {
                    let invocation = self
                        .invocations
                        .get_mut(id)
                        .ok_or(StoreError::Invalid("unknown_invocation"))?;
                    require(
                        invocation.sequence < sequence
                            && invocation.intent.call == result.call
                            && invocation.intent.turn_id == *turn
                            && invocation.result.is_none(),
                        "invalid_invocation_result",
                    )?;
                    require(
                        if recovering {
                            invocation.uncertain
                        } else {
                            !invocation.uncertain
                        },
                        "uncertain_result_requires_recovery",
                    )?;
                    require(
                        if result.status == ToolStatus::Uncertain {
                            result.metadata.provenance == Provenance::UserAcknowledgedUnknown
                        } else {
                            result.metadata.provenance != Provenance::UserAcknowledgedUnknown
                        },
                        "invalid_unknown_result",
                    )?;
                    require(
                        recovering
                            || (result.metadata.provenance == Provenance::Executor
                                && matches!(
                                    result.status,
                                    ToolStatus::Succeeded
                                        | ToolStatus::Failed
                                        | ToolStatus::TimedOut
                                        | ToolStatus::Cancelled
                                        | ToolStatus::InfrastructureFailure
                                )),
                        "invalid_executed_provenance",
                    )?;
                    invocation.result = Some(entry.id.clone());
                    self.track(id);
                } else {
                    require(
                        result.metadata.provenance == Provenance::Runtime
                            && matches!(
                                result.status,
                                ToolStatus::InvalidArguments
                                    | ToolStatus::Denied
                                    | ToolStatus::Cancelled
                                    | ToolStatus::InfrastructureFailure
                                    | ToolStatus::TimedOut
                            ),
                        "invalid_nonexecuted_result",
                    )?;
                    require(
                        !self
                            .invocation_by_call
                            .get(&key(&result.call))
                            .is_some_and(|id| self.invocations[id].result.is_none()),
                        "result_omits_intent",
                    )?;
                }
            }
            EntryPayload::ModelChanged(_) => {
                self.idle()?;
                require(
                    entry.turn_id.is_none() && starts.is_empty(),
                    "model_change_inside_turn",
                )?;
            }
            EntryPayload::Compaction(compaction) => {
                self.idle()?;
                require(
                    entry.turn_id.is_none() && starts.is_empty(),
                    "compaction_inside_turn",
                )?;
                self.compaction(entry, compaction)?;
            }
        }
        Ok(())
    }

    fn event(&mut self, event: &JournalEvent, record: &Record) -> Result<()> {
        match event {
            JournalEvent::TurnStarted(start) => {
                require(
                    self.active_turn.is_none()
                        && self.runtime_state == RuntimeState::Idle
                        && !self.has_unresolved(),
                    "concurrent_turn",
                )?;
                let user = self
                    .entries
                    .get(&start.user_entry_id)
                    .ok_or(StoreError::Invalid("missing_user_entry"))?;
                require(
                    user.sequence == record.seq
                        && user.entry.turn_id.as_ref() == Some(&start.turn_id)
                        && matches!(user.entry.payload, EntryPayload::UserMessage(_)),
                    "invalid_turn_start",
                )?;
                self.active_turn = Some(start.turn_id.clone());
            }
            JournalEvent::StateChanged(change) => {
                require(
                    self.active_turn.as_ref() == Some(&change.turn_id)
                        || (change.to == RuntimeState::Recovering
                            && self.has_unresolved()
                            && self.turns.contains_key(&change.turn_id)),
                    "state_without_turn",
                )?;
                require(
                    change.from == self.runtime_state && legal_transition(change.from, change.to),
                    "illegal_state_transition",
                )?;
                if change.to == RuntimeState::Inferring {
                    require(
                        !self.has_unresolved() && self.pending(self.head.as_ref())?.is_empty(),
                        "inference_before_results",
                    )?;
                    self.inference_response = None;
                }
                if change.to == RuntimeState::Processing && change.from == RuntimeState::Inferring {
                    require(
                        self.inference_response.as_ref() == self.head.as_ref(),
                        "processing_without_current_response",
                    )?;
                    require(
                        self.head
                            .as_ref()
                            .and_then(|id| self.entries.get(id))
                            .is_some_and(|v| {
                                matches!(v.entry.payload, EntryPayload::AssistantResponse(_))
                                    && v.entry.turn_id.as_ref() == Some(&change.turn_id)
                            }),
                        "processing_without_response",
                    )?;
                }
                if change.to == RuntimeState::Executing {
                    require(
                        self.unresolved.iter().any(|id| {
                            let v = &self.invocations[id];
                            v.result.is_none() && v.intent.turn_id == change.turn_id
                        }),
                        "executing_without_intent",
                    )?;
                }
                if change.from == RuntimeState::Executing && change.to == RuntimeState::Continuing {
                    require(!self.has_unresolved(), "continuing_without_result")?;
                }
                self.runtime_state = change.to;
            }
            JournalEvent::ApprovalRequested(request) => {
                self.active(&request.turn_id)?;
                require(
                    self.runtime_state == RuntimeState::Authorizing,
                    "approval_outside_authorizing",
                )?;
                self.next_call(&request.call, record.seq, &request.turn_id)?;
                self.unique(request.approval_id.as_str())?;
                require(
                    request.plan_digest == request.plan.sha256
                        && request.policy_revision == self.policy_revision
                        && request.expires_at.instant() > record.at.instant(),
                    "invalid_approval_binding",
                )?;
                require(
                    !self.unused_approvals.contains_key(&key(&request.call)),
                    "duplicate_pending_approval",
                )?;
                self.unused_approvals
                    .insert(key(&request.call), request.approval_id.clone());
                self.approvals.insert(
                    request.approval_id.clone(),
                    Approval {
                        request: request.clone(),
                        used: false,
                    },
                );
            }
            JournalEvent::AuthorizationDecided(decision) => {
                self.active(&decision.turn_id)?;
                require(
                    self.runtime_state == RuntimeState::Authorizing,
                    "authorization_outside_authorizing",
                )?;
                self.next_call(&decision.call, record.seq, &decision.turn_id)?;
                self.unique(decision.decision_id.as_str())?;
                require(
                    decision.plan_digest == decision.plan.sha256
                        && decision.policy_revision == self.policy_revision,
                    "authorization_binding",
                )?;
                if let Some(id) = &decision.approval_id {
                    let approval = self
                        .approvals
                        .get_mut(id)
                        .ok_or(StoreError::Invalid("unknown_approval"))?;
                    require(
                        !approval.used
                            && approval.request.call == decision.call
                            && approval.request.turn_id == decision.turn_id
                            && approval.request.plan_digest == decision.plan_digest
                            && approval.request.policy_revision == decision.policy_revision,
                        "approval_binding_mismatch",
                    )?;
                    if decision.decision == Decision::Allow {
                        require(
                            record.at.instant() < approval.request.expires_at.instant(),
                            "expired_approval",
                        )?;
                    }
                    approval.used = true;
                    self.unused_approvals.remove(&key(&approval.request.call));
                }
                self.unused_decisions.insert(decision.decision_id.clone());
                self.decisions.insert(
                    decision.decision_id.clone(),
                    Authorization {
                        decision: decision.clone(),
                        used: false,
                    },
                );
            }
            JournalEvent::ExecutionIntent(intent) => {
                self.active(&intent.turn_id)?;
                require(
                    self.runtime_state == RuntimeState::Authorizing && !self.has_unresolved(),
                    "overlapping_execution",
                )?;
                self.next_call(&intent.call, record.seq, &intent.turn_id)?;
                self.unique(intent.invocation_id.as_str())?;
                require(
                    !self.invocation_by_call.contains_key(&key(&intent.call)),
                    "duplicate_call_invocation",
                )?;
                let authorization = self
                    .decisions
                    .get_mut(&intent.decision_id)
                    .ok_or(StoreError::Invalid("unknown_authorization"))?;
                let decision = &authorization.decision;
                require(
                    !authorization.used
                        && decision.decision == Decision::Allow
                        && decision.call == intent.call
                        && decision.turn_id == intent.turn_id
                        && decision.plan_digest == intent.plan_digest
                        && decision.policy_revision == self.policy_revision,
                    "intent_authorization_mismatch",
                )?;
                if let Some(id) = &decision.approval_id {
                    require(
                        record.at.instant() < self.approvals[id].request.expires_at.instant(),
                        "expired_approval",
                    )?;
                }
                authorization.used = true;
                self.unused_decisions.remove(&intent.decision_id);
                self.invocation_by_call
                    .insert(key(&intent.call), intent.invocation_id.clone());
                self.invocations.insert(
                    intent.invocation_id.clone(),
                    Invocation {
                        intent: intent.clone(),
                        sequence: record.seq,
                        at: record.at.clone(),
                        uncertain: false,
                        result: None,
                        recovery_resolved: false,
                    },
                );
                self.track(&intent.invocation_id);
            }
            JournalEvent::ExecutionUncertain(uncertain) => {
                let invocation = self
                    .invocations
                    .get_mut(&uncertain.invocation_id)
                    .ok_or(StoreError::Invalid("unknown_invocation"))?;
                require(
                    invocation.intent.turn_id == uncertain.turn_id
                        && invocation.result.is_none()
                        && !invocation.uncertain,
                    "invalid_uncertainty",
                )?;
                invocation.uncertain = true;
                self.track(&uncertain.invocation_id);
                self.runtime_state = RuntimeState::Recovering;
            }
            JournalEvent::RecoveryResolved(resolution) => {
                require(
                    self.runtime_state == RuntimeState::Recovering,
                    "resolution_outside_recovery",
                )?;
                let invocation = self
                    .invocations
                    .get_mut(&resolution.invocation_id)
                    .ok_or(StoreError::Invalid("unknown_invocation"))?;
                require(
                    invocation.intent.turn_id == resolution.turn_id
                        && invocation.uncertain
                        && !invocation.recovery_resolved
                        && invocation.result.as_ref() == Some(&resolution.result_entry_id),
                    "invalid_recovery_result",
                )?;
                let stored = self
                    .entries
                    .get(&resolution.result_entry_id)
                    .ok_or(StoreError::Invalid("missing_recovery_result"))?;
                let EntryPayload::ToolResult(result) = &stored.entry.payload else {
                    return Err(StoreError::Invalid("recovery_result_not_tool"));
                };
                require(
                    stored.sequence == record.seq
                        && match resolution.resolution {
                            Resolution::Observed => {
                                result.result.metadata.provenance == Provenance::UserObserved
                                    && result.result.status != ToolStatus::Uncertain
                            }
                            Resolution::AcknowledgedUnknown => {
                                result.result.metadata.provenance
                                    == Provenance::UserAcknowledgedUnknown
                                    && result.result.status == ToolStatus::Uncertain
                            }
                        },
                    "recovery_resolution_provenance",
                )?;
                invocation.recovery_resolved = true;
                self.track(&resolution.invocation_id);
                if !self.has_unresolved() && self.active_turn.is_none() {
                    self.runtime_state = RuntimeState::Idle;
                }
            }
            JournalEvent::TurnFinished(finished) => {
                self.active(&finished.turn_id)?;
                let pending = self.pending(self.head.as_ref())?;
                require(
                    pending.iter().all(|call| {
                        self.invocation_by_call
                            .get(&key(call))
                            .is_some_and(|id| self.invocations[id].result.is_none())
                    }),
                    "unfinished_not_started_proposals",
                )?;
                if finished.outcome == TurnOutcome::Completed {
                    require(
                        pending.is_empty()
                            && !self.has_unresolved()
                            && matches!(
                                self.runtime_state,
                                RuntimeState::Processing | RuntimeState::Continuing
                            ),
                        "invalid_completed_turn",
                    )?;
                    require(self.head.as_ref().and_then(|id| self.entries.get(id)).is_some_and(|v| matches!(&v.entry.payload, EntryPayload::AssistantResponse(r) if r.outcome == ResponseOutcome::Completed && matches!(r.finish_reason, Some(FinishReason::Stop | FinishReason::Refusal)))), "completion_without_stop")?;
                }
                if self.has_unresolved() {
                    require(
                        finished.outcome == TurnOutcome::Interrupted,
                        "uncertain_turn_must_interrupt",
                    )?;
                }
                self.turns
                    .get_mut(&finished.turn_id)
                    .ok_or(StoreError::Invalid("unknown_turn"))?
                    .outcome = Some(finished.outcome);
                self.active_turn = None;
                self.inference_response = None;
                self.runtime_state = if self.has_unresolved() {
                    RuntimeState::Recovering
                } else {
                    RuntimeState::Idle
                };
                // Only the active turn can hold unused approvals and decisions.
                for (_, id) in std::mem::take(&mut self.unused_approvals) {
                    self.approvals[&id].used = true;
                }
                for id in std::mem::take(&mut self.unused_decisions) {
                    self.decisions[&id].used = true;
                }
            }
            JournalEvent::ConfigurationChanged(configuration) => {
                self.idle()?;
                self.configuration = configuration.configuration.clone();
                self.policy_revision = configuration.policy_revision.clone();
            }
            JournalEvent::TailRepaired(repair) => {
                require(
                    repair.original_size > repair.valid_bytes
                        && repair.quarantine_artifact.bytes
                            == repair.original_size - repair.valid_bytes,
                    "invalid_tail_repair",
                )?;
            }
        }
        Ok(())
    }

    fn compaction(&self, entry: &Entry, compaction: &Compaction) -> Result<()> {
        require(
            compaction.source_fingerprint == compaction.source_manifest.sha256
                && compaction.configuration_fingerprint == self.configuration.sha256,
            "compaction_fingerprint",
        )?;
        let ancestry = self.ancestry(entry.parent_id.as_ref())?;
        let ids: Vec<_> = ancestry.iter().map(|v| &v.entry.id).collect();
        let start = ids
            .iter()
            .position(|id| **id == compaction.covered_entry_ids[0])
            .ok_or(StoreError::Invalid("compaction_not_ancestral"))?;
        require(
            ids.get(start..start + compaction.covered_entry_ids.len())
                .is_some_and(|span| span.iter().copied().eq(compaction.covered_entry_ids.iter())),
            "compaction_not_contiguous",
        )?;
        // Completed turns in order of first appearance on the branch.
        let mut seen = BTreeSet::new();
        let mut completed_turns = Vec::new();
        for stored in &ancestry {
            if let Some(turn) = &stored.entry.turn_id
                && self
                    .turns
                    .get(turn)
                    .is_some_and(|t| t.outcome == Some(TurnOutcome::Completed))
                && seen.insert(turn)
            {
                completed_turns.push(turn);
            }
        }
        let recent: BTreeSet<_> = completed_turns.iter().rev().take(2).copied().collect();
        for id in &compaction.covered_entry_ids {
            let source = &self.entries[id].entry;
            if let Some(turn) = &source.turn_id {
                require(
                    self.turns
                        .get(turn)
                        .is_some_and(|t| t.outcome == Some(TurnOutcome::Completed))
                        && !recent.contains(turn),
                    "compaction_unfinished_or_recent_turn",
                )?;
                require(
                    ancestry
                        .iter()
                        .filter(|e| e.entry.turn_id.as_ref() == Some(turn))
                        .all(|e| compaction.covered_entry_ids.contains(&e.entry.id)),
                    "compaction_splits_turn",
                )?;
            }
        }
        for id in &compaction.supersedes {
            require(ids.contains(&id), "superseded_summary_not_ancestral")?;
            let EntryPayload::Compaction(previous) = &self.entries[id].entry.payload else {
                return Err(StoreError::Invalid("supersedes_not_summary"));
            };
            require(
                previous
                    .covered_entry_ids
                    .iter()
                    .all(|id| compaction.covered_entry_ids.contains(id)),
                "supersedes_loses_coverage",
            )?;
        }
        let already_superseded: BTreeSet<_> = ancestry
            .iter()
            .filter_map(|e| {
                if let EntryPayload::Compaction(c) = &e.entry.payload {
                    Some(c.supersedes.iter())
                } else {
                    None
                }
            })
            .flatten()
            .collect();
        for prior in ancestry {
            if let EntryPayload::Compaction(previous) = &prior.entry.payload
                && !already_superseded.contains(&prior.entry.id)
                && previous
                    .covered_entry_ids
                    .iter()
                    .any(|id| compaction.covered_entry_ids.contains(id))
            {
                require(
                    compaction.supersedes.contains(&prior.entry.id),
                    "overlapping_active_summaries",
                )?;
            }
        }
        Ok(())
    }
}

/// One iteration of the branch walk: the facts after `entry`, given its parent's.
/// Unchanged facts share the parent's allocation.
fn step(parent: &Arc<BranchFacts>, entry: &Entry) -> Result<Arc<BranchFacts>> {
    match &entry.payload {
        EntryPayload::AssistantResponse(response) => {
            require(parent.pending.is_empty(), "inference_inside_tool_exchange")?;
            if response.outcome != ResponseOutcome::Completed
                || response.finish_reason != Some(FinishReason::ToolCalls)
            {
                return Ok(parent.clone());
            }
            let pending: Arc<[CallRef]> = response
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall { call_id, .. } => Some(CallRef {
                        response_entry_id: entry.id.clone(),
                        call_id: call_id.clone(),
                    }),
                    _ => None,
                })
                .collect();
            if pending.is_empty() {
                return Ok(parent.clone());
            }
            Ok(Arc::new(BranchFacts {
                pending,
                model: parent.model.clone(),
            }))
        }
        EntryPayload::ToolResult(result) => {
            require(
                parent.pending.first() == Some(&result.result.call),
                "orphan_duplicate_or_reordered_result",
            )?;
            Ok(Arc::new(BranchFacts {
                pending: Arc::from(&parent.pending[1..]),
                model: parent.model.clone(),
            }))
        }
        EntryPayload::UserMessage(_) | EntryPayload::Compaction(_) => {
            require(parent.pending.is_empty(), "head_inside_tool_exchange")?;
            Ok(parent.clone())
        }
        EntryPayload::ModelChanged(change) => {
            require(parent.pending.is_empty(), "head_inside_tool_exchange")?;
            Ok(Arc::new(BranchFacts {
                pending: parent.pending.clone(),
                model: change.model.clone(),
            }))
        }
    }
}

fn valid_content(content: &[ContentBlock]) -> Result<()> {
    require(
        content.iter().all(|b| {
            matches!(
                b,
                ContentBlock::Text { .. }
                    | ContentBlock::Image { .. }
                    | ContentBlock::Audio { .. }
                    | ContentBlock::File { .. }
            )
        }),
        "content_role_violation",
    )
}
fn legal_transition(from: RuntimeState, to: RuntimeState) -> bool {
    use RuntimeState::*;
    matches!(
        (from, to),
        (Idle, Preparing)
            | (Preparing, Inferring)
            | (Inferring, Processing)
            | (Processing, Authorizing | Continuing)
            | (Authorizing, Executing | Processing | Continuing)
            | (Executing, Continuing)
            | (Continuing, Processing | Preparing)
    ) || (to == Cancelling && !matches!(from, Idle | Cancelling | Recovering))
        || (to == Recovering && from != Recovering)
}

#[cfg(test)]
thread_local! {
    /// Whether test builds check derived state after every applied record.
    static ORACLE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}
/// Run `f` on this thread without the test-build equivalence oracle, as
/// benchmarks must. The oracle is on by default in every other test.
#[cfg(test)]
pub(crate) fn without_oracle<R>(f: impl FnOnce() -> R) -> R {
    let previous = ORACLE.replace(false);
    let result = f();
    ORACLE.set(previous);
    result
}

/// Derived state of one snapshot, for comparing live and replayed state.
#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) struct Derived {
    pub branch: OrdMap<EntryId, Arc<BranchFacts>>,
    pub root: Arc<BranchFacts>,
    pub unresolved: OrdSet<InvocationId>,
    pub invocation_by_call: OrdMap<CallKey, InvocationId>,
    pub unused_approvals: OrdMap<CallKey, ApprovalId>,
    pub unused_decisions: OrdSet<DecisionId>,
}

/// The scanning implementations that derived state replaced, kept as an oracle.
#[cfg(test)]
impl State {
    fn walked_model_at(&self, head: Option<&EntryId>) -> Result<&ModelRef> {
        for stored in self.ancestry(head)?.into_iter().rev() {
            if let EntryPayload::ModelChanged(change) = &stored.entry.payload {
                return Ok(&change.model);
            }
        }
        Ok(&self.header.initial_model)
    }
    fn walked_pending(&self, head: Option<&EntryId>) -> Result<Vec<CallRef>> {
        let mut pending = Vec::new();
        for stored in self.ancestry(head)? {
            match &stored.entry.payload {
                EntryPayload::AssistantResponse(response) => {
                    require(pending.is_empty(), "inference_inside_tool_exchange")?;
                    if response.outcome == ResponseOutcome::Completed
                        && response.finish_reason == Some(FinishReason::ToolCalls)
                    {
                        for block in &response.content {
                            if let ContentBlock::ToolCall { call_id, .. } = block {
                                pending.push(CallRef {
                                    response_entry_id: stored.entry.id.clone(),
                                    call_id: call_id.clone(),
                                });
                            }
                        }
                    }
                }
                EntryPayload::ToolResult(result) => {
                    require(
                        pending.first() == Some(&result.result.call),
                        "orphan_duplicate_or_reordered_result",
                    )?;
                    pending.remove(0);
                }
                EntryPayload::UserMessage(_)
                | EntryPayload::ModelChanged(_)
                | EntryPayload::Compaction(_) => {
                    require(pending.is_empty(), "head_inside_tool_exchange")?;
                }
            }
        }
        Ok(pending)
    }
    pub(crate) fn derived(&self) -> Derived {
        Derived {
            branch: self.branch.clone(),
            root: self.root.clone(),
            unresolved: self.unresolved.clone(),
            invocation_by_call: self.invocation_by_call.clone(),
            unused_approvals: self.unused_approvals.clone(),
            unused_decisions: self.unused_decisions.clone(),
        }
    }
    /// Assert that branch facts and live indexes equal their scanning
    /// definitions after `record` was applied.
    fn check_derived(&self, record: &Record) {
        let mut heads = vec![None, self.head.as_ref()];
        if let RecordPayload::Commit(commit) = &record.payload {
            heads.extend(commit.entries.iter().map(|entry| Some(&entry.id)));
        }
        for head in heads {
            assert_eq!(
                self.pending(head).unwrap(),
                self.walked_pending(head).unwrap().as_slice(),
                "pending at {head:?}"
            );
            assert_eq!(
                self.model_at(head).unwrap(),
                self.walked_model_at(head).unwrap(),
                "model at {head:?}"
            );
        }
        assert!(self.branch.keys().eq(self.entries.keys()));

        let unresolved: OrdSet<_> = self
            .invocations
            .iter()
            .filter(|(_, v)| v.result.is_none() || (v.uncertain && !v.recovery_resolved))
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(self.unresolved, unresolved);
        let by_call: OrdMap<_, _> = self
            .invocations
            .iter()
            .map(|(id, v)| (key(&v.intent.call), id.clone()))
            .collect();
        assert_eq!(
            by_call.len(),
            self.invocations.len(),
            "invocations per call"
        );
        assert_eq!(self.invocation_by_call, by_call);
        let unused: Vec<_> = self.approvals.iter().filter(|(_, a)| !a.used).collect();
        let approvals: OrdMap<_, _> = unused
            .iter()
            .map(|(id, a)| (key(&a.request.call), (*id).clone()))
            .collect();
        assert_eq!(approvals.len(), unused.len(), "unused approvals per call");
        assert_eq!(self.unused_approvals, approvals);
        let decisions: OrdSet<_> = self
            .decisions
            .iter()
            .filter(|(_, d)| !d.used)
            .map(|(id, _)| id.clone())
            .collect();
        assert_eq!(self.unused_decisions, decisions);
        // `TurnFinished` drains these indexes, so only the active turn holds any.
        let active = self.active_turn.as_ref();
        assert!(
            unused
                .iter()
                .all(|(_, a)| Some(&a.request.turn_id) == active)
        );
        assert!(
            self.decisions
                .values()
                .filter(|d| !d.used)
                .all(|d| Some(&d.decision.turn_id) == active)
        );
    }
}
