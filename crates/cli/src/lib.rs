//! Client logic for the headless and terminal interfaces.
//!
//! `ask` is the scriptable headless client, `wiring` is the shared session
//! assembly, and `tui` is the pure terminal model. Terminal lifecycle and the
//! real event loop belong to the binary (m5-03).
pub mod ask;

pub mod tui;

pub mod wiring;

mod text;
