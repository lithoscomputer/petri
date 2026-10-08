//! Machinery every frontend shares.
//!
//! A frontend is pure: file text in, a `Graph` and diagnostics out. This crate
//! holds what that takes — [`diag`] for saying what went wrong and where,
//! [`yaml`] for reading files with positions, [`expr`] for the `${{ }}` grammar
//! and the strict lowering, [`files`] for the local includes some formats have,
//! [`print`] for a stable text form of a lowered graph — and [`Frontend`], the
//! trait a format implements so the CLI can hold a list of them.

pub mod diag;
mod digest;
pub mod expr;
mod files;
mod format;
pub mod print;
pub mod yaml;

pub use diag::{Diagnostic, Diagnostics, Lowered, Severity, Span};
pub use digest::graph_digest;
pub use files::{DirFiles, FileSource, MapFiles, NoFiles};
pub use format::{
    CompileInputs, DEFAULT_MODEL_VAR, DEFAULT_PROVIDER_VAR, Frontend, LAUNCH_ENVIRONMENT_VAR,
    LAUNCH_GOAL_VAR, LAUNCH_MODEL_VAR, LAUNCH_PROVIDER_VAR, LaunchSettings, REPOSITORY_VAR,
    WorkspaceRetention, by_name, detect,
};
pub use print::print_graph;
