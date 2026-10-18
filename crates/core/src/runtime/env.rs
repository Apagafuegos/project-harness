//! The only source of time and new identities for the pure core.

use crate::session::{CallId, EntryId, ResponseId, Timestamp, TurnId};

/// Time and identity generation. A step reads nothing else from the outside.
///
/// Every identity must be unique within the journal, across all identity
/// types and across reopening, because storage keeps them in one set.
pub trait Env {
    /// Timestamp for the next record.
    fn now(&mut self) -> Timestamp;
    /// New tree entry identity.
    fn entry_id(&mut self) -> EntryId;
    /// New turn identity.
    fn turn_id(&mut self) -> TurnId;
    /// New assistant response identity.
    fn response_id(&mut self) -> ResponseId;
    /// New tool call identity.
    fn call_id(&mut self) -> CallId;
}

/// Wall clock and random UUIDv7 identities, for production drivers.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemEnv;

impl Env for SystemEnv {
    fn now(&mut self) -> Timestamp {
        Timestamp::now()
    }
    fn entry_id(&mut self) -> EntryId {
        EntryId::new()
    }
    fn turn_id(&mut self) -> TurnId {
        TurnId::new()
    }
    fn response_id(&mut self) -> ResponseId {
        ResponseId::new()
    }
    fn call_id(&mut self) -> CallId {
        CallId::new()
    }
}

/// A fixed clock and counter-derived identities that are still valid UUIDv7.
///
/// Each identity holds the fixed base time, `stream` in the 12 `rand_a` bits and
/// a counter shared by all identity types in the 62 `rand_b` bits. Give every
/// runtime that writes the same journal its own `stream`, for example a new one
/// after each reopen, and identities never repeat.
#[derive(Clone, Debug)]
pub struct DeterministicEnv {
    at: Timestamp,
    millis: u64,
    stream: u16,
    counter: u64,
}

impl DeterministicEnv {
    /// Fixed base time of every deterministic environment.
    pub const BASE: &'static str = "2026-10-18T00:00:00Z";

    /// Create a generator for one of 4,096 streams (`stream` is masked to 12 bits).
    pub fn new(stream: u16) -> Self {
        let at: Timestamp = Self::BASE.parse().expect("valid base time");
        let millis = (at.instant().unix_timestamp_nanos() / 1_000_000) as u64;
        Self {
            at,
            millis,
            stream: stream & 0x0fff,
            counter: 0,
        }
    }

    fn next<T: TryFrom<String, Error = String>>(&mut self) -> T {
        self.counter += 1;
        let value = (u128::from(self.millis) << 80)
            | (0x7 << 76)
            | (u128::from(self.stream) << 64)
            | (0b10 << 62)
            | u128::from(self.counter & ((1 << 62) - 1));
        T::try_from(uuid::Uuid::from_u128(value).to_string())
            .expect("counter-derived UUIDv7 is valid")
    }
}

impl Env for DeterministicEnv {
    fn now(&mut self) -> Timestamp {
        self.at.clone()
    }
    fn entry_id(&mut self) -> EntryId {
        self.next()
    }
    fn turn_id(&mut self) -> TurnId {
        self.next()
    }
    fn response_id(&mut self) -> ResponseId {
        self.next()
    }
    fn call_id(&mut self) -> CallId {
        self.next()
    }
}
