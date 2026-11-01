//! Metrics types and optional Rust GTK3 panel implementation.
pub use multiload_qubes_snapshot::Snapshot;
pub mod metrics;

#[cfg(feature = "panel")]
mod panel;

#[cfg(feature = "panel")]
pub use panel::preview::run as run_preview;
