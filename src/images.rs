//! Image search: `img <description>` ranks indexed pictures by meaning (embeddinggemma-2).
//!
//! The model runs in `omni-embed.py` (installed with its own Python venv, onnxruntime-gpu).
//! - `serve`: started on demand (typing `img`), loads only the text side, answers one JSON
//!   line per query, and exits by itself 10 s after the last one.
//! - `crawl`: embeds new or changed images in the configured folders (~/Pictures by default,
//!   ~/.config/omni/image-folders), then exits. Runs 10 s after start and hourly unless
//!   Settings → Image search is off; Index now / Add folder… run it right away.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gio, glib};

/// Words that start an image search, followed by a space: `img cat`.
const PREFIX: &str = "img";
const CRAWL_FIRST: Duration = Duration::from_secs(10);
const CRAWL_EVERY: Duration = Duration::from_secs(3600);

#[derive(Clone)]
pub struct ImageHit {
    pub path: PathBuf,
    /// Empty when the crawl couldn't write one.
    pub thumb: PathBuf,
}

pub enum Results {
    Ready(Vec<ImageHit>),
    /// Asked; the previous query's results are shown meanwhile.
    Searching(Vec<ImageHit>),
    /// Nothing indexed yet (or the model isn't downloaded).
    Empty,
    /// Helper not installed / failed to start.
    Unavailable,
}

/// `img red terminal` → Some("red terminal"); `img` alone → Some(""). Case-insensitive.
pub fn parse(query: &str) -> Option<&str> {
    let head = query.get(..PREFIX.len())?;
    if !head.eq_ignore_ascii_case(PREFIX) {
        return None;
    }
    let rest = &query[PREFIX.len()..];
    if rest.is_empty() {
        return Some("");
    }
    rest.starts_with(' ').then(|| rest.trim())
}

fn data_dir() -> PathBuf {
    glib::user_data_dir().join("omni")
}

fn python() -> PathBuf {
    data_dir().join("embed-venv/bin/python")
}

fn script() -> PathBuf {
    data_dir().join("bin/omni-embed.py")
}

pub fn installed() -> bool {
    python().exists() && script().exists()
}

fn spawn(args: &[&str]) -> Result<gio::Subprocess, glib::Error> {
    let py = python();
    let sc = script();
    let mut argv: Vec<&std::ffi::OsStr> = vec![py.as_os_str(), "-I".as_ref(), sc.as_os_str()];
    argv.extend(args.iter().map(|a| std::ffi::OsStr::new(*a)));
    let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::STDIN_PIPE | gio::SubprocessFlags::STDOUT_PIPE);
    // omni pins Vulkan/EGL to the display GPU; CUDA doesn't read these, but keep the child clean.
    for k in ["__EGL_VENDOR_LIBRARY_FILENAMES", "__GLX_VENDOR_LIBRARY_NAME", "VK_DRIVER_FILES", "VK_ICD_FILENAMES", "OMNI_GPU_ENV"] {
        launcher.unsetenv(k);
    }
    launcher.spawn(&argv)
}

/// Read stdout line by line on the main loop; `on_line` gets each, `on_eof` runs once at the end.
fn read_lines(proc: &gio::Subprocess, on_line: impl Fn(&str) + 'static, on_eof: impl FnOnce() + 'static) {
    let Some(out) = proc.stdout_pipe() else { return on_eof() };
    let reader = gio::DataInputStream::new(&out);
    glib::spawn_future_local(async move {
        loop {
            match reader.read_line_utf8_future(glib::Priority::DEFAULT).await {
                Ok(Some(line)) => on_line(line.as_str()),
                _ => break,
            }
        }
        on_eof();
    });
}

struct Server {
    stdin: gio::OutputStream,
    /// Bumped per spawn so a dead server's EOF can't clear its replacement.
    id: u64,
}

pub struct Images {
    server: RefCell<Option<Server>>,
    spawns: Cell<u64>,
    /// The current server said "ready" (loaded). Dying before that means it can't start.
    ready: Cell<bool>,
    failed: Cell<bool>,
    /// Query sent and not answered yet, and the newest one wanted meanwhile (latest wins).
    in_flight: RefCell<Option<String>>,
    wanted: RefCell<Option<String>>,
    /// Last answer: its query, hits, and whether the index was empty.
    answer: RefCell<Option<(String, Vec<ImageHit>, bool)>>,
    on_results: RefCell<Option<Box<dyn Fn()>>>,
    crawling: Cell<bool>,
    /// Folders added while a crawl was running (path, with subdirectories): done after it.
    pending_add: RefCell<Vec<(PathBuf, bool)>>,
    /// A query lost with a dead server is asked again once; this is that query.
    retried: RefCell<Option<String>>,
    crawl_status: RefCell<String>,
    on_crawl: RefCell<Option<Box<dyn Fn(&str)>>>,
}

impl Images {
    pub fn new() -> Rc<Self> {
        let me = Rc::new(Self {
            server: RefCell::new(None),
            spawns: Cell::new(0),
            ready: Cell::new(false),
            failed: Cell::new(false),
            in_flight: RefCell::new(None),
            wanted: RefCell::new(None),
            answer: RefCell::new(None),
            on_results: RefCell::new(None),
            crawling: Cell::new(false),
            pending_add: RefCell::new(Vec::new()),
            retried: RefCell::new(None),
            crawl_status: RefCell::new(String::new()),
            on_crawl: RefCell::new(None),
        });
        *me.crawl_status.borrow_mut() = me.index_summary();
        me
    }

    /// Called when an answer arrives (the UI re-runs its search).
    pub fn connect_results(&self, f: impl Fn() + 'static) {
        *self.on_results.borrow_mut() = Some(Box::new(f));
    }

    /// Called with a status line whenever a crawl reports progress or finishes.
    pub fn connect_crawl(&self, f: impl Fn(&str) + 'static) {
        *self.on_crawl.borrow_mut() = Some(Box::new(f));
    }

    /// Start the helper ahead of the first query (`img` typed), hiding its ~2 s start.
    pub fn warm(self: &Rc<Self>) {
        if self.server.borrow().is_none() && installed() {
            self.start_server();
        }
    }

    pub fn results(self: &Rc<Self>, query: &str) -> Results {
        if !installed() || self.failed.get() {
            return Results::Unavailable;
        }
        let (fresh, hits, empty) = match self.answer.borrow().as_ref() {
            Some((q, hits, empty)) => (q == query, hits.clone(), *empty),
            None => (false, Vec::new(), false),
        };
        match (fresh, empty) {
            (true, true) => Results::Empty,
            (true, false) => Results::Ready(hits),
            (false, _) => {
                self.request(query);
                Results::Searching(hits)
            }
        }
    }

    fn request(self: &Rc<Self>, query: &str) {
        if self.in_flight.borrow().as_deref() == Some(query) {
            return;
        }
        if self.in_flight.borrow().is_some() {
            *self.wanted.borrow_mut() = Some(query.to_string());
            return;
        }
        self.send(query);
    }

    fn send(self: &Rc<Self>, query: &str) {
        if self.server.borrow().is_none() {
            self.start_server();
        }
        let line = format!("{}\n", serde_json::json!({ "q": query, "k": crate::MAX_IMAGES }));
        let ok = self
            .server
            .borrow()
            .as_ref()
            .is_some_and(|s| s.stdin.write_all(line.as_bytes(), gio::Cancellable::NONE).is_ok());
        if ok {
            *self.in_flight.borrow_mut() = Some(query.to_string());
        } else {
            // Exited between its last answer and now (idle timeout): start again.
            *self.server.borrow_mut() = None;
            self.start_server();
            if let Some(s) = self.server.borrow().as_ref() {
                if s.stdin.write_all(line.as_bytes(), gio::Cancellable::NONE).is_ok() {
                    *self.in_flight.borrow_mut() = Some(query.to_string());
                }
            }
        }
    }

    fn start_server(self: &Rc<Self>) {
        let proc = match spawn(&["serve"]) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("omni: image search helper didn't start: {e}");
                self.failed.set(true);
                return;
            }
        };
        let Some(stdin) = proc.stdin_pipe() else { return };
        let id = self.spawns.get() + 1;
        self.spawns.set(id);
        self.ready.set(false);
        *self.server.borrow_mut() = Some(Server { stdin, id });
        let me = Rc::downgrade(self);
        let me_eof = me.clone();
        read_lines(
            &proc,
            move |line| {
                if let Some(me) = me.upgrade() {
                    me.on_line(line);
                }
            },
            move || {
                let Some(me) = me_eof.upgrade() else { return };
                if me.server.borrow().as_ref().is_some_and(|s| s.id == id) {
                    *me.server.borrow_mut() = None;
                    if !me.ready.get() {
                        // Crashed while loading: don't respawn in a loop. Next omni start retries.
                        eprintln!("omni: image search helper exited while loading");
                        me.failed.set(true);
                        me.in_flight.borrow_mut().take();
                        me.wanted.borrow_mut().take();
                        if let Some(f) = me.on_results.borrow().as_ref() {
                            f();
                        }
                        return;
                    }
                    // A query lost with the process (the idle exit racing a query) is asked again
                    // of a fresh one, once: losing it twice means this query kills the helper.
                    let lost = me.in_flight.borrow_mut().take().or_else(|| me.wanted.borrow_mut().take());
                    if let Some(q) = lost {
                        if me.retried.borrow().as_deref() == Some(q.as_str()) {
                            eprintln!("omni: image search helper died twice on {q:?}");
                            *me.answer.borrow_mut() = Some((q, Vec::new(), false));
                            if let Some(f) = me.on_results.borrow().as_ref() {
                                f();
                            }
                        } else {
                            *me.retried.borrow_mut() = Some(q.clone());
                            me.send(&q);
                        }
                    }
                }
            },
        );
    }

    fn on_line(self: &Rc<Self>, line: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
        if v.get("event").and_then(|e| e.as_str()) == Some("ready") {
            self.ready.set(true);
            return;
        }
        let Some(q) = v.get("q").and_then(|q| q.as_str()) else { return };
        let hits = v
            .get("hits")
            .and_then(|h| h.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|h| {
                        Some(ImageHit {
                            path: PathBuf::from(h.get("p")?.as_str()?),
                            thumb: PathBuf::from(h.get("t").and_then(|t| t.as_str()).unwrap_or("")),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let empty = v.get("empty").and_then(|e| e.as_bool()).unwrap_or(false);
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            eprintln!("omni: image search failed for {q:?}: {e}");
        }
        *self.answer.borrow_mut() = Some((q.to_string(), hits, empty));
        *self.in_flight.borrow_mut() = None;
        *self.retried.borrow_mut() = None;
        let next = self.wanted.borrow_mut().take();
        if let Some(next) = next.filter(|n| n != q) {
            self.send(&next);
        }
        if let Some(f) = self.on_results.borrow().as_ref() {
            f();
        }
    }

    // ---- crawl ----

    pub fn crawl_status(&self) -> String {
        self.crawl_status.borrow().clone()
    }

    fn set_crawl_status(&self, s: String) {
        *self.crawl_status.borrow_mut() = s;
        if let Some(f) = self.on_crawl.borrow().as_ref() {
            f(&self.crawl_status.borrow());
        }
    }

    fn index_summary(&self) -> String {
        if !installed() {
            return "Not installed: rerun install.sh.".into();
        }
        let status = std::fs::read_to_string(data_dir().join("images/status.json")).ok();
        let v = status.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
        let count = v.as_ref().and_then(|v| v.get("count")?.as_u64());
        let folders = folders()
            .iter()
            .map(|(f, rec)| format!("{}{}", label(f), if *rec { " (with subdirectories)" } else { "" }))
            .collect::<Vec<_>>()
            .join(", ");
        match count {
            Some(n) => format!("{n} image{} indexed from {folders}.", if n == 1 { "" } else { "s" }),
            None => format!("Nothing indexed yet. Indexes {folders}."),
        }
    }

    pub fn crawling(&self) -> bool {
        self.crawling.get()
    }

    /// Embed new and changed images. One crawl at a time.
    /// - `add`: a folder (and whether to take its subdirectories) joins the list first.
    /// - `fast`: the user asked (Index now, adding a folder): full speed when the machine has
    ///   the room. Otherwise the quiet background pace.
    pub fn crawl(self: &Rc<Self>, add: Option<(&Path, bool)>, fast: bool) {
        if !installed() {
            return;
        }
        if self.crawling.get() {
            if let Some((dir, rec)) = add {
                self.pending_add.borrow_mut().push((dir.to_path_buf(), rec));
                self.set_crawl_status(format!("{} is next, after this crawl.", label(dir)));
            }
            return;
        }
        let dir = add.map(|(p, _)| p.to_string_lossy().into_owned());
        let mut args = vec!["crawl"];
        if fast {
            args.push("--fast");
        }
        if let Some(d) = dir.as_deref() {
            args.extend(["--add", d]);
            if add.is_some_and(|(_, rec)| rec) {
                args.push("--recursive");
            }
        }
        let added_label = add.map(|(p, rec)| (label(p), rec));
        // What the crawl reported: the added folder's state ("new", "deeper", "same") and
        // how many images it embedded.
        let folder_state: Rc<RefCell<Option<String>>> = Rc::default();
        let added_count: Rc<Cell<u64>> = Rc::default();
        let proc = match spawn(&args) {
            Ok(p) => p,
            Err(e) => {
                self.set_crawl_status(format!("Couldn't start indexing: {e}"));
                return;
            }
        };
        self.crawling.set(true);
        self.set_crawl_status("Looking for new images…".into());
        let me = Rc::downgrade(self);
        let me_eof = me.clone();
        let (fs, ac) = (folder_state.clone(), added_count.clone());
        read_lines(
            &proc,
            move |line| {
                let Some(me) = me.upgrade() else { return };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
                match v.get("event").and_then(|e| e.as_str()) {
                    Some("folder") => *fs.borrow_mut() = v.get("state").and_then(|s| s.as_str()).map(String::from),
                    Some("done") => ac.set(v.get("added").and_then(|a| a.as_u64()).unwrap_or(0)),
                    Some("downloading") => me.set_crawl_status("Downloading the image model (about 900 MB)…".into()),
                    Some("progress") => {
                        let done = v.get("done").and_then(|d| d.as_u64()).unwrap_or(0);
                        let total = v.get("total").and_then(|d| d.as_u64()).unwrap_or(0);
                        me.set_crawl_status(format!("Indexing images… {done} of {total}"));
                    }
                    Some("busy") => me.set_crawl_status("Already indexing.".into()),
                    _ => {}
                }
            },
            move || {
                let Some(me) = me_eof.upgrade() else { return };
                me.crawling.set(false);
                let next = me.pending_add.borrow_mut().pop();
                if let Some((dir, rec)) = next {
                    me.crawl(Some((&dir, rec)), true);
                    return;
                }
                // New vectors: answers cached for the old index are stale.
                *me.answer.borrow_mut() = None;
                let n = added_count.get();
                let images = |n: u64| format!("{n} new image{}", if n == 1 { "" } else { "s" });
                let lead = match (added_label, folder_state.borrow().as_deref()) {
                    (Some((name, _)), Some("same")) if n == 0 => {
                        format!("{name} is already indexed: no new images. Add it again with Subdirectories on to take those too.")
                    }
                    (Some((name, _)), Some("same")) => format!("{name} is already indexed: {} added.", images(n)),
                    (Some((name, _)), Some("deeper")) => format!("{name} now includes its subdirectories: {}.", images(n)),
                    (Some((name, rec)), _) => {
                        format!("Added {name}{}: {}.", if rec { " with subdirectories" } else { "" }, images(n))
                    }
                    (None, _) if fast => format!("Done: {}.", images(n)),
                    (None, _) => String::new(),
                };
                let summary = me.index_summary();
                me.set_crawl_status(if lead.is_empty() { summary } else { format!("{lead}\n{summary}") });
                if let Some(f) = me.on_results.borrow().as_ref() {
                    f();
                }
            },
        );
    }

    /// The silent crawl: 10 s after start, then hourly, while the setting is on.
    pub fn schedule_crawls(self: &Rc<Self>) {
        let me = Rc::downgrade(self);
        glib::timeout_add_local_once(CRAWL_FIRST, move || {
            let Some(me) = me.upgrade() else { return };
            if auto_crawl() {
                me.crawl(None, false);
            }
            let weak = Rc::downgrade(&me);
            glib::timeout_add_local(CRAWL_EVERY, move || {
                let Some(me) = weak.upgrade() else { return glib::ControlFlow::Break };
                if auto_crawl() {
                    me.crawl(None, false);
                }
                glib::ControlFlow::Continue
            });
        });
    }
}

fn auto_crawl_path() -> PathBuf {
    glib::user_config_dir().join("omni/image-crawl")
}

/// Settings → Image search (on by default).
pub fn auto_crawl() -> bool {
    std::fs::read_to_string(auto_crawl_path()).map_or(true, |s| s.trim() != "off")
}

pub fn set_auto_crawl(on: bool) -> std::io::Result<()> {
    let path = auto_crawl_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, if on { "on\n" } else { "off\n" })
}

/// `~/Wallpapers` for display.
fn label(dir: &Path) -> String {
    crate::recent::folder_label(&dir.join("x"))
}

/// Folders the crawl walks and whether it takes their subdirectories (same file and default
/// as omni-embed.py: one per line, a trailing `/**` for subdirectories; none = ~/Pictures/**).
pub fn folders() -> Vec<(PathBuf, bool)> {
    let home = glib::home_dir();
    match std::fs::read_to_string(glib::user_config_dir().join("omni/image-folders")) {
        Ok(s) => s
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| {
                let (l, rec) = match l.strip_suffix("/**") {
                    Some(base) => (base, true),
                    None => (l, false),
                };
                let path = match l.strip_prefix("~") {
                    Some(rest) => home.join(rest.trim_start_matches('/')),
                    None => PathBuf::from(l),
                };
                (path, rec)
            })
            .collect(),
        Err(_) => vec![(home.join("Pictures"), true)],
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn prefix() {
        assert_eq!(parse("img red terminal"), Some("red terminal"));
        assert_eq!(parse("IMG cat "), Some("cat"));
        assert_eq!(parse("img"), Some(""));
        assert_eq!(parse("img "), Some(""));
        assert_eq!(parse("imgur"), None);
        assert_eq!(parse("im"), None);
        assert_eq!(parse("image cat"), None);
    }
}
