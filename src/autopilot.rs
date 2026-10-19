//! Drives Kuana from a script of key presses and actions, for the README's
//! recording (`scripts/record.sh`) and CI's smoke test, which starts each
//! platform's build on a demo project and exits nonzero if the script can't
//! run through. `KUANA_AUTOPILOT=<file>` runs the file once the first window
//! is open. Events go straight to GPUI's dispatch, so nothing types into
//! other apps and the window needn't be frontmost.
//!
//! One command per line (`#` starts a comment):
//!
//! - `wait <ms>`: pause
//! - `pace <ms>`: pause after each key from now on (default 80)
//! - `keys <keystroke>…`: press keys as the keymap writes them (`secondary-p`
//!   is ⌘P on macOS and Ctrl+P elsewhere, `enter`, `shift-tab`)
//! - `type <text>`: type the rest of the line, a character at a time
//! - `action <name> [json]`: dispatch an action by name (`kuana::ToggleTerminal`)
//! - `size <width> <height>`: resize the window's content, in points
//! - `mark <name>`: print `kuana-autopilot: mark <name>` (the recorder starts
//!   and stops on marks)
//! - `quit`: quit (hot exit: nothing asks)

use std::{path::Path, time::Duration};

use gpui::{App, AsyncApp, Keystroke, Modifiers, Window, px, size};

#[derive(Debug, PartialEq)]
enum Step {
    Wait(u64),
    Pace(u64),
    Keys(Vec<Keystroke>),
    Type(String),
    Action(String, Option<serde_json::Value>),
    Size(f32, f32),
    Mark(String),
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
            "mark" => Step::Mark(rest.trim().to_string()),
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
            if let Err(err) = perform(&step, &mut pace, cx).await {
                eprintln!("kuana-autopilot: line {line}: {err}");
                std::process::exit(1);
            }
        }
    })
    .detach();
}

async fn perform(step: &Step, pace: &mut u64, cx: &mut AsyncApp) -> Result<(), String> {
    let executor = cx.background_executor().clone();
    let sleep = |ms: u64| executor.timer(Duration::from_millis(ms));
    match step {
        Step::Wait(ms) => sleep(*ms).await,
        Step::Pace(ms) => *pace = *ms,
        Step::Keys(keys) => {
            for key in keys {
                in_window(cx, |window, cx| {
                    window.dispatch_keystroke(key.clone(), cx);
                    Ok(())
                })?;
                sleep(*pace).await;
            }
        }
        Step::Type(text) => {
            for c in text.chars() {
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
    }

    #[test]
    fn mistakes_name_their_line() {
        let err = |text: &str| parse(text).unwrap_err();
        assert!(err("wait 1\njump 3").contains("line 2: unknown command “jump”"));
        assert!(err("wait soon").contains("line 1"));
        assert!(err("size 100").contains("line 1: size takes"));
        assert!(err("action a::B {nope").contains("line 1: the action's data isn't JSON"));
    }

    #[test]
    fn typed_characters_carry_their_text() {
        assert_eq!(typed('A'), Keystroke { modifiers: Modifiers { shift: true, ..Modifiers::none() }, key: "a".into(), key_char: Some("A".into()) });
        assert_eq!(typed(' ').key, "space");
        assert_eq!(typed('{').key_char.as_deref(), Some("{"));
    }
}
