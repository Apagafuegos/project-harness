//! Idle-only compaction orchestration. All generation is a driver effect.
use super::*;
use crate::context::compaction::*;
use crate::context::{CompactionConfig, SessionConfiguration, StrategyId};
use crate::model::ArtifactSource;
use crate::session::{
    ConfigurationChanged, JournalEvent, Record, RecordPayload, compaction_manifest,
};
use std::sync::Arc;

#[derive(Debug)]
pub(super) struct PendingStart {
    pub command_id: CommandId,
    pub fingerprint: Digest,
    pub content: Vec<ContentBlock>,
    pub waiters: Vec<RequestId>,
}
#[derive(Debug)]
enum Purpose {
    Manual(RequestId),
    Automatic,
    Start(PendingStart),
}
#[derive(Debug)]
pub(super) struct Generation {
    token: CompactionToken,
    purpose: Purpose,
}
impl Generation {
    pub(super) fn start(&self) -> Option<&PendingStart> {
        if let Purpose::Start(start) = &self.purpose {
            Some(start)
        } else {
            None
        }
    }
    pub(super) fn start_mut(&mut self) -> Option<&mut PendingStart> {
        if let Purpose::Start(start) = &mut self.purpose {
            Some(start)
        } else {
            None
        }
    }
}
impl Runtime {
    #[cfg(test)]
    pub(in crate::runtime) fn edit_compaction_preview_for_test(
        &mut self,
        edit: impl FnOnce(&mut CompactionPreview),
    ) {
        edit(&mut self.preview.as_mut().expect("cached preview").1);
    }

    /// Inspect an unconsumed compaction preview, without calling a provider.
    pub fn inspect_context_preview(
        &self,
        preview_id: &EntryId,
    ) -> Result<CompactionPreview, Rejection> {
        let (id, preview) = self.preview.as_ref().ok_or(Rejection::StalePreview {
            field: "preview_id",
        })?;
        if id != preview_id {
            return Err(Rejection::StalePreview {
                field: "preview_id",
            });
        }
        self.validate_preview(preview)
            .map_err(|field| Rejection::StalePreview { field })?;
        Ok(preview.clone())
    }

    pub(super) fn preview_compaction(
        &mut self,
        request: RequestId,
        env: &mut dyn Env,
    ) -> Option<Result<Accepted, Rejection>> {
        if let Err(e) = self.idle_check() {
            return Some(Err(e));
        }
        if self.generation.is_some() {
            return Some(Err(Rejection::Busy));
        }
        match self.begin_compaction(Purpose::Manual(request), env) {
            Ok(()) => None,
            Err(e) => Some(Err(Rejection::Compaction(e))),
        }
    }

    fn eligible(&self) -> Result<(), CompactionError> {
        select_span(
            self.snapshot(),
            self.snapshot().head(),
            self.configuration.compaction(),
        )
        .map_err(CompactionError::NotEligible)?;
        let config = self.configuration.compaction();
        if config.strategy.needs_model() && config.summary_model.is_none() {
            return Err(CompactionError::Invalid("summary_model_unconfigured"));
        }
        Ok(())
    }

    fn begin_compaction(
        &mut self,
        purpose: Purpose,
        env: &mut dyn Env,
    ) -> Result<(), CompactionError> {
        self.eligible()?;
        self.next_token += 1;
        let token = CompactionToken(self.next_token);
        let input = CompactionInput {
            snapshot: self.snapshot().clone(),
            head: self.snapshot().head().cloned(),
            configuration: self.configuration.clone(),
            context: self.config.context.clone(),
            tools: self.definitions(),
            entry_id: env.entry_id(),
            response_id: env.response_id(),
            artifacts: ArtifactSource::default(),
        };
        self.generation = Some(Generation { token, purpose });
        self.out.push(Effect::ProposeCompaction { token, input });
        Ok(())
    }

    pub(super) fn cancel_generation(&mut self, rejection: Rejection) -> bool {
        let Some(generation) = self.generation.take() else {
            return false;
        };
        self.out.push(Effect::CancelCompaction {
            token: generation.token,
        });
        self.emit(EventKind::CompactionFailed { code: "cancelled" });
        self.fail_purpose(generation.purpose, rejection);
        true
    }

    fn fail_purpose(&mut self, purpose: Purpose, rejection: Rejection) {
        match purpose {
            Purpose::Manual(request) => self.reply(request, Err(rejection)),
            Purpose::Start(start) => {
                for request in start.waiters {
                    self.reply(request, Err(rejection.clone()));
                }
            }
            Purpose::Automatic => {}
        }
    }

    pub(super) fn compaction_proposed(
        &mut self,
        token: CompactionToken,
        result: Result<Box<CompactionPreview>, CompactionError>,
        env: &mut dyn Env,
    ) {
        if self.mode != Mode::Ready || self.generation.as_ref().is_none_or(|g| g.token != token) {
            return;
        }
        let generation = self.generation.take().expect("checked");
        let preview = match result {
            Ok(preview) => *preview,
            Err(error) => {
                self.emit(EventKind::CompactionFailed { code: error.code() });
                self.fail_purpose(generation.purpose, Rejection::Compaction(error));
                return;
            }
        };
        if let Err(field) = self.validate_preview(&preview) {
            self.emit(EventKind::CompactionFailed {
                code: "stale_preview",
            });
            self.fail_purpose(generation.purpose, Rejection::StalePreview { field });
            return;
        }
        let preview_id = env.entry_id();
        let automatic = !matches!(generation.purpose, Purpose::Manual(_));
        self.emit(EventKind::CompactionPreviewed {
            preview_id: preview_id.clone(),
            automatic,
            preview: Box::new(preview.clone()),
        });
        self.preview = Some((preview_id.clone(), preview.clone()));
        match generation.purpose {
            Purpose::Manual(request) => self.reply(
                request,
                Ok(Accepted::CompactionPreviewed {
                    preview_id,
                    preview: Box::new(preview),
                }),
            ),
            Purpose::Automatic => {
                if preview.after.flags.overflow {
                    self.preview = None;
                    self.emit(EventKind::CompactionFailed {
                        code: "context_budget_exceeded",
                    });
                } else if let Some(Err(e)) = self.commit_compaction(None, preview_id, None, env) {
                    self.emit(EventKind::CompactionFailed {
                        code: rejection_code(&e),
                    });
                }
            }
            Purpose::Start(start) => {
                // Revalidation and publication are shared with the manual path.
                // The held StartTurn is resumed only after this commit's ack.
                if let Some(Err(e)) = self.commit_compaction(None, preview_id, Some(start), env) {
                    self.emit(EventKind::CompactionFailed {
                        code: rejection_code(&e),
                    });
                }
            }
        }
    }

    fn validate_preview(&self, preview: &CompactionPreview) -> Result<(), &'static str> {
        let snapshot = self.snapshot();
        let binding = &preview.binding;
        if binding.head.as_ref() != snapshot.head() || preview.entry.parent_id != binding.head {
            return Err("head");
        }
        if snapshot.model_at(snapshot.head()).ok() != Some(&binding.session_model) {
            return Err("session_model");
        }
        // Policy first: a recorded configuration change also advances the
        // sequence, and the strategy it selects is the reason the preview died.
        if binding.policy_fingerprint != policy_fingerprint(self.configuration.compaction()) {
            return Err("policy_fingerprint");
        }
        if binding.journal_sequence != snapshot.sequence() {
            return Err("journal_sequence");
        }
        let EntryPayload::Compaction(c) = &preview.entry.payload else {
            return Err("source_fingerprint");
        };
        let span = select_span(snapshot, snapshot.head(), self.configuration.compaction())
            .map_err(|_| "source_fingerprint")?;
        let manifest = compaction_manifest(
            snapshot,
            &span.covered_entry_ids,
            &span.supersedes,
            &snapshot.configuration().sha256,
        )
        .map_err(|_| "source_fingerprint")?;
        if c.covered_entry_ids != span.covered_entry_ids
            || c.supersedes != span.supersedes
            || Digest::of(&manifest) != binding.source_fingerprint
            || manifest != preview.manifest_bytes
            || c.source_fingerprint != binding.source_fingerprint
            || c.source_manifest.sha256 != binding.source_fingerprint
            || c.source_manifest.bytes != manifest.len() as u64
            || c.configuration_fingerprint != snapshot.configuration().sha256
        {
            return Err("source_fingerprint");
        }
        if c.strategy != self.configuration.compaction().strategy.as_str()
            || c.strategy_version != self.configuration.compaction().strategy.version()
            || c.model.as_ref() != expected_summary_model(self.configuration.compaction())
        {
            return Err("policy_fingerprint");
        }
        Ok(())
    }

    pub(super) fn commit_compaction(
        &mut self,
        request: Option<RequestId>,
        preview_id: EntryId,
        start: Option<PendingStart>,
        env: &mut dyn Env,
    ) -> Option<Result<Accepted, Rejection>> {
        let result = self.idle_check().and_then(|()| {
            let Some((id, preview)) = &self.preview else {
                let field = self
                    .invalid_preview
                    .as_ref()
                    .filter(|(id, _)| id == &preview_id)
                    .map_or("preview_id", |(_, field)| *field);
                return Err(Rejection::StalePreview { field });
            };
            if id != &preview_id {
                return Err(Rejection::StalePreview {
                    field: "preview_id",
                });
            }
            self.validate_preview(preview)
                .map_err(|field| Rejection::StalePreview { field })
        });
        if let Err(e) = result {
            if let Some(start) = start {
                for request in start.waiters {
                    self.reply(request, Err(e.clone()));
                }
            }
            // A failed validation consumes the preview too; it cannot revive.
            if self
                .preview
                .as_ref()
                .is_some_and(|(id, _)| id == &preview_id)
            {
                self.preview = None;
            }
            return Some(Err(e));
        }
        let (_, preview) = self.preview.take().expect("validated");
        self.invalid_preview = Some((preview_id.clone(), "preview_id"));
        let commit = advancing(self.snapshot(), vec![preview.entry.clone()], vec![]);
        let event = EventKind::CompactionCommitted {
            preview_id: preview_id.clone(),
            preview: Box::new(preview.clone()),
        };
        self.persist(
            commit,
            InFlight {
                waiters: request
                    .into_iter()
                    .map(|request| (request, Waiter::CompactionCommitted(preview_id.clone())))
                    .collect(),
                artifacts: vec![preview.manifest_bytes],
                tool_events: vec![event],
                compaction_start: start,
                ..idle_flight()
            },
            None,
            env,
        );
        None
    }

    /// Durably select another compaction strategy while idle. The exact
    /// configuration bytes are rewritten, published and bound by one
    /// `ConfigurationChanged` at a fresh policy revision; nothing else in the
    /// configuration changes and no entry is rewritten. Every outstanding
    /// preview becomes stale, because the strategy is part of its policy
    /// fingerprint.
    pub(super) fn set_compaction_strategy(
        &mut self,
        request: RequestId,
        strategy: StrategyId,
        env: &mut dyn Env,
    ) -> Option<Result<Accepted, Rejection>> {
        if let Err(rejection) = self.idle_check() {
            return Some(Err(rejection));
        }
        let snapshot = self.snapshot().clone();
        if !matches!(snapshot.0.pending(snapshot.head()), Ok(pending) if pending.is_empty()) {
            return Some(Err(Rejection::Invalid("head_inside_tool_exchange")));
        }
        if self.configuration.compaction().strategy == strategy {
            return Some(Err(Rejection::Invalid("strategy_unchanged")));
        }
        let revision = env.policy_revision();
        let bytes = match crate::context::with_compaction_strategy(
            self.configuration.bytes(),
            strategy,
            &revision,
        ) {
            Ok(bytes) => bytes,
            Err(error) => return Some(Err(Rejection::Invalid(error.code()))),
        };
        let reference = ArtifactRef {
            sha256: Digest::of(&bytes),
            bytes: bytes.len() as u64,
        };
        let configuration = match SessionConfiguration::parse(reference.clone(), &bytes) {
            Ok(configuration) => configuration,
            Err(error) => return Some(Err(Rejection::Invalid(error.code()))),
        };
        // A strategy that needs a model cannot be selected without one: the
        // recorded configuration must always be able to generate.
        let compaction = configuration.compaction();
        if compaction.strategy.needs_model() && compaction.summary_model.is_none() {
            return Some(Err(Rejection::Invalid("summary_model_unconfigured")));
        }
        let commit = audit(
            &snapshot,
            vec![JournalEvent::ConfigurationChanged(ConfigurationChanged {
                configuration: reference,
                policy_revision: revision,
            })],
        );
        self.persist(
            commit,
            InFlight {
                artifacts: vec![bytes],
                configuration: Some(configuration),
                ..waiting(request, Waiter::CompactionStrategySet(strategy))
            },
            None,
            env,
        );
        None
    }

    pub(super) fn automatic_compaction(&mut self, env: &mut dyn Env) {
        if !self.configuration.compaction().automatic()
            || self.idle_check().is_err()
            || self.generation.is_some()
        {
            return;
        }
        let report = match self.inspect_context(None, None) {
            Ok(report) => report,
            Err(_) => return,
        };
        if !report.flags.compaction_recommended {
            return;
        }
        // One attempt per terminal turn acknowledgement; never called on a
        // compaction acknowledgement or generation failure.
        if let Err(error) = self.begin_compaction(Purpose::Automatic, env) {
            self.emit(EventKind::CompactionFailed {
                code: if report.flags.overflow {
                    "context_budget_exceeded"
                } else {
                    error.code()
                },
            });
        }
    }

    /// Build the real builder projection over a private, semantically validated
    /// opening commit. It is never written and cannot generate a summary.
    fn prospective_report(
        &self,
        content: &[ContentBlock],
        env: &mut dyn Env,
    ) -> Result<ContextReport, ContextError> {
        let snapshot = self.snapshot();
        let turn_id = env.turn_id();
        let entry = Entry {
            id: env.entry_id(),
            parent_id: snapshot.head().cloned(),
            turn_id: Some(turn_id.clone()),
            payload: EntryPayload::UserMessage(UserMessage {
                content: content.to_vec(),
            }),
        };
        let commit = advancing(
            snapshot,
            vec![entry.clone()],
            vec![
                JournalEvent::TurnStarted(TurnStarted {
                    turn_id: turn_id.clone(),
                    user_entry_id: entry.id,
                    configuration: snapshot.configuration().clone(),
                }),
                transition(&turn_id, RuntimeState::Idle, RuntimeState::Preparing),
            ],
        );
        let record = Record {
            version: crate::session::CURRENT_VERSION,
            seq: snapshot.sequence() + 1,
            record_id: env
                .entry_id()
                .as_str()
                .to_owned()
                .try_into()
                .expect("UUID identity"),
            session_id: snapshot.session_id().clone(),
            at: env.now(),
            previous_checksum: Some(snapshot.checksum().clone()),
            payload: RecordPayload::Commit(commit),
        };
        let mut state = (*snapshot.0).clone();
        state
            .apply(&record, Digest::of(b"private prospective context"))
            .map_err(|_| ContextError::Invariant("prospective_start_invalid"))?;
        Ok(self
            .builder(&self.definitions())
            .build(
                &SessionSnapshot(Arc::new(state)),
                record_commit_head(&record),
                None,
            )?
            .report)
    }

    pub(super) fn budget_start(
        &mut self,
        start: PendingStart,
        allow_compaction: bool,
        env: &mut dyn Env,
    ) {
        let report = match self.prospective_report(&start.content, env) {
            Ok(report) => report,
            Err(error) => {
                for request in start.waiters {
                    self.reply(request, Err(Rejection::Invalid(error.code())));
                }
                return;
            }
        };
        if report.flags.overflow {
            let eligibility = self.eligible();
            if allow_compaction
                && self.configuration.compaction().automatic()
                && eligibility.is_ok()
            {
                // Eligibility was checked before taking ownership of the waiters.
                self.begin_compaction(Purpose::Start(start), env)
                    .expect("same pure snapshot");
                return;
            }
            let reason = match eligibility {
                Err(CompactionError::NotEligible(reason)) => Some(reason),
                _ => None,
            };
            for request in start.waiters {
                self.reply(request, Err(Rejection::ContextBudgetExceeded { reason }));
            }
            self.emit(EventKind::CompactionFailed {
                code: "context_budget_exceeded",
            });
            return;
        }
        self.commit_open_turn(
            start.command_id,
            start.content,
            start.fingerprint,
            start.waiters,
            env,
        );
    }
}
/// The model a committed compaction must record for this policy: the configured
/// summary model for the strategies that use one, and nothing otherwise.
fn expected_summary_model(config: &CompactionConfig) -> Option<&ModelRef> {
    config
        .strategy
        .needs_model()
        .then_some(config.summary_model.as_ref())
        .flatten()
}

fn record_commit_head(record: &Record) -> Option<&EntryId> {
    match &record.payload {
        RecordPayload::Commit(commit) => commit.head.to.as_ref(),
        _ => None,
    }
}
fn rejection_code(rejection: &Rejection) -> &'static str {
    match rejection {
        Rejection::Compaction(error) => error.code(),
        Rejection::StalePreview { .. } => "stale_preview",
        Rejection::ContextBudgetExceeded { .. } => "context_budget_exceeded",
        _ => "compaction_rejected",
    }
}
