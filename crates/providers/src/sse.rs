//! Bounded SSE framing and an owned, cancellable reader task.
use crate::{
    error::TransportError,
    transport::{ByteStream, controlled},
};
use futures_core::Stream;
use futures_util::StreamExt;
use harness_core::{model::OperationControl, session::ModelErrorCode};
use std::{pin::Pin, sync::Arc};

/// SSE fields. `retry` is metadata only: it never triggers a retry.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct SseEvent {
    /// Optional event type.
    pub event: Option<String>,
    /// Data lines joined by LF, without the final LF.
    pub data: String,
    /// Event ID, if specified (NUL-containing IDs are ignored).
    pub id: Option<String>,
    /// Reconnection hint in milliseconds; never acted upon.
    pub retry: Option<u64>,
}
/// Explicit memory and wire-byte limits.
#[derive(Clone, Debug)]
pub struct SseConfig {
    /// Maximum wire bytes in an event, including framing/comments.
    pub max_event_bytes: usize,
    /// Maximum wire bytes across the response.
    pub max_response_bytes: usize,
    /// Maximum reserved event bytes between reader and consumer.
    pub max_queue_bytes: usize,
    /// Maximum queued events.
    pub max_queue_events: usize,
}
impl Default for SseConfig {
    fn default() -> Self {
        Self {
            max_event_bytes: 256 * 1024,
            max_response_bytes: 4 * 1024 * 1024,
            max_queue_bytes: 1024 * 1024,
            max_queue_events: 256,
        }
    }
}
/// An owned event stream. Starts on first poll; drop aborts its only worker.
pub type SseStream = Pin<Box<dyn Stream<Item = Result<SseEvent, TransportError>> + Send>>;
/// Frame bytes from a controlled HTTP setup future. The task owns setup and
/// response, so cancellation/deadline closes even while the consumer is paused.
/// Reservations are conservative: each queued event reserves its maximum size.
pub fn read(
    setup: impl std::future::Future<Output = Result<ByteStream, TransportError>> + Send + 'static,
    config: SseConfig,
    control: OperationControl,
) -> SseStream {
    Box::pin(async_stream::stream! {
        if config.max_event_bytes == 0 || config.max_event_bytes > config.max_queue_bytes
            || config.max_queue_bytes > u32::MAX as usize
            || config.max_queue_bytes > tokio::sync::Semaphore::MAX_PERMITS || config.max_queue_events == 0
            || config.max_queue_events > tokio::sync::Semaphore::MAX_PERMITS {
            yield Err(TransportError::new(ModelErrorCode::InvalidRequest, "invalid SSE bounds"));
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel(config.max_queue_events);
        let worker_control = control.clone();
        let task = tokio::spawn(async move {
            let result = controlled(&worker_control, async {
                let bytes = setup.await?;
                let mut parser = Parser::new(bytes, config.clone());
                let budget = Arc::new(tokio::sync::Semaphore::new(config.max_queue_bytes));
                loop {
                    let slot = tx.reserve().await.map_err(|_| TransportError::stopped(false))?;
                    let permit = budget.clone().acquire_many_owned(config.max_event_bytes as u32).await.map_err(|_| TransportError::stopped(false))?;
                    match parser.next().await? {
                        Some(event) => slot.send(Ok((event, permit))),
                        None => break,
                    }
                }
                Ok(())
            }).await;
            if let Err(error) = result { let _ = tx.send(Err(error)).await; }
        });
        let _owner = TaskOwner(task);
        loop {
            match controlled(&control, async { Ok(rx.recv().await) }).await {
                Ok(Some(Ok((event, permit)))) => { drop(permit); yield Ok(event); }
                Ok(Some(Err(e))) | Err(e) => { yield Err(e); break; }
                Ok(None) => break,
            }
        }
    })
}
struct TaskOwner(tokio::task::JoinHandle<()>);
impl Drop for TaskOwner {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct Parser {
    bytes: ByteStream,
    config: SseConfig,
    chunk: Vec<u8>,
    offset: usize,
    total: usize,
    size: usize,
    line: Vec<u8>,
    event: SseEvent,
    data: bool,
    after_cr: bool,
    bom: bool,
}
impl Parser {
    fn new(bytes: ByteStream, config: SseConfig) -> Self {
        Self {
            bytes,
            config,
            chunk: vec![],
            offset: 0,
            total: 0,
            size: 0,
            line: vec![],
            event: SseEvent::default(),
            data: false,
            after_cr: false,
            bom: true,
        }
    }
    async fn next(&mut self) -> Result<Option<SseEvent>, TransportError> {
        loop {
            if self.offset == self.chunk.len() {
                match self.bytes.next().await {
                    Some(Ok(chunk)) => {
                        self.chunk = chunk;
                        self.offset = 0;
                    }
                    Some(Err(e)) => return Err(e),
                    None => {
                        return if self.size != 0 || !self.line.is_empty() {
                            Err(malformed("truncated SSE event"))
                        } else {
                            Ok(None)
                        };
                    }
                }
                if self.chunk.is_empty() {
                    continue;
                }
            }
            let b = self.chunk[self.offset];
            self.offset += 1;
            self.total = self.total.saturating_add(1);
            if self.total > self.config.max_response_bytes {
                return Err(limit());
            }
            if self.after_cr {
                self.after_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            self.size = self.size.saturating_add(1);
            if self.size > self.config.max_event_bytes {
                return Err(limit());
            }
            if b == b'\r' || b == b'\n' {
                self.after_cr = b == b'\r';
                let line = std::mem::take(&mut self.line);
                let line =
                    std::str::from_utf8(&line).map_err(|_| malformed("invalid SSE UTF-8"))?;
                let line = if self.bom {
                    self.bom = false;
                    line.strip_prefix('\u{feff}').unwrap_or(line)
                } else {
                    line
                };
                if line.is_empty() {
                    self.size = 0;
                    let event = std::mem::take(&mut self.event);
                    if std::mem::take(&mut self.data) {
                        return Ok(Some(event));
                    }
                } else if !line.starts_with(':') {
                    let (field, value) = line.split_once(':').unwrap_or((line, ""));
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    match field {
                        "event" => self.event.event = Some(value.into()),
                        "data" => {
                            if self.data {
                                self.event.data.push('\n');
                            }
                            self.event.data.push_str(value);
                            self.data = true;
                        }
                        "id" if !value.contains('\0') => self.event.id = Some(value.into()),
                        "retry"
                            if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) =>
                        {
                            self.event.retry = value.parse().ok()
                        }
                        _ => {}
                    }
                }
            } else {
                self.line.push(b);
            }
        }
    }
}
fn malformed(message: &'static str) -> TransportError {
    TransportError::new(ModelErrorCode::MalformedStream, message)
}
fn limit() -> TransportError {
    TransportError::new(ModelErrorCode::LimitExceeded, "SSE byte limit exceeded")
}
