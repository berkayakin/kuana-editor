//! An agent session in a pane tab: its header (agent, title, mode, model,
//! context meter), the transcript (answers in Markdown, thoughts folded,
//! tool calls with their diffs, terminal output and permission questions,
//! runs of finished steps folded into one line, the plan), the files it
//! changed, and the composer. Menus and anything that needs the workspace
//! (opening files and diffs, attaching context, reviewing changes) go out
//! as [`SessionViewEvent`]s.

use std::{collections::HashMap, collections::HashSet, ops::Range, path::PathBuf, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ListAlignment, ListState, ParentElement as _, Pixels, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, actions,
    Animation, AnimationExt as _, div, list, prelude::FluentBuilder as _, pulsating_between, px, relative,
};

use super::session::{ContextItem, Session, SessionEvent, State};
use super::transcript::{Entry, Group, Kind, Level, Status, Tool, ToolContent};
use crate::{
    assets::Icon,
    editor::Editor,
    markdown_preview::{self, Block, CodeAction, Draw, Inline},
    theme::Theme,
    ui::{HoverFill as _, icon_button, icon_el, tooltip_view},
};

actions!(agents, [SendMessage, StopAgent, AcceptSuggestion]);

/// The most of a mentioned file attached.
const MENTION_BYTES: usize = 256 * 1024;

/// What a file or folder dropped on a session becomes (`attachment`).
#[derive(Debug, PartialEq)]
pub enum Attachment {
    /// Its text or picture, in the context sent with the prompt.
    Context(ContextItem),
    /// Named in the prompt for the agent to look at: a folder, or a file
    /// that's neither text nor a picture (why, for a file).
    Mention { path: PathBuf, folder: bool, why: Option<String> },
    /// Not taken, and why.
    Refused(String),
}

/// Reads a path dropped on a session (blocking: call it off the UI thread).
pub fn attachment(path: &std::path::Path) -> Attachment {
    let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
    if crate::fs::metadata(path).is_ok_and(|m| m.is_dir()) {
        return Attachment::Mention { path: path.to_path_buf(), folder: true, why: None };
    }
    let bytes = match crate::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => return Attachment::Refused(format!("{name}: {e}")),
    };
    if let Some(mime) = super::session::image_mime(path) {
        if bytes.len() > super::session::MAX_IMAGE_BYTES {
            return Attachment::Refused(format!("{name} is over 5 MB"));
        }
        return Attachment::Context(ContextItem::Image { name, mime: mime.into(), data: bytes.into() });
    }
    if crate::encoding::looks_binary(&bytes) {
        let why = format!("{name} isn't text or a picture: it's named in the prompt for the agent to open");
        return Attachment::Mention { path: path.to_path_buf(), folder: false, why: Some(why) };
    }
    Attachment::Context(ContextItem::File { path: path.to_path_buf(), text: mentioned_text(&bytes) })
}

/// A file's text as attached: at most `MENTION_BYTES`, what's left out said.
fn mentioned_text(bytes: &[u8]) -> String {
    let mut text = crate::encoding::decode(&bytes[..bytes.len().min(MENTION_BYTES)], None).0;
    if bytes.len() > MENTION_BYTES {
        text.push_str(&format!("\n[… {} more bytes left out]", bytes.len() - MENTION_BYTES));
    }
    text
}

/// The transcript's column, for reading comfort in a wide pane.
const COLUMN: f32 = 780.;
/// Lines of a terminal's output or a diff shown in a tool call.
const PREVIEW_LINES: usize = 14;

#[derive(Clone, Debug, PartialEq)]
pub enum MenuKind {
    /// The permission mode.
    Mode,
    /// The model, its effort and the agent's other options.
    Model,
    AddContext,
    More,
    /// Where it works: the project folder or a worktree of its own (before
    /// the first message), then its worktree's actions.
    Place,
}

pub enum SessionViewEvent {
    OpenFile { path: PathBuf, line: Option<u32> },
    OpenDiff { path: PathBuf, old: Option<String> },
    Menu { kind: MenuKind, position: gpui::Point<Pixels> },
    Review,
    RejectAll,
    KeepAll,
    RunCommand(String),
    /// Put the files the agent changed since this turn back as they were.
    RestoreTo(usize),
    /// An earlier message, edited: send `text` in place of turn `turn`'s
    /// (the workspace asks about the files changed since, then
    /// `Session::edit_and_send`).
    Resend { turn: usize, text: String, context: Vec<ContextItem> },
}

pub struct SessionView {
    pub session: Entity<Session>,
    composer: Entity<Editor>,
    list: ListState,
    groups: Vec<Group>,
    seen: Option<(u64, usize, bool)>,
    /// Open: tool calls and folded runs by their key, thoughts by entry.
    open: HashSet<String>,
    /// Closed by the user though open by default (a diff, a permission).
    closed: HashSet<String>,
    markdown: HashMap<usize, (usize, Rc<Vec<Block>>)>,
    /// Tools' text as Markdown, by tool call and content (its length then).
    tool_markdown: std::cell::RefCell<HashMap<(String, usize), (usize, Rc<Vec<Block>>)>>,
    /// Context to send with the next prompt.
    pub context: Vec<ContextItem>,
    copied: Option<usize>,
    /// What's selected in the transcript (its text is selectable, across
    /// messages).
    selection: Rc<crate::selectable_text::TextSelection>,
    /// The message whose Copy was just clicked (a check shows a moment).
    copied_entry: Option<usize>,
    copied_reset: Option<gpui::Task<()>>,
    /// Diff previews by tool call and file, with the sizes they were made
    /// for: drawing a card mustn't diff whole files every frame.
    diff_previews: std::cell::RefCell<HashMap<(String, PathBuf), (usize, usize, Rc<Preview>)>>,
    /// The project's files, for `@` mentions, and the last query's matches.
    files: Arc<Vec<PathBuf>>,
    mentions: Option<(String, Vec<PathBuf>)>,
    mention_task: Option<(String, gpui::Task<()>)>,
    /// The earlier message being edited (its turn): sending replaces it and
    /// what came after.
    editing: Option<usize>,
    /// Why the last thing attached wasn't, shown over the composer until
    /// the next try.
    pub notice: Option<SharedString>,
    /// Whether typing asked the agent to start (`Session::start`).
    started: bool,
    /// Whether the agent was asking something when last seen.
    asking: bool,
    /// The suggestion picked with ↑/↓, and the query it was picked for (a
    /// new query starts at the first again).
    picked: (usize, String),
    /// The composer's text when Escape hid the suggestions (they stay hidden
    /// until it changes).
    dismissed: Option<String>,
    /// Files attached by an `@` mention, with the text it put in the
    /// composer: deleting that text drops the file too.
    mentioned: Vec<(PathBuf, String)>,
    /// Earlier prompts brought back with ↑ (newest first) and the one shown;
    /// over once the composer holds something else (`prompts::step`).
    recall: Option<(Vec<String>, usize)>,
    /// Whether the session's project is in a Git repository (it may work in
    /// a worktree of its own then), set by the workspace.
    pub in_repo: bool,
    /// What's being done to its worktree (made, removed…), shown on its pill.
    pub place_busy: Option<SharedString>,
    /// The chat's text size (`agent_font_size`) and its scale against the
    /// 13-point text the transcript's smaller labels were drawn for.
    chat_size: f32,
    scale: f32,
    /// Words are being dictated into the composer (`crate::dictation`), and
    /// when its mic button went down (held, releasing it stops).
    dictating: bool,
    mic_pressed: Option<std::time::Instant>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SessionViewEvent> for SessionView {}

impl SessionView {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> SessionView {
        let placeholder = format!("Ask {} anything · @ for a file, / for commands", session.read(cx).agent_name);
        let composer = cx.new(|cx| Editor::multi_line(placeholder, cx));
        let subscriptions = vec![
            cx.observe(&session, |view, session, cx| {
                view.sync(cx);
                // The agent asks something: where it asks shows, even if the
                // transcript was scrolled up (it waits for the answer).
                let asking = session.read(cx).needs_you();
                if asking && !view.asking {
                    view.follow_end();
                }
                view.asking = asking;
                cx.notify();
            }),
            cx.subscribe(&session, |_, _, _: &SessionEvent, cx| cx.notify()),
            cx.observe(&composer, |view, composer, cx| {
                // Something typed for a session whose agent isn't up (a
                // restored one): it starts now, not when the prompt's sent.
                if !view.started && !composer.read(cx).text(cx).is_empty() {
                    view.started = true;
                    view.session.update(cx, |s, cx| s.start(cx));
                }
                cx.notify()
            }),
        ];
        let mut view = SessionView {
            session,
            composer,
            list: ListState::new(0, ListAlignment::Bottom, px(800.)),
            groups: Vec::new(),
            seen: None,
            open: HashSet::new(),
            closed: HashSet::new(),
            markdown: HashMap::new(),
            tool_markdown: Default::default(),
            context: Vec::new(),
            copied: None,
            selection: {
                let (view, opener) = (cx.entity().downgrade(), cx.entity().downgrade());
                crate::selectable_text::TextSelection::with_links(
                    move |cx| {
                        view.update(cx, |_, cx| cx.notify()).ok();
                    },
                    move |url, cx| {
                        if let Some((path, line)) = file_link_target(url) {
                            opener.update(cx, |_, cx| cx.emit(SessionViewEvent::OpenFile { path, line })).ok();
                        }
                    },
                )
            },
            copied_entry: None,
            copied_reset: None,
            diff_previews: Default::default(),
            files: Arc::default(),
            mentions: None,
            mention_task: None,
            notice: None,
            editing: None,
            started: false,
            asking: false,
            picked: (0, String::new()),
            dismissed: None,
            mentioned: Vec::new(),
            recall: None,
            in_repo: false,
            place_busy: None,
            chat_size: crate::settings::DEFAULT_AGENT_FONT_SIZE,
            scale: crate::settings::DEFAULT_AGENT_FONT_SIZE / 13.,
            dictating: false,
            mic_pressed: None,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        // The prompts ↑ brings back, read now so the first ↑ needn't.
        cx.background_executor().spawn(async { super::prompts::load() }).detach();
        view._subscriptions.push(cx.observe_global::<crate::settings::Settings>(|v, cx| v.apply_text_size(cx)));
        // The mic button follows a dictation into the composer (its level too).
        if let Some(store) = crate::dictation::store(cx) {
            view._subscriptions.push(cx.observe(&store, |v, store, cx| {
                let into = store.read(cx).is_into(&v.composer);
                if into || v.dictating {
                    v.dictating = into;
                    cx.notify();
                }
            }));
        }
        view.apply_text_size(cx);
        view.sync(cx);
        // Scrolling redraws the view: the ↓ button shows while it's scrolled up.
        let this = cx.entity().downgrade();
        view.list.set_scroll_handler(move |_, _, cx| {
            this.update(cx, |_, cx| cx.notify()).ok();
        });
        view
    }

    /// Another view of the same session (a split).
    pub fn split_of(other: &Entity<SessionView>, cx: &mut Context<Self>) -> SessionView {
        let mut view = SessionView::new(other.read(cx).session.clone(), cx);
        view.files = other.read(cx).files.clone();
        view
    }

    /// The project's files, which `@` suggests.
    pub fn set_files(&mut self, files: Arc<Vec<PathBuf>>, cx: &mut Context<Self>) {
        if !Arc::ptr_eq(&self.files, &files) {
            // A big project's list may be freed here: not on the UI thread.
            let old = std::mem::replace(&mut self.files, files);
            cx.background_executor().spawn(async move { drop(old) }).detach();
            self.mentions = None;
        }
    }

    /// The `@…` being typed at the caret and its query.
    fn mention_query(&self, cx: &App) -> Option<(Range<usize>, String)> {
        let composer = self.composer.read(cx);
        let selection = composer.primary_selection();
        if !selection.range().is_empty() {
            return None;
        }
        let text = composer.text(cx);
        let range = super::mentions::mention_at(&text, selection.head)?;
        let query = text[range.start + 1..range.end].to_string();
        Some((range, query))
    }

    /// The `@…` being typed and the files it matches, as last matched: a new
    /// query is matched in the background (a big project has many files),
    /// the last matches showing meanwhile.
    fn mention(&mut self, cx: &mut Context<Self>) -> Option<(Range<usize>, Vec<PathBuf>)> {
        if self.suggestions_dismissed(cx) {
            return None;
        }
        let (range, query) = self.mention_query(cx)?;
        let matched = self.mentions.as_ref().is_some_and(|(q, _)| *q == query);
        let matching = self.mention_task.as_ref().is_some_and(|(q, _)| *q == query);
        if !matched && !matching {
            let (files, cwd, wanted) = (self.files.clone(), self.session.read(cx).folder().to_path_buf(), query.clone());
            let found = cx.background_executor().spawn(async move { super::mentions::matching(&files, &cwd, &wanted) });
            let task = cx.spawn(async move |view, cx| {
                let found = found.await;
                view.update(cx, |v, cx| {
                    if let Some((query, _)) = v.mention_task.take() {
                        v.mentions = Some((query, found));
                    }
                    cx.notify();
                })
                .ok();
            });
            self.mention_task = Some((query, task));
        }
        let found = self.mentions.as_ref().map(|(_, f)| f.clone()).unwrap_or_default();
        (!found.is_empty()).then_some((range, found))
    }

    /// The best match for the `@…` at the caret now (matched here if the
    /// background match hasn't caught up: a key was pressed to take it).
    fn current_mention(&mut self, cx: &mut Context<Self>) -> Option<(Range<usize>, PathBuf)> {
        if self.suggestions_dismissed(cx) {
            return None;
        }
        let (range, query) = self.mention_query(cx)?;
        let found = match &self.mentions {
            Some((q, found)) if *q == query => found.clone(),
            _ => super::mentions::matching(&self.files, self.session.read(cx).folder(), &query),
        };
        let ix = self.pick_index(&query, found.len());
        found.into_iter().nth(ix).map(|path| (range, path))
    }

    /// Whether Escape hid the suggestions for the text as it is.
    fn suggestions_dismissed(&self, cx: &App) -> bool {
        self.dismissed.as_ref().is_some_and(|text| *text == self.composer.read(cx).text(cx))
    }

    /// The agent's `/` commands matching what's typed at the composer's
    /// start: names starting with it, then names holding it.
    fn command_matches(&self, cx: &App) -> Vec<(String, String)> {
        let text = self.composer.read(cx).text(cx);
        let Some(typed) = text.strip_prefix('/').filter(|typed| !typed.contains(char::is_whitespace)) else { return Vec::new() };
        if self.suggestions_dismissed(cx) {
            return Vec::new();
        }
        let typed = typed.to_lowercase();
        let commands = &self.session.read(cx).commands;
        let starts = |name: &str| name.to_lowercase().starts_with(&typed);
        let mut found: Vec<(String, String)> = commands.iter().filter(|(name, _)| starts(name)).cloned().collect();
        found.extend(commands.iter().filter(|(name, _)| !starts(name) && name.to_lowercase().contains(&typed)).cloned());
        found
    }

    /// The picked suggestion of `len` for `query` (the first for a new query).
    fn pick_index(&self, query: &str, len: usize) -> usize {
        if self.picked.1 == query { self.picked.0.min(len.saturating_sub(1)) } else { 0 }
    }

    /// ↑/↓ while suggestions show: the pick moves (round), and the key is
    /// taken; otherwise it goes on to the composer.
    fn move_pick(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let (query, len) = {
            let commands = self.command_matches(cx);
            if !commands.is_empty() {
                (self.composer.read(cx).text(cx), commands.len())
            } else if let Some((_, query)) = self.mention_query(cx).filter(|_| !self.suggestions_dismissed(cx))
                && let Some((matched, found)) = &self.mentions
                && *matched == query
                && !found.is_empty()
            {
                let len = found.len();
                (query, len)
            } else {
                return false;
            }
        };
        let at = self.pick_index(&query, len) as isize;
        self.picked = ((at + delta).rem_euclid(len as isize) as usize, query);
        cx.notify();
        true
    }

    /// Dictate into the composer, or stop: the project's words as hints.
    fn toggle_dictation(&mut self, cx: &mut Context<Self>) {
        let hints = self.dictation_hints(cx);
        crate::dictation::toggle(&self.composer, hints, cx);
    }

    /// Words a dictation should expect here: the project's name, the files
    /// attached or mentioned, the names at the project's top.
    fn dictation_hints(&self, cx: &App) -> Vec<String> {
        let folder = self.session.read(cx).folder().to_path_buf();
        let attached = self.context.iter().filter_map(|item| match item {
            ContextItem::File { path, .. } | ContextItem::Selection { path, .. } => path.file_name(),
            _ => None,
        });
        let mentioned = self.mentioned.iter().filter_map(|(path, _)| path.file_name());
        // The project's files, shallowest first: their folders' names, their
        // own and their stems (`cart.py` is said "cart").
        let mut files: Vec<&std::path::Path> = self.files.iter().filter_map(|p| p.strip_prefix(&folder).ok()).collect();
        files.sort_by_key(|p| p.components().count());
        let names = files.into_iter().take(400).flat_map(|p| {
            let top = p.components().next().filter(|_| p.components().count() > 1).map(|c| c.as_os_str());
            top.into_iter().chain(p.file_name()).chain(p.file_stem())
        });
        let mut hints: Vec<String> = Vec::new();
        for name in folder.file_name().into_iter().chain(attached).chain(mentioned).chain(names) {
            let name = name.to_string_lossy().into_owned();
            if name.len() > 1 && !name.starts_with('.') && !hints.contains(&name) {
                hints.push(name);
                if hints.len() >= 100 {
                    break;
                }
            }
        }
        hints
    }

    /// The mic button pressed: starts dictating (or stops one going on).
    fn mic_down(&mut self, cx: &mut Context<Self>) {
        if self.dictating {
            self.mic_pressed = None;
            self.toggle_dictation(cx);
        } else {
            self.mic_pressed = Some(std::time::Instant::now());
            self.toggle_dictation(cx);
        }
    }

    /// Released: held a moment, it was push-to-talk, so it stops; a click
    /// keeps it listening until the next.
    fn mic_up(&mut self, cx: &mut Context<Self>) {
        if let Some(pressed) = self.mic_pressed.take()
            && pressed.elapsed() >= std::time::Duration::from_millis(450)
            && self.dictating
        {
            self.toggle_dictation(cx);
        }
    }

    /// The chat's text size, from `agent_font_size`: the composer types at
    /// it, and the transcript's rows are measured again when it changes.
    fn apply_text_size(&mut self, cx: &mut Context<Self>) {
        let size = crate::settings::Settings::get(cx).agent_font_size();
        self.composer.update(cx, |e, cx| e.set_text_size(Some(px(size)), cx));
        if (size - self.chat_size).abs() > f32::EPSILON {
            self.chat_size = size;
            self.scale = size / 13.;
            // Every row's height changes; the one at the top stays there.
            let rows = self.list.item_count();
            self.remeasure(rows, rows);
            cx.notify();
        }
    }

    /// ↑ (`older`) or ↓ with no suggestions showing: an earlier prompt into
    /// the empty composer, as `prompts::step` says; whether the key was taken.
    fn recall(&mut self, older: bool, cx: &mut Context<Self>) -> bool {
        let (text, caret) = {
            let composer = self.composer.read(cx);
            let text = composer.text(cx);
            let caret = composer.primary_selection().head.min(text.len());
            (text, caret)
        };
        let showing = self.recall.as_ref().filter(|(list, at)| list.get(*at) == Some(&text)).map(|(_, at)| *at);
        if showing.is_none() {
            self.recall = None;
            if !(older && text.is_empty()) {
                return false;
            }
            let session = self.session.read(cx);
            let own: Vec<String> = session.transcript.entries.iter().filter_map(|e| if let Entry::User { text, .. } = e { Some(text.clone()) } else { None }).collect();
            self.recall = Some((super::prompts::recallable(own.into_iter(), session.folder()), 0));
        }
        let len = self.recall.as_ref().map_or(0, |(list, _)| list.len());
        let on_edge_line = if older { !text[..caret].contains('\n') } else { !text[caret..].contains('\n') };
        match super::prompts::step(showing, len, text.is_empty(), on_edge_line, older) {
            super::prompts::Recall::Pass => {
                if showing.is_none() {
                    self.recall = None;
                }
                false
            }
            super::prompts::Recall::Stay => true,
            super::prompts::Recall::Clear => {
                self.recall = None;
                self.composer.update(cx, |e, cx| e.set_text("", cx));
                true
            }
            super::prompts::Recall::Show(at) => {
                let Some((list, shown)) = self.recall.as_mut() else { return false };
                *shown = at;
                let prompt = list[at].clone();
                // A recalled `/command` or `@file` doesn't open suggestions.
                self.dismissed = Some(prompt.clone());
                // Going back, the caret starts at the top (the next ↑ goes on
                // back); going forward, at the end.
                let caret = if older { 0 } else { prompt.len() };
                self.composer.update(cx, |e, cx| {
                    e.set_text(&prompt, cx);
                    e.select_range(caret..caret, cx);
                });
                cx.notify();
                true
            }
        }
    }

    /// A command taken: `/name ` in the composer, the caret after it.
    fn accept_command(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = format!("/{name} ");
        self.composer.update(cx, |e, cx| {
            e.set_text(&text, cx);
            e.select_range(text.len()..text.len(), cx);
        });
        window.focus(&self.composer.focus_handle(cx));
        cx.notify();
    }

    /// A suggestion taken: `@path` in the text, the file in the context.
    fn accept_mention(&mut self, range: Range<usize>, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.session.read(cx).folder().to_path_buf();
        let rel = path.strip_prefix(&cwd).unwrap_or(&path).to_string_lossy().into_owned();
        let text = self.composer.read(cx).text(cx);
        let (start, end) = (range.start.min(text.len()), range.end.min(text.len()));
        let inserted = format!("@{rel} ");
        self.mentioned.push((self.session_path(&path, cx), format!("@{rel}")));
        let caret = start + inserted.len();
        let new_text = format!("{}{inserted}{}", &text[..start], text[end..].trim_start_matches(' '));
        self.composer.update(cx, |e, cx| {
            e.set_text(&new_text, cx);
            e.select_range(caret..caret, cx);
        });
        // Read off the UI thread; it joins the context when it's in.
        let read = cx.background_executor().spawn({
            let path = path.clone();
            async move { crate::fs::read(&path) }
        });
        cx.spawn(async move |view, cx| {
            let Ok(bytes) = read.await else { return };
            let text = mentioned_text(&bytes);
            view.update(cx, |v, cx| v.add_context(ContextItem::File { path, text }, cx)).ok();
        })
        .detach();
        window.focus(&self.composer.focus_handle(cx));
    }

    /// Files and folders dropped on the session, read (`attachment`): text
    /// and pictures join the context, folders and other files are named in
    /// the prompt at the caret (`@path`), and why one wasn't taken shows.
    pub fn add_attachments(&mut self, attachments: Vec<Attachment>, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.session.read(cx).folder().to_path_buf();
        let mut named = Vec::new();
        self.notice = None;
        for attachment in attachments {
            match attachment {
                Attachment::Context(item) => self.add_context(item, cx),
                Attachment::Mention { path, folder, why } => {
                    let rel = path.strip_prefix(&cwd).unwrap_or(&path).to_string_lossy().into_owned();
                    named.push(if folder && !rel.ends_with('/') { format!("@{rel}/") } else { format!("@{rel}") });
                    if let Some(why) = why {
                        self.notice = Some(why.into());
                    }
                }
                Attachment::Refused(why) => self.notice = Some(why.into()),
            }
        }
        if !named.is_empty() {
            self.composer.update(cx, |e, cx| {
                let text = e.text(cx);
                let caret = e.primary_selection().head.min(text.len());
                // Apart from a word the caret's right after.
                let lead = if text[..caret].chars().next_back().is_some_and(|c| !c.is_whitespace()) { " " } else { "" };
                e.insert_text(&format!("{lead}{} ", named.join(" ")), cx);
            });
        }
        window.focus(&self.composer.focus_handle(cx));
        cx.notify();
    }

    fn copy_selection(&mut self, _: &crate::editor::Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.composer.read(cx).primary_selection().range().is_empty() {
            return;
        }
        if self.copy_transcript(cx) {
            cx.stop_propagation();
        }
    }

    /// Copies what's selected in the transcript; whether anything was.
    fn copy_transcript(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.selection.text() else { return false };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    /// ⌘V / Ctrl+V with a picture on the clipboard: it's attached (and the
    /// editor doesn't paste); anything else pastes as usual.
    fn paste_image(&mut self, _: &crate::editor::Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(clipboard) = cx.read_from_clipboard() else { return };
        let image = clipboard.entries().iter().find_map(|entry| match entry {
            gpui::ClipboardEntry::Image(image) => Some(image.clone()),
            _ => None,
        });
        let Some(image) = image else { return };
        cx.stop_propagation();
        let mime = match image.format {
            gpui::ImageFormat::Png => "image/png",
            gpui::ImageFormat::Jpeg => "image/jpeg",
            gpui::ImageFormat::Gif => "image/gif",
            gpui::ImageFormat::Webp => "image/webp",
            _ => {
                self.notice = Some("Agents take PNG, JPEG, GIF and WebP pictures".into());
                cx.notify();
                return;
            }
        };
        if image.bytes.len() > super::session::MAX_IMAGE_BYTES {
            self.notice = Some("The picture is over 5 MB".into());
            cx.notify();
            return;
        }
        let n = self.context.iter().filter(|c| matches!(c, ContextItem::Image { .. })).count() + 1;
        self.notice = None;
        self.add_context(ContextItem::Image { name: format!("Pasted picture {n}"), mime: mime.into(), data: image.bytes.clone().into() }, cx);
    }

    /// Tab (and Enter, while suggestions show): the first `@` suggestion.
    fn accept_suggestion(&mut self, _: &AcceptSuggestion, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((range, picked)) = self.current_mention(cx) {
            self.accept_mention(range, picked, window, cx);
            return;
        }
        let commands = self.command_matches(cx);
        if !commands.is_empty() {
            let text = self.composer.read(cx).text(cx);
            let (name, _) = commands[self.pick_index(&text, commands.len())].clone();
            self.accept_command(&name, window, cx);
        }
    }

    /// Re-groups the transcript when it changed and has the list measure again.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        let key = (session.transcript.generation, session.transcript.entries.len(), session.state.busy() || session.needs_you());
        if self.seen == Some(key) {
            return;
        }
        let old = self.list.item_count();
        let entries = session.transcript.entries.len();
        // The conversation went back: what was drawn of the entries gone
        // goes too (a new entry may come at the same index).
        if self.seen.is_some_and(|(_, len, _)| len > entries) {
            self.markdown.retain(|ix, _| *ix < entries);
            self.selection.clear();
            self.selection.forget_from(entries);
        }
        self.groups = session.transcript.groups();
        let count = self.groups.len() + 1;
        self.seen = Some(key);
        self.remeasure(old, count);
    }

    /// Whether the transcript is scrolled up from its end (more than a line).
    fn scrolled_up(&self) -> bool {
        let below = self.list.max_offset_for_scrollbar().height + self.list.scroll_px_offset_for_scrollbar().y;
        below > px(24.)
    }

    pub fn composer(&self) -> &Entity<Editor> {
        &self.composer
    }

    /// Where a project file is for this session: its copy in the session's
    /// worktree when it has one there (a file only in the project folder,
    /// untracked, stays as it is).
    fn session_path(&self, path: &std::path::Path, cx: &App) -> PathBuf {
        match &self.session.read(cx).worktree {
            Some(wt) => Some(wt.map(path)).filter(|mapped| mapped != path && mapped.exists()).unwrap_or_else(|| path.to_path_buf()),
            None => path.to_path_buf(),
        }
    }

    pub fn add_context(&mut self, item: ContextItem, cx: &mut Context<Self>) {
        // A session in a worktree of its own is given its copies' paths, so
        // it edits those, not the project folder's.
        let item = match item {
            ContextItem::File { path, text } => ContextItem::File { path: self.session_path(&path, cx), text },
            ContextItem::Selection { path, lines, text } => ContextItem::Selection { path: self.session_path(&path, cx), lines, text },
            item => item,
        };
        if !self.context.contains(&item) {
            self.context.push(item);
        }
        cx.notify();
    }

    fn send(&mut self, _: &SendMessage, window: &mut Window, cx: &mut Context<Self>) {
        if self.current_mention(cx).is_some() {
            self.accept_suggestion(&AcceptSuggestion, window, cx);
            return;
        }
        let commands = self.command_matches(cx);
        if !commands.is_empty() {
            let text = self.composer.read(cx).text(cx);
            let (name, _) = &commands[self.pick_index(&text, commands.len())];
            if text.trim() != format!("/{name}") {
                self.accept_suggestion(&AcceptSuggestion, window, cx);
                return;
            }
        }
        let text = self.composer.read(cx).text(cx);
        if text.trim().is_empty() && self.context.is_empty() {
            return;
        }
        // A file mentioned with `@`, its mention deleted since, isn't sent.
        let mentioned = std::mem::take(&mut self.mentioned);
        let context: Vec<ContextItem> = std::mem::take(&mut self.context)
            .into_iter()
            .filter(|item| match item {
                ContextItem::File { path, .. } => mentioned.iter().filter(|(p, _)| p == path).all(|(_, mention)| text.contains(mention.as_str())),
                _ => true,
            })
            .collect();
        if text.trim().is_empty() && context.is_empty() {
            return;
        }
        // For ↑ later, in this project's other sessions too.
        let (folder, sent) = (self.session.read(cx).folder().to_path_buf(), text.trim().to_string());
        cx.background_executor().spawn(async move { super::prompts::record(&folder, &sent) }).detach();
        self.recall = None;
        self.composer.update(cx, |e, cx| e.set_text("", cx));
        if let Some(turn) = self.editing.take() {
            cx.emit(SessionViewEvent::Resend { turn, text: text.trim().to_string(), context });
            cx.notify();
            return;
        }
        self.session.update(cx, |s, cx| s.send(text.trim().to_string(), context, cx));
        // The newest message shows, and what the agent does after it.
        self.follow_end();
    }

    /// Back to the end of the transcript, following it as it grows.
    fn follow_end(&self) {
        self.list.reset(self.list.item_count());
    }

    fn stop(&mut self, _: &StopAgent, _: &mut Window, cx: &mut Context<Self>) {
        // Escape while dictating takes the words back out, nothing else.
        if crate::dictation::cancel_into(&self.composer, cx) {
            return;
        }
        let suggesting = !self.command_matches(cx).is_empty() || self.current_mention(cx).is_some();
        if suggesting {
            self.dismissed = Some(self.composer.read(cx).text(cx));
            cx.notify();
            return;
        }
        if self.session.read(cx).working() {
            self.session.update(cx, |s, cx| s.cancel(cx));
        } else if self.editing.is_some() {
            self.stop_editing(cx);
        } else {
            cx.propagate();
        }
    }

    /// Editing an earlier message (Edit on it): its text in the composer,
    /// which sends it in that message's place. An agent that can't go back
    /// gets the text to send as a new message.
    fn edit_message(&mut self, turn: usize, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = self.session.read(cx).can_rewind().then_some(turn);
        self.composer.update(cx, |e, cx| {
            e.set_text(&text, cx);
            let end = text.len();
            e.select_range(end..end, cx);
        });
        window.focus(&self.composer.focus_handle(cx));
        cx.notify();
    }

    /// Puts an edit back as it was (Escape, or ✕ on its banner).
    pub fn stop_editing(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            self.composer.update(cx, |e, cx| e.set_text("", cx));
            cx.notify();
        }
    }

    /// Edits `turn` again (the workspace's question about files was cancelled).
    pub fn resume_editing(&mut self, turn: usize, text: &str, context: Vec<ContextItem>, cx: &mut Context<Self>) {
        self.editing = Some(turn);
        self.composer.update(cx, |e, cx| e.set_text(text, cx));
        self.context = context;
        cx.notify();
    }

    fn toggle(&mut self, key: String, default_open: bool, cx: &mut Context<Self>) {
        let open = self.is_open(&key, default_open);
        if open {
            self.open.remove(&key);
            if default_open {
                self.closed.insert(key);
            }
        } else {
            self.closed.remove(&key);
            self.open.insert(key);
        }
        // What's on screen stays put, so what opens grows downward, as on a
        // page (a list following its end would grow it upward instead).
        if !self.scrolled_up() && self.list.max_offset_for_scrollbar().height > px(0.) {
            self.list.scroll_by(-self.list.viewport_bounds().size.height);
        }
        let count = self.list.item_count();
        self.remeasure(count, count);
        cx.notify();
    }

    /// Has every row measured again, keeping the scroll position: a full
    /// splice would put a list scrolled up back at its top (one following
    /// the end, at the end, keeps following it).
    fn remeasure(&mut self, old: usize, count: usize) {
        let top = self.list.logical_scroll_top();
        self.list.splice(0..old, count);
        if top.item_ix < old {
            self.list.scroll_to(top);
        }
    }

    fn is_open(&self, key: &str, default_open: bool) -> bool {
        if default_open { !self.closed.contains(key) } else { self.open.contains(key) }
    }

    // ---- Rows -------------------------------------------------------------------

    fn render_item(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let row = match self.groups.get(ix).cloned() {
            Some(Group::One(entry)) => self.render_entry(entry, window, cx),
            Some(Group::Tools(run)) => self.render_run(ix, &run, cx),
            None => self.render_footer(cx),
        };
        // A centered column, like a document.
        div().w_full().flex().justify_center().px_4().child(div().w_full().max_w(px(COLUMN)).py_1p5().child(row)).into_any_element()
    }

    fn render_entry(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let k = self.scale;
        let theme = Theme::get(cx).clone();
        let Some(entry) = self.session.read(cx).transcript.entries.get(ix).cloned() else { return div().into_any_element() };
        match entry {
            Entry::User { text, context } => {
                let session = self.session.read(cx);
                let turn = session.turn_of(ix);
                // A checkpoint: the files the agent changed from here on can go back.
                let restorable = !session.working() && session.checkpoints.changed_since(turn);
                let rewinds = session.can_rewind();
                let group = SharedString::from(format!("user-{ix}"));
                // Yours on the right, in a bubble as wide as the text (as in
                // Claude's apps); what you attached over it; its actions,
                // under it, on hover.
                div()
                    .id(group.clone())
                    .group(group.clone())
                    .mt_3()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_1()
                    .when(!context.is_empty(), |el| {
                        el.child(div().max_w(relative(0.85)).flex().flex_wrap().justify_end().gap_1().children(context.iter().map(|label| chip(label.clone(), Icon::FileText, &theme))))
                    })
                    .child(
                        div()
                            .max_w(relative(0.85))
                            .px(px(14.))
                            .py(px(9.))
                            .rounded(px(16.))
                            .bg(theme.elevated)
                            .text_size(px(self.chat_size))
                            .text_color(theme.text)
                            .whitespace_normal()
                            .child({
                                let content = SharedString::from(text.clone());
                                let styled = gpui::StyledText::new(content.clone());
                                crate::selectable_text::SelectableText::new((ix, 0), content, styled, self.selection.clone(), Vec::new(), theme.selection.into())
                            }),
                    )
                    .child(
                        self.message_actions(ix, &group, text.clone(), cx)
                            .child(
                                icon_button(SharedString::from(format!("edit-{ix}")), Icon::Pencil, theme.text_dim.into(), cx)
                                    .tooltip(move |_, cx| tooltip_view(if rewinds { "Edit: change it and send it again in its place" } else { "Edit: put it in the composer to change and send again" }, cx))
                                    .on_click(cx.listener(move |v, _, window, cx| v.edit_message(turn, text.clone(), window, cx))),
                            )
                            .when(restorable, |el| {
                                el.child(
                                    icon_button(SharedString::from(format!("restore-{ix}")), Icon::Undo, theme.text_dim.into(), cx)
                                        .tooltip(|_, cx| tooltip_view("Restore Files to Here: put back what the agent changed from this message on", cx))
                                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(SessionViewEvent::RestoreTo(turn)))),
                                )
                            }),
                    )
                    .into_any_element()
            }
            Entry::Assistant { text } => {
                let blocks = self.markdown_of(ix, &text, cx);
                let view = cx.entity().downgrade();
                let draw = Draw {
                    zoom: self.chat_size / (theme.ui_font_size + 1.),
                    on_code: Some(Rc::new(move |action, _, cx| match action {
                        CodeAction::Copy { ix, text } => {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                            view.update(cx, |v, cx| {
                                v.copied = Some(ix);
                                cx.notify();
                            })
                            .ok();
                        }
                        CodeAction::Run(command) => {
                            view.update(cx, |_, cx| cx.emit(SessionViewEvent::RunCommand(command))).ok();
                        }
                    })),
                    copied: self.copied,
                    selection: Some((self.selection.clone(), ix)),
                    pieces: Default::default(),
                    wrap_code: true,
                };
                let _ = window;
                // The agent's answer as plain text across the column, its
                // Copy under it on hover.
                let group = SharedString::from(format!("answer-{ix}"));
                // The newest answer can be asked for again (agents that can
                // go back: Kuana's).
                let retry = {
                    let session = self.session.read(cx);
                    let entries = &session.transcript.entries;
                    session.can_rewind()
                        && session.is_connected()
                        && !session.state.busy()
                        && entries.iter().rposition(|e| matches!(e, Entry::Assistant { .. })) == Some(ix)
                        && !entries[ix..].iter().any(|e| matches!(e, Entry::User { .. }))
                };
                div()
                    .id(group.clone())
                    .group(group.clone())
                    .py_1()
                    .flex()
                    .flex_col()
                    .child(markdown_preview::render(&blocks, &draw, cx))
                    .child(self.message_actions(ix, &group, text, cx).when(retry, |el| {
                        el.child(
                            icon_button(SharedString::from(format!("retry-{ix}")), Icon::Refresh, theme.text_dim.into(), cx)
                                .tooltip(|_, cx| tooltip_view("Retry: answer the last message again", cx))
                                .on_click(cx.listener(|v, _, _, cx| v.session.update(cx, |s, cx| s.retry_answer(cx)))),
                        )
                    }))
                    .into_any_element()
            }
            Entry::Thought { text } => {
                let key = format!("thought-{ix}");
                let open = self.is_open(&key, false);
                let toggle = key.clone();
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id(SharedString::from(key))
                            .flex()
                            .items_center()
                            .gap_1()
                            .cursor_pointer()
                            .text_size(px(12. * k))
                            .text_color(theme.text_dim)
                            .hover(|s| s.text_color(theme.text_muted))
                            .child(icon_el(if open { Icon::ChevronDown } else { Icon::ChevronRight }, px(12.), theme.text_dim.into()))
                            .child("Thought process")
                            .on_click(cx.listener(move |v, _, _, cx| v.toggle(toggle.clone(), false, cx))),
                    )
                    .when(open, |el| {
                        el.child(div().mt_1().pl_4().text_size(px(12.5 * k)).italic().text_color(theme.text_muted).whitespace_normal().child(text))
                    })
                    .into_any_element()
            }
            Entry::Tool(tool) => self.render_tool(&tool, true, cx),
            Entry::Plan { items } => div()
                .px_3()
                .py_2()
                .rounded(px(10.))
                .border_1()
                .border_color(theme.border_subtle)
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_size(px(11. * k)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_dim).child("PLAN"))
                .children(items.into_iter().map(|item| {
                    let (icon, color): (Icon, Hsla) = match item.status {
                        Status::Done => (Icon::SquareCheck, theme.success.into()),
                        Status::Running => (Icon::ArrowRight, theme.accent.into()),
                        _ => (Icon::Square, theme.text_dim.into()),
                    };
                    div()
                        .flex()
                        .items_start()
                        .gap_2()
                        .child(div().pt(px(2.)).child(icon_el(icon, px(14.), color)))
                        .child(
                            div()
                                .whitespace_normal()
                                .text_color(if item.status == Status::Done { theme.text_dim } else { theme.text })
                                .when(item.status == Status::Done, |el| el.line_through())
                                .child(item.text),
                        )
                }))
                .into_any_element(),
            Entry::Notice { level, text } => {
                let (icon, color): (Icon, Hsla) = match level {
                    Level::Info => (Icon::CircleAlert, theme.text_dim.into()),
                    Level::Warning => (Icon::TriangleAlert, theme.warning.into()),
                    Level::Error => (Icon::CircleX, theme.error.into()),
                };
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .text_size(px(12.5 * k))
                    .text_color(color)
                    .child(div().pt(px(2.)).child(icon_el(icon, px(14.), color)))
                    .child(div().whitespace_normal().child(text))
                    .into_any_element()
            }
        }
    }

    /// A message's actions, shown while it's hovered: Copy (a check for a
    /// moment after), then what the caller adds.
    fn message_actions(&self, ix: usize, group: &SharedString, text: String, cx: &mut Context<Self>) -> gpui::Div {
        let theme = Theme::get(cx);
        let copied = self.copied_entry == Some(ix);
        div().h(px(24.)).flex().items_center().gap_0p5().invisible().group_hover(group.clone(), |s| s.visible()).when(copied, |el| el.visible()).child(
            icon_button(SharedString::from(format!("copy-message-{ix}")), if copied { Icon::Check } else { Icon::Copy }, theme.text_dim.into(), cx)
                .tooltip(|_, cx| tooltip_view("Copy", cx))
                .on_click(cx.listener(move |v, _, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                    v.copied_entry = Some(ix);
                    cx.notify();
                    v.copied_reset = Some(cx.spawn(async move |v, cx| {
                        cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
                        v.update(cx, |v, cx| {
                            v.copied_entry = None;
                            cx.notify();
                        })
                        .ok();
                    }));
                })),
        )
    }

    fn tool_markdown_of(&self, tool: &str, index: usize, text: &str, cx: &App) -> Rc<Vec<Block>> {
        let key = (tool.to_string(), index);
        if let Some((len, blocks)) = self.tool_markdown.borrow().get(&key)
            && *len == text.len()
        {
            return blocks.clone();
        }
        let base = self.session.read(cx).cwd.clone();
        let mut blocks = markdown_preview::parse(text, Some(&base));
        markdown_preview::for_each_inline(&mut blocks, &mut |inline| link_files(inline, &base));
        let blocks = Rc::new(blocks);
        self.tool_markdown.borrow_mut().insert(key, (text.len(), blocks.clone()));
        blocks
    }

    fn markdown_of(&mut self, ix: usize, text: &str, cx: &App) -> Rc<Vec<Block>> {
        if let Some((len, blocks)) = self.markdown.get(&ix)
            && *len == text.len()
        {
            return blocks.clone();
        }
        let base = self.session.read(cx).cwd.clone();
        let mut blocks = markdown_preview::parse(text, Some(&base));
        // Files the answer names (`src/main.rs:12`, or a link to one) open
        // in the editor when clicked.
        markdown_preview::for_each_inline(&mut blocks, &mut |inline| link_files(inline, &base));
        let blocks = Rc::new(blocks);
        self.markdown.insert(ix, (text.len(), blocks.clone()));
        blocks
    }

    /// A run of finished tool calls, folded into one line until opened.
    fn render_run(&mut self, ix: usize, run: &[usize], cx: &mut Context<Self>) -> AnyElement {
        let k = self.scale;
        let theme = Theme::get(cx).clone();
        let session = self.session.read(cx);
        let key = format!("run-{}", run.first().copied().unwrap_or(ix));
        let open = self.is_open(&key, false);
        let summary = session.transcript.summary(run);
        let tools: Vec<Tool> = run
            .iter()
            .filter_map(|i| match session.transcript.entries.get(*i) {
                Some(Entry::Tool(tool)) => Some(tool.clone()),
                _ => None,
            })
            .collect();
        let toggle = key.clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .id(SharedString::from(key))
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .cursor_pointer()
                    .text_size(px(12.5 * k))
                    .text_color(theme.text_muted)
                    .hover(|s| s.text_color(theme.text))
                    .child(icon_el(if open { Icon::ChevronDown } else { Icon::ChevronRight }, px(12.), theme.text_dim.into()))
                    .child(icon_el(Icon::Check, px(13.), theme.success.into()))
                    .child(summary)
                    .on_click(cx.listener(move |v, _, _, cx| v.toggle(toggle.clone(), false, cx))),
            )
            .when(open, |el| el.child(div().pl_4().flex().flex_col().gap_1().children(tools.iter().map(|t| self.render_tool(t, false, cx)))))
            .into_any_element()
    }

    fn render_tool(&self, tool: &Tool, standalone: bool, cx: &mut Context<Self>) -> AnyElement {
        let k = self.scale;
        let theme = Theme::get(cx).clone();
        let key = format!("tool-{}", tool.id);
        let asking = tool.permission.as_ref().is_some_and(|p| p.open);
        // A command the row doesn't show whole (described, or several lines).
        let command = tool_command(tool);
        let has_body = !tool.content.is_empty() || asking || command.is_some();
        let default_open = asking || tool.content.iter().any(|c| matches!(c, ToolContent::Diff { .. } | ToolContent::Terminal { .. }));
        let open = standalone && has_body && self.is_open(&key, default_open);
        let (status_icon, status_color): (Icon, Hsla) = match tool.status {
            Status::Running | Status::Pending if asking => (Icon::CircleAlert, theme.warning.into()),
            Status::Running | Status::Pending => (Icon::Refresh, theme.accent.into()),
            Status::Done => (kind_icon(tool.kind), theme.text_muted.into()),
            Status::Failed => (Icon::CircleX, theme.error.into()),
        };
        let location = tool.locations.first().cloned();
        let toggle = key.clone();
        let header = div()
            .id(SharedString::from(key))
            .flex()
            .items_center()
            .gap_1p5()
            .min_w_0()
            .text_size(px(12.5 * k))
            .when(has_body && standalone, |el| el.cursor_pointer().on_click(cx.listener(move |v, _, _, cx| v.toggle(toggle.clone(), default_open, cx))))
            .when(has_body && standalone, |el| el.child(icon_el(if open { Icon::ChevronDown } else { Icon::ChevronRight }, px(12.), theme.text_dim.into())))
            .child(div().flex_shrink_0().child(icon_el(status_icon, px(14.), status_color)))
            .child(tool_title(tool, &theme, k))
            .when_some(location, |el, loc| {
                let label = format!("{}{}", loc.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), loc.line.map(|l| format!(":{l}")).unwrap_or_default());
                let (path, line) = (loc.path.clone(), loc.line);
                el.child(
                    div()
                        .id(SharedString::from(format!("loc-{}", tool.id)))
                        .ml_1()
                        .px_1p5()
                        .rounded(px(6.))
                        .text_size(px(11.5 * k))
                        .text_color(theme.text_dim)
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.accent))
                        .child(label)
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(SessionViewEvent::OpenFile { path: path.clone(), line }))),
                )
            });
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(header)
            .when(open, |el| {
                el.child(
                    div()
                        .pl_5()
                        .flex()
                        .flex_col()
                        .gap_1p5()
                        .when_some(command, |el, command| {
                            el.child(
                                div()
                                    .rounded(px(8.))
                                    .bg(theme.elevated)
                                    .px_2()
                                    .py_1p5()
                                    .font_family(theme.mono_font.clone())
                                    .text_size(px(11.5 * k))
                                    .text_color(theme.text)
                                    .whitespace_normal()
                                    .child(format!("$ {command}")),
                            )
                        })
                        .children(tool.content.iter().map(|c| self.render_content(tool, c, cx))),
                )
            })
            .when(open && asking, |el| el.child(self.render_permission(tool, cx)))
            .when(!asking && standalone, |el| {
                let chosen = tool.permission.as_ref().and_then(|p| p.chosen.as_ref().and_then(|c| p.options.iter().find(|o| &o.id == c)));
                el.when_some(chosen, |el, option| {
                    el.child(div().pl_5().text_size(px(11.5 * k)).text_color(theme.text_dim).child(format!("You chose: {}", option.name)))
                })
            })
            .into_any_element()
    }

    fn render_content(&self, tool: &Tool, content: &ToolContent, cx: &mut Context<Self>) -> AnyElement {
        let k = self.scale;
        let theme = Theme::get(cx).clone();
        match content {
            ToolContent::Text { text } if !text.trim_start().starts_with("```") => {
                // Prose (a plan to approve, a search's summary): Markdown,
                // whole while the agent asks about it.
                let asking = tool.permission.as_ref().is_some_and(|p| p.open);
                let lines = text.lines().count();
                let key = format!("all-{}", tool.id);
                let all = asking || lines <= PREVIEW_LINES * 2 || self.is_open(&key, false);
                let shown: String = if all { text.clone() } else { text.lines().take(PREVIEW_LINES).collect::<Vec<_>>().join("\n") };
                let index = tool.content.iter().position(|c| std::ptr::eq(c, content)).unwrap_or(0);
                let blocks = self.tool_markdown_of(&tool.id, index, &shown, cx);
                let draw = Draw { zoom: self.chat_size * 0.95 / (theme.ui_font_size + 1.), on_code: None, copied: None, selection: None, pieces: Default::default(), wrap_code: true };
                div()
                    .text_color(theme.text_muted)
                    .child(markdown_preview::render(&blocks, &draw, cx))
                    .when(!asking && lines > PREVIEW_LINES * 2, |el| {
                        el.child(
                            div()
                                .id(SharedString::from(key.clone()))
                                .cursor_pointer()
                                .text_size(px(11.5 * k))
                                .text_color(theme.text_dim)
                                .hover(|s| s.text_color(theme.accent))
                                .child(if all { "Show less".to_string() } else { format!("⋯ {} more lines", lines - PREVIEW_LINES) })
                                .on_click(cx.listener(move |v, _, _, cx| v.toggle(key.clone(), false, cx))),
                        )
                    })
                    .into_any_element()
            }
            ToolContent::Text { text } => {
                let (text, output) = unfenced(text);
                let lines: Vec<&str> = text.lines().collect();
                let key = format!("all-{}", tool.id);
                let all = self.is_open(&key, false);
                // Output's end matters most (a test run's summary); other text's start.
                let hidden = lines.len().saturating_sub(PREVIEW_LINES);
                let shown = match (all || hidden == 0, output) {
                    (true, _) => lines.join("\n"),
                    (false, true) => lines[hidden..].join("\n"),
                    (false, false) => lines[..PREVIEW_LINES].join("\n"),
                };
                let more = (hidden > 0).then(|| {
                    let label = match (all, output) {
                        (true, _) => "Show less".to_string(),
                        (false, true) => format!("⋯ {hidden} line{} above", if hidden == 1 { "" } else { "s" }),
                        (false, false) => format!("⋯ {hidden} more line{}", if hidden == 1 { "" } else { "s" }),
                    };
                    div()
                        .id(SharedString::from(key.clone()))
                        .cursor_pointer()
                        .text_color(theme.text_dim)
                        .hover(|s| s.text_color(theme.accent))
                        .child(label)
                        .on_click(cx.listener(move |v, _, _, cx| v.toggle(key.clone(), false, cx)))
                });
                let (more_above, more_below) = if output && !all { (more, None) } else { (None, more) };
                div()
                    .font_family(theme.mono_font.clone())
                    .text_size(px(11.5 * k))
                    .text_color(theme.text_muted)
                    .when(output, |el| el.rounded(px(8.)).bg(theme.elevated).px_2().py_1p5())
                    .children(more_above)
                    .child(div().whitespace_normal().child(if output && text.trim().is_empty() { "(no output)".to_string() } else { shown }))
                    .children(more_below)
                    .into_any_element()
            }
            ToolContent::Diff { path, old, new } => {
                let preview = self.diff_preview_of(&tool.id, path, old.as_deref().unwrap_or(""), new);
                let (added, removed, lines) = (preview.0, preview.1, preview.2.clone());
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let (p, o) = (path.clone(), old.clone());
                div()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme.border_subtle)
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .bg(theme.elevated)
                            .text_size(px(12. * k))
                            .child(crate::ui::file_icon(path, px(14.), &theme))
                            .child(div().text_color(theme.text).child(name.clone()))
                            .when(old.is_none(), |el| el.child(div().text_color(theme.git_added).child("new file")))
                            .child(div().text_color(theme.git_added).child(format!("+{added}")))
                            .child(div().text_color(theme.git_deleted).child(format!("−{removed}")))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id(SharedString::from(format!("open-diff-{}-{name}", tool.id)))
                                    .px_1p5()
                                    .rounded(px(6.))
                                    .cursor_pointer()
                                    .text_color(theme.text_muted)
                                    .hover(|s| s.text_color(theme.accent))
                                    .child("Open Diff")
                                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(SessionViewEvent::OpenDiff { path: p.clone(), old: o.clone() }))),
                            ),
                    )
                    .child(
                        div().px_2().py_1().font_family(theme.mono_font.clone()).text_size(px(11.5 * k)).flex().flex_col().children(lines.into_iter().map(|(sign, line)| {
                            let (color, bg): (Hsla, Hsla) = match sign {
                                '+' => (theme.git_added.into(), Hsla::from(theme.git_added).alpha(0.08)),
                                '-' => (theme.git_deleted.into(), Hsla::from(theme.git_deleted).alpha(0.08)),
                                _ => (theme.text_dim.into(), Hsla::transparent_black()),
                            };
                            div().bg(bg).text_color(color).whitespace_normal().child(format!("{sign} {line}"))
                        })),
                    )
                    .into_any_element()
            }
            ToolContent::Terminal { id } => {
                // A terminal let go of long ago (`Terminals::prune`) has ended.
                let (tail, exit) = match self.session.read(cx).terminals.tail(id, PREVIEW_LINES) {
                    Some(shown) => shown,
                    None => ("(output no longer kept)".to_string(), Some(super::terminals::Exit { code: None, signal: None })),
                };
                let status = match exit {
                    None => "running…".to_string(),
                    Some(e) => match (e.code, e.signal) {
                        (Some(c), _) => format!("exit {c}"),
                        (None, Some(_)) => "stopped".to_string(),
                        (None, None) => "ended".to_string(),
                    },
                };
                div()
                    .rounded(px(8.))
                    .bg(theme.elevated)
                    .px_2()
                    .py_1p5()
                    .font_family(theme.mono_font.clone())
                    .text_size(px(11.5 * k))
                    .text_color(theme.text_muted)
                    .child(div().whitespace_normal().child(if tail.is_empty() { "(no output yet)".to_string() } else { tail }))
                    .child(div().mt_1().text_color(theme.text_dim).child(status))
                    .into_any_element()
            }
        }
    }

    fn render_permission(&self, tool: &Tool, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let Some(permission) = &tool.permission else { return div().into_any_element() };
        let first_allow = permission.options.iter().position(|o| o.kind == "allow_once");
        div()
            .pl_5()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1p5()
            .children(permission.options.iter().enumerate().map(|(i, option)| {
                let (tool_id, option_id) = (tool.id.clone(), option.id.clone());
                pill(SharedString::from(format!("perm-{}-{}", tool.id, option.id)), option.name.clone().into(), Some(i) == first_allow, !option.allows(), &theme)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        let (tool_id, option_id) = (tool_id.clone(), option_id.clone());
                        v.session.update(cx, |s, cx| s.answer_permission(&tool_id, &option_id, cx));
                    }))
            }))
            .into_any_element()
    }

    /// After the last entry: what the agent is doing, the sign-in, the queue.
    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let session = self.session.read(cx);
        let mut col = div().flex().flex_col().gap_2().pb_2();
        let status = match &session.state {
            // Starting with nothing sent shows in the composer, not here.
            State::Connecting if session.working() => Some(format!("Starting {}…", session.agent_name)),
            State::Running => Some("Working…".to_string()),
            State::Waiting => Some("Waiting for your answer".to_string()),
            _ => None,
        };
        if let Some(status) = status {
            col = col.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_size(px(12.5 * self.scale))
                    .text_color(theme.text_dim)
                    .child({
                        let spark = div().child(icon_el(Icon::Sparkle, px(13.), theme.accent.into()));
                        // A slow pulse while the agent works (still with reduced effects).
                        if session.state == State::Running && !theme.reduce_effects {
                            spark
                                .with_animation("agent-working", Animation::new(std::time::Duration::from_millis(1600)).repeat().with_easing(pulsating_between(0.35, 1.)), |el, t| {
                                    el.opacity(t)
                                })
                                .into_any_element()
                        } else {
                            spark.into_any_element()
                        }
                    })
                    .child(status),
            );
        }
        if session.state == State::NeedsAuth {
            let methods = session.auth_methods.clone();
            col = col.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1p5()
                    .children(methods.into_iter().map(|m| {
                        let id = m.id.clone();
                        pill(SharedString::from(format!("auth-{}", m.id)), m.name.clone().into(), true, false, &theme).on_click(cx.listener(move |v, _, _, cx| {
                            let id = id.clone();
                            v.session.update(cx, |s, cx| s.sign_in(&id, cx));
                        }))
                    }))
                    .child(pill("auth-retry".into(), "I've signed in".into(), false, false, &theme).on_click(cx.listener(|v, _, _, cx| v.session.update(cx, |s, cx| s.signed_in(cx))))),
            );
        }
        if session.transcript.entries.is_empty() && !session.working() {
            let project = session.folder().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // Ways to start, which fill the composer.
            let starts = [
                (Icon::FolderTree, "Explain how this project is organized"),
                (Icon::GitCompare, "Review my uncommitted changes"),
                (Icon::Bug, "Find and fix a bug in the file I have open"),
                (Icon::Play, "Run the tests and fix what fails"),
            ];
            col = col.child(
                div()
                    .pt_10()
                    .pb_2()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .child(icon_el(Icon::Sparkle, px(30.), theme.accent.into()))
                    .child(div().text_size(px(20.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(format!("What should we work on in {project}?")))
                    .child(div().text_color(theme.text_dim).child(format!("{} reads, edits and runs things here; you approve what matters.", session.agent_name)))
                    .child(div().pt_2().flex().flex_wrap().justify_center().gap_2().children(starts.into_iter().enumerate().map(|(i, (icon, prompt))| {
                        div()
                            .id(("agent-start", i))
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .h(px(30.))
                            .px_3()
                            .rounded(px(15.))
                            .border_1()
                            .border_color(theme.border_subtle)
                            .cursor_pointer()
                            .text_size(px(12.5))
                            .text_color(theme.text_muted)
                            .child(icon_el(icon, px(14.), theme.text_dim.into()))
                            .child(prompt)
                            .hover_fill(Hsla::from(theme.background).alpha(0.), theme.hover)
                            .on_click(cx.listener(move |v, _, window, cx| {
                                v.composer.update(cx, |e, cx| e.set_text(prompt, cx));
                                window.focus(&v.composer.focus_handle(cx));
                            }))
                    }))),
            );
        }
        col.into_any_element()
    }

    // ---- Header, changes, composer -----------------------------------------------------

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let session = self.session.read(cx);
        let usage = session.usage.filter(|(_, size)| *size > 0).map(|(used, size)| (used as f64 / size as f64 * 100.).round() as u64);
        crate::ui::panel_header()
            .px_3()
            .child(icon_el(Icon::Sparkle, px(14.), theme.accent.into()))
            .child(crate::ui::one_line(div().min_w_0().flex_shrink()).font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(session.title.clone()))
            .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child(session.agent_name.clone()))
            .child(div().flex_1())
            .when_some(usage, |el, pct| {
                el.child(
                    div()
                        .id("agent-usage")
                        .flex_shrink_0()
                        .text_size(px(11.5))
                        .text_color(if pct >= 80 { theme.warning } else { theme.text_dim })
                        .child(format!("{pct}%"))
                        .tooltip(move |_, cx| tooltip_view(format!("{pct}% of the model's context used"), cx)),
                )
            })
            .child(
                icon_button("agent-more", Icon::More, theme.text_dim.into(), cx)
                    .on_click(cx.listener(|_, e: &ClickEvent, _, cx| cx.emit(SessionViewEvent::Menu { kind: MenuKind::More, position: e.position() }))),
            )
    }

    /// The files the agent changed, as a cap on the composer's top edge:
    /// inset from its corners, rounded on top only and sitting on its border,
    /// so the two read as one shape (not two boxes with different corners).
    fn render_changes(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::get(cx).clone();
        let count = self.session.read(cx).changes().len();
        if count == 0 {
            return None;
        }
        Some(
            div()
                .mx(px(18.))
                .h(px(30.))
                .pl_2p5()
                .pr_1()
                .flex()
                .items_center()
                .gap_1()
                .rounded_t(px(10.))
                .border_t_1()
                .border_l_1()
                .border_r_1()
                .border_color(theme.border_subtle)
                .bg(theme.elevated)
                .text_size(px(12.))
                .child(icon_el(Icon::GitCompare, px(13.), theme.text_dim.into()))
                .child(crate::ui::one_line(div().min_w_0().pl_0p5()).text_color(theme.text_muted).child(format!("{count} file{} changed", if count == 1 { "" } else { "s" })))
                .child(div().flex_1())
                .child(cap_button("agent-review", "Review", theme.accent.into(), &theme).on_click(cx.listener(|_, _, _, cx| cx.emit(SessionViewEvent::Review))))
                .child(cap_button("agent-keep", "Keep All", theme.text_muted.into(), &theme).on_click(cx.listener(|_, _, _, cx| cx.emit(SessionViewEvent::KeepAll))))
                .child(cap_button("agent-reject", "Reject All", theme.error.into(), &theme).on_click(cx.listener(|_, _, _, cx| cx.emit(SessionViewEvent::RejectAll))))
                .into_any_element(),
        )
    }

    fn render_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let changes = self.render_changes(cx);
        let mention = self.mention(cx);
        // Paths shown relative to the project (a worktree session's own files, to its checkout).
        let (cwd, here) = { let s = self.session.read(cx); (s.folder().to_path_buf(), s.cwd.clone()) };
        let theme = Theme::get(cx).clone();
        let session = self.session.read(cx);
        let busy = session.working();
        let queued = session.queued.len();
        let mode = session.mode_name().map(|name| (mode_icon(session), name));
        let model = model_label(session);
        // Where it works: chosen before the first message (a project in a Git
        // repository), then its worktree's branch.
        let place: Option<(Icon, String, String)> = match (&self.place_busy, &session.worktree) {
            (Some(busy), _) => Some((Icon::GitBranch, busy.to_string(), busy.to_string())),
            // The branch's own name (`kuana/` is every session's).
            (None, Some(wt)) if !wt.removed => {
                Some((Icon::GitBranch, wt.branch.trim_start_matches("kuana/").to_string(), format!("Works in a worktree of its own, on branch {}", wt.branch)))
            }
            (None, None) if self.in_repo && session.transcript.entries.is_empty() => {
                Some((Icon::Folder, "Project folder".to_string(), "Where the agent works: here, or a worktree of its own".to_string()))
            }
            _ => None,
        };
        // Starting before the first prompt: where its controls will be.
        let starting = session.state == State::Connecting && !busy && mode.is_none() && model.is_none();
        let agent_name = session.agent_name.clone();
        let text = self.composer.read(cx).text(cx);
        // The agent's `/` commands while one is being typed: at most 8, the
        // picked one among them.
        let all_commands = self.command_matches(cx);
        let picked_command = self.pick_index(&text, all_commands.len());
        let first_shown = picked_command.saturating_sub(7);
        let commands: Vec<(usize, String, String)> =
            all_commands.into_iter().enumerate().skip(first_shown).take(8).map(|(i, (name, description))| (i, name, description)).collect();
        let picked_mention = self.mention_query(cx).map(|(_, query)| query).map_or(0, |query| self.pick_index(&query, usize::MAX));
        let focused = self.composer.focus_handle(cx).is_focused(window);
        let composer_height = self.composer.read(cx).content_height(cx);
        let dictate_key = crate::ui::binding_label_at(&crate::dictation::Dictate, &self.composer.focus_handle(cx), window);
        div()
            .mx_3()
            .mb_3()
            .flex()
            .flex_col()
            .when_some(mention, |el, (range, found)| {
                el.child(
                    div()
                        .mb_1()
                        .rounded(px(10.))
                        .border_1()
                        .border_color(theme.border_subtle)
                        .bg(theme.elevated)
                        .py_1()
                        .children(found.into_iter().enumerate().map(|(i, path)| {
                            let rel = path.strip_prefix(&cwd).unwrap_or(&path).to_string_lossy().into_owned();
                            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                            let range = range.clone();
                            div()
                                .id(SharedString::from(format!("mention-{i}")))
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_2()
                                .py(px(3.))
                                .cursor_pointer()
                                .when(i == picked_mention, |el| el.bg(theme.hover))
                                .row_hover(theme.hover)
                                .child(crate::ui::file_icon(&path, px(14.), &theme))
                                .child(div().flex_shrink_0().text_color(theme.text).child(name))
                                .child(crate::ui::one_line(div().flex_1().min_w_0()).text_color(theme.text_dim).child(rel))
                                .when(i == picked_mention, |el| el.child(div().flex_shrink_0().text_size(px(11.)).text_color(theme.text_dim).child("Tab")))
                                .on_click(cx.listener(move |v, _, window, cx| v.accept_mention(range.clone(), path.clone(), window, cx)))
                        })),
                )
            })
            .when(!commands.is_empty(), |el| {
                el.child(
                    div()
                        .mb_1()
                        .rounded(px(10.))
                        .border_1()
                        .border_color(theme.border_subtle)
                        .bg(theme.elevated)
                        .py_1()
                        .children(commands.into_iter().map(|(i, name, description)| {
                            let picked = i == picked_command;
                            div()
                                .id(SharedString::from(format!("cmd-{name}")))
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_2()
                                .py(px(3.))
                                .cursor_pointer()
                                .when(picked, |el| el.bg(theme.hover))
                                .row_hover(theme.hover)
                                .child(div().flex_shrink_0().text_color(theme.accent).child(format!("/{name}")))
                                .child(crate::ui::one_line(div().flex_1().min_w_0()).text_color(theme.text_dim).child(description))
                                .when(picked, |el| el.child(div().flex_shrink_0().text_size(px(11.)).text_color(theme.text_dim).child("Tab")))
                                .on_click(cx.listener(move |v, _, window, cx| v.accept_command(&name, window, cx)))
                        })),
                )
            })
            // On the box's top edge, right above it.
            .children(changes)
            .child(
                crate::ui::focus_ring(
                    div()
                        .id("agent-composer")
                        .key_context("AgentComposer")
                        // Before the composer's own paste: a picture on the
                        // clipboard is attached, text goes on to the editor.
                        .capture_action(cx.listener(Self::paste_image))
                        // Dictate here with the project's words as hints.
                        .capture_action(cx.listener(|v, _: &crate::dictation::Dictate, _, cx| {
                            cx.stop_propagation();
                            v.toggle_dictation(cx);
                        }))
                        // ⌘C with text selected in the transcript (and none in
                        // the composer) copies that.
                        .capture_action(cx.listener(Self::copy_selection))
                        // ↑/↓ pick among suggestions while they show, else
                        // bring back earlier prompts into an empty composer.
                        .capture_action(cx.listener(|v, _: &crate::editor::MoveUp, _, cx| {
                            if v.move_pick(-1, cx) || v.recall(true, cx) {
                                cx.stop_propagation();
                            }
                        }))
                        .capture_action(cx.listener(|v, _: &crate::editor::MoveDown, _, cx| {
                            if v.move_pick(1, cx) || v.recall(false, cx) {
                                cx.stop_propagation();
                            }
                        }))
                        .on_action(cx.listener(Self::send))
                        .on_action(cx.listener(Self::accept_suggestion))
                        .on_action(cx.listener(Self::stop))
                        .rounded(px(18.))
                        .border_1()
                        .bg(theme.background)
                        .px_3()
                        .pt_2p5()
                        .pb_2()
                        .flex()
                        .flex_col()
                        .gap_1p5()
                        .when_some(self.editing, |el, turn| {
                            el.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .text_size(px(12.))
                                    .text_color(theme.text_muted)
                                    .child(icon_el(Icon::Pencil, px(12.), theme.accent.into()))
                                    .child(div().flex_1().min_w_0().truncate().child(format!("Editing message {turn}: sending replaces it and everything after")))
                                    .child(
                                        icon_button("agent-stop-editing", Icon::X, theme.text_dim.into(), cx)
                                            .tooltip(|_, cx| tooltip_view("Stop Editing (Esc)", cx))
                                            .on_click(cx.listener(|v, _, _, cx| v.stop_editing(cx))),
                                    ),
                            )
                        })
                        .when_some(self.notice.clone(), |el, notice| el.child(div().text_size(px(12.)).text_color(theme.warning).child(notice)))
                        .when(!self.context.is_empty(), |el| {
                            el.child(div().flex().flex_wrap().gap_1().children(self.context.iter().enumerate().map(|(i, item)| {
                                let icon = if matches!(item, ContextItem::Image { .. }) { Icon::Palette } else { Icon::FileText };
                                // The whole place on hover: names repeat (`__init__.py`).
                                let place = match item {
                                    ContextItem::File { path, .. } | ContextItem::Selection { path, .. } => {
                                        Some(path.strip_prefix(&here).or_else(|_| path.strip_prefix(&cwd)).unwrap_or(path).display().to_string())
                                    }
                                    _ => None,
                                };
                                chip(item.label(), icon, &theme)
                                    .id(SharedString::from(format!("ctx-{i}")))
                                    .when_some(place, |el, place| el.tooltip(move |_, cx| tooltip_view(place.clone(), cx)))
                                    .child(
                                    div()
                                        .id(SharedString::from(format!("ctx-x-{i}")))
                                        .cursor_pointer()
                                        .child(icon_el(Icon::X, px(11.), theme.text_dim.into()))
                                        .on_click(cx.listener(move |v, _, _, cx| {
                                            if i < v.context.len() {
                                                v.context.remove(i);
                                            }
                                            cx.notify();
                                        })),
                                )
                            })))
                        })
                        // As tall as its text, up to a dozen lines; then it scrolls.
                        .child(div().px_0p5().h(px(f32::from(composer_height).clamp(24., 240.))).child(self.composer.clone()))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    icon_button("agent-add-context", Icon::Plus, theme.text_dim.into(), cx)
                                        .flex_shrink_0()
                                        .tooltip(|_, cx| tooltip_view("Add context: a file, the selection, the problems, the Git diff", cx))
                                        .on_click(cx.listener(|_, e: &ClickEvent, _, cx| cx.emit(SessionViewEvent::Menu { kind: MenuKind::AddContext, position: e.position() }))),
                                )
                                .when_some(mode, |el, (icon, name)| {
                                    el.child(
                                        composer_pill("agent-mode", Some(icon), name, &theme)
                                            .tooltip(|_, cx| tooltip_view("What the agent may do without asking", cx))
                                            .on_click(cx.listener(|_, e: &ClickEvent, _, cx| cx.emit(SessionViewEvent::Menu { kind: MenuKind::Mode, position: e.position() }))),
                                    )
                                })
                                .when_some(place, |el, (icon, label, tip)| {
                                    el.child(
                                        // Narrow, it gives way before the mode and the model.
                                        shrinking(composer_pill("agent-place", Some(icon), label, &theme), 8.)
                                            .tooltip(move |_, cx| tooltip_view(tip.clone(), cx))
                                            .on_click(cx.listener(|_, e: &ClickEvent, _, cx| cx.emit(SessionViewEvent::Menu { kind: MenuKind::Place, position: e.position() }))),
                                    )
                                })
                                .when(starting, |el| {
                                    el.child(crate::ui::one_line(div().min_w_0().pl_1()).text_size(px(11.5)).text_color(theme.text_dim).child(format!("Starting {agent_name}…")))
                                })
                                .when(queued > 0, |el| {
                                    el.child(crate::ui::one_line(div().min_w_0().pl_1()).text_size(px(11.5)).text_color(theme.text_dim).child(format!("{queued} queued · sent when the agent is done")))
                                })
                                .child(div().flex_1())
                                .when_some(model, |el, label| {
                                    el.child(
                                        shrinking(composer_pill("agent-model", None, label, &theme), 0.4)
                                            .tooltip(|_, cx| tooltip_view("Model and options", cx))
                                            .on_click(cx.listener(|_, e: &ClickEvent, _, cx| cx.emit(SessionViewEvent::Menu { kind: MenuKind::Model, position: e.position() }))),
                                    )
                                })
                                .child(self.render_mic(&dictate_key, &theme, cx))
                                // Send, which is Stop while the agent works and
                                // nothing's typed (typed text queues).
                                .map(|el| {
                                    if busy && text.trim().is_empty() {
                                        el.child(
                                            round_button("agent-stop", Icon::Stop, true, &theme)
                                                .tooltip(|_, cx| tooltip_view("Stop (Esc)", cx))
                                                .on_click(cx.listener(|v, _, _, cx| v.session.update(cx, |s, cx| s.cancel(cx)))),
                                        )
                                    } else {
                                        let ready = !text.trim().is_empty() || !self.context.is_empty();
                                        el.child(
                                            round_button("agent-send", Icon::ArrowUp, ready, &theme)
                                                .tooltip(|_, cx| tooltip_view("Send (Enter) · Shift+Enter for a new line", cx))
                                                .on_click(cx.listener(|v, _, window, cx| v.send(&SendMessage, window, cx))),
                                        )
                                    }
                                }),
                        ),
                    "agent-composer-ring",
                    focused,
                    &theme,
                ),
            )
    }
}

impl SessionView {
    /// The composer's mic: click to dictate (again to stop), or hold to
    /// talk; while it listens, it's lit, its ring following the voice.
    fn render_mic(&self, key: &Option<String>, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let store = crate::dictation::store(cx);
        let (level, finishing) = store.as_ref().filter(|_| self.dictating).map_or((None, false), |s| (s.read(cx).level(), s.read(cx).finishing()));
        let key = key.as_ref().map(|k| format!(" ({k})")).unwrap_or_default();
        let tip = match (level.is_some(), finishing) {
            (true, true) => "Writing down what you said…".to_string(),
            (true, false) => format!("Stop dictating{key} · Esc takes it back"),
            (false, _) => format!("Dictate{key} · hold to talk"),
        };
        let accent: Hsla = theme.accent.into();
        // Lit while it listens; otherwise a fill only under the pointer.
        let (rest, hover) = match level {
            Some(_) => (Hsla::from(theme.accent_soft), Hsla::from(theme.accent_soft)),
            None => (Hsla::from(theme.background).alpha(0.), Hsla::from(theme.hover)),
        };
        div()
            .id("agent-mic")
            .flex_shrink_0()
            .size(px(28.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .map(|el| match level {
                // Listening: a ring as loud as the voice.
                Some(level) if !finishing => el.border_2().border_color(accent.alpha(0.25 + 0.75 * level)).child(icon_el(Icon::Mic, px(15.), accent)),
                Some(_) => el.child(
                    div()
                        .child(icon_el(Icon::Mic, px(15.), accent))
                        .with_animation("mic-finishing", Animation::new(std::time::Duration::from_millis(900)).repeat().with_easing(pulsating_between(0.35, 1.)), |el, t| el.opacity(t)),
                ),
                None => el.child(icon_el(Icon::Mic, px(15.), theme.text_dim.into())),
            })
            .hover_fill(rest, hover)
            .tooltip(move |_, cx| tooltip_view(tip.clone(), cx))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|v, _, _, cx| {
                cx.stop_propagation();
                v.mic_down(cx);
            }))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(|v, _, _, cx| v.mic_up(cx)))
            .into_any_element()
    }
}

impl Focusable for SessionView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        // The tab's keyboard goes to the composer.
        let _ = &self.focus;
        self.composer.focus_handle(cx)
    }
}

impl Render for SessionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync(cx);
        let theme = Theme::get(cx).clone();
        let transcript = list(self.list.clone(), cx.processor(|this, ix: usize, window, cx| this.render_item(ix, window, cx))).size_full();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .child(self.render_header(cx))
            .child(
                div()
                    .id("agent-transcript")
                    // A click in the transcript gives it the keyboard, so ⌘C
                    // copies what's selected there.
                    .track_focus(&self.focus)
                    .key_context("AgentTranscript")
                    .on_action(cx.listener(|v, _: &crate::editor::Copy, _, cx| {
                        v.copy_transcript(cx);
                    }))
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(transcript)
                    .child(crate::ui::scrollbar("agent-transcript-scrollbar", &self.list, cx))
                    // Scrolled up: a way back to the newest.
                    .when(self.scrolled_up(), |el| {
                        el.child(
                            div().absolute().bottom(px(10.)).left_0().right_0().flex().justify_center().child(
                                div()
                                    .id("agent-to-bottom")
                                    .size(px(30.))
                                    .rounded_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .bg(theme.elevated)
                                    .border_1()
                                    .border_color(theme.border_subtle)
                                    .shadow(crate::ui::soft_shadow(&theme, 1))
                                    .child(icon_el(Icon::ArrowDown, px(16.), theme.text_muted.into()))
                                    .tooltip(|_, cx| tooltip_view("Scroll to the newest", cx))
                                    .on_click(cx.listener(|v, _, _, cx| {
                                        // Following the end again, as a new session does.
                                        v.follow_end();
                                        cx.notify();
                                    })),
                            ),
                        )
                    }),
            )
            .child(self.render_composer(window, cx))
    }
}

fn kind_icon(kind: Kind) -> Icon {
    match kind {
        Kind::Read => Icon::FileText,
        Kind::Edit => Icon::Pencil,
        Kind::Delete => Icon::Trash,
        Kind::Move => Icon::ArrowRight,
        Kind::Search => Icon::Search,
        Kind::Execute => Icon::Terminal,
        Kind::Think => Icon::Sparkle,
        Kind::Fetch => Icon::Link,
        Kind::Other => Icon::Bolt,
    }
}

/// A tool call's row: its title's first line or, for a command the agent
/// described (Claude's Bash has a `description`), the description and then
/// the command in code. Each part is one font, as GPUI's cut with "…"
/// measures with the element's font, and takes room whose width is known
/// before its text is measured (`flex_1`), which the cut needs too.
fn tool_title(tool: &Tool, theme: &Theme, k: f32) -> AnyElement {
    let command = tool.input.as_ref().and_then(|i| i.get("command")).and_then(serde_json::Value::as_str);
    if let (Some(description), Some(command), Kind::Execute) = (tool_description(tool), command, tool.kind) {
        // Agents' descriptions are a few words; a long one is cut here.
        let description: String = if description.chars().count() > 60 { format!("{}…", description.chars().take(59).collect::<String>()) } else { description.to_string() };
        return div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_shrink_0().whitespace_nowrap().text_color(theme.text_muted).child(description))
            .child(crate::ui::one_line(div().flex_1().min_w_0()).font_family(theme.mono_font.clone()).text_size(px(12. * k)).text_color(theme.text).child(first_line(command)))
            .into_any_element();
    }
    crate::ui::one_line(div().flex_1().min_w_0()).text_color(theme.text_muted).child(first_line(&tool.title).replace('`', "")).into_any_element()
}

/// The text's first line, "…" after it when there are more.
fn first_line(text: &str) -> String {
    let text = text.trim();
    match text.split_once('\n') {
        Some((line, _)) => format!("{} …", line.trim_end()),
        None => text.to_string(),
    }
}

/// What the agent said a command is for (one line), when it did.
fn tool_description(tool: &Tool) -> Option<&str> {
    tool.input.as_ref()?.get("description")?.as_str().map(str::trim).filter(|d| !d.is_empty() && !d.contains('\n'))
}

/// A command the row doesn't show whole: the card shows it.
fn tool_command(tool: &Tool) -> Option<String> {
    if tool.kind != Kind::Execute {
        return None;
    }
    let command = tool.input.as_ref()?.get("command")?.as_str()?.trim();
    (tool_description(tool).is_some() || command.contains('\n')).then(|| command.to_string())
}

/// A tool's text without the Markdown fence agents put around a command's
/// output (Claude's "```console … ```"), and whether it had one: output.
fn unfenced(text: &str) -> (&str, bool) {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```")
        && let Some((_, body)) = rest.split_once('\n')
        && let Some(inner) = body.strip_suffix("```")
    {
        return (inner.trim_end_matches('\n'), true);
    }
    (text, false)
}

/// Added and removed line counts, and the changed lines (with a line of
/// context around each run), at most `PREVIEW_LINES`, each with its sign.
type Preview = (usize, usize, Vec<(char, String)>);

impl SessionView {
    fn diff_preview_of(&self, tool: &str, path: &std::path::Path, old: &str, new: &str) -> Rc<Preview> {
        let key = (tool.to_string(), path.to_path_buf());
        let mut cache = self.diff_previews.borrow_mut();
        if let Some((o, n, preview)) = cache.get(&key)
            && (*o, *n) == (old.len(), new.len())
        {
            return preview.clone();
        }
        let preview = Rc::new(diff_preview(old, new));
        cache.insert(key, (old.len(), new.len(), preview.clone()));
        preview
    }
}

fn diff_preview(old: &str, new: &str) -> Preview {
    // A bounded diff: a huge rewrite shows coarser, not slower.
    let diff = similar::TextDiff::configure().timeout(std::time::Duration::from_millis(100)).diff_lines(old, new);
    let (mut added, mut removed) = (0, 0);
    let mut lines = Vec::new();
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    for group in diff.grouped_ops(1) {
        for op in group {
            for change in diff.iter_changes(&op) {
                if lines.len() >= PREVIEW_LINES {
                    return (added, removed, lines);
                }
                let sign = match change.tag() {
                    similar::ChangeTag::Insert => '+',
                    similar::ChangeTag::Delete => '-',
                    similar::ChangeTag::Equal => ' ',
                };
                lines.push((sign, change.value().trim_end_matches(['\n', '\r']).to_string()));
            }
        }
    }
    (added, removed, lines)
}

fn chip(label: String, icon: Icon, theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .px_1p5()
        .h(px(20.))
        .rounded(px(6.))
        .bg(theme.elevated)
        .border_1()
        .border_color(theme.border_subtle)
        .text_size(px(11.5))
        .text_color(theme.text_muted)
        .child(icon_el(icon, px(11.), theme.text_dim.into()))
        .child(label)
}

/// How a link to a file is written: `kuana-file:<path>`, then `#<line>`.
const FILE_LINK: &str = "kuana-file:";

fn file_link_target(url: &str) -> Option<(PathBuf, Option<u32>)> {
    let rest = url.strip_prefix(FILE_LINK)?;
    let (path, line) = match rest.rsplit_once('#') {
        Some((path, line)) => (path, line.parse().ok()),
        None => (rest, None),
    };
    Some((PathBuf::from(path), line))
}

/// A place written as `path`, `path:12`, `path:12:3` or `path#L12`: the path
/// and line.
fn path_and_line(text: &str) -> (&str, Option<u32>) {
    let text = text.trim();
    if let Some((path, line)) = text.rsplit_once("#L")
        && let Ok(line) = line.split('-').next().unwrap_or_default().parse()
    {
        return (path, Some(line));
    }
    let mut parts = text.splitn(3, ':');
    let path = parts.next().unwrap_or_default();
    let line = parts.next().and_then(|l| l.parse().ok());
    if line.is_some() { (path, line) } else { (text, None) }
}

/// Code spans naming a file in the project (and links to one) become links
/// that open it.
fn link_files(inline: &mut Inline, cwd: &std::path::Path) {
    let target = |text: &str| -> Option<String> {
        let (path, line) = path_and_line(text);
        if path.is_empty() || path.contains(char::is_whitespace) || path.contains("://") {
            return None;
        }
        let full = if std::path::Path::new(path).is_absolute() { PathBuf::from(path) } else { cwd.join(path) };
        full.is_file().then(|| format!("{FILE_LINK}{}{}", full.display(), line.map(|l| format!("#{l}")).unwrap_or_default()))
    };
    for (_, url) in &mut inline.links {
        if !url.contains("://") && !url.starts_with("mailto:") && let Some(file) = target(url) {
            *url = file;
        }
    }
    let codes: Vec<Range<usize>> = inline.styles.iter().filter(|(_, s)| *s == markdown_preview::Style::Code).map(|(r, _)| r.clone()).collect();
    for range in codes {
        if inline.links.iter().any(|(r, _)| r.start < range.end && range.start < r.end) {
            continue;
        }
        if let Some(text) = inline.text.get(range.clone())
            && let Some(url) = target(text)
        {
            inline.links.push((range.clone(), url));
            inline.styles.push((range, markdown_preview::Style::Link));
        }
    }
}

/// The composer's round button (Send, Stop): filled in the accent when it
/// does something, faint when not.
fn round_button(id: &'static str, icon: Icon, on: bool, theme: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .size(px(28.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(if on { theme.accent } else { theme.hover })
        .when(on, |el| el.cursor_pointer().hover(|s| s.opacity(0.85)))
        .child(icon_el(icon, px(16.), if on { theme.background.into() } else { theme.text_dim.into() }))
}

fn pill(id: SharedString, label: SharedString, primary: bool, danger: bool, theme: &Theme) -> crate::ui::Button {
    let (rest, hover) = if primary {
        (Hsla::from(theme.accent_soft), Hsla::from(theme.accent).alpha(0.24))
    } else {
        (Hsla::from(theme.hover), Hsla::from(theme.active))
    };
    div()
        .id(id)
        .h(px(24.))
        .px_2p5()
        .flex()
        .items_center()
        .rounded(px(12.))
        .cursor_pointer()
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(if primary {
            theme.accent
        } else if danger {
            theme.error
        } else {
            theme.text
        })
        .child(label)
        .hover_fill(rest, hover)
}

/// A composer control that opens a menu (the mode, the model): its value
/// and a chevron, cut short when the composer is narrow.
/// A quiet text button on the changes cap: its color says what it does; a
/// fill only under the pointer.
fn cap_button(id: &'static str, label: &'static str, color: Hsla, theme: &Theme) -> crate::ui::Button {
    div()
        .id(id)
        .h(px(22.))
        .px_2()
        .flex()
        .items_center()
        .rounded(px(6.))
        .cursor_pointer()
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(label)
        .hover_fill(Hsla::from(theme.elevated).alpha(0.), theme.hover)
}

/// How much an element gives way when its row is too narrow (flex shrink).
fn shrinking<E: gpui::Styled>(mut el: E, factor: f32) -> E {
    el.style().flex_shrink = Some(factor);
    el
}

fn composer_pill(id: &'static str, icon: Option<Icon>, label: String, theme: &Theme) -> crate::ui::Button {
    div()
        .id(id)
        .h(px(24.))
        .px_2()
        .min_w_0()
        .flex()
        .flex_shrink()
        .items_center()
        .gap_1()
        .rounded(px(12.))
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(theme.text_muted)
        .when_some(icon, |el, icon| el.child(div().flex_shrink_0().child(icon_el(icon, px(13.), theme.text_dim.into()))))
        .child(crate::ui::one_line(div().min_w_0()).child(label))
        .child(div().flex_shrink_0().child(icon_el(Icon::ChevronDown, px(11.), theme.text_dim.into())))
        .hover_bg(theme.hover)
}

/// An icon for the permission mode, by what its id says it does (agents
/// name theirs: Claude's `default`, `acceptEdits`, `plan`, `auto`,
/// `bypassPermissions`; Kuana Agent's `ask`, `edit`, `plan`).
fn mode_icon(session: &Session) -> Icon {
    let id = match session.mode_option() {
        Some(option) => option.current.as_str().unwrap_or_default().to_lowercase(),
        None => session.mode.clone().unwrap_or_default().to_lowercase(),
    };
    if id.contains("bypass") || id.contains("dontask") || id.contains("yolo") {
        Icon::ShieldAlert
    } else if id.contains("plan") {
        Icon::FileText
    } else if id.contains("edit") || id.contains("write") {
        Icon::Pencil
    } else if id.contains("auto") {
        Icon::Bolt
    } else {
        Icon::ShieldCheck
    }
}

/// What the composer's model button says: the model, its effort and the
/// switches that are on ("Opus 5.5 · High · Fast mode"); "Options" for an
/// agent with options but no model; None without options.
fn model_label(session: &Session) -> Option<String> {
    let options: Vec<&super::session::ConfigOption> = session.options.iter().filter(|o| o.category.as_deref() != Some("mode") && !o.choices.is_empty()).collect();
    if options.is_empty() {
        return None;
    }
    let of = |category: &str| options.iter().find(|o| o.category.as_deref() == Some(category));
    let mut parts = Vec::new();
    if let Some(model) = of("model") {
        parts.push(model.current_name());
    }
    if let Some(effort) = of("thought_level").filter(|o| o.current.as_str() != Some("default")) {
        parts.push(effort.current_name());
    }
    parts.extend(options.iter().filter(|o| o.switch() == Some(true)).map(|o| o.name.clone()));
    Some(if parts.is_empty() { "Options".into() } else { parts.join(" · ") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_name_a_path_and_a_line() {
        assert_eq!(path_and_line("src/main.rs:12:3"), ("src/main.rs", Some(12)));
        assert_eq!(path_and_line("src/main.rs#L40-L42"), ("src/main.rs", Some(40)));
        assert_eq!(path_and_line("Cargo.toml"), ("Cargo.toml", None));
        assert_eq!(path_and_line("a:b"), ("a:b", None));
        assert_eq!(file_link_target("kuana-file:/p/a.rs#7"), Some((PathBuf::from("/p/a.rs"), Some(7))));
        assert_eq!(file_link_target("https://x"), None);
    }

    #[test]
    fn code_spans_naming_project_files_become_links() {
        let dir = std::env::temp_dir().join(format!("kuana-file-links-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        let text = "See src/main.rs:3 and missing.rs".to_string();
        let mut inline = Inline { styles: vec![(4..17, markdown_preview::Style::Code), (22..32, markdown_preview::Style::Code)], text, links: Vec::new() };
        link_files(&mut inline, &dir);
        assert_eq!(inline.links, vec![(4..17, format!("kuana-file:{}#3", dir.join("src/main.rs").display()))]);
        assert!(inline.styles.contains(&(4..17, markdown_preview::Style::Link)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dropped_files_become_context_or_mentions() {
        let dir = std::env::temp_dir().join(format!("kuana-dropped-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        std::fs::write(dir.join("notes.md"), "# Notes\n").unwrap();
        std::fs::write(dir.join("shot.png"), [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
        std::fs::write(dir.join("data.bin"), [0u8, 159, 146, 150, 0, 0]).unwrap();
        std::fs::write(dir.join("big.png"), vec![0u8; super::super::session::MAX_IMAGE_BYTES + 1]).unwrap();
        let text = Attachment::Context(ContextItem::File { path: dir.join("notes.md"), text: "# Notes\n".into() });
        assert_eq!(attachment(&dir.join("notes.md")), text);
        assert!(matches!(attachment(&dir.join("shot.png")), Attachment::Context(ContextItem::Image { ref name, ref mime, ref data }) if name == "shot.png" && mime == "image/png" && data.len() == 7));
        assert_eq!(attachment(&dir.join("folder")), Attachment::Mention { path: dir.join("folder"), folder: true, why: None });
        assert!(matches!(attachment(&dir.join("data.bin")), Attachment::Mention { folder: false, why: Some(_), .. }));
        assert_eq!(attachment(&dir.join("big.png")), Attachment::Refused("big.png is over 5 MB".into()));
        assert!(matches!(attachment(&dir.join("gone.txt")), Attachment::Refused(_)));
        // A long file is cut, what's left out said.
        let long = mentioned_text(&vec![b'a'; MENTION_BYTES + 10]);
        assert!(long.ends_with("[… 10 more bytes left out]"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn diff_previews_count_and_show_changed_lines() {
        let (added, removed, lines) = diff_preview("a\nb\nc\nd\n", "a\nB\nc\nd\ne\n");
        assert_eq!((added, removed), (2, 1));
        assert!(lines.contains(&('-', "b".to_string())) && lines.contains(&('+', "B".to_string())) && lines.contains(&('+', "e".to_string())));
        let (added, _, lines) = diff_preview("", &"x\n".repeat(100));
        assert_eq!(added, 100);
        assert_eq!(lines.len(), PREVIEW_LINES);
    }
}
