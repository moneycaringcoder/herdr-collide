//! collide — cross-worktree collision warnings for herdr.
//!
//! The crate is split into a library plus a thin binary so that the integration
//! tests in `tests/` can reach the real modules. A binary-only crate would hide
//! all of this behind `#[path]` includes, which break as soon as a module
//! refers to `crate::`.

pub mod collide;
pub mod config;
pub mod daemon;
pub mod git;
pub mod herdr;
pub mod ignore;
pub mod model;
pub mod render;
pub mod setup;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
