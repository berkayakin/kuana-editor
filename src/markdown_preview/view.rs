//! The Markdown preview as a pane item: it shows a document that editors
//! have open and follows it as it changes (it observes the document entity,
//! so edits from any editor of it, reloads and an unloaded tab being read
//! all redraw it). The parse runs in the background, once per document
//! version (see `editor::derived`).

use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use gpui::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render, ScrollHandle,
    ScrollWheelEvent, Styled, Subscription, Task, Window, div, prelude::*, px,
};

use crate::{
    assets::Icon,
    document::Document,
    editor::derived::Derived,
    theme::Theme,
    ui::{HoverFill, icon_el, tooltip_view},
};

use super::{Block, CodeAction, Draw, parse, render};

/// How long Copy shows its check.
const COPIED_FOR: Duration = Duration::from_millis(1500);

/// For the workspace.
pub enum PreviewEvent {
    /// A code block's Run: its commands, for a Run tab.
    Run(String),
}

impl EventEmitter<PreviewEvent> for MarkdownPreview {}

pub struct MarkdownPreview {
    doc: Entity<Document>,
    /// Blocks for a (document id, version, path): a tab read from disk is a
    /// new document in the same entity, so the id is part of the key.
    blocks: Derived<(u64, u64, Option<PathBuf>), Vec<Block>>,
    scroll: ScrollHandle,
    /// The text size, relative to the UI font.
    zoom: f32,
    /// ⌘-scrolling not yet turned into a zoom step (trackpads send small deltas).
    zoom_rest: f32,
    /// The code block whose Copy shows a check, and the timer that ends it.
    copied: Option<(usize, Task<()>)>,
    focus_handle: FocusHandle,
    _observe: Subscription,
}

impl MarkdownPreview {
    pub fn new(doc: Entity<Document>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&doc, |_, _, cx| cx.notify());
        Self {
            doc,
            blocks: Derived::new(),
            scroll: ScrollHandle::new(),
            zoom: 1.,
            zoom_rest: 0.,
            copied: None,
            focus_handle: cx.focus_handle(),
            _observe: observe,
        }
    }

    /// Another preview of the same document (a split), at the same zoom.
    pub fn split_of(other: &Entity<MarkdownPreview>, cx: &mut Context<Self>) -> Self {
        let (doc, zoom) = {
            let other = other.read(cx);
            (other.doc.clone(), other.zoom)
        };
        Self { zoom, ..Self::new(doc, cx) }
    }

    pub fn document(&self) -> &Entity<Document> {
        &self.doc
    }

    pub fn path(&self, cx: &App) -> Option<PathBuf> {
        self.doc.read(cx).path.clone()
    }

    /// "Preview README.md", as the tab and window title say it.
    pub fn title(&self, cx: &App) -> String {
        format!("Preview {}", self.doc.read(cx).file_name())
    }

    /// The blocks to draw: the last parse, maybe of an older version, with
    /// the current version's parse scheduled.
    fn blocks(&mut self, cx: &mut Context<Self>) -> Arc<Vec<Block>> {
        let doc = self.doc.read(cx);
        let key = (doc.id(), doc.version(), doc.path.clone());
        let rope = doc.rope().clone();
        let base = doc.path.as_deref().and_then(Path::parent).map(Path::to_path_buf);
        self.blocks.schedule(key, move || parse(&rope.to_string(), base.as_deref()), |p| &mut p.blocks, cx);
        self.blocks.last()
    }

    fn code_action(&mut self, action: CodeAction, cx: &mut Context<Self>) {
        match action {
            CodeAction::Copy { ix, text } => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                let reset = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(COPIED_FOR).await;
                    this.update(cx, |p, cx| {
                        p.copied = None;
                        cx.notify();
                    })
                    .ok();
                });
                self.copied = Some((ix, reset));
                cx.notify();
            }
            CodeAction::Run(command) => cx.emit(PreviewEvent::Run(command)),
        }
    }

    fn zoom_by(&mut self, delta: f32, cx: &mut Context<Self>) {
        let zoom = ((self.zoom + delta) * 10.).round() / 10.;
        self.zoom = zoom.clamp(0.5, 3.);
        cx.notify();
    }

    /// The − 100% + bar at the top right; the percentage resets.
    fn zoom_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let step = |id: &'static str, icon: Icon, delta: f32, cx: &mut Context<Self>| {
            div()
                .id(id)
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .cursor_pointer()
                .hover_bg(theme.hover)
                .on_click(cx.listener(move |p, _, _, cx| p.zoom_by(delta, cx)))
                .child(icon_el(icon, px(12.), theme.text_muted.into()))
        };
        div()
            .absolute()
            .top(px(6.))
            .right(px(10.))
            .flex()
            .items_center()
            .p(px(2.))
            .rounded(px(7.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.elevated)
            .text_size(px(11.5))
            .text_color(theme.text_muted)
            .child(step("md-zoom-out", Icon::Minus, -0.1, cx))
            .child(
                div()
                    .id("md-zoom-reset")
                    .min_w(px(40.))
                    .flex()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.text))
                    .tooltip(|_, cx| tooltip_view("Reset Zoom", cx))
                    .on_click(cx.listener(|p, _, _, cx| p.zoom_by(1. - p.zoom, cx)))
                    .child(format!("{}%", (self.zoom * 100.).round())),
            )
            .child(step("md-zoom-in", Icon::Plus, 0.1, cx))
    }
}

impl Focusable for MarkdownPreview {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MarkdownPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let blocks = self.blocks(cx);
        let theme = Theme::get(cx).clone();
        let this = cx.entity().downgrade();
        let draw = Draw {
            zoom: self.zoom,
            on_code: Some(Rc::new(move |action, _: &mut Window, cx: &mut App| {
                this.update(cx, |p, cx| p.code_action(action, cx)).ok();
            })),
            copied: self.copied.as_ref().map(|(ix, _)| *ix),
        };
        div()
            .id("markdown-preview-item")
            .key_context("MarkdownPreview")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .bg(theme.background)
            .child(
                div()
                    .id("markdown-preview")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .px_6()
                    .py_5()
                    // ⌘/Ctrl + wheel zooms, like a browser.
                    .on_scroll_wheel(cx.listener(|p, event: &ScrollWheelEvent, _, cx| {
                        if event.modifiers.secondary() {
                            cx.stop_propagation();
                            p.zoom_rest += f32::from(event.delta.pixel_delta(px(20.)).y);
                            // About one wheel notch per step.
                            while p.zoom_rest.abs() >= 20. {
                                let up = p.zoom_rest > 0.;
                                p.zoom_rest -= if up { 20. } else { -20. };
                                p.zoom_by(if up { 0.1 } else { -0.1 }, cx);
                            }
                        }
                    }))
                    .child(render(&blocks, &draw, cx)),
            )
            .child(crate::ui::scrollbar("markdown-preview-scrollbar", &self.scroll, cx))
            .child(self.zoom_bar(cx))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui::{AppContext, TestAppContext};

    use super::*;
    use crate::editor::{Editor, Unloaded};

    fn headings(blocks: &[Block]) -> Vec<String> {
        blocks
            .iter()
            .filter_map(|b| match b {
                Block::Heading(_, inline) => Some(inline.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[gpui::test]
    fn follows_its_document(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("kuana-md-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("README.md");
        std::fs::write(&path, "# One\n").unwrap();
        let unloaded = Unloaded { cursor: 0, top_line: None, unsaved: None, folds: Vec::new(), encoding: None };
        let editor = cx.new(|cx| Editor::new_unloaded(Some(path.clone()), unloaded, cx));
        let doc = editor.read_with(cx, |e, _| e.doc.clone());
        let preview = cx.new(|cx| MarkdownPreview::new(doc, cx));
        let shown = |cx: &mut TestAppContext| {
            preview.update(cx, |p, cx| p.blocks(cx));
            cx.executor().advance_clock(Duration::from_millis(500));
            cx.run_until_parked();
            preview.update(cx, |p, cx| headings(&p.blocks(cx)))
        };
        // Nothing until the tab reads its file: then the file's text, though
        // the stand-in was version 0 too.
        assert!(shown(cx).is_empty());
        editor.update(cx, |e, cx| e.load(cx)).unwrap();
        assert_eq!(shown(cx), ["One"]);
        // An edit in the editor is a new version.
        editor.update(cx, |e, cx| e.replace_ranges(vec![(2..5, "Two".into())], cx));
        assert_eq!(shown(cx), ["Two"]);
        preview.read_with(cx, |p, cx| assert_eq!(p.title(cx), "Preview README.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
