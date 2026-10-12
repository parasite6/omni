//! Recently used files and folders, from GTK's shared list (~/.local/share/recently-used.xbel),
//! the same one GNOME, Nautilus and file pickers write to.

use std::path::PathBuf;

use gtk::gio;
use gtk::prelude::*;

pub struct RecentFile {
    pub file: gio::File,
    pub path: PathBuf,
    pub is_dir: bool,
    /// Last used, unix seconds.
    pub time: i64,
}

/// The `limit` most recently used local files/folders that still exist, newest first.
pub fn recent_files(limit: usize) -> Vec<RecentFile> {
    if limit == 0 {
        return Vec::new();
    }
    let mut items: Vec<(i64, String)> = gtk::RecentManager::default()
        .items()
        .into_iter()
        .filter(|i| i.uri().starts_with("file://"))
        .map(|i| (i.modified().to_unix().max(i.visited().to_unix()), i.uri().to_string()))
        .collect();
    items.sort_by(|a, b| b.0.cmp(&a.0));

    // URIs come from GTK on this thread. Stat runs off it: a wedged mount must not
    // freeze the popup, and we stop once `limit` paths still exist.
    let rows: Vec<(i64, PathBuf)> = items
        .into_iter()
        .filter_map(|(time, uri)| {
            let path = gio::File::for_uri(&uri).path()?;
            Some((time, path))
        })
        .collect();
    if rows.is_empty() {
        return Vec::new();
    }
    let Some(batch) = crate::files::timed_batch(move |tx, cancel| {
        let mut found = 0usize;
        for (time, path) in rows {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) || found >= limit {
                return;
            }
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            if tx.send((time, path, meta.is_dir())).is_err() {
                return;
            }
            found += 1;
        }
    }) else {
        return Vec::new(); // a stat from this keystroke is already in flight
    };
    batch
        .items
        .into_iter()
        .map(|(time, path, is_dir)| RecentFile { file: gio::File::for_path(&path), path, is_dir, time })
        .collect()
}

/// Add something opened from omni to the shared recent list.
pub fn record(file: &gio::File) {
    gtk::RecentManager::default().add_item(&file.uri());
}

/// "~/Documents/notes" style display of a file's folder.
pub fn folder_label(path: &std::path::Path) -> String {
    let parent = path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    let home = gtk::glib::home_dir().to_string_lossy().to_string();
    match parent.strip_prefix(&home) {
        Some(rest) => format!("~{rest}"),
        None => parent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_folder_label_prefixes_home() {
        let home = gtk::glib::home_dir();
        assert_eq!(folder_label(&home.join("Documents").join("notes.txt")), "~/Documents");
        assert_eq!(folder_label(std::path::Path::new("/etc/passwd")), "/etc");
    }
}

/// Icon for a file from its name (no disk read), or the folder icon.
pub fn icon(path: &std::path::Path, is_dir: bool) -> gio::Icon {
    if is_dir {
        return gio::ThemedIcon::new("folder").upcast();
    }
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let (content_type, _) = gio::content_type_guess(Some(name.as_str()), None);
    gio::content_type_get_icon(&content_type)
}
