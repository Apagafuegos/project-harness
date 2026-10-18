//! Headless engine for project-harness.
//!
//! Session storage and the pure runtime core are implemented. The Tokio
//! driver, providers, tools and context projection remain future work. This
//! crate is independent of terminal clients.

pub mod runtime;
pub mod session;
