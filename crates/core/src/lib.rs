//! Headless engine for project-harness.
//!
//! Session storage, the pure runtime core, its Tokio driver, the provider
//! seam with a scripted mock, and the pure context builder are implemented.
//! Native providers, tools and summary generation remain future work. This
//! crate is independent of terminal clients.

pub mod context;
pub mod model;
pub mod runtime;
pub mod session;
pub mod tools;

#[cfg(test)]
mod spawn_gate;
