//! Preserved-thinking policy for replayed thinking blocks.
//!
//! The Messages API binds a `thinking` or `redacted_thinking` block to the exact
//! prefix that produced it: the top-level `system` prompt, the set of `tools`,
//! and every `message` before the block. A replay whose prefix changed fails the
//! check. Client-side compaction is exactly such an edit: our summary replaces
//! the turns that came before the retained turns, whose thinking blocks were
//! produced while the original history was still in place.
//!
//! On accounts the API enforces the check for, an unset
//! `thinking.block_binding.prefix_mismatch_behavior` rejects that request with a
//! `400 invalid_request_error`. The harness's history is append-only, so the
//! remaining edit is compaction; the adapter therefore requests the documented
//! `drop_block` behavior on every request that replays thinking. Source:
//! `platform.claude.com/docs/en/build-with-claude/preserved-thinking`, read
//! 2026-10-30 (sections "Keeping the prefix unchanged", "What the API does with
//! an invalid block", "Compact on the client → Keep-tail compaction"). See the
//! "Amendment (m4-07)" in ADR 0018.
//!
//! `drop_block` costs output tokens when it actually drops something, because
//! the model re-creates the reasoning. It only drops when a prefix changed, and
//! the harness changes a prefix only by compacting, so the cost is one boundary
//! per compaction — the trade the reference documents for a self-written
//! keep-tail summary. The alternative it mentions (strip the kept turns'
//! thinking ourselves) would lose that reasoning permanently, including if the
//! same history is later sent to a model that could have read it, so the
//! adapter keeps the history intact and lets the API report what it dropped.
//!
//! Requests that replay nothing send neither the control nor the beta header:
//! they behave exactly as before this policy existed.

use serde_json::{Value, json};

/// Beta header that makes `thinking.block_binding` legal on a request.
pub const BETA: &str = "thinking-binding-controls-2026-08-01";

/// The `thinking.block_binding` object this adapter sends.
///
/// `drop_block` is the documented stopgap: a failing block and every thinking
/// block after it are dropped and reported in `input_transformations`, and the
/// request succeeds instead of failing with a 400. A block that passes the check
/// is unaffected, so the control is safe to send on an untouched history too.
pub fn drop_block() -> Value {
    json!({"prefix_mismatch_behavior": "drop_block"})
}
