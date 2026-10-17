//! Headless engine for project-harness.
//!
//! This crate is the future home of orchestration, provider adapters, tool
//! dispatch, session history, and context construction. It must remain independent
//! of terminal rendering and input handling.
//!
//! The foundation deliberately contains no runtime implementation or public
//! contract types yet. See `docs/contracts/` in the repository for the draft
//! contracts and their acceptance criteria.
