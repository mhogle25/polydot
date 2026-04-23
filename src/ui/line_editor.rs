// Single-line input widget with shell-style hotkeys.
//
// Architecture mirrors the lish REPL's line editor (see lish-zig
// src/line_editor.zig):
//   - Termios raw mode wrapped in an RAII guard (Drop restores).
//   - Byte-by-byte read with an escape-sequence state machine
//     (.ground / .escape / .csi / .csi_param / .csi_subparam).
//   - Buffer + cursor index; insert/delete shift bytes in place.
//   - Whole-line redraw via `\r` + prompt + buffer + `\x1b[K` + reposition.
//
// Bindings:
//   Printable byte      — insert at cursor
//   ←/→  Ctrl+B/F       — char left/right
//   Alt+B/F  ESC[1;3D/C — word left/right (punctuation-aware)
//   Home Ctrl+A         — beginning of line
//   End  Ctrl+E         — end of line
//   Backspace Ctrl+H    — delete char before cursor
//   Del                 — delete char under cursor
//   Ctrl+W              — delete word before cursor (punctuation-aware)
//   Ctrl+U              — kill whole line
//   Ctrl+K              — kill from cursor to end
//   Enter               — submit
//   Ctrl+D (empty buf)  — EOF
//   Ctrl+C / ESC        — cancel
//
// Improvements over lish (also worth porting back):
//   1. RAII termios guard. Restoration runs on every exit path including
//      panics — no global static, no SIGINT handler.
//   2. Punctuation-aware word boundaries. `is_word_char` treats only
//      [a-zA-Z0-9_] as word — Ctrl+W on `~/.config/foo` deletes only `foo`,
//      not the whole path.
//   3. ESC timeout. Lone ESC is ambiguous with `ESC b`/`ESC f`; we wait
//      ESC_TIMEOUT for a follow-up byte and treat the lack of one as
//      a standalone ESC (cancel).
//
// ASCII-only on input — multibyte bytes are inserted blindly which is
// fine for echoing back but cursor math will be off if the user types
// emoji in a commit subject. Acceptable for a v1 commit-message prompt.

use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

use anyhow::{Context, Result};

const ESC_TIMEOUT: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum ReadLineOutcome {
    Line(String),
    /// User pressed ESC or Ctrl+C.
    Cancelled,
    /// User pressed Ctrl+D on an empty buffer.
    Eof,
}

/// Read a single line from stdin with shell-style editing. Caller owns
/// the prompt's leading text — we render it verbatim each refresh.
pub fn read_line(prompt: &str) -> Result<ReadLineOutcome> {
    let stdin_fd = io::stdin().as_raw_fd();
    let _guard = RawMode::enter(stdin_fd)?;
    let mut editor = LineEditor::new();
    let mut stdout = io::stdout();
    editor.run(prompt, stdin_fd, &mut stdout)
}

// ----- RAII raw-mode guard --------------------------------------------------

struct RawMode {
    fd: RawFd,
    original: libc::termios,
}

impl RawMode {
    fn enter(fd: RawFd) -> Result<Self> {
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error()).context("tcgetattr");
        }
        let mut raw = original;
        // Local: no echo, no line buffering, no Ctrl+C signal (we read it),
        //        no literal-next escape (Ctrl+V).
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
        // Input: no Ctrl+S/Q flow control, no \r→\n translation, no break
        //        signaling, no parity check, no high-bit stripping.
        raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::BRKINT | libc::INPCK | libc::ISTRIP);
        if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) } != 0 {
            return Err(io::Error::last_os_error()).context("tcsetattr (raw)");
        }
        Ok(Self { fd, original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Best-effort restore — we're already exiting; nothing useful to do
        // on failure.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSAFLUSH, &self.original);
        }
    }
}

// ----- Editor state ---------------------------------------------------------

struct LineEditor {
    buffer: Vec<u8>,
    cursor: usize,
    escape_state: EscapeState,
    csi_param: u16,
    csi_subparam: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeState {
    Ground,
    Escape,
    Csi,
    CsiParam,
    CsiSubparam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeAction {
    MoveLeft,
    MoveRight,
    MoveWordLeft,
    MoveWordRight,
    Home,
    End,
    DeleteForward,
}

/// Outcome of feeding a single byte to the escape state machine. Distinguishes
/// "byte was consumed by the sequence" (even if it produced no action — e.g.
/// the closer of an unknown CSI) from "byte wasn't part of any sequence and
/// should be processed as regular input." Without this distinction, a byte
/// like `Z` in `ESC[Z` would close the sequence (state → Ground) and then
/// also get inserted as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeStep {
    NotEscape,
    Consumed,
    Action(EscapeAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputResult {
    Continue,
    Submit,
    Cancel,
    Eof,
}

impl LineEditor {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            cursor: 0,
            escape_state: EscapeState::Ground,
            csi_param: 0,
            csi_subparam: 0,
        }
    }

    fn run<W: Write>(&mut self, prompt: &str, fd: RawFd, out: &mut W) -> Result<ReadLineOutcome> {
        self.refresh(prompt, out)?;
        loop {
            // Block indefinitely when not mid-escape; apply ESC timeout when
            // we've just seen ESC and need to disambiguate standalone-vs-Alt.
            let in_escape = self.escape_state != EscapeState::Ground;
            let byte_opt = if in_escape {
                read_byte_with_timeout(fd, ESC_TIMEOUT)?
            } else {
                Some(read_byte_blocking(fd)?)
            };

            let byte = match byte_opt {
                Some(b) => b,
                None => {
                    // Timed out mid-escape. If we were waiting after a lone
                    // ESC, that's a cancel; for partial CSI sequences just
                    // drop them on the floor and resume.
                    if self.escape_state == EscapeState::Escape {
                        self.escape_state = EscapeState::Ground;
                        writeln!(out)?;
                        return Ok(ReadLineOutcome::Cancelled);
                    }
                    self.escape_state = EscapeState::Ground;
                    continue;
                }
            };

            match self.process_byte(byte) {
                InputResult::Continue => self.refresh(prompt, out)?,
                InputResult::Submit => {
                    writeln!(out)?;
                    out.flush().ok();
                    let s = String::from_utf8_lossy(&self.buffer).into_owned();
                    return Ok(ReadLineOutcome::Line(s));
                }
                InputResult::Cancel => {
                    writeln!(out)?;
                    out.flush().ok();
                    return Ok(ReadLineOutcome::Cancelled);
                }
                InputResult::Eof => {
                    writeln!(out)?;
                    out.flush().ok();
                    return Ok(ReadLineOutcome::Eof);
                }
            }
        }
    }

    fn process_byte(&mut self, byte: u8) -> InputResult {
        // Run the escape state machine first. If it owns the byte (either
        // mid-sequence or as a sequence closer — even an unknown one), we're
        // done. Only fall through to regular handling if the byte wasn't
        // part of any escape sequence.
        match self.process_escape(byte) {
            EscapeStep::Action(action) => {
                self.handle_escape_action(action);
                return InputResult::Continue;
            }
            EscapeStep::Consumed => return InputResult::Continue,
            EscapeStep::NotEscape => {}
        }

        match byte {
            b'\r' | b'\n' => InputResult::Submit,
            0x03 => InputResult::Cancel, // Ctrl+C
            0x04 => {
                // Ctrl+D — EOF on empty buffer, otherwise forward-delete.
                if self.buffer.is_empty() {
                    InputResult::Eof
                } else {
                    self.delete_forward();
                    InputResult::Continue
                }
            }
            0x01 => {
                self.move_to_start();
                InputResult::Continue
            }
            0x02 => {
                self.move_left();
                InputResult::Continue
            }
            0x05 => {
                self.move_to_end();
                InputResult::Continue
            }
            0x06 => {
                self.move_right();
                InputResult::Continue
            }
            0x08 | 0x7f => {
                self.delete_backward();
                InputResult::Continue
            }
            0x0b => {
                self.kill_to_end();
                InputResult::Continue
            }
            0x15 => {
                self.kill_line();
                InputResult::Continue
            }
            0x17 => {
                self.delete_word_backward();
                InputResult::Continue
            }
            // Printable ASCII or any byte ≥ 0x20 (incl. UTF-8 continuations).
            // Don't insert other control chars.
            b if b >= 0x20 => {
                self.insert_byte(b);
                InputResult::Continue
            }
            _ => InputResult::Continue,
        }
    }

    fn process_escape(&mut self, byte: u8) -> EscapeStep {
        match self.escape_state {
            EscapeState::Ground => {
                if byte == 0x1b {
                    self.escape_state = EscapeState::Escape;
                    self.csi_param = 0;
                    EscapeStep::Consumed
                } else {
                    EscapeStep::NotEscape
                }
            }
            EscapeState::Escape => {
                if byte == b'[' {
                    self.escape_state = EscapeState::Csi;
                    return EscapeStep::Consumed;
                }
                self.escape_state = EscapeState::Ground;
                // Alt+B / Alt+F (readline convention).
                match byte {
                    b'b' | b'B' => EscapeStep::Action(EscapeAction::MoveWordLeft),
                    b'f' | b'F' => EscapeStep::Action(EscapeAction::MoveWordRight),
                    _ => EscapeStep::Consumed,
                }
            }
            EscapeState::Csi => match byte {
                b'A' | b'B' => {
                    // ↑/↓ — no history; swallow.
                    self.escape_state = EscapeState::Ground;
                    EscapeStep::Consumed
                }
                b'C' => {
                    self.escape_state = EscapeState::Ground;
                    EscapeStep::Action(EscapeAction::MoveRight)
                }
                b'D' => {
                    self.escape_state = EscapeState::Ground;
                    EscapeStep::Action(EscapeAction::MoveLeft)
                }
                b'H' => {
                    self.escape_state = EscapeState::Ground;
                    EscapeStep::Action(EscapeAction::Home)
                }
                b'F' => {
                    self.escape_state = EscapeState::Ground;
                    EscapeStep::Action(EscapeAction::End)
                }
                b'0'..=b'9' => {
                    self.csi_param = u16::from(byte - b'0');
                    self.escape_state = EscapeState::CsiParam;
                    EscapeStep::Consumed
                }
                _ => {
                    self.escape_state = EscapeState::Ground;
                    EscapeStep::Consumed
                }
            },
            EscapeState::CsiParam => match byte {
                b'0'..=b'9' => {
                    self.csi_param = self
                        .csi_param
                        .saturating_mul(10)
                        .saturating_add(u16::from(byte - b'0'));
                    EscapeStep::Consumed
                }
                b';' => {
                    self.csi_subparam = 0;
                    self.escape_state = EscapeState::CsiSubparam;
                    EscapeStep::Consumed
                }
                b'~' => {
                    self.escape_state = EscapeState::Ground;
                    if self.csi_param == 3 {
                        EscapeStep::Action(EscapeAction::DeleteForward)
                    } else {
                        EscapeStep::Consumed
                    }
                }
                _ => {
                    self.escape_state = EscapeState::Ground;
                    EscapeStep::Consumed
                }
            },
            // ESC[1;3C / ESC[1;3D — xterm-style Alt+Right / Alt+Left.
            EscapeState::CsiSubparam => match byte {
                b'0'..=b'9' => {
                    self.csi_subparam = self
                        .csi_subparam
                        .saturating_mul(10)
                        .saturating_add(u16::from(byte - b'0'));
                    EscapeStep::Consumed
                }
                b'C' => {
                    self.escape_state = EscapeState::Ground;
                    if self.csi_param == 1 && self.csi_subparam == 3 {
                        EscapeStep::Action(EscapeAction::MoveWordRight)
                    } else {
                        EscapeStep::Consumed
                    }
                }
                b'D' => {
                    self.escape_state = EscapeState::Ground;
                    if self.csi_param == 1 && self.csi_subparam == 3 {
                        EscapeStep::Action(EscapeAction::MoveWordLeft)
                    } else {
                        EscapeStep::Consumed
                    }
                }
                _ => {
                    self.escape_state = EscapeState::Ground;
                    EscapeStep::Consumed
                }
            },
        }
    }

    fn handle_escape_action(&mut self, action: EscapeAction) {
        match action {
            EscapeAction::MoveLeft => self.move_left(),
            EscapeAction::MoveRight => self.move_right(),
            EscapeAction::MoveWordLeft => self.move_word_left(),
            EscapeAction::MoveWordRight => self.move_word_right(),
            EscapeAction::Home => self.move_to_start(),
            EscapeAction::End => self.move_to_end(),
            EscapeAction::DeleteForward => self.delete_forward(),
        }
    }

    // ----- Editing operations ----------------------------------------------

    fn insert_byte(&mut self, byte: u8) {
        self.buffer.insert(self.cursor, byte);
        self.cursor += 1;
    }

    fn delete_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.buffer.remove(self.cursor);
    }

    fn delete_forward(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        self.buffer.remove(self.cursor);
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    fn move_right(&mut self) {
        if self.cursor < self.buffer.len() {
            self.cursor += 1;
        }
    }

    fn move_to_start(&mut self) {
        self.cursor = 0;
    }

    fn move_to_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    fn move_word_left(&mut self) {
        let mut pos = self.cursor;
        while pos > 0 && !is_word_char(self.buffer[pos - 1]) {
            pos -= 1;
        }
        while pos > 0 && is_word_char(self.buffer[pos - 1]) {
            pos -= 1;
        }
        self.cursor = pos;
    }

    fn move_word_right(&mut self) {
        let len = self.buffer.len();
        let mut pos = self.cursor;
        while pos < len && !is_word_char(self.buffer[pos]) {
            pos += 1;
        }
        while pos < len && is_word_char(self.buffer[pos]) {
            pos += 1;
        }
        self.cursor = pos;
    }

    fn delete_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut target = self.cursor;
        while target > 0 && !is_word_char(self.buffer[target - 1]) {
            target -= 1;
        }
        while target > 0 && is_word_char(self.buffer[target - 1]) {
            target -= 1;
        }
        self.buffer.drain(target..self.cursor);
        self.cursor = target;
    }

    fn kill_line(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
    }

    fn kill_to_end(&mut self) {
        self.buffer.truncate(self.cursor);
    }

    // ----- Render ----------------------------------------------------------

    fn refresh<W: Write>(&self, prompt: &str, out: &mut W) -> Result<()> {
        // \r — back to column 0
        // prompt + buffer
        // \x1b[K — clear from cursor to end of line (wipes leftover chars)
        // \x1b[{n}D — reposition cursor backwards by `chars_after_cursor`
        out.write_all(b"\r")?;
        out.write_all(prompt.as_bytes())?;
        out.write_all(&self.buffer)?;
        out.write_all(b"\x1b[K")?;
        let after = self.buffer.len() - self.cursor;
        if after > 0 {
            write!(out, "\x1b[{after}D")?;
        }
        out.flush()?;
        Ok(())
    }
}

/// True for ASCII alphanumerics and underscore. Punctuation (`/`, `-`, `.`,
/// etc.) is treated as a word boundary so Ctrl+W on `~/.config/foo` deletes
/// only `foo`, which is what people actually want when typing path-like
/// commit messages.
fn is_word_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

// ----- Raw stdin reads ------------------------------------------------------

fn read_byte_blocking(fd: RawFd) -> Result<u8> {
    let mut buf = [0u8; 1];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), 1) };
        if n == 1 {
            return Ok(buf[0]);
        }
        if n == 0 {
            anyhow::bail!("unexpected EOF on stdin");
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err).context("read");
    }
}

fn read_byte_with_timeout(fd: RawFd, timeout: Duration) -> Result<Option<u8>> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    loop {
        let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if n > 0 {
            return read_byte_blocking(fd).map(Some);
        }
        if n == 0 {
            return Ok(None);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err).context("poll");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(bytes: &[u8]) -> (LineEditor, Vec<InputResult>) {
        let mut ed = LineEditor::new();
        let results = bytes.iter().map(|b| ed.process_byte(*b)).collect();
        (ed, results)
    }

    fn buffer(ed: &LineEditor) -> String {
        String::from_utf8(ed.buffer.clone()).unwrap()
    }

    #[test]
    fn plain_text_is_inserted() {
        let (ed, _) = drive(b"hello");
        assert_eq!(buffer(&ed), "hello");
        assert_eq!(ed.cursor, 5);
    }

    #[test]
    fn enter_submits() {
        let (_, results) = drive(b"hi\r");
        assert_eq!(results.last(), Some(&InputResult::Submit));
    }

    #[test]
    fn ctrl_c_cancels() {
        let (_, results) = drive(&[b'a', 0x03]);
        assert_eq!(results.last(), Some(&InputResult::Cancel));
    }

    #[test]
    fn ctrl_d_on_empty_buffer_is_eof() {
        let (_, results) = drive(&[0x04]);
        assert_eq!(results.last(), Some(&InputResult::Eof));
    }

    #[test]
    fn ctrl_d_with_content_forward_deletes() {
        // Type "ab", move left, Ctrl+D — should delete 'b' and continue.
        let mut bytes = vec![b'a', b'b', 0x02, 0x04];
        let (ed, results) = drive(&bytes.split_off(0));
        assert_eq!(buffer(&ed), "a");
        assert_eq!(results.last(), Some(&InputResult::Continue));
    }

    #[test]
    fn backspace_deletes_previous_char() {
        let (ed, _) = drive(b"abc\x7f");
        assert_eq!(buffer(&ed), "ab");
        assert_eq!(ed.cursor, 2);
    }

    #[test]
    fn ctrl_a_and_ctrl_e_jump_to_ends() {
        // Type "abc", Ctrl+A, then 'X' inserted at position 0, Ctrl+E, 'Y'.
        let (ed, _) = drive(&[b'a', b'b', b'c', 0x01, b'X', 0x05, b'Y']);
        assert_eq!(buffer(&ed), "XabcY");
        assert_eq!(ed.cursor, 5);
    }

    #[test]
    fn ctrl_w_deletes_word_backward_punctuation_aware() {
        // Path-like input: `~/.config/foo` then Ctrl+W → only `foo` gone.
        let (ed, _) = drive(&[
            b'~', b'/', b'.', b'c', b'o', b'n', b'f', b'i', b'g', b'/', b'f', b'o', b'o', 0x17,
        ]);
        assert_eq!(buffer(&ed), "~/.config/");
    }

    #[test]
    fn ctrl_u_kills_whole_line() {
        let (ed, _) = drive(&[b'a', b'b', b'c', 0x15]);
        assert_eq!(buffer(&ed), "");
        assert_eq!(ed.cursor, 0);
    }

    #[test]
    fn ctrl_k_kills_to_end() {
        // "abcdef", move cursor left 3 (cursor at 3), Ctrl+K → keep "abc".
        let (ed, _) = drive(&[b'a', b'b', b'c', b'd', b'e', b'f', 0x02, 0x02, 0x02, 0x0b]);
        assert_eq!(buffer(&ed), "abc");
        assert_eq!(ed.cursor, 3);
    }

    #[test]
    fn arrow_left_moves_cursor() {
        // "ab", ESC [ D → cursor at 1, then 'X' inserted between.
        let (ed, _) = drive(&[b'a', b'b', 0x1b, b'[', b'D', b'X']);
        assert_eq!(buffer(&ed), "aXb");
    }

    #[test]
    fn arrow_right_at_end_is_noop() {
        let (ed, _) = drive(&[b'a', 0x1b, b'[', b'C']);
        assert_eq!(buffer(&ed), "a");
        assert_eq!(ed.cursor, 1);
    }

    #[test]
    fn home_and_end_via_csi() {
        // "abc", ESC[H (Home), 'X', ESC[F (End), 'Y'.
        let (ed, _) = drive(&[
            b'a', b'b', b'c', 0x1b, b'[', b'H', b'X', 0x1b, b'[', b'F', b'Y',
        ]);
        assert_eq!(buffer(&ed), "XabcY");
    }

    #[test]
    fn delete_key_via_csi_3_tilde() {
        // "abc", move left twice (cursor at 1), Delete → remove 'b'.
        let (ed, _) = drive(&[b'a', b'b', b'c', 0x02, 0x02, 0x1b, b'[', b'3', b'~']);
        assert_eq!(buffer(&ed), "ac");
    }

    #[test]
    fn alt_b_jumps_word_left_punctuation_aware() {
        // "foo/bar baz" — cursor at end, Alt+B should land before "baz".
        let (ed, _) = drive(&[
            b'f', b'o', b'o', b'/', b'b', b'a', b'r', b' ', b'b', b'a', b'z', 0x1b, b'b',
        ]);
        assert_eq!(ed.cursor, 8);
    }

    #[test]
    fn alt_f_jumps_word_right_punctuation_aware() {
        // "foo/bar" cursor at 0 → Alt+F lands at 3 (end of "foo").
        let (ed, _) = drive(&[b'f', b'o', b'o', b'/', b'b', b'a', b'r', 0x01, 0x1b, b'f']);
        assert_eq!(ed.cursor, 3);
    }

    #[test]
    fn xterm_alt_left_via_csi_1_3_d() {
        // "foo bar", cursor at end → ESC[1;3D should jump back to before "bar".
        let (ed, _) = drive(&[
            b'f', b'o', b'o', b' ', b'b', b'a', b'r', 0x1b, b'[', b'1', b';', b'3', b'D',
        ]);
        assert_eq!(ed.cursor, 4);
    }

    #[test]
    fn unknown_csi_letter_is_swallowed() {
        // ESC [ Z (BackTab) — not handled; should leave editor unchanged.
        let (ed, _) = drive(&[b'a', 0x1b, b'[', b'Z', b'b']);
        assert_eq!(buffer(&ed), "ab");
    }

    #[test]
    fn is_word_char_classifies_alphanumeric_and_underscore_only() {
        assert!(is_word_char(b'a'));
        assert!(is_word_char(b'Z'));
        assert!(is_word_char(b'0'));
        assert!(is_word_char(b'_'));
        assert!(!is_word_char(b'-'));
        assert!(!is_word_char(b'.'));
        assert!(!is_word_char(b'/'));
        assert!(!is_word_char(b' '));
    }
}
