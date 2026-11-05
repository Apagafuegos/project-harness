//! Exact-byte journal framing and per-version bundled-schema validation.

use std::{collections::BTreeSet, fmt, ops::RangeInclusive, sync::OnceLock};

use serde::{
    Deserialize, Serialize,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Value, value::RawValue};

use super::{Digest, Record, StoreError, error::Result};

/// Maximum physical record size, including its LF terminator.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Largest counter representable in the persisted format.
pub const MAX_COUNTER: u64 = 9_007_199_254_740_991;
/// Record versions readers accept. Each record is validated against its own
/// version's schema; any other version fails closed as `unsupported_version`.
pub const SUPPORTED_VERSIONS: RangeInclusive<u32> = 1..=4;
/// The only version writers produce, including for sessions created under an
/// earlier version.
pub const CURRENT_VERSION: u32 = 4;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<'a> {
    #[serde(borrow)]
    record: &'a RawValue,
    sha256: Digest,
}

// Value's default map visitor overwrites duplicates. Walk every object first,
// including opaque argument objects and escaped spellings of the same key.
struct UniqueJson;
impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: de::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Check;
        impl<'de> Visitor<'de> for Check {
            type Value = UniqueJson;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON without duplicate keys")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<UniqueJson, A::Error> {
                let mut keys = BTreeSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !keys.insert(key) {
                        return Err(de::Error::custom("duplicate object key"));
                    }
                    map.next_value::<UniqueJson>()?;
                }
                Ok(UniqueJson)
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<UniqueJson, A::Error> {
                while seq.next_element::<UniqueJson>()?.is_some() {}
                Ok(UniqueJson)
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<UniqueJson, E> {
                Ok(UniqueJson)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<UniqueJson, E> {
                Ok(UniqueJson)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<UniqueJson, E> {
                Ok(UniqueJson)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<UniqueJson, E> {
                Ok(UniqueJson)
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<UniqueJson, E> {
                Ok(UniqueJson)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<UniqueJson, E> {
                Ok(UniqueJson)
            }
        }
        deserializer.deserialize_any(Check)
    }
}

pub(crate) fn unique_json(bytes: &[u8]) -> Result<Value> {
    serde_json::from_slice::<UniqueJson>(bytes)
        .map_err(|_| StoreError::Invalid("malformed_or_duplicate_json"))?;
    serde_json::from_slice(bytes).map_err(|_| StoreError::Invalid("malformed_json"))
}

fn compile(schema: &str) -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(schema).expect("bundled schema JSON");
    jsonschema::options()
        .should_validate_formats(true)
        .build(&schema)
        .expect("bundled local schema")
}

/// The bundled schema of a supported version. Each is compiled alone, on
/// first use; the files share no `$ref`.
fn schema(version: u32) -> &'static jsonschema::Validator {
    static V1: OnceLock<jsonschema::Validator> = OnceLock::new();
    static V2: OnceLock<jsonschema::Validator> = OnceLock::new();
    static V3: OnceLock<jsonschema::Validator> = OnceLock::new();
    static V4: OnceLock<jsonschema::Validator> = OnceLock::new();
    match version {
        1 => V1.get_or_init(|| {
            compile(include_str!(
                "../../../../docs/rfcs/001/schema/session-v1.schema.json"
            ))
        }),
        2 => V2.get_or_init(|| {
            compile(include_str!(
                "../../../../docs/rfcs/001/schema/session-v2.schema.json"
            ))
        }),
        3 => V3.get_or_init(|| {
            compile(include_str!(
                "../../../../docs/rfcs/001/schema/session-v3.schema.json"
            ))
        }),
        4 => V4.get_or_init(|| {
            compile(include_str!(
                "../../../../docs/rfcs/001/schema/session-v4.schema.json"
            ))
        }),
        _ => unreachable!("unsupported version {version}"),
    }
}

pub(crate) fn decode(line: &[u8]) -> Result<(Record, Digest)> {
    if line.len() + 1 > MAX_LINE_BYTES {
        return Err(StoreError::Limit);
    }
    if line.ends_with(b"\r") {
        return Err(StoreError::Invalid("crlf_framing"));
    }
    let envelope: Envelope<'_> =
        serde_json::from_slice(line).map_err(|_| StoreError::Invalid("invalid_envelope"))?;
    if Digest::of(envelope.record.get().as_bytes()) != envelope.sha256 {
        return Err(StoreError::Invalid("checksum_mismatch"));
    }
    let value = unique_json(line)?;
    // Only an integer selects a schema: `2.0` and `"2"` are not versions.
    let version = value["record"]["version"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| SUPPORTED_VERSIONS.contains(v))
        .ok_or(StoreError::Invalid("unsupported_version"))?;
    if !schema(version).is_valid(&value) {
        return Err(StoreError::Invalid("schema_violation"));
    }
    let record = serde_json::from_str(envelope.record.get())
        .map_err(|_| StoreError::Invalid("invalid_record"))?;
    Ok((record, envelope.sha256))
}

/// The exact envelope bytes of `record` without the trailing LF, bounded by
/// the physical line limit. No schema or payload check: a caller that has not
/// built the final record (the undo preflight) can still measure it.
pub(crate) fn frame(record: &Record) -> Result<(Vec<u8>, Digest)> {
    if record.version != CURRENT_VERSION {
        return Err(StoreError::Invalid("write_version"));
    }
    #[derive(Serialize)]
    struct Output<'a> {
        record: &'a RawValue,
        sha256: &'a Digest,
    }
    let raw = RawValue::from_string(String::from_utf8(bounded_json(record)?).expect("JSON UTF-8"))
        .map_err(|_| StoreError::Invalid("serialization"))?;
    let digest = Digest::of(raw.get().as_bytes());
    let bytes = bounded_json(&Output {
        record: &raw,
        sha256: &digest,
    })?;
    Ok((bytes, digest))
}

/// Frame `record`, which must be [`CURRENT_VERSION`]: no path writes an older
/// version. The bytes are decoded again, so they pass the reader's checks.
pub(crate) fn encode(record: &Record) -> Result<(Vec<u8>, Digest)> {
    let (mut bytes, digest) = frame(record)?;
    decode(&bytes)?;
    bytes.push(b'\n');
    Ok((bytes, digest))
}

// Bound serialization as well as reading; caller-owned inline data must never
// cause an unbounded second allocation before a line-limit error.
pub(crate) fn bounded_json(value: &impl Serialize) -> Result<Vec<u8>> {
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_LINE_BYTES - 1 - self.0.len() {
                return Err(std::io::Error::other("JSON size limit"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = Bounded(Vec::new());
    serde_json::to_writer(&mut out, value).map_err(|e| {
        if e.is_io() {
            StoreError::Limit
        } else {
            StoreError::Invalid("serialization")
        }
    })?;
    Ok(out.0)
}
