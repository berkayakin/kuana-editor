//! The Database view's and panel's place in the workspace (see
//! `crate::database`): connecting (asking for a password when none is
//! saved), opening objects' source in tabs, running them on Save (Oracle
//! compiles them), running SQL from the editor (⌘↩ / Ctrl+Enter, the
//! gutter's ▶) into the Database panel, and the view's right-click menus.

use std::{borrow::Cow, ops::Range, rc::Rc, sync::Arc};

use gpui::WeakEntity;

use super::*;
use crate::{
    database::{
        self, ConnectionConfig, DbKind, DbObject, ObjectFile, ObjectKind,
        backend::{FileProblem, Outcome},
        model::{DbModel, NEEDS_PASSWORD},
        oracle, script,
        view::NodeId,
    },
    editor::Submit,
    lint::{Diagnostic, Severity},
};

/// A run's statements, so Output's rows can show them in their editor.
pub(super) struct SqlRun {
    editor: WeakEntity<Editor>,
    ranges: Vec<Range<usize>>,
}

/// A run that had to ask for its connection's password: it goes on once
/// the password connects.
pub(super) struct SqlWaiting {
    editor: WeakEntity<Editor>,
    scope: SqlScope,
}

/// What a run sends.
#[derive(Clone, Copy)]
pub(super) enum SqlScope {
    /// The selection's statements, else the statement at the caret.
    Caret,
    /// The whole file.
    Script,
    /// The statement at an offset (its gutter ▶).
    At(usize),
}

/// Diagnostics lists the database fills on a document (by "server" name).
const COMPILE_PROBLEMS: &str = "database";
const RUN_PROBLEMS: &str = "database-run";

impl Workspace {
    fn db_model(&self, cx: &App) -> Entity<DbModel> {
        self.database.read(cx).model().clone()
    }

    pub(super) fn toggle_database(&mut self, _: &ToggleDatabase, window: &mut Window, cx: &mut Context<Self>) {
        if self.view_shown(SidebarMode::Database) {
            self.set_view_shown(SidebarMode::Database, false);
            self.focus_active(window, cx);
        } else {
            self.set_view_shown(SidebarMode::Database, true);
            window.focus(&self.database.focus_handle(cx));
        }
        self.save_state(cx);
        cx.notify();
    }

    fn show_database_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.view_shown(SidebarMode::Database) {
            self.toggle_database(&ToggleDatabase, window, cx);
        }
    }

    pub(super) fn new_database_connection(&mut self, _: &NewDatabaseConnection, window: &mut Window, cx: &mut Context<Self>) {
        self.open_connection_dialog(None, window, cx);
    }

    pub(super) fn toggle_database_results(&mut self, _: &ToggleDatabaseResults, window: &mut Window, cx: &mut Context<Self>) {
        if self.shows(BottomPanel::Database) {
            self.hide_bottom(BottomPanel::Database);
            self.focus_active(window, cx);
        } else {
            self.bottom = Some(BottomPanel::Database);
        }
        cx.notify();
    }

    pub(super) fn render_database_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        area(&theme).h(self.shown_panel_height()).flex_shrink_0().child(cached(&self.db_results)).child(self.resize_handle(Edge::Problems, cx))
    }

    /// New Connection… / Edit Connection…: the connection dialog, in a
    /// window of its own over this one; what it connects comes back here
    /// (`connection_entered`).
    fn open_connection_dialog(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let config = id.and_then(|id| self.db_model(cx).read(cx).config(&id).cloned());
        let display = window.display(cx).map(|d| d.id());
        let Some(this) = window.window_handle().downcast::<Workspace>() else { return };
        let on_connect = Box::new(move |config: ConnectionConfig, password: Option<String>, cx: &mut App| {
            // After the dialog's own window is done with this update.
            cx.defer(move |cx| {
                this.update(cx, |ws, window, cx| {
                    window.activate_window();
                    ws.connection_entered(config, password, window, cx);
                })
                .ok();
            });
        });
        crate::database::dialog::open(config, None, display, on_connect, cx);
    }

    /// The connection dialog's Connect: save it, then connect.
    fn connection_entered(&mut self, config: ConnectionConfig, password: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.db_model(cx);
        let id = config.id.clone();
        if let Err(err) = model.update(cx, |m, cx| m.save(config, password.clone(), cx)) {
            self.toast(ToastKind::Error, "Couldn't save the connection", Some(err), cx);
        }
        self.show_database_view(window, cx);
        self.connect_database(id, password, None, window, cx);
    }

    /// Connects `id`, asking for its password when there's none to use;
    /// then a run that was waiting for it goes on, in its editor.
    fn connect_database(&mut self, id: String, password: Option<String>, then: Option<SqlWaiting>, window: &mut Window, cx: &mut Context<Self>) {
        self.connect_with(id, password, None, then, window, cx);
    }

    /// `asked`: the password came from `ask_db_password`, with whether to
    /// keep it; it's kept once it works, and a wrong one asks again.
    fn connect_with(&mut self, id: String, password: Option<String>, asked: Option<bool>, then: Option<SqlWaiting>, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.db_model(cx);
        let typed = password.clone();
        let task = model.update(cx, |m, cx| m.connect(&id, password, cx));
        cx.spawn_in(window, async move |ws, cx| {
            let result = task.await;
            ws.update_in(cx, |ws, window, cx| match result {
                Ok(()) => {
                    if let (Some(save), Some(password)) = (asked, typed) {
                        let kept = ws.db_model(cx).update(cx, |m, cx| m.keep_password(&id, save.then_some(password.as_str()), cx));
                        if let Err(err) = kept {
                            ws.toast(ToastKind::Error, "Couldn't save the password", Some(err), cx);
                        }
                    }
                    ws.database.update(cx, |v, cx| v.expand_connection(&id, cx));
                    if let Some((editor, scope)) = then.and_then(|w| Some((w.editor.upgrade()?, w.scope))) {
                        window.focus(&editor.focus_handle(cx));
                        ws.run_sql(editor, scope, window, cx);
                    }
                }
                Err(err) if err == NEEDS_PASSWORD => ws.ask_db_password(id, then, None, window, cx),
                Err(err) if err == "Cancelled" => {}
                Err(err) if asked.is_some() => ws.ask_db_password(id, then, Some(err), window, cx),
                Err(err) => {
                    let name = ws.db_model(cx).read(cx).config(&id).map(|c| c.name.clone()).unwrap_or_default();
                    ws.toast(ToastKind::Error, format!("Couldn't connect to {name}"), Some(err), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Asks for a connection's password (none is saved) in a small dialog,
    /// with Save password, then connects; `error`: why the last one didn't.
    fn ask_db_password(&mut self, id: String, then: Option<SqlWaiting>, error: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(config) = self.db_model(cx).read(cx).config(&id).cloned() else { return };
        self.close_modal(window, cx);
        let title = format!("Connect to {}", config.name);
        let hint = format!("Password for {} at {}", config.user, config.address());
        let dialog = cx.new(|cx| InputDialog::new(title, Icon::Database, hint, "", None, window, cx).secret(cx).allow_empty().with_option("Save password", true));
        if let Some(error) = error {
            dialog.update(cx, |d, cx| d.set_error(error, cx));
        }
        let then = std::cell::RefCell::new(then);
        let subscription = cx.subscribe_in(&dialog, window, move |ws, dialog, event: &InputDialogEvent, window, cx| {
            let password = match event {
                InputDialogEvent::Confirm(text) => text.clone(),
                InputDialogEvent::Dismissed => {
                    ws.close_modal(window, cx);
                    return;
                }
            };
            let save = dialog.read(cx).option_checked();
            ws.close_modal(window, cx);
            ws.connect_with(id.clone(), Some(password), Some(save), then.borrow_mut().take(), window, cx);
        });
        self.modals.open(Modal::Input(dialog), subscription);
        cx.notify();
    }

    pub(super) fn on_database_event(&mut self, event: &DatabaseEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            DatabaseEvent::NewConnection => self.open_connection_dialog(None, window, cx),
            DatabaseEvent::Connect(id) => self.connect_database(id.clone(), None, None, window, cx),
            DatabaseEvent::Open { connection, owner, object, body, at } => {
                self.open_db_object(connection.clone(), owner.clone(), object.clone(), *body, at.clone(), window, cx)
            }
            DatabaseEvent::Menu { node, position } => self.open_database_menu(node.clone(), *position, window, cx),
            DatabaseEvent::Leave => {
                if self.active_item(cx).is_some() {
                    self.focus_active(window, cx);
                }
            }
        }
    }

    pub(super) fn on_query_results_event(&mut self, event: &QueryResultsEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            QueryResultsEvent::Close => {
                self.hide_bottom(BottomPanel::Database);
                self.focus_active(window, cx);
                cx.notify();
            }
            QueryResultsEvent::TabMenu { id, position } => self.open_result_tab_menu(*id, *position, window, cx),
            QueryResultsEvent::Rename(id) => self.open_rename_result(*id, window, cx),
            QueryResultsEvent::Commit => self.end_transaction(true, cx),
            QueryResultsEvent::Rollback => self.end_transaction(false, cx),
            QueryResultsEvent::Reveal(ix) => {
                let Some(run) = &self.db_run else { return };
                let (Some(editor), Some(range)) = (run.editor.upgrade(), run.ranges.get(*ix).cloned()) else { return };
                if let Some(item) = self.items(cx).into_iter().find(|i| i.editor() == Some(&editor)) {
                    self.activate_item(&item, window, cx);
                    editor.update(cx, |e, cx| e.select_range(range, cx));
                }
            }
        }
    }

    /// A Result tab's right-click menu.
    fn open_result_tab_menu(&mut self, id: u64, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let results = self.db_results.read(cx);
        let Some((_, named)) = results.name(id) else { return };
        let pinned = results.is_pinned(id);
        let count = results.result_count();
        let mut entries = Vec::new();
        let mut actions = Vec::new();
        let mut add = |entry: MenuEntry, call: Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>| {
            entries.push(entry);
            actions.push(Some(MenuAction::Call(call)));
        };
        add(
            MenuEntry::item(if pinned { "Unpin" } else { "Pin" }, Icon::Pin),
            Rc::new(move |ws, _, cx| ws.db_results.update(cx, |r, cx| r.toggle_pin(id, cx))),
        );
        add(MenuEntry::item("Rename…", Icon::Pencil), Rc::new(move |ws, window, cx| ws.open_rename_result(id, window, cx)));
        if named {
            add(MenuEntry::text("Reset Name"), Rc::new(move |ws, _, cx| ws.db_results.update(cx, |r, cx| r.rename(id, None, cx))));
        }
        entries.push(MenuEntry::Separator);
        actions.push(None);
        let mut add = |entry: MenuEntry, call: Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>| {
            entries.push(entry);
            actions.push(Some(MenuAction::Call(call)));
        };
        add(MenuEntry::item("Close", Icon::X), Rc::new(move |ws, _, cx| ws.db_results.update(cx, |r, cx| r.close_results(Some(id), false, cx))));
        if count > 1 {
            add(MenuEntry::text("Close Others"), Rc::new(move |ws, _, cx| ws.db_results.update(cx, |r, cx| r.close_results(Some(id), true, cx))));
            add(MenuEntry::text("Close All Results"), Rc::new(|ws, _, cx| ws.db_results.update(cx, |r, cx| r.close_results(None, false, cx))));
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    fn open_rename_result(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((initial, _)) = self.db_results.read(cx).name(id) else { return };
        self.close_modal(window, cx);
        let hint = "Leave empty to go back to its number";
        let dialog = cx.new(|cx| InputDialog::new("Rename Result", Icon::Database, hint, &initial, Some(0..initial.len()), window, cx).allow_empty());
        let subscription = cx.subscribe_in(&dialog, window, move |ws, _, event: &InputDialogEvent, window, cx| {
            if let InputDialogEvent::Confirm(name) = event {
                ws.db_results.update(cx, |r, cx| r.rename(id, Some(name.clone()), cx));
            }
            ws.close_modal(window, cx);
        });
        self.modals.open(Modal::Input(dialog), subscription);
        cx.notify();
    }

    fn end_transaction(&mut self, commit: bool, cx: &mut Context<Self>) {
        let Some((id, name)) = self.db_results.read(cx).connection.clone() else { return };
        let Some(session) = self.db_model(cx).read(cx).session(&id) else { return };
        let job = session.run(move |db| if commit { db.commit() } else { db.rollback() });
        cx.spawn(async move |ws, cx| {
            let result = job.await;
            ws.update(cx, |ws, cx| match result {
                Ok(()) => {
                    ws.db_results.update(cx, |r, cx| r.committed(cx));
                    ws.toast(ToastKind::Success, if commit { format!("Committed on {name}") } else { format!("Rolled back on {name}") }, None, cx);
                }
                Err(err) => ws.toast(ToastKind::Error, if commit { "Couldn't commit" } else { "Couldn't roll back" }, Some(err), cx),
            })
            .ok();
        })
        .detach();
    }

    // ---- Objects' source ----------------------------------------------------------------

    /// Reads an object's source into its file and opens it (the file is
    /// rewritten from the database each time, unless its tab has unsaved edits).
    #[allow(clippy::too_many_arguments)]
    fn open_db_object(&mut self, connection: String, owner: String, object: DbObject, body: bool, at: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.db_model(cx);
        let Some(session) = model.read(cx).session(&connection) else { return };
        let file = ObjectFile { connection, owner: owner.clone(), name: object.name.clone(), kind: object.kind, body };
        let Some(path) = database::object_path(&file, session.kind) else { return };
        // Unsaved edits in its tab stay.
        if let Some(editor) = self.tab_for(&path, cx)
            && editor.read(cx).doc.read(cx).is_dirty()
        {
            self.open_path(path, window, cx);
            self.reveal_subprogram(&file_path_editor(self, &file, cx), at.as_deref(), window, cx);
            return;
        }
        let (name, kind) = (object.name.clone(), object.kind);
        let job = session.run(move |db| db.source(&owner, &name, kind, body));
        cx.spawn_in(window, async move |ws, cx| {
            let result = job.await;
            ws.update_in(cx, |ws, window, cx| {
                let text = match result {
                    Ok(text) => text,
                    Err(err) => {
                        ws.toast(ToastKind::Error, format!("Couldn't read {}", file.name), Some(err), cx);
                        return;
                    }
                };
                let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&path, &text));
                if let Err(err) = written {
                    ws.toast(ToastKind::Error, "Couldn't write the object's file", Some(err.to_string()), cx);
                    return;
                }
                if let Some(editor) = ws.tab_for(&path, cx) {
                    editor.update(cx, |e, cx| {
                        let _ = e.revert(cx);
                    });
                }
                ws.open_path(path.clone(), window, cx);
                let editor = ws.tab_for(&path, cx);
                ws.reveal_subprogram(&editor, at.as_deref(), window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Puts the caret on a subprogram's PROCEDURE/FUNCTION line.
    fn reveal_subprogram(&mut self, editor: &Option<Entity<Editor>>, name: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(editor), Some(name)) = (editor, name) else { return };
        let text = editor.read(cx).text(cx);
        let pattern = format!(r#"(?im)^\s*(?:procedure|function)\s+"?{}"?\b"#, regex::escape(name));
        let Ok(re) = regex::Regex::new(&pattern) else { return };
        if let Some(m) = re.find(&text) {
            let start = m.start() + (m.as_str().len() - m.as_str().trim_start().len());
            editor.update(cx, |e, cx| e.go_to(start, window, cx));
        }
    }

    /// After a file's Save: an object's file runs against its connection
    /// (Oracle compiles it), and what's wrong in it becomes its problems.
    pub(super) fn database_saved(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = editor.read(cx).doc.read(cx).path.clone() else { return };
        let Some(object) = database::object_of(&path) else { return };
        let model = self.db_model(cx);
        let Some(config) = model.read(cx).config(&object.connection).cloned() else { return };
        if !config.kind.saves(object.kind) {
            return;
        }
        let verb = if config.kind == DbKind::Oracle { "compile" } else { "update" };
        let Some(session) = model.read(cx).session(&object.connection) else {
            self.toast(ToastKind::Info, format!("Saved locally: connect to {} to {verb} {}", config.name, object.name), None, cx);
            return;
        };
        let text = editor.read(cx).text(cx);
        let saved = object.clone();
        let job = session.run(move |db| db.save_object(&saved, &text));
        let weak = editor.downgrade();
        let rule = config.kind.name();
        cx.spawn_in(window, async move |ws, cx| {
            let result = job.await;
            ws.update_in(cx, |ws, _, cx| {
                let Some(editor) = weak.upgrade() else { return };
                match result {
                    Ok(problems) => {
                        let errors: Vec<&FileProblem> = problems.iter().filter(|p| !p.warning).collect();
                        let first = errors.first().map(|p| format!("Line {}: {}", line_of(&editor, p.offset, cx), p.message));
                        let diagnostics = problem_diagnostics(&editor, &problems, rule, cx);
                        editor.update(cx, |e, cx| {
                            e.doc.as_mut(cx).set_server_diagnostics(COMPILE_PROBLEMS, diagnostics);
                            cx.notify();
                        });
                        match (errors.len(), verb) {
                            (0, "compile") => ws.toast(ToastKind::Success, format!("Compiled {}", object.name), None, cx),
                            (0, _) => ws.toast(ToastKind::Success, format!("Updated {} in {}", object.name, config.name), None, cx),
                            (n, "compile") => ws.toast(ToastKind::Error, format!("{} has {n} compile error{}", object.name, if n == 1 { "" } else { "s" }), first, cx),
                            (_, _) => ws.toast(ToastKind::Error, format!("Couldn't update {}", object.name), first, cx),
                        }
                    }
                    Err(err) => ws.toast(ToastKind::Error, format!("Couldn't {verb} {}", object.name), Some(err), cx),
                }
                ws.db_model(cx).update(cx, |m, cx| m.refresh_schema(&object.connection, &object.owner, cx));
            })
            .ok();
        })
        .detach();
    }

    // ---- Running SQL --------------------------------------------------------------------

    /// ⌘↩ / Ctrl+Enter: the editor passes it on in SQL files (`Editor::submit`).
    pub(super) fn submit_in_editor(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_editor(cx) {
            Some(editor) if editor.focus_handle(cx).is_focused(window) && sql_editor(&editor, cx) => self.run_sql(editor, SqlScope::Caret, window, cx),
            _ => cx.propagate(),
        }
    }

    pub(super) fn run_sql_statement(&mut self, _: &RunSqlStatement, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor(cx) {
            self.run_sql(editor, SqlScope::Caret, window, cx);
        }
    }

    pub(super) fn run_sql_script(&mut self, _: &RunSqlScript, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor(cx) {
            self.run_sql(editor, SqlScope::Script, window, cx);
        }
    }

    /// Whether an editor's gutter shows a ▶ per statement: in SQL files
    /// other than an object's (saving compiles those) while a connection exists.
    /// Its statements split by the rules of the database the file runs on.
    pub(super) fn sync_sql_gutter(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let path = editor.read(cx).doc.read(cx).path.clone();
        let runs = path.as_deref().is_some_and(|p| database::is_sql_path(p) && database::object_of(p).is_none());
        let model = self.db_model(cx);
        let dialect = runs
            .then(|| {
                let id = self.sql_connection(path.as_deref(), cx);
                let model = model.read(cx);
                id.and_then(|id| model.config(&id)).or_else(|| model.connections.first()).map(|c| c.kind.dialect())
            })
            .flatten();
        editor.update(cx, |e, cx| e.set_sql_runs(dialect, cx));
    }

    pub(super) fn sync_sql_gutters(&mut self, cx: &mut Context<Self>) {
        for editor in self.shown_editors(cx) {
            self.sync_sql_gutter(&editor, cx);
        }
    }

    /// New SQL Script: a scratch file for the selected (or only) connection.
    pub(super) fn new_sql_script(&mut self, _: &NewSqlScript, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.database.read(cx).selected_connection().or_else(|| self.db_model(cx).read(cx).connections.first().map(|c| c.id.clone())) else {
            self.open_connection_dialog(None, window, cx);
            return;
        };
        self.new_script_for(id, window, cx);
    }

    fn new_script_for(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = database::new_script_path(&id) else { return };
        if let Err(err) = std::fs::write(&path, "") {
            self.toast(ToastKind::Error, "Couldn't create the script", Some(err.to_string()), cx);
            return;
        }
        self.open_path(path, window, cx);
    }

    /// The connection a file's SQL runs on: an object's or a script's own,
    /// else the one chosen for it, else the Database view's selected one, else
    /// the only one connected, else the only one there is.
    fn sql_connection(&self, path: Option<&Path>, cx: &App) -> Option<String> {
        if let Some(path) = path {
            if let Some(object) = database::object_of(path) {
                return Some(object.connection);
            }
            if let Some(id) = database::script_connection(path) {
                return Some(id);
            }
            if let Some(id) = self.sql_connections.get(path) {
                return Some(id.clone());
            }
        }
        let model = self.db_model(cx);
        let model = model.read(cx);
        if let Some(id) = self.database.read(cx).selected_connection().filter(|id| model.is_connected(id)) {
            return Some(id);
        }
        let connected = model.connected();
        match (&connected[..], &model.connections[..]) {
            ([only], _) => Some(only.id.clone()),
            ([], [only]) => Some(only.id.clone()),
            _ => None,
        }
    }

    /// Runs an editor's statements: see `SqlScope`.
    pub(super) fn run_sql(&mut self, editor: Entity<Editor>, scope: SqlScope, window: &mut Window, cx: &mut Context<Self>) {
        let path = editor.read(cx).doc.read(cx).path.clone();
        let model = self.db_model(cx);
        let Some(id) = self.sql_connection(path.as_deref(), cx) else {
            if model.read(cx).connections.is_empty() {
                self.open_connection_dialog(None, window, cx);
            } else {
                self.show_database_view(window, cx);
                self.toast(ToastKind::Info, "Choose a connection", Some("Connect to a database in the Database view, or pick one for this file in the status bar.".into()), cx);
            }
            return;
        };
        if let Some(path) = &path {
            self.sql_connections.insert(path.clone(), id.clone());
        }
        let Some(session) = model.read(cx).session(&id) else {
            // Connect first, then run.
            let task = model.update(cx, |m, cx| m.connect(&id, None, cx));
            cx.spawn_in(window, async move |ws, cx| {
                let result = task.await;
                ws.update_in(cx, |ws, window, cx| match result {
                    Ok(()) => ws.run_sql(editor, scope, window, cx),
                    Err(err) if err == NEEDS_PASSWORD => ws.ask_db_password(id, Some(SqlWaiting { editor: editor.downgrade(), scope }), None, window, cx),
                    Err(err) => ws.toast(ToastKind::Error, "Couldn't connect", Some(err), cx),
                })
                .ok();
            })
            .detach();
            return;
        };
        let text = editor.read(cx).text(cx);
        let selection = editor.read(cx).primary_selection().range();
        let db_kind = session.kind;
        // The selection's statements (as typed), else the file's, else one.
        let selected = matches!(scope, SqlScope::Caret) && !selection.is_empty();
        let (base, script_text) = if selected { (selection.start, text[selection.clone()].to_string()) } else { (0, text.clone()) };
        let mut statements = script::split(&script_text, db_kind.dialect());
        let one = match scope {
            SqlScope::Caret if !selected => Some(editor.read(cx).primary_selection().head),
            SqlScope::At(at) => Some(at),
            _ => None,
        };
        if let Some(at) = one {
            statements = script::at(&statements, &script_text, at).map(|ix| vec![statements[ix].clone()]).unwrap_or_default();
        }
        statements.retain(|s| s.kind != script::Kind::Client);
        if statements.is_empty() {
            self.toast(ToastKind::Info, "No SQL statement here", None, cx);
            return;
        }
        let sqls: Vec<String> = statements.iter().map(|s| s.sql(&script_text)).collect();
        let ranges: Vec<Range<usize>> = statements.iter().map(|s| s.range.start + base..s.range.end + base).collect();
        let name = model.read(cx).config(&id).map(|c| c.name.clone()).unwrap_or_default();
        self.db_results.update(cx, |r, cx| r.start((id.clone(), name), sqls.clone(), cx));
        self.db_run = Some(SqlRun { editor: editor.downgrade(), ranges: ranges.clone() });
        self.has_db_results = true;
        self.bottom = Some(BottomPanel::Database);
        editor.update(cx, |e, cx| e.doc.as_mut(cx).set_server_diagnostics(RUN_PROBLEMS, Vec::new()));
        cx.notify();
        let weak = editor.downgrade();
        let schema = session.schema.clone();
        cx.spawn_in(window, async move |ws, cx| {
            let mut problems: Vec<(Range<usize>, String)> = Vec::new();
            let mut changed_schema = false;
            for (ix, sql) in sqls.into_iter().enumerate() {
                let created = oracle::created_unit(&sql).is_some() || starts_ddl(&sql);
                let executed = session.run(move |db| Ok(db.execute(&sql))).await;
                let Ok(executed) = executed else { break };
                let range = ranges[ix].clone();
                match &executed.outcome {
                    Outcome::Failed { message, offset } => {
                        let at = offset.map_or(range.start, |chars| {
                            let statement = script::Statement { range: range.clone(), kind: script::Kind::Sql };
                            script::error_offset(&text, &statement, chars)
                        });
                        problems.push((at..at, message.clone()));
                    }
                    Outcome::Compiled(errors) => {
                        let first_line = text[..range.start].matches('\n').count();
                        for e in errors.iter().filter(|e| !e.warning) {
                            let line_start = text.split_inclusive('\n').take(first_line + e.line - 1).map(str::len).sum::<usize>();
                            problems.push((line_start..line_start, format!("{}:{} {}", e.line, e.column, e.message)));
                        }
                        changed_schema = true;
                    }
                    Outcome::Done if created => changed_schema = true,
                    _ => {}
                }
                let ok = ws.update(cx, |ws, cx| ws.db_results.update(cx, |r, cx| r.finish(ix, executed, cx)));
                if ok.is_err() {
                    return;
                }
            }
            ws.update(cx, |ws, cx| {
                if let Some(editor) = weak.upgrade() {
                    let diagnostics = problems
                        .into_iter()
                        .map(|(range, message)| {
                            let range = word_at(&editor.read(cx).text(cx), range.start);
                            Diagnostic { range, severity: Severity::Error, message: Arc::from(message), rule: Cow::Borrowed(db_kind.name()), unnecessary: false }
                        })
                        .collect();
                    editor.update(cx, |e, cx| {
                        e.doc.as_mut(cx).set_server_diagnostics(RUN_PROBLEMS, diagnostics);
                        cx.notify();
                    });
                }
                if changed_schema {
                    // Without a schema it started in, any of them may have changed.
                    ws.db_model(cx).update(cx, |m, cx| if schema.is_empty() { m.refresh(&id, cx) } else { m.refresh_schema(&id, &schema, cx) });
                }
            })
            .ok();
        })
        .detach();
    }

    // ---- Menus --------------------------------------------------------------------------

    fn open_database_menu(&mut self, node: NodeId, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        type Call = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;
        let mut entries: Vec<MenuEntry> = Vec::new();
        let mut actions: Vec<Option<MenuAction>> = Vec::new();
        let mut add = |entry: MenuEntry, call: Option<Call>| {
            entries.push(entry);
            actions.push(call.map(MenuAction::Call));
        };
        let model = self.db_model(cx);
        let connection = node.connection().to_string();
        let connected = model.read(cx).is_connected(&connection);
        match &node {
            NodeId::Connection(id) => {
                let id = id.clone();
                if connected {
                    let i = id.clone();
                    add(MenuEntry::item("New SQL Script", Icon::FilePlus), Some(Rc::new(move |ws, window, cx| ws.new_script_for(i.clone(), window, cx))));
                    let i = id.clone();
                    add(MenuEntry::item("Refresh", Icon::Refresh), Some(Rc::new(move |ws, _, cx| ws.db_model(cx).update(cx, |m, cx| m.refresh(&i, cx)))));
                    let i = id.clone();
                    add(MenuEntry::item("Disconnect", Icon::Stop), Some(Rc::new(move |ws, _, cx| ws.db_model(cx).update(cx, |m, cx| m.disconnect(&i, cx)))));
                } else {
                    let i = id.clone();
                    add(MenuEntry::item("Connect", Icon::Play), Some(Rc::new(move |ws, window, cx| ws.connect_database(i.clone(), None, None, window, cx))));
                    let i = id.clone();
                    add(MenuEntry::item("New SQL Script", Icon::FilePlus), Some(Rc::new(move |ws, window, cx| ws.new_script_for(i.clone(), window, cx))));
                }
                add(MenuEntry::Separator, None);
                let i = id.clone();
                add(MenuEntry::item("Edit Connection…", Icon::Pencil), Some(Rc::new(move |ws, window, cx| ws.open_connection_dialog(Some(i.clone()), window, cx))));
                let i = id.clone();
                add(MenuEntry::item("Delete Connection…", Icon::Trash).danger(), Some(Rc::new(move |ws, window, cx| ws.delete_connection(i.clone(), window, cx))));
            }
            NodeId::Object(..) | NodeId::Body(..) | NodeId::Detail(..) => {
                let Some((conn, owner, object)) = self.database.read(cx).object_at(&node, cx) else { return };
                let db_kind = model.read(cx).config(&conn).map(|c| c.kind).unwrap_or_default();
                let qualified = format!("{owner}.{}", object.name);
                if object.kind.has_columns() {
                    let (c, o, ob) = (conn.clone(), owner.clone(), object.clone());
                    add(MenuEntry::item("Select Rows", Icon::Table), Some(Rc::new(move |ws, window, cx| ws.select_rows(c.clone(), &o, &ob, window, cx))));
                }
                let body_too = object.has_body;
                let (c, o, ob) = (conn.clone(), owner.clone(), object.clone());
                let label = match object.kind {
                    ObjectKind::Package | ObjectKind::Type => "Open Specification",
                    kind if db_kind.saves(kind) => "Open Source",
                    _ => "Show DDL",
                };
                add(MenuEntry::item(label, Icon::FileCode), Some(Rc::new(move |ws, window, cx| ws.open_db_object(c.clone(), o.clone(), ob.clone(), false, None, window, cx))));
                if body_too {
                    let (c, o, ob) = (conn.clone(), owner.clone(), object.clone());
                    add(MenuEntry::item("Open Body", Icon::FileCode), Some(Rc::new(move |ws, window, cx| ws.open_db_object(c.clone(), o.clone(), ob.clone(), true, None, window, cx))));
                }
                if db_kind.recompiles() && db_kind.saves(object.kind) {
                    let (c, o, ob) = (conn.clone(), owner.clone(), object.clone());
                    add(MenuEntry::item("Compile", Icon::Refresh), Some(Rc::new(move |ws, _, cx| ws.recompile(c.clone(), o.clone(), ob.clone(), cx))));
                }
                add(MenuEntry::Separator, None);
                let name = match &node {
                    NodeId::Detail(_, _, _, detail) => detail.clone(),
                    _ => qualified.clone(),
                };
                add(MenuEntry::item("Copy Name", Icon::Copy), Some(Rc::new(move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(name.clone())))));
            }
            NodeId::Schema(..) | NodeId::Group(..) | NodeId::Schemas(..) => {
                let i = connection.clone();
                add(MenuEntry::item("Refresh", Icon::Refresh), Some(Rc::new(move |ws, _, cx| ws.db_model(cx).update(cx, |m, cx| m.refresh(&i, cx)))));
            }
            NodeId::Message(..) => return,
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    fn delete_connection(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.db_model(cx);
        let name = model.read(cx).config(&id).map(|c| c.name.clone()).unwrap_or_default();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete the connection “{name}”?"),
            Some("Its saved password is removed from the keychain. Nothing in the database changes."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |ws, cx| {
            if answer.await == Ok(0) {
                ws.update(cx, |ws, cx| {
                    if let Err(err) = ws.db_model(cx).update(cx, |m, cx| m.remove(&id, cx)) {
                        ws.toast(ToastKind::Error, "Couldn't delete the connection", Some(err), cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    /// Select Rows: the table's first rows in the Database panel.
    fn select_rows(&mut self, id: String, owner: &str, object: &DbObject, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.db_model(cx);
        let Some(session) = model.read(cx).session(&id) else { return };
        let sql = session.kind.select_rows(owner, &object.name);
        let name = model.read(cx).config(&id).map(|c| c.name.clone()).unwrap_or_default();
        self.db_results.update(cx, |r, cx| r.start((id, name), vec![sql.clone()], cx));
        self.db_run = None;
        self.has_db_results = true;
        self.bottom = Some(BottomPanel::Database);
        cx.notify();
        let job = session.run(move |db| Ok(db.execute(&sql)));
        cx.spawn_in(window, async move |ws, cx| {
            if let Ok(executed) = job.await {
                ws.update(cx, |ws, cx| ws.db_results.update(cx, |r, cx| r.finish(0, executed, cx))).ok();
            }
        })
        .detach();
    }

    /// Compile (ALTER … COMPILE) from the view.
    fn recompile(&mut self, id: String, owner: String, object: DbObject, cx: &mut Context<Self>) {
        let model = self.db_model(cx);
        let Some(session) = model.read(cx).session(&id) else { return };
        let (o, name, kind, body) = (owner.clone(), object.name.clone(), object.kind, false);
        let job = session.run(move |db| {
            let mut errors = db.recompile(&o, &name, kind, body)?;
            if object.has_body {
                errors.extend(db.recompile(&o, &name, kind, true)?);
            }
            Ok(errors)
        });
        let shown = object.name.clone();
        cx.spawn(async move |ws, cx| {
            let result = job.await;
            ws.update(cx, |ws, cx| {
                match result {
                    Ok(errors) if errors.iter().all(|e| e.warning) => ws.toast(ToastKind::Success, format!("Compiled {shown}"), None, cx),
                    Ok(errors) => {
                        let first = errors.iter().find(|e| !e.warning).map(|e| format!("Line {}: {}", e.line, e.message));
                        ws.toast(ToastKind::Error, format!("{shown} has compile errors"), first, cx)
                    }
                    Err(err) => ws.toast(ToastKind::Error, format!("Couldn't compile {shown}"), Some(err), cx),
                }
                ws.db_model(cx).update(cx, |m, cx| m.refresh_schema(&id, &owner, cx));
            })
            .ok();
        })
        .detach();
    }

    // ---- Status bar ---------------------------------------------------------------------

    /// For a SQL file: the name of the connection its statements run on.
    pub(super) fn sql_connection_label(&self, path: Option<&Path>, cx: &App) -> Option<String> {
        if !path.is_some_and(database::is_sql_path) {
            return None;
        }
        let model = self.db_model(cx);
        let id = self.sql_connection(path, cx);
        Some(id.as_ref().and_then(|id| model.read(cx).config(id)).map_or_else(|| "No connection".to_string(), |c| c.name.clone()))
    }

    pub(super)     fn open_sql_connection_menu(&mut self, path: Option<PathBuf>, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.db_model(cx);
        let connections: Vec<(String, String, bool)> =
            model.read(cx).connections.iter().map(|c| (c.id.clone(), c.name.clone(), model.read(cx).is_connected(&c.id))).collect();
        let mut entries = Vec::new();
        let mut actions = Vec::new();
        for (id, name, connected) in connections {
            entries.push(MenuEntry::item(name, if connected { Icon::Database } else { Icon::Square }));
            let path = path.clone();
            actions.push(Some(MenuAction::Call(Rc::new(move |ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
                if let Some(path) = &path {
                    ws.sql_connections.insert(path.clone(), id.clone());
                    ws.sync_sql_gutters(cx);
                }
                if !ws.db_model(cx).read(cx).is_connected(&id) {
                    ws.connect_database(id.clone(), None, None, window, cx);
                }
                cx.notify();
            }))));
        }
        entries.push(MenuEntry::Separator);
        actions.push(None);
        entries.push(MenuEntry::item("New Connection…", Icon::Plus));
        actions.push(Some(MenuAction::Call(Rc::new(|ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| ws.open_connection_dialog(None, window, cx)))));
        self.show_context_menu(entries, actions, position, None, window, cx);
    }
}

/// The tab of an object's file, if open.
fn file_path_editor(ws: &Workspace, file: &ObjectFile, cx: &App) -> Option<Entity<Editor>> {
    let kind = ws.db_model(cx).read(cx).config(&file.connection)?.kind;
    ws.tab_for(&database::object_path(file, kind)?, cx)
}

/// Whether ⌘↩ in this editor runs SQL.
fn sql_editor(editor: &Entity<Editor>, cx: &App) -> bool {
    editor.read(cx).doc.read(cx).path.as_deref().is_some_and(database::is_sql_path)
}

/// Whether a statement changes what the Database view lists.
fn starts_ddl(sql: &str) -> bool {
    let first = sql.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    matches!(first.as_str(), "create" | "drop" | "alter" | "rename" | "comment")
}

/// An object file's problems as diagnostics (`rule`: the database's name).
fn problem_diagnostics(editor: &Entity<Editor>, problems: &[FileProblem], rule: &'static str, cx: &App) -> Vec<Diagnostic> {
    let text = editor.read(cx).text(cx);
    problems
        .iter()
        .map(|p| {
            let offset = p.offset.min(text.len());
            let offset = (0..=offset).rev().find(|&o| text.is_char_boundary(o)).unwrap_or(0);
            Diagnostic {
                range: word_at(&text, offset),
                severity: if p.warning { Severity::Warning } else { Severity::Error },
                message: Arc::from(p.message.as_str()),
                rule: Cow::Borrowed(rule),
                unnecessary: false,
            }
        })
        .collect()
}

/// The 1-based line of a byte offset in an editor's text.
fn line_of(editor: &Entity<Editor>, offset: usize, cx: &App) -> usize {
    let doc = editor.read(cx).doc.read(cx);
    doc.row_of(offset.min(doc.rope().len_bytes())) + 1
}

/// The word at `offset` (at least one character), for a problem's underline.
fn word_at(text: &str, offset: usize) -> Range<usize> {
    let offset = offset.min(text.len());
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$' || c == '#';
    let end = text[offset..].char_indices().find(|&(_, c)| !ident(c)).map_or(text.len(), |(i, _)| offset + i);
    if end > offset {
        return offset..end;
    }
    // Not on a word: the character there (unless it's a line end).
    match text[offset..].chars().next() {
        Some(c) if c != '\n' => offset..offset + c.len_utf8(),
        _ => offset..offset,
    }
}
