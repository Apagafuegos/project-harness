//! Deterministic native call identities for replayed tool calls.
//!
//! A tool call stores the id its provider issued (`provider_call_id`) and is
//! paired with its result by the harness's own `CallId`. Both native adapters
//! send that native id back: OpenAI as `function_call.call_id` /
//! `function_call_output.call_id`, Anthropic as `tool_use.id` /
//! `tool_result.tool_use_id`.
//!
//! After a provider switch, the id in the projection was issued by the *other*
//! provider. Passing it through is the honest thing to do — it is the identity
//! the stored turn actually carries, and it must stay byte-identical on every
//! later request, because a request's `messages` are part of the
//! preserved-thinking prefix. Both providers issue plain ASCII identifiers, so
//! a foreign id is normally sent verbatim.
//!
//! What must never happen is sending an id the target does not accept: an id
//! with punctuation, whitespace, control bytes or non-ASCII text, an empty one,
//! or one longer than any provider's documented identifier length. Neither
//! API's reference states a pattern this crate could quote, so instead of
//! guessing at one the adapters keep to the alphabet and length every issued id
//! already satisfies: ASCII letters, digits, `_` and `-`, at most 64 bytes.
//!
//! When the native id does not fit, the substitute is derived from the
//! harness's own `CallId`, which is unique per call and stable across
//! requests — never from a counter, a timestamp or a hash of request contents.
//! The same call therefore gets the same id in the `tool_use` block and in its
//! `tool_result`, on this request and on every later one, so a switch can
//! neither unpair a call from its result nor edit the prefix it already sent.

/// Longest native call id this crate sends, in bytes.
///
/// A conservative adapter rule, not a quoted API limit: every id either provider
/// has issued here (`call_…`, `toolu_…`, `fc_…`) is far shorter, and a UUID is
/// 36 bytes.
const MAX_BYTES: usize = 64;

/// The id to send for one tool call: the provider's own when it is usable,
/// otherwise a deterministic substitute derived from `harness`.
pub(crate) fn native_call_id(native: Option<&str>, harness: &str) -> String {
    if let Some(native) = native.filter(|id| usable(id)) {
        return native.to_owned();
    }
    let mut substitute: String = harness
        .chars()
        .map(|character| {
            if usable_char(character) {
                character
            } else {
                '_'
            }
        })
        .collect();
    substitute.truncate(MAX_BYTES);
    if substitute.is_empty() {
        substitute.push_str("call");
    }
    substitute
}

fn usable(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_BYTES && id.chars().all(usable_char)
}

fn usable_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_' || character == '-'
}
