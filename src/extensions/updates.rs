//! Keeps extensions installed from the registry up to date, as Zed does:
//! shortly after launch and every few hours, Kuana installs the newer
//! versions the registry lists, except for extensions pinned in
//! `auto_update_extensions` (`{"<id>": false}`). An update is the registry's
//! signed and checksummed package, as Install is (`registry::install`);
//! extensions installed another way (Git, a folder, linked from Zed) are
//! left alone (`registry::Installed::Elsewhere`).

use std::time::Duration;

use gpui::App;

use super::registry::{self, Installed, Listed};
use crate::settings::Settings;

/// After launch, once the extensions have loaded and the window has settled.
const FIRST_CHECK: Duration = Duration::from_secs(30);
/// Like Kuana's own updates (`updater`).
const CHECK_EVERY: Duration = Duration::from_secs(4 * 60 * 60);

/// Checks shortly after launch and every few hours.
pub fn init(cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FIRST_CHECK).await;
        loop {
            let Ok(pinned) = cx.update(|cx| pinned(Settings::get(cx))) else { break };
            let found = cx.background_executor().spawn(async move { due(&pinned) }).await;
            if !found.is_empty() {
                let updated = cx.background_executor().spawn(async move { install(&found) }).await;
                if !updated.is_empty() && cx.update(|cx| updated_now(&updated, cx)).is_err() {
                    break;
                }
            }
            cx.background_executor().timer(CHECK_EVERY).await;
        }
    })
    .detach();
}

/// The extensions `auto_update_extensions` keeps at their version.
fn pinned(settings: &Settings) -> Vec<String> {
    settings.auto_update_extensions.iter().filter(|(_, on)| !**on).map(|(id, _)| id.clone()).collect()
}

/// The registry's newer versions of what it installed here, minus the
/// pinned. Blocking; quiet when the registry can't be reached (the next
/// check tries again).
fn due(pinned: &[String]) -> Vec<Listed> {
    let installed = super::installed();
    if !installed.iter().any(|e| registry::installed_here(&e.path)) {
        return Vec::new();
    }
    let Ok(listed) = registry::available() else { return Vec::new() };
    outdated(&listed, &installed, pinned)
}

fn outdated(listed: &[Listed], installed: &[super::ExtensionInfo], pinned: &[String]) -> Vec<Listed> {
    listed.iter().filter(|l| !pinned.contains(&l.id) && registry::installed(l, installed) == Installed::Outdated).cloned().collect()
}

/// Installs each update; what worked, as "Name version". Blocking. One that
/// fails stays at its version until the next check.
fn install(due: &[Listed]) -> Vec<String> {
    due.iter()
        .filter_map(|listed| match registry::install(listed) {
            Ok(name) => Some(format!("{name} {}", listed.version)),
            Err(err) => {
                eprintln!("kuana: couldn't update the extension {}: {err}", listed.id);
                None
            }
        })
        .collect()
}

/// Loads the new versions and says which they are.
fn updated_now(updated: &[String], cx: &mut App) {
    super::reload(cx);
    let list = match updated {
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        [] => return,
    };
    super::announce(format!("Updated {list}"), cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_are_left_out() {
        let settings: Settings = serde_json::from_str(r#"{"auto_update_extensions": {"oracle-sql": false, "zig": true}}"#).unwrap();
        assert_eq!(pinned(&settings), ["oracle-sql"]);
        assert!(pinned(&Settings::default()).is_empty());
    }
}
