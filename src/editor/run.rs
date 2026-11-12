//! The gutter's ▶ buttons, Run and Debug at caret (runnable functions and
//! tests in the file, see `crate::run`, run by the workspace in a Run tab
//! or a debug session), and the file's breakpoints.

use std::{path::PathBuf, sync::Arc};

use gpui::{App, Context, Pixels, Point, Window};

use super::{Editor, EditorLspEvent, EditorMode};
use crate::{
    document::Breakpoint,
    run::{self, RunTarget},
    syntax::Language,
};

/// Run or debug this (from a gutter ▶, Run or Debug at caret).
pub struct EditorRunEvent {
    pub target: RunTarget,
    pub debug: bool,
    /// A ▶ was clicked here: offer Run and Debug.
    pub menu_at: Option<Point<Pixels>>,
}

impl gpui::EventEmitter<EditorRunEvent> for Editor {}

/// A SQL statement's ▶ was clicked: run the statement at `at` (its first word).
pub struct EditorRunSql {
    pub at: usize,
}

impl gpui::EventEmitter<EditorRunSql> for Editor {}

/// This file's breakpoints changed (a running debug session needs them).
pub struct EditorBreakpointsChanged;

impl gpui::EventEmitter<EditorBreakpointsChanged> for Editor {}

/// A line number was right-clicked: offer the breakpoint options for `row`.
pub struct EditorBreakpointMenu {
    pub row: usize,
    pub position: Point<Pixels>,
}

impl gpui::EventEmitter<EditorBreakpointMenu> for Editor {}

impl Editor {
    /// What can run in this file (cached per version).
    pub fn run_targets(&mut self, cx: &App) -> Arc<Vec<RunTarget>> {
        let (key, job) = self.run_job(cx);
        self.run_targets.now(key, job)
    }

    /// The targets to draw (gutter ▶): the last ones found (see `Derived`).
    pub(super) fn shown_run_targets(&mut self, cx: &mut Context<Self>) -> Arc<Vec<RunTarget>> {
        let (key, job) = self.run_job(cx);
        self.run_targets.schedule(key, job, |e| &mut e.run_targets, cx);
        self.run_targets.last()
    }

    /// Finding targets reads the disk (the crate, the package), so it runs
    /// in the background for drawing.
    fn run_job(&self, cx: &App) -> ((u64, Option<PathBuf>, Language), impl FnOnce() -> Vec<RunTarget> + Send + 'static) {
        let (path, language) = (self.doc.read(cx).path.clone(), self.doc.read(cx).language);
        let (tree, rope) = (self.doc.read(cx).syntax().tree().cloned(), self.doc.read(cx).rope().clone());
        let key = (self.doc.read(cx).tree_version(), path.clone(), language);
        let job = move || match (path, tree) {
            (Some(path), Some(tree)) => run::file_targets(language, &path, &rope.to_string(), &tree),
            _ => Vec::new(),
        };
        (key, job)
    }

    /// The target whose ▶ is on `row`.
    pub(super) fn run_target_at(&mut self, row: usize, cx: &App) -> Option<RunTarget> {
        self.run_targets(cx).iter().find(|t| t.row == Some(row)).cloned()
    }

    /// The innermost test or function around the caret, else the file's
    /// first runnable thing (only debuggable ones when `debug`).
    fn target_at_caret(&mut self, debug: bool, cx: &App) -> Option<RunTarget> {
        let row = self.doc.read(cx).row_of(self.primary_selection().head);
        let targets = self.run_targets(cx);
        let usable = |t: &&RunTarget| !debug || t.debug.is_some();
        targets
            .iter()
            .filter(usable)
            .filter(|t| t.rows.contains(&row))
            .min_by_key(|t| t.rows.len())
            .or_else(|| targets.iter().find(usable))
            .cloned()
    }

    /// Run at caret (⌃⇧R / Ctrl+Shift+F10).
    pub(super) fn run_at_caret(&mut self, _: &super::RunAtCaret, _: &mut Window, cx: &mut Context<Self>) {
        match self.target_at_caret(false, cx) {
            Some(target) => cx.emit(EditorRunEvent { target, debug: false, menu_at: None }),
            None => cx.emit(EditorLspEvent::Message("Nothing to run here".into())),
        }
    }

    /// Debug at caret (⌃⇧D / Ctrl+Shift+F9).
    pub(super) fn debug_at_caret(&mut self, _: &super::DebugAtCaret, _: &mut Window, cx: &mut Context<Self>) {
        match self.target_at_caret(true, cx) {
            Some(target) => cx.emit(EditorRunEvent { target, debug: true, menu_at: None }),
            None => cx.emit(EditorLspEvent::Message("Nothing to debug here".into())),
        }
    }

    // ---- SQL statements --------------------------------------------------------------

    /// Whether the gutter shows a ▶ per SQL statement, and by whose rules
    /// statements split: the workspace says so for SQL scripts while a
    /// database connection exists (the file's, else the first).
    pub fn set_sql_runs(&mut self, dialect: Option<crate::database::script::Dialect>, cx: &mut Context<Self>) {
        if self.sql_runs != dialect {
            self.sql_runs = dialect;
            cx.notify();
        }
    }

    /// Each statement's first row and offset: the last ones found (see `Derived`).
    pub(super) fn shown_sql_statements(&mut self, cx: &mut Context<Self>) -> Arc<Vec<(usize, usize)>> {
        let Some(dialect) = self.sql_runs else { return Arc::default() };
        let (key, job) = self.sql_job(dialect, cx);
        self.sql_statements.schedule(key, job, |e| &mut e.sql_statements, cx);
        self.sql_statements.last()
    }

    fn sql_job(&self, dialect: crate::database::script::Dialect, cx: &App) -> ((u64, crate::database::script::Dialect), impl FnOnce() -> Vec<(usize, usize)> + Send + 'static) {
        let rope = self.doc.read(cx).rope().clone();
        let job = move || {
            let mut rows: Vec<(usize, usize)> =
                crate::database::script::starts(&rope.to_string(), dialect).into_iter().map(|at| (rope.byte_to_line(at), at)).collect();
            // One ▶ per line: it runs the line's first statement.
            rows.dedup_by_key(|(row, _)| *row);
            rows
        };
        ((self.doc.read(cx).version(), dialect), job)
    }

    /// The statement whose ▶ is on `row` (by the current text).
    pub(super) fn sql_statement_at(&mut self, row: usize, cx: &App) -> Option<usize> {
        let dialect = self.sql_runs?;
        let (key, job) = self.sql_job(dialect, cx);
        let statements = self.sql_statements.now(key, job);
        statements.iter().find(|(r, _)| *r == row).map(|&(_, at)| at)
    }

    // ---- Breakpoints ---------------------------------------------------------------

    /// Breakpoints only go where a debugger can stop.
    pub fn can_break(&self, cx: &App) -> bool {
        let doc = self.doc.read(cx);
        self.mode == EditorMode::Full && doc.path.as_deref().is_some_and(|path| crate::debugger::launch::can_stop_in(doc.language, path))
    }

    pub fn toggle_breakpoint_at(&mut self, row: usize, cx: &mut Context<Self>) {
        self.doc.as_mut(cx).toggle_breakpoint(row);
        cx.emit(EditorBreakpointsChanged);
        cx.notify();
    }

    /// Toggle Line Breakpoint (⌘F8 / F9) on the caret's line.
    pub(super) fn toggle_breakpoint(&mut self, _: &super::ToggleBreakpoint, _: &mut Window, cx: &mut Context<Self>) {
        if !self.can_break(cx) {
            cx.emit(EditorLspEvent::Message(crate::debugger::launch::NO_DEBUGGER.into()));
            return;
        }
        let mut rows: Vec<usize> = self.selections.iter().map(|s| self.doc.read(cx).row_of(s.head)).collect();
        rows.dedup();
        for row in rows {
            self.doc.as_mut(cx).toggle_breakpoint(row);
        }
        cx.emit(EditorBreakpointsChanged);
        cx.notify();
    }

    pub fn breakpoints(&self, cx: &App) -> Vec<Breakpoint> {
        self.doc.read(cx).breakpoints()
    }

    /// Restores breakpoints (a file opening, a session).
    pub fn set_breakpoints(&mut self, breakpoints: &[Breakpoint], cx: &mut Context<Self>) {
        self.doc.as_mut(cx).set_breakpoints(breakpoints);
        cx.notify();
    }

    pub fn breakpoint(&self, row: usize, cx: &App) -> Option<Breakpoint> {
        self.doc.read(cx).breakpoint(row).cloned()
    }

    /// Adds or changes one breakpoint (a condition or log message was edited).
    pub fn set_breakpoint(&mut self, breakpoint: Breakpoint, cx: &mut Context<Self>) {
        self.doc.as_mut(cx).set_breakpoint(breakpoint);
        cx.emit(EditorBreakpointsChanged);
        cx.notify();
    }

    /// The caret's row.
    pub fn caret_row(&self, cx: &App) -> usize {
        self.doc.read(cx).row_of(self.primary_selection().head)
    }

    /// Highlights the line a debug session paused on (None: running or done).
    pub fn set_debug_line(&mut self, row: Option<usize>, cx: &mut Context<Self>) {
        if self.doc.read(cx).debug_row() != row {
            self.doc.as_mut(cx).set_debug_row(row);
            cx.notify();
        }
    }
}
