//! The app's database state, shared by every window (a `Global`): the saved
//! connections, the live ones, and what's been read of their catalogs. The
//! Database view draws it and the workspace runs statements through it.

use std::{collections::HashMap, sync::Arc};

use gpui::{App, AppContext, Context, Entity, Global, Task};

use super::{ColumnInfo, ConnectionConfig, DbObject, Subprogram, backend::Session};

pub enum Live {
    Connecting,
    Connected(Arc<Session>),
    Failed(String),
}

#[derive(Clone)]
pub enum Loaded<T> {
    Loading,
    Ready(T),
    Failed(String),
}

/// What's listed under an object: a table's columns, a package's subprograms.
#[derive(Clone)]
pub enum Details {
    Columns(Vec<ColumnInfo>),
    Subprograms(Vec<Subprogram>),
}

/// (connection, owner)
pub type SchemaKey = (String, String);
/// (connection, owner, object name)
pub type ObjectKey = (String, String, String);

pub struct DbModel {
    pub connections: Vec<ConnectionConfig>,
    pub live: HashMap<String, Live>,
    pub schemas: HashMap<String, Loaded<Vec<String>>>,
    pub objects: HashMap<SchemaKey, Loaded<Vec<DbObject>>>,
    pub details: HashMap<ObjectKey, Loaded<Details>>,
}

struct GlobalModel(Entity<DbModel>);

impl Global for GlobalModel {}

impl DbModel {
    pub fn global(cx: &mut App) -> Entity<DbModel> {
        if let Some(model) = cx.try_global::<GlobalModel>() {
            return model.0.clone();
        }
        let model = cx.new(|_| DbModel {
            connections: super::load_connections(),
            live: HashMap::new(),
            schemas: HashMap::new(),
            objects: HashMap::new(),
            details: HashMap::new(),
        });
        cx.set_global(GlobalModel(model.clone()));
        model
    }

    pub fn config(&self, id: &str) -> Option<&ConnectionConfig> {
        self.connections.iter().find(|c| c.id == id)
    }

    pub fn session(&self, id: &str) -> Option<Arc<Session>> {
        match self.live.get(id) {
            Some(Live::Connected(session)) => Some(session.clone()),
            _ => None,
        }
    }

    pub fn is_connected(&self, id: &str) -> bool {
        matches!(self.live.get(id), Some(Live::Connected(_)))
    }

    /// The connected ones, in the list's order.
    pub fn connected(&self) -> Vec<&ConnectionConfig> {
        self.connections.iter().filter(|c| self.is_connected(&c.id)).collect()
    }

    /// Adds or replaces a connection, saving the list (and its password, when
    /// it's to be kept). A changed connection disconnects.
    pub fn save(&mut self, config: ConnectionConfig, password: Option<String>, cx: &mut Context<Self>) -> Result<(), String> {
        match (&password, config.save_password) {
            (Some(password), true) => super::save_password(&config.id, password)?,
            (_, false) => super::forget_password(&config.id),
            _ => {}
        }
        match self.connections.iter().position(|c| c.id == config.id) {
            Some(ix) => {
                if self.connections[ix] != config {
                    self.forget(&config.id);
                }
                self.connections[ix] = config;
            }
            None => self.connections.push(config),
        }
        let saved = super::save_connections(&self.connections).map_err(|e| format!("Couldn't save the connections: {e}"));
        cx.notify();
        saved
    }

    /// The password prompt's Save password: keeps `password` in the keychain
    /// (None: forgets the saved one), without disconnecting.
    pub fn keep_password(&mut self, id: &str, password: Option<&str>, cx: &mut Context<Self>) -> Result<(), String> {
        let Some(config) = self.connections.iter_mut().find(|c| c.id == id) else { return Ok(()) };
        match password {
            Some(password) => super::save_password(id, password)?,
            None => super::forget_password(id),
        }
        config.save_password = password.is_some();
        let saved = super::save_connections(&self.connections).map_err(|e| format!("Couldn't save the connections: {e}"));
        cx.notify();
        saved
    }

    pub fn remove(&mut self, id: &str, cx: &mut Context<Self>) -> Result<(), String> {
        self.forget(id);
        self.connections.retain(|c| c.id != id);
        super::forget_password(id);
        let saved = super::save_connections(&self.connections).map_err(|e| format!("Couldn't save the connections: {e}"));
        cx.notify();
        saved
    }

    /// Drops a connection's session and everything read through it.
    fn forget(&mut self, id: &str) {
        self.live.remove(id);
        self.schemas.remove(id);
        self.objects.retain(|(c, _), _| c != id);
        self.details.retain(|(c, _, _), _| c != id);
    }

    pub fn disconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        self.forget(id);
        cx.notify();
    }

    /// Reads everything again (after the user changed things elsewhere).
    pub fn refresh(&mut self, id: &str, cx: &mut Context<Self>) {
        self.schemas.remove(id);
        self.objects.retain(|(c, _), _| c != id);
        self.details.retain(|(c, _, _), _| c != id);
        cx.notify();
    }

    /// Reads one schema's objects again (after compiling one of them).
    pub fn refresh_schema(&mut self, id: &str, owner: &str, cx: &mut Context<Self>) {
        if self.objects.contains_key(&(id.to_string(), owner.to_string())) {
            self.objects.remove(&(id.to_string(), owner.to_string()));
            self.load_objects(id, owner, cx);
        }
    }

    /// Connects with `password`, else the saved one. Resolves with an error
    /// to show (also kept in `live` as `Failed`).
    pub fn connect(&mut self, id: &str, password: Option<String>, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        let Some(config) = self.config(id).cloned() else { return Task::ready(Err("No such connection".into())) };
        let password = match password.or_else(|| config.save_password.then(|| super::saved_password(id)).flatten()) {
            Some(password) => password,
            // A SQLite file has none.
            None if !config.kind.has_login() => String::new(),
            None => return Task::ready(Err(NEEDS_PASSWORD.into())),
        };
        self.live.insert(id.to_string(), Live::Connecting);
        cx.notify();
        let connecting = Session::connect(&config, password);
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let result = connecting.await.unwrap_or_else(|_| Err("The connection thread stopped".into()));
            this.update(cx, |this, cx| {
                // Disconnected (or removed) meanwhile.
                if !matches!(this.live.get(&id), Some(Live::Connecting)) {
                    return Err("Cancelled".to_string());
                }
                let out = match result {
                    Ok(session) => {
                        this.live.insert(id.clone(), Live::Connected(Arc::new(session)));
                        Ok(())
                    }
                    Err(err) => {
                        this.live.insert(id.clone(), Live::Failed(err.clone()));
                        Err(err)
                    }
                };
                cx.notify();
                out
            })
            .unwrap_or_else(|_| Err("Closed".into()))
        })
    }

    pub fn load_schemas(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.schemas.contains_key(id) {
            return;
        }
        let Some(session) = self.session(id) else { return };
        self.schemas.insert(id.to_string(), Loaded::Loading);
        let job = session.run(|db| db.schemas());
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                if this.is_connected(&id) {
                    this.schemas.insert(id, loaded(result));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub fn load_objects(&mut self, id: &str, owner: &str, cx: &mut Context<Self>) {
        let key = (id.to_string(), owner.to_string());
        if self.objects.contains_key(&key) {
            return;
        }
        let Some(session) = self.session(id) else { return };
        self.objects.insert(key.clone(), Loaded::Loading);
        let owner = owner.to_string();
        let job = session.run(move |db| db.objects(&owner));
        cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                if this.is_connected(&key.0) {
                    this.objects.insert(key, loaded(result));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// A table's columns or a package's subprograms.
    pub fn load_details(&mut self, id: &str, owner: &str, object: &DbObject, cx: &mut Context<Self>) {
        let key = (id.to_string(), owner.to_string(), object.name.clone());
        if self.details.contains_key(&key) {
            return;
        }
        let Some(session) = self.session(id) else { return };
        self.details.insert(key.clone(), Loaded::Loading);
        let (owner, name, columns) = (owner.to_string(), object.name.clone(), object.kind.has_columns());
        let job = session.run(move |db| if columns { db.columns(&owner, &name).map(Details::Columns) } else { db.subprograms(&owner, &name).map(Details::Subprograms) });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                if this.is_connected(&key.0) {
                    this.details.insert(key, loaded(result));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

/// `connect`'s error when there's no password to connect with.
pub const NEEDS_PASSWORD: &str = "Enter the password to connect";

fn loaded<T>(result: Result<T, String>) -> Loaded<T> {
    match result {
        Ok(value) => Loaded::Ready(value),
        Err(err) => Loaded::Failed(err),
    }
}
