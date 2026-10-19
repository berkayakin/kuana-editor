//! The integrated terminal: a shell on a pseudo-terminal, emulated by
//! `alacritty_terminal` and painted with GPUI like the editor. Keys go to
//! the shell (the keymap turns off app shortcuts in the "Terminal"
//! context); text arrives through the input handler, so dead keys and IMEs
//! work (what an IME composes shows at the cursor until it's committed).
//! Find (⌘F) searches the scrollback and the screen.

use std::{
    borrow::Cow,
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use alacritty_terminal::{
    event::{Event as TermEvent, EventListener, Notify, OnResize, WindowSize},
    event_loop::{EventLoop, Msg, Notifier},
    grid::{Dimensions, Scroll},
    index::{Column, Direction, Line, Point as GridPoint, Side},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{
        Config, Term, TermMode,
        cell::{Cell, Flags},
        color::{COUNT, Colors},
        search::{Match, RegexIter, RegexSearch},
        test::TermSize,
    },
    tty,
    vte::ansi::{Color, CursorShape, NamedColor, Rgb},
};
use futures::{StreamExt, channel::mpsc};
use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font, FontStyle, FontWeight, GlobalElementId,
    Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, KeyDownEvent, LayoutId, Modifiers, ModifiersChangedEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render,
    ScrollWheelEvent, ShapedLine, SharedString, Style, Subscription, Task, TextRun, UTF16Selection, UnderlineStyle,
    Window, actions, div, fill, font, point, prelude::*, px, relative, size,
};

use crate::{
    assets::Icon,
    editor::{Cancel, Editor, EditorEvent, MoveDown, MoveUp, Newline, NewlineBelow},
    run::RunTarget,
    settings::Settings,
    theme::Theme,
    ui::{HoverFill, icon_el, input_box, tooltip_view},
};

actions!(terminal, [TerminalCopy, TerminalPaste, TerminalClear, TerminalFind]);

/// Matches beyond this aren't collected (a huge scrollback full of hits):
/// the newest ones are kept.
const MAX_MATCHES: usize = 5000;

/// Find in the output: the query field and its matches.
struct Search {
    input: Entity<Editor>,
    /// In grid coordinates as of `history`, top to bottom.
    matches: Vec<Match>,
    /// Scrollback lines when `matches` were found: new output moves lines up.
    history: usize,
    current: Option<usize>,
    /// Output arrived since `matches` were found.
    stale: bool,
    /// A search running in the background (see `schedule_search`).
    task: Option<Task<()>>,
    _subscription: Subscription,
}

/// Forwards the emulator's events to the view's task.
#[derive(Clone)]
struct Listener(mpsc::UnboundedSender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.unbounded_send(event);
    }
}

pub enum TerminalEvent {
    TitleChanged,
    /// The shell exited; the tab should close. Once, after its last output.
    Exited,
    /// The shell couldn't start (its pty starts in the background).
    Failed(String),
    /// ⌘/Ctrl-click on a `path:line:col` in the output (1-based line and column).
    OpenFile { path: PathBuf, line: Option<u32>, column: Option<u32> },
}

/// Something clickable in the output.
#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    Url(String),
    File { path: PathBuf, line: Option<u32>, column: Option<u32> },
}

pub struct TerminalView {
    term: Arc<FairMutex<Term<Listener>>>,
    pty: Pty,
    title: SharedString,
    /// A name the user gave the tab, shown instead of the shell's title.
    custom_title: Option<SharedString>,
    /// The shell's process, to find the folder it's in now and what it runs.
    process: Option<ShellProcess>,
    focus_handle: FocusHandle,
    /// Columns, rows and cell size last sent to the shell.
    size: Option<(u16, u16, Pixels, Pixels)>,
    /// Geometry from the last frame, for mouse hit-testing.
    origin: Point<Pixels>,
    cell: (Pixels, Pixels),
    selecting: bool,
    /// Remainder of fractional wheel scrolling, in lines.
    scroll_rest: f32,
    /// Set for a Run tab: what runs, and its exit code once it's done.
    run: Option<(RunTarget, RunState)>,
    /// The program's exit code, from its exit until its output is all read.
    exit_code: Option<i32>,
    /// Where the shell started, for relative paths in its output.
    cwd: PathBuf,
    /// The link under the pointer while ⌘/Ctrl is held: screen row, columns.
    link: Option<(usize, Range<usize>, Link)>,
    last_mouse: Option<Point<Pixels>>,
    search: Option<Search>,
    /// What an input method is composing, not sent to the shell yet.
    marked: Option<Marked>,
    _events: Task<()>,
}

/// The shell's pty, which starts in the background (`TerminalView::create`).
enum Pty {
    /// Starting: what's typed meanwhile waits here.
    Starting(Vec<u8>),
    Running(Notifier),
    /// It couldn't start.
    Failed,
}

/// A started pty, from the background.
struct Started {
    notifier: Notifier,
    process: Option<ShellProcess>,
}

/// A terminal's processes, to tell its shell at the prompt from a command
/// the shell runs.
#[cfg_attr(not(unix), allow(dead_code))]
struct ShellProcess {
    /// The pty's process: the shell, or `login` running it (`login`).
    pid: u32,
    /// On macOS a shell that isn't configured starts through `/usr/bin/login`
    /// (alacritty's default, so it's a tty session), which runs the shell as
    /// its child: the shell is that child (`shell`, found once it's there).
    login: bool,
    shell: std::cell::OnceCell<u32>,
    /// A copy of the pty's master end, to ask which process group is in the
    /// foreground.
    master: Option<std::fs::File>,
}

impl ShellProcess {
    /// The shell's process (see `login`).
    fn shell(&self) -> Option<u32> {
        if !self.login {
            return Some(self.pid);
        }
        if let Some(&shell) = self.shell.get() {
            return Some(shell);
        }
        let shell = first_child(self.pid)?;
        Some(*self.shell.get_or_init(|| shell))
    }

    /// The command in the pty's foreground, by name, unless that's the shell
    /// (or `login`, until the shell takes the foreground).
    fn foreground_command(&self) -> Option<String> {
        let group = foreground_group(self.master.as_ref()?)?;
        let shell = self.shell()?;
        (group != shell && group != self.pid).then(|| process_name(group).unwrap_or_else(|| "A process".into()))
    }
}

/// Text an input method is composing (Chinese, Japanese, Korean, dead
/// keys): drawn at the cursor, and sent to the shell once it's committed.
#[derive(Debug, PartialEq)]
struct Marked {
    text: String,
    /// The part the input method selected, in UTF-16 units of `text`: its
    /// caret when empty, the clause being converted otherwise.
    selected: Option<Range<usize>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunState {
    Running,
    /// Exit code (None when it was killed by a signal).
    Exited(Option<i32>),
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl TerminalView {
    /// Starts a shell in `cwd`, or with `run`, a command in the user's shell
    /// that stays on screen with its exit code when done. The tab is there
    /// at once; its shell starts in the background (starting a process and
    /// looking for the project's venv don't hold up the window), and
    /// `TerminalEvent::Failed` says if it couldn't.
    pub fn create(cwd: PathBuf, run: Option<RunTarget>, cx: &mut App) -> Entity<Self> {
        let configured = Settings::get(cx).terminal_shell.clone();
        let (tx, rx) = mpsc::unbounded();
        let listener = Listener(tx);
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &TermSize::new(80, 24), listener.clone())));
        cx.new(|cx: &mut Context<Self>| {
            let start = {
                let (term, cwd, run) = (term.clone(), cwd.clone(), run.clone());
                cx.background_executor().spawn(async move { start_pty(configured, &cwd, run.as_ref(), term, listener) })
            };
            cx.spawn(async move |this, cx| {
                let mut started = Some(start.await);
                this.update(cx, |view, cx| {
                    if let Some(started) = started.take() {
                        view.pty_started(started, cx);
                    }
                })
                .ok();
                // Closed before its shell was up: end the shell.
                if let Some(Ok(pty)) = started {
                    let _ = pty.notifier.0.send(Msg::Shutdown);
                }
            })
            .detach();
            let mut view = Self::new(term, rx, cwd, cx);
            if let Some(target) = run {
                view.title = target.name.clone().into();
                view.run = Some((target, RunState::Running));
            }
            view
        })
    }

    /// The pty is up: what was typed meanwhile goes to the shell, at the
    /// size the view has by now.
    fn pty_started(&mut self, started: std::io::Result<Started>, cx: &mut Context<Self>) {
        match started {
            Ok(Started { notifier, process }) => {
                let Pty::Starting(typed) = std::mem::replace(&mut self.pty, Pty::Running(notifier)) else { return };
                self.process = process;
                if let (Pty::Running(notifier), Some(size)) = (&mut self.pty, self.size) {
                    notifier.on_resize(window_size(size));
                }
                if !typed.is_empty() {
                    self.write(typed);
                }
            }
            Err(err) => {
                self.pty = Pty::Failed;
                cx.emit(TerminalEvent::Failed(err.to_string()));
            }
        }
    }

    /// What a Run tab runs and how it went.
    pub fn run(&self) -> Option<&(RunTarget, RunState)> {
        self.run.as_ref()
    }

    /// The output, to read as text on another thread (a Run's problems).
    pub fn output(&self) -> Output {
        Output(self.term.clone())
    }

    /// Interrupts the running program (Ctrl+C).
    pub fn interrupt(&mut self, cx: &mut Context<Self>) {
        self.input(&b"\x03"[..], cx);
    }

    /// The view of a terminal whose pty starts later (`pty_started`).
    fn new(term: Arc<FairMutex<Term<Listener>>>, mut rx: mpsc::UnboundedReceiver<TermEvent>, cwd: PathBuf, cx: &mut Context<Self>) -> Self {
        let events = cx.spawn(async move |this, cx| {
            while let Some(event) = rx.next().await {
                // Handle everything that piled up, then redraw once.
                let mut batch = vec![event];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                let alive = this
                    .update(cx, |view, cx| {
                        for event in batch {
                            view.handle_event(event, cx);
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        let shell_name = std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit('/').next().map(str::to_string))
            .unwrap_or_else(|| "Terminal".into());
        Self {
            term,
            pty: Pty::Starting(Vec::new()),
            title: shell_name.into(),
            custom_title: None,
            process: None,
            focus_handle: cx.focus_handle(),
            size: None,
            origin: Point::default(),
            cell: (px(8.), px(16.)),
            selecting: false,
            scroll_rest: 0.,
            run: None,
            exit_code: None,
            cwd,
            link: None,
            last_mouse: None,
            search: None,
            marked: None,
            _events: events,
        }
    }

    pub fn title(&self) -> SharedString {
        self.custom_title.clone().unwrap_or_else(|| self.title.clone())
    }

    /// The name the user gave the tab, if any.
    pub fn custom_title(&self) -> Option<&SharedString> {
        self.custom_title.as_ref()
    }

    /// Renames the tab; None goes back to the shell's own title.
    pub fn set_custom_title(&mut self, title: Option<String>, cx: &mut Context<Self>) {
        self.custom_title = title.filter(|t| !t.trim().is_empty()).map(Into::into);
        cx.emit(TerminalEvent::TitleChanged);
        cx.notify();
    }

    /// What closing the tab would end: a Run tab's program until it exits,
    /// else the command the shell is running (the pty's foreground process
    /// group isn't the shell's own), by name. None at the shell's prompt.
    pub fn running_process(&self) -> Option<String> {
        if let Some((target, state)) = &self.run {
            return (*state == RunState::Running).then(|| target.name.clone());
        }
        self.process.as_ref()?.foreground_command()
    }

    /// The folder the shell is in now (after any `cd`), else where it started.
    pub fn current_dir(&self) -> PathBuf {
        self.process.as_ref().and_then(ShellProcess::shell).and_then(process_cwd).unwrap_or_else(|| self.cwd.clone())
    }

    fn handle_event(&mut self, event: TermEvent, cx: &mut Context<Self>) {
        match event {
            // A Run tab keeps its configuration's name.
            TermEvent::Title(title) if self.run.is_none() => {
                self.title = short_title(&title).into();
                cx.emit(TerminalEvent::TitleChanged);
            }
            TermEvent::PtyWrite(text) => self.write(text.into_bytes()),
            TermEvent::ClipboardStore(_, text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            TermEvent::ClipboardLoad(_, format) => {
                let text = cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default();
                self.write(format(&text).into_bytes());
            }
            TermEvent::ColorRequest(index, format) => {
                let theme = Theme::get(cx);
                let color = palette(theme, index).unwrap_or(Rgb { r: 0, g: 0, b: 0 });
                self.write(format(color).into_bytes());
            }
            TermEvent::TextAreaSizeRequest(format) => {
                if let Some(size) = self.size {
                    self.write(format(window_size(size)).into_bytes());
                }
            }
            // Output may still be draining (a Run tab's): the end comes with `Exit`.
            TermEvent::ChildExit(status) => self.exit_code = status.code(),
            TermEvent::Exit => {
                if let Some((_, state @ RunState::Running)) = self.run.as_mut() {
                    *state = RunState::Exited(self.exit_code);
                }
                cx.emit(TerminalEvent::Exited);
            }
            TermEvent::Wakeup => {
                if let Some(search) = self.search.as_mut() {
                    search.stale = true;
                    // While output streams, at most one search at a time.
                    if search.task.is_none() {
                        self.schedule_search(Duration::from_millis(100), false, cx);
                    }
                }
            }
            _ => {}
        }
    }

    fn write(&mut self, bytes: impl Into<Cow<'static, [u8]>>) {
        match &mut self.pty {
            Pty::Running(notifier) => notifier.notify(bytes),
            Pty::Starting(typed) => typed.extend_from_slice(&bytes.into()),
            Pty::Failed => {}
        }
    }

    /// Sends typed input: back to the live screen, without a selection.
    fn input(&mut self, bytes: impl Into<Cow<'static, [u8]>>, cx: &mut Context<Self>) {
        {
            let mut term = self.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
        }
        self.write(bytes);
        cx.notify();
    }

    fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Keys typed into Find aren't for the shell, nor keys an input
        // method left while it composes (they'd land before its text).
        if self.marked.is_some() || self.search.as_ref().is_some_and(|s| s.input.focus_handle(cx).is_focused(window)) {
            return;
        }
        if let Some(bytes) = escape_for(&event.keystroke, self.mode()) {
            cx.stop_propagation();
            self.input(bytes, cx);
        }
        // Plain text arrives through the input handler.
    }

    fn copy(&mut self, _: &TerminalCopy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.term.lock().selection_to_string().filter(|t| !t.is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, _: &TerminalPaste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else { return };
        // Shells expect Enter as \r; bracketed paste lets them tell pasted
        // newlines from typed ones.
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bytes = if self.mode().contains(TermMode::BRACKETED_PASTE) {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
        } else {
            text
        };
        self.input(bytes.into_bytes(), cx);
    }

    fn clear(&mut self, _: &TerminalClear, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_all(cx);
    }

    /// Clears the screen and the scrollback.
    pub fn clear_all(&mut self, cx: &mut Context<Self>) {
        // Ctrl+L redraws the prompt at the top; the scrollback goes too.
        self.term.lock().grid_mut().clear_history();
        self.input(&b"\x0c"[..], cx);
    }

    // ---- Find --------------------------------------------------------------------------

    /// Find (⌘F / Ctrl+Shift+F in the terminal): opens the query field, with
    /// the selection as the query.
    fn open_search(&mut self, _: &TerminalFind, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.term.lock().selection_to_string().filter(|t| !t.trim().is_empty() && !t.contains('\n'));
        let input = match &self.search {
            Some(search) => search.input.clone(),
            None => {
                let input = cx.new(|cx| Editor::single_line("Find", cx));
                let subscription = cx.subscribe(&input, |this: &mut Self, _, _: &EditorEvent, cx| {
                    this.schedule_search(Duration::from_millis(30), true, cx);
                });
                self.search = Some(Search {
                    input: input.clone(),
                    matches: Vec::new(),
                    history: 0,
                    current: None,
                    stale: false,
                    task: None,
                    _subscription: subscription,
                });
                input
            }
        };
        if let Some(text) = selected {
            input.update(cx, |e, cx| e.set_text(text.trim(), cx));
        }
        input.update(cx, |e, cx| e.select_all_text(cx));
        window.focus(&input.focus_handle(cx));
        cx.notify();
    }

    fn close_search(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.take().is_some() {
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    /// Finds the query in the scrollback and on screen here and now (for
    /// stepping through matches, which mustn't use old ones).
    fn find_matches(&mut self, cx: &App) {
        let Some(search) = self.search.as_mut() else { return };
        let query = search.input.read(cx).text(cx);
        search.task = None;
        search.stale = false;
        let (matches, history) = find_in(&self.term.lock(), &query);
        self.take_matches(matches, history);
    }

    /// Searches in the background after `delay`, scanning the whole
    /// scrollback with the terminal locked there, not while drawing. A new
    /// query (`jump`) shows the newest match.
    fn schedule_search(&mut self, delay: Duration, jump: bool, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else { return };
        let input = search.input.clone();
        let term = self.term.clone();
        search.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let Ok(query) = this.update(cx, |this, cx| {
                // Output from here on is caught by the next search.
                if let Some(search) = this.search.as_mut() {
                    search.stale = false;
                }
                input.read(cx).text(cx)
            }) else {
                return;
            };
            let (matches, history) = cx.background_executor().spawn(async move { find_in(&term.lock(), &query) }).await;
            this.update(cx, |this, cx| {
                let Some(search) = this.search.as_mut() else { return };
                // This task is running; let it finish instead of dropping it.
                if let Some(task) = search.task.take() {
                    task.detach();
                }
                let again = search.stale;
                this.take_matches(matches, history);
                if jump && let Some(search) = this.search.as_mut() {
                    // The newest output first: the bottom match.
                    search.current = search.matches.len().checked_sub(1);
                    this.reveal_match();
                }
                if again {
                    this.schedule_search(Duration::from_millis(100), false, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// New matches, keeping the current one where it was: lines move up as
    /// output scrolls, but their place from the top of the history doesn't.
    fn take_matches(&mut self, matches: Vec<Match>, history: usize) {
        let Some(search) = self.search.as_mut() else { return };
        let from_top = |m: &Match, history: usize| (m.start().line.0 + history as i32, m.start().column.0);
        let current = search.current.and_then(|i| search.matches.get(i)).map(|m| from_top(m, search.history));
        search.matches = matches;
        search.history = history;
        search.current = current.and_then(|at| search.matches.iter().position(|m| from_top(m, history) == at));
    }

    /// ↵ / ↑ goes to older output, ⇧↵ / ↓ to newer, wrapping around.
    fn step_match(&mut self, older: bool, cx: &mut Context<Self>) {
        if self.search.as_ref().is_some_and(|s| s.stale) {
            self.find_matches(cx);
        }
        let Some(search) = self.search.as_mut() else { return };
        let len = search.matches.len();
        if len == 0 {
            return;
        }
        search.current = Some(match (search.current, older) {
            (None, _) => len - 1,
            (Some(i), true) => (i + len - 1) % len,
            (Some(i), false) => (i + 1) % len,
        });
        self.reveal_match();
        cx.notify();
    }

    /// Scrolls the current match into view, centered when it was off screen.
    fn reveal_match(&mut self) {
        let Some(search) = self.search.as_ref() else { return };
        let Some(m) = search.current.and_then(|i| search.matches.get(i)) else { return };
        let mut term = self.term.lock();
        let rows = term.screen_lines() as i32;
        let offset = term.grid().display_offset() as i32;
        let line = m.start().line.0;
        if !(-offset..rows - offset).contains(&line) {
            let target = (rows / 2 - line).clamp(0, term.grid().history_size() as i32);
            term.scroll_display(Scroll::Delta(target - offset));
        }
    }

    fn on_find_older(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.step_match(true, cx);
    }

    fn on_find_newer(&mut self, _: &NewlineBelow, _: &mut Window, cx: &mut Context<Self>) {
        self.step_match(false, cx);
    }

    fn on_find_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step_match(true, cx);
    }

    fn on_find_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step_match(false, cx);
    }

    fn render_search(&self, search: &Search, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let count = match (search.current, search.matches.len()) {
            (_, 0) if search.input.read(cx).text(cx).is_empty() => String::new(),
            (_, 0) => "No results".into(),
            (Some(i), n) => format!("{}/{n}{}", i + 1, if n == MAX_MATCHES { "+" } else { "" }),
            (None, n) => n.to_string(),
        };
        let button = |id: &'static str, icon: Icon, tip: &'static str, cx: &mut Context<Self>, f: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            div()
                .id(id)
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .cursor_pointer()
                .hover_bg(theme.hover)
                .tooltip(move |_, cx| tooltip_view(tip, cx))
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, window, cx| f(this, window, cx)))
                .child(icon_el(icon, px(13.), theme.text_muted.into()))
        };
        div()
            .absolute()
            .top_1()
            .right_4()
            .flex()
            .items_center()
            .gap_1()
            .p_1()
            .rounded(px(12.))
            .bg(theme.elevated)
            .border_1()
            .border_color(gpui::Hsla::from(theme.border).alpha(0.85))
            .shadow(crate::ui::soft_shadow(&theme, 2))
            // Clicks here aren't selections in the output.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().w(px(220.)).child(input_box(&search.input, Some(Icon::Search), window, cx)))
            .child(div().min_w(px(64.)).px_1().text_size(px(12.)).text_color(theme.text_dim).child(count))
            .child(button("terminal-find-older", Icon::ChevronUp, "Previous Match (↵)", cx, |this, _, cx| this.step_match(true, cx)))
            .child(button("terminal-find-newer", Icon::ChevronDown, "Next Match (⇧↵)", cx, |this, _, cx| this.step_match(false, cx)))
            .child(button("terminal-find-close", Icon::X, "Close (Esc)", cx, |this, window, cx| this.close_search(&Cancel, window, cx)))
    }

    // ---- Mouse -------------------------------------------------------------------------

    /// The grid cell under a window position, and which half of it.
    fn grid_point(&self, position: Point<Pixels>) -> (GridPoint, Side) {
        let term = self.term.lock();
        let (w, h) = self.cell;
        let x = (position.x - self.origin.x).max(px(0.));
        let y = (position.y - self.origin.y).max(px(0.));
        let col = ((x / w) as usize).min(term.columns().saturating_sub(1));
        let row = ((y / h) as usize).min(term.screen_lines().saturating_sub(1));
        let side = if (x / w).fract() < 0.5 { Side::Left } else { Side::Right };
        let line = Line(row as i32 - term.grid().display_offset() as i32);
        (GridPoint::new(line, Column(col)), side)
    }

    /// Whether a program asked for mouse events (vim, htop…); Shift overrides.
    fn mouse_reporting(&self, modifiers: &Modifiers) -> bool {
        self.mode().intersects(TermMode::MOUSE_MODE) && !modifiers.shift
    }

    fn report_mouse(&mut self, button: u8, position: Point<Pixels>, pressed: bool, modifiers: &Modifiers) {
        let (point, _) = self.grid_point(position);
        let row = point.line.0 + self.term.lock().grid().display_offset() as i32;
        let mut code = button;
        if modifiers.shift {
            code += 4;
        }
        if modifiers.alt {
            code += 8;
        }
        if modifiers.control {
            code += 16;
        }
        let action = if pressed { 'M' } else { 'm' };
        self.write(format!("\x1b[<{code};{};{}{action}", point.column.0 + 1, row + 1).into_bytes());
    }

    /// The URL or existing file at `position` (in screen coordinates).
    fn link_at(&self, position: Point<Pixels>) -> Option<(usize, Range<usize>, Link)> {
        let (point, _) = self.grid_point(position);
        let term = self.term.lock();
        let screen_row = (point.line.0 + term.grid().display_offset() as i32).max(0) as usize;
        let row = &term.grid()[point.line];
        let chars: Vec<char> = (0..term.columns()).map(|c| row[Column(c)].c).collect();
        drop(term);
        let (range, text) = word_at(&chars, point.column.0)?;
        Some((screen_row, range, parse_link(&text, &self.cwd)?))
    }

    fn update_link(&mut self, position: Point<Pixels>, modifiers: &Modifiers, cx: &mut Context<Self>) {
        let link = if modifiers.secondary() { self.link_at(position) } else { None };
        if link != self.link {
            self.link = link;
            cx.notify();
        }
    }

    fn on_modifiers_changed(&mut self, event: &ModifiersChangedEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(position) = self.last_mouse {
            self.update_link(position, &event.modifiers, cx);
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        // ⌘/Ctrl-click opens a link, even in programs that take the mouse.
        if event.modifiers.secondary()
            && let Some((_, _, link)) = self.link_at(event.position)
        {
            // Focus moves away, so this view won't see ⌘ go up: drop the underline now.
            self.link = None;
            cx.notify();
            match link {
                Link::Url(url) => cx.open_url(&url),
                Link::File { path, line, column } => cx.emit(TerminalEvent::OpenFile { path, line, column }),
            }
            return;
        }
        if self.mouse_reporting(&event.modifiers) {
            self.report_mouse(0, event.position, true, &event.modifiers);
            return;
        }
        let (point, side) = self.grid_point(event.position);
        let ty = match event.click_count {
            2 => SelectionType::Semantic,
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        self.term.lock().selection = Some(Selection::new(ty, point, side));
        self.selecting = true;
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.last_mouse = Some(event.position);
        if event.pressed_button.is_none() {
            self.update_link(event.position, &event.modifiers, cx);
        }
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let (point, side) = self.grid_point(event.position);
        if let Some(selection) = self.term.lock().selection.as_mut() {
            selection.update(point, side);
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.mouse_reporting(&event.modifiers) {
            self.report_mouse(0, event.position, false, &event.modifiers);
            return;
        }
        self.selecting = false;
        // A click without a drag leaves no selection.
        let mut term = self.term.lock();
        if term.selection.as_ref().is_some_and(|s| s.ty == SelectionType::Simple && term.selection_to_string().is_none_or(|t| t.is_empty())) {
            term.selection = None;
        }
        cx.notify();
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(self.cell.1);
        self.scroll_rest += f32::from(delta.y) / f32::from(self.cell.1);
        let lines = self.scroll_rest.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_rest -= lines as f32;
        let mode = self.mode();
        if self.mouse_reporting(&event.modifiers) {
            let button = if lines > 0 { 64 } else { 65 };
            for _ in 0..lines.unsigned_abs().min(10) {
                self.report_mouse(button, event.position, true, &event.modifiers);
            }
        } else if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            // Full-screen programs without mouse support (less, man) scroll with arrows.
            let arrow: &[u8] = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            self.write(arrow.repeat(lines.unsigned_abs() as usize));
        } else {
            self.term.lock().scroll_display(Scroll::Delta(lines));
            cx.notify();
        }
    }

    // ---- Layout ------------------------------------------------------------------------

    fn layout(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) -> TerminalFrame {
        let theme = Theme::get(cx).clone();
        let (mono, font_size) = terminal_font(&theme);
        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&mono);
        let cell_w = text_system.advance(font_id, font_size, 'm').map(|s| s.width).unwrap_or(font_size * 0.6);
        let cell_h = (font_size * 1.35).round();
        let padding = px(8.);
        let inner = Bounds::from_corners(bounds.origin + point(padding, px(4.)), bounds.bottom_right() - point(padding, px(4.)));
        let cols = ((inner.size.width / cell_w).floor() as u16).max(2);
        let rows = ((inner.size.height / cell_h).floor() as u16).max(1);
        if self.size != Some((cols, rows, cell_w, cell_h)) {
            self.size = Some((cols, rows, cell_w, cell_h));
            self.term.lock().resize(TermSize::new(cols as usize, rows as usize));
            // A pty still starting gets the size when it's up.
            if let Pty::Running(notifier) = &mut self.pty {
                notifier.on_resize(window_size((cols, rows, cell_w, cell_h)));
            }
        }
        self.origin = inner.origin;
        self.cell = (cell_w, cell_h);
        let focused = self.focus_handle.is_focused(window) && window.is_window_active();
        // The cells on screen, gathered while the terminal is locked; colors
        // and shaping wait until it isn't, so the shell's output doesn't.
        let (cells, cursor, cursor_wide, colors, offset) = {
            let term = self.term.lock();
            let content = term.renderable_content();
            let cursor = content.cursor;
            // While an input method composes, its text is where the cursor shows.
            let block_cursor = focused && self.marked.is_none() && cursor.shape == CursorShape::Block;
            let offset = content.display_offset as i32;
            let mut cells = FrameCells::default();
            for indexed in content.display_iter {
                let row = indexed.point.line.0 + offset;
                if (0..rows as i32).contains(&row) {
                    let selected = content.selection.is_some_and(|s| s.contains(indexed.point));
                    cells.push(row as usize, indexed.point.column.0, indexed.cell, selected, block_cursor && indexed.point == cursor.point);
                }
            }
            let wide = term.grid()[cursor.point].flags.contains(Flags::WIDE_CHAR);
            (cells.finish(), cursor, wide, *content.colors, offset)
        };
        let default_bg: Hsla = theme.background.into();
        let x_of = |col: usize| inner.origin.x + cell_w * col as f32;
        let y_of = |row: usize| inner.origin.y + cell_h * row as f32;
        let mut frame = TerminalFrame { bounds, line_height: cell_h, ..Default::default() };
        // Find's matches on screen, a row at a time (a match can wrap).
        if let Some(search) = &self.search {
            for (ix, m) in search.matches.iter().enumerate() {
                let (start, end) = (m.start(), m.end());
                if end.line.0 + offset < 0 || start.line.0 + offset >= rows as i32 {
                    continue;
                }
                let color = if search.current == Some(ix) { theme.search_active } else { theme.search_match };
                for line in start.line.0..=end.line.0 {
                    let row = line + offset;
                    if !(0..rows as i32).contains(&row) {
                        continue;
                    }
                    let from = if line == start.line.0 { start.column.0 } else { 0 };
                    let to = if line == end.line.0 { end.column.0 + 1 } else { cols as usize };
                    frame.highlights.push(fill(Bounds::new(point(x_of(from), y_of(row as usize)), size(cell_w * (to - from) as f32, cell_h)), color));
                }
            }
        }
        let mut palette = Palette::new(&theme, colors);
        for background in &cells.backgrounds {
            let color = palette.get(background.ink);
            if color != default_bg {
                let origin = point(x_of(background.cols.start), y_of(background.row));
                frame.backgrounds.push(fill(Bounds::new(origin, size(cell_w * background.cols.len() as f32, cell_h)), color));
            }
        }

        // The cursor, under the text.
        let cursor_row = cursor.point.line.0 + offset;
        let cursor_shown = (0..rows as i32).contains(&cursor_row);
        let cursor_at = point(x_of(cursor.point.column.0), y_of(cursor_row.max(0) as usize));
        if cursor.shape != CursorShape::Hidden && self.marked.is_none() && cursor_shown {
            let color: Hsla = theme.cursor.into();
            let width = if cursor_wide { cell_w * 2. } else { cell_w };
            let quad = match (focused, cursor.shape) {
                (true, CursorShape::Beam) => fill(Bounds::new(cursor_at, size(px(2.), cell_h)), color),
                (true, CursorShape::Underline) => fill(Bounds::new(cursor_at + point(px(0.), cell_h - px(2.)), size(width, px(2.))), color),
                (true, _) => fill(Bounds::new(cursor_at, size(width, cell_h)), color),
                (false, _) => gpui::outline(Bounds::new(cursor_at, size(width, cell_h)), color, gpui::BorderStyle::Solid),
            };
            frame.cursor = Some(quad);
        }

        for piece in cells.pieces {
            let runs: Vec<TextRun> = piece.runs.iter().map(|&(len, style)| text_run(len, style, &mono, &mut palette)).collect();
            let origin = point(x_of(piece.col), y_of(piece.row));
            let line = text_system.shape_line(piece.text.into(), font_size, &runs, piece.grid.then_some(cell_w));
            if !piece.grid || glyph_per_char(&line) {
                frame.lines.push((origin, line));
                continue;
            }
            // A ligature, or a character the font split or reordered: the
            // fixed width would put glyphs in the wrong cells, so each cell
            // is shaped alone.
            let mut chars = line.text.chars();
            let mut x = origin.x;
            for run in &runs {
                let mut left = run.len;
                while left > 0
                    && let Some(c) = chars.next()
                {
                    left = left.saturating_sub(c.len_utf8());
                    if c != ' ' || run.underline.is_some() || run.strikethrough.is_some() {
                        let run = TextRun { len: c.len_utf8(), ..run.clone() };
                        frame.lines.push((point(x, origin.y), text_system.shape_line(c.to_string().into(), font_size, &[run], None)));
                    }
                    x += cell_w;
                }
            }
        }

        // What an input method is composing, at the cursor over what's there.
        if let Some(marked) = &self.marked
            && cursor_shown
        {
            let text_color: Hsla = theme.text.into();
            let line = text_system.shape_line(marked.text.clone().into(), font_size, &marked_runs(marked, &mono, text_color), None);
            // Its caret, unless it selected a part (a clause to convert).
            let caret = match &marked.selected {
                Some(selected) if !selected.is_empty() => None,
                Some(selected) => Some(utf16_to_byte(&marked.text, selected.start)),
                None => Some(marked.text.len()),
            };
            let caret = caret
                .filter(|_| focused)
                .map(|at| fill(Bounds::new(cursor_at + point(line.x_for_index(at), px(0.)), size(px(2.), cell_h)), theme.cursor));
            frame.marked = Some(Composition {
                background: fill(Bounds::new(cursor_at, size(line.width.max(cell_w), cell_h)), default_bg),
                origin: cursor_at,
                line,
                caret,
            });
        }
        if let Some((row, cols, _)) = &self.link {
            let y = y_of(*row + 1) - px(1.);
            frame.link = Some(fill(Bounds::new(point(x_of(cols.start), y), size(cell_w * cols.len() as f32, px(1.))), theme.accent));
        }
        frame
    }

    /// Where the cursor's cell is in the window, as of the last frame.
    fn cursor_origin(&self) -> Point<Pixels> {
        let term = self.term.lock();
        let cursor = term.grid().cursor.point;
        let row = cursor.line.0 + term.grid().display_offset() as i32;
        let (w, h) = self.cell;
        self.origin + point(w * cursor.column.0 as f32, h * row as f32)
    }
}

/// The run of non-space characters around `col`, without the brackets,
/// quotes and punctuation that usually surround a path or URL in output.
fn word_at(chars: &[char], col: usize) -> Option<(Range<usize>, String)> {
    let stop = |c: char| c.is_whitespace() || "\"'`<>()[]{},|".contains(c);
    if chars.get(col).is_none_or(|c| stop(*c)) {
        return None;
    }
    let start = chars[..col].iter().rposition(|c| stop(*c)).map_or(0, |i| i + 1);
    let mut end = chars[col..].iter().position(|c| stop(*c)).map_or(chars.len(), |i| col + i);
    // A sentence's full stop or a trailing colon isn't part of it.
    while end > start + 1 && ".:;!?".contains(chars[end - 1]) {
        end -= 1;
    }
    (col < end).then(|| (start..end, chars[start..end].iter().collect()))
}

/// The query's matches in the scrollback and on screen (case-insensitive
/// unless it has capitals), top to bottom, the newest `MAX_MATCHES`; and
/// how many scrollback lines there were.
fn find_in(term: &Term<Listener>, query: &str) -> (Vec<Match>, usize) {
    let history = term.grid().history_size();
    let mut matches = Vec::new();
    if !query.is_empty()
        && let Ok(mut regex) = RegexSearch::new(&regex::escape(query))
    {
        // From the bottom up, so a capped list keeps the newest.
        let bottom = GridPoint::new(term.bottommost_line(), term.last_column());
        let top = GridPoint::new(term.topmost_line(), Column(0));
        matches.extend(RegexIter::new(bottom, top, Direction::Left, term, &mut regex).take(MAX_MATCHES));
        matches.reverse();
    }
    (matches, history)
}

/// A process's working directory.
#[cfg(target_os = "macos")]
fn process_cwd(pid: u32) -> Option<PathBuf> {
    use std::{ffi::CStr, os::unix::ffi::OsStrExt};
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: `info` is a buffer of `size` bytes for PROC_PIDVNODEPATHINFO.
    let read = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDVNODEPATHINFO, 0, (&raw mut info).cast(), size) };
    if read != size {
        return None;
    }
    // SAFETY: the kernel fills `vip_path` with a NUL-terminated path.
    let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes()));
    path.is_dir().then_some(path)
}

#[cfg(target_os = "linux")]
fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok().filter(|p| p.is_dir())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

/// The process group in the foreground of a pty, from its master end: the
/// shell's own at its prompt, a command's while it runs.
#[cfg(unix)]
fn foreground_group(master: &std::fs::File) -> Option<u32> {
    use std::os::fd::AsRawFd;
    // SAFETY: a file descriptor `master` keeps open.
    let group = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
    // Negative: an error (the shell is gone); 0: no foreground group yet.
    u32::try_from(group).ok().filter(|&g| g > 0)
}

#[cfg(not(unix))]
fn foreground_group(_master: &std::fs::File) -> Option<u32> {
    None
}

/// A process's name (a group's leader names the command running).
#[cfg(target_os = "macos")]
fn process_name(pid: u32) -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` has room for the `buf.len()` bytes `proc_name` may write.
    let len = unsafe { libc::proc_name(pid as libc::c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let name = String::from_utf8_lossy(buf.get(..usize::try_from(len).ok()?)?).into_owned();
    (!name.is_empty()).then_some(name)
}

#[cfg(target_os = "linux")]
fn process_name(pid: u32) -> Option<String> {
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(name.trim_end().to_string()).filter(|n| !n.is_empty())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_name(_pid: u32) -> Option<String> {
    None
}

/// A child of a process (`login` has one: the shell it runs).
#[cfg(target_os = "macos")]
fn first_child(pid: u32) -> Option<u32> {
    let mut children = [0 as libc::pid_t; 8];
    let size = std::mem::size_of_val(&children) as libc::c_int;
    // SAFETY: `children` has room for the `size` bytes it may write.
    unsafe { libc::proc_listchildpids(pid as libc::pid_t, children.as_mut_ptr().cast(), size) };
    children.iter().find(|&&child| child > 0).map(|&child| child as u32)
}

#[cfg(not(target_os = "macos"))]
fn first_child(_pid: u32) -> Option<u32> {
    None
}

/// A shell's title with a program's path in it shortened to the program's
/// name. The Windows console titles itself with its program's path:
/// `C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe`, and
/// `C:\WINDOWS\system32\cmd.exe - npm test` while cmd runs a command.
fn short_title(title: &str) -> String {
    let Some(end) = title.to_ascii_lowercase().find(".exe").map(|i| i + 4) else { return title.into() };
    // `.exe` ends the name (not `.exec`, `.exe.txt`).
    if !title[end..].chars().next().is_none_or(char::is_whitespace) {
        return title.into();
    }
    // The path starts at a drive (`C:\`), which may come after other text.
    let bytes = title.as_bytes();
    let drive = (0..end - 2).rev().find(|&i| {
        bytes[i].is_ascii_alphabetic()
            && bytes[i + 1] == b':'
            && matches!(bytes[i + 2], b'\\' | b'/')
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
    });
    let Some(start) = drive else { return title.into() };
    let name = title[start..end - 4].rsplit(['\\', '/']).next().unwrap_or_default();
    format!("{}{name}{}", &title[..start], &title[end..])
}

/// A URL, or a file that exists (relative to `cwd`) with an optional
/// `:line` and `:column`, as compilers and test runners print them.
fn parse_link(text: &str, cwd: &Path) -> Option<Link> {
    if text.starts_with("https://") || text.starts_with("http://") {
        return Some(Link::Url(text.to_string()));
    }
    let mut parts = text.splitn(3, ':');
    let path = parts.next()?;
    let line = parts.next().and_then(|n| n.parse().ok());
    let column = parts.next().and_then(|n| n.parse().ok());
    let path = match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(PathBuf::from)?.join(rest),
        None => cwd.join(path),
    };
    path.is_file().then_some(Link::File { path, line, column })
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        // Closes the pty; the shell gets SIGHUP. (One still starting is
        // ended once it's up, in `create`.)
        if let Pty::Running(notifier) = &self.pty {
            let _ = notifier.0.send(Msg::Shutdown);
        }
    }
}

/// A terminal's output, to read on another thread (`TerminalView::output`).
pub struct Output(Arc<FairMutex<Term<Listener>>>);

impl Output {
    /// Everything on screen and in the scrollback, as text (wrapped lines
    /// joined, combining characters kept).
    pub fn text(&self) -> String {
        let term = self.0.lock();
        let columns = term.columns();
        let mut out = String::new();
        for line in term.topmost_line().0..=term.bottommost_line().0 {
            let row = &term.grid()[Line(line)];
            let text: String = (0..columns)
                .map(|c| &row[Column(c)])
                .filter(|cell| !cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER))
                .flat_map(|cell| std::iter::once(cell.c).chain(cell.zerowidth().unwrap_or_default().iter().copied()))
                .collect();
            if row[Column(columns - 1)].flags.contains(Flags::WRAPLINE) {
                out.push_str(&text);
            } else {
                out.push_str(text.trim_end());
                out.push('\n');
            }
        }
        out
    }
}

/// What a terminal frame paints.
#[derive(Default)]
pub struct TerminalFrame {
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    backgrounds: Vec<PaintQuad>,
    /// Find's matches, over the cell backgrounds and under the text.
    highlights: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    /// What an input method is composing, over everything else.
    marked: Option<Composition>,
    /// Under the link the pointer is on (⌘/Ctrl held).
    link: Option<PaintQuad>,
    hitbox: Option<Hitbox>,
}

/// Composed text as drawn: on the background color, so the cells it
/// covers don't show through, with its caret.
struct Composition {
    background: PaintQuad,
    origin: Point<Pixels>,
    line: ShapedLine,
    caret: Option<PaintQuad>,
}

/// Starts a terminal's shell, off the UI thread (`TerminalView::create`):
/// the command, the venv it activates, the pty and the thread reading it.
fn start_pty(
    configured: Option<Vec<String>>,
    cwd: &Path,
    run: Option<&RunTarget>,
    term: Arc<FairMutex<Term<Listener>>>,
    listener: Listener,
) -> std::io::Result<Started> {
    let venv = crate::python::venv(cwd);
    let shell = match run {
        // PowerShell (the user's when that's PowerShell 7) with their
        // profile, as a login shell would read it. The script goes
        // encoded: Windows hands a program one command line, unquoted.
        Some(target) if cfg!(windows) => {
            let program = configured.as_ref().and_then(|c| c.first()).filter(|p| is_powershell(p)).cloned();
            let script = powershell_run_script(&target.command, cwd, venv.as_deref());
            Some(vec![program.unwrap_or_else(|| "powershell".into()), "-NoLogo".into(), "-EncodedCommand".into(), encoded_command(&script)])
        }
        Some(target) => {
            // A login, interactive shell so PATH matches the user's terminal.
            let program = configured
                .as_ref()
                .and_then(|c| c.first().cloned())
                .or_else(|| std::env::var("SHELL").ok())
                .unwrap_or_else(|| "/bin/sh".into());
            // After the profile, which may reorder PATH.
            let setup = venv.as_deref().map(crate::python::exports).unwrap_or_default();
            Some(vec![program, "-l".into(), "-i".into(), "-c".into(), setup + &run_script(&target.command)])
        }
        None => configured.clone(),
    };
    // A new shell activates the project's venv, typed in like a user would:
    // a login profile can reorder PATH set beforehand.
    // The activate script is the project's own shell script.
    let activate = match (run, &venv) {
        (None, Some(venv)) if crate::trust::is_trusted(cwd) => {
            let program = configured.as_ref().and_then(|c| c.first().cloned()).or_else(|| std::env::var("SHELL").ok());
            let program = program.unwrap_or_else(|| if cfg!(windows) { "powershell".into() } else { "sh".into() });
            crate::python::activate_command(venv, &program)
        }
        _ => None,
    };
    let mut env = HashMap::new();
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("COLORTERM".to_string(), "truecolor".to_string());
    env.insert("TERM_PROGRAM".to_string(), "Kuana".to_string());
    let shell = shell.and_then(|command| {
        let (program, args) = command.split_first()?;
        Some(tty::Shell::new(program.clone(), args.to_vec()))
    });
    // Without a shell, alacritty starts the user's through `login` on macOS.
    let login = cfg!(target_os = "macos") && shell.is_none();
    // A Run tab must show everything the program printed before it exited.
    let drain_on_exit = run.is_some();
    // Windows has more options.
    #[allow(clippy::needless_update)]
    let options = tty::Options { shell, working_directory: Some(cwd.to_path_buf()), drain_on_exit, env, ..Default::default() };
    let window_size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
    let pty = {
        #[cfg(unix)]
        let _unblocked = SignalsUnblocked::new();
        tty::new(&options, window_size, 0)?
    };
    #[cfg(unix)]
    let process = Some(ShellProcess {
        pid: pty.child().id(),
        login,
        shell: std::cell::OnceCell::new(),
        master: pty.file().try_clone().ok(),
    });
    #[cfg(not(unix))]
    let process = None;
    let event_loop = EventLoop::new(term, listener, pty, false, false)?;
    let notifier = Notifier(event_loop.channel());
    event_loop.spawn();
    if let Some(line) = activate {
        // The shell reads it once it's ready.
        notifier.notify((line + "\r").into_bytes());
    }
    Ok(Started { notifier, process })
}

/// No signals blocked on this thread while it lives. A process inherits its
/// starter's signal mask, and the background executor's threads (GCD's on
/// macOS) block signals: a shell started there would never get ⌃C, SIGHUP
/// or SIGTERM, nor would anything it runs.
#[cfg(unix)]
struct SignalsUnblocked(libc::sigset_t);

#[cfg(unix)]
impl SignalsUnblocked {
    fn new() -> Self {
        // SAFETY: this thread's own mask, set from an empty set; the old one is kept to restore.
        unsafe {
            let (mut none, mut old) = (std::mem::zeroed(), std::mem::zeroed());
            libc::sigemptyset(&mut none);
            libc::pthread_sigmask(libc::SIG_SETMASK, &none, &mut old);
            Self(old)
        }
    }
}

#[cfg(unix)]
impl Drop for SignalsUnblocked {
    fn drop(&mut self) {
        // SAFETY: puts back the mask `new` found.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &self.0, std::ptr::null_mut()) };
    }
}

/// Columns, rows and cell size (`TerminalView::size`), as the pty takes them.
fn window_size((cols, rows, w, h): (u16, u16, Pixels, Pixels)) -> WindowSize {
    WindowSize { num_lines: rows, num_cols: cols, cell_width: f32::from(w) as u16, cell_height: f32::from(h) as u16 }
}

/// The shell script for a Run tab: shows the command, runs it, then says
/// how it ended (the tab stays open). The no-op INT trap lets the wrapper
/// outlive a Stop; the command runs in a subshell, which resets the trap so
/// ⌃C still stops it. (`status` is read-only in zsh.)
fn run_script(command: &str) -> String {
    let shown = command.replace('\'', r"'\''");
    format!(
        "trap : INT\nprintf '\\033[2m$ %s\\033[0m\\n' '{shown}'\n(\n{command}\n)\nkuana_code=$?\nprintf '\\n\\033[2mProcess finished with exit code %s\\033[0m\\n' \"$kuana_code\"\nexit $kuana_code\n"
    )
}

/// The same in PowerShell (Run tabs on Windows), back in `cwd` first (a
/// profile may change folders) and with the venv's PATH. The last line is
/// written in `finally`, so a Stop still shows it; `$?` is false for a
/// command that failed without an exit code (one that isn't found).
fn powershell_run_script(command: &str, cwd: &Path, venv: Option<&Path>) -> String {
    use crate::run::powershell_string as quote;
    // Quoted, a program would just be a string.
    let call = if command.starts_with(['\'', '"']) { "& " } else { "" };
    format!(
        "Set-Location -LiteralPath {cwd}\n{setup}Write-Host {shown} -ForegroundColor DarkGray\n\
         try {{\n$global:LASTEXITCODE = 0\n{call}{command}\n$kuanaOk = $?\n}} finally {{\n\
         $kuanaCode = if ($kuanaOk) {{ 0 }} elseif ($global:LASTEXITCODE) {{ $global:LASTEXITCODE }} else {{ 1 }}\n\
         Write-Host ''\nWrite-Host \"Process finished with exit code $kuanaCode\" -ForegroundColor DarkGray\n}}\nexit $kuanaCode\n",
        cwd = quote(&cwd.to_string_lossy()),
        setup = venv.map(crate::python::powershell_exports).unwrap_or_default(),
        shown = quote(&format!("$ {command}")),
    )
}

/// A script for PowerShell's `-EncodedCommand`: base64 of its UTF-16LE.
fn encoded_command(script: &str) -> String {
    use base64::Engine;
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn is_powershell(program: &str) -> bool {
    let name = program.rsplit(['\\', '/']).next().unwrap_or_default().to_ascii_lowercase();
    matches!(name.strip_suffix(".exe").unwrap_or(&name), "pwsh" | "powershell")
}

// ---- Cells to draw -----------------------------------------------------------------

/// Underlines and strikethrough, which draw on blank cells too.
const DECORATIONS: Flags = Flags::ALL_UNDERLINES.union(Flags::STRIKEOUT);
/// The flags that change how a cell's text draws.
const STYLE_FLAGS: Flags = DECORATIONS.union(Flags::BOLD).union(Flags::ITALIC).union(Flags::DIM);
const ZWJ: char = '\u{200D}';

/// A cell's color before the theme resolves it, which `Palette` does after
/// the terminal is unlocked, once per color and frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ink {
    /// A palette index, bold's bright and dim's plain colors already picked.
    Index(u16),
    Rgb(Rgb),
    Selection,
    /// The default background: text under a block cursor.
    Background,
}

impl Ink {
    fn of(color: Color, bold: bool) -> Self {
        match color {
            Color::Spec(rgb) => Ink::Rgb(rgb),
            Color::Indexed(i) => Ink::Index(i.into()),
            Color::Named(named) => {
                let n = named as u16;
                let dim = NamedColor::DimBlack as u16;
                Ink::Index(if bold && n < 8 {
                    // Bold text in a basic color uses the bright variant, like most terminals.
                    n + 8
                } else if (dim..dim + 8).contains(&n) {
                    // Dim colors aren't in the palette; DIM text is faded when painted.
                    n - dim
                } else {
                    n
                })
            }
        }
    }
}

/// How a cell's text looks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct CellStyle {
    fg: Ink,
    /// Only `STYLE_FLAGS`.
    flags: Flags,
}

/// Text to shape at a cell: a run of cells that each hold one character
/// (`grid`: shaped at once with a fixed cell width, which puts glyph n in
/// cell n whatever font it comes from), or a wide character or one with
/// zero-width characters after it (combining accents, emoji selectors and
/// joiners), shaped alone so the shaper places them.
#[derive(Debug)]
struct Piece {
    row: usize,
    col: usize,
    /// The column after its last character.
    end: usize,
    text: String,
    /// Byte lengths and styles, in order.
    runs: Vec<(usize, CellStyle)>,
    grid: bool,
    /// Blank cells after it so far: inside it if a character follows.
    blanks: usize,
}

impl Piece {
    fn new(row: usize, col: usize, grid: bool) -> Self {
        Self { row, col, end: col, text: String::new(), runs: Vec::new(), grid, blanks: 0 }
    }

    fn push_str(&mut self, text: &str, style: CellStyle) {
        self.text.push_str(text);
        match self.runs.last_mut() {
            Some((len, last)) if *last == style => *len += text.len(),
            _ => self.runs.push((text.len(), style)),
        }
    }

    /// A grid run's next character, after the blank cells before it: spaces
    /// in the run's style (but not underlined or struck through).
    fn push_char(&mut self, c: char, style: CellStyle) {
        if self.blanks > 0 {
            let last = self.runs.last().map_or(style, |&(_, last)| last);
            let plain = CellStyle { flags: last.flags.difference(DECORATIONS), ..last };
            for _ in 0..self.blanks {
                self.push_str(" ", plain);
            }
        }
        self.push_str(c.encode_utf8(&mut [0; 4]), style);
        self.end += self.blanks + 1;
        self.blanks = 0;
    }
}

/// A row's cells of one background color.
#[derive(Debug)]
struct Background {
    row: usize,
    cols: Range<usize>,
    ink: Ink,
}

/// A frame's cells, as text to shape and backgrounds to fill: gathered
/// while the terminal is locked, so this only copies and compares.
#[derive(Default, Debug)]
struct FrameCells {
    pieces: Vec<Piece>,
    backgrounds: Vec<Background>,
    /// The last piece ends with a zero-width joiner: the next character joins it.
    joining: bool,
}

impl FrameCells {
    /// The next cell on screen (row by row, left to right).
    fn push(&mut self, row: usize, col: usize, cell: &Cell, selected: bool, under_cursor: bool) {
        let flags = cell.flags;
        // A wide character's second half (it covers it), or a line's end
        // that couldn't fit one.
        if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            return;
        }
        let width = if flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
        let mut fg = Ink::of(cell.fg, flags.contains(Flags::BOLD));
        let mut bg = Ink::of(cell.bg, false);
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if selected {
            bg = Ink::Selection;
        }
        if under_cursor {
            fg = Ink::Background;
        }
        match self.backgrounds.last_mut() {
            Some(last) if last.row == row && last.cols.end == col && last.ink == bg => last.cols.end = col + width,
            _ => self.backgrounds.push(Background { row, cols: col..col + width, ink: bg }),
        }

        let zerowidth = cell.zerowidth().unwrap_or_default();
        let style = CellStyle { fg, flags: flags.intersection(STYLE_FLAGS) };
        // A tab's first cell holds the tab.
        let c = if cell.c.is_control() { ' ' } else { cell.c };
        // Nothing to draw: a space, or hidden text.
        let blank = flags.contains(Flags::HIDDEN) || (c == ' ' && zerowidth.is_empty() && !flags.intersects(DECORATIONS));
        let joining = std::mem::take(&mut self.joining);
        let last = self.pieces.last_mut().filter(|p| p.row == row && p.end + p.blanks == col);
        if blank {
            if let Some(last) = last.filter(|p| p.grid) {
                last.blanks += width;
            }
            return;
        }
        let text = || std::iter::once(c).chain(zerowidth.iter().copied()).collect::<String>();
        match last {
            // An emoji sequence (👨‍👩‍👧 is three joined) is one picture, at its first cell.
            Some(last) if joining && !last.grid => {
                last.push_str(&text(), style);
                last.end = col + width;
                self.joining = zerowidth.last() == Some(&ZWJ);
            }
            Some(last) if last.grid && width == 1 && zerowidth.is_empty() => last.push_char(c, style),
            _ if width == 1 && zerowidth.is_empty() => {
                let mut piece = Piece::new(row, col, true);
                piece.push_char(c, style);
                self.pieces.push(piece);
            }
            _ => {
                let mut piece = Piece::new(row, col, false);
                piece.push_str(&text(), style);
                piece.end = col + width;
                self.pieces.push(piece);
                self.joining = zerowidth.last() == Some(&ZWJ);
            }
        }
    }

    /// An underline or strikethrough at a run's end would run to the
    /// shaped line's own width; one more (undecorated) space ends it at the
    /// cell's edge.
    fn finish(mut self) -> Self {
        for piece in self.pieces.iter_mut().filter(|p| p.grid) {
            if let Some(&(_, style)) = piece.runs.last()
                && style.flags.intersects(DECORATIONS)
            {
                piece.push_str(" ", CellStyle { flags: style.flags.difference(DECORATIONS), ..style });
            }
        }
        self
    }
}

/// Theme colors for `Ink`s, each palette entry worked out once a frame.
struct Palette<'a> {
    theme: &'a Theme,
    /// Colors programs set (OSC 4, 10, 11…).
    overrides: Colors,
    resolved: [Option<Hsla>; COUNT],
}

impl<'a> Palette<'a> {
    fn new(theme: &'a Theme, overrides: Colors) -> Self {
        Self { theme, overrides, resolved: [None; COUNT] }
    }

    fn get(&mut self, ink: Ink) -> Hsla {
        match ink {
            Ink::Rgb(rgb) => to_hsla(rgb),
            Ink::Selection => self.theme.selection.into(),
            Ink::Background => self.theme.background.into(),
            Ink::Index(i) => {
                let i = usize::from(i).min(COUNT - 1);
                *self.resolved[i].get_or_insert_with(|| {
                    self.overrides[i].or_else(|| palette(self.theme, i)).map(to_hsla).unwrap_or(self.theme.text.into())
                })
            }
        }
    }
}

/// A style's text run.
fn text_run(len: usize, style: CellStyle, font: &Font, palette: &mut Palette) -> TextRun {
    let fg = palette.get(style.fg);
    let color = if style.flags.contains(Flags::DIM) { fg.alpha(fg.a * 0.6) } else { fg };
    let mut font = font.clone();
    if style.flags.contains(Flags::BOLD) {
        font.weight = FontWeight::BOLD;
    }
    if style.flags.contains(Flags::ITALIC) {
        font.style = FontStyle::Italic;
    }
    TextRun {
        len,
        font,
        color,
        background_color: None,
        underline: style.flags.intersects(Flags::ALL_UNDERLINES).then(|| UnderlineStyle {
            thickness: px(1.),
            color: Some(color),
            wavy: style.flags.contains(Flags::UNDERCURL),
        }),
        strikethrough: style.flags.contains(Flags::STRIKEOUT).then(|| gpui::StrikethroughStyle { thickness: px(1.), color: Some(color) }),
    }
}

/// Whether shaping gave every character one glyph, in order: what a fixed
/// cell width needs to put each glyph in its cell.
fn glyph_per_char(line: &ShapedLine) -> bool {
    line.runs.iter().flat_map(|run| &run.glyphs).map(|glyph| glyph.index).eq(line.text.char_indices().map(|(i, _)| i))
}

/// The terminal's font and size: the editor's font, a point smaller.
fn terminal_font(theme: &Theme) -> (Font, Pixels) {
    (font(theme.mono_font.clone()), px(theme.editor_font_size - 1.))
}

/// Composed text's runs: underlined, the part being converted thicker.
fn marked_runs(marked: &Marked, font: &Font, color: Hsla) -> Vec<TextRun> {
    let run = |len: usize, thickness: f32| TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline: Some(UnderlineStyle { thickness: px(thickness), color: Some(color), wavy: false }),
        strikethrough: None,
    };
    let Some(selected) = marked.selected.as_ref().filter(|s| !s.is_empty()) else { return vec![run(marked.text.len(), 1.)] };
    let (start, end) = (utf16_to_byte(&marked.text, selected.start), utf16_to_byte(&marked.text, selected.end));
    [(start, 1.), (end - start, 2.), (marked.text.len() - end, 1.)].into_iter().filter(|&(len, _)| len > 0).map(|(len, thickness)| run(len, thickness)).collect()
}

/// The byte offset of a UTF-16 offset in `text` (input methods count in
/// UTF-16), at a character boundary and at most its end.
fn utf16_to_byte(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, c) in text.char_indices() {
        if units >= utf16 {
            return i;
        }
        units += c.len_utf16();
    }
    text.len()
}

// ---- Colors ------------------------------------------------------------------------

fn to_hsla(rgb: Rgb) -> Hsla {
    gpui::Rgba { r: rgb.r as f32 / 255., g: rgb.g as f32 / 255., b: rgb.b as f32 / 255., a: 1. }.into()
}

fn to_rgb(color: gpui::Rgba) -> Rgb {
    Rgb { r: (color.r * 255.).round() as u8, g: (color.g * 255.).round() as u8, b: (color.b * 255.).round() as u8 }
}

/// Colors 0–15 from the theme, 16–255 the standard xterm cube and grays,
/// then the default foreground, background and cursor.
fn palette(theme: &Theme, index: usize) -> Option<Rgb> {
    let ansi = [
        theme.terminal_black,
        theme.terminal_red,
        theme.terminal_green,
        theme.terminal_yellow,
        theme.terminal_blue,
        theme.terminal_magenta,
        theme.terminal_cyan,
        theme.terminal_white,
        theme.terminal_bright_black,
        theme.terminal_bright_red,
        theme.terminal_bright_green,
        theme.terminal_bright_yellow,
        theme.terminal_bright_blue,
        theme.terminal_bright_magenta,
        theme.terminal_bright_cyan,
        theme.terminal_bright_white,
    ];
    Some(match index {
        0..=15 => to_rgb(ansi[index]),
        16..=231 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            Rgb { r: level(i / 36), g: level(i / 6 % 6), b: level(i % 6) }
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            Rgb { r: v, g: v, b: v }
        }
        i if i == NamedColor::Foreground as usize => to_rgb(theme.text),
        i if i == NamedColor::Background as usize => to_rgb(theme.background),
        i if i == NamedColor::Cursor as usize => to_rgb(theme.cursor),
        _ => return None,
    })
}

// ---- Keys --------------------------------------------------------------------------

/// The bytes a key sends, for keys that aren't plain text. None for text
/// (handled by the input handler) and for ⌘ shortcuts.
fn escape_for(keystroke: &gpui::Keystroke, mode: TermMode) -> Option<Cow<'static, [u8]>> {
    let m = keystroke.modifiers;
    if m.platform {
        return None;
    }
    // xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let param = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.control as u8;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let cursor_key = |letter: char| -> Cow<'static, [u8]> {
        if param > 1 {
            format!("\x1b[1;{param}{letter}").into_bytes().into()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes().into()
        } else {
            format!("\x1b[{letter}").into_bytes().into()
        }
    };
    let tilde = |code: u8| -> Cow<'static, [u8]> {
        if param > 1 { format!("\x1b[{code};{param}~").into_bytes().into() } else { format!("\x1b[{code}~").into_bytes().into() }
    };
    let meta = |bytes: &[u8]| -> Cow<'static, [u8]> {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        if m.alt {
            out.push(0x1b);
        }
        out.extend_from_slice(bytes);
        out.into()
    };
    Some(match keystroke.key.as_str() {
        "enter" => meta(b"\r"),
        "tab" if m.shift => b"\x1b[Z"[..].into(),
        "tab" => b"\t"[..].into(),
        "escape" => b"\x1b"[..].into(),
        "backspace" if m.control => meta(b"\x08"),
        "backspace" => meta(b"\x7f"),
        "space" if m.control => b"\x00"[..].into(),
        "up" => cursor_key('A'),
        "down" => cursor_key('B'),
        "right" => cursor_key('C'),
        "left" => cursor_key('D'),
        "home" => cursor_key('H'),
        "end" => cursor_key('F'),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" | "f2" | "f3" | "f4" => {
            let letter = ['P', 'Q', 'R', 'S'][keystroke.key[1..].parse::<usize>().ok()? - 1];
            if param > 1 { format!("\x1b[1;{param}{letter}").into_bytes().into() } else { format!("\x1bO{letter}").into_bytes().into() }
        }
        key if key.starts_with('f') && key.len() <= 3 => {
            let n: u8 = key[1..].parse().ok()?;
            let code = match n {
                5 => 15,
                6..=10 => n + 11,
                11 | 12 => n + 12,
                _ => return None,
            };
            tilde(code)
        }
        key if m.control => {
            // Ctrl+letter and the punctuation that maps to control codes.
            let c = key.chars().next()?;
            let byte = match c {
                'a'..='z' => c as u8 - b'a' + 1,
                '@' | '2' => 0,
                '[' | '3' => 27,
                '\\' | '4' => 28,
                ']' | '5' => 29,
                '^' | '6' => 30,
                '_' | '-' | '7' => 31,
                '/' => 31,
                '?' | '8' => 127,
                _ => return None,
            };
            if key.chars().count() != 1 {
                return None;
            }
            meta(&[byte])
        }
        // Option types characters on macOS (`@` on many layouts), so only
        // elsewhere does Alt act as Meta.
        _ if m.alt && !cfg!(target_os = "macos") => {
            let text = keystroke.key_char.clone().unwrap_or_else(|| keystroke.key.clone());
            meta(text.as_bytes())
        }
        _ => return None,
    })
}

// ---- View --------------------------------------------------------------------------

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search = self.search.as_ref().map(|search| self.render_search(search, window, cx).into_any_element());
        let theme = Theme::get(cx);
        div()
            .id("terminal")
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .bg(theme.background)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::clear))
            .on_action(cx.listener(Self::open_search))
            .on_action(cx.listener(Self::close_search))
            .on_action(cx.listener(Self::on_find_older))
            .on_action(cx.listener(Self::on_find_newer))
            .on_action(cx.listener(Self::on_find_up))
            .on_action(cx.listener(Self::on_find_down))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(TerminalElement { view: cx.entity() })
            .children(search)
    }
}

/// Text input goes to the shell. What an input method composes stays here
/// (`marked`) until it's committed: the shell can't take back what it got.
/// That text is the only text the input method sees, at 0..its length.
impl EntityInputHandler for TerminalView {
    fn text_for_range(&mut self, range: Range<usize>, adjusted: &mut Option<Range<usize>>, _: &mut Window, _: &mut Context<Self>) -> Option<String> {
        let text = &self.marked.as_ref()?.text;
        let end = utf16_to_byte(text, range.end);
        let start = utf16_to_byte(text, range.start).min(end);
        *adjusted = Some(text[..start].encode_utf16().count()..text[..end].encode_utf16().count());
        Some(text[start..end].to_string())
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        let range = match &self.marked {
            Some(marked) => marked.selected.clone().unwrap_or_else(|| {
                let end = marked.text.encode_utf16().count();
                end..end
            }),
            None => 0..0,
        };
        Some(UTF16Selection { range, reversed: false })
    }

    /// While there is one, macOS gives keys to the input method first, so
    /// Enter commits the composition instead of reaching the shell ahead of it.
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|marked| 0..marked.text.encode_utf16().count())
    }

    /// The composition is accepted as it is (Linux does this on a click).
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(marked) = self.marked.take() {
            self.input(marked.text.into_bytes(), cx);
        }
    }

    /// Typed or committed text; a commit replaces the composition, which
    /// the shell never saw.
    fn replace_text_in_range(&mut self, _: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        let composed = self.marked.take().is_some();
        if !text.is_empty() {
            self.input(text.to_string().into_bytes(), cx);
        } else if composed {
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if text.is_empty() {
            // Cancelled (Escape), or all of it deleted.
            self.marked = None;
        } else {
            if self.marked.is_none() {
                // It shows at the cursor, on the live screen.
                self.term.lock().scroll_display(Scroll::Bottom);
            }
            self.marked = Some(Marked { text: text.to_string(), selected });
        }
        cx.notify();
    }

    /// The input method's window goes under the composed text, at the cursor.
    fn bounds_for_range(&mut self, range: Range<usize>, _: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let mut origin = self.cursor_origin();
        if let Some(marked) = &self.marked {
            let (font, font_size) = terminal_font(Theme::get(cx));
            let runs = marked_runs(marked, &font, Theme::get(cx).text.into());
            let line = window.text_system().shape_line(marked.text.clone().into(), font_size, &runs, None);
            origin.x += line.x_for_index(utf16_to_byte(&marked.text, range.start));
        }
        Some(Bounds::new(origin, size(self.cell.0, self.cell.1)))
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}

struct TerminalElement {
    view: Entity<TerminalView>,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalFrame;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> TerminalFrame {
        let mut frame = self.view.update(cx, |view, cx| view.layout(bounds, window, cx));
        frame.hitbox = Some(window.insert_hitbox(bounds, HitboxBehavior::Normal));
        frame
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        frame: &mut TerminalFrame,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.view.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.view.clone()), cx);
        if let Some(hitbox) = frame.hitbox.as_ref() {
            let style = if frame.link.is_some() { CursorStyle::PointingHand } else { CursorStyle::IBeam };
            window.set_cursor_style(style, hitbox);
        }
        window.with_content_mask(Some(ContentMask { bounds: frame.bounds }), |window| {
            if let Some(underline) = frame.link.take() {
                window.paint_quad(underline);
            }
            for quad in frame.backgrounds.drain(..).chain(frame.highlights.drain(..)) {
                window.paint_quad(quad);
            }
            if let Some(cursor) = frame.cursor.take() {
                window.paint_quad(cursor);
            }
            for (origin, line) in &frame.lines {
                line.paint(*origin, frame.line_height, window, cx).ok();
            }
            if let Some(marked) = frame.marked.take() {
                window.paint_quad(marked.background);
                marked.line.paint(marked.origin, frame.line_height, window, cx).ok();
                if let Some(caret) = marked.caret {
                    window.paint_quad(caret);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_runs_keep_their_command_and_exit_code() {
        let script = powershell_run_script("npx jest -t 'doesn''t crash'", Path::new(r"C:\Users\O'Brien\app"), Some(Path::new(r"C:\app\.venv")));
        let lines: Vec<&str> = script.lines().collect();
        assert_eq!(lines[0], r"Set-Location -LiteralPath 'C:\Users\O''Brien\app'");
        let bin = crate::python::bin_dir(Path::new(r"C:\app\.venv"));
        assert_eq!(lines[2], format!("$env:PATH = '{};' + $env:PATH", bin.display()));
        assert_eq!(lines[4], "Write-Host '$ npx jest -t ''doesn''''t crash''' -ForegroundColor DarkGray");
        assert_eq!(lines[7..9], ["npx jest -t 'doesn''t crash'", "$kuanaOk = $?"]);
        assert_eq!(lines.last(), Some(&"exit $kuanaCode"));
        // A quoted program is called, not printed.
        assert!(powershell_run_script(r"'C:\my tools\x.exe' a", Path::new("C:\\"), None).contains("\n& 'C:\\my tools\\x.exe' a\n"));
        assert_eq!(encoded_command("A"), "QQA=");
        assert!(is_powershell(r"C:\Program Files\PowerShell\7\pwsh.exe") && is_powershell("powershell") && !is_powershell("cmd"));
    }

    #[test]
    fn titles_name_programs_without_their_folders() {
        assert_eq!(short_title(r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe"), "powershell");
        assert_eq!(short_title(r"C:\Program Files\PowerShell\7\pwsh.exe"), "pwsh");
        assert_eq!(short_title("C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"), "powershell");
        assert_eq!(short_title(r"C:\WINDOWS\system32\cmd.exe - npm test"), "cmd - npm test");
        assert_eq!(short_title(r"Administrator: C:\WINDOWS\system32\cmd.exe"), "Administrator: cmd");
        for kept in ["zsh", "me@mac: ~/dev/kuana", r"C:\Users\me\project", "vim notes.exe.txt", "make.exec", "setup.exe"] {
            assert_eq!(short_title(kept), kept);
        }
    }

    /// A Run tab ends with its command's exit code (a failed test or build
    /// must not show as finished fine).
    #[cfg(unix)]
    #[test]
    fn run_script_ends_with_the_commands_exit_code() {
        let output = std::process::Command::new("/bin/sh").arg("-c").arg(run_script("exit 3")).output().unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&output.stdout).contains("exit code 3"));
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn finds_a_process_working_directory() {
        let dir = std::env::temp_dir().join(format!("kuana-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = std::process::Command::new("sleep").arg("5").current_dir(&dir).spawn().unwrap();
        let found = process_cwd(child.id()).and_then(|p| p.canonicalize().ok());
        let expected = dir.canonicalize().ok();
        child.kill().ok();
        child.wait().ok();
        std::fs::remove_dir(&dir).ok();
        assert!(found.is_some());
        assert_eq!(found, expected);
    }

    fn keys(source: &str, mode: TermMode) -> Option<Vec<u8>> {
        escape_for(&gpui::Keystroke::parse(source).unwrap(), mode).map(|c| c.into_owned())
    }

    #[test]
    fn find_keeps_the_newest_matches() {
        let (tx, _rx) = mpsc::unbounded();
        let mut term = Term::new(Config::default(), &TermSize::new(40, 10), Listener(tx));
        let mut parser = alacritty_terminal::vte::ansi::Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
        let lines = MAX_MATCHES + 10;
        let output: String = (0..lines).map(|i| format!("hit {i}\r\n")).collect();
        parser.advance(&mut term, output.as_bytes());
        let (matches, _) = find_in(&term, "hit");
        assert_eq!(matches.len(), MAX_MATCHES);
        assert!(matches.windows(2).all(|w| w[0].start() < w[1].start()), "top to bottom");
        let text = |m: &Match| term.bounds_to_string(GridPoint::new(m.start().line, Column(0)), GridPoint::new(m.start().line, Column(12)));
        assert!(text(matches.last().unwrap()).starts_with(&format!("hit {}", lines - 1)));
        assert!(text(&matches[0]).starts_with("hit 10"), "{}", text(&matches[0]));
    }

    #[test]
    fn finds_links_in_output() {
        let dir = std::env::temp_dir().join(format!("kuana-term-links-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        let row: Vec<char> = "  --> src/main.rs:12:5 (see https://example.com/a?b=1).".chars().collect();
        let at = |needle: &str| row.iter().collect::<String>().find(needle).unwrap() + 1;
        let (range, text) = word_at(&row, at("src")).unwrap();
        assert_eq!((range, text.as_str()), (6..22, "src/main.rs:12:5"));
        assert_eq!(
            parse_link(&text, &dir),
            Some(Link::File { path: dir.join("src/main.rs"), line: Some(12), column: Some(5) })
        );
        let (_, url) = word_at(&row, at("https")).unwrap();
        assert_eq!(parse_link(&url, &dir), Some(Link::Url("https://example.com/a?b=1".into())));
        // Plain words and missing files aren't links.
        let (_, word) = word_at(&row, at("see")).unwrap();
        assert_eq!(parse_link(&word, &dir), None);
        assert_eq!(parse_link("src/missing.rs:3", &dir), None);
        assert!(word_at(&row, 0).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn maps_special_keys() {
        let none = TermMode::empty();
        assert_eq!(keys("enter", none).unwrap(), b"\r");
        assert_eq!(keys("up", none).unwrap(), b"\x1b[A");
        assert_eq!(keys("up", TermMode::APP_CURSOR).unwrap(), b"\x1bOA");
        assert_eq!(keys("ctrl-left", none).unwrap(), b"\x1b[1;5D");
        assert_eq!(keys("ctrl-c", none).unwrap(), &[3]);
        assert_eq!(keys("ctrl-r", none).unwrap(), &[18]);
        assert_eq!(keys("shift-tab", none).unwrap(), b"\x1b[Z");
        assert_eq!(keys("f5", none).unwrap(), b"\x1b[15~");
        assert_eq!(keys("f1", none).unwrap(), b"\x1bOP");
        assert_eq!(keys("pagedown", none).unwrap(), b"\x1b[6~");
        // Text goes through the input handler, ⌘ shortcuts to the app.
        assert!(keys("a", none).is_none());
        assert!(keys("cmd-c", none).is_none());
    }

    #[test]
    fn xterm_palette() {
        let theme = Theme::blank_for_tests();
        assert_eq!(palette(&theme, 16), Some(Rgb { r: 0, g: 0, b: 0 }));
        assert_eq!(palette(&theme, 231), Some(Rgb { r: 255, g: 255, b: 255 }));
        assert_eq!(palette(&theme, 232), Some(Rgb { r: 8, g: 8, b: 8 }));
    }

    // ---- Drawing -----------------------------------------------------------------------

    fn parse(term: &mut Term<Listener>, output: &str) {
        let mut parser = alacritty_terminal::vte::ansi::Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
        parser.advance(term, output.as_bytes());
    }

    /// A terminal `cols` wide that printed `output`.
    fn printed(output: &str, cols: usize, rows: usize) -> Term<Listener> {
        let (tx, _) = mpsc::unbounded();
        let mut term = Term::new(Config::default(), &TermSize::new(cols, rows), Listener(tx));
        parse(&mut term, output);
        term
    }

    /// The screen's cells, as a frame gathers them.
    fn cells(term: &Term<Listener>) -> FrameCells {
        let mut cells = FrameCells::default();
        for indexed in term.renderable_content().display_iter {
            cells.push(indexed.point.line.0 as usize, indexed.point.column.0, indexed.cell, false, false);
        }
        cells.finish()
    }

    /// A row's pieces: column, text, and whether it's shaped on the cell grid.
    fn pieces(cells: &FrameCells, row: usize) -> Vec<(usize, &str, bool)> {
        cells.pieces.iter().filter(|p| p.row == row).map(|p| (p.col, p.text.as_str(), p.grid)).collect()
    }

    /// A piece's runs: their text and color.
    fn runs(piece: &Piece) -> Vec<(&str, Ink)> {
        let mut at = 0;
        piece
            .runs
            .iter()
            .map(|&(len, style)| {
                at += len;
                (&piece.text[at - len..at], style.fg)
            })
            .collect()
    }

    #[test]
    fn rows_of_any_script_shape_as_one_run() {
        // A TUI's border around text (lazygit, htop): box drawing and
        // accented letters join the ASCII around them, colors are runs in
        // the piece, and it ends at the last character, not the row's end.
        let term = printed("\x1b[34m│\x1b[0m café ─┼─ naïve \x1b[34m│\x1b[0m\r\nab漢cd\r\n", 40, 3);
        let cells = cells(&term);
        assert_eq!(pieces(&cells, 0), [(0, "│ café ─┼─ naïve │", true)]);
        let (blue, plain) = (Ink::Index(NamedColor::Blue as u16), Ink::Index(NamedColor::Foreground as u16));
        assert_eq!(runs(&cells.pieces[0]), [("│ ", blue), ("café ─┼─ naïve ", plain), ("│", blue)]);
        // A wide character is shaped alone; the columns after it stay right.
        assert_eq!(pieces(&cells, 1), [(0, "ab", true), (2, "漢", false), (4, "cd", true)]);
    }

    #[test]
    fn zero_width_characters_draw_with_their_cell() {
        // The accent macOS writes after its letter (decomposed file names),
        // the selector that makes ❤ an emoji, and joined emoji: one picture,
        // at the first of their cells.
        let output = "cafe\u{301} ok\r\n\u{2764}\u{fe0f}!\r\n\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}=3\r\n";
        let term = printed(output, 40, 4);
        let cells = cells(&term);
        assert_eq!(pieces(&cells, 0), [(0, "caf", true), (3, "e\u{301}", false), (5, "ok", true)]);
        assert_eq!(pieces(&cells, 1), [(0, "\u{2764}\u{fe0f}", false), (1, "!", true)]);
        assert_eq!(pieces(&cells, 2), [(0, "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}", false), (6, "=3", true)]);
        // The text a Run's problems are read from keeps them too.
        assert!(Output(Arc::new(FairMutex::new(term))).text().starts_with("cafe\u{301} ok\n\u{2764}\u{fe0f}!\n"));
    }

    #[test]
    fn underlines_end_at_their_last_cell() {
        let term = printed("\x1b[4mlink\x1b[0m and\r\nsee \x1b[4mhere\x1b[0m\r\n", 40, 3);
        let cells = cells(&term);
        let underlined = |piece: &Piece| piece.runs.iter().map(|&(len, style)| (len, style.flags.intersects(DECORATIONS))).collect::<Vec<_>>();
        // The space after a link isn't underlined with it.
        assert_eq!(underlined(&cells.pieces[0]), [(4, true), (4, false)]);
        // At a row's end, a plain space stops the underline on its cell's
        // edge (it would run to the shaped line's own width).
        assert_eq!(cells.pieces[1].text, "see here ");
        assert_eq!(underlined(&cells.pieces[1]), [(4, false), (4, true), (1, false)]);
    }

    /// A terminal view whose shell never starts: what it sends stays queued.
    fn without_shell(cx: &mut Context<TerminalView>) -> TerminalView {
        let (tx, rx) = mpsc::unbounded();
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &TermSize::new(40, 10), Listener(tx))));
        TerminalView::new(term, rx, PathBuf::from("/"), cx)
    }

    /// What a view sent its shell so far.
    fn sent(view: &TerminalView) -> String {
        match &view.pty {
            Pty::Starting(typed) => String::from_utf8_lossy(typed).into_owned(),
            _ => panic!("the shell shouldn't have started"),
        }
    }

    /// A view whose element lays the terminal out and keeps the frame.
    struct Host {
        terminal: Entity<TerminalView>,
        frame: std::rc::Rc<std::cell::RefCell<Option<TerminalFrame>>>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Probe { terminal: self.terminal.clone(), frame: self.frame.clone() }
        }
    }

    struct Probe {
        terminal: Entity<TerminalView>,
        frame: std::rc::Rc<std::cell::RefCell<Option<TerminalFrame>>>,
    }

    impl IntoElement for Probe {
        type Element = Self;

        fn into_element(self) -> Self {
            self
        }
    }

    impl Element for Probe {
        type RequestLayoutState = ();
        type PrepaintState = ();

        fn id(&self) -> Option<ElementId> {
            None
        }

        fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
            None
        }

        fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
            let mut style = Style::default();
            style.size.width = relative(1.).into();
            style.size.height = relative(1.).into();
            (window.request_layout(style, [], cx), ())
        }

        fn prepaint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, bounds: Bounds<Pixels>, _: &mut (), window: &mut Window, cx: &mut App) {
            let frame = self.terminal.update(cx, |t, cx| t.layout(bounds, window, cx));
            *self.frame.borrow_mut() = Some(frame);
        }

        fn paint(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, _: Bounds<Pixels>, _: &mut (), _: &mut (), _: &mut Window, _: &mut App) {}
    }

    /// A terminal without a shell, in an 800 by 600 window.
    fn hosted(cx: &mut gpui::TestAppContext) -> (Entity<TerminalView>, Entity<Host>, &mut gpui::VisualTestContext) {
        cx.update(|cx| cx.set_global(Theme::blank_for_tests()));
        let terminal = cx.new(without_shell);
        let host = cx.new(|_| Host { terminal: terminal.clone(), frame: Default::default() });
        let cx = cx.add_empty_window();
        cx.update(|window, cx| {
            window.activate_window();
            window.focus(&terminal.focus_handle(cx));
        });
        (terminal, host, cx)
    }

    fn frame(host: &Entity<Host>, cx: &mut gpui::VisualTestContext) -> TerminalFrame {
        cx.draw(point(px(0.), px(0.)), size(px(800.), px(600.)), |_, _| gpui::AnyView::from(host.clone()));
        host.update(cx, |h, _| h.frame.borrow_mut().take()).expect("laid out")
    }

    #[gpui::test]
    fn a_tui_row_is_shaped_once(cx: &mut gpui::TestAppContext) {
        let (terminal, host, cx) = hosted(cx);
        terminal.update(cx, |t, _| parse(&mut t.term.lock(), "\x1b[34m┌─┤\x1b[0m main.rs \x1b[1;34m├──┐\x1b[0m"));
        let frame = frame(&host, cx);
        let lines: Vec<(Point<Pixels>, &str)> = frame.lines.iter().map(|(at, line)| (*at, line.text.as_ref())).collect();
        // At the text's top left, past the padding.
        assert_eq!(lines, [(point(px(8.), px(4.)), "┌─┤ main.rs ├──┐")]);
    }

    #[gpui::test]
    fn composed_text_waits_for_its_commit(cx: &mut gpui::TestAppContext) {
        let (terminal, host, cx) = hosted(cx);
        // かん, typed in a Japanese input method: shown, not sent.
        cx.update(|window, cx| {
            terminal.update(cx, |t, cx| {
                t.replace_and_mark_text_in_range(None, "か", Some(1..1), window, cx);
                t.replace_and_mark_text_in_range(None, "かん", Some(2..2), window, cx);
                // While there is marked text, macOS hands Enter to the input method.
                assert_eq!(t.marked_text_range(window, cx), Some(0..2));
                assert_eq!(t.selected_text_range(false, window, cx).map(|s| s.range), Some(2..2));
                let mut adjusted = None;
                assert_eq!(t.text_for_range(1..9, &mut adjusted, window, cx).as_deref(), Some("ん"));
                assert_eq!(adjusted, Some(1..2));
            })
        });
        assert_eq!(terminal.read_with(cx, |t, _| sent(t)), "");
        // It's drawn where the cursor is, which it replaces meanwhile.
        let drawn = frame(&host, cx);
        assert_eq!(drawn.marked.as_ref().map(|m| m.origin), Some(point(px(8.), px(4.))));
        assert!(drawn.cursor.is_none());
        // Committed (Enter): sent once, and the cursor is back.
        cx.update(|window, cx| {
            terminal.update(cx, |t, cx| {
                t.replace_text_in_range(None, "漢", window, cx);
                assert_eq!(t.marked_text_range(window, cx), None);
            })
        });
        assert_eq!(terminal.read_with(cx, |t, _| sent(t)), "漢");
        let drawn = frame(&host, cx);
        assert!(drawn.marked.is_none() && drawn.cursor.is_some());
        // Cancelled (Escape): nothing sent. Let go of (`unmark_text`): accepted.
        cx.update(|window, cx| {
            terminal.update(cx, |t, cx| {
                t.replace_and_mark_text_in_range(None, "じ", None, window, cx);
                t.replace_and_mark_text_in_range(None, "", None, window, cx);
                assert_eq!(t.marked_text_range(window, cx), None);
                t.replace_and_mark_text_in_range(None, "字", None, window, cx);
                t.unmark_text(window, cx);
            })
        });
        assert_eq!(terminal.read_with(cx, |t, _| sent(t)), "漢字");
    }

    /// A Run's end: its exit code comes first, then its last output; it's
    /// over (and its problems read) once, after both.
    #[cfg(unix)]
    #[gpui::test]
    fn a_run_ends_once_after_its_output(cx: &mut gpui::TestAppContext) {
        use std::os::unix::process::ExitStatusExt;
        let terminal = cx.new(|cx| {
            let mut view = without_shell(cx);
            let target = RunTarget { name: "Test".into(), command: "exit 3".into(), cwd: "/".into(), row: None, rows: 0..0, debug: None, test: None };
            view.run = Some((target, RunState::Running));
            view
        });
        let ended = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = ended.clone();
        cx.update(|cx| {
            cx.subscribe(&terminal, move |_, event: &TerminalEvent, _| counter.set(counter.get() + matches!(event, TerminalEvent::Exited) as u32))
                .detach()
        });
        terminal.update(cx, |t, cx| {
            t.handle_event(TermEvent::ChildExit(std::process::ExitStatus::from_raw(3 << 8)), cx);
            assert_eq!(t.run().map(|(_, state)| *state), Some(RunState::Running));
            t.handle_event(TermEvent::Exit, cx);
        });
        cx.run_until_parked();
        assert_eq!(ended.get(), 1);
        assert_eq!(terminal.read_with(cx, |t, _| t.run().map(|(_, state)| *state)), Some(RunState::Exited(Some(3))));
    }

    /// Shells start on a background thread, which may block signals (GCD's
    /// workers do); the shell must still get them (⌃C, closing the tab).
    #[cfg(unix)]
    #[test]
    fn shells_get_signals_whatever_thread_starts_them() {
        let (started, mut events) = std::thread::spawn(|| {
            // SAFETY: this new thread's own mask.
            unsafe {
                let mut all = std::mem::zeroed();
                libc::sigfillset(&mut all);
                libc::pthread_sigmask(libc::SIG_SETMASK, &all, std::ptr::null_mut());
            }
            let (tx, rx) = mpsc::unbounded();
            let term = Arc::new(FairMutex::new(Term::new(Config::default(), &TermSize::new(80, 24), Listener(tx.clone()))));
            let shell = Some(vec!["/bin/sleep".into(), "30".into()]);
            (start_pty(shell, &std::env::temp_dir(), None, term, Listener(tx)).unwrap(), rx)
        })
        .join()
        .unwrap();
        // SAFETY: a signal to the child this test started.
        unsafe { libc::kill(started.process.as_ref().unwrap().pid as i32, libc::SIGTERM) };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match events.try_recv() {
                Ok(TermEvent::Exit) => break,
                Ok(_) => {}
                Err(_) => {
                    assert!(std::time::Instant::now() < deadline, "SIGTERM didn't end the shell");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        let _ = started.notifier.0.send(Msg::Shutdown);
    }

    /// A command the shell runs is in the pty's foreground (closing the tab
    /// asks first); at the prompt, the shell itself is.
    #[cfg(unix)]
    #[test]
    fn finds_the_command_a_shell_runs() {
        command_in_foreground(Some(vec!["/bin/sh".into(), "-i".into()]));
    }

    /// The same for the user's own shell, which macOS starts through `login`
    /// (an idle shell isn't a command `login` runs).
    #[cfg(target_os = "macos")]
    #[test]
    fn finds_the_command_a_login_shell_runs() {
        command_in_foreground(None);
    }

    #[cfg(unix)]
    fn command_in_foreground(shell: Option<Vec<String>>) {
        let (tx, _events) = mpsc::unbounded();
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &TermSize::new(80, 24), Listener(tx.clone()))));
        let started = start_pty(shell, &std::env::temp_dir(), None, term, Listener(tx)).unwrap();
        let process = started.process.as_ref().unwrap();
        let wait_for = |what: &str, done: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while !done() {
                assert!(std::time::Instant::now() < deadline, "{what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        // Typed while the shell starts, it runs after the profile (whose own
        // commands may have the foreground meanwhile).
        started.notifier.notify(&b"sleep 30\r"[..]);
        wait_for("the command never got the foreground", &|| process.foreground_command().as_deref() == Some("sleep"));
        // Back at the prompt, the shell is idle.
        started.notifier.notify(&b"\x03"[..]);
        wait_for("the shell never got the foreground back", &|| process.foreground_command().is_none());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(process.foreground_command(), None);
        assert_eq!(foreground_group(process.master.as_ref().unwrap()), process.shell());
        let _ = started.notifier.0.send(Msg::Shutdown);
    }
}
