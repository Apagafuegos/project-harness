use crate::session::ContentBlock;
use std::{
    io::Read,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

#[derive(Default)]
struct Bytes {
    combined: Vec<u8>,
    streams: [Vec<u8>; 2],
}
pub(super) struct Capture {
    bytes: Arc<Mutex<Bytes>>,
    pub overflow: Arc<AtomicBool>,
    pub io_error: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}
impl Capture {
    pub fn start(
        stdout: impl Read + Send + 'static,
        stderr: impl Read + Send + 'static,
        cap: usize,
    ) -> Self {
        let mut capture = Self {
            bytes: Arc::default(),
            overflow: Arc::default(),
            io_error: Arc::default(),
            threads: Vec::new(),
        };
        capture.reader(stdout, 0, cap);
        capture.reader(stderr, 1, cap);
        capture
    }
    fn reader(&mut self, mut input: impl Read + Send + 'static, stream: usize, cap: usize) {
        let bytes = self.bytes.clone();
        let overflow = self.overflow.clone();
        let io_error = self.io_error.clone();
        self.threads.push(thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                let n = match input.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        io_error.store(true, Ordering::Release);
                        break;
                    }
                };
                let mut data = bytes.lock().expect("capture mutex");
                let keep = n.min(cap.saturating_sub(data.combined.len()));
                data.combined.extend_from_slice(&buffer[..keep]);
                data.streams[stream].extend_from_slice(&buffer[..keep]);
                if keep < n {
                    overflow.store(true, Ordering::Release);
                }
                // Continue draining until namespace teardown closes every writer.
            }
        }));
    }
    pub fn finish(self) -> (Vec<u8>, Vec<Vec<u8>>, bool, bool) {
        for task in self.threads {
            if task.join().is_err() {
                self.io_error.store(true, Ordering::Release);
            }
        }
        let mut bytes = self.bytes.lock().expect("capture mutex");
        // ExecutionOutcome uses raw artifact bytes: stdout first, stderr second.
        let artifacts = bytes.streams.iter_mut().map(std::mem::take).collect();
        (
            std::mem::take(&mut bytes.combined),
            artifacts,
            self.overflow.load(Ordering::Acquire),
            self.io_error.load(Ordering::Acquire),
        )
    }
}

/// Bound the shared outcome's compact JSON, not just its unescaped text.
pub(super) fn content(bytes: &[u8], cap: usize) -> (Vec<ContentBlock>, bool) {
    let decoded = String::from_utf8_lossy(bytes);
    let lossy = matches!(decoded, std::borrow::Cow::Owned(_));
    let mut low = 0;
    let mut high = cap;
    let mut best = vec![ContentBlock::Text {
        text: String::new(),
    }];
    let mut truncated = true;
    while low <= high {
        let budget = low + (high - low) / 2;
        let (text, elided) = render(&decoded, budget);
        let candidate = vec![ContentBlock::Text { text }];
        if serde_json::to_vec(&candidate)
            .expect("text serialization")
            .len()
            <= cap
        {
            best = candidate;
            truncated = lossy || elided;
            low = budget + 1;
        } else if budget == 0 {
            break;
        } else {
            high = budget - 1;
        }
    }
    (best, truncated)
}

#[cfg(test)]
fn model_text(bytes: &[u8], cap: usize) -> (String, bool) {
    let decoded = String::from_utf8_lossy(bytes);
    let (text, elided) = render(&decoded, cap);
    (
        text,
        elided || matches!(decoded, std::borrow::Cow::Owned(_)),
    )
}
fn render(decoded: &str, cap: usize) -> (String, bool) {
    const MARKER: &str = "\n[... output truncated ...]\n";
    const SHORT: &str = "[truncated]";
    if decoded.len() <= cap {
        return (decoded.to_owned(), false);
    }
    let marker = if cap >= MARKER.len() {
        MARKER
    } else {
        &SHORT[..cap.min(SHORT.len())]
    };
    let budget = cap - marker.len();
    let mut head = budget / 2;
    while !decoded.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = decoded.len() - (budget - budget / 2);
    while !decoded.is_char_boundary(tail) {
        tail += 1;
    }
    (
        format!("{}{}{}", &decoded[..head], marker, &decoded[tail..]),
        true,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utf8_head_tail_and_marker_respect_the_byte_cap() {
        let bytes = "é".repeat(1000).into_bytes();
        let (text, truncated) = model_text(&bytes, 128);
        assert!(truncated && text.len() <= 128);
        assert!(text.starts_with('é') && text.ends_with('é') && text.contains("truncated"));
        let (text, truncated) = model_text(&[255; 1000], 128);
        assert!(truncated && text.len() <= 128);
    }
    #[test]
    fn compact_json_bounds_include_escaping_and_block_overhead() {
        for bytes in [
            vec![0; 40000],
            vec![b'"'; 40000],
            vec![255; 40000],
            b"\0".repeat(20),
        ] {
            let (blocks, truncated) = content(&bytes, 128);
            assert!(serde_json::to_vec(&blocks).unwrap().len() <= 128);
            assert!(truncated);
        }
    }
    #[test]
    fn capture_is_bounded_across_both_streams() {
        let capture = Capture::start(
            std::io::Cursor::new(vec![b'a'; 200]),
            std::io::Cursor::new(vec![b'b'; 200]),
            100,
        );
        let (bytes, artifacts, overflow, error) = capture.finish();
        assert_eq!(bytes.len(), 100);
        assert_eq!(artifacts.iter().map(Vec::len).sum::<usize>(), 100);
        assert!(overflow && !error);
    }
}
