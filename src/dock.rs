//! Pinning apps to the quickshell dock, from an app row's right-click menu.
//!
//! The dock owns its pinned list; omni only reads it (to offer Pin or Unpin) and asks
//! the dock to change it over quickshell IPC, so the two never write the same file.
//! Pins are desktop entry ids without ".desktop" (com.spotify.Client), which is how
//! the dock stores them.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The dock's settings: the user override wins when it defines the pinned list,
/// otherwise the shipped file (the same order the dock merges them in).
fn settings_files() -> Vec<PathBuf> {
    let config = gtk::glib::user_config_dir();
    vec![config.join("ml4w-dock/dock.json"), config.join("ml4w/settings/dock.json")]
}

/// The dock's settings may carry a /* ... */ comment block and trailing commas.
fn parse(text: &str) -> Option<serde_json::Value> {
    let mut clean = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        clean.push_str(&rest[..start]);
        rest = rest[start + 2..].find("*/").map_or("", |end| &rest[start + 2 + end + 2..]);
    }
    clean.push_str(rest);
    serde_json::from_str(&clean).ok().or_else(|| {
        let relaxed = clean.replace(",\n}", "\n}").replace(",\n]", "\n]");
        serde_json::from_str(&relaxed).ok()
    })
}

/// Whether the app with this desktop id is pinned to the dock.
pub fn is_pinned(id: &str) -> bool {
    for file in settings_files() {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let Some(pinned) = parse(&text).and_then(|v| v.pointer("/apps/pinned").cloned()) else { continue };
        return pinned.as_array().is_some_and(|list| list.iter().any(|p| p.as_str() == Some(id)));
    }
    false
}

/// Ask the dock to pin or unpin the app. Runs `qs ipc` as an argument list (no shell)
/// in the background; a failure is logged, the dock is simply left as it was.
pub fn set_pinned(id: &str, pinned: bool) {
    let config = gtk::glib::user_config_dir().join("quickshell");
    let verb = if pinned { "pin" } else { "unpin" };
    let result = Command::new("qs")
        .arg("ipc")
        .arg("-p")
        .arg(&config)
        .args(["call", "dock", verb, id])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match result {
        // Reap it off the main thread so it never lingers as a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("omni: couldn't reach the dock to {verb} {id}: {e}"),
    }
}

/// A desktop entry id without its ".desktop" suffix.
pub fn id_of(info: &gtk::gio::AppInfo) -> Option<String> {
    use gtk::prelude::AppInfoExt;
    let id = info.id()?.to_string();
    Some(id.strip_suffix(".desktop").unwrap_or(&id).to_string())
}
