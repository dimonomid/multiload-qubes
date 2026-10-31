//! Metrics types and optional Rust GTK3 panel implementation.
pub use multiload_qubes_snapshot::Snapshot;
pub mod metrics;

#[cfg(feature = "panel")]
mod graphs;
#[cfg(feature = "panel")]
mod palette;
#[cfg(feature = "panel")]
mod panel;

#[cfg(feature = "panel")]
pub use graphs::Graphs;

#[cfg(feature = "panel")]
pub use panel::preview::run as run_preview;
