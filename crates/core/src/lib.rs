//! Headless engine for project-harness.
//!
//! Session storage, the pure runtime core, its Tokio driver and the provider
//! seam with a scripted mock are implemented. Native providers, tools and
//! context projection remain future work. This crate is independent of
//! terminal clients.

pub mod model;
pub mod runtime;
pub mod session;
