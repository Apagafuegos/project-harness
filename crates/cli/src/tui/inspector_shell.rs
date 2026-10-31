//! Mechanical asynchronous inspection plumbing, separate from the pure App.
use super::{Action, App, runtime::Commands};
use futures_util::{future::BoxFuture, stream::FuturesUnordered};
use harness_core::{
    context::{ContextReport, compaction::CompactionPreview},
    runtime::{InspectError, Rejection},
    session::EntryId,
};

pub(super) type Inspections = FuturesUnordered<BoxFuture<'static, Inspection>>;
pub(super) enum Inspection {
    Context(Box<Result<ContextReport, InspectError>>),
    Preview(EntryId, Box<Result<CompactionPreview, Rejection>>),
}

pub(super) fn dispatch(action: Action, handle: &impl Commands, pending: &mut Inspections) {
    match action {
        Action::InspectContext {
            head,
            model_override,
        } => {
            let future = handle.inspect_context(head, model_override);
            pending.push(Box::pin(async move {
                Inspection::Context(Box::new(future.await))
            }));
        }
        Action::InspectPreview(id) => {
            let future = handle.inspect_preview(id.clone());
            pending.push(Box::pin(async move {
                Inspection::Preview(id, Box::new(future.await))
            }));
        }
        _ => unreachable!("inspection action"),
    }
}

pub(super) fn deliver(app: &mut App, reply: Inspection) {
    match reply {
        Inspection::Context(result) => app.on_context_report(*result),
        Inspection::Preview(id, result) => app.on_compaction_preview(id, *result),
    }
}
