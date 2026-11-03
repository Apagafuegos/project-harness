//! Durable session handover and sequential navigation work for the terminal shell.
use super::{Action, App, SessionInfo, runtime::Commands};
use crate::wiring::{self, SessionOptions};
use futures_util::{Stream, future::BoxFuture};
use harness_core::{model::ModelProvider, runtime::*, session::*};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::sync::{Semaphore, watch};

/// Result of shell-only navigation; no engine command or side effect is retried.
pub enum NavigationReply {
    /// Completed catalog scan, after its shared locks have been released.
    Sessions(Vec<SessionCatalogEntry>),
    /// A new handle is live and its subscription will begin with a reset.
    Opened {
        /// Nonfatal creation diagnostics.
        warnings: Vec<String>,
        /// Explicit continuation switch after the new reset.
        startup: Option<Command>,
        /// Coherent replacement view; never compare startup against the old session.
        view: Box<RuntimeView>,
    },
}
/// Owned navigation future, kept beside command futures in the nonblocking loop.
pub type NavigationFuture = BoxFuture<'static, Result<NavigationReply, String>>;
pub(super) struct PendingNavigation(VecDeque<NavigationFuture>);
impl PendingNavigation {
    pub(super) fn new() -> Self {
        Self(VecDeque::new())
    }
    pub(super) fn push(&mut self, future: NavigationFuture) {
        self.0.push_back(future);
    }
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub(super) async fn next(&mut self) -> Option<Result<NavigationReply, String>> {
        let result = self.0.front_mut()?.await;
        self.0.pop_front();
        Some(result)
    }
}

pub(super) fn dispatch(action: Action, handle: &impl Commands, pending: &mut PendingNavigation) {
    pending.push(handle.navigation(action));
}
pub(super) fn apply(
    app: &mut App,
    result: Result<NavigationReply, String>,
) -> Option<Option<Command>> {
    match result {
        Ok(NavigationReply::Sessions(entries)) => app.on_sessions(entries),
        Ok(NavigationReply::Opened {
            warnings,
            startup,
            view,
        }) => {
            app.navigation_reset();
            app.on_subscription(SubscriptionItem::Reset(ClientView::from(*view)));
            if !warnings.is_empty() {
                app.set_error(warnings.join("; "));
            }
            return Some(startup);
        }
        Err(error) => {
            app.navigation_finished();
            app.set_error(error);
        }
    }
    None
}

pub(super) fn sequence(
    futures: Vec<BoxFuture<'static, Result<Accepted, Rejection>>>,
) -> BoxFuture<'static, Result<Accepted, Rejection>> {
    Box::pin(async move {
        let mut last = Err(Rejection::Invalid("empty_commands"));
        for future in futures {
            last = Ok(future.await?);
        }
        last
    })
}

struct State {
    handle: SessionHandle,
    session: SessionId,
}
/// One serialized session owner. The single permit orders catalog scans,
/// commands and durable handover, so a list's shared journal locks can never
/// overlap this client's writer open. The state guard itself is only held for
/// the non-awaiting copy or replacement of the handle. The permit covers work,
/// not turns: core busy/recovery rules still decide every command.
#[derive(Clone)]
pub struct SessionCommands {
    order: Arc<Semaphore>,
    state: Arc<std::sync::Mutex<State>>,
    options: SessionOptions,
    provider: Arc<dyn ModelProvider>,
    changed: watch::Sender<SessionHandle>,
}
impl SessionCommands {
    /// Adopt a session opened by shared wiring.
    pub fn new(
        opened: wiring::WiredSession,
        options: SessionOptions,
        provider: Arc<dyn ModelProvider>,
    ) -> Self {
        let (changed, _) = watch::channel(opened.handle.clone());
        Self {
            order: Arc::new(Semaphore::new(1)),
            state: Arc::new(std::sync::Mutex::new(State {
                handle: opened.handle,
                session: opened.session,
            })),
            options,
            provider,
            changed,
        }
    }
    /// The current session identity, also useful to injected-loop clients.
    /// Waits for an in-flight handover, so a caller never sees a shut-down
    /// handle between the two halves of a replacement.
    pub async fn session(&self) -> SessionId {
        let _permit = self.order.acquire().await;
        self.state().session.clone()
    }
    /// A coherent current handle for client observations. Never cache across handover.
    pub async fn handle(&self) -> SessionHandle {
        let _permit = self.order.acquire().await;
        self.state().handle.clone()
    }
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("session state not poisoned")
    }
    /// Subscription follows replacements. The new subscription's first reset
    /// supersedes all old events; closure during handover is not a loop failure.
    pub fn subscriptions(&self) -> impl Stream<Item = SubscriptionItem> + use<> {
        let mut receiver = self.changed.subscribe();
        async_stream::stream! {
            let mut sub = receiver.borrow_and_update().subscribe();
            loop {
                tokio::select! {
                    biased;
                    changed = receiver.changed() => {
                        if changed.is_err() { break; }
                        sub = receiver.borrow_and_update().subscribe();
                    }
                    item = sub.next() => match item {
                        Some(item) => yield item,
                        None => {
                            if receiver.changed().await.is_err() { break; }
                            sub = receiver.borrow_and_update().subscribe();
                        }
                    }
                }
            }
        }
    }
    /// Drain the current engine and release its writer lock on every exit path.
    pub async fn shutdown(&self) {
        let _permit = self.order.acquire().await;
        let handle = self.state().handle.clone();
        handle.shutdown().await;
    }
    /// Construct the initial pure client with the same merged catalog.
    pub fn app(&self) -> App {
        let mut app = App::with_theme(
            SessionInfo {
                workspace: self.options.workspace.clone(),
                model: self.options.model.clone(),
                context_limit: None,
            },
            super::theme::Theme::from_environment(),
        );
        app.set_catalog(self.options.driver.runtime.context.catalog.clone());
        app
    }
}
impl Commands for SessionCommands {
    // Read-only inspections use the current handle without the navigation
    // permit, so they never wait behind a handover. The App discards a report
    // whose head no longer matches its view.
    fn inspect_context(
        &self,
        head: Option<EntryId>,
        model_override: Option<ModelRef>,
    ) -> BoxFuture<'static, Result<harness_core::context::ContextReport, InspectError>> {
        let handle = self.state().handle.clone();
        Commands::inspect_context(&handle, head, model_override)
    }
    fn inspect_preview(
        &self,
        id: EntryId,
    ) -> BoxFuture<'static, Result<harness_core::context::compaction::CompactionPreview, Rejection>>
    {
        let handle = self.state().handle.clone();
        Commands::inspect_preview(&handle, id)
    }
    fn command(&self, command: Command) -> BoxFuture<'static, Result<Accepted, Rejection>> {
        let order = self.order.clone();
        let state = self.state.clone();
        Box::pin(async move {
            let _permit = order.acquire().await;
            let handle = state
                .lock()
                .expect("session state not poisoned")
                .handle
                .clone();
            handle.command(command).await
        })
    }
    fn sequence(&self, commands: Vec<Command>) -> BoxFuture<'static, Result<Accepted, Rejection>> {
        let order = self.order.clone();
        let state = self.state.clone();
        Box::pin(async move {
            // One permit keeps a navigation's SelectHead and StartTurn adjacent.
            let _permit = order.acquire().await;
            let handle = state
                .lock()
                .expect("session state not poisoned")
                .handle
                .clone();
            let mut last = Err(Rejection::Invalid("empty_commands"));
            for command in commands {
                last = Ok(handle.command(command).await?);
            }
            last
        })
    }
    fn startup(&self) -> Option<Command> {
        wiring::continuation_switch(&self.options)
            .map(|(model, policy)| Command::SwitchModel { model, policy })
    }
    fn navigation(&self, action: Action) -> NavigationFuture {
        let order = self.order.clone();
        let state = self.state.clone();
        let mut options = self.options.clone();
        let provider = self.provider.clone();
        let changed = self.changed.clone();
        Box::pin(async move {
            let _permit = order.acquire().await;
            let current = state
                .lock()
                .expect("session state not poisoned")
                .handle
                .clone();
            if matches!(action, Action::ListSessions) {
                let entries = tokio::task::spawn_blocking(move || {
                    DataRoot::open(&options.data_root, false)
                        .and_then(|root| root.sessions())
                        .map_err(|_| "session_catalog_unavailable".to_owned())
                })
                .await
                .map_err(|_| "session_catalog_unavailable".to_owned())??;
                return Ok(NavigationReply::Sessions(entries));
            }
            let view = current.view().await.map_err(|_| "runtime fault")?;
            // Check the authoritative view under the same permit as pending starts.
            if view.active_turn.is_some() || view.phase != Phase::Durable(RuntimeState::Idle) {
                return Err(match view.phase {
                    Phase::AwaitingResolution => "command rejected: resolution_required",
                    Phase::RecoveryRequired => "command rejected: recovery_required",
                    Phase::Faulted => "command rejected: runtime fault",
                    _ => "command rejected: busy",
                }
                .into());
            }
            options.session = match action {
                Action::OpenSession(id) => Some(id),
                Action::NewSession => None,
                _ => return Err("invalid_navigation".into()),
            };
            // Preflight workspace/status without taking writer ownership. The
            // returned vector owns no locks; open is strictly after this scan.
            if let Some(id) = &options.session {
                let root =
                    DataRoot::open(&options.data_root, false).map_err(|_| "invalid_data_root")?;
                match JournalStore::inspect(&root, id) {
                    Ok(inspection) => {
                        if inspection.diagnostic.is_some() {
                            return Err("needs repair: run harness journal repair".into());
                        }
                        let snapshot = inspection.snapshot.ok_or("missing_session_header")?;
                        if snapshot.header().workspace != options.workspace.to_string_lossy() {
                            return Err("different workspace: open refused".into());
                        }
                    }
                    Err(StoreError::Locked) => {} // open_retry owns the bounded lock retry.
                    Err(_) => return Err("session_unreadable".into()),
                }
            }
            let previous = state
                .lock()
                .expect("session state not poisoned")
                .session
                .clone();
            current.shutdown().await;
            let opened = open_retry(&options, provider.clone()).await;
            let (opened, failure) = match opened {
                Ok(opened) => (opened, None),
                Err(error) => {
                    options.session = Some(previous);
                    let restored = open_retry(&options, provider.clone()).await.map_err(
                        |_| "session handover failed; previous session could not be reopened",
                    )?;
                    (restored, Some(error))
                }
            };
            // Reapply the exact continuation policy after recovery, as at startup.
            // Busy/recovery waits are driven by views; never retry a switch.
            if failure.is_none() {
                loop {
                    let view = opened.handle.view().await.map_err(|_| "runtime fault")?;
                    if view.phase != Phase::RecoveryRequired {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                // Refusals are rendered by the normal command path after reset.
            }
            {
                let mut next = state.lock().expect("session state not poisoned");
                next.session = opened.session;
                next.handle = opened.handle.clone();
            }
            changed.send_replace(opened.handle.clone());
            if let Some(error) = failure {
                return Err(error);
            }
            let startup = wiring::continuation_switch(&options)
                .map(|(model, policy)| Command::SwitchModel { model, policy });
            let view = Box::new(opened.handle.view().await.map_err(|_| "runtime fault")?);
            Ok(NavigationReply::Opened {
                warnings: opened.warnings,
                startup,
                view,
            })
        })
    }
}

async fn open_retry(
    options: &SessionOptions,
    provider: Arc<dyn ModelProvider>,
) -> Result<wiring::WiredSession, String> {
    for attempt in 0..2 {
        match wiring::open_session(options, provider.clone()).await {
            Ok(opened) => return Ok(opened),
            Err((_, error)) if error == "session_locked" && attempt == 0 => {
                tokio::time::sleep(Duration::from_millis(30)).await
            }
            Err((_, error)) if error == "session_locked" => {
                return Err("busy: session_locked (after one retry)".into());
            }
            Err((_, error)) => return Err(error),
        }
    }
    unreachable!()
}
