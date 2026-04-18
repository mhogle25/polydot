// User-facing output: println!/eprintln! for conversational UI (progress,
// prompts, summaries). Diagnostic logging goes through `tracing`, gated on
// --verbose / RUST_LOG. Don't conflate the two.
//
// Helpers here are intentionally small — we want output that's pleasant to
// scan in a terminal without dragging in a tables/colors crate. If layout
// needs grow past trivial padding, revisit then.

use std::fmt::Write;

pub mod menu;

pub use menu::{Menu, MenuOption};

/// Right-pad `s` with spaces so it occupies at least `width` display columns.
/// Treats every char as one column — fine for the ASCII labels we emit.
pub fn pad_right(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.to_string()
    } else {
        let mut out = String::with_capacity(width);
        out.push_str(s);
        for _ in 0..(width - len) {
            out.push(' ');
        }
        out
    }
}

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
        let _ = writeln!(out, "  {}{value}", pad_right(label, label_width));
    }
    out
}
