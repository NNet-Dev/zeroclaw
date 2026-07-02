//! Diagnostic parsing and rendering helpers for tool output.

mod ansi;
mod parser;
mod render;

pub use ansi::strip_ansi;
pub use parser::parse_diagnostics;
pub use render::render_diagnostics_block;
pub use zeroclaw_api::diagnostics::{Diagnostic, DiagnosticKey, DiagnosticSeverity};
