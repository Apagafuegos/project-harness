//! Test-only loopback attribution and opt-in side-probe registry.
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub fn nonce() -> String {
    let mut bytes = [0_u8; 24];
    std::fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut bytes)
        .unwrap();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Each registration owns a probe worker; no scanning and no work after drop.
pub struct SideProbe {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl SideProbe {
    pub fn register(address: SocketAddr) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker =
            (std::env::var("HARNESS_TEST_LOOPBACK_PROBE").as_deref() == Ok("1")).then(|| {
                // Seed the backlog before the client starts, deterministically exercising
                // attribution even when the async runtime has only one worker.
                probe(address);
                let stop = stop.clone();
                thread::spawn(move || {
                    // Bound the traffic: do not fill an undrained S02 backlog
                    // and thereby prevent the very leak we want to detect.
                    for _ in 0..3 {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        probe(address);
                        thread::sleep(Duration::from_millis(20));
                    }
                })
            });
        Self { stop, worker }
    }
}
fn probe(address: SocketAddr) {
    for bytes in [
        b"".as_slice(),
        b"GET / HTTP/1.1\r\n\r\n",
        b"unrelated probe\n",
    ] {
        if let Ok(mut socket) = TcpStream::connect_timeout(&address, Duration::from_millis(20)) {
            socket
                .set_write_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            let _ = socket.write_all(bytes);
        }
    }
}
impl Drop for SideProbe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

/// Drain pending connections, reading each for a bounded time and byte count.
/// A byte-for-byte nonce match is evidence of a leak, regardless of tool status.
pub fn assert_no_nonce(listener: &TcpListener, nonce: &str) {
    listener.set_nonblocking(true).unwrap();
    let mut ignored = 0;
    loop {
        match listener.accept() {
            Ok((mut socket, _)) => {
                socket
                    .set_read_timeout(Some(Duration::from_millis(30)))
                    .unwrap();
                let mut input = Vec::new();
                let read_deadline = Instant::now() + Duration::from_millis(100);
                while input.len() < 8192 && Instant::now() < read_deadline {
                    let mut buf = [0; 1024];
                    match socket.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => input.extend_from_slice(&buf[..n]),
                    }
                    assert!(
                        !input.windows(nonce.len()).any(|v| v == nonce.as_bytes()),
                        "sandbox nonce reached host listener"
                    );
                }
                ignored += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("loopback accept: {e}"),
        }
    }
    eprintln!(
        "loopback {}: ignored {ignored} unrelated connections",
        listener.local_addr().unwrap()
    );
}
