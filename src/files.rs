//! omni's own file-name index (like Everything / FSearch): every file and folder name under
//! the indexed folders, kept in memory and searched with omni's own ranking in a few ms.
//!
//! LocalSearch can't do this fast at ~2M+ files (unranked results in index order, ranked
//! queries take seconds), so omni walks the disk itself (in parallel, in the background) and
//! keeps up with new files through LocalSearch's change events. Deleted files are dropped
//! when results are shown (each shown path is checked), and a periodic re-walk catches
//! everything else. Which folders to index comes from LocalSearch's own settings.
//!
//! Storage is compact: names live in one big string, each entry points at its parent
//! folder, so full paths are only built for the handful of results shown.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

const NONE: u32 = u32::MAX;

const DIR: u8 = 1;
/// Inside the user's home folder.
const HOME: u8 = 2;
/// The name or a parent folder starts with '.'.
const HIDDEN: u8 = 4;
/// Inside a dependency/build/package folder (node_modules, site-packages, target, ...).
const JUNK: u8 = 8;
/// Inside one of the home folders (~/Documents, ~/Downloads, ...).
const XDG: u8 = 16;
/// One of the home folders itself.
const XDG_ROOT: u8 = 32;
/// A file directly in a `$PATH` folder: a command (python, git, ...).
const COMMAND: u8 = 64;
/// Commands shown per query, in their own group so home files and commands can't crowd
/// each other out.
pub const MAX_COMMANDS: usize = 3;

/// UI-thread filesystem calls give up after this. It sits under the 2s watchdog.
const STAT_TIMEOUT: Duration = Duration::from_millis(200);
/// One directory listing this slow is treated as a wedged mount; the walk publishes
/// what it already has instead of blocking forever.
const DIR_JOB_TIMEOUT: Duration = Duration::from_secs(5);

/// One stat batch at a time. A stuck stat cannot be cancelled, so a later keystroke
/// skips the check rather than leaking a thread per keystroke.
static STAT_BUSY: AtomicBool = AtomicBool::new(false);

/// Parallel tests would otherwise flake on `STAT_BUSY`. Production skips instead of waiting.
#[cfg(test)]
static STAT_TEST_LOCK: Mutex<()> = Mutex::new(());

/// The user's home folders (Documents, Downloads, ...), from XDG user-dirs, so renamed or
/// translated ones count too.
fn home_folders(home: &std::path::Path) -> Vec<PathBuf> {
    use glib::UserDirectory::*;
    [Desktop, Documents, Downloads, Music, Pictures, PublicShare, Templates, Videos]
        .into_iter()
        .filter_map(glib::user_special_dir)
        .filter(|p| p != home && p.parent() == Some(home))
        .collect()
}

/// Folder names whose contents are almost never what you're looking for. Still indexed,
/// just ranked well below everything else.
const JUNK_DIRS: &[&str] = &[
    "node_modules", "site-packages", "dist-packages", "__pypackages__", ".git", "target", "pkgs",
    "vendor", ".venv", "venv", ".npm", ".cargo", ".rustup", ".gradle", ".m2", "Trash", "icons",
    "locale", "flatpak", "containers", "anaconda3", "miniconda", "miniconda3", ".nvm",
    "build", "Build", "_build", "buildtrees", "vcpkg", "vcpkg_installed", "dist", "obj", "coredump",
    "steamapps", "__MACOSX",
];

/// The index. Memory is the point of this layout (~4M entries in ~70 MB, less with the
/// contents of `SKIP_INSIDE` folders left out):
/// - each distinct name is stored once, ASCII-lowercased (only 29% of names are distinct:
///   index.js, README.md, __init__.py ...); the real capitalization of the few results
///   shown is read back from disk;
/// - entries are grouped by name, so an entry is just its parent and a flags byte.
pub struct Index {
    /// Distinct lowercase names, each followed by a NUL; the arena starts with a NUL, so
    /// every name is preceded by one ("\0" + word finds names starting with word).
    arena: Vec<u8>,
    /// Start of each distinct name in `arena` (ascending), plus a sentinel.
    name_off: Vec<u32>,
    /// Name `u` owns entries `name_start[u]..name_start[u + 1]` (plus a sentinel).
    name_start: Vec<u32>,
    /// Per entry: parent entry (NONE for a root) and flags.
    parent: Vec<u32>,
    flags: Vec<u8>,
    /// Root entries and their real (correctly capitalized) paths.
    roots: Vec<(u32, PathBuf)>,
    /// `$PATH` folders in the index and their place in `$PATH` (0 = searched first).
    command_dirs: HashMap<u32, u16>,
}

#[derive(Clone)]
pub struct FileHit {
    pub path: PathBuf,
    pub is_dir: bool,
    /// One of the home folders (~/Documents, ...): shown above the file results.
    pub home_folder: bool,
    /// A command: an executable directly in a `$PATH` folder.
    pub command: bool,
    score: i32,
}

/// Folders omni indexes names under, besides whatever LocalSearch covers. Names are
/// indexed everywhere; LocalSearch (contents) is scoped to home and drives on purpose.
fn name_roots(home: &std::path::Path) -> Vec<PathBuf> {
    let mut roots = vec![home.to_path_buf()];
    roots.extend(["/usr", "/etc", "/opt", "/var", "/srv", "/mnt"].iter().map(PathBuf::from));
    roots.push(PathBuf::from("/run/media").join(glib::user_name()));
    roots
}

/// Folders to index names under (omni's own list plus LocalSearch's) and folder names to
/// skip (cache folders plus LocalSearch's ignore list).
pub fn config() -> (Vec<PathBuf>, HashSet<String>) {
    let home = glib::home_dir();
    let mut roots: Vec<PathBuf> = name_roots(&home);
    let mut ignored: HashSet<String> = [".cache", "cache", "Cache", "caches", "__pycache__"].iter().map(|s| s.to_string()).collect();

    let schema = "org.freedesktop.Tracker3.Miner.Files";
    let available = gio::SettingsSchemaSource::default().and_then(|s| s.lookup(schema, true)).is_some();
    if available {
        let settings = gio::Settings::new(schema);
        for dir in settings.strv("index-recursive-directories") {
            let dir = dir.as_str();
            let path = match dir {
                "$HOME" => Some(home.clone()),
                "&DESKTOP" => glib::user_special_dir(glib::UserDirectory::Desktop),
                "&DOCUMENTS" => glib::user_special_dir(glib::UserDirectory::Documents),
                "&DOWNLOAD" => glib::user_special_dir(glib::UserDirectory::Downloads),
                "&MUSIC" => glib::user_special_dir(glib::UserDirectory::Music),
                "&PICTURES" => glib::user_special_dir(glib::UserDirectory::Pictures),
                "&VIDEOS" => glib::user_special_dir(glib::UserDirectory::Videos),
                d if d.starts_with('/') => Some(PathBuf::from(d)),
                _ => None,
            };
            roots.extend(path);
        }
        ignored.extend(settings.strv("ignored-directories").iter().map(|s| s.to_string()));
    }
    // Drop roots inside other roots (e.g. ~/Documents inside ~) and ones that don't exist.
    roots.sort();
    roots.dedup();
    let all = roots.clone();
    roots.retain(|r| r.is_dir() && !all.iter().any(|o| o != r && r.starts_with(o)));
    (roots, ignored)
}

impl Index {
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    /// Memory held by the index, in bytes.
    pub fn memory(&self) -> usize {
        self.arena.capacity()
            + 4 * (self.name_off.capacity() + self.name_start.capacity() + self.parent.capacity())
            + self.flags.capacity()
    }

    /// Walk `roots` in parallel and build the index. Takes a few seconds for millions of
    /// files; call it off the UI thread.
    pub fn build(roots: &[PathBuf], ignored: &HashSet<String>) -> Self {
        Self::build_with_path(roots, ignored, std::env::var_os("PATH").as_deref())
    }

    /// `build`, with commands taken from `path_var` (a `$PATH` value).
    fn build_with_path(roots: &[PathBuf], ignored: &HashSet<String>, path_var: Option<&std::ffi::OsStr>) -> Self {
        let home = glib::home_dir();
        // $PATH folders, each at its first place in $PATH: that's the one the shell searches.
        // Keyed by the resolved path (/usr/sbin is a link to /usr/bin on Fedora) and by the
        // path as written, since the walk uses the path as written (a symlinked ancestor of
        // $HOME). Relative and empty entries are skipped: they'd resolve against omni's cwd.
        let mut path_rank: HashMap<PathBuf, u16> = HashMap::new();
        let mut rank_of: HashMap<PathBuf, u16> = HashMap::new();
        for dir in path_var.map(std::env::split_paths).into_iter().flatten() {
            if !dir.is_absolute() {
                continue;
            }
            let Ok(resolved) = dir.canonicalize() else { continue };
            let next = rank_of.len().min(u16::MAX as usize) as u16;
            let rank = *rank_of.entry(resolved.clone()).or_insert(next);
            path_rank.entry(resolved).or_insert(rank);
            path_rank.entry(dir).or_insert(rank);
        }
        let mut command_dirs_old: HashMap<u32, u16> = HashMap::new();
        // `SKIP_INSIDE` folders aren't walked, except on the way down to a $PATH folder
        // (~/.cargo/bin): those and their ancestors are walked as usual.
        let path_chain: HashSet<PathBuf> =
            path_rank.keys().flat_map(|dir| dir.ancestors().map(PathBuf::from)).collect();

        // Walk: entries in discovery order with an id per distinct lowercase name.
        let mut names: Vec<Box<str>> = Vec::new();
        let mut ids: std::collections::HashMap<Box<str>, u32> = std::collections::HashMap::new();
        let mut e_parent: Vec<u32> = Vec::new();
        let mut e_name: Vec<u32> = Vec::new();
        let mut e_flags: Vec<u8> = Vec::new();
        let mut intern = |name: &str| -> u32 {
            let lower = name.to_ascii_lowercase();
            if let Some(&id) = ids.get(lower.as_str()) {
                return id;
            }
            let id = names.len() as u32;
            let boxed: Box<str> = lower.into_boxed_str();
            names.push(boxed.clone());
            ids.insert(boxed, id);
            id
        };

        struct Job {
            idx: u32,
            path: PathBuf,
            flags: u8,
        }
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        // (job, this dir's (st_dev, st_ino), children: name, is_dir, child dir id).
        type DirId = (u64, u64);
        let (out_tx, out_rx) = mpsc::channel::<(Job, Option<DirId>, Vec<(String, bool, Option<DirId>)>)>();

        let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
        let mut handles = Vec::new();
        for _ in 0..workers {
            let (rx, tx) = (job_rx.clone(), out_tx.clone());
            handles.push(std::thread::spawn(move || {
                lower_priority();
                loop {
                    let job = match rx.lock().unwrap().recv() {
                        Ok(j) => j,
                        Err(_) => return, // all jobs done
                    };
                    // symlink_metadata does not follow links. A bind mount of a directory
                    // inside itself shares (st_dev, st_ino); symlinks are not dirs here.
                    let self_id = std::fs::symlink_metadata(&job.path).ok().map(|m| (m.dev(), m.ino()));
                    let mut children = Vec::new();
                    if let Ok(dir) = std::fs::read_dir(&job.path) {
                        for e in dir.flatten() {
                            // file_type() doesn't follow symlinks: a symlinked folder is listed
                            // but never descended into (no loops through links, no escaping the roots).
                            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                            let id = if is_dir {
                                std::fs::symlink_metadata(e.path()).ok().map(|m| (m.dev(), m.ino()))
                            } else {
                                None
                            };
                            children.push((e.file_name().to_string_lossy().into_owned(), is_dir, id));
                        }
                    }
                    if tx.send((job, self_id, children)).is_err() {
                        return;
                    }
                }
            }));
        }
        drop(out_tx);

        // Home folder names (lowercase isn't used: these are matched as walked).
        let xdg_names: Vec<String> = home_folders(&home)
            .iter()
            .filter_map(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
            .collect();
        let mut root_paths: Vec<(u32, PathBuf)> = Vec::new();
        let mut pending = 0usize;
        for root in roots {
            let mut flags = DIR;
            if root.starts_with(&home) {
                flags |= HOME;
            }
            let idx = e_parent.len() as u32;
            e_parent.push(NONE);
            e_name.push(intern(&root.to_string_lossy()));
            e_flags.push(flags);
            root_paths.push((idx, root.clone()));
            pending += 1;
            let _ = job_tx.send(Job { idx, path: root.clone(), flags });
        }

        // Collect results on this thread; hand out child folders as jobs.
        // (st_dev, st_ino) already walked. Bind-mount loops share an inode; other
        // filesystems (/mnt, /run/media, a mount under home) stay, they have their own.
        let mut seen: HashSet<(u64, u64)> = HashSet::new();
        while pending > 0 {
            // A read_dir that never returns must not pin this walk. Healthy workers exit
            // once job_tx is dropped; a stuck one is detached and may outlive the walk.
            let Ok((job, self_id, children)) = out_rx.recv_timeout(DIR_JOB_TIMEOUT) else { break };
            pending -= 1;
            if let Some(id) = self_id {
                seen.insert(id);
            }
            let in_home_root = job.path == home;
            let command_rank = path_rank.get(&job.path).copied();
            if let Some(rank) = command_rank {
                command_dirs_old.insert(job.idx, rank);
            }
            for (name, is_dir, id) in children {
                if is_dir && ignored.contains(&name) {
                    continue;
                }
                let mut flags = job.flags & (HOME | HIDDEN | JUNK | XDG);
                if is_dir && in_home_root && xdg_names.contains(&name) {
                    flags |= XDG_ROOT | XDG;
                }
                if name.starts_with('.') {
                    flags |= HIDDEN;
                }
                if is_dir {
                    flags |= DIR;
                    if JUNK_DIRS.contains(&name.as_str()) {
                        flags |= JUNK;
                    }
                } else if command_rank.is_some() {
                    flags |= COMMAND;
                }
                let idx = e_parent.len() as u32;
                e_parent.push(job.idx);
                e_name.push(intern(&name));
                e_flags.push(flags);
                let child = job.path.join(&name);
                if is_dir && skip_inside(&name) && !path_chain.contains(&child) {
                    continue;
                }
                if is_dir && claim_dir(&mut seen, id) {
                    pending += 1;
                    // Children inherit "inside a home folder", not "is the home folder".
                    let _ = job_tx.send(Job { idx, path: child, flags: flags & !XDG_ROOT });
                }
            }
        }
        drop(job_tx);
        drop(handles); // detach; joining would block the next walk on a stuck read_dir
        drop(ids);

        // Arena of distinct names (in id order) and their offsets.
        let mut arena: Vec<u8> = Vec::with_capacity(1 + names.iter().map(|n| n.len() + 1).sum::<usize>());
        arena.push(0);
        let mut name_off: Vec<u32> = Vec::with_capacity(names.len() + 1);
        for n in &names {
            name_off.push(arena.len() as u32);
            arena.extend_from_slice(n.as_bytes());
            arena.push(0);
        }
        name_off.push(arena.len() as u32 + 1); // sentinel: a name ends at the next start - 1
        drop(names);

        // Group entries by name (counting sort), remapping parent links.
        let n_names = name_off.len() - 1;
        let mut name_start = vec![0u32; n_names + 1];
        for &id in &e_name {
            name_start[id as usize + 1] += 1;
        }
        for i in 0..n_names {
            name_start[i + 1] += name_start[i];
        }
        let mut cursor = name_start.clone();
        let mut new_idx = vec![0u32; e_name.len()];
        for (old, &id) in e_name.iter().enumerate() {
            new_idx[old] = cursor[id as usize];
            cursor[id as usize] += 1;
        }
        drop(cursor);
        drop(e_name);
        let mut parent = vec![NONE; e_parent.len()];
        let mut flags = vec![0u8; e_parent.len()];
        for old in 0..e_parent.len() {
            let n = new_idx[old] as usize;
            parent[n] = if e_parent[old] == NONE { NONE } else { new_idx[e_parent[old] as usize] };
            flags[n] = e_flags[old];
        }
        let roots = root_paths.into_iter().map(|(i, p)| (new_idx[i as usize], p)).collect();
        let command_dirs = command_dirs_old.into_iter().map(|(i, rank)| (new_idx[i as usize], rank)).collect();

        Index { arena, name_off, name_start, parent, flags, roots, command_dirs }
    }

    /// Distinct name containing arena byte `pos`.
    fn name_at(&self, pos: usize) -> u32 {
        (self.name_off.partition_point(|&o| o as usize <= pos) - 1) as u32
    }

    fn name(&self, u: u32) -> &[u8] {
        let start = self.name_off[u as usize] as usize;
        // Each name ends where the next starts minus its NUL (the sentinel covers the last).
        let end = (self.name_off[u as usize + 1] as usize - 1).min(self.arena.len());
        let name = &self.arena[start..end];
        name.strip_suffix(&[0]).unwrap_or(name)
    }

    fn name_of_entry(&self, e: u32) -> u32 {
        (self.name_start.partition_point(|&s| s <= e) - 1) as u32
    }

    /// `e` is somewhere below one of the `dirs` entries.
    fn inside(&self, mut e: u32, dirs: &[u32]) -> bool {
        while self.parent[e as usize] != NONE {
            e = self.parent[e as usize];
            if dirs.contains(&e) {
                return true;
            }
        }
        false
    }

    /// Entries for the folder `dir`, matched case-insensitively ("~/music" finds ~/Music;
    /// several if folders differ only in case). Empty if it isn't in the index. A folder
    /// above the roots gives the roots inside it.
    pub fn entries_at(&self, dir: &std::path::Path) -> Vec<u32> {
        let lower = |p: &std::path::Path| PathBuf::from(p.to_string_lossy().to_ascii_lowercase());
        let dir = lower(dir);
        let Some((root, rest)) = self
            .roots
            .iter()
            .filter_map(|(e, path)| Some((*e, dir.strip_prefix(lower(path)).ok()?.to_path_buf())))
            .max_by_key(|(_, rest)| std::cmp::Reverse(rest.components().count()))
        else {
            // Above the indexed roots ("/", "/home", "/run/media"): the roots inside it.
            // Their own descendants are what's in scope; the roots themselves aren't listed.
            return self.roots.iter().filter(|(_, path)| lower(path).starts_with(&dir)).map(|(e, _)| *e).collect();
        };
        let mut current = vec![root];
        for part in rest.components() {
            let name = part.as_os_str().to_string_lossy();
            // The name's id: "\0name\0" in the arena.
            let pattern = [&[0u8][..], name.as_bytes(), &[0u8][..]].concat();
            let Some(pos) = memchr::memmem::find(&self.arena, &pattern) else { return Vec::new() };
            let u = self.name_at(pos + 1) as usize;
            current = (self.name_start[u]..self.name_start[u + 1])
                .filter(|&e| self.flags[e as usize] & DIR != 0 && current.contains(&self.parent[e as usize]))
                .collect();
            if current.is_empty() {
                break;
            }
        }
        current
    }

    fn depth(&self, mut e: u32) -> i32 {
        let mut d = 0;
        while self.parent[e as usize] != NONE {
            e = self.parent[e as usize];
            d += 1;
        }
        d
    }

    /// Root-first components of `e`. Names are lowercase; capitalization is restored
    /// later, off the UI thread. None if a component isn't valid UTF-8.
    fn chain(&self, mut e: u32) -> Option<Vec<Step>> {
        let mut steps = Vec::new();
        loop {
            let parent = self.parent[e as usize];
            if parent == NONE {
                let root = self.roots.iter().find(|(i, _)| *i == e)?.1.clone();
                steps.push(Step { entry: e, name: None, root: Some(root) });
                break;
            }
            let lower = std::str::from_utf8(self.name(self.name_of_entry(e))).ok()?.to_string();
            steps.push(Step { entry: e, name: Some(lower), root: None });
            e = parent;
        }
        steps.reverse();
        Some(steps)
    }

    /// Best files and folders whose names contain every word of `query`.
    /// `scope`: only entries inside these folders (see `entries_at`), at any depth.
    /// Commands (see `MAX_COMMANDS`) come first in the files list, flagged `command`.
    pub fn search(&self, query: &str, max_files: usize, max_dirs: usize, scope: Option<&[u32]>) -> (Vec<FileHit>, Vec<FileHit>) {
        if scope.is_some_and(|s| s.is_empty()) {
            return (Vec::new(), Vec::new()); // the folder isn't in the index
        }
        let query = bounded_query(query);
        let tokens = parse_query_tokens(query);
        let words: Vec<Vec<u8>> = tokens.iter().map(QueryToken::name_needle).collect();
        if words.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let whole = query.trim().to_ascii_lowercase().into_bytes();
        // Keep extra candidates (some may turn out deleted when checked), separately for
        // files, folders and commands so one can't crowd out another.
        let keeps = [max_files * 3, max_dirs * 3, MAX_COMMANDS * 3];

        // SIMD scan for the longest word over all distinct names, split across cores; the
        // other words are checked only on those hits. One or two letters match name starts
        // only, plus a name ending in ".word" so "md" finds readme.md. Not a substring:
        // a one-letter query must not hit every name that merely contains that letter.
        let longest = words.iter().max_by_key(|w| w.len()).unwrap().clone();
        let prefix_only = longest.len() <= 2;
        // (pattern, match starts on the NUL before the name).
        let mut patterns: Vec<(Vec<u8>, bool)> = Vec::new();
        if prefix_only {
            patterns.push(([&[0u8][..], &longest].concat(), true));
            let mut ext = Vec::with_capacity(longest.len() + 2);
            ext.push(b'.');
            ext.extend_from_slice(&longest);
            ext.push(0);
            patterns.push((ext, false));
        } else {
            patterns.push((longest, false));
        }
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
        let chunk = self.arena.len().div_ceil(threads).max(1);
        let mut candidates: Vec<(i32, u32)> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|t| {
                    let (words, whole, patterns) = (&words, &whole, &patterns);
                    s.spawn(move || {
                        // This thread owns matches *starting* in [lo, hi).
                        let lo = (t * chunk).min(self.arena.len());
                        let hi = ((t + 1) * chunk).min(self.arena.len());
                        // [files, folders, commands]: candidates and the score to beat once full.
                        let mut best: [Vec<(i32, u32)>; 3] = [Vec::new(), Vec::new(), Vec::new()];
                        let mut floor = [i32::MIN; 3];
                        let mut scored = HashSet::new();
                        for (pattern, at_nul) in patterns {
                            let end = (hi + pattern.len()).min(self.arena.len());
                            let finder = memchr::memmem::Finder::new(pattern);
                            let mut last = NONE;
                            for pos in finder.find_iter(&self.arena[lo..end]) {
                                let pos = lo + pos;
                                if pos >= hi {
                                    break;
                                }
                                // In prefix mode the match starts at the NUL before the name.
                                let at = if *at_nul { pos + 1 } else { pos };
                                if at == 0 || at >= self.arena.len() {
                                    continue;
                                }
                                let u = self.name_at(at);
                                if u == last || !scored.insert(u) {
                                    continue;
                                }
                                last = u;
                                let name = self.name(u);
                                // Score the name once, then each place it occurs.
                                let as_file = score_name(name, false, words, whole);
                                let as_dir = score_name(name, true, words, whole);
                                if as_file.is_none() && as_dir.is_none() {
                                    continue;
                                }
                                // Typed exactly ("python"): the command the shell would run is the
                                // one from the earliest $PATH folder holding that name.
                                let shell_pick = (name == whole.as_slice())
                                    .then(|| {
                                        (self.name_start[u as usize]..self.name_start[u as usize + 1])
                                            .filter(|&e| self.flags[e as usize] & COMMAND != 0)
                                            .min_by_key(|&e| self.command_dirs.get(&self.parent[e as usize]).copied().unwrap_or(u16::MAX))
                                    })
                                    .flatten();
                                for e in self.name_start[u as usize]..self.name_start[u as usize + 1] {
                                    if scope.is_some_and(|s| !self.inside(e, s)) {
                                        continue;
                                    }
                                    let flags = self.flags[e as usize];
                                    let Some(base) = (if flags & DIR != 0 { as_dir } else { as_file }) else { continue };
                                    let mut score = base + adjust(flags, self.depth(e), name.len());
                                    if shell_pick == Some(e) {
                                        score += COMMAND_BONUS;
                                    }
                                    let k = if flags & DIR != 0 { 1 } else if flags & COMMAND != 0 { 2 } else { 0 };
                                    if best[k].len() >= keeps[k] && score <= floor[k] {
                                        continue;
                                    }
                                    best[k].push((score, e));
                                    if best[k].len() >= keeps[k] * 2 {
                                        best[k].sort_unstable_by(|a, b| b.0.cmp(&a.0));
                                        best[k].truncate(keeps[k]);
                                        floor[k] = best[k].last().map_or(i32::MIN, |b| b.0);
                                    }
                                }
                            }
                        }
                        let [f, d, c] = best;
                        f.into_iter().chain(d).chain(c).collect::<Vec<_>>()
                    })
                })
                .collect();
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        // Equal scores in a fixed order: commands first, newest version first among them
        // (python3.15 before python3.9); everything else in natural order. This must stay a
        // total order, or sort_unstable_by may panic.
        candidates.sort_unstable_by(|a, b| {
            let (na, nb) = (self.name(self.name_of_entry(a.1)), self.name(self.name_of_entry(b.1)));
            let (ca, cb) = (self.flags[a.1 as usize] & COMMAND != 0, self.flags[b.1 as usize] & COMMAND != 0);
            let by_name = if ca && cb { natural_cmp(nb, na) } else { natural_cmp(na, nb) };
            b.0.cmp(&a.0).then(cb.cmp(&ca)).then(by_name).then(a.1.cmp(&b.1))
        });

        let planned: Vec<(i32, bool, bool, bool, Vec<Step>)> = candidates
            .into_iter()
            .filter_map(|(score, e)| {
                let steps = self.chain(e)?;
                let flags = self.flags[e as usize];
                Some((score, flags & DIR != 0, flags & XDG_ROOT != 0, flags & COMMAND != 0, steps))
            })
            .collect();
        if planned.is_empty() {
            return (Vec::new(), Vec::new());
        }
        // Stat off this thread. A wedged mount gives up after STAT_TIMEOUT; paths already
        // resolved are kept, the rest of this keystroke is skipped.
        let Some(batch) = timed_batch(move |tx, cancel| {
            let mut cache: HashMap<u32, PathBuf> = HashMap::new();
            let mut files = 0usize;
            let mut dirs = 0usize;
            let mut commands = 0usize;
            let mut seen_paths = HashSet::new();
            for (score, is_dir, home_folder, mut command, steps) in planned {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                if files >= max_files && dirs >= max_dirs && commands >= MAX_COMMANDS {
                    break;
                }
                if is_dir {
                    if dirs >= max_dirs {
                        continue;
                    }
                } else if command {
                    // A command that turns out not to run can still take a file slot, so
                    // skip before the stat only when both groups are full.
                    if commands >= MAX_COMMANDS && files >= max_files {
                        continue;
                    }
                } else if files >= max_files {
                    continue;
                }
                // Deleted since the last walk: skipped. Names that differ only in case
                // resolve to one path; list it once.
                let Some(path) = resolve_chain(&steps, &mut cache, cancel) else {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    continue;
                };
                if !seen_paths.insert(path.clone()) {
                    continue;
                }
                // Only what can actually run counts as a command (a README in a bin
                // folder is a file); checked here, off the UI thread, for the few shown.
                if command && !is_runnable(&path) {
                    if files >= max_files {
                        continue;
                    }
                    command = false;
                } else if command && commands >= MAX_COMMANDS {
                    continue;
                }
                if is_dir {
                    dirs += 1;
                } else if command {
                    commands += 1;
                } else {
                    files += 1;
                }
                if tx.send(FileHit { path, is_dir, home_folder, command, score }).is_err() {
                    return;
                }
            }
        }) else {
            return (Vec::new(), Vec::new());
        };
        let mut commands = Vec::new();
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        for hit in batch.items {
            if hit.is_dir {
                if dirs.len() < max_dirs {
                    dirs.push(hit);
                }
            } else if hit.command {
                if commands.len() < MAX_COMMANDS {
                    commands.push(hit);
                }
            } else if files.len() < max_files {
                files.push(hit);
            }
        }
        commands.extend(files);
        (commands, dirs)
    }
}

/// One component of an index path. `root` is set only for the indexed root (already
/// correctly capitalized). Every other step is a lowercase name.
struct Step {
    entry: u32,
    name: Option<String>,
    root: Option<PathBuf>,
}

/// Restore capitalization. `symlink_metadata` / `read_dir` run on the stat thread.
/// `None` if the path is gone, or `cancel` was set (the caller already stopped waiting).
fn resolve_chain(steps: &[Step], cache: &mut HashMap<u32, PathBuf>, cancel: &AtomicBool) -> Option<PathBuf> {
    let mut last = None;
    for step in steps {
        if let Some(path) = cache.get(&step.entry) {
            last = Some(path.clone());
            continue;
        }
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let path = if let Some(root) = &step.root {
            root.clone()
        } else {
            let dir = last.clone()?;
            let lower = step.name.as_deref()?;
            // Fast path: the name really is lowercase.
            let direct = dir.join(lower);
            if std::fs::symlink_metadata(&direct).is_ok() {
                direct
            } else {
                std::fs::read_dir(&dir).ok()?.flatten().find(|ent| ent.file_name().to_string_lossy().eq_ignore_ascii_case(lower))?.path()
            }
        };
        cache.insert(step.entry, path.clone());
        last = Some(path);
    }
    last
}

/// `true` when `id` was not already walked. A missing id (stat failed) still descends;
/// the bind-mount bug is the same `(st_dev, st_ino)` showing up twice.
fn claim_dir(seen: &mut HashSet<(u64, u64)>, id: Option<(u64, u64)>) -> bool {
    match id {
        Some(id) => seen.insert(id),
        None => true,
    }
}

/// First 8 words and first 256 bytes. The cut is always a char boundary so a huge paste
/// cannot panic on a split codepoint or pin the UI ranking a giant query.
fn bounded_query(query: &str) -> &str {
    let mut end = query.len().min(256);
    while end > 0 && !query.is_char_boundary(end) {
        end -= 1;
    }
    let head = &query[..end];
    let mut words = 0usize;
    let mut in_word = false;
    for (i, c) in head.char_indices() {
        if c.is_whitespace() {
            if in_word {
                in_word = false;
                if words >= 8 {
                    return &head[..i];
                }
            }
        } else if !in_word {
            in_word = true;
            words += 1;
        }
    }
    head
}

/// mtime or ctime is at least `not_before` (unix seconds).
fn fresh_enough(mtime: i64, ctime: i64, not_before: i64) -> bool {
    mtime.max(ctime) >= not_before
}

/// Added to the command the shell would run for an exact-name query (earliest $PATH folder).
/// Above any home or ~/Documents bonus, so it leads its group.
const COMMAND_BONUS: i32 = 1200;

/// A regular file (links followed) the user may execute.
fn is_runnable(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    // SAFETY: a valid NUL-terminated path; access only reads it.
    path.is_file() && unsafe { libc::access(c_path.as_ptr(), libc::X_OK) } == 0
}

/// Byte order, except that runs of digits compare as numbers ("3.9" < "3.15").
fn natural_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let (si, sj) = (i, j);
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            // Without leading zeros, a longer run is the larger number.
            let (x, y) = (trim_zeros(&a[si..i]), trim_zeros(&b[sj..j]));
            let ord = x.len().cmp(&y.len()).then_with(|| x.cmp(y));
            if ord != Ordering::Equal {
                return ord;
            }
        } else {
            if a[i] != b[j] {
                return a[i].cmp(&b[j]);
            }
            i += 1;
            j += 1;
        }
    }
    (a.len() - i).cmp(&(b.len() - j))
}

fn trim_zeros(digits: &[u8]) -> &[u8] {
    let start = digits.iter().position(|&d| d != b'0').unwrap_or(digits.len());
    &digits[start..]
}

/// Score a lowercase name against the lowercase query words (name-only part), or None if
/// any word is missing from it.
fn score_name(name: &[u8], is_dir: bool, words: &[Vec<u8>], whole: &[u8]) -> Option<i32> {
    let mut s = 0i32;
    let dot = name.iter().rposition(|&b| b == b'.').filter(|&p| p > 0);
    for (i, w) in words.iter().enumerate() {
        let pos = memchr::memmem::find(name, w)?;
        // Typing the extension ("pdf") means "files of that type", not "pdf" in the name.
        if !is_dir && dot.is_some_and(|d| pos == d + 1 && name.len() == d + 1 + w.len()) {
            s += 220;
            continue;
        }
        let at_word_start = pos == 0 || !name[pos - 1].is_ascii_alphanumeric();
        s += match (i, pos, at_word_start) {
            (0, 0, _) => 300,    // name starts with the query
            (_, _, true) => 180, // a word in the name starts with it
            _ => 60,             // somewhere inside
        };
    }
    // The whole name (or the name without its extension) is exactly the query.
    let stem = dot.map_or(name, |p| &name[..p]);
    if stem == whole || name == whole {
        s += 500;
    }
    Some(s)
}

/// Folders managed by a tool rather than by you: package stores, editor state, VCS
/// internals, toolchains, Trash. The folder itself is indexed; what's inside isn't walked:
/// it's a large share of a disk's names (every name is RAM) and never the file you're after.
/// Your own dotfiles (~/.config, ~/.local) and project environments (.venv) are walked.
const SKIP_INSIDE: &[&str] = &[
    "node_modules", "bower_components", ".npm", ".yarn", ".pnpm-store", ".nvm",
    ".vscode", ".vscode-server", ".vscode-oss", ".cursor", ".idea",
    ".git", ".hg", ".svn",
    ".rustup", ".cargo", ".gradle", ".m2",
    ".tox", ".mypy_cache", ".pytest_cache", ".ruff_cache", ".next", ".nuxt", ".turbo", ".parcel-cache",
    "Trash", ".Trash", "flatpak", "containers",
];

fn skip_inside(name: &str) -> bool {
    SKIP_INSIDE.contains(&name) || name.starts_with(".Trash-")
}

/// `dir` is, or is inside, a folder the walk doesn't enter.
fn inside_skipped(dir: &std::path::Path) -> bool {
    if std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir)) {
        return false;
    }
    dir.components().any(|c| skip_inside(&c.as_os_str().to_string_lossy()))
}

/// Where the entry lives: home first, hidden and dependency/build folders down, shallow
/// paths and shorter names up.
fn adjust(flags: u8, depth: i32, name_len: usize) -> i32 {
    let mut s = 0;
    if flags & HOME != 0 {
        s += 150;
    }
    if flags & HIDDEN != 0 {
        s -= 250;
    }
    if flags & JUNK != 0 {
        s -= 400;
    }
    if flags & XDG_ROOT != 0 {
        s += 900; // ~/Documents beats every other "documents" folder
    } else if flags & XDG != 0 {
        s += 120; // your own files over the same name elsewhere
    }
    s - depth * 12 - name_len as i32 / 3
}

/// Files that appeared since the last walk (from LocalSearch's events). Searched alongside
/// the index. A finished walk drops only entries older than when that walk started, so a
/// file created after its directory was listed is kept. `since` is then the end of the walk.
#[derive(Default)]
pub struct Extras {
    items: Vec<(PathBuf, bool)>,
    seen: HashSet<PathBuf>,
    /// Unix seconds. Set to the start of a walk while it runs, then to the end once the
    /// new index is published. Live events compare against this.
    since: i64,
    /// Start of the latest walk. Events held until the first index exists use this:
    /// `since` has already moved to the end of that walk by the time they are applied.
    walk_started: i64,
    /// Too many new files since the walk: re-walk early instead of growing this list.
    pub overflow: bool,
}

/// Most new files tracked between walks; beyond this a fresh walk is cheaper.
const EXTRAS_CAP: usize = 20_000;

impl Extras {
    fn add(&mut self, path: PathBuf, is_dir: bool) {
        if self.items.len() >= EXTRAS_CAP {
            self.overflow = true;
            return;
        }
        if self.seen.insert(path.clone()) {
            self.items.push((path, is_dir));
        }
    }

    /// Drop paths stat'd as missing or older than the walk start, then stamp `since`
    /// with the end of the walk. Paths not in `drop_paths` (stat timed out, or added
    /// while we were stating) stay.
    fn commit_walk(&mut self, ended_at: i64, drop_paths: &HashSet<PathBuf>) {
        if !drop_paths.is_empty() {
            self.items.retain(|(path, _)| !drop_paths.contains(path));
        }
        self.seen.clear();
        self.seen.extend(self.items.iter().map(|(path, _)| path.clone()));
        self.since = ended_at;
        self.overflow = false;
    }

    /// `scope`: only paths inside this folder (compared case-insensitively).
    /// Does not stat. `files::search` checks existence after it drops the index lock.
    pub fn search(&self, query: &str, scope: Option<&std::path::Path>) -> Vec<FileHit> {
        let query = bounded_query(query);
        let scope = scope.map(|s| s.to_string_lossy().to_ascii_lowercase().trim_end_matches('/').to_string() + "/");
        let tokens = parse_query_tokens(query);
        let words: Vec<Vec<u8>> = tokens.iter().map(QueryToken::name_needle).collect();
        if words.is_empty() {
            return Vec::new();
        }
        let whole = query.trim().to_ascii_lowercase().into_bytes();
        let home = glib::home_dir();
        let home_dirs = home_folders(&home);
        self.items
            .iter()
            .filter_map(|(path, is_dir)| {
                if let Some(scope) = &scope {
                    if !path.to_string_lossy().to_ascii_lowercase().starts_with(scope.as_str()) {
                        return None;
                    }
                }
                // Same rule as the walk: nothing from inside a SKIP_INSIDE folder.
                if path.parent().is_some_and(inside_skipped) {
                    return None;
                }
                let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
                let mut flags = if *is_dir { DIR } else { 0 };
                if path.starts_with(&home) {
                    flags |= HOME;
                }
                if path.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
                    flags |= HIDDEN;
                }
                for dir in &home_dirs {
                    if path == dir {
                        flags |= XDG_ROOT | XDG;
                    } else if path.starts_with(&dir) {
                        flags |= XDG;
                    }
                }
                let depth = path.components().count() as i32;
                let score = score_name(name.as_bytes(), *is_dir, &words, &whole)? + adjust(flags, depth, name.len());
                let home_folder = flags & XDG_ROOT != 0;
                Some(FileHit { path: path.clone(), is_dir: *is_dir, home_folder, command: false, score })
            })
            .collect()
    }
}

/// Merge index results with extras, best first, without duplicates.
pub fn merge(mut a: Vec<FileHit>, b: Vec<FileHit>, max: usize) -> Vec<FileHit> {
    for hit in b {
        if !a.iter().any(|h| h.path == hit.path) {
            a.push(hit);
        }
    }
    a.sort_by(|x, y| y.score.cmp(&x.score));
    a.truncate(max);
    a
}

/// Shared state between the UI and the background walker.
pub type Shared = Arc<Mutex<(Option<Arc<Index>>, Extras)>>;

/// What a timed filesystem batch managed to finish.
pub(crate) struct BatchResult<T> {
    pub items: Vec<T>,
    /// `work` returned before the deadline.
    pub finished: bool,
}

/// Run `work` on a background thread and collect what it sends within [`STAT_TIMEOUT`].
/// `None` means another batch is still in flight: skip this check. On timeout the thread
/// is left running (`cancel` is set so it stops between paths) and is not joined.
/// Do not call this while holding the index mutex, and do not call it from inside `work`.
pub(crate) fn timed_batch<T: Send + 'static>(work: impl FnOnce(&mpsc::Sender<T>, &AtomicBool) + Send + 'static) -> Option<BatchResult<T>> {
    // Tests run in parallel; production skips when a batch is already running.
    #[cfg(test)]
    let _serialize = STAT_TEST_LOCK.lock().unwrap();

    if STAT_BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return None;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let worker_cancel = Arc::clone(&cancel);
    let spawned = std::thread::Builder::new().name("omni-stat".into()).spawn(move || {
        // Clear the gate before dropping the sender, so the waiter observes "free"
        // by the time it sees the channel disconnect.
        struct Done<T> {
            tx: Option<mpsc::Sender<T>>,
        }
        impl<T> Drop for Done<T> {
            fn drop(&mut self) {
                STAT_BUSY.store(false, Ordering::Release);
                self.tx.take();
            }
        }
        let done = Done { tx: Some(tx) };
        work(done.tx.as_ref().unwrap(), worker_cancel.as_ref());
    });
    if spawned.is_err() {
        STAT_BUSY.store(false, Ordering::Release);
        return None;
    }
    let deadline = std::time::Instant::now() + STAT_TIMEOUT;
    let mut items = Vec::new();
    let finished = loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break false;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(item) => items.push(item),
            Err(mpsc::RecvTimeoutError::Timeout) => break false,
            Err(mpsc::RecvTimeoutError::Disconnected) => break true,
        }
    };
    if !finished {
        cancel.store(true, Ordering::Release);
    }
    Some(BatchResult { items, finished })
}

/// Canonicalize `path` off the caller thread. `None` if that stat was skipped (hung mount
/// or another batch still running): the caller treats the scope as missing.
fn canonicalize_within(path: &std::path::Path) -> Option<PathBuf> {
    let path = path.to_path_buf();
    let batch = timed_batch(move |tx, _cancel| {
        // Failure (wrong case, not a directory): keep the path as typed.
        let resolved = std::fs::canonicalize(&path).unwrap_or(path);
        let _ = tx.send(resolved);
    })?;
    batch.items.into_iter().next()
}

/// Drop hits whose path no longer exists. A skipped or timed-out stat counts as missing.
fn drop_missing(hits: Vec<FileHit>) -> Vec<FileHit> {
    if hits.is_empty() {
        return hits;
    }
    let paths: Vec<PathBuf> = hits.iter().map(|h| h.path.clone()).collect();
    let Some(batch) = timed_batch(move |tx, cancel| {
        for (i, path) in paths.into_iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            if tx.send((i, path.exists())).is_err() {
                return;
            }
        }
    }) else {
        return Vec::new();
    };
    let mut keep = vec![false; hits.len()];
    for (i, exists) in batch.items {
        if exists && i < keep.len() {
            keep[i] = true;
        }
    }
    hits.into_iter().enumerate().filter(|(i, _)| keep[*i]).map(|(_, hit)| hit).collect()
}

/// Paths to remove after a walk. A stat that doesn't finish in time is kept.
fn extras_to_drop(items: &[(PathBuf, bool)], started_at: i64) -> HashSet<PathBuf> {
    if items.is_empty() {
        return HashSet::new();
    }
    let paths: Vec<PathBuf> = items.iter().map(|(path, _)| path.clone()).collect();
    let Some(batch) = timed_batch(move |tx, cancel| {
        for (i, path) in paths.into_iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let keep = match std::fs::symlink_metadata(&path) {
                Ok(meta) => fresh_enough(meta.mtime(), meta.ctime(), started_at),
                Err(_) => false, // gone
            };
            if tx.send((i, keep)).is_err() {
                return;
            }
        }
    }) else {
        return HashSet::new(); // another stat is stuck: keep every entry
    };
    let mut verdict = vec![None; items.len()];
    for (i, keep) in batch.items {
        if i < verdict.len() {
            verdict[i] = Some(keep);
        }
    }
    items
        .iter()
        .enumerate()
        .filter_map(|(i, (path, _))| match verdict[i] {
            Some(false) => Some(path.clone()),
            _ => None, // fresh, or the stat never came back
        })
        .collect()
}

/// Snapshot of the current index (cheap: an Arc clone) and a search over it plus extras.
/// `scope`: only inside this folder and its subfolders (`~/Music: NF`).
pub fn search(shared: &Shared, query: &str, max_files: usize, max_dirs: usize, scope: Option<&std::path::Path>) -> (Vec<FileHit>, Vec<FileHit>) {
    // A symlinked folder (ML4W links ~/.config/hypr into ~/.mydotfiles) isn't walked
    // through; its real location is. Unresolvable (e.g. typed in the wrong case): as typed.
    // A hung canonicalize is "missing": don't search a scope we couldn't resolve.
    let scope_resolved = if let Some(scope) = scope {
        match canonicalize_within(scope) {
            Some(path) => Some(path),
            None => return (Vec::new(), Vec::new()),
        }
    } else {
        None
    };
    let scope = scope_resolved.as_deref();
    // Copy candidates under the lock. Stat only after it is dropped.
    let (index, extra) = {
        let guard = shared.lock().unwrap();
        (guard.0.clone(), guard.1.search(query, scope))
    };
    let (files, dirs) = index.map_or((Vec::new(), Vec::new()), |i| {
        let ids = scope.map(|s| i.entries_at(s));
        i.search(query, max_files, max_dirs, ids.as_deref())
    });
    let extra = drop_missing(extra);
    let (ef, ed): (Vec<_>, Vec<_>) = extra.into_iter().partition(|h| !h.is_dir);
    // Commands keep their own group; new files merge with the rest. A new file already
    // listed as a command is dropped (merge only dedups against the file list).
    let (mut commands, files): (Vec<_>, Vec<_>) = files.into_iter().partition(|h| h.command);
    let ef: Vec<_> = ef.into_iter().filter(|h| !commands.iter().any(|c| c.path == h.path)).collect();
    commands.extend(merge(files, ef, max_files));
    (commands, merge(dirs, ed, max_dirs))
}

/// Start the background walker: builds the index now and again every `every`.
pub fn start_walker(shared: Shared, every: std::time::Duration) {
    std::thread::Builder::new()
        .name("omni-walk".into())
        .spawn(move || loop {
            lower_priority();
            let (roots, ignored) = config();
            let start = std::time::Instant::now();
            let started_at = glib::real_time() / 1_000_000;
            // Live events during the walk compare against the start, not zero.
            {
                let mut guard = shared.lock().unwrap();
                guard.1.since = started_at;
                guard.1.walk_started = started_at;
            }
            let index = Index::build(&roots, &ignored);
            eprintln!("omni: indexed {} files and folders in {:.1}s", index.len(), start.elapsed().as_secs_f32());
            let ended_at = glib::real_time() / 1_000_000;
            let snapshot = {
                let guard = shared.lock().unwrap();
                guard.1.items.clone()
            };
            // Stat outside the lock. Timeout keeps the entry; a confirmed-old or missing path is dropped.
            let drop_paths = extras_to_drop(&snapshot, started_at);
            {
                let mut guard = shared.lock().unwrap();
                guard.0 = Some(Arc::new(index)); // the previous index is freed here
                guard.1.commit_walk(ended_at, &drop_paths);
            }
            // Hand the walk's temporary allocations (and the old index) back to the system.
            // SAFETY: malloc_trim only releases free heap memory.
            unsafe { libc::malloc_trim(0) };
            // Wait for the next walk; go early if too many new files piled up.
            let next = std::time::Instant::now() + every;
            while std::time::Instant::now() < next && !shared.lock().unwrap().1.overflow {
                std::thread::sleep(std::time::Duration::from_secs(10));
            }
        })
        .expect("can't start the file walker thread");
}

/// Resolve one queue of URNs. Stops at the first stat batch that times out or is skipped,
/// so a wedged path doesn't turn into a thread per path.
fn apply_live(conn: &crate::sparql::Connection, shared: &Shared, urns: &[String], since: i64) {
    let not_before = since - 2;
    for chunk in urns.chunks(500) {
        let paths = resolve_urns(conn, chunk);
        if paths.is_empty() {
            continue;
        }
        let Some(batch) = timed_batch(move |tx, cancel| {
            for path in paths {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
                // Created, written or renamed (ctime) since the cutoff: new.
                if !fresh_enough(meta.mtime(), meta.ctime(), not_before) {
                    continue;
                }
                if tx.send((path, meta.is_dir())).is_err() {
                    return;
                }
            }
        }) else {
            return; // a stat batch is already running
        };
        let finished = batch.finished;
        if !batch.items.is_empty() {
            crate::LIVE_ADDED.fetch_add(batch.items.len(), Ordering::Relaxed);
            let mut guard = shared.lock().unwrap();
            for (path, is_dir) in batch.items {
                guard.1.add(path, is_dir);
            }
        }
        if !finished {
            return; // this path wedged: skip the rest of the batch
        }
    }
}

/// The live-update worker: takes LocalSearch event URNs from a queue, resolves them in
/// batches, and adds only files that are new since the last walk. One thread, whatever the
/// event volume: a full LocalSearch re-index sends an event for every file it has, and
/// almost all of those are already in the index.
///
/// The returned sender is the unbounded channel main already uses. A forwarder moves
/// batches onto a bounded queue and drops them when that queue is full, so a worker stuck
/// in stat cannot grow memory without bound. Events that arrive before the first index
/// exists are kept and applied once it does.
pub fn start_live_worker(conn: Arc<crate::sparql::Connection>, shared: Shared) -> mpsc::Sender<Vec<String>> {
    let (tx, rx) = mpsc::channel::<Vec<String>>();
    let (bound_tx, bound_rx) = mpsc::sync_channel::<Vec<String>>(64);
    std::thread::Builder::new()
        .name("omni-live-q".into())
        .spawn(move || {
            // Drain even when the worker is behind. try_send on Full drops that batch;
            // the next recv takes whatever main has already queued.
            while let Ok(batch) = rx.recv() {
                match bound_tx.try_send(batch) {
                    Ok(()) => {}
                    Err(mpsc::TrySendError::Full(_)) => {}
                    Err(mpsc::TrySendError::Disconnected(_)) => return,
                }
            }
        })
        .expect("can't start the live-update forwarder");
    std::thread::Builder::new()
        .name("omni-live".into())
        .spawn(move || {
            lower_priority();
            let mut queued: Vec<String> = Vec::new();
            // The first batch applied after the index appears uses walk_started, not `since`
            // (that has already moved to the end of the walk). Later batches use `since`.
            let mut caught_up = false;
            loop {
                let mut disconnected = false;
                if queued.is_empty() {
                    match bound_rx.recv() {
                        Ok(batch) => queued.extend(batch),
                        Err(_) => break,
                    }
                } else {
                    // Waiting for the first index: block, don't spin. 250ms is just the
                    // wake-up if no further event arrives after the index is published.
                    match bound_rx.recv_timeout(Duration::from_millis(250)) {
                        Ok(batch) => queued.extend(batch),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => disconnected = true,
                    }
                }
                loop {
                    match bound_rx.try_recv() {
                        Ok(more) => queued.extend(more),
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
                if !queued.is_empty() {
                    let (ready, since, started) = {
                        let guard = shared.lock().unwrap();
                        (guard.0.is_some(), guard.1.since, guard.1.walk_started)
                    };
                    // No index yet: keep the URNs and wait. recv_timeout above is the wait.
                    if ready && (since != 0 || started != 0) {
                        let cutoff = if !caught_up {
                            caught_up = true;
                            if started != 0 { started } else { since }
                        } else {
                            since
                        };
                        if cutoff != 0 {
                            let urns = std::mem::take(&mut queued);
                            apply_live(&conn, &shared, &urns, cutoff);
                        }
                    }
                }
                if disconnected {
                    break;
                }
            }
        })
        .expect("can't start the live-update thread");
    tx
}

/// Run this thread at nice 10 so walking the disk never competes with the desktop.
fn lower_priority() {
    // SAFETY: on Linux, PRIO_PROCESS with who=0 affects only the calling thread.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) };
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    gio::File::for_uri(uri).path()
}

/// One LocalSearch FTS hit: path plus optional `fts:snippet` text.
#[derive(Clone, Debug)]
pub struct FtsHit {
    pub path: PathBuf,
    pub snippet: Option<String>,
}

/// One whitespace-separated unit of a file/contents query. Double-quoted spans are phrases.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryToken {
    Word(String),
    Phrase(String),
}

impl QueryToken {
    /// Lowercase bytes to require in a file name (phrase = contiguous substring).
    fn name_needle(&self) -> Vec<u8> {
        match self {
            Self::Word(s) | Self::Phrase(s) => s.to_ascii_lowercase().into_bytes(),
        }
    }
}

/// Split `query` on whitespace; keep `"quoted phrases"` as one token. Same tokens feed
/// filename matching and LocalSearch `fts:match`.
pub fn parse_query_tokens(query: &str) -> Vec<QueryToken> {
    let query = bounded_query(query);
    let mut tokens = Vec::new();
    let mut rest = query;
    while !rest.is_empty() {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            break;
        }
        rest = trimmed;
        if let Some(after_quote) = rest.strip_prefix('"') {
            if let Some(end) = after_quote.find('"') {
                let phrase = &after_quote[..end];
                if !phrase.is_empty() {
                    tokens.push(QueryToken::Phrase(phrase.to_string()));
                }
                rest = &after_quote[end + 1..];
            } else {
                // Unclosed quote: treat the remainder as a phrase.
                if !after_quote.is_empty() {
                    tokens.push(QueryToken::Phrase(after_quote.to_string()));
                }
                break;
            }
        } else {
            let end = rest.find(|c: char| c.is_whitespace() || c == '"').unwrap_or(rest.len());
            let word = &rest[..end];
            if !word.is_empty() {
                tokens.push(QueryToken::Word(word.to_string()));
            }
            rest = &rest[end..];
        }
    }
    tokens
}

/// Escape one FTS atom so `AND`/`OR`/`NOT`/`NEAR` and FTS punctuation are literal.
fn escape_fts_atom(text: &str) -> String {
    let upper = text.to_ascii_uppercase();
    let needs_quote = matches!(upper.as_str(), "AND" | "OR" | "NOT" | "NEAR")
        || text.chars().any(|c| !(c.is_ascii_alphanumeric() || c == '_'));
    if needs_quote {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

/// LocalSearch FTS expression: every token AND-ed (implicit), `*` only on the last bare word.
pub fn fts_match_expression(tokens: &[QueryToken]) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    let last = tokens.len() - 1;
    let parts: Vec<String> = tokens
        .iter()
        .enumerate()
        .map(|(i, tok)| match tok {
            QueryToken::Phrase(p) => format!("\"{}\"", p.replace('"', "\"\"")),
            QueryToken::Word(w) => {
                let atom = escape_fts_atom(w);
                if i == last && !atom.starts_with('"') {
                    format!("{atom}*")
                } else {
                    atom
                }
            }
        })
        .collect();
    Some(parts.join(" "))
}

/// `file://` URI for a directory scope, with a trailing `/` for STRSTARTS matching.
/// Resolves symlinks when possible so the URI matches LocalSearch's indexed paths.
pub fn scope_file_uri(dir: &std::path::Path) -> String {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut uri = gio::File::for_path(&dir).uri().to_string();
    if !uri.ends_with('/') {
        uri.push('/');
    }
    uri
}

/// Build the LocalSearch FTS SPARQL for `query` (content via `fts:match`, not filename).
/// Optional `scope` restricts to files under that directory (recursive STRSTARTS on nie:url).
/// Paths only (no `fts:snippet`): snippets are filled lazily for visible rows.
pub fn content_search_sparql(query: &str, limit: usize, scope: Option<&std::path::Path>) -> Option<String> {
    let tokens = parse_query_tokens(query);
    let fts = fts_match_expression(&tokens)?;
    let fts = crate::sparql::escape(&fts);
    let scope_filter = scope
        .map(|dir| {
            let uri = crate::sparql::escape(&scope_file_uri(dir));
            format!(" FILTER(STRSTARTS(?u, \"{uri}\"))")
        })
        .unwrap_or_default();
    Some(format!(
        "SELECT DISTINCT ?u WHERE {{ \
         ?d fts:match \"{fts}\" ; nie:isStoredAs ?f . ?f nie:url ?u .{scope_filter} \
         }} ORDER BY DESC(fts:rank(?d)) LIMIT {limit}"
    ))
}

/// Files whose *contents* match `query` (LocalSearch full-text index), up to `limit`.
#[allow(dead_code)]
pub fn content_search(
    conn: &crate::sparql::Connection,
    query: &str,
    limit: usize,
    scope: Option<&std::path::Path>,
) -> Vec<PathBuf> {
    content_search_localsearch(conn, query, limit, scope)
        .0
        .into_iter()
        .map(|h| h.path)
        .collect()
}

/// LocalSearch only (no ripgrep). Returns `(hits, sparql, error)`.
pub fn content_search_localsearch(
    conn: &crate::sparql::Connection,
    query: &str,
    limit: usize,
    scope: Option<&std::path::Path>,
) -> (Vec<FtsHit>, String, Option<String>) {
    let Some(sparql) = content_search_sparql(query, limit, scope) else {
        return (Vec::new(), String::new(), None);
    };
    match conn.query(&sparql) {
        Ok(rows) => {
            let hits = rows
                .iter()
                .filter_map(|r| {
                    let path = uri_to_path(r.first()?)?;
                    if !path.exists() {
                        return None;
                    }
                    Some(FtsHit {
                        path,
                        snippet: None,
                    })
                })
                .collect();
            (hits, sparql, None)
        }
        Err(e) => {
            eprintln!("omni: content search failed: {e}");
            (Vec::new(), sparql, Some(e.to_string()))
        }
    }
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Roots to scan with ripgrep: the scope folder, or configured trees when unscoped
/// (`~/Documents/github`, `~/Projects`, `~/Documents` — never `$HOME`).
fn content_rg_roots(scope: Option<&std::path::Path>) -> Vec<PathBuf> {
    if let Some(dir) = scope {
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        return if dir.is_dir() { vec![dir] } else { Vec::new() };
    }
    let home = glib::home_dir();
    let mut roots = Vec::new();
    for rel in ["Documents/github", "Projects", "Documents"] {
        let p = home.join(rel);
        if p.is_dir() {
            roots.push(p);
        }
    }
    roots
}

/// Whether `query` tokens need regex (prefix on last word, or special chars). Else `-F`.
fn rg_use_fixed_strings(tokens: &[QueryToken]) -> bool {
    if tokens.is_empty() {
        return true;
    }
    let last = tokens.len() - 1;
    for (i, tok) in tokens.iter().enumerate() {
        match tok {
            QueryToken::Phrase(_) => {}
            QueryToken::Word(w) if i == last => return false, // prefix `\w*`
            QueryToken::Word(w) => {
                if w.chars().any(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-') {
                    return false;
                }
            }
        }
    }
    true
}

fn rg_binary() -> Option<&'static str> {
    static RG: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *RG.get_or_init(|| {
        for cand in ["/usr/bin/rg", "rg"] {
            let ok = std::process::Command::new(cand)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                return Some(cand);
            }
        }
        None
    })
}

/// Escape a literal for ripgrep's default regex mode (prefix `term*`).
fn rg_regex_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if matches!(c, '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Blocking collect helper (tests). Prefer [`content_search_rg_stream`] in production.
pub fn content_search_rg(query: &str, limit: usize, scope: Option<&std::path::Path>) -> Vec<FtsHit> {
    let cancel = AtomicBool::new(false);
    let slot = Mutex::new(None);
    let mut hits = Vec::new();
    content_search_rg_stream(query, limit, scope, &cancel, &slot, |hit| {
        hits.push(hit);
    });
    hits
}

/// Stream ripgrep hits via `on_hit`. Kills the process at `limit` or when `cancel` is set.
/// No snippets here — callers fill those lazily for visible rows.
pub fn content_search_rg_stream(
    query: &str,
    limit: usize,
    scope: Option<&std::path::Path>,
    cancel: &AtomicBool,
    child_slot: &Mutex<Option<std::process::Child>>,
    mut on_hit: impl FnMut(FtsHit),
) -> usize {
    if limit == 0 || cancel.load(Ordering::Relaxed) {
        return 0;
    }
    let tokens = parse_query_tokens(query);
    if tokens.is_empty() {
        return 0;
    }
    let roots = content_rg_roots(scope);
    if roots.is_empty() {
        return 0;
    }
    let Some(rg) = rg_binary() else {
        return 0;
    };

    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    let mut cmd = std::process::Command::new(rg);
    cmd.args([
        "--files-with-matches",
        "--max-count",
        "1",
        "--max-filesize",
        "1M",
        "--max-columns",
        "300",
        "--smart-case",
        "--no-messages",
        "--color=never",
        "--threads",
        &threads.to_string(),
    ]);
    // Hidden dirs skipped by default (node_modules/.git via gitignore + these globs).
    // Explicit scope may be a hidden folder — still search it as the root.
    if scope.is_none() {
        cmd.args(["--glob", "!.*/**"]);
    }
    cmd.args([
        "--glob",
        "!.git/**",
        "--glob",
        "!**/node_modules/**",
        "--glob",
        "!**/target/**",
        "--glob",
        "!**/.venv/**",
        "--glob",
        "!**/.cache/**",
    ]);
    if tokens.len() > 1 {
        cmd.arg("--all-match");
    }
    let fixed = rg_use_fixed_strings(&tokens);
    if fixed {
        cmd.arg("--fixed-strings");
    }
    let last = tokens.len() - 1;
    for (i, tok) in tokens.iter().enumerate() {
        let pat = match tok {
            QueryToken::Phrase(p) if fixed => p.clone(),
            QueryToken::Phrase(p) => rg_regex_literal(p),
            QueryToken::Word(w) if fixed => w.clone(),
            QueryToken::Word(w) if i == last => format!("{}\\w*", rg_regex_literal(w)),
            QueryToken::Word(w) => rg_regex_literal(w),
        };
        cmd.arg("-e");
        cmd.arg(pat);
    }
    for root in &roots {
        cmd.arg(root);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("omni: ripgrep content search failed: {e}");
            return 0;
        }
    };
    let stdout = child.stdout.take();
    {
        let mut slot = child_slot.lock().unwrap();
        if let Some(mut old) = slot.take() {
            let _ = old.kill();
            let _ = old.wait();
        }
        *slot = Some(child);
    }
    let Some(stdout) = stdout else {
        return 0;
    };
    use std::io::{BufRead, BufReader};
    let reader = BufReader::new(stdout);
    let mut count = 0usize;
    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(line) = line else { break };
        let path = PathBuf::from(line.trim());
        if !path.is_file() {
            continue;
        }
        on_hit(FtsHit {
            path,
            snippet: None,
        });
        count += 1;
        if count >= limit {
            break;
        }
    }
    // Cap reached or EOF: kill so a rare-term scan of a huge tree doesn't keep running.
    if let Some(mut child) = child_slot.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    count
}

/// Read a short snippet for a content hit (lazy, off the search hot path). Caps at 1 MB.
pub fn content_snippet_for(path: &std::path::Path, query: &str) -> Option<String> {
    use std::io::Read;
    let needles: Vec<String> = parse_query_tokens(query)
        .into_iter()
        .map(|t| match t {
            QueryToken::Word(s) | QueryToken::Phrase(s) => s.to_ascii_lowercase(),
        })
        .collect();
    if needles.is_empty() {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 1024 * 1024];
    let n = file.read(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf[..n]);
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if needles.iter().any(|n| lower.contains(n.as_str())) {
            let trimmed = collapse_ws(line.trim());
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.chars().count() > 120 {
                return Some(trimmed.chars().take(120).collect::<String>() + "\u{2026}");
            }
            return Some(trimmed);
        }
    }
    None
}

/// Paths for LocalSearch resource URNs (from its change events).
pub fn resolve_urns(conn: &crate::sparql::Connection, urns: &[String]) -> Vec<PathBuf> {
    let values: Vec<String> = urns
        .iter()
        .filter(|u| !u.contains(['<', '>', '"', ' ', '{', '}', '\\']))
        .map(|u| format!("<{u}>"))
        .collect();
    if values.is_empty() {
        return Vec::new();
    }
    let sparql = format!(
        "SELECT DISTINCT ?u WHERE {{ VALUES ?r {{ {} }} {{ ?r nie:url ?u }} UNION {{ ?r nie:isStoredAs ?f . ?f nie:url ?u }} }}",
        values.join(" ")
    );
    conn.query(&sparql)
        .map(|rows| rows.iter().filter_map(|r| uri_to_path(r.first()?)).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_sparql_uses_fts_match_not_filename() {
        let q = content_search_sparql("omnicontentprobeQUUX42XYZ", 4, None).expect("terms");
        assert!(q.contains("fts:match"), "{q}");
        assert!(!q.contains("fts:snippet"), "{q}");
        assert!(q.contains("omnicontentprobeQUUX42XYZ*"), "{q}");
        assert!(q.contains("ORDER BY DESC(fts:rank(?d))"), "{q}");
        assert!(!q.contains("nfo:fileName"), "{q}");
        assert!(!q.contains("STRSTARTS"), "{q}");
        // Multi-word: AND all terms; prefix * only on the last bare word.
        let multi = content_search_sparql("hello world", 4, None).unwrap();
        assert!(multi.contains("hello world*"), "{multi}");
        assert!(!multi.contains("hello*"), "{multi}");
        let phrase = content_search_sparql("\"crystal clear water\"", 4, None).unwrap();
        assert!(phrase.contains("crystal clear water"), "{phrase}");
        assert!(!phrase.contains("water*"), "{phrase}");
    }

    #[test]
    fn content_sparql_scope_uses_strstarts_on_file_uri() {
        let dir = PathBuf::from("/home/someone/Documents");
        let q = content_search_sparql("needle", 4, Some(&dir)).unwrap();
        assert!(q.contains("STRSTARTS(?u,"), "{q}");
        assert!(q.contains("file:///home/someone/Documents/"), "{q}");
        assert!(q.contains("fts:match"), "{q}");
        assert!(q.contains("ORDER BY DESC(fts:rank(?d))"), "{q}");
    }

    #[test]
    fn content_rg_finds_file_under_git_scope() {
        if rg_binary().is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("omni-rg-{}-git", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        struct Rm(PathBuf);
        impl Drop for Rm {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _rm = Rm(dir.clone());
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let path = dir.join("note.txt");
        std::fs::write(&path, b"unique zebrafluxcapacitor token here\n").unwrap();
        let hits = content_search_rg("zebrafluxcapacitor", 4, Some(&dir));
        assert!(
            hits.iter().any(|h| h.path == path),
            "rg must find content inside a .git tree LocalSearch skips: {hits:?}"
        );
        let miss = content_search_rg("zebrafluxcapacitor nowhere", 4, Some(&dir));
        assert!(miss.is_empty(), "{miss:?}");
    }

    #[test]
    fn query_tokens_split_whitespace_and_phrases() {
        assert_eq!(
            parse_query_tokens(r#"alpha beta"#),
            vec![QueryToken::Word("alpha".into()), QueryToken::Word("beta".into())]
        );
        assert_eq!(
            parse_query_tokens(r#""crystal clear water""#),
            vec![QueryToken::Phrase("crystal clear water".into())]
        );
        assert_eq!(
            parse_query_tokens(r#"alpha "crystal clear" water"#),
            vec![
                QueryToken::Word("alpha".into()),
                QueryToken::Phrase("crystal clear".into()),
                QueryToken::Word("water".into()),
            ]
        );
        assert_eq!(fts_match_expression(&parse_query_tokens("alpha beta")).as_deref(), Some("alpha beta*"));
        assert_eq!(
            fts_match_expression(&parse_query_tokens(r#""crystal clear water""#)).as_deref(),
            Some(r#""crystal clear water""#)
        );
    }

    #[test]
    fn files_query_is_capped_at_eight_words_and_256_bytes() {
        let nine = "one two three four five six seven eight nine";
        assert_eq!(bounded_query(nine), "one two three four five six seven eight");
        assert_eq!(bounded_query("only three words"), "only three words");
        assert_eq!(bounded_query(&"a".repeat(300)).len(), 256);
        // 255 ASCII bytes then a 2-byte char must not slice the codepoint.
        let mut border = "b".repeat(255);
        border.push('é');
        let capped = bounded_query(&border);
        assert_eq!(capped.len(), 255);
        assert!(border.is_char_boundary(capped.len()));
        let _ = bounded_query(&"é".repeat(200));
    }

    #[test]
    fn files_fresh_mtime_or_ctime() {
        assert!(fresh_enough(10, 1, 10));
        assert!(fresh_enough(1, 10, 10));
        assert!(!fresh_enough(9, 9, 10));
    }

    #[test]
    fn files_dir_identity_skips_a_repeat() {
        let mut seen = HashSet::new();
        assert!(claim_dir(&mut seen, Some((1, 2))));
        assert!(!claim_dir(&mut seen, Some((1, 2))));
        assert!(claim_dir(&mut seen, Some((1, 3))));
        assert!(claim_dir(&mut seen, None));
    }

    #[test]
    fn files_timed_stat_returns_a_fast_result() {
        let batch = timed_batch(|tx, _cancel| {
            let _ = tx.send(7);
        })
        .expect("gate was busy");
        assert!(batch.finished);
        assert_eq!(batch.items, vec![7]);
    }

    #[test]
    fn files_extras_match_extension_without_statting() {
        let mut extras = Extras::default();
        let path = PathBuf::from("/no/such/readme.md");
        extras.seen.insert(path.clone());
        extras.items.push((path.clone(), false));
        let hits = extras.search("md", None);
        assert_eq!(hits.len(), 1, "a missing path must still rank; existence is checked later");
        assert_eq!(hits[0].path, path);
        // Ninth word is not in the name. Capping to eight words lets the rest match.
        let mut named = Extras::default();
        let hello = PathBuf::from("/tmp/helloa");
        named.seen.insert(hello.clone());
        named.items.push((hello, false));
        let q = "hello a a a a a a a missing";
        assert_eq!(named.search(q, None).len(), 1);
    }

    #[test]
    fn files_commands_get_their_own_group() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("omni-files-{}-cmd", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        struct Rm(PathBuf);
        impl Drop for Rm {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _rm = Rm(dir.clone());
        let (bin, other_bin, docs) = (dir.join("bin"), dir.join("other/bin"), dir.join("docs"));
        for d in [&bin, &other_bin, &docs] {
            std::fs::create_dir_all(d).unwrap();
        }
        let exe = |path: PathBuf| {
            std::fs::write(&path, b"#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        // Plenty of shallower same-named files that outrank the commands as files.
        for name in ["tool.png", "tool.py", "tool.h", "tool.txt", "tool.md", "tool.rs"] {
            std::fs::write(dir.join(name), b"a").unwrap();
        }
        std::fs::write(docs.join("tool"), b"a").unwrap();
        for name in ["tool", "tool3.9", "tool3.10", "tool3.15"] {
            exe(bin.join(name));
        }
        exe(other_bin.join("tool"));
        std::fs::write(bin.join("tool-readme"), b"not runnable").unwrap();
        std::os::unix::fs::symlink(&bin, dir.join("bin-link")).unwrap();

        // other/bin comes first in $PATH, as the shell would search it; bin twice via a link.
        let path_var = std::env::join_paths([&other_bin, &dir.join("bin-link"), &bin]).unwrap();
        let index = Index::build_with_path(std::slice::from_ref(&dir), &HashSet::new(), Some(&path_var));
        let (files, _) = index.search("tool", 5, 3, None);
        let commands: Vec<_> = files.iter().filter(|h| h.command).map(|h| h.path.clone()).collect();
        // The shell's own "tool" first, then the newest versions; capped at MAX_COMMANDS.
        assert_eq!(commands, vec![other_bin.join("tool"), bin.join("tool"), bin.join("tool3.15")]);
        // Commands come first and don't take the files' slots.
        assert!(files[..3].iter().all(|h| h.command));
        assert_eq!(files.iter().filter(|h| !h.command).count(), 5);
        assert!(!files.iter().any(|h| h.path == bin.join("tool-readme") && h.command));
        // Once the command group is full, a non-runnable file in a $PATH folder still takes
        // a file slot instead of being dropped.
        let (wide, _) = index.search("tool", 20, 3, None);
        assert!(
            wide.iter().any(|h| h.path == bin.join("tool-readme") && !h.command),
            "tool-readme must be listed as a file"
        );

        // Without $PATH nothing is a command.
        let plain = Index::build_with_path(std::slice::from_ref(&dir), &HashSet::new(), None);
        assert!(plain.search("tool", 5, 3, None).0.iter().all(|h| !h.command));
    }

    #[test]
    fn natural_order_compares_numbers_by_value() {
        use std::cmp::Ordering::*;
        assert_eq!(natural_cmp(b"python3.9", b"python3.15"), Less);
        assert_eq!(natural_cmp(b"python3.10", b"python3.9"), Greater);
        assert_eq!(natural_cmp(b"file007", b"file7"), Equal);
        assert_eq!(natural_cmp(b"a", b"b"), Less);
        assert_eq!(natural_cmp(b"python3", b"python3.15"), Less);
    }

    #[test]
    fn files_short_query_matches_extension_not_infix() {
        let dir = std::env::temp_dir().join(format!("omni-files-{}-ext", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        struct Rm(PathBuf);
        impl Drop for Rm {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _rm = Rm(dir.clone());
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("ReadMe.MD"), b"a").unwrap();
        std::fs::write(dir.join("md"), b"a").unwrap();
        std::fs::write(dir.join("ba"), b"a").unwrap();
        std::fs::write(dir.join("xxay"), b"a").unwrap();
        std::fs::write(dir.join("a"), b"a").unwrap();
        std::fs::write(dir.join("x.a"), b"a").unwrap();
        std::fs::write(dir.join("helloa"), b"a").unwrap();
        std::fs::write(dir.join("keep.txt"), b"a").unwrap();
        let gone = dir.join("gone.txt");
        std::fs::write(&gone, b"a").unwrap();
        std::fs::write(dir.join("sub").join("note.txt"), b"a").unwrap();

        let index = Index::build(std::slice::from_ref(&dir), &HashSet::new());
        std::fs::remove_file(&gone).unwrap();

        let names = |q: &str| {
            let (files, dirs) = index.search(q, 50, 50, None);
            let mut got: Vec<String> = files
                .into_iter()
                .chain(dirs)
                .map(|h| h.path.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            got.sort();
            got
        };

        let md = names("md");
        assert!(md.iter().any(|n| n == "ReadMe.MD"), "extension: {md:?}");
        assert!(md.iter().any(|n| n == "md"), "prefix: {md:?}");
        assert!(!md.iter().any(|n| n == "xxay"), "{md:?}");

        let a = names("a");
        assert!(a.iter().any(|n| n == "a"), "{a:?}");
        assert!(a.iter().any(|n| n == "x.a"), "{a:?}");
        assert!(!a.iter().any(|n| n == "ba"), "trailing letter is not an extension: {a:?}");
        assert!(!a.iter().any(|n| n == "xxay"), "infix must not match: {a:?}");
        assert!(!a.iter().any(|n| n == "helloa"), "infix must not match: {a:?}");
        assert!(!a.iter().any(|n| n == "ReadMe.MD"), "infix must not match: {a:?}");

        let e = names("e");
        assert!(
            !e.iter().any(|n| n == "helloa" || n == "ReadMe.MD" || n == "note.txt"),
            "infix must not match: {e:?}"
        );

        let txt = names("txt");
        assert!(txt.iter().any(|n| n == "note.txt"), "nested dir: {txt:?}");
        assert!(txt.iter().any(|n| n == "keep.txt"), "{txt:?}");
        assert!(!txt.iter().any(|n| n == "gone.txt"), "deleted: {txt:?}");

        let hello = names("hello a a a a a a a missing");
        assert!(hello.iter().any(|n| n == "helloa"), "ninth word must be ignored: {hello:?}");

        let huge = "word ".repeat(80);
        let _ = index.search(&huge, 5, 3, None);
        let mut border = "x".repeat(255);
        border.push('é');
        let _ = index.search(&border, 5, 3, None);
        let _ = index.search(&"é".repeat(200), 5, 3, None);
    }
}
