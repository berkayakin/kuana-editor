//! Drives Kuana from a script of key presses and actions, for the README's
//! recording (`scripts/record.sh`) and CI's smoke test, which starts each
//! platform's build on a demo project and exits nonzero if the script can't
//! run through. `KUANA_AUTOPILOT=<file>` runs the file once the first window
//! is open. Events go straight to GPUI's dispatch, so nothing types into
//! other apps and the window needn't be frontmost. On macOS it needn't even
//! show: GPUI stops drawing a window other windows cover, so while one is
//! covered, each step but `wait` draws a frame first (`draw`), and what a
//! mark's screenshot by window id gets (`screencapture -l`) is current.
//!
//! One command per line (`#` starts a comment):
//!
//! - `wait <ms>`: pause
//! - `pace <ms>`: pause after each key from now on (default 80)
//! - `keys <keystroke>…`: press keys as the keymap writes them (`secondary-p`
//!   is ⌘P on macOS and Ctrl+P elsewhere, `enter`, `shift-tab`)
//! - `type <text>`: type the rest of the line, a character at a time
//! - `action <name> [json]`: dispatch an action by name (`kuana::ToggleTerminal`)
//! - `click <x> <y> [right] [double]`: a mouse click at a point of the window's
//!   content (points from its top left), the pointer moved there first
//! - `hover <x> <y>`: the pointer moved to a point, without a click (what shows
//!   under it, a tooltip or a hover button, can be screenshot)
//! - `drag <x> <y> <to x> <to y> [mark <name>]`: press, move in steps, release
//!   (mouse steps are macOS only: GPUI takes mouse events from the system
//!   alone, so they're handed to its view as AppKit events, still without
//!   leaving Kuana); with a mark, it's printed before the release and the
//!   button held a moment, for a screenshot of what a drag shows (a drop target)
//! - `answer <button>`: click the button so named on the question the window
//!   asks (a sheet on macOS: `window.prompt`'s Undo Changes, Cancel…)
//! - `size <width> <height>`: resize the window's content, in points
//! - `mark <name>`: print `kuana-autopilot: mark <name>` (the recorder starts
//!   and stops on marks)
//! - `follow <file>`: from here on, take steps from `file` as lines are added
//!   to it (checked every 200 ms), until one quits: to drive a running Kuana
//!   step by step, looking at it between steps. A step there that doesn't
//!   parse or fails is reported and skipped.
//! - `quit`: quit (hot exit: nothing asks)

use std::{path::Path, time::Duration};

use gpui::{App, AsyncApp, Keystroke, Modifiers, MouseButton, Window, px, size};

#[derive(Debug, PartialEq)]
enum Step {
    Wait(u64),
    Pace(u64),
    Keys(Vec<Keystroke>),
    Type(String),
    Action(String, Option<serde_json::Value>),
    Size(f32, f32),
    /// A click at a point: the button and how many presses.
    Click(f32, f32, MouseButton, usize),
    Hover(f32, f32),
    /// From, to, and a mark printed while the button's still down.
    Drag((f32, f32), (f32, f32), Option<String>),
    Mark(String),
    /// Click a button of the window's question (a sheet), by its title.
    Answer(String),
    Follow(std::path::PathBuf),
    Quit,
}

/// A parsed script: its steps with their line numbers.
pub struct Script(Vec<(usize, Step)>);

/// The script named by `KUANA_AUTOPILOT`, if any. A bad script ends Kuana
/// (exit code 2) before any window opens.
pub fn from_env() -> Option<Script> {
    let path = std::env::var_os("KUANA_AUTOPILOT")?;
    match read(Path::new(&path)) {
        Ok(steps) => Some(Script(steps)),
        Err(err) => {
            eprintln!("kuana-autopilot: {err}");
            std::process::exit(2);
        }
    }
}

fn read(path: &Path) -> Result<Vec<(usize, Step)>, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("can't read {}: {err}", path.display()))?;
    parse(&text).map_err(|err| format!("{}: {err}", path.display()))
}

fn parse(text: &str) -> Result<Vec<(usize, Step)>, String> {
    let mut steps = Vec::new();
    for (ix, line) in text.lines().enumerate() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (command, rest) = line.split_once(' ').unwrap_or((line, ""));
        let line_no = ix + 1;
        let number = |what: &str| rest.trim().parse::<u64>().map_err(|_| format!("line {line_no}: {what} takes a number of milliseconds"));
        let step = match command {
            "wait" => Step::Wait(number("wait")?),
            "pace" => Step::Pace(number("pace")?),
            "keys" => Step::Keys(
                rest.split_whitespace()
                    .map(|k| Keystroke::parse(k).map_err(|err| format!("line {line_no}: {err}")))
                    .collect::<Result<_, _>>()?,
            ),
            // Only the separating space goes: the text may start with spaces.
            "type" => Step::Type(rest.to_string()),
            "action" => {
                let (name, data) = rest.trim().split_once(' ').unwrap_or((rest.trim(), ""));
                let data = if data.trim().is_empty() {
                    None
                } else {
                    Some(serde_json::from_str(data).map_err(|err| format!("line {line_no}: the action's data isn't JSON: {err}"))?)
                };
                Step::Action(name.to_string(), data)
            }
            "size" => {
                let dims: Vec<f32> = rest.split_whitespace().filter_map(|n| n.parse().ok()).collect();
                let [w, h] = dims[..] else { return Err(format!("line {line_no}: size takes a width and a height")) };
                Step::Size(w, h)
            }
            "click" => {
                let mut words = rest.split_whitespace();
                let (Some(x), Some(y)) = (words.next().and_then(|n| n.parse().ok()), words.next().and_then(|n| n.parse().ok())) else {
                    return Err(format!("line {line_no}: click takes an x and a y"));
                };
                let (mut button, mut count) = (MouseButton::Left, 1);
                for word in words {
                    match word {
                        "right" => button = MouseButton::Right,
                        "double" => count = 2,
                        other => return Err(format!("line {line_no}: click doesn't know “{other}”")),
                    }
                }
                Step::Click(x, y, button, count)
            }
            "hover" => {
                let n: Vec<f32> = rest.split_whitespace().filter_map(|n| n.parse().ok()).collect();
                let [x, y] = n[..] else { return Err(format!("line {line_no}: hover takes an x and a y")) };
                Step::Hover(x, y)
            }
            "drag" => {
                let (points, mark) = match rest.split_once(" mark ") {
                    Some((points, mark)) => (points, Some(mark.trim().to_string())),
                    None => (rest, None),
                };
                let n: Vec<f32> = points.split_whitespace().filter_map(|n| n.parse().ok()).collect();
                let [x, y, to_x, to_y] = n[..] else { return Err(format!("line {line_no}: drag takes two points")) };
                Step::Drag((x, y), (to_x, to_y), mark)
            }
            "mark" => Step::Mark(rest.trim().to_string()),
            "answer" => Step::Answer(rest.trim().to_string()),
            "follow" => Step::Follow(rest.trim().into()),
            "quit" => Step::Quit,
            other => return Err(format!("line {line_no}: unknown command “{other}”")),
        };
        steps.push((line_no, step));
    }
    Ok(steps)
}

/// A key press that types `c`.
fn typed(c: char) -> Keystroke {
    let key = match c {
        ' ' => "space".to_string(),
        '\t' => "tab".to_string(),
        c => c.to_lowercase().to_string(),
    };
    Keystroke { modifiers: Modifiers { shift: c.is_uppercase(), ..Modifiers::none() }, key, key_char: Some(c.to_string()) }
}

/// Runs `steps` in the frontmost window (Preferences, when a step opened it).
/// A step that fails ends Kuana with exit code 1, so CI notices.
pub fn run(script: Script, cx: &mut App) {
    cx.spawn(async move |cx| {
        let mut pace = 80;
        for (line, step) in script.0 {
            if let Step::Follow(path) = &step {
                follow(path, &mut pace, cx).await;
                return;
            }
            if let Err(err) = perform(&step, &mut pace, cx).await {
                eprintln!("kuana-autopilot: line {line}: {err}");
                std::process::exit(1);
            }
        }
    })
    .detach();
}

/// Runs the lines added to `path` as they come (see `follow` in the module
/// docs); what's there already runs first.
async fn follow(path: &Path, pace: &mut u64, cx: &mut AsyncApp) {
    let mut done = 0;
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        // Only whole lines: one being written waits for its line break.
        let whole = text.rfind('\n').map_or("", |end| &text[..end]);
        let lines: Vec<&str> = whole.lines().collect();
        for (ix, line) in lines.iter().enumerate().skip(done) {
            done = ix + 1;
            match parse(line) {
                Ok(steps) => {
                    for (_, step) in steps {
                        if let Err(err) = perform(&step, pace, cx).await {
                            eprintln!("kuana-autopilot: {}:{}: {err}", path.display(), ix + 1);
                        }
                    }
                }
                Err(err) => eprintln!("kuana-autopilot: {}:{}: {err}", path.display(), ix + 1),
            }
        }
        cx.background_executor().timer(Duration::from_millis(200)).await;
    }
}

async fn perform(step: &Step, pace: &mut u64, cx: &mut AsyncApp) -> Result<(), String> {
    let executor = cx.background_executor().clone();
    let sleep = |ms: u64| executor.timer(Duration::from_millis(ms));
    if !matches!(step, Step::Wait(_) | Step::Pace(_) | Step::Follow(_)) {
        draw();
    }
    match step {
        Step::Wait(ms) => sleep(*ms).await,
        Step::Pace(ms) => *pace = *ms,
        Step::Keys(keys) => {
            for key in keys {
                // Where a key goes comes from the last frame (what has focus).
                draw();
                in_window(cx, |window, cx| {
                    window.dispatch_keystroke(key.clone(), cx);
                    Ok(())
                })?;
                sleep(*pace).await;
            }
        }
        Step::Type(text) => {
            for c in text.chars() {
                draw();
                in_window(cx, |window, cx| {
                    window.dispatch_keystroke(typed(c), cx);
                    Ok(())
                })?;
                sleep(*pace).await;
            }
        }
        Step::Action(name, data) => {
            in_window(cx, |window, cx| {
                let action = cx.build_action(name, data.clone()).map_err(|err| err.to_string())?;
                window.dispatch_action(action, cx);
                Ok(())
            })?;
            sleep(*pace).await;
        }
        Step::Size(w, h) => in_window(cx, |window, _| {
            window.resize(size(px(*w), px(*h)));
            Ok(())
        })?,
        Step::Click(x, y, button, count) => {
            let (down, up) = if *button == MouseButton::Right { (Mouse::RightDown, Mouse::RightUp) } else { (Mouse::Down, Mouse::Up) };
            mouse(cx, Mouse::Moved, (*x, *y), 0)?;
            sleep(*pace).await;
            for clicks in 1..=*count {
                mouse(cx, down, (*x, *y), clicks)?;
                mouse(cx, up, (*x, *y), clicks)?;
            }
            sleep(*pace).await;
        }
        Step::Hover(x, y) => {
            mouse(cx, Mouse::Moved, (*x, *y), 0)?;
            sleep(*pace).await;
        }
        Step::Drag(from, to, mark) => {
            const STEPS: usize = 8;
            let at = |step: usize| {
                let t = step as f32 / STEPS as f32;
                (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
            };
            mouse(cx, Mouse::Moved, at(0), 0)?;
            sleep(*pace).await;
            mouse(cx, Mouse::Down, at(0), 1)?;
            // A frame between moves: what a drag draws (a resize tracker, a
            // drop target's drag-over styles and hitbox) takes the next ones.
            // A covered window draws no frames by itself, so each is drawn.
            for step in 1..=STEPS {
                sleep(30).await;
                mouse(cx, Mouse::Dragged, at(step), 0)?;
                draw();
            }
            sleep(30).await;
            draw();
            if let Some(name) = mark {
                use std::io::Write as _;
                println!("kuana-autopilot: mark {name}");
                std::io::stdout().flush().ok();
                // The screenshot's taken from outside meanwhile.
                sleep(900).await;
                mouse(cx, Mouse::Dragged, at(STEPS), 0)?;
                draw();
            }
            mouse(cx, Mouse::Up, at(STEPS), 1)?;
            sleep(*pace).await;
        }
        Step::Answer(title) => {
            answer(title)?;
            sleep(*pace).await;
        }
        // Handled by `run` (it takes over the rest of the script).
        Step::Follow(_) => {}
        Step::Mark(name) => {
            use std::io::Write as _;
            println!("kuana-autopilot: mark {name}");
            std::io::stdout().flush().ok();
        }
        Step::Quit => {
            cx.update(|cx| cx.quit()).map_err(|err| err.to_string())?;
        }
    }
    Ok(())
}

/// A mouse event's kind, by AppKit's `NSEventType` number.
#[derive(Clone, Copy)]
enum Mouse {
    Down = 1,
    Up = 2,
    RightDown = 3,
    RightUp = 4,
    Moved = 5,
    Dragged = 6,
}

/// Hands the frontmost window's GPUI view a mouse event at a point of its
/// content (from the top left), as AppKit would.
#[cfg(target_os = "macos")]
fn mouse(_: &mut AsyncApp, kind: Mouse, at: (f32, f32), clicks: usize) -> Result<(), String> {
    use objc2::runtime::AnyObject;
    use objc2::{Encode, Encoding, RefEncode, class, msg_send};

    /// The Core Graphics event behind an `NSEvent`.
    #[repr(C)]
    struct CGEvent([u8; 0]);
    unsafe impl RefEncode for CGEvent {
        const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("__CGEvent", &[]));
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventSetIntegerValueField(event: *mut CGEvent, field: u32, value: i64);
    }
    const MOUSE_EVENT_BUTTON_NUMBER: u32 = 3;

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
    let (window, view) = frontmost_view()?;
    unsafe {
        let content: &AnyObject = msg_send![window, contentView];
        let frame: Rect = msg_send![content, frame];
        // Window coordinates start at the bottom.
        let location = Point { x: at.0 as f64, y: frame.size.y - at.1 as f64 };
        let number: isize = msg_send![window, windowNumber];
        let context: *const AnyObject = std::ptr::null();
        let event: *mut AnyObject = msg_send![
            class!(NSEvent),
            mouseEventWithType: kind as usize,
            location: location,
            modifierFlags: 0_usize,
            timestamp: 0_f64,
            windowNumber: number,
            context: context,
            eventNumber: 0_isize,
            clickCount: clicks as isize,
            pressure: 1_f32,
        ];
        let mut event = event.as_ref().ok_or("couldn't make the mouse event")?;
        // AppKit makes every such event the left button's, and GPUI reads the
        // button from its number: set on the Core Graphics event behind it,
        // which a new event is made from.
        if matches!(kind, Mouse::RightDown | Mouse::RightUp) {
            let cg: *mut CGEvent = msg_send![event, CGEvent];
            if cg.is_null() {
                return Err("the mouse event has no Core Graphics event".into());
            }
            CGEventSetIntegerValueField(cg, MOUSE_EVENT_BUTTON_NUMBER, 1);
            let right: *mut AnyObject = msg_send![class!(NSEvent), eventWithCGEvent: cg];
            event = right.as_ref().ok_or("couldn't make the right button's event")?;
            let (button, place): (isize, Point) = (msg_send![event, buttonNumber], msg_send![event, locationInWindow]);
            if button != 1 || (place.x - location.x).abs() > 0.5 || (place.y - location.y).abs() > 0.5 {
                return Err(format!("the right button's event came out as button {button} at {}, {}", place.x, place.y));
            }
        }
        match kind {
            Mouse::Down => msg_send![view, mouseDown: event],
            Mouse::Up => msg_send![view, mouseUp: event],
            Mouse::RightDown => msg_send![view, rightMouseDown: event],
            Mouse::RightUp => msg_send![view, rightMouseUp: event],
            Mouse::Moved => msg_send![view, mouseMoved: event],
            Mouse::Dragged => msg_send![view, mouseDragged: event],
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn mouse(_: &mut AsyncApp, _: Mouse, _: (f32, f32), _: usize) -> Result<(), String> {
    Err("mouse steps only work on macOS".into())
}

/// The frontmost GPUI window and its view (AppKit objects, alive while the
/// window is open).
#[cfg(target_os = "macos")]
fn frontmost_view() -> Result<(&'static objc2::runtime::AnyObject, &'static objc2::runtime::AnyObject), String> {
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::{class, msg_send};

    let is_a = |object: &AnyObject, class: &AnyClass| -> bool {
        let is: Bool = unsafe { msg_send![object, isKindOfClass: class] };
        is.as_bool()
    };
    let (Some(window_class), Some(view_class)) = (AnyClass::get(c"GPUIWindow"), AnyClass::get(c"GPUIView")) else {
        return Err("no GPUI window class".into());
    };
    unsafe {
        let app: &AnyObject = msg_send![class!(NSApplication), sharedApplication];
        // Front to back.
        let windows: &AnyObject = msg_send![app, orderedWindows];
        let count: usize = msg_send![windows, count];
        let window = (0..count).map(|i| -> &AnyObject { msg_send![windows, objectAtIndex: i] }).find(|w| is_a(w, window_class)).ok_or("no window is open")?;
        let content: &AnyObject = msg_send![window, contentView];
        let subviews: &AnyObject = msg_send![content, subviews];
        let count: usize = msg_send![subviews, count];
        let view = (0..count).map(|i| -> &AnyObject { msg_send![subviews, objectAtIndex: i] }).find(|v| is_a(v, view_class)).ok_or("the window has no GPUI view")?;
        Ok((window, view))
    }
}

/// Draws the frontmost window's frame now when other windows cover it all
/// (GPUI's display link is stopped then), as AppKit asks a view to on a
/// resize: what changed since shows in a screenshot, and clicks and keys go
/// by what's there now. Never inside `cx.update`: drawing takes the app.
#[cfg(target_os = "macos")]
fn draw() {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    /// `NSWindowOcclusionStateVisible`.
    const VISIBLE: usize = 1 << 1;
    let Ok((window, view)) = frontmost_view() else { return };
    unsafe {
        let state: usize = msg_send![window, occlusionState];
        if state & VISIBLE == 0 {
            let layer: *const AnyObject = msg_send![view, layer];
            let _: () = msg_send![view, displayLayer: layer];
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn draw() {}

/// Clicks the button titled `title` on the sheet the frontmost window shows
/// (GPUI asks `window.prompt`'s questions with an `NSAlert` sheet), as a
/// click would: the question answers that way.
#[cfg(target_os = "macos")]
fn answer(title: &str) -> Result<(), String> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};

    fn find<'a>(view: &'a AnyObject, button: &AnyClass, title: &str) -> Option<&'a AnyObject> {
        unsafe {
            let is: Bool = msg_send![view, isKindOfClass: button];
            if is.as_bool() {
                let name: *const AnyObject = msg_send![view, title];
                let utf8: *const std::ffi::c_char = match name.as_ref() {
                    Some(name) => msg_send![name, UTF8String],
                    None => std::ptr::null(),
                };
                if !utf8.is_null() && std::ffi::CStr::from_ptr(utf8).to_string_lossy() == title {
                    return Some(view);
                }
            }
            let subviews: &AnyObject = msg_send![view, subviews];
            let count: usize = msg_send![subviews, count];
            (0..count).find_map(|i| -> Option<&'a AnyObject> {
                let child: &AnyObject = msg_send![subviews, objectAtIndex: i];
                find(child, button, title)
            })
        }
    }
    let button = AnyClass::get(c"NSButton").ok_or("no NSButton class")?;
    let (window, _) = frontmost_view()?;
    unsafe {
        let sheet: *const AnyObject = msg_send![window, attachedSheet];
        let sheet = sheet.as_ref().ok_or("the window asks nothing")?;
        let content: &AnyObject = msg_send![sheet, contentView];
        let found = find(content, button, title).ok_or_else(|| format!("the question has no “{title}” button"))?;
        let sender: *const AnyObject = std::ptr::null();
        let _: () = msg_send![found, performClick: sender];
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn answer(_: &str) -> Result<(), String> {
    Err("answer only works on macOS".into())
}

fn in_window(cx: &mut AsyncApp, f: impl FnOnce(&mut Window, &mut App) -> Result<(), String>) -> Result<(), String> {
    cx.update(|cx| {
        let window = cx.active_window().or_else(|| cx.windows().into_iter().next()).ok_or("no window is open")?;
        window.update(cx, |_, window, cx| f(window, cx)).map_err(|err| err.to_string())?
    })
    .map_err(|err| err.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_parse_into_steps() {
        let steps = parse(
            "# a demo\n\nwait 500\npace 40\nkeys secondary-p enter\ntype   fn main\naction kuana::GoToLine {\"line\": 3}\n\
             action kuana::ToggleTerminal\nsize 1280 800\nmark start\nquit\n",
        )
        .unwrap();
        let steps: Vec<Step> = steps.into_iter().map(|(_, s)| s).collect();
        assert_eq!(steps[0], Step::Wait(500));
        assert_eq!(steps[1], Step::Pace(40));
        assert_eq!(steps[2], Step::Keys(vec![Keystroke::parse("secondary-p").unwrap(), Keystroke::parse("enter").unwrap()]));
        assert_eq!(steps[3], Step::Type("  fn main".into()));
        assert_eq!(steps[4], Step::Action("kuana::GoToLine".into(), Some(serde_json::json!({"line": 3}))));
        assert_eq!(steps[5], Step::Action("kuana::ToggleTerminal".into(), None));
        assert_eq!(steps[6], Step::Size(1280., 800.));
        assert_eq!(steps[7], Step::Mark("start".into()));
        assert_eq!(steps[8], Step::Quit);
        let steps = parse("click 10 20\nclick 5.5 6 right\nclick 1 2 double\ndrag 1 2 30 40\nhover 7 8\ndrag 1 2 30 40 mark over the pane\n").unwrap();
        let steps: Vec<Step> = steps.into_iter().map(|(_, s)| s).collect();
        assert_eq!(steps[0], Step::Click(10., 20., MouseButton::Left, 1));
        assert_eq!(steps[1], Step::Click(5.5, 6., MouseButton::Right, 1));
        assert_eq!(steps[2], Step::Click(1., 2., MouseButton::Left, 2));
        assert_eq!(steps[3], Step::Drag((1., 2.), (30., 40.), None));
        assert_eq!(steps[4], Step::Hover(7., 8.));
        assert_eq!(steps[5], Step::Drag((1., 2.), (30., 40.), Some("over the pane".into())));
    }

    #[test]
    fn mistakes_name_their_line() {
        let err = |text: &str| parse(text).unwrap_err();
        assert!(err("wait 1\njump 3").contains("line 2: unknown command “jump”"));
        assert!(err("wait soon").contains("line 1"));
        assert!(err("size 100").contains("line 1: size takes"));
        assert!(err("click 10").contains("line 1: click takes"));
        assert!(err("click 1 2 middle").contains("line 1: click doesn't know"));
        assert!(err("drag 1 2 3").contains("line 1: drag takes"));
        assert!(err("hover 1").contains("line 1: hover takes"));
        assert!(err("action a::B {nope").contains("line 1: the action's data isn't JSON"));
    }

    #[test]
    fn typed_characters_carry_their_text() {
        assert_eq!(typed('A'), Keystroke { modifiers: Modifiers { shift: true, ..Modifiers::none() }, key: "a".into(), key_char: Some("A".into()) });
        assert_eq!(typed(' ').key, "space");
        assert_eq!(typed('{').key_char.as_deref(), Some("{"));
    }
}
