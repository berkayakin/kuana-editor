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

/// The transcript's column, for reading comfort in a wide pane.
const COLUMN: f32 = 780.;
/// Lines of a terminal's output or a diff shown in a tool call.
const PREVIEW_LINES: usize = 14;

#[derive(Clone, Debug, PartialEq)]
pub enum MenuKind {
    Mode,
    Option(String),
    AddContext,
    More,
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
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SessionViewEvent> for SessionView {}

impl SessionView {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> SessionView {
        let placeholder = format!("Ask {} anything · @ for a file, / for commands", session.read(cx).agent_name);
        let composer = cx.new(|cx| Editor::multi_line(placeholder, cx));
        let subscriptions = vec![
            cx.observe(&session, |view, _, cx| {
                view.sync(cx);
                cx.notify();
            }),
            cx.subscribe(&session, |_, _, _: &SessionEvent, cx| cx.notify()),
            cx.observe(&composer, |_, _, cx| cx.notify()),
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
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
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
        let (range, query) = self.mention_query(cx)?;
        let matched = self.mentions.as_ref().is_some_and(|(q, _)| *q == query);
        let matching = self.mention_task.as_ref().is_some_and(|(q, _)| *q == query);
        if !matched && !matching {
            let (files, cwd, wanted) = (self.files.clone(), self.session.read(cx).cwd.clone(), query.clone());
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
        let (range, query) = self.mention_query(cx)?;
        let found = match &self.mentions {
            Some((q, found)) if *q == query => found.clone(),
            _ => super::mentions::matching(&self.files, &self.session.read(cx).cwd, &query),
        };
        found.into_iter().next().map(|path| (range, path))
    }

    /// A suggestion taken: `@path` in the text, the file in the context.
    fn accept_mention(&mut self, range: Range<usize>, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.session.read(cx).cwd.clone();
        let rel = path.strip_prefix(&cwd).unwrap_or(&path).to_string_lossy().into_owned();
        let text = self.composer.read(cx).text(cx);
        let (start, end) = (range.start.min(text.len()), range.end.min(text.len()));
        let inserted = format!("@{rel} ");
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
            let mut text = crate::encoding::decode(&bytes[..bytes.len().min(MENTION_BYTES)], None).0;
            if bytes.len() > MENTION_BYTES {
                text.push_str(&format!("\n[… {} more bytes left out]", bytes.len() - MENTION_BYTES));
            }
            view.update(cx, |v, cx| v.add_context(ContextItem::File { path, text }, cx)).ok();
        })
        .detach();
        window.focus(&self.composer.focus_handle(cx));
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
        if let Some((range, first)) = self.current_mention(cx) {
            self.accept_mention(range, first, window, cx);
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

    pub fn composer(&self) -> &Entity<Editor> {
        &self.composer
    }

    pub fn add_context(&mut self, item: ContextItem, cx: &mut Context<Self>) {
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
        let text = self.composer.read(cx).text(cx);
        if text.trim().is_empty() && self.context.is_empty() {
            return;
        }
        let context = std::mem::take(&mut self.context);
        self.composer.update(cx, |e, cx| e.set_text("", cx));
        if let Some(turn) = self.editing.take() {
            cx.emit(SessionViewEvent::Resend { turn, text: text.trim().to_string(), context });
            cx.notify();
            return;
        }
        self.session.update(cx, |s, cx| s.send(text.trim().to_string(), context, cx));
        // The newest message shows.
        self.list.scroll_to_reveal_item(self.list.item_count().saturating_sub(1));
    }

    fn stop(&mut self, _: &StopAgent, _: &mut Window, cx: &mut Context<Self>) {
        if self.session.read(cx).state.busy() {
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
        let theme = Theme::get(cx).clone();
        let Some(entry) = self.session.read(cx).transcript.entries.get(ix).cloned() else { return div().into_any_element() };
        match entry {
            Entry::User { text, context } => {
                let session = self.session.read(cx);
                let turn = session.turn_of(ix);
                // A checkpoint: the files the agent changed from here on can go back.
                let restorable = !session.state.busy() && session.checkpoints.changed_since(turn);
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
                    zoom: 0.92,
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
                            .text_size(px(12.))
                            .text_color(theme.text_dim)
                            .hover(|s| s.text_color(theme.text_muted))
                            .child(icon_el(if open { Icon::ChevronDown } else { Icon::ChevronRight }, px(12.), theme.text_dim.into()))
                            .child("Thought process")
                            .on_click(cx.listener(move |v, _, _, cx| v.toggle(toggle.clone(), false, cx))),
                    )
                    .when(open, |el| {
                        el.child(div().mt_1().pl_4().text_size(px(12.5)).italic().text_color(theme.text_muted).whitespace_normal().child(text))
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
                .child(div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.text_dim).child("PLAN"))
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
                    .text_size(px(12.5))
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
                    .text_size(px(12.5))
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
        let theme = Theme::get(cx).clone();
        let key = format!("tool-{}", tool.id);
        let asking = tool.permission.as_ref().is_some_and(|p| p.open);
        let has_body = !tool.content.is_empty() || asking;
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
            .text_size(px(12.5))
            .when(has_body && standalone, |el| el.cursor_pointer().on_click(cx.listener(move |v, _, _, cx| v.toggle(toggle.clone(), default_open, cx))))
            .when(has_body && standalone, |el| el.child(icon_el(if open { Icon::ChevronDown } else { Icon::ChevronRight }, px(12.), theme.text_dim.into())))
            .child(icon_el(status_icon, px(14.), status_color))
            .child(div().min_w_0().flex().flex_wrap().items_baseline().children(title_spans(&tool.title, &theme)))
            .when_some(location, |el, loc| {
                let label = format!("{}{}", loc.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), loc.line.map(|l| format!(":{l}")).unwrap_or_default());
                let (path, line) = (loc.path.clone(), loc.line);
                el.child(
                    div()
                        .id(SharedString::from(format!("loc-{}", tool.id)))
                        .ml_1()
                        .px_1p5()
                        .rounded(px(6.))
                        .text_size(px(11.5))
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
            .when(open, |el| el.child(div().pl_5().flex().flex_col().gap_1p5().children(tool.content.iter().map(|c| self.render_content(tool, c, cx)))))
            .when(open && asking, |el| el.child(self.render_permission(tool, cx)))
            .when(!asking && standalone, |el| {
                let chosen = tool.permission.as_ref().and_then(|p| p.chosen.as_ref().and_then(|c| p.options.iter().find(|o| &o.id == c)));
                el.when_some(chosen, |el, option| {
                    el.child(div().pl_5().text_size(px(11.5)).text_color(theme.text_dim).child(format!("You chose: {}", option.name)))
                })
            })
            .into_any_element()
    }

    fn render_content(&self, tool: &Tool, content: &ToolContent, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        match content {
            ToolContent::Text { text } => {
                let shown: String = text.lines().take(PREVIEW_LINES).collect::<Vec<_>>().join("\n");
                div().text_size(px(12.)).text_color(theme.text_muted).font_family(theme.mono_font.clone()).whitespace_normal().child(shown).into_any_element()
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
                            .text_size(px(12.))
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
                        div().px_2().py_1().font_family(theme.mono_font.clone()).text_size(px(11.5)).flex().flex_col().children(lines.into_iter().map(|(sign, line)| {
                            let (color, bg): (Hsla, Hsla) = match sign {
                                '+' => (theme.git_added.into(), Hsla::from(theme.git_added).alpha(0.08)),
                                '-' => (theme.git_deleted.into(), Hsla::from(theme.git_deleted).alpha(0.08)),
                                _ => (theme.text_dim.into(), Hsla::transparent_black()),
                            };
                            div().bg(bg).text_color(color).whitespace_nowrap().overflow_hidden().child(format!("{sign} {line}"))
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
                    .text_size(px(11.5))
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
            State::Connecting => Some(format!("Starting {}…", session.agent_name)),
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
                    .text_size(px(12.5))
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
        if session.transcript.entries.is_empty() && !session.state.busy() {
            let project = session.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
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
        let mode = session.mode.as_ref().and_then(|m| session.modes.iter().find(|x| &x.id == m)).map(|m| m.name.clone());
        let options: Vec<(String, String)> = session
            .options
            .iter()
            .filter(|o| !o.choices.is_empty())
            .map(|o| {
                let current = o.current.as_str().unwrap_or_default();
                let name = o.choices.iter().find(|(v, _)| v == current).map_or_else(|| current.to_string(), |(_, n)| n.clone());
                (o.id.clone(), name)
            })
            .collect();
        let usage = session.usage.filter(|(_, size)| *size > 0).map(|(used, size)| (used as f64 / size as f64 * 100.).round() as u64);
        crate::ui::panel_header()
            .px_3()
            .child(icon_el(Icon::Sparkle, px(14.), theme.accent.into()))
            .child(div().min_w_0().flex_shrink().truncate().font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child(session.title.clone()))
            .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child(session.agent_name.clone()))
            .child(div().flex_1())
            .when_some(mode, |el, mode| {
                el.child(menu_pill("agent-mode", mode, &theme).on_click(cx.listener(|_, e: &ClickEvent, _, cx| cx.emit(SessionViewEvent::Menu { kind: MenuKind::Mode, position: e.position() }))))
            })
            .children(options.into_iter().map(|(id, name)| {
                let kind = MenuKind::Option(id.clone());
                menu_pill(SharedString::from(format!("agent-opt-{id}")), name, &theme)
                    .on_click(cx.listener(move |_, e: &ClickEvent, _, cx| cx.emit(SessionViewEvent::Menu { kind: kind.clone(), position: e.position() })))
            }))
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

    fn render_changes(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::get(cx).clone();
        let count = self.session.read(cx).changes().len();
        if count == 0 {
            return None;
        }
        Some(
            div()
                .flex()
                .items_center()
                .gap_1p5()
                .px_3()
                .py_1()
                .mx_3()
                .mb_1()
                .rounded(px(10.))
                .bg(theme.elevated)
                .text_size(px(12.5))
                .child(icon_el(Icon::GitCompare, px(14.), theme.text_muted.into()))
                .child(div().text_color(theme.text).child(format!("{count} file{} changed", if count == 1 { "" } else { "s" })))
                .child(div().flex_1())
                .child(pill("agent-review".into(), "Review".into(), true, false, &theme).on_click(cx.listener(|_, _, _, cx| cx.emit(SessionViewEvent::Review))))
                .child(pill("agent-keep".into(), "Keep All".into(), false, false, &theme).on_click(cx.listener(|_, _, _, cx| cx.emit(SessionViewEvent::KeepAll))))
                .child(pill("agent-reject".into(), "Reject All".into(), false, true, &theme).on_click(cx.listener(|_, _, _, cx| cx.emit(SessionViewEvent::RejectAll))))
                .into_any_element(),
        )
    }

    fn render_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mention = self.mention(cx);
        let cwd = self.session.read(cx).cwd.clone();
        let theme = Theme::get(cx).clone();
        let session = self.session.read(cx);
        let busy = session.state.busy();
        let queued = session.queued.len();
        let text = self.composer.read(cx).text(cx);
        // The agent's `/` commands while one is being typed.
        let commands: Vec<(String, String)> = match text.strip_prefix('/') {
            Some(typed) if !typed.contains(char::is_whitespace) => {
                session.commands.iter().filter(|(name, _)| name.starts_with(typed)).take(6).cloned().collect()
            }
            _ => Vec::new(),
        };
        let focused = self.composer.focus_handle(cx).is_focused(window);
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
                                .when(i == 0, |el| el.bg(theme.hover))
                                .row_hover(theme.hover)
                                .child(crate::ui::file_icon(&path, px(14.), &theme))
                                .child(div().text_color(theme.text).child(name))
                                .child(div().min_w_0().truncate().text_color(theme.text_dim).child(rel))
                                .when(i == 0, |el| el.child(div().flex_1()).child(div().text_size(px(11.)).text_color(theme.text_dim).child("Tab")))
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
                        .children(commands.into_iter().map(|(name, description)| {
                            let insert = format!("/{name} ");
                            div()
                                .id(SharedString::from(format!("cmd-{name}")))
                                .flex()
                                .gap_2()
                                .px_2()
                                .py(px(3.))
                                .cursor_pointer()
                                .row_hover(theme.hover)
                                .child(div().text_color(theme.accent).child(format!("/{name}")))
                                .child(div().min_w_0().truncate().text_color(theme.text_dim).child(description))
                                .on_click(cx.listener(move |v, _, window, cx| {
                                    let insert = insert.clone();
                                    v.composer.update(cx, |e, cx| e.set_text(&insert, cx));
                                    window.focus(&v.composer.focus_handle(cx));
                                }))
                        })),
                )
            })
            .child(
                crate::ui::focus_ring(
                    div()
                        .id("agent-composer")
                        .key_context("AgentComposer")
                        // Before the composer's own paste: a picture on the
                        // clipboard is attached, text goes on to the editor.
                        .capture_action(cx.listener(Self::paste_image))
                        // ⌘C with text selected in the transcript (and none in
                        // the composer) copies that.
                        .capture_action(cx.listener(Self::copy_selection))
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
                                chip(item.label(), icon, &theme).child(
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
                        .child(div().px_0p5().min_h(px(24.)).max_h(px(240.)).child(self.composer.clone()))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    icon_button("agent-add-context", Icon::Plus, theme.text_dim.into(), cx)
                                        .tooltip(|_, cx| tooltip_view("Add context: a file, the selection, the problems, the Git diff", cx))
                                        .on_click(cx.listener(|_, e: &ClickEvent, _, cx| cx.emit(SessionViewEvent::Menu { kind: MenuKind::AddContext, position: e.position() }))),
                                )
                                .when(queued > 0, |el| {
                                    el.child(div().text_size(px(11.5)).text_color(theme.text_dim).child(format!("{queued} queued · sent when the agent is done")))
                                })
                                .child(div().flex_1())
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
                    .when(self.list.logical_scroll_top().item_ix + 1 < self.list.item_count(), |el| {
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
                                        let count = v.list.item_count();
                                        v.list.reset(count);
                                        cx.notify();
                                    })),
                            ),
                        )
                    }),
            )
            .children(self.render_changes(cx))
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

/// A tool title with its `code` parts in the code font.
fn title_spans(title: &str, theme: &Theme) -> Vec<AnyElement> {
    title
        .split('`')
        .enumerate()
        .filter(|(_, s)| !s.is_empty())
        .map(|(i, s)| {
            if i % 2 == 1 {
                div().px(px(3.)).font_family(theme.mono_font.clone()).text_size(px(12.)).text_color(theme.text).child(s.to_string()).into_any_element()
            } else {
                div().text_color(theme.text_muted).child(s.to_string()).into_any_element()
            }
        })
        .collect()
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

/// A rounded button with a label that isn't static (an agent's option names).
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
    // The text's runs are laid out in order.
    inline.styles.sort_by_key(|(range, _)| range.start);
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

/// A header control that opens a menu: its current value and a chevron.
fn menu_pill(id: impl Into<SharedString>, label: String, theme: &Theme) -> crate::ui::Button {
    div()
        .id(gpui::ElementId::Name(id.into()))
        .h(px(24.))
        .px_2()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_1()
        .rounded(px(8.))
        .cursor_pointer()
        .text_size(px(12.))
        .text_color(theme.text_muted)
        .child(label)
        .child(icon_el(Icon::ChevronDown, px(11.), theme.text_dim.into()))
        .hover_bg(theme.hover)
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
    fn diff_previews_count_and_show_changed_lines() {
        let (added, removed, lines) = diff_preview("a\nb\nc\nd\n", "a\nB\nc\nd\ne\n");
        assert_eq!((added, removed), (2, 1));
        assert!(lines.contains(&('-', "b".to_string())) && lines.contains(&('+', "B".to_string())) && lines.contains(&('+', "e".to_string())));
        let (added, _, lines) = diff_preview("", &"x\n".repeat(100));
        assert_eq!(added, 100);
        assert_eq!(lines.len(), PREVIEW_LINES);
    }
}
