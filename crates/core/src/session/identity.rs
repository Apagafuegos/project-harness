//! Validated identities, timestamps, and content digests.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::{Uuid, Variant};

macro_rules! identity {
    ($($name:ident),+ $(,)?) => {$ (
        #[doc = concat!("Engine-issued UUIDv7 ", stringify!($name), ".")]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Generate a new engine identity.
            pub fn new() -> Self { Self(Uuid::now_v7().to_string()) }
            /// Return its canonical lowercase representation.
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl TryFrom<String> for $name {
            type Error = String;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                let id = Uuid::parse_str(&value).map_err(|_| "invalid UUIDv7")?;
                if id.get_version_num() != 7 || id.get_variant() != Variant::RFC4122 || id.to_string() != value {
                    return Err("expected canonical lowercase UUIDv7".into());
                }
                Ok(Self(value))
            }
        }
        impl FromStr for $name {
            type Err = String;
            fn from_str(value: &str) -> Result<Self, Self::Err> { value.to_owned().try_into() }
        }
        impl From<$name> for String { fn from(value: $name) -> Self { value.0 } }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
        }
    )+};
}

identity!(
    SessionId,
    RecordId,
    EntryId,
    TurnId,
    ResponseId,
    CallId,
    InvocationId,
    ApprovalId,
    DecisionId,
    CommandId,
    UndoId
);

/// A validated lowercase SHA-256 digest, also safe as an artifact filename.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);

impl Digest {
    /// Hash the exact supplied bytes.
    pub fn of(bytes: &[u8]) -> Self {
        Self(format!("{:x}", Sha256::digest(bytes)))
    }
    /// Return the lowercase hexadecimal representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for Digest {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("expected lowercase SHA-256 digest".into());
        }
        Ok(Self(value))
    }
}
impl FromStr for Digest {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.to_owned().try_into()
    }
}
impl From<Digest> for String {
    fn from(value: Digest) -> Self {
        value.0
    }
}
impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A UTC RFC 3339 timestamp ending in `Z`; never used for journal ordering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp(String);
impl Timestamp {
    /// Capture the current UTC wall clock.
    pub fn now() -> Self {
        Self(
            OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .expect("UTC date fits RFC 3339"),
        )
    }
    /// Return its canonical RFC 3339 text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub(crate) fn instant(&self) -> OffsetDateTime {
        OffsetDateTime::parse(&self.0, &Rfc3339).expect("validated timestamp")
    }
}
impl TryFrom<String> for Timestamp {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if !value.ends_with('Z') || OffsetDateTime::parse(&value, &Rfc3339).is_err() {
            return Err("expected UTC RFC 3339 timestamp ending in Z".into());
        }
        Ok(Self(value))
    }
}
impl FromStr for Timestamp {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.to_owned().try_into()
    }
}
impl From<Timestamp> for String {
    fn from(value: Timestamp) -> Self {
        value.0
    }
}
