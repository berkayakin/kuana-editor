//! What the terminal does with shell integration's marks (`shell_marks`):
//! a dot by each prompt for how its command ended, marks at the right edge
//! for every command in the scrollback, Previous / Next Command, Copy
//! Command Output, and the shell's folder for links and new terminals.

use std::{collections::HashMap, path::PathBuf};

use alacritty_terminal::{
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point as GridPoint},
    term::Term,
};
use gpui::{ClipboardItem, Context, Hsla, Pixels, Point, Window, fill, point, px, size};

use super::{
    Listener, TerminalCopyCommandOutput, TerminalFrame, TerminalNextCommand, TerminalPreviousCommand, TerminalView,
    shell_marks::{self, Mark},
};
use crate::theme::Theme;

/// Columns looked at for a prompt's start: prompts start a line (zsh makes
/// sure of it); a line's first few cells are cheap to check every frame.
const PROMPT_COLUMNS: usize = 3;

/// A prompt in the grid: its number, first and last line (negative lines
/// are the scrollback's).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Prompt {
    pub id: u32,
    pub first: i32,
    pub last: i32,
}

/// Commands by prompt: running (None) or ended, with the code if it was said.
#[derive(Default)]
pub(super) struct Commands {
    pub ran: HashMap<u32, Option<Option<i32>>>,
    /// The shell's folder (OSC 7).
    pub cwd: Option<PathBuf>,
    /// A prompt came: the shell is integrated, the grid is worth looking at.
    pub seen: bool,
}

impl Commands {
    pub fn take(&mut self, marks: Vec<Mark>) {
        for mark in marks {
            match mark {
                Mark::Prompt(_) => self.seen = true,
                Mark::Started(id) => {
                    self.ran.insert(id, None);
                }
                Mark::Finished(id, code) => {
                    self.ran.insert(id, Some(code));
                }
                Mark::Cwd(path) => self.cwd = Some(path),
            }
        }
    }
}

/// The prompts in the scrollback and on screen, oldest first.
pub(super) fn prompts(term: &Term<Listener>) -> Vec<Prompt> {
    let grid = term.grid();
    let top = -(grid.history_size() as i32);
    let bottom = term.screen_lines() as i32;
    let columns = term.columns().min(PROMPT_COLUMNS);
    let mut found: Vec<Prompt> = Vec::new();
    for line in top..bottom {
        let row = &grid[Line(line)];
        let id = (0..columns).find_map(|c| row[Column(c)].hyperlink().and_then(|h| shell_marks::prompt_of(h.uri())));
        let Some(id) = id else { continue };
        match found.last_mut() {
            // A prompt on several lines (or wrapped).
            Some(prompt) if prompt.id == id => prompt.last = line,
            _ => found.push(Prompt { id, first: line, last: line }),
        }
    }
    found
}

/// Scrolls the previous (`older`) or next prompt to the top of the view; past
/// the last, to the bottom.
pub(super) fn scroll_to_prompt(term: &mut Term<Listener>, older: bool) {
    let offset = term.grid().display_offset() as i32;
    // The line at the top of the view now.
    let top = -offset;
    let all = prompts(term);
    let target = if older { all.iter().rev().find(|p| p.first < top) } else { all.iter().find(|p| p.first > top) };
    match target {
        // Its first line at the top of the view (or the bottom of the scrollback).
        Some(target) => term.scroll_display(Scroll::Delta((-target.first).max(0) - offset)),
        None if !older => term.scroll_display(Scroll::Bottom),
        None => {}
    }
}

/// The output of the command typed at the prompt holding grid line `at`
/// (None: the last command that ended): the lines after its prompt up to
/// the next prompt (or the cursor's line), trailing spaces trimmed.
pub(super) fn output_of(term: &Term<Listener>, commands: &Commands, at: Option<i32>) -> Option<String> {
    let all = prompts(term);
    let ix = match at {
        Some(line) => all.iter().rposition(|p| p.first <= line)?,
        None => all.iter().rposition(|p| commands.ran.get(&p.id).is_some_and(|e| e.is_some()))?,
    };
    let start = all[ix].last + 1;
    let end = match all.get(ix + 1) {
        Some(next) => next.first - 1,
        None => term.grid().cursor.point.line.0 - 1,
    };
    if end < start {
        return Some(String::new());
    }
    let last_column = Column(term.columns().saturating_sub(1));
    let text = term.bounds_to_string(GridPoint::new(Line(start), Column(0)), GridPoint::new(Line(end), last_column));
    Some(text.lines().map(str::trim_end).collect::<Vec<_>>().join("\n").trim_end().to_string())
}

impl TerminalView {
    /// Marks the shell sent since the last look.
    pub(super) fn take_marks(&mut self) {
        if let Some(marks) = &self.shell_marks {
            let marks = marks.take();
            self.commands.take(marks);
        }
    }

    /// Dots by the prompts on screen and the commands' marks at the right edge.
    pub(super) fn paint_command_marks(&self, term: &Term<Listener>, frame: &mut TerminalFrame, origin: Point<Pixels>, cell_h: Pixels, offset: i32, cx: &gpui::App) {
        if !self.commands.seen {
            return;
        }
        let theme = Theme::get(cx);
        let color = |ended: Option<Option<i32>>| -> Hsla {
            match ended {
                None => theme.accent.into(),
                Some(Some(0)) => Hsla::from(theme.text_dim).alpha(0.8),
                Some(None) => Hsla::from(theme.text_dim).alpha(0.8),
                Some(Some(_)) => theme.error.into(),
            }
        };
        let rows = term.screen_lines() as i32;
        let all = prompts(term);
        let total = (term.grid().history_size() + term.screen_lines()).max(1) as f32;
        let top = -(term.grid().history_size() as i32);
        let bounds = frame.bounds;
        for prompt in &all {
            let Some(&ended) = self.commands.ran.get(&prompt.id) else { continue };
            // The dot, in the padding left of the prompt's first line.
            let row = prompt.first + offset;
            if (0..rows).contains(&row) {
                // In the 8-point padding, clear of the text.
                let d = px(5.);
                let y = origin.y + cell_h * row as f32 + (cell_h - d) / 2.;
                frame.marks.push(fill(gpui::Bounds::new(point(bounds.left() + px(1.), y), size(d, d)), color(ended)).corner_radii(d / 2.));
            }
            // Where it is in all the lines, at the right edge.
            let y = bounds.top() + bounds.size.height * ((prompt.first - top) as f32 / total);
            frame.marks.push(fill(gpui::Bounds::new(point(bounds.right() - px(4.), y), size(px(3.), px(2.))), color(ended)));
        }
    }

    fn scroll_to_prompt(&mut self, older: bool, cx: &mut Context<Self>) {
        scroll_to_prompt(&mut self.term.lock(), older);
        cx.notify();
    }

    pub(super) fn previous_command(&mut self, _: &TerminalPreviousCommand, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_to_prompt(true, cx);
    }

    pub(super) fn next_command(&mut self, _: &TerminalNextCommand, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_to_prompt(false, cx);
    }

    /// The output of the command that ran at a prompt: the lines after its
    /// prompt (and what was typed there) up to the next prompt. With no
    /// line: the last command that ended.
    pub fn command_output(&self, at: Option<i32>) -> Option<String> {
        output_of(&self.term.lock(), &self.commands, at)
    }

    pub(super) fn copy_command_output(&mut self, _: &TerminalCopyCommandOutput, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.command_output(None) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Copies the output of the command at a window position (the terminal's menu).
    pub fn copy_command_output_at(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let line = self.grid_point(position).0.line.0;
        if let Some(text) = self.command_output(Some(line)) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Whether the shell marks its commands (the menu offers their output).
    pub fn has_commands(&self) -> bool {
        self.commands.seen
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::{
        term::{Config, test::TermSize},
        vte::ansi::{Processor, StdSyncHandler},
    };
    use futures::channel::mpsc;

    use super::*;
    use crate::terminal::shell_marks::{MarkFilter, Marks};

    /// A terminal `rows` high that got `output` through the mark filter.
    fn shell(output: &str, rows: usize) -> (Term<Listener>, Commands) {
        let (tx, _rx) = mpsc::unbounded();
        let mut term = Term::new(Config::default(), &TermSize::new(30, rows), Listener(tx));
        let marks = Marks::default();
        let mut out = Vec::new();
        MarkFilter::new(marks.clone()).feed(output.as_bytes(), &mut out);
        Processor::<StdSyncHandler>::new().advance(&mut term, &out);
        let mut commands = Commands::default();
        commands.take(marks.take());
        (term, commands)
    }

    /// A prompt, a typed command, its output and how it ended.
    fn session(commands: &[(&str, &str, i32)]) -> String {
        let mut text = String::new();
        for (command, output, code) in commands {
            text += &format!("\x1b]133;A\x07$ \x1b]133;B\x07{command}\r\n\x1b]133;C\x07{output}\x1b]133;D;{code}\x07");
        }
        text + "\x1b]133;A\x07$ \x1b]133;B\x07"
    }

    #[test]
    fn prompts_and_command_output_come_from_the_grid() {
        let (term, commands) = shell(&session(&[("echo hi", "hi\r\nthere\r\n", 0), ("false", "", 1), ("ls", "a  b\r\n", 0)]), 12);
        let found: Vec<(u32, i32)> = prompts(&term).iter().map(|p| (p.id, p.first)).collect();
        assert_eq!(found, [(1, 0), (2, 3), (3, 4), (4, 6)]);
        assert_eq!(commands.ran.get(&1), Some(&Some(Some(0))));
        assert_eq!(commands.ran.get(&2), Some(&Some(Some(1))));
        assert!(commands.seen);
        // The last command that ended, and the one at a line.
        assert_eq!(output_of(&term, &commands, None).as_deref(), Some("a  b"));
        assert_eq!(output_of(&term, &commands, Some(1)).as_deref(), Some("hi\nthere"));
        assert_eq!(output_of(&term, &commands, Some(3)).as_deref(), Some(""));
    }

    #[test]
    fn prompts_are_found_in_the_scrollback_and_scrolled_to() {
        // Enough output that the first commands go into the scrollback.
        let long: String = (0..20).map(|i| format!("line {i}\r\n")).collect();
        let (mut term, commands) = shell(&session(&[("one", &long, 0), ("two", &long, 0)]), 8);
        let all = prompts(&term);
        assert_eq!(all.iter().map(|p| p.id).collect::<Vec<_>>(), [1, 2, 3]);
        assert!(all[0].first < 0 && all[1].first < 0, "{all:?}");
        assert_eq!(output_of(&term, &commands, Some(all[0].first)).unwrap().lines().count(), 20);
        // Previous: prompt 2's line at the top; again: prompt 1's; next: back to 2, then the bottom.
        scroll_to_prompt(&mut term, true);
        assert_eq!(term.grid().display_offset() as i32, -all[1].first);
        scroll_to_prompt(&mut term, true);
        assert_eq!(term.grid().display_offset() as i32, -all[0].first);
        scroll_to_prompt(&mut term, false);
        assert_eq!(term.grid().display_offset() as i32, -all[1].first);
        scroll_to_prompt(&mut term, false);
        scroll_to_prompt(&mut term, false);
        assert_eq!(term.grid().display_offset(), 0);
    }
}
