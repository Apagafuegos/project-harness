//! Polled by hand with a no-op waker: no runtime, no clock.

use std::task::Waker;

use super::*;
use crate::{
    model::ModelInput,
    session::{ModelRef, ResponseId},
};

fn request(n: u128) -> ModelRequest {
    let id = uuid::Uuid::from_u128((0x019a_0000_0000_u128 << 80) | (0x7 << 76) | (0b10 << 62) | n);
    ModelRequest {
        artifacts: Default::default(),
        response_id: ResponseId::try_from(id.to_string()).unwrap(),
        input: ModelInput {
            model: ModelRef {
                provider_instance: "mock".into(),
                model: "scripted".into(),
            },
            items: Vec::new(),
            tools: Vec::new(),
            options: super::super::InferenceOptions {
                max_output_tokens: 4096,
                reasoning_effort: None,
                provider_options: Default::default(),
            },
            continuation: None,
        },
    }
}

fn poll(stream: &mut ModelEventStream) -> Poll<Option<ProviderEvent>> {
    stream
        .as_mut()
        .poll_next(&mut Context::from_waker(Waker::noop()))
}

fn open(provider: &ScriptedProvider, n: u128) -> (ModelEventStream, CancellationToken) {
    let control = OperationControl::default();
    let cancel = control.cancel.clone();
    (provider.stream(request(n), control), cancel)
}

#[test]
fn events_are_yielded_in_order_then_eof_repeats() {
    let provider = ScriptedProvider::new([Script::new([
        Step::Event(ProviderEvent::Started),
        Step::text(0, "a"),
        // Yielded as written, even after the terminal event.
        Step::completed(FinishReason::Stop),
        Step::text(0, "after"),
    ])]);
    let (mut stream, _) = open(&provider, 1);
    assert_eq!(poll(&mut stream), Poll::Ready(Some(ProviderEvent::Started)));
    assert!(matches!(
        poll(&mut stream),
        Poll::Ready(Some(ProviderEvent::TextDelta { block_index: 0, text })) if text == "a"
    ));
    assert!(matches!(
        poll(&mut stream),
        Poll::Ready(Some(ProviderEvent::Finished {
            outcome: ProviderOutcome::Completed,
            finish_reason: Some(FinishReason::Stop),
            error: None,
            continuation: None,
        }))
    ));
    assert!(matches!(
        poll(&mut stream),
        Poll::Ready(Some(ProviderEvent::TextDelta { .. }))
    ));
    assert_eq!(poll(&mut stream), Poll::Ready(None));
    assert_eq!(poll(&mut stream), Poll::Ready(None));
    let [record] = &provider.streams()[..] else {
        panic!("one stream")
    };
    assert_eq!(record.request, request(1));
    assert_eq!(record.yielded, 4);
    assert!(!record.cancelled && !record.dropped);
    drop(stream);
    assert!(provider.streams()[0].dropped);
}

#[test]
fn wait_holds_until_released_or_the_sender_is_dropped() {
    let (release, wait) = oneshot::channel();
    let (dropped, wait_dropped) = oneshot::channel::<()>();
    let provider = ScriptedProvider::new([Script::new([
        Step::Event(ProviderEvent::Started),
        Step::Wait(wait),
        Step::text(0, "released"),
        Step::Wait(wait_dropped),
    ])]);
    let (mut stream, cancel) = open(&provider, 1);
    assert!(poll(&mut stream).is_ready());
    assert!(poll(&mut stream).is_pending());
    // Cancellation does not release a wait.
    cancel.cancel();
    assert!(poll(&mut stream).is_pending());
    release.send(()).unwrap();
    assert!(matches!(
        poll(&mut stream),
        Poll::Ready(Some(ProviderEvent::TextDelta { .. }))
    ));
    assert!(poll(&mut stream).is_pending());
    drop(dropped);
    assert_eq!(poll(&mut stream), Poll::Ready(None));
    assert!(provider.streams()[0].cancelled);
}

#[test]
fn await_cancel_expresses_confirmation_and_cancellation_during_setup() {
    let provider = ScriptedProvider::new([
        // Mid-stream: partial text, then a confirmed cancellation.
        Script::new([
            Step::Event(ProviderEvent::Started),
            Step::text(0, "partial"),
            Step::AwaitCancel,
            Step::cancelled(),
        ]),
        // During setup: nothing before the cancellation, then EOF.
        Script::new([Step::AwaitCancel]),
    ]);
    let (mut stream, cancel) = open(&provider, 1);
    assert!(poll(&mut stream).is_ready());
    assert!(poll(&mut stream).is_ready());
    assert!(poll(&mut stream).is_pending());
    assert!(poll(&mut stream).is_pending());
    cancel.cancel();
    assert!(matches!(
        poll(&mut stream),
        Poll::Ready(Some(ProviderEvent::Finished {
            outcome: ProviderOutcome::Cancelled,
            ..
        }))
    ));
    assert_eq!(poll(&mut stream), Poll::Ready(None));

    let (mut setup, cancel) = open(&provider, 2);
    assert!(poll(&mut setup).is_pending());
    cancel.cancel();
    assert_eq!(poll(&mut setup), Poll::Ready(None));
    assert_eq!(provider.streams()[1].yielded, 0);
}

#[test]
fn stall_ignores_cancellation_until_dropped() {
    let provider = ScriptedProvider::new([Script::new([
        Step::Event(ProviderEvent::Started),
        Step::Stall,
        Step::text(0, "never"),
    ])]);
    let (mut stream, cancel) = open(&provider, 1);
    assert!(poll(&mut stream).is_ready());
    cancel.cancel();
    for _ in 0..3 {
        assert!(poll(&mut stream).is_pending());
    }
    drop(stream);
    let record = &provider.streams()[0];
    assert_eq!(record.yielded, 1);
    assert!(record.cancelled && record.dropped);
}

#[test]
fn scripts_are_taken_in_order_and_exhaustion_is_a_setup_failure() {
    let provider = ScriptedProvider::default();
    provider.push(Script::new([Step::Event(ProviderEvent::Started)]));
    assert_eq!(provider.remaining(), 1);
    let (mut first, _) = open(&provider, 1);
    assert_eq!(poll(&mut first), Poll::Ready(Some(ProviderEvent::Started)));
    assert_eq!(provider.remaining(), 0);
    let (mut second, _) = open(&provider, 2);
    let Poll::Ready(Some(ProviderEvent::Finished {
        outcome: ProviderOutcome::Failed,
        error: Some(error),
        ..
    })) = poll(&mut second)
    else {
        panic!("setup failure")
    };
    assert_eq!(error.code, ModelErrorCode::ProviderFailure);
    assert!(error.message.starts_with("no_script"));
    assert_eq!(poll(&mut second), Poll::Ready(None));
    let requests: Vec<_> = provider
        .streams()
        .into_iter()
        .map(|record| record.request)
        .collect();
    assert_eq!(requests, [request(1), request(2)]);
}
