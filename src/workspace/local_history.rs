//! Local History in the workspace (see `crate::local_history`): a file's
//! versions as a Find panel tab, each showing its diff with the file as it
//! is when chosen, and Restore putting one back.

use std::time::SystemTime;

use super::*;
use crate::{
    local_history::{self, Version},
    results_panel::{Leading, Opener, ResultNode},
};

impl Workspace {
    /// Local History of the current file.
    pub(super) fn show_local_history(&mut self, _: &ShowLocalHistory, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_editor(cx).and_then(|e| e.read(cx).doc.read(cx).path.clone()) {
            Some(path) => self.show_local_history_of(path, window, cx),
            None => self.toast(ToastKind::Info, "Open a file to see its Local History", None, cx),
        }
    }

    /// Lists `path`'s versions in the Find panel, by day, the newest first.
    pub(super) fn show_local_history_of(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let read_path = path.clone();
        cx.spawn_in(window, async move |ws, cx| {
            let versions = cx.background_executor().spawn(async move { local_history::versions(&read_path) }).await;
            ws.update_in(cx, |ws, window, cx| {
                if versions.is_empty() {
                    ws.toast(ToastKind::Info, format!("No Local History for {name} yet"), Some("Versions are kept as the file is saved or changes on disk".into()), cx);
                    return;
                }
                let this = cx.entity().downgrade();
                let nodes = history_tree(&versions, SystemTime::now(), |version| {
                    let (this, path, version) = (this.clone(), path.clone(), version.clone());
                    Opener(std::rc::Rc::new(move |focus, window, cx| {
                        this.update(cx, |ws, cx| ws.show_history_diff(path.clone(), &version, focus, window, cx)).ok();
                    }))
                });
                ws.results.update(cx, |p, cx| p.show(format!("Local History of {name}").into(), nodes, Vec::new(), false, cx));
                ws.has_results = true;
                ws.bottom = Some(BottomPanel::Results);
                window.focus(&ws.results.focus_handle(cx));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A version against the file as it is (its open document's text), in
    /// the file's Local History diff tab; `focus`: the diff takes the keyboard.
    fn show_history_diff(&mut self, path: PathBuf, version: &Version, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = version.read() else {
            self.toast(ToastKind::Error, "Couldn't read that version", None, cx);
            return;
        };
        let label = format!("{} · {}", when(version.time), version.kind.label());
        let diff = self.show_diff(crate::diff_view::DiffSource::History { path, label, text: text.into() }, window, cx);
        match diff {
            Some(diff) if focus => window.focus(&diff.focus_handle(cx)),
            _ => window.focus(&self.results.focus_handle(cx)),
        }
    }

    /// Restore: the file's text becomes the version's, as one edit (Undo
    /// takes it back); what it was is kept in Local History first.
    pub(super) fn restore_version(&mut self, path: PathBuf, text: Arc<str>, window: &mut Window, cx: &mut Context<Self>) {
        self.open_path(path.clone(), window, cx);
        let Some(editor) = self.tab_for(&path, cx) else {
            self.toast(ToastKind::Error, "Couldn't open the file to restore it", None, cx);
            return;
        };
        let current = editor.read(cx).doc.read(cx).rope().clone();
        let kept = path.clone();
        cx.background_executor().spawn(async move { local_history::record_rope(&kept, &current, local_history::Kind::BeforeRestore) }).detach();
        editor.update(cx, |e, cx| e.replace_text(&text, cx));
        self.toast(ToastKind::Success, "Restored the version from Local History", Some("Undo takes it back".into()), cx);
    }
}

/// The versions grouped by day ("Today", "Yesterday", a date), each with
/// its time, why it was kept and how long ago; `open` shows one.
fn history_tree(versions: &[Version], now: SystemTime, open: impl Fn(&Version) -> Opener) -> Vec<ResultNode> {
    let mut days: Vec<ResultNode> = Vec::new();
    for version in versions {
        let day = day_label(version.time, now);
        if days.last().is_none_or(|d| d.label.as_ref() != day) {
            days.push(ResultNode {
                leading: Leading::Icon(Icon::History),
                label: day.into(),
                highlight: None,
                detail: None,
                target: None,
                open: None,
                children: Vec::new(),
                loader: None,
                expanded: true,
            });
        }
        let node = ResultNode {
            leading: Leading::Blank,
            label: clock(version.time).into(),
            highlight: None,
            detail: Some(format!("{} · {}", version.kind.label(), when(version.time)).into()),
            target: None,
            open: Some(open(version)),
            children: Vec::new(),
            loader: None,
            expanded: false,
        };
        days.last_mut().expect("pushed above").children.push(node);
    }
    days
}

/// Local time as (days since the epoch, seconds into the day).
fn local(time: SystemTime) -> (i64, i64) {
    let secs = time.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64) + local_offset();
    (secs.div_euclid(86_400), secs.rem_euclid(86_400))
}

/// The local time zone's offset from UTC, in seconds.
fn local_offset() -> i64 {
    #[cfg(unix)]
    {
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { !libc::localtime_r(&now, &mut tm).is_null() } {
            return tm.tm_gmtoff;
        }
    }
    0
}

/// "14:32:05".
fn clock(time: SystemTime) -> String {
    let (_, secs) = local(time);
    format!("{:02}:{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60)
}

/// "Today", "Yesterday", else the date ("2026-10-12").
fn day_label(time: SystemTime, now: SystemTime) -> String {
    let (day, today) = (local(time).0, local(now).0);
    match today - day {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => {
            // Days since the epoch to a civil date (Howard Hinnant's algorithm).
            let z = day + 719_468;
            let era = z.div_euclid(146_097);
            let doe = z - era * 146_097;
            let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = yoe + era * 400 + i64::from(m <= 2);
            format!("{y:04}-{m:02}-{d:02}")
        }
    }
}

/// "5 minutes ago".
fn when(time: SystemTime) -> String {
    crate::git::relative_time(time.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn times_read_naturally() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(day_label(now, now), "Today");
        assert_eq!(day_label(now - Duration::from_secs(86_400), now), "Yesterday");
        let label = day_label(now - Duration::from_secs(10 * 86_400), now);
        assert!(label.starts_with("2026-12-") || label.starts_with("2027-01-"), "{label}");
        assert_eq!(clock(now).len(), 8);
    }
}
