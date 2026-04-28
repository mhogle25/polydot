// Shell-style path expansion.
//
// Expands:
//   - `~` at the start of input (followed by `/` or end-of-input) → home dir
//   - `$NAME` → env var contents
//   - `$$` → literal `$`
//
// Anything else passes through verbatim. No transforms, no substitution syntax.

use std::path::PathBuf;

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

pub fn expand(input: &str, env: &impl Env) -> Result<String> {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    // Leading `~` is home only when at index 0 and followed by `/` or end-of-input.
    if !bytes.is_empty() && bytes[0] == b'~' && (bytes.len() == 1 || bytes[1] == b'/') {
        let home = env
            .home()
            .ok_or_else(|| Error::Path("home directory not available".into()))?;
        out.push_str(&home.to_string_lossy());
        i = 1;
    }

    while i < bytes.len() {
        let c = bytes[i];
        if c == b'$' {
            // $$ → literal $
            if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
                out.push('$');
                i += 2;
                continue;
            }
            // $NAME → env var
            if i + 1 < bytes.len() && is_ident_start(bytes[i + 1]) {
                let name_start = i + 1;
                let mut name_end = name_start;
                while name_end < bytes.len() && is_ident_continue(bytes[name_end]) {
                    name_end += 1;
                }
                let name = &input[name_start..name_end];
                let value = env
                    .var(name)
                    .ok_or_else(|| Error::Path(format!("env var ${name} not set")))?;
                out.push_str(&value);
                i = name_end;
                continue;
            }
            // Lone `$` falls through as literal.
        }
        if c < 128 {
            out.push(c as char);
            i += 1;
        } else {
            let ch = input[i..]
                .chars()
                .next()
                .ok_or_else(|| Error::Path("invalid utf-8 in path string".into()))?;
            out.push(ch);
            i += ch.len_utf8();
        }
    }

    Ok(out)
}

fn is_ident_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic()
}

fn is_ident_continue(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn ex(s: &str) -> String {
        expand(s, &env()).unwrap()
    }

    #[test]
    fn literal_passes_through() {
        assert_eq!(ex("/foo/bar"), "/foo/bar");
        assert_eq!(ex(""), "");
    }

    #[test]
    fn home_at_start() {
        assert_eq!(ex("~"), "/home/test");
        assert_eq!(ex("~/foo"), "/home/test/foo");
    }

    #[test]
    fn home_only_at_start_of_string() {
        assert_eq!(ex("/foo/~/bar"), "/foo/~/bar");
    }

    #[test]
    fn home_followed_by_non_slash_is_literal() {
        // We don't support `~user` syntax; treat as literal.
        assert_eq!(ex("~user"), "~user");
    }

    #[test]
    fn env_var_expands() {
        assert_eq!(ex("$FOO"), "bar");
        assert_eq!(ex("/$FOO/baz"), "/bar/baz");
    }

    #[test]
    fn env_var_terminates_at_non_ident() {
        assert_eq!(ex("$FOO-suffix"), "bar-suffix");
    }

    #[test]
    fn dollar_dollar_renders_dollar() {
        assert_eq!(ex("$$5"), "$5");
    }

    #[test]
    fn lone_dollar_is_literal() {
        assert_eq!(ex("end$"), "end$");
    }

    #[test]
    fn missing_env_var_errors() {
        let err = expand("$NOPE", &env()).unwrap_err();
        assert!(matches!(err, Error::Path(_)));
    }

    #[test]
    fn missing_home_errors() {
        let no_home = MockEnv {
            home: None,
            vars: HashMap::new(),
        };
        let err = expand("~/foo", &no_home).unwrap_err();
        assert!(matches!(err, Error::Path(_)));
    }
}
