// Single-line input prompt, backed by `reedline`.
//
// Used for free-form input (commit messages). reedline handles all the
// editing keybindings (Ctrl+A/E/W/K/U, arrow keys, word boundaries, etc.)
// plus UTF-8, history if we ever want it, and termios cleanup on panic.

use std::borrow::Cow;

use anyhow::{Context, Result};
use reedline::{Prompt, PromptEditMode, PromptHistorySearch, Reedline, Signal};

#[derive(Debug)]
pub enum ReadLineOutcome {
    Line(String),
    /// User pressed ESC or Ctrl+C.
    Cancelled,
    /// User pressed Ctrl+D on an empty buffer.
    Eof,
}

pub fn read_line(prompt: &str) -> Result<ReadLineOutcome> {
    let mut editor = Reedline::create();
    let prompt = PlainPrompt(prompt.to_string());
    match editor.read_line(&prompt).context("reading line")? {
        Signal::Success(s) => Ok(ReadLineOutcome::Line(s)),
        Signal::CtrlC => Ok(ReadLineOutcome::Cancelled),
        Signal::CtrlD => Ok(ReadLineOutcome::Eof),
        // Signal is #[non_exhaustive]; treat any future variant as cancel.
        _ => Ok(ReadLineOutcome::Cancelled),
    }
}

struct PlainPrompt(String);

impl Prompt for PlainPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed(&self.0)
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
    fn render_prompt_history_search_indicator(&self, _: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Borrowed("")
    }
}
