//! Shared, offline-tested HTTP/SSE and credential boundary for native adapters,
//! plus the OpenAI Responses adapter. No provider wire format or live credential
//! discovery runs automatically; only an explicitly invoked live check does.

pub mod credentials;
pub mod error;
pub mod openai;
pub mod sse;
pub mod transport;

#[cfg(test)]
mod tests;
