// Recursive-descent parser for path expressions.
//
// Single source pass; emits a Vec<Node> with byte-offset spans into the
// original input. Transform names are validated here so unknown transforms
// surface at config load, not at evaluation.

use super::ast::{Expression, Node, Transform, TransformKind};
use crate::error::{Error, Result};

pub fn parse(input: &str) -> Result<Expression> {
    let mut parser = Parser::new(input);
    let expr = parser.parse_expression(StopAt::Eof)?;
    if !parser.is_eof() {
        return Err(Error::Path(format!(
            "trailing input at offset {}",
            parser.pos
        )));
    }
    Ok(expr)
}

#[derive(Copy, Clone)]
enum StopAt {
    Eof,
    PipeOrBrace,
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn peek_at(&self, offset_chars: usize) -> Option<char> {
        self.input[self.pos..].chars().nth(offset_chars)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.bump();
        }
    }

    /// First non-whitespace char from current position, without advancing.
    fn next_non_whitespace(&self) -> Option<char> {
        self.input[self.pos..].chars().find(|c| !c.is_whitespace())
    }

    fn flush_literal(&self, nodes: &mut Vec<Node>, start: &mut Option<usize>) {
        if let Some(s) = start.take()
            && s < self.pos
        {
            nodes.push(Node::Literal {
                text: self.input[s..self.pos].to_string(),
                span: s..self.pos,
            });
        }
    }

    fn parse_expression(&mut self, stop: StopAt) -> Result<Expression> {
        let start = self.pos;
        let mut nodes = Vec::new();
        let mut lit_start: Option<usize> = None;
        let mut at_start = true;

        loop {
            let done = match (stop, self.peek()) {
                (StopAt::Eof, None) => true,
                (StopAt::PipeOrBrace, None) => {
                    return Err(Error::Path(format!(
                        "unterminated `${{ ... }}` starting at offset {start}"
                    )));
                }
                (StopAt::PipeOrBrace, Some('|' | '}')) => true,
                // Whitespace immediately preceding a `|` or `}` is structural
                // (separates expression body from transforms / closer), not literal.
                (StopAt::PipeOrBrace, Some(c))
                    if c.is_whitespace()
                        && matches!(self.next_non_whitespace(), Some('|' | '}')) =>
                {
                    true
                }
                _ => false,
            };
            if done {
                break;
            }

            // ~ at the start of the expression context.
            if at_start && self.peek() == Some('~') && is_home_terminator(self.peek_at(1), stop) {
                self.flush_literal(&mut nodes, &mut lit_start);
                let s = self.pos;
                self.bump();
                nodes.push(Node::Home { span: s..self.pos });
                at_start = false;
                continue;
            }

            // $ constructs.
            if self.peek() == Some('$') {
                match self.peek_at(1) {
                    Some('$') => {
                        self.flush_literal(&mut nodes, &mut lit_start);
                        let s = self.pos;
                        self.bump();
                        self.bump();
                        nodes.push(Node::EscapedDollar { span: s..self.pos });
                        at_start = false;
                        continue;
                    }
                    Some('{') => {
                        self.flush_literal(&mut nodes, &mut lit_start);
                        nodes.push(self.parse_substitution()?);
                        at_start = false;
                        continue;
                    }
                    Some(c) if is_ident_start(c) => {
                        self.flush_literal(&mut nodes, &mut lit_start);
                        nodes.push(self.parse_var()?);
                        at_start = false;
                        continue;
                    }
                    _ => {
                        // lone `$` is literal — fall through.
                    }
                }
            }

            // Any other character is literal.
            if lit_start.is_none() {
                lit_start = Some(self.pos);
            }
            self.bump();
            at_start = false;
        }

        self.flush_literal(&mut nodes, &mut lit_start);
        Ok(Expression {
            nodes,
            span: start..self.pos,
        })
    }

    fn parse_substitution(&mut self) -> Result<Node> {
        let start = self.pos;
        self.bump(); // $
        self.bump(); // {

        self.skip_whitespace();
        let expr = self.parse_expression(StopAt::PipeOrBrace)?;
        if expr.nodes.is_empty() {
            return Err(Error::Path(format!(
                "empty expression in `${{ ... }}` at offset {start}"
            )));
        }

        self.skip_whitespace();
        let mut transforms = Vec::new();
        while self.peek() == Some('|') {
            self.bump();
            self.skip_whitespace();
            transforms.push(self.parse_transform()?);
            self.skip_whitespace();
        }

        match self.peek() {
            Some('}') => {
                self.bump();
            }
            None => {
                return Err(Error::Path(format!(
                    "unterminated `${{ ... }}` starting at offset {start}"
                )));
            }
            Some(_) => {
                return Err(Error::Path(format!(
                    "expected `}}` to close substitution at offset {}",
                    self.pos
                )));
            }
        }

        Ok(Node::Substitution {
            expr: Box::new(expr),
            transforms,
            span: start..self.pos,
        })
    }

    fn parse_var(&mut self) -> Result<Node> {
        let dollar_start = self.pos;
        self.bump(); // $
        let name_start = self.pos;
        while let Some(c) = self.peek() {
            if is_ident_continue(c) {
                self.bump();
            } else {
                break;
            }
        }
        let name = self.input[name_start..self.pos].to_string();
        Ok(Node::Var {
            name,
            span: dollar_start..self.pos,
        })
    }

    fn parse_transform(&mut self) -> Result<Transform> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                self.bump();
            } else {
                break;
            }
        }
        let name = &self.input[start..self.pos];
        if name.is_empty() {
            return Err(Error::Path(format!(
                "expected transform name at offset {start}"
            )));
        }
        let kind = TransformKind::from_name(name)
            .ok_or_else(|| Error::Path(format!("unknown transform `{name}` at offset {start}")))?;
        Ok(Transform {
            kind,
            span: start..self.pos,
        })
    }
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

fn is_home_terminator(next: Option<char>, stop: StopAt) -> bool {
    match next {
        None | Some('/') => true,
        Some(c) if c.is_whitespace() => true,
        Some('|' | '}') => matches!(stop, StopAt::PipeOrBrace),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn must_parse(s: &str) -> Expression {
        parse(s).unwrap_or_else(|e| panic!("parse failed: {e}"))
    }

    #[test]
    fn empty_input() {
        let expr = must_parse("");
        assert!(expr.nodes.is_empty());
        assert_eq!(expr.span, 0..0);
    }

    #[test]
    fn pure_literal() {
        let expr = must_parse("/foo/bar");
        assert_eq!(expr.nodes.len(), 1);
        assert!(matches!(&expr.nodes[0], Node::Literal { text, .. } if text == "/foo/bar"));
    }

    #[test]
    fn home_at_start() {
        let expr = must_parse("~/foo");
        assert!(matches!(expr.nodes[0], Node::Home { .. }));
        assert!(matches!(&expr.nodes[1], Node::Literal { text, .. } if text == "/foo"));
    }

    #[test]
    fn home_alone() {
        let expr = must_parse("~");
        assert_eq!(expr.nodes.len(), 1);
        assert!(matches!(expr.nodes[0], Node::Home { .. }));
    }

    #[test]
    fn home_only_at_start() {
        // Mid-string ~ is literal.
        let expr = must_parse("/foo/~/bar");
        assert_eq!(expr.nodes.len(), 1);
        assert!(matches!(&expr.nodes[0], Node::Literal { text, .. } if text == "/foo/~/bar"));
    }

    #[test]
    fn env_var_basic() {
        let expr = must_parse("$FOO/bar");
        assert!(matches!(&expr.nodes[0], Node::Var { name, .. } if name == "FOO"));
        assert!(matches!(&expr.nodes[1], Node::Literal { text, .. } if text == "/bar"));
    }

    #[test]
    fn env_var_terminates_on_non_ident() {
        let expr = must_parse("$FOO-suffix");
        assert!(matches!(&expr.nodes[0], Node::Var { name, .. } if name == "FOO"));
        assert!(matches!(&expr.nodes[1], Node::Literal { text, .. } if text == "-suffix"));
    }

    #[test]
    fn dollar_dollar_is_escape() {
        let expr = must_parse("$$5");
        assert!(matches!(expr.nodes[0], Node::EscapedDollar { .. }));
        assert!(matches!(&expr.nodes[1], Node::Literal { text, .. } if text == "5"));
    }

    #[test]
    fn lone_dollar_is_literal() {
        let expr = must_parse("end$");
        assert_eq!(expr.nodes.len(), 1);
        assert!(matches!(&expr.nodes[0], Node::Literal { text, .. } if text == "end$"));
    }

    #[test]
    fn substitution_single_transform() {
        let expr = must_parse("${~ | slug}");
        let Node::Substitution {
            expr: inner,
            transforms,
            ..
        } = &expr.nodes[0]
        else {
            panic!("expected substitution");
        };
        assert!(matches!(inner.nodes[0], Node::Home { .. }));
        assert_eq!(transforms.len(), 1);
        assert_eq!(transforms[0].kind, TransformKind::Slug);
    }

    #[test]
    fn substitution_chained() {
        let expr = must_parse("${~/foo | dirname | basename}");
        let Node::Substitution { transforms, .. } = &expr.nodes[0] else {
            panic!("expected substitution");
        };
        assert_eq!(transforms.len(), 2);
        assert_eq!(transforms[0].kind, TransformKind::Dirname);
        assert_eq!(transforms[1].kind, TransformKind::Basename);
    }

    #[test]
    fn substitution_no_transforms() {
        let expr = must_parse("${~/foo}");
        let Node::Substitution { transforms, .. } = &expr.nodes[0] else {
            panic!("expected substitution");
        };
        assert!(transforms.is_empty());
    }

    #[test]
    fn substitution_with_env_var() {
        let expr = must_parse("${$HOME | slug}");
        let Node::Substitution { expr: inner, .. } = &expr.nodes[0] else {
            panic!("expected substitution");
        };
        assert!(matches!(&inner.nodes[0], Node::Var { name, .. } if name == "HOME"));
    }

    #[test]
    fn whitespace_around_pipes_is_tolerated() {
        let a = must_parse("${~|slug}");
        let b = must_parse("${~ | slug}");
        let c = must_parse("${  ~  |  slug  }");
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn unknown_transform_errors_at_parse() {
        let err = parse("${~ | nope}").unwrap_err();
        assert!(matches!(err, Error::Path(msg) if msg.contains("unknown transform")));
    }

    #[test]
    fn unterminated_substitution_errors() {
        let err = parse("${~ | slug").unwrap_err();
        assert!(matches!(err, Error::Path(msg) if msg.contains("unterminated")));
    }

    #[test]
    fn empty_substitution_errors() {
        let err = parse("${ | slug}").unwrap_err();
        assert!(matches!(err, Error::Path(msg) if msg.contains("empty expression")));
    }

    #[test]
    fn missing_transform_after_pipe_errors() {
        let err = parse("${~ |}").unwrap_err();
        assert!(matches!(err, Error::Path(msg) if msg.contains("expected transform name")));
    }

    #[test]
    fn realistic_compound_expression() {
        let expr = must_parse("~/.notes/${~/dev/projects/example-app | slug}/index");
        assert_eq!(expr.nodes.len(), 4);
        assert!(matches!(expr.nodes[0], Node::Home { .. }));
        assert!(matches!(&expr.nodes[1], Node::Literal { text, .. } if text == "/.notes/"));
        assert!(matches!(expr.nodes[2], Node::Substitution { .. }));
        assert!(matches!(&expr.nodes[3], Node::Literal { text, .. } if text == "/index"));
    }

    #[test]
    fn span_covers_full_input() {
        let input = "~/foo/${~ | slug}/bar";
        let expr = must_parse(input);
        assert_eq!(expr.span, 0..input.len());
    }

    #[test]
    fn display_round_trips_canonical_form() {
        // Canonical surface form (single space around `|`, no padding inside `${}`).
        let canonical = "~/.notes/${~/dev/projects/example-app | slug}/index";
        let expr = must_parse(canonical);
        assert_eq!(expr.to_string(), canonical);
    }

    #[test]
    fn display_normalizes_whitespace() {
        // Non-canonical input parses fine; Display emits canonical form.
        let input = "${  ~/foo   |   slug  }";
        let expr = must_parse(input);
        assert_eq!(expr.to_string(), "${~/foo | slug}");
    }
}
