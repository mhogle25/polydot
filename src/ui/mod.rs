// User-facing output: println!/eprintln! for conversational UI (progress,
// prompts, summaries). Diagnostic logging goes through `tracing`, gated on
// --verbose / RUST_LOG. Don't conflate the two.
//
// Helpers here are intentionally small — we want output that's pleasant to
// scan in a terminal without dragging in a tables/colors crate. If layout
// needs grow past trivial padding, revisit then.

use std::fmt::Write;

pub mod line_editor;
pub mod menu;

pub use menu::{Menu, MenuOption};

/// Render a list of `(label, value)` rows with `label` column right-padded
/// to fit the longest label plus two spaces of breathing room. Intended for
/// the small per-repo blocks in `polydot status`.
pub fn render_kv(rows: &[(&str, String)]) -> String {
    let label_width = rows
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    let mut out = String::new();
    for (label, value) in rows {
        let _ = writeln!(out, "  {label:<label_width$}{value}");
    }
    out
}
