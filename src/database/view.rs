//! The Database view (a sidebar view beside Files and Commit): connections,
//! and once connected, the user's schema grouped by kind (Tables, Views,
//! Packages, …), then Other Schemas. Lists load as they open. A click on a
//! package, procedure, function, trigger, type or view opens its source
//! (`DatabaseEvent::Open`); a table opens to its columns, and its DDL on a
//! double click. Typing in the filter field narrows every loaded list.

use std::{collections::HashSet, ops::Range};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, MouseButton, MouseDownEvent, Render, ScrollStrategy,
    SharedString, Subscription, UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use super::{
    DbObject, ObjectKind,
    model::{DbModel, Details, Live, Loaded},
};
use crate::{
    assets::Icon,
    editor::{Cancel, Editor, EditorEvent, MoveDown, MoveLeft, MoveRight, MoveUp, Newline},
    theme::Theme,
    ui::{HoverFill, icon_el, tooltip_view},
};

/// A row of the tree, by what it stands for (stable across reloads, so
/// expansion and selection survive them).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NodeId {
    Connection(String),
    /// "Other Schemas" under a connection.
    Schemas(String),
    Schema(String, String),
    Group(String, String, ObjectKind),
    Object(String, String, ObjectKind, String),
    /// A package's or type's body, under it.
    Body(String, String, ObjectKind, String),
    /// A column or a subprogram: (connection, owner, object, its name).
    Detail(String, String, String, String),
    /// "Loading…", an error, "None".
    Message(String, usize),
}

impl NodeId {
    pub fn connection(&self) -> &str {
        match self {
            NodeId::Connection(c)
            | NodeId::Schemas(c)
            | NodeId::Schema(c, _)
            | NodeId::Group(c, _, _)
            | NodeId::Object(c, _, _, _)
            | NodeId::Body(c, _, _, _)
            | NodeId::Detail(c, _, _, _)
            | NodeId::Message(c, _) => c,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Normal,
    Muted,
    Error,
    Success,
    Accent,
}

struct Row {
    id: NodeId,
    depth: usize,
    icon: Option<(Icon, Tone)>,
    label: SharedString,
    detail: Option<SharedString>,
    tone: Tone,
    expandable: bool,
    expanded: bool,
}

pub enum DatabaseEvent {
    NewConnection,
    /// Connect (the workspace asks for a password when none is saved).
    Connect(String),
    /// Open an object's source (`body`: a package's or type's body), at a
    /// subprogram when given.
    Open { connection: String, owner: String, object: DbObject, body: bool, at: Option<String> },
    Menu { node: NodeId, position: gpui::Point<gpui::Pixels> },
    /// Escape: back to the editor.
    Leave,
}

pub struct DatabaseView {
    model: Entity<DbModel>,
    focus: FocusHandle,
    filter: Entity<Editor>,
    expanded: HashSet<NodeId>,
    selected: Option<NodeId>,
    rows: Vec<Row>,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DatabaseEvent> for DatabaseView {}

impl DatabaseView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let model = DbModel::global(cx);
        let filter = cx.new(|cx| Editor::single_line("Filter objects", cx));
        let subscriptions = vec![
            cx.observe(&model, |this, _, cx| {
                this.rebuild(cx);
                cx.notify();
            }),
            cx.subscribe(&filter, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::Edited) {
                    this.rebuild(cx);
                    cx.notify();
                }
            }),
        ];
        let mut this = Self {
            model,
            focus: cx.focus_handle(),
            filter,
            expanded: HashSet::new(),
            selected: None,
            rows: Vec::new(),
            scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        this.rebuild(cx);
        this
    }

    pub fn model(&self) -> &Entity<DbModel> {
        &self.model
    }

    /// The connection of the selected row (what new scripts and ⌘↩ use when
    /// a file has none of its own).
    pub fn selected_connection(&self) -> Option<String> {
        self.selected.as_ref().map(|n| n.connection().to_string())
    }

    /// Opens a connection's node (after connecting).
    pub fn expand_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        self.expanded.insert(NodeId::Connection(id.to_string()));
        self.selected = Some(NodeId::Connection(id.to_string()));
        self.rebuild(cx);
        cx.notify();
    }

    fn filter_text(&self, cx: &App) -> String {
        self.filter.read(cx).text(cx).trim().to_lowercase()
    }

    /// The rows as the model and the expanded nodes say, asking the model
    /// for what opened nodes need.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let filter = self.filter_text(cx);
        let mut rows = Vec::new();
        let mut wanted: Vec<NodeId> = Vec::new();
        let model = self.model.read(cx);
        for config in &model.connections {
            let id = NodeId::Connection(config.id.clone());
            let live = model.live.get(&config.id);
            let (icon_tone, detail) = match live {
                Some(Live::Connected(session)) => (Tone::Success, Some(session.user.clone())),
                Some(Live::Connecting) => (Tone::Muted, Some("connecting…".to_string())),
                Some(Live::Failed(_)) => (Tone::Error, Some(config.address())),
                None => (Tone::Muted, Some(config.address())),
            };
            let connected = matches!(live, Some(Live::Connected(_)));
            let expanded = connected && (self.expanded.contains(&id) || !filter.is_empty());
            rows.push(Row {
                id: id.clone(),
                depth: 0,
                icon: Some((Icon::Database, icon_tone)),
                label: config.name.clone().into(),
                detail: detail.map(Into::into),
                tone: Tone::Normal,
                expandable: true,
                expanded,
            });
            if let Some(Live::Failed(err)) = live
                && self.expanded.contains(&id)
            {
                rows.push(message_row(&config.id, 0, 1, err, Tone::Error));
            }
            if !expanded {
                continue;
            }
            let Some(Live::Connected(session)) = live else { continue };
            let user = session.user.clone();
            self.schema_rows(&config.id, &user, 1, &filter, &mut rows, &mut wanted, cx);
            // Other schemas, while not filtering.
            if filter.is_empty() {
                let others = NodeId::Schemas(config.id.clone());
                let open = self.expanded.contains(&others);
                rows.push(Row {
                    id: others.clone(),
                    depth: 1,
                    icon: Some((Icon::FolderOpen, Tone::Muted)),
                    label: "Other Schemas".into(),
                    detail: None,
                    tone: Tone::Normal,
                    expandable: true,
                    expanded: open,
                });
                if open {
                    match self.model.read(cx).schemas.get(&config.id) {
                        None => {
                            wanted.push(others.clone());
                            rows.push(message_row(&config.id, 2, 2, "Loading…", Tone::Muted));
                        }
                        Some(Loaded::Loading) => rows.push(message_row(&config.id, 2, 2, "Loading…", Tone::Muted)),
                        Some(Loaded::Failed(err)) => rows.push(message_row(&config.id, 2, 2, err, Tone::Error)),
                        Some(Loaded::Ready(schemas)) => {
                            for schema in schemas.clone().iter().filter(|s| **s != user) {
                                let node = NodeId::Schema(config.id.clone(), schema.clone());
                                let open = self.expanded.contains(&node);
                                rows.push(Row {
                                    id: node,
                                    depth: 2,
                                    icon: Some((Icon::Folder, Tone::Muted)),
                                    label: schema.clone().into(),
                                    detail: None,
                                    tone: Tone::Normal,
                                    expandable: true,
                                    expanded: open,
                                });
                                if open {
                                    self.schema_rows(&config.id, schema, 3, &filter, &mut rows, &mut wanted, cx);
                                }
                            }
                        }
                    }
                }
            }
        }
        self.rows = rows;
        // Ask for what opened nodes need, after drawing (the model notifies).
        if !wanted.is_empty() {
            let model = self.model.clone();
            cx.defer(move |cx| {
                model.update(cx, |model, cx| {
                    for node in wanted {
                        match node {
                            NodeId::Schemas(c) => model.load_schemas(&c, cx),
                            NodeId::Schema(c, owner) => model.load_objects(&c, &owner, cx),
                            NodeId::Object(c, owner, kind, name) => {
                                let object = model.objects.get(&(c.clone(), owner.clone())).and_then(|l| match l {
                                    Loaded::Ready(list) => list.iter().find(|o| o.name == name && o.kind == kind).cloned(),
                                    _ => None,
                                });
                                if let Some(object) = object {
                                    model.load_details(&c, &owner, &object, cx);
                                }
                            }
                            _ => {}
                        }
                    }
                });
            });
        }
    }

    /// A schema's groups and, for open ones, their objects.
    #[allow(clippy::too_many_arguments)]
    fn schema_rows(&self, conn: &str, owner: &str, depth: usize, filter: &str, rows: &mut Vec<Row>, wanted: &mut Vec<NodeId>, cx: &App) {
        let model = self.model.read(cx);
        let objects = match model.objects.get(&(conn.to_string(), owner.to_string())) {
            None => {
                wanted.push(NodeId::Schema(conn.to_string(), owner.to_string()));
                rows.push(message_row(conn, depth, rows.len(), "Loading…", Tone::Muted));
                return;
            }
            Some(Loaded::Loading) => {
                rows.push(message_row(conn, depth, rows.len(), "Loading…", Tone::Muted));
                return;
            }
            Some(Loaded::Failed(err)) => {
                rows.push(message_row(conn, depth, rows.len(), err, Tone::Error));
                return;
            }
            Some(Loaded::Ready(objects)) => objects,
        };
        if objects.is_empty() {
            rows.push(message_row(conn, depth, rows.len(), "No objects", Tone::Muted));
            return;
        }
        for kind in ObjectKind::ALL {
            let matching: Vec<&DbObject> =
                objects.iter().filter(|o| o.kind == kind && (filter.is_empty() || o.name.to_lowercase().contains(filter))).collect();
            if matching.is_empty() {
                continue;
            }
            let group = NodeId::Group(conn.to_string(), owner.to_string(), kind);
            let open = !filter.is_empty() || self.expanded.contains(&group);
            let invalid = matching.iter().filter(|o| !o.valid || (o.has_body && !o.body_valid)).count();
            rows.push(Row {
                id: group,
                depth,
                icon: None,
                label: kind.plural().into(),
                detail: Some(if invalid > 0 { format!("{}  ·  {invalid} invalid", matching.len()) } else { matching.len().to_string() }.into()),
                tone: Tone::Normal,
                expandable: true,
                expanded: open,
            });
            if !open {
                continue;
            }
            for object in matching {
                let node = NodeId::Object(conn.to_string(), owner.to_string(), kind, object.name.clone());
                let has_children = kind.has_columns() || matches!(kind, ObjectKind::Package | ObjectKind::Type);
                let open = has_children && self.expanded.contains(&node);
                let valid = object.valid && (!object.has_body || object.body_valid);
                rows.push(Row {
                    id: node.clone(),
                    depth: depth + 1,
                    icon: Some((kind.icon(), if valid { Tone::Accent } else { Tone::Error })),
                    label: object.name.clone().into(),
                    detail: (!valid).then(|| "invalid".into()),
                    tone: if valid { Tone::Normal } else { Tone::Error },
                    expandable: has_children,
                    expanded: open,
                });
                if open {
                    self.object_rows(conn, owner, object, depth + 2, rows, wanted, cx);
                }
            }
        }
    }

    /// Under an open object: a body, then columns or subprograms.
    #[allow(clippy::too_many_arguments)]
    fn object_rows(&self, conn: &str, owner: &str, object: &DbObject, depth: usize, rows: &mut Vec<Row>, wanted: &mut Vec<NodeId>, cx: &App) {
        if object.has_body {
            rows.push(Row {
                id: NodeId::Body(conn.to_string(), owner.to_string(), object.kind, object.name.clone()),
                depth,
                icon: Some((Icon::FileCode, if object.body_valid { Tone::Muted } else { Tone::Error })),
                label: "Body".into(),
                detail: (!object.body_valid).then(|| "invalid".into()),
                tone: if object.body_valid { Tone::Normal } else { Tone::Error },
                expandable: false,
                expanded: false,
            });
        }
        let key = (conn.to_string(), owner.to_string(), object.name.clone());
        match self.model.read(cx).details.get(&key) {
            None => {
                wanted.push(NodeId::Object(conn.to_string(), owner.to_string(), object.kind, object.name.clone()));
                rows.push(message_row(conn, depth, rows.len(), "Loading…", Tone::Muted));
            }
            Some(Loaded::Loading) => rows.push(message_row(conn, depth, rows.len(), "Loading…", Tone::Muted)),
            Some(Loaded::Failed(err)) => rows.push(message_row(conn, depth, rows.len(), err, Tone::Error)),
            Some(Loaded::Ready(Details::Columns(columns))) => {
                for column in columns {
                    rows.push(Row {
                        id: NodeId::Detail(conn.to_string(), owner.to_string(), object.name.clone(), column.name.clone()),
                        depth,
                        icon: Some(if column.primary { (Icon::Key, Tone::Accent) } else { (Icon::ViewColumn, Tone::Muted) }),
                        label: column.name.clone().into(),
                        detail: Some(if column.nullable { column.data_type.clone() } else { format!("{} not null", column.data_type) }.into()),
                        tone: Tone::Normal,
                        expandable: false,
                        expanded: false,
                    });
                }
            }
            Some(Loaded::Ready(Details::Subprograms(subprograms))) => {
                for sub in subprograms {
                    rows.push(Row {
                        id: NodeId::Detail(conn.to_string(), owner.to_string(), object.name.clone(), sub.name.clone()),
                        depth,
                        icon: Some((Icon::Function, Tone::Muted)),
                        label: sub.name.clone().into(),
                        detail: Some(if sub.function { "function" } else { "procedure" }.into()),
                        tone: Tone::Normal,
                        expandable: false,
                        expanded: false,
                    });
                }
            }
        }
    }

    /// The object a row is (or is under).
    fn object_of(&self, node: &NodeId, cx: &App) -> Option<(String, String, DbObject)> {
        let (conn, owner, kind, name) = match node {
            NodeId::Object(c, o, k, n) | NodeId::Body(c, o, k, n) => (c, o, Some(*k), n),
            NodeId::Detail(c, o, n, _) => (c, o, None, n),
            _ => return None,
        };
        let model = self.model.read(cx);
        let Some(Loaded::Ready(list)) = model.objects.get(&(conn.clone(), owner.clone())) else { return None };
        let object = list.iter().find(|o| o.name == *name && kind.is_none_or(|k| k == o.kind))?;
        Some((conn.clone(), owner.clone(), object.clone()))
    }

    /// What a menu or an action on `node` is about.
    pub fn object_at(&self, node: &NodeId, cx: &App) -> Option<(String, String, DbObject)> {
        self.object_of(node, cx)
    }

    fn set_expanded(&mut self, node: NodeId, open: bool, cx: &mut Context<Self>) {
        if open {
            self.expanded.insert(node);
        } else {
            self.expanded.remove(&node);
        }
        self.rebuild(cx);
        cx.notify();
    }

    /// A row's click: open or toggle, as its kind says.
    fn activate(&mut self, ix: usize, double: bool, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix) else { return };
        let (id, expanded, expandable) = (row.id.clone(), row.expanded, row.expandable);
        self.selected = Some(id.clone());
        match &id {
            NodeId::Connection(conn) => {
                if self.model.read(cx).is_connected(conn) {
                    self.set_expanded(id.clone(), !expanded, cx);
                } else if !matches!(self.model.read(cx).live.get(conn), Some(Live::Connecting)) {
                    self.expanded.insert(id.clone());
                    cx.emit(DatabaseEvent::Connect(conn.clone()));
                }
            }
            NodeId::Object(..) => {
                let Some((connection, owner, object)) = self.object_of(&id, cx) else { return };
                // Tables open to their columns; their DDL on a double click.
                if object.kind.compiles() || double {
                    cx.emit(DatabaseEvent::Open { connection, owner, object, body: false, at: None });
                } else if expandable {
                    self.set_expanded(id.clone(), !expanded, cx);
                }
            }
            NodeId::Body(..) => {
                if let Some((connection, owner, object)) = self.object_of(&id, cx) {
                    cx.emit(DatabaseEvent::Open { connection, owner, object, body: true, at: None });
                }
            }
            NodeId::Detail(_, _, _, name) => {
                if let Some((connection, owner, object)) = self.object_of(&id, cx)
                    && matches!(object.kind, ObjectKind::Package | ObjectKind::Type)
                {
                    let body = object.has_body;
                    cx.emit(DatabaseEvent::Open { connection, owner, object, body, at: Some(name.clone()) });
                }
            }
            NodeId::Message(..) => {}
            _ if expandable => self.set_expanded(id.clone(), !expanded, cx),
            _ => {}
        }
        cx.notify();
    }

    fn select_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows.get(ix) {
            self.selected = Some(row.id.clone());
            self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
            cx.notify();
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.rows.iter().position(|r| r.id == *selected)
    }

    fn on_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected_index().map_or(0, |i| i.saturating_sub(1));
        self.select_index(ix, cx);
    }

    fn on_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected_index().map_or(0, |i| (i + 1).min(self.rows.len().saturating_sub(1)));
        self.select_index(ix, cx);
    }

    fn on_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected_index() else { return };
        let row = &self.rows[ix];
        if row.expanded {
            let id = row.id.clone();
            self.set_expanded(id, false, cx);
        } else if let Some(parent) = (0..ix).rev().find(|&i| self.rows[i].depth < row.depth) {
            self.select_index(parent, cx);
        }
    }

    fn on_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.selected_index() else { return };
        let row = &self.rows[ix];
        if row.expandable && !row.expanded {
            if let NodeId::Connection(_) = row.id {
                self.activate(ix, false, cx);
            } else {
                let id = row.id.clone();
                self.set_expanded(id, true, cx);
            }
        }
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        // Nothing selected yet: the first row (a connection).
        let ix = self.selected_index().unwrap_or(0);
        if ix < self.rows.len() {
            self.activate(ix, true, cx);
        }
    }

    fn on_escape(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if !self.filter.read(cx).text(cx).is_empty() {
            self.filter.update(cx, |e, cx| e.set_text("", cx));
            window.focus(&self.focus);
            return;
        }
        cx.emit(DatabaseEvent::Leave);
    }

    fn render_row(&self, ix: usize, focused: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::get(cx);
        let row = &self.rows[ix];
        let selected = self.selected.as_ref() == Some(&row.id);
        let color = |tone: Tone| -> Hsla {
            match tone {
                Tone::Normal => theme.text.into(),
                Tone::Muted => theme.text_dim.into(),
                Tone::Error => theme.error.into(),
                Tone::Success => theme.success.into(),
                Tone::Accent => theme.text_muted.into(),
            }
        };
        let chevron = if row.expandable {
            let (id, expanded) = (row.id.clone(), row.expanded);
            div()
                .id(("db-chevron", ix))
                .flex_shrink_0()
                .child(icon_el(if expanded { Icon::ChevronDown } else { Icon::ChevronRight }, px(14.), theme.text_dim.into()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.focus);
                    this.selected = Some(id.clone());
                    if let NodeId::Connection(conn) = &id
                        && !this.model.read(cx).is_connected(conn)
                    {
                        this.activate(ix, false, cx);
                        return;
                    }
                    this.set_expanded(id.clone(), !expanded, cx);
                }))
                .into_any_element()
        } else {
            div().size(px(14.)).flex_shrink_0().into_any_element()
        };
        let group = matches!(row.id, NodeId::Group(..) | NodeId::Schemas(..));
        let selected_bg = if focused { theme.accent_soft } else { theme.active };
        let id = row.id.clone();
        div()
            .id(("db-row", ix))
            .w_full()
            .h(px(24.))
            .flex()
            .items_center()
            .gap_1p5()
            .pl(px(6. + 14. * row.depth as f32))
            .pr_2()
            .cursor_pointer()
            .when(selected, |el| el.bg(selected_bg))
            .row_hover(theme.hover)
            .child(chevron)
            .children(row.icon.map(|(icon, tone)| icon_el(icon, px(14.), color(tone))))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(color(row.tone))
                    .when(group, |el| el.font_weight(FontWeight::MEDIUM))
                    .when(matches!(row.id, NodeId::Message(..)), |el| el.text_size(px(12.)))
                    .child(row.label.clone()),
            )
            .children(row.detail.clone().map(|d| {
                div()
                    .flex_shrink()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.5))
                    .text_color(if matches!(row.tone, Tone::Error) { Hsla::from(theme.error).alpha(0.8) } else { theme.text_dim.into() })
                    .child(d)
            }))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                window.focus(&this.focus);
                this.activate(ix, event.click_count() > 1, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus);
                    this.selected = Some(id.clone());
                    cx.emit(DatabaseEvent::Menu { node: id.clone(), position: event.position });
                    cx.notify();
                }),
            )
            .into_any_element()
    }
}

fn message_row(conn: &str, depth: usize, unique: usize, text: &str, tone: Tone) -> Row {
    Row {
        id: NodeId::Message(conn.to_string(), unique),
        depth,
        icon: None,
        label: text.to_string().into(),
        detail: None,
        tone,
        expandable: false,
        expanded: false,
    }
}

impl Focusable for DatabaseView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DatabaseView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let focused = self.focus.contains_focused(window, cx);
        let empty = self.model.read(cx).connections.is_empty();
        let any_connected = !self.model.read(cx).connected().is_empty();
        let header = div()
            .flex()
            .items_center()
            .gap_1()
            .h(px(30.))
            .px_1()
            .mb_1()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border_subtle)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl_1p5()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child("DATABASE"),
            )
            .child(
                crate::ui::icon_button("db-add", Icon::Plus, theme.text_muted.into(), cx)
                    .tooltip(|_, cx| tooltip_view("New Connection…", cx))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DatabaseEvent::NewConnection))),
            )
            .when(any_connected, |el| {
                el.child(
                    crate::ui::icon_button("db-refresh", Icon::Refresh, theme.text_muted.into(), cx)
                        .tooltip(|_, cx| tooltip_view("Refresh", cx))
                        .on_click(cx.listener(|this, _, _, cx| {
                            let ids: Vec<String> = this.model.read(cx).connected().iter().map(|c| c.id.clone()).collect();
                            this.model.update(cx, |m, cx| ids.iter().for_each(|id| m.refresh(id, cx)));
                        })),
                )
            });
        let body = if empty {
            div()
                .flex()
                .flex_col()
                .gap_3()
                .p_4()
                .text_color(theme.text_muted)
                .child("No database connections.")
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.text_dim)
                        .child("Connect to an Oracle database to browse its tables, packages, procedures and functions, edit and compile them, and run SQL."),
                )
                .child(
                    crate::ui::text_button("db-new-connection", "New Connection…", true, cx)
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DatabaseEvent::NewConnection))),
                )
                .into_any_element()
        } else {
            let rows = self.rows.len();
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "db-tree",
                        rows,
                        cx.processor(move |this, range: Range<usize>, _, cx| {
                            range.filter(|&ix| ix < this.rows.len()).map(|ix| this.render_row(ix, focused, cx)).collect()
                        }),
                    )
                    .track_scroll(self.scroll.clone())
                    .size_full()
                    .pb_2(),
                )
                .child(crate::ui::list_scrollbar("db-scrollbar", &self.scroll, cx))
                .into_any_element()
        };
        div()
            .key_context("DatabaseView")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_left))
            .on_action(cx.listener(Self::on_right))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_escape))
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .when(any_connected, |el| {
                el.child(div().flex().px_1p5().pb_1p5().flex_shrink_0().child(crate::ui::input_box(&self.filter, Some(Icon::Search), window, cx).h(px(28.))))
            })
            .child(body)
    }
}
