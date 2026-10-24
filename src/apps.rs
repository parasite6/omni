//! Installed apps: loading, GNOME-style matching, launch counts.
//!
//! Matching is by word prefix and substring (like GNOME Shell): "brave" should find Brave,
//! not Proton Mail. Only when that finds no app by name does loose matching take over
//! (misspellings, missing or swapped letters: "frfox", "ptormaster").

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

pub struct App {
    pub info: gio::AppInfo,
    pub name: String,
    name_lc: String,
    name_words: Vec<String>,
    /// The name lowercased with spaces and punctuation removed ("visualstudiocode").
    name_compact: String,
    /// Generic name, keywords and executable, matched at lower weight than the name.
    extra_words: Vec<String>,
    /// "New window", "New private window", ... (declared by the app, plus TWEAKS).
    pub actions: Vec<AppAction>,
}

#[derive(Clone)]
pub struct AppAction {
    pub name: String,
    pub launch: Launch,
    name_words: Vec<String>,
    /// Other words that find it: the launcher file's own name for a renamed action.
    keywords: Vec<String>,
}

#[derive(Clone)]
pub enum Launch {
    /// An action declared in the app's .desktop file, by id.
    Desktop(String),
    /// The app's executable with these arguments.
    Args(&'static [&'static str]),
}

/// Fix-ups for apps whose launcher file doesn't match the app's own menu: rename declared
/// actions (by id), add ones it leaves out. Matched by .desktop id, not executable: a
/// browser's installed web apps (GitHub, Proton Mail, ...) run the same executable.
struct Tweak {
    desktop_ids: &'static [&'static str],
    rename: &'static [(&'static str, &'static str)],
    /// (name, arguments, extra search words)
    add: &'static [(&'static str, &'static [&'static str], &'static str)],
}

const TWEAKS: &[Tweak] = &[Tweak {
    desktop_ids: &["brave-browser.desktop", "com.brave.Browser.desktop"],
    rename: &[("new-window", "New window"), ("new-private-window", "New private window")],
    add: &[("New private window with Tor", &["--tor"], "onion incognito")],
}];

fn load_actions(info: &gio::AppInfo) -> Vec<AppAction> {
    let id = info.id().map(|i| i.to_string()).unwrap_or_default();
    let tweak = TWEAKS.iter().find(|t| t.desktop_ids.contains(&id.as_str()));
    let mut actions = Vec::new();
    if let Some(d) = info.downcast_ref::<gio_unix::DesktopAppInfo>() {
        for id in d.list_actions() {
            let declared = d.action_name(&id).to_string();
            let renamed = tweak.and_then(|t| t.rename.iter().find(|(r, _)| *r == id.as_str())).map(|(_, n)| n.to_string());
            let keywords = if renamed.is_some() { words(&declared) } else { Vec::new() };
            let name = renamed.unwrap_or(declared);
            actions.push(AppAction { name_words: words(&name), keywords, name, launch: Launch::Desktop(id.to_string()) });
        }
    }
    for (name, args, extra) in tweak.map_or(&[][..], |t| t.add) {
        actions.push(AppAction {
            name: name.to_string(),
            name_words: words(name),
            keywords: words(extra),
            launch: Launch::Args(args),
        });
    }
    actions
}

/// Start an app action (closes nothing; the caller hides the popup first).
pub fn launch_action(info: &gio::AppInfo, action: &AppAction, ctx: &gio::AppLaunchContext) -> Result<(), glib::Error> {
    match &action.launch {
        Launch::Desktop(id) => {
            if let Some(d) = info.downcast_ref::<gio_unix::DesktopAppInfo>() {
                d.launch_action(id, Some(ctx));
            }
            Ok(())
        }
        Launch::Args(args) => {
            let mut command = glib::shell_quote(info.executable()).to_string_lossy().to_string();
            for arg in *args {
                command.push(' ');
                command.push_str(&glib::shell_quote(arg).to_string_lossy());
            }
            gio::AppInfo::create_from_commandline(
                &command,
                Some(&action.name),
                gio::AppInfoCreateFlags::SUPPORTS_STARTUP_NOTIFICATION,
            )?
            .launch(&[], Some(ctx))
        }
    }
}

pub struct Apps {
    apps: Vec<App>,
    usage: HashMap<String, Usage>,
    /// The default web browser's .desktop id: its actions win ties ("private window").
    default_browser: Option<String>,
}

/// Bonus for the default browser's actions: wins a tie, never beats a better-matched word.
const DEFAULT_BROWSER_POINTS: u32 = 30;

/// Launches from omni: how many, and when last (unix seconds, 0 = unknown).
#[derive(Default, Clone, Copy)]
struct Usage {
    count: u32,
    last: i64,
}

/// Bonus points per past launch, capped so frequent apps can't beat a clearly better match.
const USAGE_POINTS: u32 = 3;
const USAGE_CAP: u32 = 20;

fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

impl Apps {
    pub fn len(&self) -> usize {
        self.apps.len()
    }

    pub fn load() -> Self {
        let mut apps = Self { apps: Vec::new(), usage: load_usage(), default_browser: None };
        apps.reload();
        apps
    }

    /// Re-read the installed app list (called when apps are installed/removed).
    pub fn reload(&mut self) {
        self.default_browser = gio::AppInfo::default_for_uri_scheme("https").and_then(|a| a.id()).map(|id| id.to_string());
        let mut seen = HashSet::new();
        self.apps = gio::AppInfo::all()
            .into_iter()
            .filter(|a| a.should_show())
            // Same name from two .desktop files (e.g. a local override): keep the first, the user's.
            .filter(|a| seen.insert(a.display_name().to_lowercase()))
            .map(|info| {
                let mut extra = String::new();
                if let Some(d) = info.downcast_ref::<gio_unix::DesktopAppInfo>() {
                    if let Some(g) = d.generic_name() {
                        extra.push_str(&g);
                    }
                    for k in d.keywords() {
                        extra.push(' ');
                        extra.push_str(&k);
                    }
                }
                if let Some(exe) = info.executable().file_name() {
                    extra.push(' ');
                    extra.push_str(&exe.to_string_lossy());
                }
                let name = info.display_name().to_string();
                App {
                    name_lc: name.to_lowercase(),
                    name_compact: name.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect(),
                    name_words: words(&name),
                    extra_words: words(&extra),
                    actions: load_actions(&info),
                    name,
                    info,
                }
            })
            .collect();
    }

    /// Score one app against the query words; None if any word doesn't match.
    /// `loose`: a word that matches nothing exactly may instead be loosely similar to a
    /// word of the app's name or keywords (see `similarity`), scored by how close it is.
    fn score(app: &App, query_words: &[String], loose: bool) -> Option<u32> {
        let mut total = 0;
        for (i, w) in query_words.iter().enumerate() {
            if let Some(s) = Self::word_score(app, i, w) {
                total += s;
                continue;
            }
            if !loose {
                return None;
            }
            let best = |words: &mut dyn Iterator<Item = &str>| words.filter_map(|n| similarity(w, n)).fold(0.0f32, f32::max);
            let name = best(&mut app.name_words.iter().map(String::as_str).chain(std::iter::once(app.name_compact.as_str())));
            let extra = best(&mut app.extra_words.iter().map(String::as_str));
            // Weak resemblances are noise: a name word must be fairly close, a keyword very close.
            let name = if name >= 0.6 { name * 50.0 } else { 0.0 };
            let extra = if extra >= 0.8 { extra * 15.0 } else { 0.0 };
            let s = name.max(extra) as u32;
            if s == 0 {
                return None;
            }
            total += s;
        }
        Some(total)
    }

    /// How well one query word (the `i`th) matches the app, exactly; None if it doesn't.
    fn word_score(app: &App, i: usize, w: &str) -> Option<u32> {
        {
            let s = if i == 0 && app.name_lc.starts_with(w) {
                100
            } else if app.name_words.iter().any(|n| n.starts_with(w)) {
                70
            } else if app.name_lc.contains(w) {
                40
            } else if app.extra_words.iter().any(|e| e.starts_with(w)) {
                25
            } else if app.extra_words.iter().any(|e| e.contains(w)) {
                10
            } else {
                return None;
            };
            Some(s)
        }
    }

    /// Best matching apps for `query`, highest score first. The bool: matched by its name
    /// (every word starts a word of the name), not just a keyword or a loose guess; such apps
    /// rank above app actions that the query names only by the action's own words.
    pub fn search(&mut self, query: &str, limit: usize) -> Vec<(&App, u32, bool)> {
        let query_words = words(query);
        if query_words.is_empty() {
            return Vec::new();
        }
        // (index, score with launch bonus, score without it)
        let matches = |loose: bool| -> Vec<(usize, u32, u32)> {
            self.apps
                .iter()
                .enumerate()
                .filter_map(|(i, app)| {
                    let base = Self::score(app, &query_words, loose)?;
                    let uses = self.usage.get(app.info.id().as_deref().unwrap_or("")).map_or(0, |u| u.count);
                    Some((i, base + uses.min(USAGE_CAP) * USAGE_POINTS, base))
                })
                .collect()
        };
        // Loose matching only when nothing matches by name as typed (only keyword hits or
        // nothing at all), so it never adds noise to a normal search.
        let mut hits = matches(false);
        let by_name = |h: &(usize, u32, u32)| h.2 >= 40 * query_words.len() as u32;
        if !hits.iter().any(by_name) {
            hits = matches(true);
            // Keep the close guesses: drop ones far weaker than the best.
            let best = hits.iter().map(|h| h.2).max().unwrap_or(0);
            hits.retain(|h| h.2 * 2 >= best);
        }
        let strong = 70 * query_words.len() as u32;
        let mut hits: Vec<(usize, u32, bool)> = hits.into_iter().map(|(i, s, base)| (i, s, base >= strong)).collect();

        // Higher score first; on ties the shorter name is the closer match.
        hits.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then(self.apps[a.0].name.len().cmp(&self.apps[b.0].name.len()))
                .then(self.apps[a.0].name_lc.cmp(&self.apps[b.0].name_lc))
        });
        hits.truncate(limit);
        hits.into_iter().map(|(i, s, by_name)| (&self.apps[i], s, by_name)).collect()
    }

    /// App actions the query names (see action_score), best first. The bool: the query also
    /// names the action's app ("claude chat"), so it's wanted over other apps; a match on the
    /// action's own words alone ("chat") ranks below the apps.
    pub fn search_actions(&self, query: &str, limit: usize) -> Vec<(&App, &AppAction, bool)> {
        let query_words = words(query);
        if query_words.is_empty() {
            return Vec::new();
        }
        let mut hits: Vec<(&App, &AppAction, u32, bool)> = Vec::new();
        for app in &self.apps {
            let app_words: Vec<String> = app.name_words.iter().chain(&app.extra_words).cloned().collect();
            let id = app.info.id().map(|i| i.to_string()).unwrap_or_default();
            let uses = self.usage.get(&id).map_or(0, |u| u.count);
            let mut bonus = uses.min(USAGE_CAP) * USAGE_POINTS;
            if self.default_browser.as_deref() == Some(id.as_str()) {
                bonus += DEFAULT_BROWSER_POINTS;
            }
            for action in &app.actions {
                if let Some((s, names_app)) = action_score(&app_words, &action.name_words, &action.keywords, &query_words) {
                    hits.push((app, action, s + bonus, names_app));
                }
            }
        }
        hits.sort_by(|a, b| b.2.cmp(&a.2).then(a.1.name.len().cmp(&b.1.name.len())));
        hits.truncate(limit);
        hits.into_iter().map(|(app, action, _, names_app)| (app, action, names_app)).collect()
    }

    /// The actions of an installed app (for its right-click menu).
    pub fn actions_for(&self, info: &gio::AppInfo) -> Vec<AppAction> {
        self.apps.iter().find(|a| a.info.equal(info)).map(|a| a.actions.clone()).unwrap_or_default()
    }

    pub fn record_launch(&mut self, app: &gio::AppInfo) {
        let Some(id) = app.id() else { return };
        let u = self.usage.entry(id.to_string()).or_default();
        u.count += 1;
        u.last = glib::real_time() / 1_000_000;
        save_usage(&self.usage);
    }

    /// Apps launched from omni, most recent first: (app, last launch unix seconds).
    pub fn recent(&self, limit: usize) -> Vec<(&App, i64)> {
        let mut out: Vec<(&App, i64)> = self
            .apps
            .iter()
            .filter_map(|a| {
                let last = self.usage.get(a.info.id()?.as_str())?.last;
                (last > 0).then_some((a, last))
            })
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1));
        out.truncate(limit);
        out
    }
}

/// Words too generic to find an action on their own (every browser and terminal has a
/// "New window"); they still count once the query also names the app.
const GENERIC_ACTION_WORDS: &[&str] = &["new", "window", "tab", "open", "app"];

/// How similar a typed word is to a word of an app's name, 0..1, or None if not close.
/// Loose on purpose: any misspelling, missing, extra or swapped letters ("frfox",
/// "ptormaster", "brvae"), and also a word still being typed (compared with the start of
/// the name word). Two ways to be close:
/// - few edits: about a third of the typed letters wrong (at least 1);
/// - typed letters all appear in order in the word ("frfox" in "firefox").
fn similarity(query: &str, word: &str) -> Option<f32> {
    // Bytes, not chars: no allocation per comparison (this runs for every app word on
    // every keystroke that matches no app exactly). Names longer than CAP are cut.
    let q = &query.as_bytes()[..query.len().min(CAP)];
    let w = &word.as_bytes()[..word.len().min(CAP)];
    if q.len() < 3 || w.is_empty() {
        return None;
    }
    let mut best: Option<f32> = None;

    let max = (q.len() / 3).max(1);
    let n = q.len();
    let d = [w.len(), n.saturating_sub(1), n, n + 1, n + 2]
        .into_iter()
        .filter(|&len| len >= 1 && len <= w.len() && len.abs_diff(n) <= max)
        .map(|len| edit_distance(q, &w[..len]))
        .min()
        .unwrap_or(usize::MAX);
    if d <= max {
        best = Some(1.0 - d as f32 / (n as f32 + 1.0));
    }

    // In-order letters: closeness = typed letters / stretch of the word they span.
    let mut it = w.iter().enumerate();
    let mut first = None;
    let mut last = 0;
    for c in q {
        match it.find(|(_, x)| *x == c) {
            Some((i, _)) => {
                first.get_or_insert(i);
                last = i;
            }
            None => return best,
        }
    }
    // Must start like the word, and not be scattered across it.
    let first = first.unwrap_or(0);
    let span = last - first + 1;
    if q[0] != w[0] || span * 2 > n * 3 {
        return best;
    }
    let sub = n as f32 / span as f32 * 0.9;
    Some(best.map_or(sub, |b| b.max(sub)))
}

/// Longest word compared by `similarity`, in bytes.
const CAP: usize = 48;

/// Edit distance with neighbour swaps (optimal string alignment), over three rolling rows
/// on the stack. Both inputs are at most CAP long.
fn edit_distance(a: &[u8], b: &[u8]) -> usize {
    // u8 cells: distances never exceed CAP.
    let m = b.len();
    let mut prev2 = [0u8; CAP + 1];
    let mut prev: [u8; CAP + 1] = std::array::from_fn(|j| j as u8);
    let mut cur = [0u8; CAP + 1];
    for i in 1..=a.len() {
        cur[0] = i as u8;
        for j in 1..=m {
            let cost = u8::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                cur[j] = cur[j].min(prev2[j - 2] + 1);
            }
        }
        prev2 = prev;
        prev = cur;
    }
    prev[m] as usize
}

/// Score an app action against the query words; None unless every word matches the app's
/// name or the action, and at least one matches the action. Without the app's name in the
/// query, an action needs a distinctive whole word ("incognito", "tor"), so typing "ne"
/// doesn't list every app's "New window". Also returns whether the query named the app.
fn action_score(app_words: &[String], action_words: &[String], keywords: &[String], query: &[String]) -> Option<(u32, bool)> {
    let (mut total, mut names_action, mut names_app, mut distinctive) = (0, false, false, false);
    for w in query {
        let generic = GENERIC_ACTION_WORDS.contains(&w.as_str());
        let exact = |list: &[String]| list.iter().any(|a| a == w);
        let prefix = |list: &[String]| list.iter().any(|a| a.starts_with(w.as_str()));
        total += if prefix(action_words) {
            names_action = true;
            distinctive |= !generic && exact(action_words);
            if exact(action_words) { 80 } else { 70 }
        } else if prefix(keywords) {
            names_action = true;
            distinctive |= !generic && exact(keywords);
            if exact(keywords) { 80 } else { 40 }
        } else if prefix(app_words) {
            names_app = true;
            50
        } else {
            return None;
        };
    }
    (names_action && (names_app || distinctive)).then_some((total, names_app))
}

fn usage_path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| glib::home_dir().join(".local/state"));
    state.join("omni").join("usage.tsv")
}

/// Lines: `id<TAB>count<TAB>last` (older files have no `last` column).
fn load_usage() -> HashMap<String, Usage> {
    let Ok(text) = std::fs::read_to_string(usage_path()) else { return HashMap::new() };
    text.lines()
        .filter_map(|l| {
            let mut cols = l.split('\t');
            let id = cols.next()?;
            let count = cols.next()?.parse().ok()?;
            let last = cols.next().and_then(|c| c.parse().ok()).unwrap_or(0);
            Some((id.to_string(), Usage { count, last }))
        })
        .collect()
}

fn save_usage(usage: &HashMap<String, Usage>) {
    let path = usage_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text: String = usage.iter().map(|(id, u)| format!("{id}\t{}\t{}\n", u.count, u.last)).collect();
    // Write then rename, so a crash mid-write can't leave a truncated file.
    // The pid keeps two omni processes from sharing one temp file and renaming a torn write.
    let tmp = path.with_file_name(format!("usage.tsv.{}.tmp", std::process::id()));
    if let Err(e) = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path)) {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("omni: can't save {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Act {
        app: Vec<String>,
        name: Vec<String>,
        keywords: Vec<String>,
    }

    fn act(app: &str, name: &str, keywords: &str) -> Act {
        Act { app: words(app), name: words(name), keywords: words(keywords) }
    }

    fn brave() -> Vec<(&'static str, Act)> {
        vec![
            ("window", act("Brave Web Browser brave browser stable", "New window", "")),
            ("private", act("Brave Web Browser brave browser stable", "New private window", "new incognito window")),
            ("tor", act("Brave Web Browser brave browser stable", "New private window with Tor", "onion incognito")),
        ]
    }

    /// Which of Brave's actions the query finds, best first.
    fn found(query: &str) -> Vec<&'static str> {
        let q = words(query);
        let mut hits: Vec<(u32, &str)> = brave()
            .iter()
            .filter_map(|(id, a)| Some((action_score(&a.app, &a.name, &a.keywords, &q)?.0, *id)))
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0));
        hits.into_iter().map(|(_, id)| id).collect()
    }

    #[test]
    fn loose_matches() {
        let close = |q: &str, w: &str| similarity(q, w).is_some();
        assert!(close("brvae", "brave"));
        assert!(close("forfox", "firefox"));
        assert!(close("frfox", "firefox"));
        assert!(close("ptormaster", "portmaster"));
        assert!(close("spotfy", "spotify"));
        assert!(close("thundr", "thunderbird"));
        assert!(close("rbave", "brave"));
        assert!(!close("bluetooth", "brave"));
        assert!(!close("zzz", "firefox"));
        // closer spellings score higher
        assert!(similarity("firefx", "firefox") > similarity("frfox", "firefox"));
    }

    #[test]
    fn app_name_alone_shows_no_actions() {
        assert!(found("brave").is_empty());
        assert!(found("br").is_empty());
    }

    #[test]
    fn generic_words_alone_show_no_actions() {
        for q in ["n", "ne", "new", "window", "new window", "w"] {
            assert!(found(q).is_empty(), "{q:?}");
        }
    }

    #[test]
    fn distinctive_words_find_actions() {
        assert_eq!(found("brave tor"), ["tor"]);
        assert_eq!(found("tor"), ["tor"]);
        assert_eq!(found("incognito")[0], "private");
        assert!(found("incognito").contains(&"tor"));
        assert_eq!(found("private window").len(), 2);
        assert_eq!(found("private").len(), 2);
    }

    #[test]
    fn app_name_plus_action_word() {
        assert_eq!(found("brave new window")[0], "window");
        assert_eq!(found("brave n").len(), 3);
        assert_eq!(found("brave private")[0], "private");
        assert!(found("brave xyz").is_empty());
        assert!(found("tor xyz").is_empty());
    }
}
