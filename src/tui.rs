//! Interactive shared-file detail pane.
//!
//! [`state`] is the pure transition layer, [`view`] owns ratatui presentation,
//! and [`run`] owns gathering and terminal lifecycle.

mod run;
mod state;
pub mod view;

pub use run::{map_key_event, run_watch};
pub use state::{adopt, apply, display_order, show_hunks, Detail, Key, Mode, RowId};
