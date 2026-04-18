// Path expression evaluator.
//
// Implemented in Phase 1. Supports:
//   $VAR             — env var expansion
//   ~                — home directory expansion (always-on in path contexts)
//   ${expr | xform}  — apply transform(s) to expression
//   $$               — literal $
//
// Built-in transforms: slug, basename, dirname.
