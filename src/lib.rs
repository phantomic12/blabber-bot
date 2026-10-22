//! blabber-bot: a Discord music bot with age-restricted video support.
//!
//! The binary in `main.rs` wires these modules together; the library target
//! exists so integration tests can exercise the extractor and persistence
//! layers directly.

pub mod commands;
pub mod config;
pub mod extractor;
pub mod format;
pub mod persistence;
pub mod player;
