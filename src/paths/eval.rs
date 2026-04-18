// Path expression evaluator.
//
// Walks an AST + an Env (env vars + home dir source) to produce a string.
// All semantic errors that depend on env state — missing vars, missing home —
// surface here. Syntactic errors and unknown transforms surface in the parser.

use std::path::{Path, PathBuf};

use super::ast::{Expression, Node, TransformKind};
use crate::error::{Error, Result};

pub trait Env {
    fn var(&self, name: &str) -> Option<String>;
    fn home(&self) -> Option<PathBuf>;
}

pub struct SystemEnv;

impl Env for SystemEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn home(&self) -> Option<PathBuf> {
        dirs::home_dir()
    }
}

pub fn evaluate(expr: &Expression, env: &impl Env) -> Result<String> {
    let mut out = String::new();
    write_expression(expr, env, &mut out)?;
    Ok(out)
}

fn write_expression(expr: &Expression, env: &impl Env, out: &mut String) -> Result<()> {
    for node in &expr.nodes {
        write_node(node, env, out)?;
    }
    Ok(())
}

fn write_node(node: &Node, env: &impl Env, out: &mut String) -> Result<()> {
    match node {
        Node::Literal { text, .. } => out.push_str(text),
        Node::Home { .. } => {
            let home = env
                .home()
                .ok_or_else(|| Error::Path("home directory not available".into()))?;
            out.push_str(&home.to_string_lossy());
        }
        Node::EscapedDollar { .. } => out.push('$'),
        Node::Var { name, .. } => {
            let value = env
                .var(name)
                .ok_or_else(|| Error::Path(format!("env var ${name} not set")))?;
            out.push_str(&value);
        }
        Node::Substitution {
            expr, transforms, ..
        } => {
            let mut value = String::new();
            write_expression(expr, env, &mut value)?;
            for t in transforms {
                value = apply_transform(&value, t.kind);
            }
            out.push_str(&value);
        }
    }
    Ok(())
}

fn apply_transform(value: &str, kind: TransformKind) -> String {
    match kind {
        TransformKind::Slug => value.replace('/', "-"),
        TransformKind::Basename => Path::new(value)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        TransformKind::Dirname => Path::new(value)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::parse;
    use std::collections::HashMap;

    struct MockEnv {
        home: Option<PathBuf>,
        vars: HashMap<String, String>,
    }

    impl Env for MockEnv {
        fn var(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }
        fn home(&self) -> Option<PathBuf> {
            self.home.clone()
        }
    }

    fn env() -> MockEnv {
        MockEnv {
            home: Some(PathBuf::from("/home/test")),
            vars: [("FOO".to_string(), "bar".to_string())]
                .into_iter()
                .collect(),
        }
    }

    fn eval(input: &str) -> String {
        let expr = parse(input).unwrap();
        evaluate(&expr, &env()).unwrap()
    }

    #[test]
    fn literal_passes_through() {
        assert_eq!(eval("/foo/bar"), "/foo/bar");
        assert_eq!(eval(""), "");
    }

    #[test]
    fn home_expands() {
        assert_eq!(eval("~"), "/home/test");
        assert_eq!(eval("~/foo"), "/home/test/foo");
    }

    #[test]
    fn env_var_expands() {
        assert_eq!(eval("$FOO"), "bar");
        assert_eq!(eval("/$FOO/baz"), "/bar/baz");
    }

    #[test]
    fn dollar_dollar_renders_dollar() {
        assert_eq!(eval("$$5"), "$5");
    }

    #[test]
    fn slug_transform() {
        assert_eq!(eval("${~/dev/lish | slug}"), "-home-test-dev-lish");
        assert_eq!(eval("${~ | slug}"), "-home-test");
    }

    #[test]
    fn basename_transform() {
        assert_eq!(eval("${/foo/bar/baz | basename}"), "baz");
    }

    #[test]
    fn dirname_transform() {
        assert_eq!(eval("${/foo/bar/baz | dirname}"), "/foo/bar");
    }

    #[test]
    fn chained_transforms_left_to_right() {
        assert_eq!(eval("${/foo/bar/baz | dirname | basename}"), "bar");
    }

    #[test]
    fn missing_env_var_errors_at_eval() {
        let expr = parse("$NOPE").unwrap();
        let err = evaluate(&expr, &env()).unwrap_err();
        assert!(matches!(err, Error::Path(_)));
    }

    #[test]
    fn missing_home_errors_at_eval() {
        let expr = parse("~/foo").unwrap();
        let no_home = MockEnv {
            home: None,
            vars: HashMap::new(),
        };
        let err = evaluate(&expr, &no_home).unwrap_err();
        assert!(matches!(err, Error::Path(_)));
    }

    #[test]
    fn realistic_dogfood_evaluation() {
        assert_eq!(
            eval("~/.claude/projects/${~/dev/projects/lish-zig | slug}/memory"),
            "/home/test/.claude/projects/-home-test-dev-projects-lish-zig/memory",
        );
    }
}
