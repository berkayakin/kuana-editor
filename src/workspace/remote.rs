//! Remote projects (`plans/remote-dev.md`): Connect to SSH Host… (hosts
//! from `~/.ssh/config`, recent remote projects, any `user@host`), Connect
//! to WSL…, Attach to Running Container… and Open in Dev Container connect
//! in the background (`kuana_core::remote::connection`), then open a folder
//! there like a local one: its path is under the machine's mount
//! (`/kuana-remote/<name>/…`), so the rest of Kuana works as usual. A lost
//! connection is noticed (`watch_connection`) and offered again.

use std::time::Duration;

use kuana_core::remote::{
    connection::{self, Transport},
    container, ssh, wsl,
};

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
        for path in SessionState::load().recent_projects.into_iter().filter(|p| connection::host_of(p).is_some()) {
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

    /// Connects to the machine mounted as `host` (`Transport::name`) in the
    /// background; then opens `then` (a project there, and where), or asks
    /// which folder to open.
    pub(super) fn connect_host(&mut self, host: String, then: Option<(PathBuf, OpenIn)>, window: &mut Window, cx: &mut Context<Self>) {
        if self.connecting.as_deref() == Some(host.as_str()) {
            return;
        }
        let transport = Transport::from_name(&host);
        let label = transport.label();
        self.connecting = Some(host.clone());
        self.toast(ToastKind::Info, format!("Connecting to {label}…"), None, cx);
        let (progress_tx, mut progress_rx) = futures::channel::mpsc::unbounded::<String>();
        let connecting = cx.background_executor().spawn(async move {
            let options = connection::Options {
                program: program_for(&transport),
                control_dir: crate::theme::config_dir().map(|d| d.join("ssh")).unwrap_or_else(std::env::temp_dir),
                askpass: crate::askpass::env().unwrap_or_default().into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
                server_binary: &crate::updater::server_binary,
                progress: &|step| {
                    let _ = progress_tx.unbounded_send(step.to_string());
                },
            };
            connection::connect(&transport, &options)
        });
        cx.spawn_in(window, async move |this, cx| {
            // Steps worth telling (installing the server takes a while).
            let shown = this.clone();
            let mut telling = cx.spawn(async move |cx| {
                use futures::StreamExt as _;
                while let Some(step) = progress_rx.next().await {
                    if step.starts_with("Installing") || step.starts_with("Starting ") && !step.starts_with("Starting kuana-server") {
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
                        Some((path, target)) => ws.open_project(path, target, window, cx),
                        None => ws.ask_for_remote_folder(connected, window, cx),
                    },
                    Err(err) => {
                        ws.reconnecting = None;
                        ws.toast(ToastKind::Error, format!("Couldn't connect to {label}"), Some(err), cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Asks which folder on a connected host to open (`~/…` is the home's).
    fn ask_for_remote_folder(&mut self, connected: connection::Connected, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let label = connected.transport.label();
        let hint = format!("A folder on {label}");
        let dialog = cx.new(|cx| InputDialog::new(format!("Open Folder on {label}"), Icon::Folder, hint, "~/", None, window, cx));
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
                dialog.update(cx, |d, cx| d.set_error(format!("No folder {host_path} on {label}"), cx));
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
        let Some(host) = self.root.as_deref().and_then(connection::host_of) else { return };
        self.connection_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CHECK_EVERY).await;
                if connection::connected(&host).is_some() {
                    continue;
                }
                let label = Transport::from_name(&host).label();
                this.update(cx, |ws, cx| {
                    ws.toast_with_button(ToastKind::Error, format!("The connection to {label} was lost"), Some("Files there can't be read or saved until it's back.".into()), Some(ToastButton::Reconnect), cx);
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
        let Some(host) = connection::host_of(&root) else { return };
        connection::disconnect(&host);
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

    /// Connect to WSL… (Windows): a picker of the installed distributions.
    pub(super) fn connect_to_wsl(&mut self, _: &ConnectToWsl, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let listing = cx.background_executor().spawn(async move { wsl::distros() });
        cx.spawn_in(window, async move |this, cx| {
            let listed = listing.await;
            this.update_in(cx, |ws, window, cx| match listed {
                Ok(distros) if distros.is_empty() => ws.toast(ToastKind::Info, "No WSL distributions are installed", Some("wsl --install sets one up.".into()), cx),
                Ok(distros) => {
                    let choices = distros
                        .into_iter()
                        .enumerate()
                        .map(|(i, d)| Choice { id: Transport::Wsl(d.clone()).name(), file: None, label: d.into(), detail: (i == 0).then(|| "Default".into()), icon: Some(Icon::Terminal.path().into()) })
                        .collect();
                    ws.pick_machine("Connect to WSL", "Distribution", choices, window, cx);
                }
                Err(err) => ws.toast(ToastKind::Error, "Couldn't list WSL distributions", Some(err), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Attach to Running Container…: a picker of `docker ps`.
    pub(super) fn attach_to_container(&mut self, _: &AttachToContainer, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let listing = cx.background_executor().spawn(async move { container::running(&docker_program()) });
        cx.spawn_in(window, async move |this, cx| {
            let listed = listing.await;
            this.update_in(cx, |ws, window, cx| match listed {
                Ok(running) if running.is_empty() => ws.toast(ToastKind::Info, "No containers are running", None, cx),
                Ok(running) => {
                    let choices = running
                        .into_iter()
                        .map(|c| Choice {
                            id: Transport::Container { name: c.name.clone(), user: None }.name(),
                            file: None,
                            label: c.name.into(),
                            detail: Some(format!("{} · {}", c.image, c.id).into()),
                            icon: Some(Icon::Terminal.path().into()),
                        })
                        .collect();
                    ws.pick_machine("Attach to Running Container", "Container", choices, window, cx);
                }
                Err(err) => ws.toast(ToastKind::Error, "Couldn't list containers", Some(err), cx),
            })
            .ok();
        })
        .detach();
    }

    /// A picker of machines (`Transport::name`s) to connect to.
    fn pick_machine(&mut self, title: &str, placeholder: &str, choices: Vec<Choice>, window: &mut Window, cx: &mut Context<Self>) {
        let (title, placeholder) = (title.to_string(), placeholder.to_string());
        let picker = cx.new(|cx| {
            let mut picker = Picker::new(PickerMode::Choices, None, Arc::new(Vec::new()), Vec::new(), Arc::new(Vec::new()), window, cx);
            picker.set_choices(title.into(), &placeholder, choices, cx);
            picker.set_title_icon(Icon::Terminal, cx);
            picker
        });
        let subscription = cx.subscribe_in(&picker, window, |ws, _, event: &PickerEvent, window, cx| match event {
            PickerEvent::Chosen(name) => {
                let name = name.clone();
                ws.close_modal(window, cx);
                ws.connect_host(name, None, window, cx);
            }
            PickerEvent::Dismissed => ws.close_modal(window, cx),
            _ => {}
        });
        self.modals.open(Modal::Picker(picker), subscription);
        cx.notify();
    }

    /// Open in Dev Container: brings up the project's dev container (made
    /// the first time), connects to it and opens the project there.
    pub(super) fn open_in_dev_container(&mut self, _: &OpenInDevContainer, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone().filter(|r| !crate::fs::is_remote(r)) else {
            self.toast(ToastKind::Info, "Open a local folder with a dev container configuration first", None, cx);
            return;
        };
        if container::config_file(&root).is_none() {
            self.toast(ToastKind::Info, "This project has no dev container configuration", Some(".devcontainer/devcontainer.json describes one.".into()), cx);
            return;
        }
        if self.connecting.is_some() {
            return;
        }
        // Its commands and image come from the project: only for a trusted one.
        if !crate::trust::is_trusted(&root) {
            self.toast(ToastKind::Error, "Trust this project to open it in its dev container", Some("Its configuration runs commands and builds images.".into()), cx);
            return;
        }
        self.connecting = Some(String::new());
        self.toast(ToastKind::Info, "Starting the dev container…", None, cx);
        let (progress_tx, mut progress_rx) = futures::channel::mpsc::unbounded::<String>();
        let dir = root.clone();
        let starting = cx.background_executor().spawn(async move {
            let options = container::UpOptions {
                docker: docker_program(),
                cli: crate::lsp::servers::which("devcontainer"),
                progress: &|step| {
                    let _ = progress_tx.unbounded_send(step.to_string());
                },
            };
            container::up(&dir, &options)
        });
        cx.spawn_in(window, async move |this, cx| {
            let shown = this.clone();
            let mut telling = cx.spawn(async move |cx| {
                use futures::StreamExt as _;
                while let Some(step) = progress_rx.next().await {
                    shown.update(cx, |ws, cx| ws.toast(ToastKind::Info, step, None, cx)).ok();
                }
            });
            let result = starting.await;
            drop(std::mem::replace(&mut telling, Task::ready(())));
            this.update_in(cx, |ws, window, cx| {
                ws.connecting = None;
                match result {
                    Ok(up) => {
                        for warning in &up.warnings {
                            ws.toast(ToastKind::Info, "The dev container was set up in part", Some(warning.clone()), cx);
                        }
                        let name = Transport::Container { name: up.container, user: up.user }.name();
                        let project = crate::fs::local_path(&crate::fs::mount_root(&name), &up.workspace);
                        // The window's project reopens there.
                        ws.connect_host(name, Some((project, OpenIn::ThisWindow)), window, cx);
                    }
                    Err(err) => ws.toast(ToastKind::Error, "Couldn't start the dev container", Some(err), cx),
                }
            })
            .ok();
        })
        .detach();
    }

    /// A local project with a dev container configuration offers to open in
    /// it, once per project.
    pub(super) fn offer_dev_container(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone().filter(|r| !crate::fs::is_remote(r)) else { return };
        if cfg!(test) || SessionState::load().devcontainer_offered.contains(&root) {
            return;
        }
        let check = root.clone();
        cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async move { container::config_file(&check).is_some() }).await;
            if !found {
                return;
            }
            this.update(cx, |ws, cx| {
                if ws.root.as_deref() != Some(root.as_path()) {
                    return;
                }
                if let Err(err) = SessionState::change(|state| state.devcontainer_offered.push(root.clone())) {
                    eprintln!("couldn't note the dev container offer: {err}");
                }
                ws.toast_with_button(
                    ToastKind::Info,
                    "This project has a dev container",
                    Some("Open it there to work with the tools its container has. Open in Dev Container in Find Action does it later.".into()),
                    Some(ToastButton::OpenInDevContainer),
                    cx,
                );
            })
            .ok();
        })
        .detach();
    }
}

/// The program a transport runs, as the user's shell would find it.
fn program_for(transport: &Transport) -> Option<String> {
    match transport {
        Transport::Ssh(_) => Some(std::env::var("KUANA_SSH").unwrap_or_else(|_| "ssh".into())),
        Transport::Wsl(_) => None,
        Transport::Container { .. } => Some(docker_program()),
    }
}

/// `docker` (or `KUANA_DOCKER`, e.g. `podman`), found on the login shell's PATH.
fn docker_program() -> String {
    let named = std::env::var("KUANA_DOCKER").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "docker".into());
    crate::lsp::servers::which(&named).map(|p| p.display().to_string()).unwrap_or(named)
}
