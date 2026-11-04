//! Test-only serialization between subprocess spawning and journal locking.
//!
//! A forked child inherits every descriptor of this process, including another
//! thread's journal, and with it that journal's `flock`. Close-on-exec closes
//! the descriptor at `exec`, but until then the child keeps the lock alive even
//! though the writer already closed its own descriptor, so a concurrent
//! release-then-reopen can report `session_locked` with no engine at fault
//! (ADR 0012, "known in-process lock/spawn gap").
//!
//! Every journal lock acquisition and every subprocess spawn in a test build
//! takes this one gate, so the two can never overlap and a lock failure inside
//! a test is a real lock defect. Hold it for the whole life of the child (spawn
//! *and* wait) and for nothing else: never across another acquisition.
use std::sync::{Mutex, MutexGuard};

static GATE: Mutex<()> = Mutex::new(());

/// Hold the spawn/lock gate for the current scope.
pub(crate) fn hold() -> MutexGuard<'static, ()> {
    // A test that panics while holding it must not fail every later test.
    GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
