//! Shared, offline-tested HTTP/SSE and credential boundary for native adapters.
//! No provider wire format or live credential discovery runs automatically.

pub mod credentials;
pub mod error;
pub mod sse;
pub mod transport;

#[cfg(test)]
mod tests;
