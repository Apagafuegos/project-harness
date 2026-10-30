//! Attributed loopback server shared by the offline acceptance tour.
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[allow(dead_code, clippy::duplicate_mod)]
#[path = "../../../../tests/support/loopback.rs"]
mod loopback;

pub struct Server {
    pub url: String,
    pub nonce: String,
    replies: Arc<Mutex<VecDeque<String>>>,
    task: tokio::task::JoinHandle<()>,
    _probe: loopback::SideProbe,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    pub async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let address = listener.local_addr().expect("address");
        let probe = loopback::SideProbe::register(address);
        let path = format!("/fixture/{}", loopback::nonce());
        let nonce = loopback::nonce();
        let url = format!("http://{address}{path}");
        let replies = Arc::new(Mutex::new(VecDeque::<String>::new()));
        let queue = replies.clone();
        let expected_nonce = nonce.clone();
        let task = tokio::spawn(async move {
            let mut handlers = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = tokio::select! {
                    peer = listener.accept() => peer.expect("accept"),
                    Some(result) = handlers.join_next(), if !handlers.is_empty() => {
                        result.expect("handler");
                        continue;
                    }
                };
                let queue = queue.clone();
                let path = path.clone();
                let nonce = expected_nonce.clone();
                handlers.spawn(async move {
                    let request = tokio::time::timeout(Duration::from_secs(2), async {
                        let mut bytes = Vec::new();
                        let mut chunk = [0u8; 4096];
                        loop {
                            let count = socket.read(&mut chunk).await.ok()?;
                            if count == 0 || bytes.len() + count > 2 * 1024 * 1024 { return None; }
                            bytes.extend_from_slice(&chunk[..count]);
                            if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                                let head = std::str::from_utf8(&bytes[..end]).ok()?;
                                if head.lines().next() != Some(format!("POST {path} HTTP/1.1").as_str()) { return None; }
                                let head = head.to_ascii_lowercase();
                                if !head.lines().any(|l| l == format!("x-harness-test: {nonce}")) { return None; }
                                let length: usize = head.lines().find_map(|l| l.strip_prefix("content-length:"))?.trim().parse().ok()?;
                                if bytes.len() >= end + 4 + length { return Some(()); }
                            }
                        }
                    }).await;
                    if !matches!(request, Ok(Some(()))) { return; }
                    let reply = queue.lock().expect("queue").pop_front().expect("queued fixture");
                    let (_, body) = reply.split_once("---\n").expect("separator");
                    let header = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                    socket.write_all(header.as_bytes()).await.expect("headers");
                    socket.write_all(body.as_bytes()).await.expect("body");
                    socket.shutdown().await.expect("close");
                });
            }
        });
        Self {
            url,
            nonce,
            replies,
            task,
            _probe: probe,
        }
    }
    pub fn reply(&self, fixture: &str) {
        self.replies
            .lock()
            .expect("queue")
            .push_back(fixture.into());
    }
    pub fn reply_text(&self, text: &str) {
        let events = [
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"msg_summary","type":"message","role":"assistant","content":[],"stop_reason":null,"stop_sequence":null}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ];
        let mut fixture = String::from("status: 200\n---\n");
        for (event, data) in events {
            fixture.push_str(&format!("event: {event}\ndata: {data}\n\n"));
        }
        self.reply(&fixture);
    }
}
