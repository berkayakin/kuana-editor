//! A Run tab's whole output as plain text, kept from the bytes its program
//! writes. The terminal's scrollback holds only its last rows, so a run that
//! prints more (a verbose test run, a long build) would lose its first errors
//! before Problems and the test results read them. The log sits between the
//! pty and alacritty's reader (`Teed`), sees every byte once, and keeps lines
//! the way they'd end up on screen: carriage returns and erased lines rewrite
//! the line (progress bars), other escape sequences are dropped.

use std::{
    collections::VecDeque,
    io::{self, Read},
    sync::{Arc, Mutex},
};

use alacritty_terminal::{
    event::{OnResize, WindowSize},
    tty::{ChildEvent, EventedPty, EventedReadWrite},
    vte::{Params, Parser, Perform},
};
use polling::{Event, PollMode, Poller};

/// The most text kept; past it the oldest lines go.
const MAX_BYTES: usize = 32 << 20;

/// Output turned into lines, shared between the pty's thread and the app.
#[derive(Clone, Default)]
pub struct RunLog(Arc<Mutex<Log>>);

impl RunLog {
    pub fn advance(&self, bytes: &[u8]) {
        let mut log = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Log { parser, lines } = &mut *log;
        parser.advance(lines, bytes);
    }

    /// Everything kept, a line each.
    pub fn text(&self) -> String {
        let log = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let lines = &log.lines;
        let mut out = String::with_capacity(lines.bytes + lines.line.len() + lines.done.len());
        for line in &lines.done {
            out.push_str(line);
            out.push('\n');
        }
        if !lines.line.is_empty() {
            out.push_str(lines.line.iter().collect::<String>().trim_end());
            out.push('\n');
        }
        out
    }
}

#[derive(Default)]
struct Log {
    parser: Parser,
    lines: Lines,
}

#[derive(Default)]
struct Lines {
    done: VecDeque<String>,
    /// Bytes in `done`.
    bytes: usize,
    /// The line being written, by column.
    line: Vec<char>,
    col: usize,
}

impl Lines {
    fn end_line(&mut self) {
        let line: String = self.line.drain(..).collect();
        let line = line.trim_end().to_string();
        self.bytes += line.len() + 1;
        self.done.push_back(line);
        self.col = 0;
        while self.bytes > MAX_BYTES
            && let Some(old) = self.done.pop_front()
        {
            self.bytes -= old.len() + 1;
        }
    }
}

impl Perform for Lines {
    fn print(&mut self, c: char) {
        if self.col < self.line.len() {
            self.line[self.col] = c;
        } else {
            self.line.resize(self.col, ' ');
            self.line.push(c);
        }
        self.col += 1;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.end_line(),
            b'\r' => self.col = 0,
            0x08 => self.col = self.col.saturating_sub(1),
            b'\t' => self.col = (self.col / 8 + 1) * 8,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        let first = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0) as usize;
        match action {
            // Erase in line: to its end, to the cursor, or all of it.
            'K' => match first {
                0 => self.line.truncate(self.col),
                1 => self.line.iter_mut().take(self.col + 1).for_each(|c| *c = ' '),
                _ => self.line.clear(),
            },
            // Cursor to a column, forward, back.
            'G' => self.col = first.saturating_sub(1),
            'C' => self.col += first.max(1),
            'D' => self.col = self.col.saturating_sub(first.max(1)),
            _ => {}
        }
    }
}

/// A pty whose output also goes to a `RunLog`, when it has one.
pub struct Teed<P> {
    reader: TeeReader<P>,
}

pub struct TeeReader<P> {
    pty: P,
    log: Option<RunLog>,
}

impl<P> Teed<P> {
    pub fn new(pty: P, log: Option<RunLog>) -> Self {
        Self { reader: TeeReader { pty, log } }
    }
}

impl<P: EventedReadWrite> Read for TeeReader<P> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.pty.reader().read(buf)?;
        if let Some(log) = &self.log {
            log.advance(&buf[..read]);
        }
        Ok(read)
    }
}

impl<P: EventedReadWrite> EventedReadWrite for Teed<P> {
    type Reader = TeeReader<P>;
    type Writer = P::Writer;

    unsafe fn register(&mut self, poller: &Arc<Poller>, event: Event, mode: PollMode) -> io::Result<()> {
        // SAFETY: the pty outlives its registration as it would unwrapped: `Teed` owns it.
        unsafe { self.reader.pty.register(poller, event, mode) }
    }

    fn reregister(&mut self, poller: &Arc<Poller>, event: Event, mode: PollMode) -> io::Result<()> {
        self.reader.pty.reregister(poller, event, mode)
    }

    fn deregister(&mut self, poller: &Arc<Poller>) -> io::Result<()> {
        self.reader.pty.deregister(poller)
    }

    fn reader(&mut self) -> &mut Self::Reader {
        &mut self.reader
    }

    fn writer(&mut self) -> &mut Self::Writer {
        self.reader.pty.writer()
    }
}

impl<P: EventedPty> EventedPty for Teed<P> {
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.reader.pty.next_child_event()
    }
}

impl<P: OnResize> OnResize for Teed<P> {
    fn on_resize(&mut self, size: WindowSize) {
        self.reader.pty.on_resize(size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logged(chunks: &[&[u8]]) -> String {
        let log = RunLog::default();
        for chunk in chunks {
            log.advance(chunk);
        }
        log.text()
    }

    #[test]
    fn keeps_every_line_past_any_scrollback() {
        let output: String = (0..30_000).map(|i| format!("line {i}\r\n")).collect();
        let text = logged(&[output.as_bytes()]);
        assert_eq!(text.lines().count(), 30_000);
        assert_eq!(text.lines().next(), Some("line 0"));
    }

    #[test]
    fn progress_bars_leave_their_last_state() {
        let text = logged(&[b"Building [=>  ] 1/3\r\x1b[K", b"Building [==> ] 2/3\r\x1b[Kerror: x\r\ndone"]);
        assert_eq!(text, "error: x\ndone\n");
    }

    #[test]
    fn colors_go_and_split_characters_come_together() {
        let text = logged(&[b"\x1b[1;31merror\x1b[0m: caf\xc3", b"\xa9\n"]);
        assert_eq!(text, "error: café\n");
    }
}
