//! Shared, offline-tested HTTP/SSE and credential boundary for native adapters,
//! plus the OpenAI Responses and Anthropic Messages adapters and the router
//! that dispatches each request to the instance its `ModelRef` names. No
//! provider wire format or live credential discovery runs automatically; only
//! an explicitly invoked live check does.

pub mod anthropic;
pub mod credentials;
pub mod error;
mod native_id;
pub mod openai;
pub mod router;
pub mod sse;
pub mod transport;

#[cfg(test)]
mod tests;
