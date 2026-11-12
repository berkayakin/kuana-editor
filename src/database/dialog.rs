//! New Connection / Edit Connection: an Oracle connection's name, where it
//! is (host, port and service name or SID, or a connect string: EZConnect, a
//! TNS descriptor or a tnsnames.ora alias), the user, password and role.
//! Test Connection connects and disconnects; Connect saves and connects.
//!
//! It opens in a window of its own (`open`), like Settings, so it can be
//! moved aside to look something up: one at a time, centered over the
//! project window it was opened from, dragged by its title bar.

use gpui::{
    App, AppContext, Bounds, Context, DisplayId, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, Render, Subscription, Task,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, point, prelude::*, px, size,
};

use super::{ConnectionConfig, Role, oracle::Session};
use crate::{
    assets::Icon,
    editor::{Cancel, Editor, EditorEvent, Indent, Newline, Outdent},
    theme::Theme,
    ui::{chrome_bg, icon_el, input_box, text_button, tooltip_view},
    workspace::{self, CloseTab, CloseWindow, TITLEBAR_HEIGHT},
};

/// What happens with the filled-in connection: saving and connecting it, in
/// the project window the dialog came from.
pub type OnConnect = Box<dyn Fn(ConnectionConfig, Option<String>, &mut App)>;

/// The window's size: the dialog's fields and buttons, with room for a
/// two-line message.
const WINDOW_SIZE: (f32, f32) = (540., 560.);

/// Opens the connection dialog in its own window (`config`: the connection
/// to edit; `ask_password`: the reason to ask for its password), centered on
/// `display`. One at a time: when one is open, it's brought forward instead.
pub fn open(config: Option<ConnectionConfig>, ask_password: Option<String>, display: Option<DisplayId>, on_connect: OnConnect, cx: &mut App) {
    if let Some(existing) = cx.windows().into_iter().find_map(|w| w.downcast::<ConnectionWindow>()) {
        existing.update(cx, |_, window, _| window.activate_window()).ok();
        return;
    }
    let title = if config.is_some() { "Edit Connection" } else { "New Connection" };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(display, size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx))),
        display_id: display,
        titlebar: Some(TitlebarOptions { title: Some(title.into()), appears_transparent: true, traffic_light_position: Some(point(px(16.), px(13.))) }),
        is_resizable: false,
        is_minimizable: false,
        app_id: Some("dev.kuana.editor".into()),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        cx.new(|cx| {
            let dialog = cx.new(|cx| ConnectionDialog::new(config, ask_password, window, cx));
            let subscription = cx.subscribe_in(&dialog, window, move |_, _, event: &ConnectionDialogEvent, window, cx| {
                if let ConnectionDialogEvent::Connect { config, password } = event {
                    on_connect(config.clone(), password.clone(), cx);
                }
                window.remove_window();
            });
            ConnectionWindow { dialog, _subscription: subscription }
        })
    });
    if let Err(err) = opened {
        eprintln!("couldn't open the connection window: {err}");
    }
}

/// The window around the dialog.
pub struct ConnectionWindow {
    dialog: Entity<ConnectionDialog>,
    _subscription: Subscription,
}

impl Focusable for ConnectionWindow {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.dialog.focus_handle(cx)
    }
}

impl Render for ConnectionWindow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.dialog.clone())
    }
}

pub enum ConnectionDialogEvent {
    /// Save it (with the password typed, if any), then connect.
    Connect { config: ConnectionConfig, password: Option<String> },
    Dismissed,
}

enum Test {
    Running,
    Passed(String),
    Failed(String),
}

pub struct ConnectionDialog {
    id: String,
    editing: bool,
    name: Entity<Editor>,
    host: Entity<Editor>,
    port: Entity<Editor>,
    service: Entity<Editor>,
    connect_string: Entity<Editor>,
    user: Entity<Editor>,
    password: Entity<Editor>,
    use_sid: bool,
    advanced: bool,
    role: Role,
    save_password: bool,
    error: Option<String>,
    test: Option<Test>,
    _test_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ConnectionDialogEvent> for ConnectionDialog {}

impl ConnectionDialog {
    /// `config`: the connection to edit (None: a new one). `ask_password`:
    /// opened to connect one without a saved password, so the password field
    /// has the keyboard and the problem is said.
    pub fn new(config: Option<ConnectionConfig>, ask_password: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let field = |placeholder: &'static str, cx: &mut Context<Self>| cx.new(|cx| Editor::single_line(placeholder, cx));
        let name = field("My database", cx);
        let host = field("localhost", cx);
        let port = field("1521", cx);
        let service = field("FREEPDB1", cx);
        let connect_string = field("host:port/service, a (DESCRIPTION=…) or a tnsnames.ora alias", cx);
        let user = field("hr", cx);
        let password = cx.new(|cx| Editor::password("Password", cx));
        let editing = config.is_some();
        let config = config.unwrap_or_else(|| ConnectionConfig { id: super::new_id(), port: 1521, save_password: true, ..Default::default() });
        let set = |field: &Entity<Editor>, text: &str, cx: &mut Context<Self>| {
            field.update(cx, |e, cx| {
                e.set_text(text, cx);
                e.select_range(text.len()..text.len(), cx);
            })
        };
        set(&name, &config.name, cx);
        set(&host, &config.host, cx);
        set(&port, &if config.port == 0 { String::new() } else { config.port.to_string() }, cx);
        set(&service, &config.service, cx);
        set(&connect_string, &config.connect_string, cx);
        set(&user, &config.user, cx);
        if editing && config.save_password
            && let Some(saved) = super::saved_password(&config.id)
        {
            set(&password, &saved, cx);
        }
        let focus = if ask_password.is_some() { &password } else { &name };
        window.focus(&focus.focus_handle(cx));
        let on_edit = |this: &mut Self, _: Entity<Editor>, _: &EditorEvent, cx: &mut Context<Self>| {
            this.error = None;
            if matches!(this.test, Some(Test::Passed(_)) | Some(Test::Failed(_))) {
                this.test = None;
            }
            cx.notify();
        };
        let subscriptions = [&name, &host, &port, &service, &connect_string, &user, &password].into_iter().map(|f| cx.subscribe(f, on_edit)).collect();
        Self {
            id: config.id.clone(),
            editing,
            name,
            host,
            port,
            service,
            connect_string,
            user,
            password,
            use_sid: config.use_sid,
            advanced: !config.connect_string.trim().is_empty(),
            role: config.role,
            save_password: config.save_password,
            error: ask_password,
            test: None,
            _test_task: None,
            _subscriptions: subscriptions,
        }
    }

    fn text(field: &Entity<Editor>, cx: &App) -> String {
        field.read(cx).text(cx).trim().to_string()
    }

    /// The connection as filled in, or what's missing.
    fn config(&self, cx: &App) -> Result<ConnectionConfig, String> {
        let user = Self::text(&self.user, cx);
        let connect_string = if self.advanced { Self::text(&self.connect_string, cx) } else { String::new() };
        let host = Self::text(&self.host, cx);
        let service = Self::text(&self.service, cx);
        let port_text = Self::text(&self.port, cx);
        let port = if port_text.is_empty() { 1521 } else { port_text.parse::<u16>().map_err(|_| "The port is a number, like 1521".to_string())? };
        if self.advanced && connect_string.is_empty() {
            return Err("Enter a connect string".into());
        }
        if !self.advanced && (host.is_empty() || service.is_empty()) {
            return Err(format!("Enter the host and the {}", if self.use_sid { "SID" } else { "service name" }));
        }
        if user.is_empty() {
            return Err("Enter the user name".into());
        }
        let mut name = Self::text(&self.name, cx);
        if name.is_empty() {
            name = if self.advanced { format!("{user}@{connect_string}") } else { format!("{user}@{host}/{service}") };
        }
        Ok(ConnectionConfig {
            id: self.id.clone(),
            name,
            host,
            port,
            service,
            use_sid: self.use_sid,
            connect_string,
            user,
            role: self.role,
            save_password: self.save_password,
        })
    }

    fn password_text(&self, cx: &App) -> Option<String> {
        let text = self.password.read(cx).text(cx);
        (!text.is_empty()).then_some(text)
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        match self.config(cx) {
            Ok(config) => {
                let password = self.password_text(cx);
                if password.is_none() {
                    self.error = Some("Enter the password".into());
                    cx.notify();
                    return;
                }
                cx.emit(ConnectionDialogEvent::Connect { config, password });
            }
            Err(err) => {
                self.error = Some(err);
                cx.notify();
            }
        }
    }

    fn test(&mut self, cx: &mut Context<Self>) {
        let config = match self.config(cx) {
            Ok(config) => config,
            Err(err) => {
                self.error = Some(err);
                cx.notify();
                return;
            }
        };
        let password = self.password_text(cx).unwrap_or_default();
        self.test = Some(Test::Running);
        let connecting = Session::connect(&config, password);
        self._test_task = Some(cx.spawn(async move |this, cx| {
            let result = connecting.await.unwrap_or_else(|_| Err("The connection thread stopped".into()));
            this.update(cx, |this, cx| {
                this.test = Some(match result {
                    Ok(session) => Test::Passed(format!("Connected as {} · Oracle {}", session.user, session.version)),
                    Err(err) => Test::Failed(err),
                });
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_enter(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.connect(cx);
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ConnectionDialogEvent::Dismissed);
    }

    fn fields(&self) -> Vec<&Entity<Editor>> {
        if self.advanced {
            vec![&self.name, &self.connect_string, &self.user, &self.password]
        } else {
            vec![&self.name, &self.host, &self.port, &self.service, &self.user, &self.password]
        }
    }

    fn step_field(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self.fields();
        let at = fields.iter().position(|f| f.focus_handle(cx).is_focused(window)).unwrap_or(0);
        let next = if back { (at + fields.len() - 1) % fields.len() } else { (at + 1) % fields.len() };
        window.focus(&fields[next].focus_handle(cx));
    }

    fn on_tab(&mut self, _: &Indent, window: &mut Window, cx: &mut Context<Self>) {
        self.step_field(false, window, cx);
    }

    fn on_shift_tab(&mut self, _: &Outdent, window: &mut Window, cx: &mut Context<Self>) {
        self.step_field(true, window, cx);
    }
}

impl Focusable for ConnectionDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.name.focus_handle(cx)
    }
}

impl Render for ConnectionDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let label = |text: &'static str| div().text_size(px(12.)).font_weight(FontWeight::MEDIUM).text_color(theme.text_muted).child(text);
        let check = |id: &'static str, on: bool, text: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_2()
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(theme.text_muted)
                .hover(|s| s.text_color(theme.text))
                .child(icon_el(if on { Icon::SquareCheck } else { Icon::Square }, px(15.), if on { theme.accent.into() } else { theme.text_dim.into() }))
                .child(text)
        };
        let segment = |id: &'static str, text: &'static str, on: bool| {
            div()
                .id(id)
                .px_2p5()
                .h(px(26.))
                .flex()
                .items_center()
                .rounded(px(13.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(if on { theme.accent } else { theme.text_muted })
                .when(on, |el| el.bg(theme.accent_soft))
                .when(!on, |el| el.hover(|s| s.bg(theme.hover)))
                .child(text)
        };
        let (hint, hint_color): (String, Hsla) = match (&self.error, &self.test) {
            (Some(err), _) => (err.clone(), theme.error.into()),
            (None, Some(Test::Running)) => ("Connecting…".into(), theme.text_dim.into()),
            (None, Some(Test::Passed(text))) => (text.clone(), theme.success.into()),
            (None, Some(Test::Failed(err))) => (err.clone(), theme.error.into()),
            (None, None) => (
                if self.advanced { "EZConnect (host:port/service), a TNS descriptor, or an alias from tnsnames.ora (TNS_ADMIN)." } else { "Oracle Database 12c or newer. No Oracle Client needed." }.into(),
                theme.text_dim.into(),
            ),
        };
        let title = if self.editing { "Edit Connection" } else { "New Connection" };
        let where_fields = if self.advanced {
            div().flex().flex_col().gap_1p5().child(label("Connect string")).child(div().flex().child(input_box(&self.connect_string, Some(Icon::Link), window, cx)))
        } else {
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(div().flex().flex_col().gap_1p5().flex_1().child(label("Host")).child(div().flex().child(input_box(&self.host, None, window, cx))))
                        .child(div().flex().flex_col().gap_1p5().w(px(90.)).child(label("Port")).child(div().flex().child(input_box(&self.port, None, window, cx)))),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .pt_1()
                        .child(label(if self.use_sid { "SID" } else { "Service name" }))
                        .child(div().flex_1())
                        .child(segment("db-service-name", "Service name", !self.use_sid).on_click(cx.listener(|this, _, _, cx| {
                            this.use_sid = false;
                            cx.notify();
                        })))
                        .child(segment("db-sid", "SID", self.use_sid).on_click(cx.listener(|this, _, _, cx| {
                            this.use_sid = true;
                            cx.notify();
                        }))),
                )
                .child(div().flex().child(input_box(&self.service, None, window, cx)))
        };
        let testing = matches!(self.test, Some(Test::Running));
        // A title bar of its own, like Settings': the window drags by it and
        // (off macOS) has its buttons in it.
        let title_bar = div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .h(px(TITLEBAR_HEIGHT))
            .w_full()
            .when(cfg!(windows), |el| el.occlude())
            .child(
                workspace::window_drag_area("connection-title")
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(13.))
                    .text_color(theme.text_muted)
                    .child(title),
            )
            .when(!cfg!(target_os = "macos"), |el| el.child(workspace::window_controls(window, cx)));
        div()
            .id("connection-window")
            .key_context("ConnectionDialog")
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_shift_tab))
            // ⌘W closes the window; it has no tabs.
            .on_action(cx.listener(|_, _: &CloseTab, _, cx| cx.emit(ConnectionDialogEvent::Dismissed)))
            .on_action(cx.listener(|_, _: &CloseWindow, _, cx| cx.emit(ConnectionDialogEvent::Dismissed)))
            .size_full()
            .flex()
            .flex_col()
            .bg(chrome_bg(&theme, 0.6))
            .font_family(theme.ui_font.clone())
            .text_size(px(theme.ui_font_size))
            .text_color(theme.text)
            .child(title_bar)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .pt_1()
                    .pb_3()
                    .child(
                        div()
                            .size(px(34.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(9.))
                            .bg(theme.accent_soft)
                            .child(icon_el(Icon::Database, px(17.), theme.accent.into())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.text).child("Oracle Database"))
                            .child(div().text_size(px(12.)).text_color(theme.text_dim).child("Host, service and user, or a connect string")),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .px_4()
                    .pb_2()
                    .child(label("Name"))
                    .child(div().flex().child(input_box(&self.name, None, window, cx)))
                    .child(div().h(px(4.)))
                    .child(where_fields)
                    .child(
                        div()
                            .id("db-advanced")
                            .pt_0p5()
                            .text_size(px(12.))
                            .text_color(theme.accent)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .child(if self.advanced { "Use host, port and service name" } else { "Use a connect string or tnsnames.ora alias" })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.advanced = !this.advanced;
                                let field = if this.advanced { &this.connect_string } else { &this.host };
                                window.focus(&field.focus_handle(cx));
                                cx.notify();
                            })),
                    )
                    .child(div().h(px(4.)))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(div().flex().flex_col().gap_1p5().flex_1().child(label("User")).child(div().flex().child(input_box(&self.user, None, window, cx))))
                            .child(
                                div().flex().flex_col().gap_1p5().flex_1().child(label("Password")).child(div().flex().child(input_box(&self.password, None, window, cx))),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .pt_1p5()
                            .child(label("Role"))
                            .child(div().w(px(6.)))
                            .child(segment("db-role-default", "Default", self.role == Role::Default).on_click(cx.listener(|this, _, _, cx| {
                                this.role = Role::Default;
                                cx.notify();
                            })))
                            .child(segment("db-role-sysdba", "SYSDBA", self.role == Role::Sysdba).on_click(cx.listener(|this, _, _, cx| {
                                this.role = Role::Sysdba;
                                cx.notify();
                            })))
                            .child(segment("db-role-sysoper", "SYSOPER", self.role == Role::Sysoper).on_click(cx.listener(|this, _, _, cx| {
                                this.role = Role::Sysoper;
                                cx.notify();
                            })))
                            .child(div().flex_1())
                            .child(
                                check("db-save-password", self.save_password, "Save password")
                                    .tooltip(|_, cx| tooltip_view("Kept in the system keychain", cx))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_password = !this.save_password;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(div().min_h(px(34.)).pt_1().text_size(px(12.)).text_color(hint_color).line_clamp(2).child(hint)),
            )
            // The buttons stay at the window's foot.
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .child(
                        text_button("db-test", if testing { "Testing…" } else { "Test Connection" }, false, cx)
                            .when(testing, |el| el.opacity(0.5))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !matches!(this.test, Some(Test::Running)) {
                                    this.test(cx)
                                }
                            })),
                    )
                    .child(div().flex_1())
                    .child(text_button("db-cancel", "Cancel", false, cx).on_click(cx.listener(|_, _, _, cx| cx.emit(ConnectionDialogEvent::Dismissed))))
                    .child(text_button("db-connect", "Connect", true, cx).on_click(cx.listener(|this, _, _, cx| this.connect(cx)))),
            )
    }
}
