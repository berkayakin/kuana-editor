//! Remote projects over SSH (`plans/remote-dev.md`): Connect to SSH Host…
//! (hosts from `~/.ssh/config`, recent remote projects, any `user@host`)
//! connects in the background (`kuana_core::remote::ssh`), then asks for a
//! folder there and opens it like a local one: its path is under the host's
//! mount (`/kuana-remote/<host>/…`), so the rest of Kuana works as usual.
//! A lost connection is noticed (`watch_connection`) and offered again.

use std::time::Duration;

use kuana_core::remote::ssh;

use super::*;
use crate::{
    input_dialog::{InputDialog, InputDialogEvent},
    picker::{Choice, Picker, PickerEvent, PickerMode},
};

/// How often a remote project's connection is looked at.
const CHECK_EVERY: Duration = Duration::from_secs(2);
/// The picker's "type a host" entry.
const OTHER: &str = "\u{0}other";

impl Workspace {
    pub(super) fn connect_to_host(&mut self, _: &ConnectToHost, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let mut choices = Vec::new();
        // Remote projects opened before, newest first.
        for path in SessionState::load().recent_projects.into_iter().filter(|p| ssh::host_of(p).is_some()) {
            choices.push(Choice {
                id: path.to_string_lossy().into_owned(),
                file: None,
                label: crate::clone_dialog::display_path(&path).into(),
                detail: Some("Recent project".into()),
                icon: Some(Icon::Folder.path().into()),
            });
        }
        let home = crate::clone_dialog::home_dir().unwrap_or_default();
        for host in ssh::config_hosts(&home) {
            choices.push(Choice { id: format!("host:{host}"), file: None, label: host.into(), detail: Some("~/.ssh/config".into()), icon: Some(Icon::Terminal.path().into()) });
        }
        choices.push(Choice { id: OTHER.into(), file: None, label: "Other Host…".into(), detail: Some("user@host".into()), icon: None });
        let picker = cx.new(|cx| {
            let mut picker = Picker::new(PickerMode::Choices, None, Arc::new(Vec::new()), Vec::new(), Arc::new(Vec::new()), window, cx);
            picker.set_choices("Connect to SSH Host".into(), "Host or project", choices, cx);
            picker.set_title_icon(Icon::Terminal, cx);
            picker
        });
        let subscription = cx.subscribe_in(&picker, window, |ws, _, event: &PickerEvent, window, cx| match event {
            PickerEvent::Chosen(id) => {
                let id = id.clone();
                ws.close_modal(window, cx);
                if id == OTHER {
                    ws.ask_for_host(window, cx);
                } else if let Some(host) = id.strip_prefix("host:") {
                    ws.connect_host(host.to_string(), None, window, cx);
                } else {
                    ws.open_project(PathBuf::from(id), OpenIn::Auto, window, cx);
                }
            }
            PickerEvent::Dismissed => ws.close_modal(window, cx),
            _ => {}
        });
        self.modals.open(Modal::Picker(picker), subscription);
        cx.notify();
    }

    fn ask_for_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = cx.new(|cx| InputDialog::new("Connect to SSH Host", Icon::Terminal, "user@host, or a Host from ~/.ssh/config", "", None, window, cx));
        let subscription = cx.subscribe_in(&dialog, window, |ws, _, event: &InputDialogEvent, window, cx| {
            let host = match event {
                InputDialogEvent::Confirm(text) => text.trim().to_string(),
                InputDialogEvent::Dismissed => String::new(),
            };
            ws.close_modal(window, cx);
            if !host.is_empty() {
                ws.connect_host(host, None, window, cx);
            }
        });
        self.modals.open(Modal::Input(dialog), subscription);
        cx.notify();
    }

    /// Connects to `host` in the background; then opens `then` (a project
    /// there), or asks which folder to open.
    pub(super) fn connect_host(&mut self, host: String, then: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if self.connecting.as_deref() == Some(host.as_str()) {
            return;
        }
        self.connecting = Some(host.clone());
        self.toast(ToastKind::Info, format!("Connecting to {host}…"), None, cx);
        let (progress_tx, mut progress_rx) = futures::channel::mpsc::unbounded::<String>();
        let name = host.clone();
        let connecting = cx.background_executor().spawn(async move {
            let options = ssh::Options {
                program: std::env::var("KUANA_SSH").unwrap_or_else(|_| "ssh".into()),
                control_dir: crate::theme::config_dir().map(|d| d.join("ssh")).unwrap_or_else(std::env::temp_dir),
                askpass: crate::askpass::env().unwrap_or_default().into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
                server_binary: &crate::updater::server_binary,
                progress: &|step| {
                    let _ = progress_tx.unbounded_send(step.to_string());
                },
            };
            ssh::connect(&name, &options)
        });
        cx.spawn_in(window, async move |this, cx| {
            // Steps worth telling (installing the server takes a while).
            let shown = this.clone();
            let mut telling = cx.spawn(async move |cx| {
                use futures::StreamExt as _;
                while let Some(step) = progress_rx.next().await {
                    if step.starts_with("Installing") {
                        shown.update(cx, |ws, cx| ws.toast(ToastKind::Info, step, None, cx)).ok();
                    }
                }
            });
            let result = connecting.await;
            drop(std::mem::replace(&mut telling, Task::ready(())));
            this.update_in(cx, |ws, window, cx| {
                ws.connecting = None;
                match result {
                    // Back after a lost connection: the project is read again.
                    Ok(_) if ws.reconnecting.take().is_some() => ws.reconnected(cx),
                    Ok(connected) => match then {
                        Some(path) => ws.open_project(path, OpenIn::Auto, window, cx),
                        None => ws.ask_for_remote_folder(connected, window, cx),
                    },
                    Err(err) => {
                        ws.reconnecting = None;
                        ws.toast(ToastKind::Error, format!("Couldn't connect to {host}"), Some(err), cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Asks which folder on a connected host to open (`~/…` is the home's).
    fn ask_for_remote_folder(&mut self, connected: ssh::Connected, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let hint = format!("A folder on {}", connected.name);
        let dialog = cx.new(|cx| InputDialog::new(format!("Open Folder on {}", connected.name), Icon::Folder, hint, "~/", None, window, cx));
        let subscription = cx.subscribe_in(&dialog, window, move |ws, dialog, event: &InputDialogEvent, window, cx| {
            let typed = match event {
                InputDialogEvent::Confirm(text) => text.trim().to_string(),
                InputDialogEvent::Dismissed => {
                    ws.close_modal(window, cx);
                    return;
                }
            };
            let host_path = match typed.strip_prefix('~') {
                Some(rest) => format!("{}/{}", connected.home.trim_end_matches('/'), rest.trim_start_matches('/')),
                None if typed.starts_with('/') => typed.clone(),
                None => format!("{}/{typed}", connected.home.trim_end_matches('/')),
            };
            let path = crate::fs::local_path(&connected.root, &host_path);
            // Looked at there: a round trip, short for a dialog's answer.
            if !path.is_dir_on_host() {
                dialog.update(cx, |d, cx| d.set_error(format!("No folder {host_path} on {}", connected.name), cx));
                return;
            }
            ws.close_modal(window, cx);
            ws.open_project(path, OpenIn::Auto, window, cx);
        });
        self.modals.open(Modal::Input(dialog), subscription);
        cx.notify();
    }

    /// For a remote project: notices its connection going, and offers it again.
    pub(super) fn watch_connection(&mut self, cx: &mut Context<Self>) {
        self.connection_task = None;
        let Some(host) = self.root.as_deref().and_then(ssh::host_of) else { return };
        self.connection_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CHECK_EVERY).await;
                if ssh::connected(&host).is_some() {
                    continue;
                }
                this.update(cx, |ws, cx| {
                    ws.toast_with_button(ToastKind::Error, format!("The connection to {host} was lost"), Some("Files there can't be read or saved until it's back.".into()), Some(ToastButton::Reconnect), cx);
                })
                .ok();
                break;
            }
        }));
    }

    /// Connects the project's host again; once back, everything that was
    /// read from it is read again.
    pub(super) fn reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        let Some(host) = ssh::host_of(&root) else { return };
        ssh::disconnect(&host);
        // `connect_host` would ask for a folder: this one comes back instead.
        self.reconnecting = Some(root);
        self.connect_host(host, None, window, cx);
    }

    /// After a reconnect: the tree, Git, the watcher and open files again.
    pub(super) fn reconnected(&mut self, cx: &mut Context<Self>) {
        self.start_watching(cx);
        self.tree.update(cx, |t, cx| t.relist_all(cx));
        self.refresh_file_index(cx);
        self.refresh_git(cx);
        self.reload_documents(|_| true, cx);
        self.watch_connection(cx);
        cx.notify();
    }
}
