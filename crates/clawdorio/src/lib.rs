//! Umbrella crate for Clawdorio.
//!
//! This crate is intentionally small: it re-exports the engine and protocol crates
//! so downstream code can depend on a single crate name (`clawdorio`).

pub use clawdorio_engine as engine;
pub use clawdorio_protocol as protocol;
