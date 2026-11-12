//! The Database view's and panel's place in the workspace (see
//! `crate::database`): connecting (asking for a password when none is
//! saved), opening objects' source in tabs, compiling them on Save, running
//! SQL from the editor (⌘↩ / Ctrl+Enter) into the Database panel, and the
//! view's right-click menus.

use std::{borrow::Cow, ops::Range, rc::Rc, sync::Arc};

use gpui::WeakEntity;

use super::*;
use crate::{
    database::{
        self, ConnectionConfig, DbObject, ObjectFile, ObjectKind,
        model::{DbModel, NEEDS_PASSWORD},
        oracle::{self, CompileError, Outcome},
        script,
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

/// Diagnostics lists the database fills on a document (by "server" name).
const COMPILE_PROBLEMS: &str = "oracle";
const RUN_PROBLEMS: &str = "oracle-run";

impl Workspace {
    fn db_model(&self, cx: &App) -> Entity<DbModel> {
        self.database.read(cx).model().clone()
    }

    pub(super) fn toggle_database(&mut self, _: &ToggleDatabase, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_sidebar && self.sidebar_mode == SidebarMode::Database {
            self.show_sidebar = false;
            self.focus_active(window, cx);
        } else {
            self.show_sidebar = true;
            self.sidebar_mode = SidebarMode::Database;
            window.focus(&self.database.focus_handle(cx));
        }
        self.save_state(cx);
        cx.notify();
    }

    fn show_database_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !(self.show_sidebar && self.sidebar_mode == SidebarMode::Database) {
            self.toggle_database(&ToggleDatabase, window, cx);
        }
    }

    pub(super) fn new_database_connection(&mut self, _: &NewDatabaseConnection, window: &mut Window, cx: &mut Context<Self>) {
        self.open_connection_dialog(None, None, window, cx);
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

    /// New Connection… / Edit Connection…, or asking for the password of `id`.
    /// The connection dialog, in a window of its own over this one; what it
    /// connects comes back here (`connection_entered`).
    fn open_connection_dialog(&mut self, id: Option<String>, ask: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
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
        crate::database::dialog::open(config, ask, display, on_connect, cx);
    }

    /// The connection dialog's Connect: save it, then connect.
    fn connection_entered(&mut self, config: ConnectionConfig, password: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.db_model(cx);
        let id = config.id.clone();
        if let Err(err) = model.update(cx, |m, cx| m.save(config, password.clone(), cx)) {
            self.toast(ToastKind::Error, "Couldn't save the connection", Some(err), cx);
        }
        self.show_database_view(window, cx);
        self.connect_database(id, password, window, cx);
    }

    /// Connects `id`, asking for its password when there's none to use.
    fn connect_database(&mut self, id: String, password: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.db_model(cx);
        let task = model.update(cx, |m, cx| m.connect(&id, password, cx));
        cx.spawn_in(window, async move |ws, cx| {
            let result = task.await;
            ws.update_in(cx, |ws, window, cx| match result {
                Ok(()) => ws.database.update(cx, |v, cx| v.expand_connection(&id, cx)),
                Err(err) if err == NEEDS_PASSWORD => ws.open_connection_dialog(Some(id), Some(err), window, cx),
                Err(err) if err == "Cancelled" => {}
                Err(err) => {
                    let name = ws.db_model(cx).read(cx).config(&id).map(|c| c.name.clone()).unwrap_or_default();
                    ws.toast(ToastKind::Error, format!("Couldn't connect to {name}"), Some(err), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn on_database_event(&mut self, event: &DatabaseEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            DatabaseEvent::NewConnection => self.open_connection_dialog(None, None, window, cx),
            DatabaseEvent::Connect(id) => self.connect_database(id.clone(), None, window, cx),
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

    fn end_transaction(&mut self, commit: bool, cx: &mut Context<Self>) {
        let Some((id, name)) = self.db_results.read(cx).connection.clone() else { return };
        let Some(session) = self.db_model(cx).read(cx).session(&id) else { return };
        let job = session.run(move |conn| if commit { oracle::commit(conn) } else { oracle::rollback(conn) });
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
        let Some(path) = database::object_path(&file) else { return };
        // Unsaved edits in its tab stay.
        if let Some(editor) = self.tab_for(&path, cx)
            && editor.read(cx).doc.read(cx).is_dirty()
        {
            self.open_path(path, window, cx);
            self.reveal_subprogram(&file_path_editor(self, &file, cx), at.as_deref(), window, cx);
            return;
        }
        let user = session.user.clone();
        let (name, kind) = (object.name.clone(), object.kind);
        let job = session.run(move |conn| oracle::source(conn, &user, &owner, &name, kind, body));
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

    /// After a file's Save: an object's file is compiled against its connection.
    pub(super) fn database_saved(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = editor.read(cx).doc.read(cx).path.clone() else { return };
        let Some(object) = database::object_of(&path) else { return };
        if !object.kind.compiles() {
            return;
        }
        let model = self.db_model(cx);
        let Some(session) = model.read(cx).session(&object.connection) else {
            let name = model.read(cx).config(&object.connection).map(|c| c.name.clone()).unwrap_or_else(|| "its connection".into());
            self.toast(ToastKind::Info, format!("Saved locally: connect to {name} to compile {}", object.name), None, cx);
            return;
        };
        let text = editor.read(cx).text(cx);
        let statements = script::split(&text);
        let Some(statement) = statements.iter().find(|s| s.kind != script::Kind::SqlPlus).cloned() else { return };
        let sql = statement.sql(&text);
        let first_line = text[..statement.range.start].matches('\n').count();
        // ALL_SOURCE's first line is the unit's from its keyword (PACKAGE …): where that is on the CREATE line.
        let lead = unit_column(&text[statement.range.clone()], object.kind.object_type());
        let (owner, name, source_type) = (object.owner.clone(), object.name.clone(), object.source_type().to_string());
        let job = session.run(move |conn| oracle::compile(conn, &sql, &owner, &name, &source_type));
        let weak = editor.downgrade();
        cx.spawn_in(window, async move |ws, cx| {
            let result = job.await;
            ws.update_in(cx, |ws, _, cx| {
                let Some(editor) = weak.upgrade() else { return };
                match result {
                    Ok(errors) => {
                        let problems = errors.iter().filter(|e| !e.warning).count();
                        let diagnostics = compile_diagnostics(&editor, &errors, first_line, lead, cx);
                        editor.update(cx, |e, cx| {
                            e.doc.as_mut(cx).set_server_diagnostics(COMPILE_PROBLEMS, diagnostics);
                            cx.notify();
                        });
                        if problems == 0 {
                            ws.toast(ToastKind::Success, format!("Compiled {}", object.name), None, cx);
                        } else {
                            let first = errors.iter().find(|e| !e.warning).map(|e| format!("Line {}: {}", e.line + first_line, e.message));
                            ws.toast(ToastKind::Error, format!("{} has {problems} compile error{}", object.name, if problems == 1 { "" } else { "s" }), first, cx);
                        }
                    }
                    Err(err) => ws.toast(ToastKind::Error, format!("Couldn't compile {}", object.name), Some(err), cx),
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
            Some(editor) if editor.focus_handle(cx).is_focused(window) && sql_editor(&editor, cx) => self.run_sql(false, window, cx),
            _ => cx.propagate(),
        }
    }

    pub(super) fn run_sql_statement(&mut self, _: &RunSqlStatement, window: &mut Window, cx: &mut Context<Self>) {
        self.run_sql(false, window, cx);
    }

    pub(super) fn run_sql_script(&mut self, _: &RunSqlScript, window: &mut Window, cx: &mut Context<Self>) {
        self.run_sql(true, window, cx);
    }

    /// New SQL Script: a scratch file for the selected (or only) connection.
    pub(super) fn new_sql_script(&mut self, _: &NewSqlScript, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.database.read(cx).selected_connection().or_else(|| self.db_model(cx).read(cx).connections.first().map(|c| c.id.clone())) else {
            self.open_connection_dialog(None, None, window, cx);
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
    /// the only one connected.
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
        (connected.len() == 1).then(|| connected[0].id.clone())
    }

    /// Runs the statement at the caret, the selection, or (`whole`) the file.
    fn run_sql(&mut self, whole: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor(cx) else { return };
        let path = editor.read(cx).doc.read(cx).path.clone();
        let model = self.db_model(cx);
        let Some(id) = self.sql_connection(path.as_deref(), cx) else {
            if model.read(cx).connections.is_empty() {
                self.open_connection_dialog(None, None, window, cx);
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
                    Ok(()) => ws.run_sql(whole, window, cx),
                    Err(err) if err == NEEDS_PASSWORD => ws.open_connection_dialog(Some(id), Some(err), window, cx),
                    Err(err) => ws.toast(ToastKind::Error, "Couldn't connect", Some(err), cx),
                })
                .ok();
            })
            .detach();
            return;
        };
        let text = editor.read(cx).text(cx);
        let selection = editor.read(cx).primary_selection().range();
        // The selection's statements (as typed), else the file's, else the caret's.
        let (base, script_text) = if !whole && !selection.is_empty() { (selection.start, text[selection.clone()].to_string()) } else { (0, text.clone()) };
        let mut statements = script::split(&script_text);
        if !whole && selection.is_empty() {
            let caret = editor.read(cx).primary_selection().head;
            statements = script::at(&statements, &script_text, caret).map(|ix| vec![statements[ix].clone()]).unwrap_or_default();
        }
        statements.retain(|s| s.kind != script::Kind::SqlPlus);
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
        let user = session.user.clone();
        cx.spawn_in(window, async move |ws, cx| {
            let mut problems: Vec<(Range<usize>, String)> = Vec::new();
            let mut changed_schema = false;
            for (ix, sql) in sqls.into_iter().enumerate() {
                let created = oracle::created_unit(&sql).is_some() || starts_ddl(&sql);
                let executed = session.run(move |conn| Ok(oracle::execute(conn, &sql))).await;
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
                            Diagnostic { range, severity: Severity::Error, message: Arc::from(message), rule: Cow::Borrowed("oracle"), unnecessary: false }
                        })
                        .collect();
                    editor.update(cx, |e, cx| {
                        e.doc.as_mut(cx).set_server_diagnostics(RUN_PROBLEMS, diagnostics);
                        cx.notify();
                    });
                }
                if changed_schema {
                    ws.db_model(cx).update(cx, |m, cx| m.refresh_schema(&id, &user, cx));
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
                    add(MenuEntry::item("Connect", Icon::Play), Some(Rc::new(move |ws, window, cx| ws.connect_database(i.clone(), None, window, cx))));
                    let i = id.clone();
                    add(MenuEntry::item("New SQL Script", Icon::FilePlus), Some(Rc::new(move |ws, window, cx| ws.new_script_for(i.clone(), window, cx))));
                }
                add(MenuEntry::Separator, None);
                let i = id.clone();
                add(MenuEntry::item("Edit Connection…", Icon::Pencil), Some(Rc::new(move |ws, window, cx| ws.open_connection_dialog(Some(i.clone()), None, window, cx))));
                let i = id.clone();
                add(MenuEntry::item("Delete Connection…", Icon::Trash).danger(), Some(Rc::new(move |ws, window, cx| ws.delete_connection(i.clone(), window, cx))));
            }
            NodeId::Object(..) | NodeId::Body(..) | NodeId::Detail(..) => {
                let Some((conn, owner, object)) = self.database.read(cx).object_at(&node, cx) else { return };
                let qualified = format!("{owner}.{}", object.name);
                if object.kind.has_columns() {
                    let (c, o, ob) = (conn.clone(), owner.clone(), object.clone());
                    add(MenuEntry::item("Select Rows", Icon::Table), Some(Rc::new(move |ws, window, cx| ws.select_rows(c.clone(), &o, &ob, window, cx))));
                }
                let body_too = object.has_body;
                let (c, o, ob) = (conn.clone(), owner.clone(), object.clone());
                let label = match object.kind {
                    ObjectKind::Package | ObjectKind::Type => "Open Specification",
                    kind if kind.compiles() => "Open Source",
                    _ => "Show DDL",
                };
                add(MenuEntry::item(label, Icon::FileCode), Some(Rc::new(move |ws, window, cx| ws.open_db_object(c.clone(), o.clone(), ob.clone(), false, None, window, cx))));
                if body_too {
                    let (c, o, ob) = (conn.clone(), owner.clone(), object.clone());
                    add(MenuEntry::item("Open Body", Icon::FileCode), Some(Rc::new(move |ws, window, cx| ws.open_db_object(c.clone(), o.clone(), ob.clone(), true, None, window, cx))));
                }
                if object.kind.compiles() {
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
        let sql = format!("SELECT * FROM \"{owner}\".\"{}\" FETCH FIRST {} ROWS ONLY", object.name, oracle::MAX_ROWS);
        let name = model.read(cx).config(&id).map(|c| c.name.clone()).unwrap_or_default();
        self.db_results.update(cx, |r, cx| r.start((id, name), vec![sql.clone()], cx));
        self.db_run = None;
        self.has_db_results = true;
        self.bottom = Some(BottomPanel::Database);
        cx.notify();
        let job = session.run(move |conn| Ok(oracle::execute(conn, &sql)));
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
        let job = session.run(move |conn| {
            let mut errors = oracle::recompile(conn, &o, &name, kind, body)?;
            if object.has_body {
                errors.extend(oracle::recompile(conn, &o, &name, kind, true)?);
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
                }
                if !ws.db_model(cx).read(cx).is_connected(&id) {
                    ws.connect_database(id.clone(), None, window, cx);
                }
                cx.notify();
            }))));
        }
        entries.push(MenuEntry::Separator);
        actions.push(None);
        entries.push(MenuEntry::item("New Connection…", Icon::Plus));
        actions.push(Some(MenuAction::Call(Rc::new(|ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| ws.open_connection_dialog(None, None, window, cx)))));
        self.show_context_menu(entries, actions, position, None, window, cx);
    }
}

/// The tab of an object's file, if open.
fn file_path_editor(ws: &Workspace, file: &ObjectFile, cx: &App) -> Option<Entity<Editor>> {
    ws.tab_for(&database::object_path(file)?, cx)
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

/// Where the unit's own text starts on its CREATE line (its keyword's byte column).
fn unit_column(statement: &str, keyword: &str) -> usize {
    let line = statement.lines().next().unwrap_or("");
    let upper = line.to_ascii_uppercase();
    let first = keyword.split(' ').next().unwrap_or(keyword);
    upper.find(&format!(" {first}")).map_or(0, |i| i + 1)
}

/// ALL_ERRORS rows as the file's problems: `first_line` is the statement's
/// line in the file, `lead` the unit keyword's column on it.
fn compile_diagnostics(editor: &Entity<Editor>, errors: &[CompileError], first_line: usize, lead: usize, cx: &App) -> Vec<Diagnostic> {
    let doc = editor.read(cx).doc.read(cx);
    let text = doc.rope().to_string();
    let lines = doc.rope().len_lines();
    errors
        .iter()
        .map(|e| {
            let row = (first_line + e.line - 1).min(lines.saturating_sub(1));
            let start = doc.line_start(row);
            let line = doc.line_text(row);
            let column = if e.line == 1 { lead } else { 0 };
            let byte = column + line[column.min(line.len())..].char_indices().nth(e.column.saturating_sub(1)).map_or(line.len().saturating_sub(column), |(i, _)| i);
            let range = word_at(&text, start + byte.min(line.len()));
            Diagnostic {
                range,
                severity: if e.warning { Severity::Warning } else { Severity::Error },
                message: Arc::from(e.message.as_str()),
                rule: Cow::Borrowed("oracle"),
                unnecessary: false,
            }
        })
        .collect()
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
