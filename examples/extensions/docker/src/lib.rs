//! A sample Kuana extension: the Docker containers on this machine as a
//! view, with Start, Stop, Show Logs, Follow Logs and Inspect on each, and
//! Docker Compose Up from a compose file's right-click menu. It runs the
//! `docker` command through Kuana (`run-process`, the `process` permission)
//! and answers with data; Kuana draws the view, menus, panel and prompts.
//! Follow Logs leaves `docker logs --follow` running (`spawn-process`, of
//! interface 0.2): Kuana adds its lines to the panel as they come.
//!
//! Build: `cargo build --release --target wasm32-wasip2`, then copy
//! `target/wasm32-wasip2/release/kuana_docker.wasm` to `kuana.wasm` beside
//! `extension.toml`.

wit_bindgen::generate!({
    path: "../../../src/extensions/kuana-wit/v0.2",
    world: "extension",
});

use kuana::extension::{
    host,
    types::{PanelContent, Table, Tone},
};
use serde_json::Value;

struct Docker;

const VIEW: &str = "containers";
/// The extension's name for the one program it leaves running.
const FOLLOWING: &str = "follow";

/// `docker <args>`'s output, or its error output as the error.
fn docker(args: &[&str]) -> Result<String, String> {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let output = host::run_process("docker", &args, None)?;
    if output.status != 0 {
        let detail = output.stderr.trim();
        return Err(if detail.is_empty() { format!("docker exited with {}", output.status) } else { detail.to_string() });
    }
    Ok(output.stdout)
}

/// The containers, running first, as `docker ps --all` lists them.
fn containers() -> Result<Vec<Value>, String> {
    let text = docker(&["ps", "--all", "--no-trunc", "--format", "{{json .}}"])?;
    let mut list: Vec<Value> = text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
    list.sort_by_key(|c| (field(c, "State") != "running", field(c, "Names")));
    Ok(list)
}

fn field(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

impl Guest for Docker {
    fn activate() -> Result<(), String> {
        Ok(())
    }

    fn children(view: String, parent: Option<String>) -> Result<Vec<TreeItem>, String> {
        if view != VIEW {
            return Ok(Vec::new());
        }
        let list = containers()?;
        match parent {
            // A container's details: its image, ports and status.
            Some(id) => {
                let Some(c) = list.iter().find(|c| field(c, "ID") == id) else { return Ok(Vec::new()) };
                let detail = |key: &str, label: &str, icon: &str| TreeItem {
                    id: format!("{id}/{key}"),
                    label: label.to_string(),
                    detail: Some(field(c, key)).filter(|v| !v.is_empty()).or(Some("none".into())),
                    icon: Some(icon.to_string()),
                    tone: Tone::Muted,
                    kind: Some("detail".into()),
                    expandable: false,
                    command: None,
                };
                Ok(vec![detail("Image", "Image", "package"), detail("Ports", "Ports", "link"), detail("Status", "Status", "history")])
            }
            None => {
                if list.is_empty() {
                    return Ok(vec![TreeItem {
                        id: "empty".into(),
                        label: "No containers".into(),
                        detail: None,
                        icon: None,
                        tone: Tone::Muted,
                        kind: None,
                        expandable: false,
                        command: None,
                    }]);
                }
                Ok(list
                    .iter()
                    .map(|c| {
                        let running = field(c, "State") == "running";
                        TreeItem {
                            id: field(c, "ID"),
                            label: field(c, "Names"),
                            detail: Some(field(c, "Image")),
                            icon: Some("icons/container.svg".into()),
                            tone: if running { Tone::Success } else { Tone::Muted },
                            kind: Some(if running { "running" } else { "stopped" }.into()),
                            expandable: true,
                            command: Some("logs".into()),
                        }
                    })
                    .collect())
            }
        }
    }

    fn run_command(command: String, target: Option<String>) -> Result<(), String> {
        let name = |id: &str| containers().ok().and_then(|l| l.into_iter().find(|c| field(c, "ID") == id)).map(|c| field(&c, "Names"));
        match (command.as_str(), target) {
            ("refresh", _) => host::refresh(VIEW, None),
            ("logs", Some(id)) => {
                let text = docker(&["logs", "--tail", "500", &id]).unwrap_or_else(|e| e);
                let lines: Vec<String> = text.lines().map(str::to_string).collect();
                let title = name(&id).map(|n| format!("Logs: {n}"));
                host::show_panel("logs", title.as_deref(), &PanelContent::Text(if lines.is_empty() { vec!["(no output)".into()] } else { lines }));
            }
            // One container at a time: following another ends the first.
            ("follow", Some(id)) => {
                host::kill_process(FOLLOWING);
                let title = name(&id).map(|n| format!("Following: {n}"));
                host::show_panel("logs", title.as_deref(), &PanelContent::Text(Vec::new()));
                let args = ["logs", "--follow", "--tail", "100", &id].map(str::to_string);
                // It comes back at once; the lines go to the panel as the container prints them.
                // The one just ended is gone (and says nothing more), so the name is free.
                if let Err(err) = host::spawn_process(FOLLOWING, "docker", &args, None, Some("logs")) {
                    host::append_panel("logs", &[format!("Couldn't follow the logs: {err}")]);
                }
            }
            // A program the code ends says nothing more: the panel's note is ours.
            ("stop-following", _) => {
                host::kill_process(FOLLOWING);
                host::append_panel("logs", &["(stopped following)".to_string()]);
            }
            ("inspect", Some(id)) => {
                let json = docker(&["inspect", &id])?;
                let file = format!("inspect-{}.json", name(&id).unwrap_or_else(|| id.chars().take(12).collect()));
                std::fs::write(&file, json).map_err(|e| e.to_string())?;
                host::open_file(&file, None)?;
            }
            ("start", Some(id)) => {
                docker(&["start", &id])?;
                host::notify(Tone::Success, &format!("Started {}", name(&id).unwrap_or(id)), None);
                host::refresh(VIEW, None);
            }
            ("stop", Some(id)) => {
                let shown = name(&id).unwrap_or_else(|| id.clone());
                if host::confirm(&format!("Stop {shown}?"), "Its processes get SIGTERM, then SIGKILL after 10 seconds.", "Stop") {
                    docker(&["stop", &id])?;
                    host::notify(Tone::Success, &format!("Stopped {shown}"), None);
                    host::refresh(VIEW, None);
                }
            }
            ("compose-up", Some(file)) => {
                host::run_in_terminal("Compose Up", &format!("docker compose -f '{}' up -d", file.replace('\'', "'\\''")))?;
            }
            // A summary as a table, from the view's toolbar or Find Action.
            ("table", _) => {
                let rows = containers()?
                    .iter()
                    .map(|c| ["Names", "Image", "State", "Status", "Ports"].iter().map(|k| Some(field(c, k)).filter(|v| !v.is_empty())).collect())
                    .collect();
                let columns = ["Name", "Image", "State", "Status", "Ports"].iter().map(|c| c.to_string()).collect();
                host::show_panel("logs", Some("All containers"), &PanelContent::Table(Table { columns, rows }));
            }
            (other, _) => return Err(format!("unknown command {other}")),
        }
        Ok(())
    }

    fn on_event(event: Event) -> Result<(), String> {
        // `docker logs --follow` ended by itself: the container stopped.
        if event.name == "process-exited" {
            host::append_panel("logs", &["(no longer following)".to_string()]);
        }
        Ok(())
    }
}

export!(Docker);
