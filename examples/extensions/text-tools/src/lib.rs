//! A sample Kuana extension for interface 0.2: it changes the open file and
//! follows what the user does, where the Docker sample shows a view.
//!
//! - Transform Selection… asks which way (`pick`) and rewrites each selection
//!   as one undo step (`editor`, `apply-edits`);
//! - a status bar item counts the words of the file the user is in
//!   (`set-status`), kept current by events (`editor-changed`, `file-saved`,
//!   `settings-changed`);
//! - Show Text Statistics fills a panel with a table.
//!
//! Build: `cargo build --release --target wasm32-wasip2`, then copy
//! `target/wasm32-wasip2/release/kuana_text_tools.wasm` to `kuana.wasm`
//! beside `extension.toml`.

wit_bindgen::generate!({
    path: "../../../src/extensions/kuana-wit/v0.2",
    world: "extension",
});

use kuana::extension::{
    host,
    types::{PanelContent, PickItem, Span, Status, Table, TextEdit, Tone},
};

struct TextTools;

/// The ways Transform Selection… offers: the pick list's ids and labels.
const TRANSFORMS: [(&str, &str, &str); 4] = [
    ("upper", "UPPERCASE", "HELLO WORLD"),
    ("lower", "lowercase", "hello world"),
    ("title", "Title Case", "Hello World"),
    ("snake", "snake_case", "hello_world"),
];

fn transform(text: &str, how: &str) -> String {
    match how {
        "upper" => text.to_uppercase(),
        "lower" => text.to_lowercase(),
        "title" => {
            let mut out = String::with_capacity(text.len());
            let mut start = true;
            for c in text.chars() {
                if start { out.extend(c.to_uppercase()) } else { out.extend(c.to_lowercase()) }
                start = !c.is_alphanumeric();
            }
            out
        }
        _ => {
            let words: Vec<String> = text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
            words.join("_")
        }
    }
}

fn transform_selection() -> Result<(), String> {
    // The tab the user is in, with its text. Offsets are bytes into it.
    // An error: there is a file, but this extension may not read it. None: no file.
    let editor = host::editor(None, true)?.ok_or("No file is open")?;
    let text = editor.text.as_deref().unwrap_or_default();
    let selected: Vec<Span> = editor.selections.iter().copied().filter(|s| s.start < s.end).collect();
    if selected.is_empty() {
        host::notify(Tone::Normal, "Select some text first", None);
        return Ok(());
    }
    let items: Vec<PickItem> =
        TRANSFORMS.iter().map(|(id, label, example)| PickItem { id: id.to_string(), label: label.to_string(), detail: Some(example.to_string()), icon: None }).collect();
    let Some(how) = host::pick("Transform the selection to…", &items) else { return Ok(()) };
    let edits: Vec<TextEdit> =
        selected.iter().map(|s| TextEdit { span: *s, text: transform(&text[s.start as usize..s.end as usize], &how) }).collect();
    // The user chose while the file stayed open: if they typed meanwhile, the
    // version no longer matches and nothing is changed.
    match host::apply_edits(&editor.path, editor.version, &edits) {
        Ok(_) => Ok(()),
        Err(err) if err.contains("changed") => {
            host::notify(Tone::Warning, "The text changed meanwhile", Some("Run Transform Selection again."));
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Words, characters and lines of `text`.
fn counts(text: &str) -> (usize, usize, usize) {
    (text.split_whitespace().count(), text.chars().count(), text.lines().count())
}

/// The `count` setting: Kuana fills in the manifest's default.
fn counted() -> String {
    let settings: serde_json::Value = serde_json::from_str(&host::settings()).unwrap_or_default();
    settings["count"].as_str().unwrap_or("words").to_string()
}

/// Puts the count of the file the user is in on the status bar.
fn show_count() {
    // No file, or one this extension can't read: nothing to count.
    let Ok(Some(editor)) = host::editor(None, true) else {
        host::set_status("count", None);
        return;
    };
    let (words, characters, lines) = counts(editor.text.as_deref().unwrap_or_default());
    let text = match counted().as_str() {
        "characters" => format!("{characters} characters"),
        "lines" => format!("{lines} lines"),
        _ => format!("{words} words"),
    };
    let tooltip = format!("{words} words, {characters} characters, {lines} lines. Click for more.");
    host::set_status("count", Some(&Status { text, icon: Some("type".into()), tone: Tone::Muted, tooltip: Some(tooltip) }));
}

fn show_stats(path: Option<String>) -> Result<(), String> {
    let editor = host::editor(path.as_deref(), true)?.ok_or("No file is open")?;
    let text = editor.text.as_deref().unwrap_or_default();
    let (words, characters, lines) = counts(text);
    let longest = text.lines().map(|l| l.chars().count()).max().unwrap_or(0);
    let row = |name: &str, value: usize| vec![Some(name.to_string()), Some(value.to_string())];
    let table = Table {
        columns: vec!["Measure".into(), "Value".into()],
        rows: vec![row("Words", words), row("Characters", characters), row("Lines", lines), row("Longest line", longest), row("Selections", editor.selections.len())],
    };
    let name = editor.path.rsplit(['/', '\\']).next().unwrap_or(&editor.path).to_string();
    host::show_panel("stats", Some(&name), &PanelContent::Table(table));
    Ok(())
}

impl Guest for TextTools {
    fn activate() -> Result<(), String> {
        Ok(())
    }

    // No views: the Docker sample shows those.
    fn children(_view: String, _parent: Option<String>) -> Result<Vec<TreeItem>, String> {
        Ok(Vec::new())
    }

    fn run_command(command: String, target: Option<String>) -> Result<(), String> {
        match command.as_str() {
            "transform" => transform_selection(),
            // From a tab's menu, `target` is that tab's file.
            "stats" => show_stats(target),
            other => Err(format!("unknown command {other}")),
        }
    }

    fn on_event(event: Event) -> Result<(), String> {
        if matches!(event.name.as_str(), "editor-changed" | "file-saved" | "settings-changed") {
            show_count();
        }
        Ok(())
    }
}

export!(TextTools);
