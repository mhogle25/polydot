mod ast;
mod eval;
mod parser;

pub use ast::{Expression, Node, Span, Transform, TransformKind};
pub use eval::{Env, SystemEnv, evaluate};
pub use parser::parse;

use crate::error::Result;

/// Convenience: parse and evaluate in one shot. For configs, prefer parsing
/// once at load time (so syntax errors surface early) and evaluating later.
pub fn evaluate_str(input: &str, env: &impl Env) -> Result<String> {
    let expr = parse(input)?;
    evaluate(&expr, env)
}
