// Vertical-list menu widget with vim navigation and letter shortcuts.
//
// Bindings:
//   j / ↓        — down (wraps)
//   k / ↑        — up (wraps)
//   Enter / l    — select highlighted
//   Space        — select highlighted
//   <shortcut>   — jump to and select that option in one keystroke
//   Esc          — select the configured cancel option, or no-op
//
// The control flow is split from rendering so unit tests can drive the
// decision loop with scripted keys without needing a TTY. Real callers
// use [`Menu::interact`], which does raw-mode key reads against the user's
// terminal and clears + redraws the menu on each navigation step.

use std::io::{self, Write};

use console::{Key, Term};

use crate::error::{Error, Result};

const FOOTER_HINT: &str = "j/k or ↑/↓ · Enter · letter to jump";
const CURSOR_MARK: &str = "❯ ";
const CURSOR_PAD: &str = "  ";

pub struct MenuOption<T> {
    pub shortcut: char,
    pub label: String,
    pub value: T,
}

impl<T> MenuOption<T> {
    pub fn new(shortcut: char, label: impl Into<String>, value: T) -> Self {
        Self {
            shortcut,
            label: label.into(),
            value,
        }
    }
}

pub struct Menu<T> {
    options: Vec<MenuOption<T>>,
    default_index: usize,
    cancel_index: Option<usize>,
    footer_hint: String,
}

impl<T> Menu<T> {
    pub fn new(options: Vec<MenuOption<T>>) -> Self {
        assert!(!options.is_empty(), "Menu requires at least one option");
        Self {
            options,
            default_index: 0,
            cancel_index: None,
            footer_hint: FOOTER_HINT.to_string(),
        }
    }

    pub fn default_index(mut self, index: usize) -> Self {
        assert!(index < self.options.len(), "default index out of range");
        self.default_index = index;
        self
    }

    /// Index of the option Esc should select. If unset, Esc is a no-op.
    pub fn cancel_index(mut self, index: usize) -> Self {
        assert!(index < self.options.len(), "cancel index out of range");
        self.cancel_index = Some(index);
        self
    }

    /// Run the menu against the user's terminal. Requires an attended TTY.
    pub fn interact(self) -> Result<T> {
        let term = Term::stderr();
        if !term.features().is_attended() {
            return Err(Error::Config(
                "interactive menu requires a terminal".to_string(),
            ));
        }
        let initial = self.default_index;
        let cancel = self.cancel_index;
        let footer = self.footer_hint.clone();
        let mut renderer = TermRenderer::new(term);
        let mut reader = TermKeyReader {
            term: renderer.term.clone(),
        };
        renderer.term.hide_cursor()?;
        let outcome = run(
            initial,
            cancel,
            &self.options,
            &footer,
            &mut reader,
            |state| renderer.render(state, &self.options, &footer),
        );
        let _ = renderer.term.show_cursor();
        let _ = renderer.clear();
        let chosen_index = outcome?;
        Ok(self.into_value(chosen_index))
    }

    fn into_value(mut self, index: usize) -> T {
        self.options.swap_remove(index).value
    }
}

/// One reduction step over the menu state. Pure — no I/O, no rendering.
fn next_state<T>(
    current: usize,
    key: Key,
    options: &[MenuOption<T>],
    cancel_index: Option<usize>,
) -> Step {
    let n = options.len();
    match key {
        Key::ArrowDown | Key::Char('j') => Step::Move((current + 1) % n),
        Key::ArrowUp | Key::Char('k') => Step::Move(if current == 0 { n - 1 } else { current - 1 }),
        Key::Enter | Key::Char('l') | Key::Char(' ') => Step::Select(current),
        Key::Escape => match cancel_index {
            Some(i) => Step::Select(i),
            None => Step::NoOp,
        },
        Key::Char(c) => {
            for (i, opt) in options.iter().enumerate() {
                if opt.shortcut == c {
                    return Step::Select(i);
                }
            }
            Step::NoOp
        }
        _ => Step::NoOp,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Move(usize),
    Select(usize),
    NoOp,
}

trait KeyReader {
    fn read_key(&mut self) -> io::Result<Key>;
}

struct TermKeyReader {
    term: Term,
}

impl KeyReader for TermKeyReader {
    fn read_key(&mut self) -> io::Result<Key> {
        self.term.read_key()
    }
}

/// Drives the decision loop. Rendering is supplied via the `render` closure
/// so tests can pass a no-op renderer.
fn run<T, R, F>(
    initial: usize,
    cancel_index: Option<usize>,
    options: &[MenuOption<T>],
    _footer: &str,
    reader: &mut R,
    mut render: F,
) -> Result<usize>
where
    R: KeyReader,
    F: FnMut(usize) -> io::Result<()>,
{
    let mut current = initial;
    render(current)?;
    loop {
        let key = reader.read_key()?;
        match next_state(current, key, options, cancel_index) {
            Step::Move(new) => {
                current = new;
                render(current)?;
            }
            Step::Select(i) => return Ok(i),
            Step::NoOp => {}
        }
    }
}

/// Owns the live terminal handle and tracks how many lines the last frame
/// occupied so we can wipe and redraw cleanly without disturbing whatever
/// the caller wrote above us.
struct TermRenderer {
    term: Term,
    lines_drawn: usize,
}

impl TermRenderer {
    fn new(term: Term) -> Self {
        Self {
            term,
            lines_drawn: 0,
        }
    }

    fn render<T>(
        &mut self,
        current: usize,
        options: &[MenuOption<T>],
        footer: &str,
    ) -> io::Result<()> {
        if self.lines_drawn > 0 {
            self.term.clear_last_lines(self.lines_drawn)?;
        }
        let mut buf = String::new();
        for (i, opt) in options.iter().enumerate() {
            let mark = if i == current {
                CURSOR_MARK
            } else {
                CURSOR_PAD
            };
            buf.push_str(mark);
            buf.push_str(&opt.label);
            buf.push('\n');
        }
        buf.push_str(footer);
        buf.push('\n');
        self.term.write_all(buf.as_bytes())?;
        self.lines_drawn = options.len() + 1;
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        if self.lines_drawn > 0 {
            self.term.clear_last_lines(self.lines_drawn)?;
            self.lines_drawn = 0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<MenuOption<&'static str>> {
        vec![
            MenuOption::new('o', "[o]verwrite", "overwrite"),
            MenuOption::new('b', "[b]ackup", "backup"),
            MenuOption::new('a', "[a]dopt", "adopt"),
            MenuOption::new('s', "[s]kip", "skip"),
            MenuOption::new('d', "[d]iff", "diff"),
            MenuOption::new('q', "[q]uit", "quit"),
        ]
    }

    #[test]
    fn arrow_down_moves_down_and_wraps() {
        let opts = options();
        assert_eq!(next_state(0, Key::ArrowDown, &opts, None), Step::Move(1));
        assert_eq!(next_state(5, Key::ArrowDown, &opts, None), Step::Move(0));
    }

    #[test]
    fn arrow_up_moves_up_and_wraps() {
        let opts = options();
        assert_eq!(next_state(1, Key::ArrowUp, &opts, None), Step::Move(0));
        assert_eq!(next_state(0, Key::ArrowUp, &opts, None), Step::Move(5));
    }

    #[test]
    fn vim_keys_match_arrows() {
        let opts = options();
        assert_eq!(
            next_state(0, Key::Char('j'), &opts, None),
            next_state(0, Key::ArrowDown, &opts, None)
        );
        assert_eq!(
            next_state(3, Key::Char('k'), &opts, None),
            next_state(3, Key::ArrowUp, &opts, None)
        );
    }

    #[test]
    fn enter_selects_current() {
        let opts = options();
        assert_eq!(next_state(2, Key::Enter, &opts, None), Step::Select(2));
    }

    #[test]
    fn l_and_space_also_select_current() {
        let opts = options();
        assert_eq!(next_state(2, Key::Char('l'), &opts, None), Step::Select(2));
        assert_eq!(next_state(2, Key::Char(' '), &opts, None), Step::Select(2));
    }

    #[test]
    fn letter_jumps_and_selects() {
        let opts = options();
        assert_eq!(next_state(0, Key::Char('d'), &opts, None), Step::Select(4));
        assert_eq!(next_state(3, Key::Char('o'), &opts, None), Step::Select(0));
    }

    #[test]
    fn unknown_letter_is_noop() {
        let opts = options();
        assert_eq!(next_state(0, Key::Char('z'), &opts, None), Step::NoOp);
    }

    #[test]
    fn esc_with_cancel_index_selects_it() {
        let opts = options();
        assert_eq!(next_state(0, Key::Escape, &opts, Some(5)), Step::Select(5));
    }

    #[test]
    fn esc_without_cancel_index_is_noop() {
        let opts = options();
        assert_eq!(next_state(0, Key::Escape, &opts, None), Step::NoOp);
    }

    /// Drives the full decision loop with scripted keys, no renderer.
    struct Scripted {
        keys: std::vec::IntoIter<Key>,
    }
    impl KeyReader for Scripted {
        fn read_key(&mut self) -> io::Result<Key> {
            self.keys.next().ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "scripted keys exhausted")
            })
        }
    }
    fn drive(initial: usize, cancel: Option<usize>, keys: Vec<Key>) -> usize {
        let opts = options();
        let mut reader = Scripted {
            keys: keys.into_iter(),
        };
        run(initial, cancel, &opts, FOOTER_HINT, &mut reader, |_| Ok(())).unwrap()
    }

    #[test]
    fn arrow_down_then_enter_selects_second_option() {
        assert_eq!(drive(0, None, vec![Key::ArrowDown, Key::Enter]), 1);
    }

    #[test]
    fn vim_navigation_then_l_selects() {
        assert_eq!(
            drive(
                0,
                None,
                vec![Key::Char('j'), Key::Char('j'), Key::Char('l')]
            ),
            2
        );
    }

    #[test]
    fn letter_shortcut_skips_navigation() {
        assert_eq!(drive(0, None, vec![Key::Char('q')]), 5);
    }

    #[test]
    fn esc_routes_to_cancel_index() {
        assert_eq!(drive(2, Some(5), vec![Key::Escape]), 5);
    }

    #[test]
    fn unknown_keys_are_skipped_until_real_input() {
        // Ignored noise keys (Tab, Char('x')) followed by a real selection.
        assert_eq!(
            drive(0, None, vec![Key::Tab, Key::Char('x'), Key::Char('s')]),
            3
        );
    }
}
