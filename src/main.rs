// A GUI app on Windows: no console window opens with it.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod about;
mod autopilot;
mod python;
mod assets;
mod branch_picker;
mod busy;
mod cli;
mod clone_dialog;
mod conflict;
mod contrast;
mod context_menu;
mod dap;
mod debugger;
mod diff_view;
mod document;
mod document_diff;
mod document_store;
mod downloads;
mod editor;
mod extensions;
mod editorconfig;
mod encoding;
mod file_history;
mod file_icons;
mod find_bar;
mod fold;
mod format;
mod git;
mod git_log;
mod hierarchy;
mod git_panel;
mod image_view;
mod instance;
mod input_dialog;
mod keymap;
mod languages;
mod local_history;
mod lint;
mod lsp;
mod markdown_preview;
mod onboarding;
mod pane_item;
mod paths;
mod panic_log;
mod picker;
mod preview;
mod problems_panel;
mod project;
mod project_settings;
mod project_tree;
mod process;
mod trust;
mod trust_dialog;
mod preferences;
mod run;
mod schemas;
mod project_search;
mod push_dialog;
mod remotes_dialog;
mod results_panel;
mod search;
mod settings;
mod snippets;
mod state;
mod syntax;
mod terminal;
mod test_run;
mod theme;
mod theme_picker;
mod transition;
mod ui;
mod updater;
mod wasi_sdk;
mod watcher;
mod workspace;
mod wrap;

use std::path::PathBuf;

use gpui::{App, Application, Menu, MenuItem};

use crate::{
    editor::*,
    workspace::{
        CheckForUpdates, CloseTab, CloseWindow, DecreaseFontSize, FileStructure, Find, IncreaseFontSize, OpenSettings, OpenExtensions, ProjectTrust,
        ResetFontSize, Rerun, RunConfiguration, SplitDown, SplitRight, StopRun, ShowFileHistory, ToggleGitLog, ToggleTerminal, FindAction, FindInFiles, GitBranches, GitStash, ManageRemotes, GitClone, GitFetch,
        GitInit, GitPull, GitPush, GoToFile, GoToLine, MinimizeWindow, ZoomWindow, NewFile, NewWindow, NextTab, OpenFile,
        OpenFolder, OpenFolderInThisWindow, OpenRemote, PrevTab, Quit, RecentFiles, RecentProjects,
        Replace, ReplaceInFiles, Save, SaveAs, SearchEverywhere, SelectTheme, ShowCommit,
        ToggleProblems, ToggleResults, ToggleSidebar, WelcomeSetup, GoToSymbol, InstallShellCommand,
        DebugConfiguration, Resume, PauseProgram, StepOver, StepInto, StepOut, AttachToProcess, AddConditionalBreakpoint, AddLogpoint,
    },
};

fn menus() -> Vec<Menu> {
    menus_for(cfg!(target_os = "macos"))
}

/// The app's menus: macOS's menu bar, or the title bar's ☰ elsewhere
/// (`Workspace::open_app_menu`), where there's no app menu: its items go
/// under File and Help, as Windows and Linux apps have them.
fn menus_for(mac: bool) -> Vec<Menu> {
    let mut file = vec![
        MenuItem::action("New File", NewFile),
        MenuItem::action("New Window", NewWindow),
        MenuItem::separator(),
        MenuItem::action("Open File…", OpenFile),
        MenuItem::action("Open Folder…", OpenFolder),
        MenuItem::action("Open Folder in This Window…", OpenFolderInThisWindow),
        MenuItem::action("Recent Projects…", RecentProjects),
        MenuItem::action("Clone Repository…", GitClone),
        MenuItem::separator(),
        MenuItem::action("Save", Save),
        MenuItem::action("Save As…", SaveAs),
        MenuItem::separator(),
    ];
    if !mac {
        file.extend([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::action("Extensions…", OpenExtensions),
            MenuItem::action("Project Trust…", ProjectTrust),
            MenuItem::separator(),
        ]);
    }
    file.extend([MenuItem::action("Close Tab", CloseTab), MenuItem::action("Close Window", CloseWindow)]);
    if !mac {
        file.extend([MenuItem::separator(), MenuItem::action("Exit", Quit)]);
    }
    let mut menus = Vec::new();
    if mac {
        menus.push(Menu {
            name: "Kuana".into(),
            items: vec![
                MenuItem::action("About Kuana", about::About),
                MenuItem::action("Check for Updates…", CheckForUpdates),
                MenuItem::separator(),
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::action("Extensions…", OpenExtensions),
                MenuItem::action("Project Trust…", ProjectTrust),
                MenuItem::action("Welcome Setup…", WelcomeSetup),
                MenuItem::action("Install 'kuana' Shell Command", InstallShellCommand),
                MenuItem::separator(),
                MenuItem::action("Quit Kuana", Quit),
            ],
        });
    }
    menus.extend([
        Menu { name: "File".into(), items: file },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::action("Undo", Undo),
                MenuItem::action("Redo", Redo),
                MenuItem::separator(),
                MenuItem::action("Cut", Cut),
                MenuItem::action("Copy", Copy),
                MenuItem::action("Paste", Paste),
                MenuItem::separator(),
                MenuItem::action("Select All", SelectAll),
                MenuItem::action("Select Next Occurrence", SelectNextOccurrence),
                MenuItem::action("Toggle Comment", ToggleComment),
                MenuItem::action("Duplicate Line", DuplicateLine),
                MenuItem::action("Rename…", RenameSymbol),
                MenuItem::action("Reformat Code", ReformatCode),
                MenuItem::separator(),
                MenuItem::action("Find…", Find),
                MenuItem::action("Replace…", Replace),
                MenuItem::action("Find in Files…", FindInFiles),
                MenuItem::action("Replace in Files…", ReplaceInFiles),
            ],
        },
        Menu {
            name: "Navigate".into(),
            items: vec![
                MenuItem::action("Search Everywhere", SearchEverywhere),
                MenuItem::action("Go to File…", GoToFile),
                MenuItem::action("Recent Files…", RecentFiles),
                MenuItem::action("Go to Line…", GoToLine),
                MenuItem::action("File Structure…", FileStructure),
                MenuItem::action("Go to Symbol…", GoToSymbol),
                MenuItem::separator(),
                MenuItem::action("Go to Declaration", GoToDeclaration),
                MenuItem::action("Go to Implementation", GoToImplementation),
                MenuItem::action("Go to Type Definition", GoToTypeDefinition),
                MenuItem::action("Find Usages", FindUsages),
                MenuItem::separator(),
                MenuItem::action("Next Problem", NextProblem),
                MenuItem::action("Previous Problem", PreviousProblem),
                MenuItem::separator(),
                MenuItem::action("Find Action…", FindAction),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Toggle Sidebar", ToggleSidebar),
                MenuItem::action("Toggle Problems", ToggleProblems),
                MenuItem::action("Find Panel", ToggleResults),
                MenuItem::action("Terminal", ToggleTerminal),
                MenuItem::action("Soft-Wrap", ToggleSoftWrap),
                MenuItem::separator(),
                MenuItem::action("Select Theme…", SelectTheme),
                MenuItem::action("Increase Font Size", IncreaseFontSize),
                MenuItem::action("Decrease Font Size", DecreaseFontSize),
                MenuItem::action("Reset Font Size", ResetFontSize),
                MenuItem::separator(),
                MenuItem::action("Split Right", SplitRight),
                MenuItem::action("Split Down", SplitDown),
                MenuItem::separator(),
                MenuItem::action("Next Tab", NextTab),
                MenuItem::action("Previous Tab", PrevTab),
            ],
        },
        Menu {
            name: "Run".into(),
            items: vec![
                MenuItem::action("Run", Rerun),
                MenuItem::action("Run…", RunConfiguration),
                MenuItem::action("Run at Caret", RunAtCaret),
                MenuItem::action("Stop", StopRun),
                MenuItem::separator(),
                MenuItem::action("Debug at Caret", DebugAtCaret),
                MenuItem::action("Debug…", DebugConfiguration),
                MenuItem::action("Attach to Process…", AttachToProcess),
                MenuItem::action("Toggle Line Breakpoint", ToggleBreakpoint),
                MenuItem::action("Add Conditional Breakpoint…", AddConditionalBreakpoint),
                MenuItem::action("Add Logpoint…", AddLogpoint),
                MenuItem::action("Resume Program", Resume),
                MenuItem::action("Pause Program", PauseProgram),
                MenuItem::action("Step Over", StepOver),
                MenuItem::action("Step Into", StepInto),
                MenuItem::action("Step Out", StepOut),
            ],
        },
        Menu {
            name: "Git".into(),
            items: vec![
                MenuItem::action("Commit…", ShowCommit),
                MenuItem::action("Update Project…", GitPull),
                MenuItem::action("Push…", GitPush),
                MenuItem::action("Fetch", GitFetch),
                MenuItem::separator(),
                MenuItem::action("Branches…", GitBranches),
                MenuItem::action("Stash Changes…", GitStash),
                MenuItem::action("Manage Remotes…", ManageRemotes),
                MenuItem::action("Log", ToggleGitLog),
                MenuItem::action("Show History for File", ShowFileHistory),
                MenuItem::action("Open Repository in Browser", OpenRemote),
                MenuItem::separator(),
                MenuItem::action("Clone Repository…", GitClone),
                MenuItem::action("Initialize Repository", GitInit),
            ],
        },
    ]);
    menus.push(if mac {
        // GPUI makes the menu named "Window" the app's windows menu: macOS then
        // lists the open windows in it and in the Dock icon's menu.
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Minimize", MinimizeWindow),
                MenuItem::action("Zoom", ZoomWindow),
            ],
        }
    } else {
        Menu {
            name: "Help".into(),
            items: vec![
                MenuItem::action("Welcome Setup…", WelcomeSetup),
                MenuItem::separator(),
                MenuItem::action("Check for Updates…", CheckForUpdates),
                MenuItem::action("About Kuana", about::About),
            ],
        }
    });
    menus
}

/// Run as a bare binary (`cargo run`), macOS names the app after the executable, `kuana`.
/// This fixes the menu bar; the Dock label still says `kuana` until Kuana runs as the bundled
/// `Kuana.app`, which gets its name from Info.plist. Must run before the application starts.
#[cfg(target_os = "macos")]
fn set_app_name() {
    use objc2::{rc::Retained, runtime::AnyObject};
    use objc2_foundation::{NSBundle, NSMutableDictionary, NSString, ns_string};

    let bundle = NSBundle::mainBundle();
    if bundle.bundlePath().to_string().ends_with(".app") {
        return;
    }
    let Some(info) = bundle.infoDictionary() else { return };
    // Without a bundle, Foundation builds this dictionary in memory and it is mutable.
    let info: Retained<NSMutableDictionary<NSString, AnyObject>> = unsafe { Retained::cast_unchecked(info) };
    info.insert(ns_string!("CFBundleName"), &*NSString::from_str("Kuana"));
}

/// Sets the Dock icon so it also shows when running the bare binary
/// (`cargo run`); a bundled `.app` gets it from `Kuana.icns` as well.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    static ICON: &[u8] = include_bytes!("../assets/app-icon/macos/icon_512.png");

    let Some(mtm) = MainThreadMarker::new() else { return };
    let data = NSData::with_bytes(ICON);
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    }
}

/// AppKit zooms a window on a double click in its title bar strip before the
/// view under the pointer sees the click: on the title bar's buttons too, and
/// along with Kuana's own drag areas, so the two could cancel out. Double
/// clicks in that strip go to GPUI's view directly (`window_drag_area` zooms);
/// single ones still go through AppKit (the traffic lights are its buttons).
#[cfg(target_os = "macos")]
fn limit_window_zoom() {
    use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
    use objc2::{Encode, Encoding, msg_send, sel};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Point {
        x: f64,
        y: f64,
    }
    unsafe impl Encode for Point {
        const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect {
        origin: Point,
        size: Point,
    }
    unsafe impl Encode for Rect {
        const ENCODING: Encoding =
            Encoding::Struct("CGRect", &[Point::ENCODING, Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING])]);
    }
    const LEFT_MOUSE_DOWN: usize = 1;

    /// GPUI's view, among the content view's subviews (a blur view may be one too).
    unsafe fn gpui_view(window: &AnyObject) -> Option<&AnyObject> {
        let class = AnyClass::get(c"GPUIView")?;
        unsafe {
            let content: &AnyObject = msg_send![window, contentView];
            let subviews: &AnyObject = msg_send![content, subviews];
            let count: usize = msg_send![subviews, count];
            (0..count).map(|i| -> &AnyObject { msg_send![subviews, objectAtIndex: i] }).find(|view| {
                let is: Bool = msg_send![*view, isKindOfClass: class];
                is.as_bool()
            })
        }
    }

    extern "C-unwind" fn send_event(this: &AnyObject, _: Sel, event: &AnyObject) {
        unsafe {
            let kind: usize = msg_send![event, type];
            // `clickCount` only exists on mouse events.
            if kind == LEFT_MOUSE_DOWN && { let clicks: isize = msg_send![event, clickCount]; clicks } >= 2_isize {
                let at: Point = msg_send![event, locationInWindow];
                let content: &AnyObject = msg_send![this, contentView];
                let frame: Rect = msg_send![content, frame];
                // Window coordinates start at the bottom.
                if frame.size.y - at.y < workspace::TITLEBAR_HEIGHT as f64
                    && let Some(view) = gpui_view(this)
                {
                    let _: () = msg_send![view, mouseDown: event];
                    return;
                }
            }
            // The class GPUI's is made from (not `this`'s superclass: observing
            // a window swaps its class for a subclass).
            let panel = AnyClass::get(c"GPUIPanel").is_some_and(|panel| {
                let is: Bool = msg_send![this, isKindOfClass: panel];
                is.as_bool()
            });
            let Some(base) = AnyClass::get(if panel { c"NSPanel" } else { c"NSWindow" }) else { return };
            let _: () = msg_send![super(this, base), sendEvent: event];
        }
    }
    // GPUI registers its window classes before `main` runs.
    let imp = unsafe { std::mem::transmute::<extern "C-unwind" fn(&AnyObject, Sel, &AnyObject), Imp>(send_event) };
    for name in [c"GPUIWindow", c"GPUIPanel"] {
        if let Some(class) = AnyClass::get(name) {
            unsafe { objc2::ffi::class_addMethod(std::ptr::from_ref(class).cast_mut(), sel!(sendEvent:), imp, c"v@:@".as_ptr()) };
        }
    }
}

fn main() {
    theme::move_legacy_config();
    if let Some(config) = theme::config_dir() {
        panic_log::install(config.join("logs"));
        local_history::init(&config);
    }
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().is_some_and(|a| a == "extension") {
        std::process::exit(cli::extension_command(&raw[1..]));
    }
    let args: Vec<PathBuf> = raw.into_iter().map(PathBuf::from).collect();
    let autopilot = autopilot::from_env();
    // A Kuana already running takes the paths; this launch is done.
    let Ok(instance) = instance::claim(&args) else { return };
    // Git, language servers and debug adapters run with the login shell's
    // PATH (`search_path`); ask the shell while the window opens, not when
    // the first Git status waits for it.
    std::thread::spawn(|| {
        lsp::servers::search_path();
    });

    #[cfg(target_os = "macos")]
    set_app_name();
    #[cfg(target_os = "macos")]
    limit_window_zoom();

    // Finder (Open With, double-click) and the `kuana` command send paths as
    // open-file events; they're handled once the app is running.
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<Vec<String>>();
    let app = Application::new().with_assets(assets::Assets);
    // Later launches' paths come the same way (none: see below).
    if let Some(instance) = instance {
        let open_tx = open_tx.clone();
        instance.serve(move |paths| {
            let urls = paths.iter().filter_map(|p| url::Url::from_file_path(p).ok()).map(String::from).collect();
            let _ = open_tx.unbounded_send(urls);
        });
    }
    app.on_open_urls(move |urls| {
        let _ = open_tx.unbounded_send(urls);
    });
    app.run(move |cx: &mut App| {
        cx.spawn(async move |cx| {
            use futures::StreamExt as _;
            while let Some(urls) = open_rx.next().await {
                let paths = cli::paths_from_urls(&urls);
                let opened = match (urls.is_empty(), paths.is_empty()) {
                    // A launch without paths on macOS: the running Kuana comes
                    // to the front, as a click on its Dock icon does.
                    (true, _) if cfg!(target_os = "macos") => cx.update(|cx| cx.activate(true)),
                    // Elsewhere a new window, as VS Code opens: what the Start
                    // menu and the taskbar's Shift+click / middle click mean.
                    (true, _) => cx.update(|cx| {
                        let front = cx.active_window().or_else(|| cx.windows().into_iter().next());
                        let near = front.and_then(|w| w.update(cx, |_, window, _| window.bounds()).ok());
                        workspace::open_window(near, cx, |_, _, _| {});
                        cx.activate(true);
                    }),
                    (false, true) => Ok(()),
                    (false, false) => cx.update(|cx| open_external(paths, cx)),
                };
                if opened.is_err() {
                    break;
                }
            }
        })
        .detach();
        #[cfg(target_os = "macos")]
        set_dock_icon();
        assets::load_fonts(cx);
        // Settings first: the theme and key bindings come from them.
        let problems = settings::init(cx);
        cx.set_global(settings::StartupProblems(problems));
        // Before the theme, which may come from an extension, and before any
        // file opens, whose language may too.
        extensions::reload(cx);
        theme::init(cx);
        updater::init(cx);
        state::init(cx);
        // Quitting doesn't leave a file being saved half written.
        cx.on_app_quit(|_| {
            document::wait_for_saves();
            async {}
        })
        .detach();
        cx.on_action(|_: &IncreaseFontSize, cx| theme::zoom(Some(1.), cx));
        cx.on_action(|_: &DecreaseFontSize, cx| theme::zoom(Some(-1.), cx));
        cx.on_action(|_: &ResetFontSize, cx| theme::zoom(None, cx));
        // Quitting asks each window with unsaved edits in turn.
        cx.on_action(|_: &Quit, cx| cx.defer(workspace::quit));
        cx.on_action(|_: &about::About, cx| about::open(cx));
        // Also from the Preferences window, which has no workspace to handle them.
        cx.on_action(|_: &OpenSettings, cx| preferences::open(None, cx));
        cx.on_action(|_: &OpenExtensions, cx| preferences::open(Some(preferences::Page::Extensions), cx));
        cx.on_action(|_: &NewWindow, cx| {
            workspace::open_window(None, cx, |_, _, _| {});
        });
        cx.set_menus(menus());
        // Menu items show their shortcuts; another keymap changes them.
        cx.observe_global::<keymap::Installed>(|cx| cx.set_menus(menus())).detach();
        // The Dock menu and jump list list recent projects, ones opened by
        // Kuanas from before they did too.
        workspace::update_recent_projects(|_| {}, cx);

        // Each folder on the command line gets its own window;
        // files open in the first one. Without paths, reopen the last project.
        let (dirs, files): (Vec<PathBuf>, Vec<PathBuf>) = args
            .into_iter()
            .map(|p| paths::canonicalize(&p).unwrap_or(p))
            .partition(|p| p.is_dir());
        let mut dirs = dirs.into_iter();
        let first_dir = dirs.next();
        let restore = first_dir.is_none() && files.is_empty();
        let onboarding = state::SessionState::needs_onboarding();
        let mut last = workspace::open_window(None, cx, move |workspace, window, cx| {
            if restore {
                workspace.restore_last_project(window, cx);
            }
            if let Some(dir) = first_dir {
                workspace.set_root(dir, window, cx);
            }
            for path in files {
                workspace.open_path(path, window, cx);
            }
            if onboarding {
                workspace.show_onboarding(window, cx);
            }
        });
        let first = last;
        for dir in dirs {
            let near = last.and_then(|w| w.update(cx, |_, window, _| window.bounds()).ok());
            last = workspace::open_window(near, cx, move |workspace, window, cx| workspace.set_root(dir, window, cx)).or(last);
        }
        cx.activate(true);
        match autopilot {
            Some(steps) => autopilot::run(steps, cx),
            // A scripted run has no one to ask.
            None => {
                if let (Some(window), Some(config)) = (first, theme::config_dir()) {
                    window.update(cx, |_, window, cx| panic_log::offer_report(&config.join("logs"), window, cx)).ok();
                }
            }
        }
        // The About window alone doesn't keep Kuana running.
        cx.on_window_closed(|cx| {
            if cx.windows().iter().all(|w| w.downcast::<workspace::Workspace>().is_none()) {
                cx.quit();
            }
        })
        .detach();
    });
}

/// Opens paths from another app in the frontmost Kuana window (a new one
/// when there's none).
fn open_external(paths: Vec<std::path::PathBuf>, cx: &mut App) {
    cx.activate(true);
    let window = cx
        .active_window()
        .and_then(|w| w.downcast::<workspace::Workspace>())
        .or_else(|| cx.windows().into_iter().find_map(|w| w.downcast::<workspace::Workspace>()));
    match window {
        Some(window) => {
            window.update(cx, |ws, window, cx| ws.open_external(paths, window, cx)).ok();
        }
        None => {
            workspace::open_window(None, cx, move |ws, window, cx| ws.open_external(paths, window, cx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Action;

    fn action_names(menus: &[Menu]) -> Vec<&'static str> {
        menus
            .iter()
            .flat_map(|m| &m.items)
            .filter_map(|item| match item {
                MenuItem::Action { action, .. } => Some(action.name()),
                _ => None,
            })
            .collect()
    }

    /// Without the app menu, its items still have a place (Window's and the
    /// shell command are macOS's own).
    #[test]
    fn menus_elsewhere_keep_the_app_menus_items() {
        let elsewhere = action_names(&menus_for(false));
        let mac_only = [MinimizeWindow.name(), ZoomWindow.name(), InstallShellCommand.name()];
        for name in action_names(&menus_for(true)) {
            assert!(elsewhere.contains(&name) || mac_only.contains(&name), "{name} is only in the macOS menus");
        }
    }
}
