//! "Clone Repository" (JetBrains' Get from Version Control): a URL, a parent
//! folder and options. The clone runs here, with live progress, so a failure
//! can be fixed and retried without retyping anything.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, IntoElement,
    PathPromptOptions, Render, Subscription, Task, Window, div, prelude::*, px, relative,
};

use crate::{
    assets::Icon,
    editor::{Cancel, Editor, EditorEvent, Indent, Newline, Outdent},
    git::{self, CloneJob},
    theme::Theme,
    ui::{icon_el, input_box, modal_panel, text_button, tooltip_view},
};

pub enum CloneDialogEvent {
    Cloned(PathBuf),
    Dismissed,
}

pub struct CloneDialog {
    url: Entity<Editor>,
    folder: Entity<Editor>,
    shallow: bool,
    error: Option<String>,
    job: Option<Arc<CloneJob>>,
    _task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloneDialogEvent> for CloneDialog {}

impl CloneDialog {
    pub fn new(parent: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let url = cx.new(|cx| Editor::single_line("https://github.com/owner/repo.git", cx));
        let folder = cx.new(|cx| Editor::single_line("Parent folder", cx));
        set_field(&folder, &display_path(&parent), cx);
        // Like JetBrains, start from a repository URL on the clipboard.
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
            && looks_like_git_url(text.trim())
        {
            let text = text.trim().to_string();
            url.update(cx, |e, cx| {
                e.set_text(&text, cx);
                e.select_range(0..text.len(), cx);
            });
        }
        window.focus(&url.focus_handle(cx));
        // Clear a stale error as soon as either field changes.
        let on_edit = |this: &mut Self, _: Entity<Editor>, _: &EditorEvent, cx: &mut Context<Self>| {
            this.error = None;
            cx.notify();
        };
        let subscriptions = vec![cx.subscribe(&url, on_edit), cx.subscribe(&folder, on_edit)];
        Self { url, folder, shallow: false, error: None, job: None, _task: None, _subscriptions: subscriptions }
    }

    /// While cloning, clicking outside the dialog doesn't close it.
    pub fn is_busy(&self) -> bool {
        self.job.is_some()
    }

    fn url_text(&self, cx: &App) -> String {
        self.url.read(cx).text(cx).trim().to_string()
    }

    fn parent_dir(&self, cx: &App) -> PathBuf {
        expand_home(self.folder.read(cx).text(cx).trim())
    }

    /// Where the clone will land, or why it can't start.
    fn destination(&self, cx: &App) -> Result<PathBuf, String> {
        let url = self.url_text(cx);
        if url.is_empty() {
            return Err("Enter a repository URL".into());
        }
        let name = git::repo_name_from_url(&url).ok_or("Can't tell the folder name from this URL")?;
        let parent = self.parent_dir(cx);
        if parent.as_os_str().is_empty() {
            return Err("Choose a folder to clone into".into());
        }
        let dest = parent.join(&name);
        if dest.exists() && std::fs::read_dir(&dest).map_or(true, |mut d| d.next().is_some()) {
            return Err(format!("{} already exists and isn't empty", display_path(&dest)));
        }
        Ok(dest)
    }

    fn clone_repo(&mut self, cx: &mut Context<Self>) {
        if self.job.is_some() {
            return;
        }
        let dest = match self.destination(cx) {
            Ok(dest) => dest,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        let parent = self.parent_dir(cx);
        if let Err(e) = std::fs::create_dir_all(&parent) {
            self.error = Some(format!("Couldn't create {}: {e}", display_path(&parent)));
            cx.notify();
            return;
        }
        let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let url = self.url_text(cx);
        let job = match git::asking(|| git::start_clone(&parent, &url, &name, self.shallow)) {
            Ok(job) => Arc::new(job),
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        self.error = None;
        self.job = Some(job.clone());
        self._task = Some(cx.spawn(async move |this, cx| {
            let waiter = job.clone();
            let result = cx.background_executor().spawn(async move { waiter.wait() });
            // Repaint the progress line while git works.
            let ticker = {
                let this = this.clone();
                let cx = cx.clone();
                async move {
                    loop {
                        cx.background_executor().timer(Duration::from_millis(120)).await;
                        if this.update(&mut cx.clone(), |_, cx| cx.notify()).is_err() {
                            break;
                        }
                    }
                }
            };
            let result = futures_select(result, ticker).await;
            this.update(cx, |this, cx| {
                this.job = None;
                match result {
                    Ok(path) => cx.emit(CloneDialogEvent::Cloned(path)),
                    Err(e) if e == "Cancelled" => {}
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn cancel_or_dismiss(&mut self, cx: &mut Context<Self>) {
        match &self.job {
            Some(job) => job.cancel(),
            None => cx.emit(CloneDialogEvent::Dismissed),
        }
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Clone Here".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await
                && let Some(path) = paths.pop()
            {
                this.update_in(cx, |this, window, cx| {
                    set_field(&this.folder, &display_path(&path), cx);
                    window.focus(&this.url.focus_handle(cx));
                })
                .ok();
            }
        })
        .detach();
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.clone_repo(cx);
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel_or_dismiss(cx);
    }

    /// Tab and Shift-Tab move between the two fields.
    fn on_tab(&mut self, _: &Indent, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_field(window, cx);
    }

    fn on_shift_tab(&mut self, _: &Outdent, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_field(window, cx);
    }

    fn switch_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = if self.url.focus_handle(cx).is_focused(window) { &self.folder } else { &self.url };
        window.focus(&target.focus_handle(cx));
    }
}

impl Focusable for CloneDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.url.focus_handle(cx)
    }
}

impl Render for CloneDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let busy = self.job.is_some();
        let label = |text: &'static str| {
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text_muted)
                .child(text)
        };
        let (hint, hint_color): (String, Hsla) = match (&self.error, self.destination(cx)) {
            (Some(e), _) => (e.clone(), theme.error.into()),
            (None, Ok(dest)) => (format!("Clones into {}", display_path(&dest)), theme.text_dim.into()),
            (None, Err(_)) => ("Paste an HTTPS or SSH URL. Private repositories need an SSH key or a credential helper.".into(), theme.text_dim.into()),
        };
        let progress = self.job.as_ref().map(|j| j.progress.lock().unwrap().clone());
        let percent = progress.as_deref().and_then(progress_percent);

        modal_panel(cx)
            .key_context("CloneDialog")
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_shift_tab))
            .w(px(540.))
            .child(
                // Header
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .pt_4()
                    .pb_3()
                    .child(
                        div()
                            .size(px(34.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(9.))
                            .bg(theme.accent_soft)
                            .child(icon_el(Icon::Download, px(17.), theme.accent.into())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("Clone Repository"))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(theme.text_dim)
                                    .child("Copy a Git repository to this computer and open it"),
                            ),
                    ),
            )
            .child(
                // Fields
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .px_4()
                    .pb_3()
                    .child(label("Repository URL"))
                    .child(div().flex().child(input_box(&self.url, Some(Icon::Link), window, cx)))
                    .child(div().h(px(4.)))
                    .child(label("Directory"))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(input_box(&self.folder, Some(Icon::Folder), window, cx))
                            .child(
                                text_button("clone-browse", "Browse…", false, cx)
                                    .h(px(30.))
                                    .on_click(cx.listener(|this, _, window, cx| this.browse(window, cx))),
                            ),
                    )
                    .child(
                        div()
                            .min_h(px(32.))
                            .pt_0p5()
                            .text_size(px(12.))
                            .text_color(hint_color)
                            .line_clamp(2)
                            .child(hint),
                    )
                    .child(
                        div()
                            .id("clone-shallow")
                            .flex()
                            .items_center()
                            .gap_2()
                            .w_full()
                            .cursor_pointer()
                            .text_color(theme.text_muted)
                            .hover(|s| s.text_color(theme.text))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.shallow = !this.shallow;
                                cx.notify();
                            }))
                            .tooltip(|_, cx| tooltip_view("git clone --depth 1: faster, but without history", cx))
                            .child(icon_el(
                                if self.shallow { Icon::SquareCheck } else { Icon::Square },
                                px(15.),
                                if self.shallow { theme.accent.into() } else { theme.text_dim.into() },
                            ))
                            .child("Shallow clone (latest commit only)"),
                    ),
            )
            .when_some(progress, |el, line| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1p5()
                        .mx_4()
                        .mb_3()
                        .p_2p5()
                        .rounded(px(8.))
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border_subtle)
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .text_size(px(12.))
                                .child(div().text_color(theme.text).font_weight(FontWeight::MEDIUM).child("Cloning…"))
                                .child(div().flex_1().min_w_0().truncate().text_color(theme.text_dim).child(line)),
                        )
                        .child(
                            div().h(px(4.)).w_full().rounded_full().bg(theme.hover).child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .bg(theme.accent)
                                    .w(relative(percent.unwrap_or(4.) / 100.)),
                            ),
                        ),
                )
            })
            .child(
                // Footer
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .child(
                        text_button("clone-cancel", if busy { "Stop" } else { "Cancel" }, false, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_or_dismiss(cx))),
                    )
                    .child(
                        text_button("clone-go", if busy { "Cloning…" } else { "Clone" }, true, cx)
                            .when(busy || self.destination(cx).is_err(), |el| el.opacity(0.5))
                            .on_click(cx.listener(|this, _, _, cx| this.clone_repo(cx))),
                    ),
            )
    }
}

/// Runs `main` to completion while `side` (an endless loop) ticks alongside.
async fn futures_select<T>(main: impl Future<Output = T>, side: impl Future<Output = ()>) -> T {
    use std::pin::pin;
    use std::task::Poll;
    let mut main = pin!(main);
    let mut side = pin!(side);
    std::future::poll_fn(|cx| {
        if let Poll::Ready(v) = main.as_mut().poll(cx) {
            return Poll::Ready(v);
        }
        let _ = side.as_mut().poll(cx);
        Poll::Pending
    })
    .await
}

/// Replaces a field's text, leaving the caret at the end rather than all selected.
fn set_field(field: &Entity<Editor>, text: &str, cx: &mut App) {
    field.update(cx, |e, cx| {
        e.set_text(text, cx);
        e.select_range(text.len()..text.len(), cx);
    });
}

fn looks_like_git_url(text: &str) -> bool {
    !text.contains(char::is_whitespace)
        && (text.starts_with("https://") || text.starts_with("http://") || text.starts_with("ssh://") || text.starts_with("git@"))
        && (text.ends_with(".git") || ["github.com", "gitlab.com", "bitbucket.org", "codeberg.org"].iter().any(|h| text.contains(h)))
}

/// "Receiving objects:  45% (450/1000)" → 45.
fn progress_percent(line: &str) -> Option<f32> {
    let pct = line.find('%')?;
    let start = line[..pct].rfind(|c: char| !c.is_ascii_digit()).map_or(0, |i| i + 1);
    line[start..pct].parse().ok()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// Shows paths under the home folder as `~/…`.
pub fn display_path(path: &Path) -> String {
    match home_dir().and_then(|h| path.strip_prefix(&h).ok().map(Path::to_path_buf)) {
        Some(rel) if rel.as_os_str().is_empty() => "~".into(),
        Some(rel) => format!("~{}{}", std::path::MAIN_SEPARATOR, rel.display()),
        None => path.display().to_string(),
    }
}

/// `~/…` as the path under the home folder.
pub fn expand_home(text: &str) -> PathBuf {
    match (text.strip_prefix('~'), home_dir()) {
        (Some(rest), Some(home)) => home.join(rest.trim_start_matches(['/', '\\'])),
        _ => PathBuf::from(text),
    }
}

/// A sensible default for where to clone: next to the open project, else home.
pub fn default_parent(root: Option<&Path>) -> PathBuf {
    root.and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(home_dir)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent() {
        assert_eq!(progress_percent("Receiving objects:  45% (450/1000), 1 MiB"), Some(45.));
        assert_eq!(progress_percent("Cloning into 'x'..."), None);
    }

    #[test]
    fn urls() {
        assert!(looks_like_git_url("git@github.com:a/b.git"));
        assert!(looks_like_git_url("https://github.com/a/b"));
        assert!(!looks_like_git_url("hello world"));
    }
}
