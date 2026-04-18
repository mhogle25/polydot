// Path expression AST.
//
// Every node carries a Span (byte range into the source string) so future
// editor tooling — syntax highlighting, error squiggles, hover info — can
// map AST nodes back to source positions without a re-parse.

use std::fmt;
use std::ops::Range;

use serde::{Deserialize, Serialize};

pub type Span = Range<usize>;

#[derive(Debug, Clone)]
pub struct Expression {
    pub nodes: Vec<Node>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Node {
    /// Verbatim text — anything not interpreted.
    Literal { text: String, span: Span },
    /// `~` at the start of an expression.
    Home { span: Span },
    /// `$$` — renders as a literal `$`.
    EscapedDollar { span: Span },
    /// `$NAME` — env var lookup. Span covers the whole `$NAME`.
    Var { name: String, span: Span },
    /// `${ expr | t1 | t2 | ... }`. Span covers `${...}` inclusive.
    Substitution {
        expr: Box<Expression>,
        transforms: Vec<Transform>,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct Transform {
    pub kind: TransformKind,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransformKind {
    Slug,
    Basename,
    Dirname,
}

impl TransformKind {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "slug" => Some(Self::Slug),
            "basename" => Some(Self::Basename),
            "dirname" => Some(Self::Dirname),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Slug => "slug",
            Self::Basename => "basename",
            Self::Dirname => "dirname",
        }
    }
}

// --- equality ignores spans (round-trip preserves structure, not byte offsets) ---

impl PartialEq for Expression {
    fn eq(&self, other: &Self) -> bool {
        self.nodes == other.nodes
    }
}
impl Eq for Expression {}

impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Literal { text: a, .. }, Self::Literal { text: b, .. }) => a == b,
            (Self::Home { .. }, Self::Home { .. }) => true,
            (Self::EscapedDollar { .. }, Self::EscapedDollar { .. }) => true,
            (Self::Var { name: a, .. }, Self::Var { name: b, .. }) => a == b,
            (
                Self::Substitution {
                    expr: a,
                    transforms: at,
                    ..
                },
                Self::Substitution {
                    expr: b,
                    transforms: bt,
                    ..
                },
            ) => a == b && at == bt,
            _ => false,
        }
    }
}
impl Eq for Node {}

impl PartialEq for Transform {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}
impl Eq for Transform {}

// --- surface form: Display is the inverse of `parser::parse` (modulo whitespace) ---

impl fmt::Display for Expression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for node in &self.nodes {
            write!(f, "{node}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal { text, .. } => f.write_str(text),
            Self::Home { .. } => f.write_str("~"),
            Self::EscapedDollar { .. } => f.write_str("$$"),
            Self::Var { name, .. } => write!(f, "${name}"),
            Self::Substitution {
                expr, transforms, ..
            } => {
                write!(f, "${{{expr}")?;
                for t in transforms {
                    write!(f, " | {}", t.kind.name())?;
                }
                f.write_str("}")
            }
        }
    }
}

// --- serde: deserialize from string via parser, serialize from Display ---

impl Serialize for Expression {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Expression {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        super::parse(&s).map_err(serde::de::Error::custom)
    }
}
