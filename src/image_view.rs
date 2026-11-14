//! An image file in a pane: the picture, scaled down to fit, with its
//! format, dimensions and size underneath. It holds no text, so nothing is
//! edited or written; Save As copies the file (see `pane_item::Saving`).

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use gpui::{App, Context, FocusHandle, Focusable, IntoElement, ParentElement, Render, Styled, Window, div, prelude::*, px};

use crate::{preview::ImageInfo, theme::Theme};

pub struct ImageView {
    path: PathBuf,
    /// Read when the view is made and again when the file changes.
    info: ImageInfo,
    /// The file's modification time when `info` was read.
    mtime: Option<SystemTime>,
    focus_handle: FocusHandle,
}

impl ImageView {
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        Self { info: ImageInfo::read(&path), mtime: mtime(&path), path, focus_handle: cx.focus_handle() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file was renamed or moved (Save As, the file tree).
    pub fn set_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.path = path;
        self.reread(cx);
    }

    /// Reads the details again if another program changed the file.
    pub fn reload_if_changed(&mut self, cx: &mut Context<Self>) {
        if mtime(&self.path) != self.mtime {
            self.reread(cx);
        }
    }

    fn reread(&mut self, cx: &mut Context<Self>) {
        self.info = ImageInfo::read(&self.path);
        self.mtime = mtime(&self.path);
        cx.notify();
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    crate::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Focusable for ImageView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ImageView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx);
        div()
            .id("image")
            .key_context("ImagePreview")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p_6()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(gpui::img(self.path.clone()).size_full().object_fit(gpui::ObjectFit::ScaleDown)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .h(px(30.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_t_1()
                    .border_color(theme.border_subtle)
                    .text_size(px(12.))
                    .text_color(theme.text_dim)
                    .child(self.info.label()),
            )
    }
}
