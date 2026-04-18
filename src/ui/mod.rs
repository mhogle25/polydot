// User-facing output channel: println!/eprintln! for conversational UI
// (progress, prompts, summaries). Diagnostic logging goes through `tracing`,
// gated on --verbose / RUST_LOG. Don't conflate the two.
