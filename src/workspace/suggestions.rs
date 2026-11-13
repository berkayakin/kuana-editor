//! Offers the registry's extension for a file Kuana has no language for.

use crate::extensions::{self, registry, suggest};

use super::*;

impl Workspace {
    /// A file opened as plain text that one of the registry's extensions
    /// covers: a toast offers to install it. Once per extension, remembered
    /// in `state.json`, so declining it is a matter of not clicking; quiet
    /// when the registry can't be read or doesn't list it.
    pub(super) fn suggest_extension(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        // Tests don't read the registry.
        if cfg!(test) {
            return;
        }
        let doc = editor.read(cx).doc.read(cx);
        if doc.language != Language::PlainText {
            return;
        }
        let Some(path) = doc.path.clone() else { return };
        let Some(id) = suggest::for_path(&path) else { return };
        if SessionState::load().suggested_extensions.iter().any(|known| known == id) || !suggest::offer_once(id) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let listed = cx
                .background_executor()
                .spawn(async move {
                    // Installed some other way (a folder, from Zed's): its files just aren't this one's.
                    if extensions::installed().iter().any(|e| e.id == id) {
                        return None;
                    }
                    registry::available().ok()?.iter().find(|listed| listed.id == id).cloned()
                })
                .await;
            let Some(listed) = listed else {
                suggest::forget_offer(id);
                return;
            };
            this.update(cx, |ws, cx| {
                if let Err(err) = SessionState::change(|state| state.suggested_extensions.push(id.to_string())) {
                    eprintln!("couldn't note the suggested extension: {err}");
                }
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let detail = format!("Install it for syntax highlighting and more in files like {name}. Other extensions are in Settings → Extensions.");
                ws.toast_with_button(
                    ToastKind::Info,
                    format!("The {} extension covers this file", listed.name),
                    Some(detail),
                    Some(ToastButton::InstallExtension(id)),
                    cx,
                );
            })
            .ok();
        })
        .detach();
    }

    /// Installs the extension a toast offered, from the registry, and loads it.
    pub(super) fn install_suggested_extension(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if !suggest::start_install(id) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let listed = registry::available()?.iter().find(|listed| listed.id == id).cloned();
                    let listed = listed.ok_or_else(|| format!("The registry doesn't list “{id}” any more."))?;
                    registry::install(&listed).map(|name| format!("Installed {name} {}", listed.version))
                })
                .await;
            suggest::end_install(id);
            this.update(cx, |ws, cx| match result {
                Ok(message) => {
                    // Open files of its languages take them on (`ExtensionsChanged`).
                    extensions::reload(cx);
                    ws.toast(ToastKind::Success, message, None, cx);
                }
                Err(err) => ws.toast(ToastKind::Error, "Couldn't install the extension", Some(err), cx),
            })
            .ok();
        })
        .detach();
    }
}
