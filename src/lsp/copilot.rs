//! GitHub Copilot's suggestions. Its language server
//! (`@github/copilot-language-server`, installed like other npm servers)
//! runs beside each file's own servers when `edit_prediction_provider` is
//! `"copilot"`: it gets the file's text like them, and the editor asks it for
//! ghost text (`textDocument/inlineCompletion`, see `editor/prediction.rs`).
//! It answers nothing else, so it isn't one of the servers editors send their
//! other requests to, and has its own status bar item.
//!
//! Signing in is GitHub's device flow, run by the server: `signIn` gives a
//! code to enter on github.com, and running the command it returns waits
//! until that's done (the server opens the page through `window/showDocument`).
//! The server keeps the sign-in itself, so it holds for every window.

use std::{path::Path, sync::Arc};

use gpui::{App, Context, Task};
use serde_json::{Value, json};

use super::{LspStore, Server, ServerKey, ServerState, install, servers};
use crate::{
    settings::{EditPredictionProvider, Settings},
    syntax::Language,
};

pub const ID: &str = "copilot";

/// Whether `path` gets Copilot's suggestions: Copilot is the provider, the
/// file's settings (language, project) don't turn them off, and it isn't a
/// file that usually holds secrets.
pub fn wanted(language: Language, path: &Path, cx: &App) -> bool {
    let settings = Settings::get(cx);
    settings.edit_prediction_provider == Some(EditPredictionProvider::Copilot)
        && !holds_secrets(path)
        && settings.for_file(language, Some(path)).edit_predictions
}

/// Files whose text shouldn't leave the computer: environment files
/// (`.env`, `.env.local`), keys and certificates, credentials for package
/// registries.
fn holds_secrets(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    name == ".env" || name.starts_with(".env.") || [".netrc", ".npmrc", ".pypirc"].contains(&name.as_str()) || ["pem", "key", "p12", "pfx"].contains(&ext.as_str())
}

/// Whether a Copilot server is signed in.
#[derive(Clone, PartialEq, Debug)]
pub enum Auth {
    /// Not known yet (it just started).
    Checking,
    SignedOut,
    /// Waiting for the code shown to be entered on github.com.
    SigningIn(String),
    /// Signed in as this GitHub user.
    SignedIn(String),
    /// Signed in, but the account has no Copilot (this user).
    NotAuthorized(String),
}

/// What the status bar shows for a file's Copilot.
#[derive(Clone, PartialEq, Debug)]
pub enum CopilotStatus {
    Starting,
    /// Not installed: Kuana's installer for it.
    Missing(Option<&'static install::Installer>),
    Installing,
    SignedOut,
    SigningIn(String),
    /// Signed in as this user; what the server says is wrong, if anything
    /// (offline, over a limit).
    Ready { user: String, problem: Option<String> },
    NotAuthorized(String),
    Failed(String),
}

/// What `signIn` answered.
pub enum SignIn {
    /// Signed in already (another window did it).
    Done,
    /// Enter `code` at `uri`, then run `command` (`finish_copilot_sign_in`).
    Code { code: String, uri: String, command: Value },
}

impl LspStore {
    /// The Copilot server `path`'s document has, if any.
    fn copilot_key(&self, path: &Path) -> Option<&ServerKey> {
        self.documents.get(path)?.servers.iter().map(|(key, _)| key).find(|key| key.id == ID)
    }

    fn running(&self, key: &ServerKey) -> Option<Arc<Server>> {
        match self.servers.get(key)? {
            ServerState::Running(server) => Some(server.clone()),
            _ => None,
        }
    }

    /// The Copilot server to ask for `path`'s suggestions: running, signed
    /// in and with the document open.
    pub fn copilot_for(&self, path: &Path) -> Option<Arc<Server>> {
        let key = self.copilot_key(path)?;
        let opened = self.documents.get(path)?.opened(key);
        (opened && matches!(self.copilot_auth.get(key), Some(Auth::SignedIn(_)))).then(|| self.running(key)).flatten()
    }

    pub fn copilot_status(&self, path: &Path) -> Option<CopilotStatus> {
        let key = self.copilot_key(path)?;
        Some(match self.servers.get(key)? {
            ServerState::Finding | ServerState::Starting(..) => CopilotStatus::Starting,
            ServerState::Missing(spec) => match install::for_spec(spec) {
                Some(installer) if install::installing(installer.command) => CopilotStatus::Installing,
                installer => CopilotStatus::Missing(installer),
            },
            ServerState::Failed(message) => CopilotStatus::Failed(message.clone()),
            ServerState::Running(_) => match self.copilot_auth.get(key).unwrap_or(&Auth::Checking) {
                Auth::Checking => CopilotStatus::Starting,
                Auth::SignedOut => CopilotStatus::SignedOut,
                Auth::SigningIn(code) => CopilotStatus::SigningIn(code.clone()),
                Auth::SignedIn(user) => CopilotStatus::Ready { user: user.clone(), problem: self.copilot_problems.get(key).cloned() },
                Auth::NotAuthorized(user) => CopilotStatus::NotAuthorized(user.clone()),
            },
        })
    }

    /// Asks a Copilot server whether it's signed in (`checkStatus`).
    pub(super) fn check_copilot(&mut self, key: &ServerKey, cx: &mut Context<Self>) {
        let Some(server) = self.running(key) else { return };
        let (key, generation) = (key.clone(), self.generations.get(key).copied());
        cx.spawn(async move |this, cx| {
            let result = server.request("checkStatus", json!({ "localChecksOnly": false })).await;
            this.update(cx, |s, cx| {
                if s.generations.get(&key).copied() != generation {
                    return;
                }
                let auth = match &result {
                    Ok(result) => auth_of(result),
                    Err(err) => {
                        s.copilot_problems.insert(key.clone(), err.message.clone());
                        Auth::SignedOut
                    }
                };
                // A sign-in under way stays until it ends.
                if !matches!(s.copilot_auth.get(&key), Some(Auth::SigningIn(_))) || matches!(auth, Auth::SignedIn(_)) {
                    s.copilot_auth.insert(key, auth);
                }
                s.copilot_changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// `didChangeStatus`: the server's own word on how it's doing (an
    /// error or warning shows in its status), and maybe a changed sign-in.
    pub(super) fn copilot_said(&mut self, key: &ServerKey, params: &Value, cx: &mut Context<Self>) {
        let message = params.get("message").and_then(Value::as_str).unwrap_or_default().trim();
        match params.get("kind").and_then(Value::as_str) {
            Some("Error" | "Warning") if !message.is_empty() => {
                self.copilot_problems.insert(key.clone(), message.to_string());
            }
            _ => {
                self.copilot_problems.remove(key);
            }
        }
        self.check_copilot(key, cx);
        cx.notify();
    }

    /// Starts signing in `path`'s Copilot: the code to enter, unless it's
    /// signed in already.
    pub fn start_copilot_sign_in(&mut self, path: &Path, cx: &mut Context<Self>) -> Task<Result<SignIn, String>> {
        let Some(key) = self.copilot_key(path).cloned() else { return Task::ready(Err("Copilot isn't running for this file".into())) };
        let Some(server) = self.running(&key) else { return Task::ready(Err("Copilot is still starting".into())) };
        cx.spawn(async move |this, cx| {
            let result = server.request("signIn", json!({})).await.map_err(|e| e.message)?;
            let sign_in = match result.get("status").and_then(Value::as_str) {
                Some("AlreadySignedIn" | "OK") => SignIn::Done,
                _ => {
                    let code = result.get("userCode").and_then(Value::as_str).ok_or("Copilot gave no code to sign in with")?.to_string();
                    let uri = result.get("verificationUri").and_then(Value::as_str).unwrap_or("https://github.com/login/device").to_string();
                    let command = result.get("command").cloned().unwrap_or_else(|| json!({ "command": "github.copilot.finishDeviceFlow", "arguments": [] }));
                    SignIn::Code { code, uri, command }
                }
            };
            this.update(cx, |s, cx| {
                match &sign_in {
                    SignIn::Done => s.check_copilot(&key, cx),
                    SignIn::Code { code, .. } => {
                        s.copilot_auth.insert(key.clone(), Auth::SigningIn(code.clone()));
                        s.copilot_changed(cx);
                    }
                }
            })
            .ok();
            Ok(sign_in)
        })
    }

    /// Runs the sign-in's `command`, which opens github.com and waits until
    /// the code is entered there (or the code expires).
    pub fn finish_copilot_sign_in(&mut self, path: &Path, command: Value, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        let Some(key) = self.copilot_key(path).cloned() else { return Task::ready(Err("Copilot isn't running for this file".into())) };
        let Some(server) = self.running(&key) else { return Task::ready(Err("Copilot stopped".into())) };
        let params = json!({
            "command": command.get("command").cloned().unwrap_or_default(),
            "arguments": command.get("arguments").cloned().unwrap_or_else(|| json!([])),
        });
        cx.spawn(async move |this, cx| {
            let result = server.request("workspace/executeCommand", params).await;
            this.update(cx, |s, cx| {
                s.copilot_auth.insert(key.clone(), Auth::Checking);
                s.check_copilot(&key, cx);
                s.copilot_changed(cx);
            })
            .ok();
            result.map(drop).map_err(|e| e.message)
        })
    }

    /// Gives up a sign-in under way (the code wasn't wanted after all).
    pub fn cancel_copilot_sign_in(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(key) = self.copilot_key(path).cloned()
            && matches!(self.copilot_auth.get(&key), Some(Auth::SigningIn(_)))
        {
            self.copilot_auth.insert(key, Auth::SignedOut);
            self.copilot_changed(cx);
        }
    }

    pub fn copilot_sign_out(&mut self, path: &Path, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        let Some(key) = self.copilot_key(path).cloned() else { return Task::ready(Ok(())) };
        let Some(server) = self.running(&key) else { return Task::ready(Ok(())) };
        cx.spawn(async move |this, cx| {
            let result = server.request("signOut", json!({})).await;
            this.update(cx, |s, cx| {
                // Every Copilot server of the window: the sign-in is the account's.
                let keys: Vec<ServerKey> = s.copilot_auth.keys().cloned().collect();
                for key in keys {
                    s.copilot_auth.insert(key, Auth::SignedOut);
                }
                s.copilot_changed(cx);
            })
            .ok();
            result.map(drop).map_err(|e| e.message)
        })
    }

    /// Installs Copilot's server for `path`'s document.
    pub fn install_copilot(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(key) = self.copilot_key(path)
            && let Some(ServerState::Missing(spec)) = self.servers.get(key)
        {
            let spec = *spec;
            self.install_spec(spec, cx);
        }
    }

    /// The sign-in changed: editors ask for suggestions from their next edit on.
    fn copilot_changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// Settings changed: documents that should get Copilot now are opened
    /// with it, ones that shouldn't any more are closed with it, and Copilot
    /// servers no document uses stop.
    pub(super) fn sync_copilot(&mut self, cx: &mut Context<Self>) {
        let mut gained = Vec::new();
        let mut lost = Vec::new();
        for (path, open) in &self.documents {
            let Some(doc) = open.doc.upgrade() else { continue };
            let language = doc.read(cx).language;
            let has = open.servers.iter().any(|(key, _)| key.id == ID);
            match (wanted(language, path, cx), has) {
                (true, false) if open.editor.is_some() => gained.push((path.clone(), language)),
                (false, true) => lost.push(path.clone()),
                _ => {}
            }
        }
        for path in lost {
            let Some(open) = self.documents.get_mut(&path) else { continue };
            let removed: Vec<(ServerKey, bool)> = open.servers.iter().filter(|(key, _)| key.id == ID).cloned().collect();
            open.servers.retain(|(key, _)| key.id != ID);
            for (key, opened) in removed {
                if opened && let Some(client) = self.client(&key) {
                    client.notify("textDocument/didClose", json!({ "textDocument": { "uri": super::protocol::path_to_uri(&path) } }));
                }
            }
        }
        let used: Vec<ServerKey> = self.documents.values().flat_map(|d| d.servers.iter().map(|(key, _)| key.clone())).filter(|key| key.id == ID).collect();
        let idle: Vec<ServerKey> = self.servers.keys().filter(|key| key.id == ID && !used.contains(key)).cloned().collect();
        for key in idle {
            self.stop(&key, cx);
        }
        for (path, language) in gained {
            let mut keys = self.copilot_keys(language, &path, cx);
            keys.retain(|(key, _, _)| crate::trust::is_trusted(&key.root));
            self.connect(&path, language, keys, cx);
        }
        cx.notify();
    }

    /// Copilot's server key for a file that should get its suggestions.
    #[allow(clippy::type_complexity)]
    pub(super) fn copilot_keys(&self, language: Language, path: &Path, cx: &App) -> Vec<(ServerKey, Option<Vec<String>>, &'static servers::ServerSpec)> {
        if !wanted(language, path, cx) {
            return Vec::new();
        }
        vec![(ServerKey { id: ID.to_string(), root: self.root_for(&servers::COPILOT, path) }, None, &servers::COPILOT)]
    }
}

/// A `checkStatus` answer as a sign-in.
fn auth_of(result: &Value) -> Auth {
    let user = result.get("user").and_then(Value::as_str).unwrap_or_default().to_string();
    match result.get("status").and_then(Value::as_str) {
        Some("OK" | "AlreadySignedIn" | "MaybeOk") => Auth::SignedIn(user),
        Some("NotAuthorized") => Auth::NotAuthorized(user),
        _ => Auth::SignedOut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_with_secrets_get_no_suggestions() {
        for name in [".env", ".env.local", "id.pem", "server.KEY", ".npmrc"] {
            assert!(holds_secrets(Path::new("/p").join(name).as_path()), "{name}");
        }
        for name in ["env.rs", ".envrc.md", "keys.rs", "main.py"] {
            assert!(!holds_secrets(Path::new("/p").join(name).as_path()), "{name}");
        }
    }

    #[test]
    fn sign_in_states() {
        assert_eq!(auth_of(&json!({ "status": "OK", "user": "octo" })), Auth::SignedIn("octo".into()));
        assert_eq!(auth_of(&json!({ "status": "NotAuthorized", "user": "octo" })), Auth::NotAuthorized("octo".into()));
        assert_eq!(auth_of(&json!({ "status": "NotSignedIn" })), Auth::SignedOut);
        assert_eq!(auth_of(&json!({})), Auth::SignedOut);
    }
}
