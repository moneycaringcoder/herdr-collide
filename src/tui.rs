//! Interactive shared-file detail pane.
//!
//! [`state`] is the pure transition layer, [`view`] owns ratatui presentation,
//! and [`run`] owns gathering and terminal lifecycle.

mod run;
mod state;
mod view;

pub use run::run_watch;
