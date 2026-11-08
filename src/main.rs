mod answer;
mod browser_frame;
mod chromium;
mod apps;
mod calc;
mod dock;
mod files;
mod glass;
mod probe;
mod recent;
mod ring;
mod services;
mod search_provider;
mod search_view;
mod sparql;
mod util;
mod web;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

const APP_ID: &str = "dev.omni.Launcher";

/// LocalSearch change events received / file paths added from them (for the debug query).
pub static LIVE_EVENTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static LIVE_ADDED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_APPS: usize = 6;
const MAX_ACTIONS: usize = 3;
/// Recent apps/files/folders shown under the bar when nothing is typed.
const MAX_RECENT: usize = 5;
const MAX_FILES: usize = 5;
const MAX_DIRS: usize = 3;
/// How long after a confirmation appears before Yes / Always count (µs).
const CONFIRM_ARM_US: i64 = 500_000;
const SEARCH_PLACEHOLDER: &str = "Search apps, files and the web";
const REPLY_PLACEHOLDER: &str = "Reply to Claude \u{b7} Esc to go back to search";
/// Per-kind limit when the filter narrows results to some kinds (more room for each).
const MAX_FILTERED: usize = 8;

/// Result kinds the filter can show (bits). All bits set = no filter.
const SHOW_APPS: u8 = 1;
const SHOW_FILES: u8 = 2;
const SHOW_FOLDERS: u8 = 4;
const SHOW_SERVICES: u8 = 8;
const SHOW_ALL: u8 = SHOW_APPS | SHOW_FILES | SHOW_FOLDERS | SHOW_SERVICES;
/// "All" as saved before services existed.
const OLD_SHOW_ALL: u8 = SHOW_APPS | SHOW_FILES | SHOW_FOLDERS;
/// Services shown for a query (they only match by name, so few are enough).
const MAX_SERVICES: usize = 3;
const MAX_CONTENT: usize = 50;
const MAX_OPEN_WITH: usize = 10;
/// File *contents* search debounce (names/apps stay immediate).
const CONTENT_DELAY: Duration = Duration::from_millis(120);
/// Reuse identical Contents results for this long.
const CONTENT_CACHE_TTL: Duration = Duration::from_secs(30);
/// Flush streamed content hits to the UI at least this often.
const CONTENT_BATCH: Duration = Duration::from_millis(50);
/// Re-walk the disk this often (new files arrive in between via LocalSearch's events).
const REWALK: Duration = Duration::from_secs(30 * 60);

/// Card geometry on screen (logical px).
const CARD_WIDTH: i32 = 720;
const CARD_TOP: i32 = 160;
/// The card stops this far above the bottom of the screen; longer lists scroll inside it.
const BOTTOM_GAP: i32 = 96;
/// Non-list chrome: padding + header + path footer (must match style.css).
const CARD_CHROME: i32 = 108;
/// Result row height and how many fit before the list scrolls (style.css --row-height).
const ROW_HEIGHT: i32 = 52;
const MAX_VISIBLE_ROWS: i32 = 7;
/// Selection highlight base height for scaleY (matches row height).
const SEL_BASE_H: f64 = ROW_HEIGHT as f64;
/// Accent pill size / inset (style.css .omni-selection-pill).
const SEL_PILL_H: f64 = 18.0;
const SEL_PILL_W: i32 = 3;
const SEL_PILL_X: i32 = 6;
/// Selection move timing (transform only).
const SELECT_MOTION_MS: f64 = 120.0;
/// Path footer strip (style.css .omni-path / .omni-path-text).
const PATH_FOOTER_HEIGHT: i32 = 32;
const PATH_FOOTER_PAD_X: f64 = 14.0;
const PATH_ELLIPSIS: &str = "\u{2026}";

/// What Enter does on a result row.
#[derive(Clone)]
enum Action {
    App(gio::AppInfo),
    /// "New private window" and the like (see apps.rs).
    AppAction(gio::AppInfo, apps::AppAction),
    /// A file or folder: open with its default app.
    Open(gio::File),
    /// A file, from the right-click menu's "Open with".
    OpenWith(gio::AppInfo, gio::File),
    Web(String),
    /// From the web row's right-click menu: search in a private or Tor window.
    WebIn(String, web::Window),
    Claude(String),
    /// Calculator result: copy to the clipboard.
    Copy(String),
    /// A systemd service: activating it does nothing; its actions are on right-click.
    Service(services::Service),
    /// One of a service's actions, from that menu.
    ServiceVerb(services::Service, services::Verb),
    /// Show a service's `systemctl status` in the popup.
    ServiceStatus(services::Service),
    /// Pin (true) or unpin an app on the quickshell dock, by desktop id.
    DockPin(String, bool),
    /// Informational row (Contents empty / unavailable); not activatable.
    Info,
}

/// How a file row matched the query (Contents mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchType {
    Name,
    Content,
    Both,
}

struct Hit {
    title: String,
    subtitle: String,
    /// When true, `subtitle` is Pango markup (accent-highlighted snippet).
    subtitle_markup: bool,
    icon: Option<gio::Icon>,
    action: Action,
    /// A file: right-click / Menu key opens Open and Open with. Folders use the same
    /// flag as false and get a smaller Open / containing-folder / copy-path menu.
    /// (App rows always have a menu: Open and the app's actions.)
    file_menu: bool,
    /// Set for file/folder rows when Contents can classify the match.
    match_type: Option<MatchType>,
}

/// Cache identity: search text + optional directory scope (Contents on).
#[derive(Clone, Debug, PartialEq, Eq)]
struct ContentKey {
    query: String,
    /// Canonical directory path string, or empty when unscoped.
    scope: String,
}

impl ContentKey {
    fn new(query: &str, scope: Option<&std::path::Path>) -> Self {
        Self {
            query: query.to_string(),
            scope: scope.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        }
    }

    fn scope_path(&self) -> Option<PathBuf> {
        (!self.scope.is_empty()).then(|| PathBuf::from(&self.scope))
    }
}

/// One contents hit: path plus optional subtitle snippet (filled lazily).
struct ContentHit {
    path: PathBuf,
    snippet: Option<String>,
}

/// In-memory Contents results. Old hits stay visible until the new request's first batch.
enum ContentCache {
    Idle,
    Failed { key: ContentKey, message: String },
    Active {
        /// Key of the hits currently shown.
        key: ContentKey,
        hits: Vec<ContentHit>,
        searching: bool,
        /// In-flight key when it differs from `key` (keep showing old hits until first batch).
        pending_key: Option<ContentKey>,
        /// When `searching` last became false (TTL cache).
        ready_at: Option<Instant>,
    },
}

impl Default for ContentCache {
    fn default() -> Self {
        Self::Idle
    }
}

impl ContentCache {
    fn searching(&self) -> bool {
        matches!(self, Self::Active { searching: true, .. })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum UiMode {
    #[default]
    Launcher,
    Settings,
    WebSearch,
}

#[derive(Clone, Copy)]
enum UiModeAction {
    OpenSettings,
    Back,
    Close,
}

impl UiMode {
    fn after(self, action: UiModeAction) -> Self {
        match action {
            UiModeAction::OpenSettings => Self::Settings,
            UiModeAction::Back | UiModeAction::Close => Self::Launcher,
        }
    }

    fn consumes_escape(self) -> bool {
        self != Self::Launcher
    }

    fn stack_name(self) -> &'static str {
        match self {
            Self::Launcher => "launcher",
            Self::Settings => "settings",
            Self::WebSearch => "web-search",
        }
    }
}

struct Ui {
    window: gtk::ApplicationWindow,
    card: glass::GlassCard,
    main_stack: gtk::Stack,
    mode: Cell<UiMode>,
    settings_back: gtk::Button,
    search_provider: Cell<search_provider::SearchProvider>,
    search_view: search_view::SearchView,
    entry: gtk::SearchEntry,
    list: gtk::ListBox,
    list_scroll: gtk::ScrolledWindow,
    list_overlay: gtk::Overlay,
    /// Floating selection fill (translateY + scaleY); sibling of the pill, not its parent.
    selection_fill: gtk::Box,
    /// Accent pill (translateY only; fixed 18px height).
    selection_pill: gtk::Box,
    /// CSS providers: transform values vs transition on/off (separate so snap can gate transition).
    sel_transform_css: gtk::CssProvider,
    sel_transition_css: gtk::CssProvider,
    /// Owned selection: always valid while `hits` is non-empty (0..count). Not GTK focus.
    selected_index: Cell<usize>,
    /// Current highlight geometry (logical; driven through CSS transform, not layout).
    sel_y: Cell<f64>,
    sel_h: Cell<f64>,
    /// Cached per-row (y, height) in overlay coords — measured only on list/layout updates.
    row_geom: RefCell<Vec<(f64, f64)>>,
    /// Snap path (results update / resize): transition disabled until next frame.
    is_snapping: Cell<bool>,
    /// Transition CSS currently enabled for selection chrome.
    sel_transition_on: Cell<bool>,
    /// True between list rebuild and deferred post-layout snap (skip resize snap).
    sel_layout_pending: Cell<bool>,
    /// Resize/scale changed while a snap was in flight — re-place once snap finishes.
    sel_needs_replace: Cell<bool>,
    /// Bumped on each results visual update (for paint-frame accounting).
    sel_update_gen: Cell<u64>,
    /// FrameClock paint callbacks counted for the current `sel_update_gen`.
    sel_paint_frames: Cell<u32>,
    /// Monotonic start of the current open (OMNI_LOG_OPEN staging).
    open_t0: Cell<Option<Instant>>,
    /// Bumped on hide / cancel so deferred snap/retry ticks become no-ops.
    sel_place_gen: Cell<u64>,
    /// Allow place while the window is still unmapped (open path, before present).
    place_while_hidden: Cell<bool>,
    /// Ignore hover-driven selection until this monotonic time (µs), and until real motion.
    hover_suppress_until: Cell<i64>,
    /// Last pointer position for hover — ignore synthetic moves when the cursor is still.
    hover_pointer: Cell<(f64, f64)>,
    /// Coalesce hover selection updates to one per frame.
    hover_frame_pending: Cell<bool>,
    hover_pending_index: Cell<Option<usize>>,
    reveal: gtk::Revealer,
    apps: RefCell<apps::Apps>,
    web: RefCell<web::Engine>,
    hits: RefCell<Vec<Hit>>,
    /// Parallel to list row order — keys for reuse without unsafe widget data.
    row_keys: RefCell<Vec<String>>,
    answer: Rc<answer::Answer>,
    answer_reveal: gtk::Revealer,
    /// Self-reference for callbacks that outlive a borrow (content search results).
    me: RefCell<Weak<Ui>>,
    files: files::Shared,
    /// LocalSearch connection (contents search, change events); None until connected.
    sparql: RefCell<Option<Arc<sparql::Connection>>>,
    /// Kept alive so its change events keep arriving.
    notifier: RefCell<Option<glib::Object>>,
    contents: gtk::ToggleButton,
    /// Filter dropdown button and which kinds it shows (SHOW_* bits).
    filter_button: gtk::Button,
    filter: Cell<u8>,
    /// Contents (full-text) search cache; invalidated when the toggle flips or the query changes.
    content: RefCell<ContentCache>,
    content_gen: Cell<u64>,
    /// Cancel flag for the in-flight Contents backends (ripgrep + drop stale batches).
    content_cancel: RefCell<Arc<AtomicBool>>,
    /// Live ripgrep child so a newer query can kill it immediately.
    content_rg: Arc<Mutex<Option<std::process::Child>>>,
    path_reveal: gtk::Revealer,
    path_box: gtk::Box,
    path_label: gtk::Label,
    /// Full (untruncated) path for the selected row; display is derived + cached.
    path_full: RefCell<String>,
    /// Cache key (full path, footer content width) → displayed truncated string.
    path_trunc_cache: RefCell<Option<(String, i32, String)>>,
    /// Bumped on every hover change, so a delayed "hide" doesn't undo a newer hover.
    hover_gen: Cell<u64>,
    /// Query the list was last built for (to keep the selection when results fill in).
    shown_query: RefCell<String>,
    /// Heartbeat timer for the watchdog; runs only while the popup is open.
    heartbeat: RefCell<Option<glib::SourceId>>,
    /// In a conversation with Claude: the search bar is the reply box, typing doesn't search.
    chatting: Cell<bool>,
    /// Confirmation layer over the card: its question, detail line, and what Yes does.
    confirm: gtk::Box,
    confirm_text: gtk::Label,
    confirm_link_label: gtk::Label,
    confirm_action: RefCell<Option<Box<dyn FnOnce(&Ui)>>>,
    /// What No (and Esc, a click on the dim, closing) and the optional third button do.
    confirm_no_action: RefCell<Option<Box<dyn FnOnce(&Ui)>>>,
    confirm_always_action: RefCell<Option<Box<dyn FnOnce(&Ui)>>>,
    confirm_yes: gtk::Button,
    confirm_no: gtk::Button,
    confirm_always: gtk::Button,
    /// When the layer last appeared (approving is ignored for a moment after that), and the
    /// scroller around its detail.
    confirm_shown_at: Cell<i64>,
    confirm_scroll: gtk::ScrolledWindow,
    /// A service's status panel: shown, its title, output, scroller; bumped per request.
    status_reveal: gtk::Revealer,
    status_title: gtk::Label,
    status_body: gtk::Label,
    status_scroll: gtk::ScrolledWindow,
    status_gen: Cell<u64>,
    /// systemd services (system and user), re-read in the background on every open.
    services: RefCell<Vec<services::Service>>,
    services_loading: Cell<bool>,
    /// The open right-click menu. Its keys, clicks and scrolls are its own while it's up.
    menu: RefCell<Option<gtk::Popover>>,
}

fn main() -> glib::ExitCode {
    probe::init();
    probe::log("process start");
    scrub_env();
    use_display_gpu_only();

    // Debug: `omni --search QUERY` prints ranked results and exits.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--search") {
        let query = args[2..].join(" ");
        let mut apps = apps::Apps::load();
        // same order as the popup: actions naming their app, apps, then the other actions
        let actions: Vec<String> = apps.search_actions(&query, MAX_ACTIONS).into_iter()
            .map(|(app, action, named)| format!("{}action  {} \u{b7} {}", if named { "+" } else { "-" }, action.name, app.name))
            .collect();
        actions.iter().filter(|l| l.starts_with('+')).for_each(|l| println!("{}", &l[1..]));
        let mut later = actions.iter().filter(|l| l.starts_with('-'));
        let mut placed = false;
        for (app, score, by_name) in apps.search(&query, MAX_APPS) {
            if !by_name && !placed {
                later.by_ref().for_each(|l| println!("{}", &l[1..]));
                placed = true;
            }
            println!("{score:>6}  {}", app.name);
        }
        later.for_each(|l| println!("{}", &l[1..]));
        return glib::ExitCode::SUCCESS;
    }

    // Debug: `omni --web QUERY` prints what a web search would open, without opening it.
    if args.get(1).map(String::as_str) == Some("--web") {
        let engine = web::Engine::detect();
        println!("{} -> {}", engine.name, engine.describe(&args[2..].join(" ")));
        return glib::ExitCode::SUCCESS;
    }

    // Must happen before GTK touches the display (and while still single-threaded).
    if let Err(e) = prepare_hyprland_env() {
        eprintln!("omni: {e}");
        return glib::ExitCode::FAILURE;
    }

    // Debug: `omni --check-env` shows what the display/Hyprland environment resolved to.
    if args.get(1).map(String::as_str) == Some("--check-env") {
        for k in ["GDK_BACKEND", "WAYLAND_DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE"] {
            println!("{k}={}", std::env::var(k).unwrap_or_default());
        }
        let mon = focused_monitor();
        println!("focused monitor: {}", mon.map_or("NOT FOUND".into(), |m| format!("{} {}x{} at {},{}", m.name, m.width, m.height, m.x, m.y)));
        return glib::ExitCode::SUCCESS;
    }

    // Debug: `omni --files QUERY...` builds the file index, then times searches for each query.
    if args.get(1).map(String::as_str) == Some("--files") {
        let (roots, ignored) = files::config();
        println!("roots: {roots:?}");
        let t = std::time::Instant::now();
        let index = files::Index::build(&roots, &ignored);
        println!("indexed {} entries in {:.2}s, index holds {} MB", index.len(), t.elapsed().as_secs_f32(), index.memory() / 1_000_000);
        // SAFETY: only releases free heap memory.
        unsafe { libc::malloc_trim(0) };
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            if let Some(rss) = status.lines().find(|l| l.starts_with("VmRSS")) {
                println!("{rss}");
            }
        }
        for q in &args[2..] {
            let t = std::time::Instant::now();
            let (f, d) = index.search(q, 5, 3, None);
            println!("\n\"{q}\": {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
            for h in f.iter().chain(&d) {
                println!("  {}  {}", if h.is_dir { "dir " } else { "file" }, h.path.display());
            }
        }
        return glib::ExitCode::SUCCESS;
    }

    // Debug: `omni --recent` prints what the popup shows with nothing typed.
    if args.get(1).map(String::as_str) == Some("--recent") {
        if gtk::init().is_err() {
            return glib::ExitCode::FAILURE;
        }
        let apps = apps::Apps::load();
        let mut rows: Vec<(i64, String)> =
            apps.recent(MAX_RECENT).into_iter().map(|(a, t)| (t, format!("app     {}", a.name))).collect();
        for f in recent::recent_files(MAX_RECENT) {
            rows.push((f.time, format!("{}  {}", if f.is_dir { "folder" } else { "file  " }, f.path.display())));
        }
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        for (t, r) in rows.into_iter().take(MAX_RECENT) {
            let when = glib::DateTime::from_unix_local(t).ok().and_then(|d| d.format("%b %d %H:%M").ok());
            println!("{}  {r}", when.unwrap_or_default());
        }
        return glib::ExitCode::SUCCESS;
    }

    // `omni --daemon` starts hidden and stays resident; plain `omni` toggles the popup.
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    let ui: Rc<RefCell<Option<Rc<Ui>>>> = Rc::default();

    for signal in [libc::SIGTERM, libc::SIGINT] {
        let app = app.downgrade();
        glib_unix::unix_signal_add_local(signal, move || {
            if let Some(app) = app.upgrade() { app.quit(); }
            glib::ControlFlow::Break
        });
    }

    let slot = ui.clone();
    app.connect_startup(move |app| {
        let built = build_ui(app);
        search_provider::start_icon_preload();
        probe::start_loop_probe();
        listen_for_toggle(&built);
        *slot.borrow_mut() = Some(built);
        std::mem::forget(app.hold()); // keep running while hidden
    });

    let command_ui = ui.clone();
    app.connect_command_line(move |_, cmd| {
        let daemon = cmd.arguments().iter().any(|a| a == "--daemon");
        if !daemon {
            if let Some(ui) = command_ui.borrow().as_ref() {
                toggle(ui);
            }
        }
        0.into()
    });

    let result = app.run();
    if let Some(ui) = ui.borrow().as_ref() { ui.search_view.close(); }
    if let Err(error) = chromium::finish() {
        eprintln!("omni: {error}");
        return glib::ExitCode::FAILURE;
    }
    result
}

/// At early login Hyprland can start programs before WAYLAND_DISPLAY (or the instance
/// signature hyprctl needs) is exported. Without them GTK may fall back to X11 and hang
/// while holding omni's app id, and every hyprctl call would fail for this process's
/// whole life. So: Wayland only, and fill in missing values from `hyprctl instances`
/// (which works without a signature), waiting up to 5 s for Hyprland to be ready.
fn prepare_hyprland_env() -> Result<(), String> {
    // SAFETY: called first thing in main, before GTK or any other thread exists.
    unsafe { std::env::set_var("GDK_BACKEND", "wayland") };

    let missing = |k: &str| std::env::var(k).map_or(true, |v| v.is_empty());
    if !missing("WAYLAND_DISPLAY") && !missing("HYPRLAND_INSTANCE_SIGNATURE") {
        return Ok(());
    }
    for _ in 0..25 {
        let found = util::output_with_timeout(
            std::process::Command::new("hyprctl").args(["instances", "-j"]),
            Duration::from_millis(500),
        )
        .and_then(|out| serde_json::from_slice::<serde_json::Value>(&out).ok())
        .and_then(|list| {
            // Newest instance whose compositor process is alive.
            let mut alive: Vec<_> = list
                .as_array()?
                .iter()
                .filter(|i| i["pid"].as_i64().is_some_and(|p| std::path::Path::new(&format!("/proc/{p}")).exists()))
                .cloned()
                .collect();
            alive.sort_by_key(|i| i["time"].as_i64().unwrap_or(0));
            alive.pop()
        });
        if let Some(inst) = found {
            if let (Some(sig), Some(sock)) = (inst["instance"].as_str(), inst["wl_socket"].as_str()) {
                if !sock.is_empty() && glib::user_runtime_dir().join(sock).exists() {
                    // SAFETY: still single-threaded (see above); the helper thread that read
                    // hyprctl's output has finished.
                    unsafe {
                        if missing("HYPRLAND_INSTANCE_SIGNATURE") {
                            std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", sig);
                        }
                        if missing("WAYLAND_DISPLAY") {
                            std::env::set_var("WAYLAND_DISPLAY", sock);
                        }
                    }
                    return Ok(());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err("Hyprland's Wayland display isn't available; not starting".into())
}

/// Render on the GPU that drives the screens, and load only its driver. On hybrid laptops
/// the session may prefer NVIDIA (ML4W sets __GLX_VENDOR_LIBRARY_NAME=nvidia, and NVIDIA's EGL
/// vendor file is listed first), which loads ~33 MB of NVIDIA driver into omni for nothing
/// and risks waking the dGPU. Only applied when every connected screen is on an Intel or AMD
/// GPU; if one is on NVIDIA, nothing changes.
///
/// The GL libraries pick their vendor while omni is loading, before main() runs, so changing
/// the variables in-process is too late: omni sets them and re-executes itself once
/// (OMNI_GPU_ENV marks the second run, so it can't loop).
/// Drop inherited secrets before anything starts. omni passes its environment to every
/// app it launches and to Claude, where `printenv` counts as read-only and runs without a
/// prompt. Restarted from a Claude Code session, omni would otherwise carry that session's
/// tokens (`CLAUDE_CODE_MESSAGING_TOKEN`, ...). Runs first, while omni is single-threaded.
fn scrub_env() {
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_none_or(secret_env_name) {
            // SAFETY: no other threads exist yet (first thing in main).
            unsafe { std::env::remove_var(&name) };
        }
    }
}

/// Claude Code's own session variables, Anthropic settings, and anything named like a
/// credential. Not SSH_AUTH_SOCK: a socket path, and apps need it.
fn secret_env_name(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    up.starts_with("CLAUDE_CODE_")
        || up == "CLAUDECODE"
        || up.starts_with("ANTHROPIC_")
        || ["TOKEN", "SECRET", "PASSWORD", "PASSWD", "API_KEY", "APIKEY", "ACCESS_KEY", "PRIVATE_KEY", "CREDENTIAL"]
            .iter()
            .any(|w| up.contains(w))
}

fn use_display_gpu_only() {
    if std::env::var_os("OMNI_GPU_ENV").is_some() {
        return;
    }
    let mut vendors = std::collections::HashSet::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        // Connectors look like "card1-DP-1"; their card is "card1".
        let Some((card, _)) = name.split_once('-') else { continue };
        let connected = std::fs::read_to_string(e.path().join("status")).is_ok_and(|s| s.trim() == "connected");
        if connected {
            if let Ok(v) = std::fs::read_to_string(format!("/sys/class/drm/{card}/device/vendor")) {
                vendors.insert(v.trim().to_string());
            }
        }
    }
    let vendors: Vec<&str> = vendors.iter().map(String::as_str).collect();
    let icd = match vendors.as_slice() {
        ["0x8086"] => "/usr/share/vulkan/icd.d/intel_icd.x86_64.json",
        ["0x1002"] => "/usr/share/vulkan/icd.d/radeon_icd.x86_64.json",
        _ => return, // NVIDIA, mixed, or unknown: don't interfere
    };
    let egl = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
    if !std::path::Path::new(icd).exists() || !std::path::Path::new(egl).exists() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("OMNI_GPU_ENV", "1")
        .env("__EGL_VENDOR_LIBRARY_FILENAMES", egl)
        .env("__GLX_VENDOR_LIBRARY_NAME", "mesa")
        .env("VK_DRIVER_FILES", icd)
        .env("VK_ICD_FILENAMES", icd)
        .exec();
    // Only reached if exec failed: carry on with the session's settings.
    eprintln!("omni: couldn't re-exec with display-GPU settings: {err}");
}

const DEFAULT_CSS: &str = include_str!("style.css");

/// Built-in style, then the user's ~/.config/omni/style.css on top if it exists.
fn load_css() {
    let display = gdk::Display::default().expect("no display");
    let builtin = gtk::CssProvider::new();
    builtin.load_from_string(DEFAULT_CSS);
    gtk::style_context_add_provider_for_display(&display, &builtin, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);

    let user = glib::user_config_dir().join("omni").join("style.css");
    if user.exists() {
        let custom = gtk::CssProvider::new();
        custom.load_from_path(&user);
        gtk::style_context_add_provider_for_display(&display, &custom, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);
    }
}

fn build_ui(app: &gtk::Application) -> Rc<Ui> {
    load_css();

    // The surface covers the whole screen; everything outside the card is transparent
    // and closes the popup when clicked (like rofi).
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("omni")
        .css_classes(["omni"])
        .build();

    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_namespace(Some("omni"));
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_exclusive_zone(-1); // cover bars too, don't get pushed around by them

    let entry = gtk::SearchEntry::builder()
        .placeholder_text(SEARCH_PLACEHOLDER)
        .hexpand(true)
        .search_delay(0)
        .css_classes(["omni-search"])
        .build();
    // No clear ("erase") button: Esc/backspace do that. It's the entry's last child; GTK
    // only toggles its child-visibility, so hiding the widget itself sticks.
    if let Some(clear) = entry.last_child().filter(|c| c.is::<gtk::Image>()) {
        clear.set_visible(false);
    }
    let contents = gtk::ToggleButton::builder()
        .label("Contents")
        .tooltip_text("Also match text inside files (Ctrl+T)")
        .css_classes(["omni-toggle"])
        .valign(gtk::Align::Center)
        .focus_on_click(false)
        .build();

    let filter_label = gtk::Label::new(Some("All"));
    let filter_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    filter_content.set_halign(gtk::Align::Center);
    filter_content.append(&filter_label);
    filter_content.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
    let filter_button = gtk::Button::builder()
        .child(&filter_content)
        .tooltip_text("Show only some kinds of results (Ctrl+F)")
        .css_classes(["omni-toggle", "omni-filter"])
        .valign(gtk::Align::Center)
        .focus_on_click(false)
        .build();

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    bar.add_css_class("omni-bar");
    // Overlay kept for layout stability; SearchRing currently draws nothing (flat search).
    let search = gtk::Overlay::builder().child(&entry).hexpand(true).build();
    search.add_overlay(&ring::SearchRing::new());
    bar.append(&search);
    bar.append(&filter_button);
    bar.append(&contents);

    let list = gtk::ListBox::new();
    // Selection is owned by Ui::selected_index — GTK must not clear it on outside clicks.
    list.set_selection_mode(gtk::SelectionMode::None);
    list.set_can_focus(false); // keep keyboard focus on the search field
    list.add_css_class("omni-results");

    // Floating selection chrome: fill + pill are overlay siblings so scaleY on the fill
    // never scales the pill. Both sit at top:0; motion is CSS transform only.
    let selection_fill = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    selection_fill.add_css_class("omni-selection");
    selection_fill.set_can_target(false);
    selection_fill.set_can_focus(false);
    selection_fill.set_visible(false);
    let selection_pill = gtk::Box::new(gtk::Orientation::Vertical, 0);
    selection_pill.add_css_class("omni-selection-pill");
    selection_pill.set_can_target(false);
    selection_pill.set_can_focus(false);
    selection_pill.set_visible(false);
    selection_pill.set_size_request(SEL_PILL_W, SEL_PILL_H as i32);

    let sel_transform_css = gtk::CssProvider::new();
    let sel_transition_css = gtk::CssProvider::new();
    let display = gdk::Display::default().expect("no display");
    gtk::style_context_add_provider_for_display(
        &display,
        &sel_transition_css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &sel_transform_css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 3,
    );

    let list_overlay = gtk::Overlay::builder().child(&list).build();
    list_overlay.add_overlay(&selection_fill);
    list_overlay.add_overlay(&selection_pill);

    // Results slide open below the search bar as soon as there is something to show.
    // Long lists scroll inside the card instead of running off the screen (height capped
    // per monitor when the popup opens, see fit_to_monitor).
    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        // Scrolls (wheel, keys, selection) but never shows a scrollbar.
        .vscrollbar_policy(gtk::PolicyType::External)
        .propagate_natural_height(true)
        .max_content_height(MAX_VISIBLE_ROWS * ROW_HEIGHT)
        .child(&list_overlay)
        .build();
    let reveal = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .transition_duration(220)
        .child(&list_scroll)
        .build();

    // Card = blurred backdrop (GlassCard) + metal frame + glass face + content.
    // Face = content + path strip edge-to-edge inside the frame (mockup vessel).
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("omni-content");
    content.append(&bar);
    content.append(&reveal);
    let surface = gtk::Box::new(gtk::Orientation::Vertical, 0);
    surface.add_css_class("omni-glass");
    surface.append(&content);

    // Claude's answer replaces the result list while it's showing.
    let answer = answer::Answer::new();
    let answer_reveal = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .transition_duration(220)
        .child(&answer.panel)
        .build();
    answer_reveal.add_css_class("omni-answer-wrap");
    content.append(&answer_reveal);

    // A service's status (`systemctl status`), shown in place of the results.
    let status_title = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).max_width_chars(1).build();
    status_title.add_css_class("omni-answer-title");
    let status_hint = gtk::Label::new(Some("Esc to go back"));
    status_hint.add_css_class("omni-status-hint");
    let status_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let status_icon = gtk::Image::from_icon_name("system-run-symbolic");
    status_header.append(&status_icon);
    status_header.append(&status_title);
    status_header.append(&status_hint);
    let status_body = gtk::Label::builder()
        .xalign(0.0)
        .yalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .max_width_chars(1)
        .hexpand(true)
        .build();
    status_body.add_css_class("omni-status-body");
    let status_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .propagate_natural_height(true)
        .max_content_height(460)
        .child(&status_body)
        .build();
    let status_panel = gtk::Box::new(gtk::Orientation::Vertical, 10);
    status_panel.add_css_class("omni-answer");
    status_panel.append(&status_header);
    status_panel.append(&status_scroll);
    let status_reveal = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .transition_duration(220)
        .child(&status_panel)
        .build();
    status_reveal.add_css_class("omni-answer-wrap");
    content.append(&status_reveal);

    // Footer: full path of the selected result (rows keep a shortened secondary).
    // Truncation is measured manually — no toolkit ellipsize / max-chars (those cut early).
    let path_label = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::None)
        .single_line_mode(true)
        .hexpand(true)
        .halign(gtk::Align::Fill)
        .build();
    path_label.add_css_class("omni-path-text");
    let path_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    path_box.add_css_class("omni-path");
    path_box.set_hexpand(true);
    path_box.set_valign(gtk::Align::Center);
    path_box.set_size_request(-1, PATH_FOOTER_HEIGHT);
    path_box.append(&path_label);
    let path_reveal = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::None)
        .transition_duration(0)
        .reveal_child(false)
        .child(&path_box)
        .build();
    surface.append(&path_reveal);

    let launcher_plane = gtk::Overlay::builder().child(&surface).build();
    let settings_button = gtk::Button::builder()
        .icon_name("preferences-system-symbolic")
        .tooltip_text("Settings")
        .css_classes(["flat", "omni-settings-button"])
        .focus_on_click(false)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .build();
    let settings_slip = gtk::Overlay::new();
    settings_slip.add_css_class("omni-settings-slip");
    settings_slip.set_halign(gtk::Align::End);
    settings_slip.set_valign(gtk::Align::Start);
    settings_slip.set_size_request(44, 44);
    settings_slip.set_margin_top(0);
    settings_slip.set_overflow(gtk::Overflow::Hidden);
    settings_slip.set_child(Some(&settings_button));
    settings_button.set_can_target(false);
    let corner_hover = gtk::EventControllerMotion::new();
    let gear = settings_button.clone();
    corner_hover.connect_enter(move |_, _, _| gear.set_can_target(true));
    let gear = settings_button.clone();
    corner_hover.connect_leave(move |_| gear.set_can_target(false));
    settings_slip.add_controller(corner_hover);
    launcher_plane.add_overlay(&settings_slip);

    let back_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    back_content.append(&gtk::Image::from_icon_name("go-previous-symbolic"));
    back_content.append(&gtk::Label::new(Some("Back")));
    let settings_back = gtk::Button::builder()
        .child(&back_content)
        .css_classes(["flat", "omni-settings-back"])
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .focus_on_click(false)
        .build();
    let settings_plane = gtk::Box::new(gtk::Orientation::Vertical, 0);
    settings_plane.add_css_class("omni-glass");
    settings_plane.add_css_class("omni-settings-plane");
    settings_plane.set_vexpand(true);
    settings_plane.append(&settings_back);
    let provider = load_search_provider();
    let search_settings = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    search_settings.add_css_class("omni-search-settings");
    let search_label = gtk::Label::builder().label("Search engine").xalign(0.0).hexpand(true).build();
    let names: Vec<_> = search_provider::SearchProvider::ALL.iter().map(|p| p.name()).collect();
    let engine_dropdown = gtk::DropDown::from_strings(&names);
    engine_dropdown.set_selected(search_provider::SearchProvider::ALL.iter().position(|p| *p == provider).unwrap_or(0) as u32);
    search_settings.append(&search_label);
    search_settings.append(&engine_dropdown);
    settings_plane.append(&search_settings);
    let search_view = search_view::SearchView::new();

    let main_stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(160)
        .hhomogeneous(true)
        .vhomogeneous(false)
        .build();
    main_stack.add_named(&launcher_plane, Some(UiMode::Launcher.stack_name()));
    main_stack.add_named(&settings_plane, Some(UiMode::Settings.stack_name()));
    main_stack.add_named(&search_view.root, Some(UiMode::WebSearch.stack_name()));
    main_stack.set_visible_child_name(UiMode::Launcher.stack_name());

    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("omni-frame");
    frame.append(&main_stack);

    let card = glass::GlassCard::new();
    card.add_css_class("omni-card");
    card.set_halign(gtk::Align::Center);
    card.set_valign(gtk::Align::Start);
    card.set_width_request(CARD_WIDTH);
    card.set_margin_top(CARD_TOP);
    // A dimmed layer over the whole card for confirmations (see confirm_link): the box sits
    // on top of omni, in its centre.
    let confirm_text = gtk::Label::builder().xalign(0.0).wrap(true).max_width_chars(1).hexpand(true).build();
    confirm_text.add_css_class("omni-confirm-text");
    let confirm_link_label = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .max_width_chars(1)
        .hexpand(true)
        .build();
    confirm_link_label.add_css_class("omni-confirm-link");
    let confirm_yes = gtk::Button::builder().label("Yes").css_classes(["omni-answer-button", "omni-confirm-yes"]).build();
    let confirm_no = gtk::Button::builder().label("No").css_classes(["omni-answer-button"]).build();
    let confirm_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    confirm_buttons.set_halign(gtk::Align::End);
    // Third answer, for permission prompts only ("Always allow").
    let confirm_always = gtk::Button::builder().label("Always allow").css_classes(["omni-answer-button"]).visible(false).build();
    confirm_buttons.append(&confirm_no);
    confirm_buttons.append(&confirm_always);
    confirm_buttons.append(&confirm_yes);
    let confirm_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    confirm_box.add_css_class("omni-confirm-box");
    confirm_box.set_halign(gtk::Align::Center);
    confirm_box.set_valign(gtk::Align::Center);
    confirm_box.set_width_request(320);
    confirm_box.append(&confirm_text);
    // The detail (a whole command or file content) scrolls rather than being cut short.
    let confirm_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(220)
        .child(&confirm_link_label)
        .build();
    confirm_box.append(&confirm_scroll);
    confirm_box.append(&confirm_buttons);
    let confirm = gtk::Box::new(gtk::Orientation::Vertical, 0);
    confirm.add_css_class("omni-scrim");
    confirm.append(&confirm_box);
    confirm_box.set_vexpand(true);
    confirm.set_visible(false);
    let layers = gtk::Overlay::builder().child(&frame).build();
    layers.add_overlay(&confirm);
    card.append(&layers);
    window.set_child(Some(&card));

    let ui = Rc::new(Ui {
        window: window.clone(),
        card: card.clone(),
        main_stack: main_stack.clone(),
        mode: Cell::new(UiMode::default()),
        settings_back: settings_back.clone(),
        search_provider: Cell::new(provider),
        search_view,
        entry: entry.clone(),
        list: list.clone(),
        list_scroll: list_scroll.clone(),
        list_overlay: list_overlay.clone(),
        selection_fill: selection_fill.clone(),
        selection_pill: selection_pill.clone(),
        sel_transform_css,
        sel_transition_css,
        selected_index: Cell::new(0),
        sel_y: Cell::new(0.0),
        sel_h: Cell::new(SEL_BASE_H),
        row_geom: RefCell::default(),
        is_snapping: Cell::new(true),
        sel_transition_on: Cell::new(false),
        sel_layout_pending: Cell::new(false),
        sel_needs_replace: Cell::new(false),
        sel_update_gen: Cell::new(0),
        sel_paint_frames: Cell::new(0),
        open_t0: Cell::new(None),
        sel_place_gen: Cell::new(0),
        place_while_hidden: Cell::new(false),
        hover_suppress_until: Cell::new(0),
        hover_pointer: Cell::new((f64::NAN, f64::NAN)),
        hover_frame_pending: Cell::new(false),
        hover_pending_index: Cell::new(None),
        reveal: reveal.clone(),
        apps: RefCell::new(apps::Apps::load()),
        web: RefCell::new(web::Engine::detect()),
        hits: RefCell::default(),
        row_keys: RefCell::default(),
        answer,
        answer_reveal,
        me: RefCell::default(),
        files: files::Shared::default(),
        sparql: RefCell::default(),
        notifier: RefCell::default(),
        contents: contents.clone(),
        filter_button: filter_button.clone(),
        filter: Cell::new(load_filter_setting()),
        content: RefCell::default(),
        content_gen: Cell::new(0),
        content_cancel: RefCell::new(Arc::new(AtomicBool::new(false))),
        content_rg: Arc::new(Mutex::new(None)),
        path_reveal: path_reveal.clone(),
        path_box: path_box.clone(),
        path_label: path_label.clone(),
        path_full: RefCell::default(),
        path_trunc_cache: RefCell::default(),
        hover_gen: Cell::new(0),
        shown_query: RefCell::default(),
        heartbeat: RefCell::default(),
        menu: RefCell::default(),
        chatting: Cell::new(false),
        confirm: confirm.clone(),
        confirm_text: confirm_text.clone(),
        confirm_link_label,
        confirm_action: RefCell::default(),
        confirm_no_action: RefCell::default(),
        confirm_always_action: RefCell::default(),
        confirm_yes: confirm_yes.clone(),
        confirm_no: confirm_no.clone(),
        confirm_always: confirm_always.clone(),
        confirm_shown_at: Cell::new(0),
        confirm_scroll: confirm_scroll.clone(),
        services: RefCell::default(),
        services_loading: Cell::new(false),
        status_reveal: status_reveal.clone(),
        status_title,
        status_body,
        status_scroll,
        status_gen: Cell::new(0),
    });
    *ui.me.borrow_mut() = Rc::downgrade(&ui);

    // Re-measure footer truncation whenever the strip's width changes.
    let u = ui.clone();
    ui.path_box.connect_notify_local(Some("width"), move |_, _| {
        apply_path_display(&u);
    });

    let u = ui.clone();
    confirm_yes.connect_clicked(move |_| finish_confirm(&u, Answered::Yes));
    let u = ui.clone();
    confirm_always.connect_clicked(move |_| finish_confirm(&u, Answered::Always));
    let u = ui.clone();
    confirm_no.connect_clicked(move |_| close_confirm(&u));

    let u = ui.clone();
    settings_back.connect_clicked(move |_| show_launcher(&u));
    let u = ui.clone();
    ui.search_view.back.connect_clicked(move |_| show_launcher(&u));
    let u = ui.clone();
    engine_dropdown.connect_selected_notify(move |dropdown| {
        if let Some(&provider) = search_provider::SearchProvider::ALL.get(dropdown.selected() as usize) {
            u.search_provider.set(provider);
            if let Err(error) = save_search_provider(provider) {
                notify_user("Couldn’t save search engine", &error.to_string());
            }
        }
    });
    let u = ui.clone();
    settings_button.connect_clicked(move |_| show_settings(&u));

    // Claude Code asking permission (a command, an edit, a web page ...).
    let weak = Rc::downgrade(&ui);
    ui.answer.set_permission_handler(move |ask| {
        if let Some(ui) = weak.upgrade() {
            ask_permission(&ui, ask);
        }
    });
    // ... and the prompt going stale (Claude stopped): just take it away.
    let weak = Rc::downgrade(&ui);
    ui.answer.set_permission_gone_handler(move || {
        if let Some(ui) = weak.upgrade() {
            dismiss_confirm(&ui);
        }
    });
    // Clicking the dimmed area around the box is No.
    let dim_click = gtk::GestureClick::new();
    let (u, cb) = (ui.clone(), confirm_box.clone());
    dim_click.connect_pressed(move |_, _, x, y| {
        let on_box = cb.compute_bounds(&u.confirm).is_some_and(|b| b.contains_point(&gtk::graphene::Point::new(x as f32, y as f32)));
        if !on_box {
            close_confirm(&u);
        }
    });
    confirm.add_controller(dim_click);

    // Model and effort chip in Claude's header.
    let u = ui.clone();
    ui.answer.chip.connect_clicked(move |_| show_model_menu(&u));

    // A link in Claude's answer: opening it closes omni (and its temporary conversation),
    // so ask first.
    let weak = Rc::downgrade(&ui);
    ui.answer.set_link_handler(move |uri| {
        if let Some(ui) = weak.upgrade() {
            confirm_link(&ui, uri);
        }
    });

    // File names: omni's own index, walked in the background (see files.rs).
    files::start_walker(ui.files.clone(), REWALK);
    watchdog::start();
    connect_localsearch(&ui);

    // Contents toggle: remembered between opens; flipping it invalidates cache and re-runs.
    // Defer all side effects to idle so the ToggleButton gesture can finish (sync refresh
    // + CSS snaps from inside `toggled` froze the control).
    contents.set_active(load_contents_setting());
    let u = ui.clone();
    contents.connect_toggled(move |b| {
        let on = b.is_active();
        save_contents_setting(on);
        // Always cancel + clear searching/pending (Idle) so the footer never sticks.
        invalidate_content_cache(&u);
        contents_log(format!("toggle → {on}; cache cleared"));
        let u = u.clone();
        glib::idle_add_local_once(move || {
            if u.answer_reveal.reveals_child() {
                return;
            }
            let q = u.entry.text();
            let q = q.trim().to_string();
            let (search_q, scope) = content_search_args(&q);
            if on && !search_q.is_empty() {
                start_content_search(&u, &search_q, scope.as_deref(), Instant::now());
            }
            refresh(&u);
            u.entry.grab_focus();
        });
    });

    // Filter dropdown: remembered between opens.
    filter_label.set_label(&filter_name(ui.filter.get()));
    let u = ui.clone();
    filter_button.connect_clicked(move |_| {
        show_filter_menu(&u, false);
        u.entry.grab_focus();
    });

    // Re-read apps when something is installed or removed. GIO stops sending "changed"
    // once the last reference to the monitor is gone, so it is kept for the process.
    let weak = Rc::downgrade(&ui);
    let app_monitor = gio::AppInfoMonitor::get();
    app_monitor.connect_changed(move |_| {
        probe::log("apps-changed signal");
        if let Some(ui) = weak.upgrade() {
            let t_reload = Instant::now();
            ui.apps.borrow_mut().reload();
            probe::log(format!("apps reloaded ({:.1}ms)", t_reload.elapsed().as_secs_f64() * 1000.0));
            if ui.window.is_visible() && !ui.chatting.get() && !ui.entry.text().trim().is_empty() {
                refresh(&ui);
            }
        }
    });
    std::mem::forget(app_monitor);

    let u = ui.clone();
    entry.connect_search_changed(move |_| {
        probe::log(format!("keystroke {:?}", u.entry.text().to_string()));
        sel_log(format!(
            "keystroke text={:?} selectedIndex={}",
            u.entry.text().to_string(),
            u.selected_index.get()
        ));
        // Typing with a menu open: dismiss first (launcher feel), then refresh.
        if u.menu.borrow().is_some() {
            dismiss_menu(&u);
        }
        refresh(&u);
        probe::log("keystroke refresh done");
        sel_log_state(&u, "after_keystroke_refresh");
    });

    let u = ui.clone();
    entry.connect_activate(move |_| {
        if u.menu.borrow().is_some() {
            return; // menu layer owns Enter; don't activate the selected row
        }
        if u.chatting.get() {
            return send_reply(&u);
        }
        if u.answer_reveal.reveals_child() || u.status_reveal.reveals_child() {
            return; // answer or status showing: Enter has nothing to pick
        }
        if !u.hits.borrow().is_empty() {
            activate(&u, u.selected_index.get());
        }
    });

    // Act on the next idle tick: activating rebuilds this list, which shouldn't happen
    // inside the list's own signal. Capture the row widget so a refresh that rebuilds
    // the list before idle runs can't activate a stale index.
    let u = ui.clone();
    list.connect_row_activated(move |_, row| {
        if u.menu.borrow().is_some() {
            return;
        }
        let index = row.index() as usize;
        set_selected_index(&u, index, false);
        let (u, row) = (u.clone(), row.clone());
        glib::idle_add_local_once(move || {
            if u.list.row_at_index(row.index()).as_ref() != Some(&row) {
                return;
            }
            activate(&u, index);
        });
    });

    // Hover moves selection (same chrome as keyboard); leaving the list keeps the last index.
    // Throttle to one update per frame; ignore still-cursor synthetics and same-row noise.
    let hover = gtk::EventControllerMotion::new();
    let u = ui.clone();
    hover.connect_motion(move |_, x, y| {
        if u.menu.borrow().is_some() {
            return;
        }
        // After show: ignore stationary pointer / synthetic enters until suppress ends
        // and a real move is seen (hover_pointer starts as NaN on each open).
        let now = glib::monotonic_time();
        if now < u.hover_suppress_until.get() {
            return;
        }
        let (px, py) = u.hover_pointer.get();
        if !px.is_finite() || !py.is_finite() {
            // First motion after suppress: record position, do not change selection.
            u.hover_pointer.set((x, y));
            return;
        }
        if (px - x).abs() < 0.5 && (py - y).abs() < 0.5 {
            return;
        }
        u.hover_pointer.set((x, y));
        let Some(row) = u.list.row_at_y(y as i32) else {
            return;
        };
        let index = row.index() as usize;
        if index == u.selected_index.get() {
            return;
        }
        u.hover_pending_index.set(Some(index));
        if u.hover_frame_pending.replace(true) {
            return;
        }
        let weak = u.me.borrow().clone();
        u.list_overlay.add_tick_callback(move |_, _| {
            let Some(ui) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            ui.hover_frame_pending.set(false);
            if glib::monotonic_time() < ui.hover_suppress_until.get() {
                return glib::ControlFlow::Break;
            }
            if let Some(idx) = ui.hover_pending_index.take() {
                if idx != ui.selected_index.get() {
                    set_selected_index(&ui, idx, true);
                }
            }
            glib::ControlFlow::Break
        });
    });
    list.add_controller(hover);

    // Layout slot is always top:0 with fixed sizes — transforms drive the visible move.
    ui.list_overlay.connect_get_child_position(move |overlay, widget| {
        let w = overlay.width().max(1);
        if widget.has_css_class("omni-selection") {
            return Some(gdk::Rectangle::new(0, 0, w, SEL_BASE_H as i32));
        }
        if widget.has_css_class("omni-selection-pill") {
            return Some(gdk::Rectangle::new(SEL_PILL_X, 0, SEL_PILL_W, SEL_PILL_H as i32));
        }
        None
    });

    // Overlay-local row geometry is unchanged by scroll; don't touch chrome (avoids snap stutter).
    let u = ui.clone();
    ui.list_overlay.connect_map(move |_| {
        cache_row_geometry(&u);
        snap_selection_chrome(&u);
    });
    // Resize only — skip duplicate notifies and never snap while CSS from the last snap
    // is still settling (that loop froze the whole UI, including the Contents toggle).
    let overlay_wh = Cell::new((0i32, 0i32));
    for prop in ["width", "height"] {
        let u = ui.clone();
        let overlay_wh = overlay_wh.clone();
        ui.list_overlay.connect_notify_local(Some(prop), move |overlay, _| {
            let wh = (overlay.width(), overlay.height());
            if overlay_wh.replace(wh) == wh {
                return;
            }
            if u.is_snapping.get() || u.sel_layout_pending.get() {
                u.sel_needs_replace.set(true);
                return;
            }
            cache_row_geometry(&u);
            snap_selection_chrome(&u);
        });
    }
    let u = ui.clone();
    ui.list.connect_notify_local(Some("scale-factor"), move |_, _| {
        if u.is_snapping.get() || u.sel_layout_pending.get() {
            u.sel_needs_replace.set(true);
            return;
        }
        cache_row_geometry(&u);
        snap_selection_chrome(&u);
    });

    // Right-click on the window (capture). Menus use autohide=false so GTK/Wayland
    // don't take a pointer grab that leaves the next click at the same point dead
    // until the cursor moves. We dismiss ourselves on a second right-click or a
    // primary click outside the menu.
    let right_click = gtk::EventControllerLegacy::new();
    right_click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    right_click.connect_event(move |_, event| {
        let Some(button) = event.downcast_ref::<gdk::ButtonEvent>() else {
            return glib::Propagation::Proceed;
        };
        if event.event_type() != gdk::EventType::ButtonPress {
            return glib::Propagation::Proceed;
        }
        let Some((x, y)) = event.position() else {
            return glib::Propagation::Proceed;
        };
        let secondary = button.button() == gdk::BUTTON_SECONDARY;
        let primary = button.button() == gdk::BUTTON_PRIMARY;

        if u.menu.borrow().is_some() {
            if secondary || (primary && !click_inside_menu(&u, x, y)) {
                // Menu is a layer: outside click dismisses only — don't activate
                // the list row (or anything else) underneath.
                dismiss_menu(&u);
                return glib::Propagation::Stop;
            }
            // Primary inside the menu: let the menu item handle it.
            return glib::Propagation::Proceed;
        }

        if !secondary {
            return glib::Propagation::Proceed;
        }
        let Some((index, at)) = row_menu_target(&u, x, y) else {
            return glib::Propagation::Proceed;
        };
        if open_row_menu(&u, index, Some(at)) {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(right_click);

    // Esc hides, ↑/↓ move through results while typing stays in the entry, Ctrl+T flips Contents.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    keys.connect_key_pressed(move |_, key, _, mods| {
        // Menu is a layer: Esc dismisses it (don't hide the window); Enter activates the
        // focused menu item if any, else dismisses. Don't fall through to row activate.
        if u.menu.borrow().is_some() {
            match key {
                gdk::Key::Escape => {
                    dismiss_menu(&u);
                    return glib::Propagation::Stop;
                }
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    if let Some(focused) =
                        gtk::prelude::GtkWindowExt::focus(&u.window).and_downcast::<gtk::Button>()
                    {
                        focused.emit_clicked();
                    } else {
                        dismiss_menu(&u);
                    }
                    return glib::Propagation::Stop;
                }
                gdk::Key::Menu => return glib::Propagation::Stop,
                gdk::Key::F10 if mods.contains(gdk::ModifierType::SHIFT_MASK) => {
                    return glib::Propagation::Stop;
                }
                // ↑/↓ Proceed so filter/model menus can move focus; we never reach
                // move_selection while the menu is open.
                _ => return glib::Propagation::Proceed,
            }
        }
        // Link confirmation up: Esc = No, Enter = Yes, Tab moves between them.
        if u.confirm.is_visible() {
            match key {
                gdk::Key::Escape => close_confirm(&u),
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    // The focused button; with none, the safe answer (No / Deny).
                    if let Some(focused) = gtk::prelude::GtkWindowExt::focus(&u.window).and_downcast::<gtk::Button>() {
                        focused.emit_clicked();
                    } else {
                        close_confirm(&u);
                    }
                }
                _ => return glib::Propagation::Proceed,
            }
            return glib::Propagation::Stop;
        }
        if key == gdk::Key::Escape && u.mode.get().consumes_escape() {
            show_launcher(&u);
            return glib::Propagation::Stop;
        }
        if u.mode.get() != UiMode::Launcher {
            return glib::Propagation::Proceed;
        }
        match key {
            // In a conversation, Esc goes back to searching; otherwise it closes.
            gdk::Key::Escape if u.chatting.get() => end_chat(&u),
            // Showing a service's status: back to the results.
            gdk::Key::Escape if u.status_reveal.reveals_child() => refresh(&u),
            gdk::Key::Escape => hide(&u),
            gdk::Key::Menu => open_selected_menu(&u),
            gdk::Key::F10 if mods.contains(gdk::ModifierType::SHIFT_MASK) => open_selected_menu(&u),
            // Shift+Enter: ask Claude, whatever is selected.
            gdk::Key::Return | gdk::Key::KP_Enter if mods.contains(gdk::ModifierType::SHIFT_MASK) => {
                if u.chatting.get() {
                    send_reply(&u);
                } else {
                    let query = u.entry.text().trim().to_string();
                    if !query.is_empty() {
                        run(&u, Action::Claude(query));
                    }
                }
            }
            // Ctrl+Enter: always search the web, whatever is selected.
            gdk::Key::Return | gdk::Key::KP_Enter if mods.contains(gdk::ModifierType::CONTROL_MASK) => {
                let query = u.entry.text().trim().to_string();
                if !query.is_empty() {
                    run(&u, Action::Web(query));
                }
            }
            gdk::Key::Down => move_selection(&u, 1),
            gdk::Key::Up => move_selection(&u, -1),
            gdk::Key::t | gdk::Key::T if mods.contains(gdk::ModifierType::CONTROL_MASK) => {
                u.contents.set_active(!u.contents.is_active());
            }
            gdk::Key::f if mods.contains(gdk::ModifierType::CONTROL_MASK) => show_filter_menu(&u, true),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(keys);

    // Scroll wheel / touchpad moves the selection like ↑/↓. Convert surface pixels to
    // row-sized steps; wheel units are already detents. With a Claude answer showing, the
    // scroll goes to the answer text instead.
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    // Before the list's own scroller sees it: the wheel moves the selection, and the list
    // follows the selection.
    scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    let pending = Rc::new(Cell::new(0.0f64));
    for begin in [true, false] {
        let pending = pending.clone();
        if begin {
            scroll.connect_scroll_begin(move |_| pending.set(0.0));
        } else {
            scroll.connect_scroll_end(move |_| pending.set(0.0));
        }
    }
    let last_unit = Cell::new(gdk::ScrollUnit::Wheel);
    scroll.connect_scroll(move |controller, _, dy| {
        if u.mode.get() != UiMode::Launcher || u.answer_reveal.reveals_child() || u.status_reveal.reveals_child() || u.menu.borrow().is_some() {
            return glib::Propagation::Proceed;
        }
        let unit = controller.unit();
        if last_unit.replace(unit) != unit {
            pending.set(0.0);
        }
        let steps = selection_scroll_steps(&pending, dy, unit);
        // Jump straight to the event's final row: no intermediate CSS/layout work
        // or redundant idle callbacks, including when already at a list boundary.
        move_selection(&u, steps);
        glib::Propagation::Stop
    });
    window.add_controller(scroll);

    // Primary click outside the card closes the popup. (Not button 0 — that also
    // claimed right-clicks and fought the context menu.)
    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    let u = ui.clone();
    click.connect_pressed(move |_, _, x, y| {
        if u.menu.borrow().is_some() {
            return;
        }
        let inside = u
            .card
            .compute_bounds(&u.window)
            .is_some_and(|b| b.contains_point(&gtk::graphene::Point::new(x as f32, y as f32)));
        if !inside {
            hide(&u);
        }
    });
    window.add_controller(click);

    ui
}

/// Recompute results for the current query and rebuild the list.
fn refresh(ui: &Ui) {
    // In a conversation the bar is the reply box: typing there is a reply, not a search.
    if ui.chatting.get() {
        return;
    }
    // Typing again leaves the answer (stopping Claude) or a service's status.
    close_answer(ui);
    close_status(ui);
    let query = ui.entry.text();
    let query = query.trim();

    let mut hits = Vec::new();
    if query.is_empty() {
        hits = recent_hits(ui);
    } else if let Some(calc) = calc_hit(query) {
        // A complete calculation: the result is the only thing shown.
        hits.push(calc);
    } else if let Some((dir, inner)) = parse_scope(query) {
        // Don't require dir.is_dir(): ~/music must scope when the folder is ~/Music.
        // Name search and Contents FTS are independent filters (OR for match kind, AND scope).
        hits = scoped_hits(ui, &dir, inner);
        let show = ui.filter.get();
        if ui.contents.is_active() && show & SHOW_FILES != 0 && !inner.is_empty() {
            merge_content_hits(ui, inner, Some(dir.as_path()), &mut hits);
            sort_file_hits_by_match_type(&mut hits);
            promote_content_matches(&mut hits);
        }
    } else {
        // Fixed group order: app actions, apps, files, folders, file contents, web search,
        // Ask Claude. The web row is first only when nothing above it matches.
        // Actions above the apps only when the query also names their app ("brave tor",
        // "claude chat"). One that matches on its own words alone ("chat" → Claude's New chat)
        // comes after the apps matched by name (ChatGPT), but before keyword/loose guesses
        // ("private" → private windows above "Print Editor").
        // The filter drops whole kinds; a narrowed filter gives each shown kind more room.
        let show = ui.filter.get();
        let roomy = show != SHOW_ALL;
        if show & SHOW_APPS != 0 {
            let mut later = Vec::new();
            for (app, action, names_app) in ui.apps.borrow().search_actions(query, MAX_ACTIONS) {
                if names_app { hits.push(action_hit(app, action)) } else { later.push(action_hit(app, action)) }
            }
            let max_apps = if roomy { MAX_FILTERED } else { MAX_APPS };
            for (app, _score, by_name) in ui.apps.borrow_mut().search(query, max_apps) {
                if !by_name {
                    hits.append(&mut later);
                }
                hits.push(app_hit(&app.info, &app.name));
            }
            hits.append(&mut later);
        }
        // systemd services by name ("bluetooth"): selecting only selects; actions are on right-click.
        if show & SHOW_SERVICES != 0 {
            let max = if roomy { MAX_FILTERED } else { MAX_SERVICES };
            for s in services::search(&ui.services.borrow(), query, max) {
                hits.push(service_hit(s));
            }
        }
        // File and folder names: instant (omni's own index, a few ms).
        let max_files = if show & SHOW_FILES == 0 { 0 } else if roomy { MAX_FILTERED } else { MAX_FILES };
        let max_dirs = if show & SHOW_FOLDERS == 0 { 0 } else if roomy { MAX_FILTERED } else { MAX_DIRS };
        let (mut file_hits, mut dir_hits) = if max_files + max_dirs == 0 {
            (Vec::new(), Vec::new()) // Apps only: skip the file scan entirely
        } else {
            files::search(&ui.files, query, max_files.max(1), max_dirs.max(1), None)
        };
        file_hits.truncate(max_files);
        dir_hits.truncate(max_dirs);
        // A matching home folder (~/Documents, ...) goes above the files; other folders
        // keep their place after them.
        let (home_dirs, other_dirs): (Vec<_>, Vec<_>) = dir_hits.into_iter().partition(|h| h.home_folder);
        for h in home_dirs.into_iter().chain(file_hits).chain(other_dirs) {
            hits.push(file_hit(&h.path, h.is_dir, MatchType::Name, None, query));
        }
        // File contents (LocalSearch FTS + ripgrep for git trees): merge with name hits.
        if ui.contents.is_active() && show & SHOW_FILES != 0 {
            merge_content_hits(ui, query, None, &mut hits);
            sort_file_hits_by_match_type(&mut hits);
            promote_content_matches(&mut hits);
        } else {
            sort_file_hits_by_match_type(&mut hits);
        }
        hits.push(web_hit(ui, query));
        hits.extend(claude_hit(ui, query));
    }

    // Results filling in for the same query keep the same result (by stable id). A new
    // query re-ranks everything, so the best match (row 0) is selected: following the old
    // pick let a weaker match stay selected below better ones that just appeared.
    let prev_index = ui.selected_index.get();
    let same_query = *ui.shown_query.borrow() == query;
    let prev_key = same_query
        .then(|| ui.hits.borrow().get(prev_index).map(hit_key))
        .flatten();
    let chrome_was_shown =
        ui.selection_fill.is_visible() && ui.selection_fill.opacity() >= 1.0 && !ui.hits.borrow().is_empty();
    *ui.shown_query.borrow_mut() = query.to_string();

    let next = prev_key
        .as_ref()
        .and_then(|k| hits.iter().position(|h| &hit_key(h) == k))
        .unwrap_or(0);
    let keep_scroll = prev_key
        .as_ref()
        .is_some_and(|k| hits.iter().any(|h| &hit_key(h) == k));
    // Preserve the visible position during the diff when the selected slot stays
    // put. Its actual allocation is still reconciled after GTK lays out the rows.
    let same_selection = chrome_was_shown && next == prev_index;

    // Invalidate hover targets; do NOT hide chrome (that caused the typing blink).
    ui.hover_gen.set(ui.hover_gen.get() + 1);

    arm_sel_paint_count(ui);
    apply_hits(ui, &hits);

    if !keep_scroll {
        ui.list_scroll.vadjustment().set_value(0.0);
    }
    let has = !hits.is_empty();
    if ui.reveal.reveals_child() != has {
        ui.reveal.set_reveal_child(has);
    }
    // Footer keeps a fixed height whenever the list is shown so the panel doesn't jump.
    if ui.path_reveal.reveals_child() != has {
        ui.path_reveal.set_reveal_child(has);
    }
    *ui.hits.borrow_mut() = hits;
    if has {
        ui.selected_index.set(next.min(ui.hits.borrow().len() - 1));
        sel_log(format!(
            "refresh results count={} selectedIndex={} id={:?} keep_scroll={keep_scroll} same_selection={same_selection}",
            ui.hits.borrow().len(),
            ui.selected_index.get(),
            ui.hits.borrow().get(ui.selected_index.get()).map(hit_key),
        ));
        update_path_footer(ui, Some(ui.selected_index.get()));
        finish_results_selection_visual(ui, same_selection, chrome_was_shown);
        if next > 0 && keep_scroll {
            let weak = ui.me.borrow().clone();
            let idx = ui.selected_index.get();
            glib::idle_add_local_once(move || {
                let Some(ui) = weak.upgrade() else { return };
                let weak = ui.me.borrow().clone();
                ui.list_overlay.add_tick_callback(move |_, _| {
                    if let Some(ui) = weak.upgrade() {
                        scroll_to_cached(&ui, idx);
                    }
                    glib::ControlFlow::Break
                });
            });
        }
    } else {
        ui.selected_index.set(0);
        ui.sel_layout_pending.set(false);
        ui.row_geom.borrow_mut().clear();
        hide_selection_chrome(ui);
        ui.is_snapping.set(false);
        if motion_enabled() {
            set_sel_transition_enabled(ui, true);
        }
        update_path_footer(ui, None);
        sel_log("refresh empty → hide chrome");
    }
}

/// Keep existing chrome steady during a diff, then reconcile every slot after layout.
fn finish_results_selection_visual(ui: &Ui, same_selection: bool, chrome_was_shown: bool) {
    ui.sel_layout_pending.set(true);
    if !same_selection {
        ui.is_snapping.set(true);
        set_sel_transition_enabled(ui, false);
        // A reused row can still report its previous allocation here. Use the fixed
        // slot as a provisional position; the deferred snap measures the real bounds.
        if let Some((y, h)) = estimate_sel_geom(ui) {
            apply_sel_transform(ui, y, h);
            show_selection_chrome(ui);
        } else if chrome_was_shown {
            show_selection_chrome(ui);
        }
    }
    // Same index does not imply same allocation: a diff can replace/reorder rows
    // without changing the selected index or the overlay's overall dimensions.
    schedule_selection_snap(ui);
}

/// Count FrameClock paints for the current results update (OMNI_LOG_SEL).
fn arm_sel_paint_count(ui: &Ui) {
    let update_gen = ui.sel_update_gen.get() + 1;
    ui.sel_update_gen.set(update_gen);
    ui.sel_paint_frames.set(0);
    if std::env::var_os("OMNI_LOG_SEL").is_none() {
        return;
    }
    let Some(clock) = ui.list_overlay.frame_clock() else {
        return;
    };
    let weak = ui.me.borrow().clone();
    clock.connect_after_paint(move |_| {
        let Some(ui) = weak.upgrade() else { return };
        if ui.sel_update_gen.get() != update_gen {
            return;
        }
        ui.sel_paint_frames.set(ui.sel_paint_frames.get().saturating_add(1));
    });
}

/// Reconcile after at least one layout phase, ignoring superseded updates/sessions.
fn schedule_selection_snap(ui: &Ui) {
    let weak = ui.me.borrow().clone();
    let update_gen = ui.sel_update_gen.get();
    let place_gen = ui.sel_place_gen.get();
    let frames = Cell::new(0u8);
    ui.list_overlay.add_tick_callback(move |_, _| {
        let Some(ui) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if ui.sel_update_gen.get() != update_gen || ui.sel_place_gen.get() != place_gen {
            return glib::ControlFlow::Break;
        }
        let n = frames.get() + 1;
        frames.set(n);
        // GTK tick callbacks run BEFORE layout. The first tick may still see the
        // previous allocation (including a nonzero one); only read on a later tick.
        if n == 1 {
            return glib::ControlFlow::Continue;
        }
        cache_row_geometry(&ui);
        let ready = cached_sel_geom(&ui).is_some() || ui.hits.borrow().is_empty();
        if !ready && n < 60 {
            return glib::ControlFlow::Continue;
        }
        ui.sel_layout_pending.set(false);
        if ready {
            update_selection_visual(&ui);
            verify_selection_geometry(&ui);
        } else {
            ui.is_snapping.set(false);
            sel_log("schedule_selection_snap: no allocated row after 60 frames");
        }
        glib::ControlFlow::Break
    });
}

/// Diff list rows by `hit_key`: update survivors in place, insert/remove only deltas.
/// Never clears the list to empty between queries.
fn apply_hits(ui: &Ui, hits: &[Hit]) {
    let old_keys = std::mem::take(&mut *ui.row_keys.borrow_mut());
    let new_keys: Vec<String> = hits.iter().map(hit_key).collect();
    let query = ui.entry.text();
    let query = query.trim();

    // Fast path: identical identity sequence — patch labels only, no reparent.
    if old_keys == new_keys {
        for (i, hit) in hits.iter().enumerate() {
            if let Some(row) = ui.list.row_at_index(i as i32) {
                if !refresh_row_contents(&row, hit, query) {
                    let fresh = make_row(ui, hit);
                    ui.list.remove(&row);
                    ui.list.insert(&fresh, i as i32);
                }
            }
        }
        *ui.row_keys.borrow_mut() = new_keys;
        return;
    }

    let mut by_key: std::collections::HashMap<String, gtk::ListBoxRow> =
        std::collections::HashMap::with_capacity(old_keys.len());
    for (i, key) in old_keys.iter().enumerate() {
        if let Some(row) = ui.list.row_at_index(i as i32) {
            by_key.insert(key.clone(), row);
        }
    }

    let new_set: std::collections::HashSet<&str> = new_keys.iter().map(|s| s.as_str()).collect();
    // Drop rows that disappeared — list never goes fully empty if any key survives.
    for (key, row) in by_key.iter() {
        if !new_set.contains(key.as_str()) {
            ui.list.remove(row);
        }
    }
    by_key.retain(|k, _| new_set.contains(k.as_str()));

    for (i, hit) in hits.iter().enumerate() {
        let key = &new_keys[i];
        if let Some(row) = by_key.remove(key) {
            if row.index() != i as i32 {
                ui.list.remove(&row);
                ui.list.insert(&row, i as i32);
            }
            if !refresh_row_contents(&row, hit, query) {
                let fresh = make_row(ui, hit);
                ui.list.remove(&row);
                ui.list.insert(&fresh, i as i32);
            }
        } else {
            let row = make_row(ui, hit);
            ui.list.insert(&row, i as i32);
        }
    }
    // Strip any stragglers past the new length (keys out of sync).
    while let Some(row) = ui.list.row_at_index(hits.len() as i32) {
        ui.list.remove(&row);
    }
    *ui.row_keys.borrow_mut() = new_keys;
}

/// Update labels on a reused row. Returns false if structure must be rebuilt.
fn refresh_row_contents(row: &gtk::ListBoxRow, hit: &Hit, query: &str) -> bool {
    if row.has_css_class("omni-calc") || matches!(hit.action, Action::Copy(_) | Action::Info) {
        return false;
    }
    let Some(hbox) = row.child().and_downcast::<gtk::Box>() else {
        return false;
    };
    // Expect: icon | text-vbox [| badge]
    let Some(vbox) = hbox.first_child().and_then(|c| c.next_sibling()).and_downcast::<gtk::Box>() else {
        return false;
    };
    let wants_badge = matches!(hit.match_type, Some(MatchType::Content | MatchType::Both));
    let has_badge = vbox
        .next_sibling()
        .is_some_and(|w| w.has_css_class("omni-content-badge"));
    if wants_badge != has_badge {
        return false;
    }
    if let Some(title) = vbox.first_child().and_downcast::<gtk::Label>() {
        title.set_markup(&highlight_title(&hit.title, query));
    } else {
        return false;
    }
    match (vbox.first_child().and_then(|c| c.next_sibling()), hit.subtitle.is_empty()) {
        (Some(w), false) => {
            let Some(sub) = w.downcast_ref::<gtk::Label>() else {
                return false;
            };
            if hit.subtitle_markup {
                sub.set_use_markup(true);
                sub.set_markup(&hit.subtitle);
            } else {
                sub.set_use_markup(false);
                sub.set_text(&hit.subtitle);
            }
        }
        (None, false) | (Some(_), true) => return false,
        (None, true) => {}
    }
    true
}

/// A file or folder row.
fn file_hit(
    path: &std::path::Path,
    is_dir: bool,
    match_type: MatchType,
    snippet: Option<&str>,
    query: &str,
) -> Hit {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    let (subtitle, subtitle_markup) = match (match_type, snippet) {
        (MatchType::Content | MatchType::Both, Some(s)) if !s.is_empty() => {
            (highlight_snippet(s, query), true)
        }
        _ => (recent::folder_label(path), false),
    };
    Hit {
        title: name,
        subtitle,
        subtitle_markup,
        icon: Some(recent::icon(path, is_dir)),
        action: Action::Open(gio::File::for_path(path)),
        file_menu: !is_dir,
        match_type: Some(match_type),
    }
}

/// Accent-color markup for matched terms in a snippet (weight unchanged).
fn highlight_snippet(snippet: &str, query: &str) -> String {
    highlight_title(snippet, query)
}

fn contents_log(msg: impl std::fmt::Display) {
    if std::env::var_os("OMNI_LOG_CONTENTS").is_some() {
        eprintln!("omni[contents]: {msg}");
    }
}

fn sel_log(msg: impl std::fmt::Display) {
    if std::env::var_os("OMNI_LOG_SEL").is_some() {
        eprintln!("omni[sel]: {msg}");
    }
}

/// Snapshot highlight + selection for diagnostics (OMNI_LOG_SEL).
fn sel_log_state(ui: &Ui, phase: &str) {
    if std::env::var_os("OMNI_LOG_SEL").is_none() {
        return;
    }
    let count = ui.hits.borrow().len();
    let idx = ui.selected_index.get();
    let id = ui.hits.borrow().get(idx).map(hit_key).unwrap_or_else(|| "-".into());
    let fill = &ui.selection_fill;
    let pill = &ui.selection_pill;
    let parent = fill
        .parent()
        .map(|p| p.type_().name().to_string())
        .unwrap_or_else(|| "none".into());
    let row_geom = ui.row_geom.borrow().get(idx).copied();
    sel_log(format!(
        "{phase} count={count} selectedIndex={idx} id={id:?} \
         fill(vis={},opacity={:.2},parent={parent}) pill(vis={},opacity={:.2}) \
         transform(y={:.1},h={:.1}) isSnapping={} sel_layout_pending={} transition_on={} \
         row_geom={row_geom:?}",
        fill.is_visible(),
        fill.opacity(),
        pill.is_visible(),
        pill.opacity(),
        ui.sel_y.get(),
        ui.sel_h.get(),
        ui.is_snapping.get(),
        ui.sel_layout_pending.get(),
        ui.sel_transition_on.get(),
    ));
}

/// Per-keystroke verify line: opacity + chrome y/h vs selected row y/h.
fn sel_log_verify(ui: &Ui) {
    if std::env::var_os("OMNI_LOG_SEL").is_none() {
        return;
    }
    let count = ui.hits.borrow().len();
    let idx = ui.selected_index.get();
    let row = ui.row_geom.borrow().get(idx).copied();
    let (ry, rh) = row.unwrap_or((f64::NAN, f64::NAN));
    let cy = ui.sel_y.get();
    let ch = ui.sel_h.get();
    let op = ui.selection_fill.opacity();
    // During defer, row height may be 0 while chrome holds the prior place — still ok if opacity is 1.
    let ok = count == 0
        || (op >= 1.0
            && ch >= 1.0
            && (rh < 1.0 || ((cy - ry).abs() < 0.5 && (ch - rh).abs() < 0.5)));
    sel_log(format!(
        "VERIFY ok={ok} text={:?} count={count} selectedIndex={idx} opacity={op:.2} \
         chrome(y={cy:.1},h={ch:.1}) row(y={ry:.1},h={rh:.1})",
        ui.entry.text().to_string(),
    ));
}

fn invalidate_content_cache(ui: &Ui) {
    cancel_content_backends(ui);
    ui.content_gen.set(ui.content_gen.get() + 1);
    *ui.content.borrow_mut() = ContentCache::Idle;
}

fn match_type_rank(m: Option<MatchType>) -> u8 {
    match m {
        // Contents on: best content hits first (name+content, then content-only, then name).
        Some(MatchType::Both) => 0,
        Some(MatchType::Content) => 1,
        Some(MatchType::Name) => 2,
        None => 3,
    }
}

/// Stable sort of file Open rows: both → content → name. Other rows keep relative order.
fn sort_file_hits_by_match_type(hits: &mut Vec<Hit>) {
    let mut start = None;
    let mut i = 0;
    while i <= hits.len() {
        let is_file = hits
            .get(i)
            .is_some_and(|h| matches!(h.action, Action::Open(_)) && h.match_type.is_some());
        if is_file {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start {
            hits[s..i].sort_by_key(|h| match_type_rank(h.match_type));
            start = None;
        }
        i += 1;
    }
}

/// With Contents on, float content matches above apps/services/name-only rows.
fn promote_content_matches(hits: &mut Vec<Hit>) {
    let mut content = Vec::new();
    let mut rest = Vec::new();
    for h in hits.drain(..) {
        if matches!(h.match_type, Some(MatchType::Content | MatchType::Both)) {
            content.push(h);
        } else {
            rest.push(h);
        }
    }
    content.sort_by_key(|h| match_type_rank(h.match_type));
    hits.extend(content);
    hits.extend(rest);
}

fn hit_path(hit: &Hit) -> Option<PathBuf> {
    match &hit.action {
        Action::Open(f) | Action::OpenWith(_, f) => f.path(),
        _ => None,
    }
}

fn apply_content_hits_to_list(query: &str, content_hits: &[ContentHit], hits: &mut Vec<Hit>) -> (usize, usize) {
    let mut both = 0usize;
    let mut content_only = 0usize;
    for ch in content_hits {
        let mut upgraded = false;
        for hit in hits.iter_mut() {
            if hit_path(hit).as_ref() == Some(&ch.path) {
                hit.match_type = Some(MatchType::Both);
                if let Some(ref snip) = ch.snippet {
                    hit.subtitle = highlight_snippet(snip, query);
                    hit.subtitle_markup = true;
                }
                upgraded = true;
                both += 1;
                break;
            }
        }
        if !upgraded {
            hits.push(file_hit(
                &ch.path,
                false,
                MatchType::Content,
                ch.snippet.as_deref(),
                query,
            ));
            content_only += 1;
        }
    }
    (both, content_only)
}

/// Merge content hits into `hits`, or schedule a search / show empty state.
fn merge_content_hits(ui: &Ui, query: &str, scope: Option<&std::path::Path>, hits: &mut Vec<Hit>) {
    let key = ContentKey::new(query, scope);
    let cache = ui.content.borrow();
    match &*cache {
        ContentCache::Failed { key: k, message } if k == &key => {
            let message = message.clone();
            drop(cache);
            hits.push(content_failed_hit(&message));
        }
        ContentCache::Active {
            key: shown_key,
            hits: content_hits,
            searching,
            pending_key,
            ready_at,
        } => {
            let inflight_for_us = pending_key.as_ref() == Some(&key)
                || (pending_key.is_none() && *searching && shown_key == &key);
            let showing_us = shown_key == &key;

            if showing_us {
                let (both, content_only) = apply_content_hits_to_list(query, content_hits, hits);
                contents_log(format!(
                    "merge query={:?} raw={} both={both} content={content_only} searching={searching}",
                    key.query,
                    content_hits.len()
                ));
                if !*searching && content_hits.is_empty() {
                    let scoped = key.scope_path().is_some();
                    drop(cache);
                    hits.push(content_empty_hit(scoped));
                    return;
                }
                if !*searching {
                    if ready_at.is_some_and(|at| at.elapsed() < CONTENT_CACHE_TTL) {
                        return; // warm cache
                    }
                    drop(cache);
                    schedule_content_search(ui, query, scope);
                    return;
                }
                return; // searching this key
            }

            if inflight_for_us {
                // Keep previous content rows on screen until the first batch for `key` arrives.
                let (_b, _c) = apply_content_hits_to_list(query, content_hits, hits);
                contents_log(format!(
                    "merge query={:?} inflight (keeping {} prior hits)",
                    key.query,
                    content_hits.len()
                ));
                return;
            }

            drop(cache);
            schedule_content_search(ui, query, scope);
        }
        ContentCache::Idle => {
            drop(cache);
            schedule_content_search(ui, query, scope);
        }
        ContentCache::Failed { .. } => {
            drop(cache);
            schedule_content_search(ui, query, scope);
        }
    }
}

fn content_empty_hit(scoped: bool) -> Hit {
    Hit {
        title: "No content matches".into(),
        subtitle: if scoped {
            "No content matches in this folder. It may not be content-indexed.".into()
        } else {
            "This folder may not be content-indexed.".into()
        },
        subtitle_markup: false,
        icon: Some(gio::ThemedIcon::new("dialog-information-symbolic").upcast()),
        action: Action::Info,
        file_menu: false,
        match_type: None,
    }
}

fn content_failed_hit(message: &str) -> Hit {
    Hit {
        title: "Contents search unavailable".into(),
        subtitle: message.to_string(),
        subtitle_markup: false,
        icon: Some(gio::ThemedIcon::new("dialog-warning-symbolic").upcast()),
        action: Action::Info,
        file_menu: false,
        match_type: None,
    }
}

/// The query is a complete calculation (see calc.rs): its result, copied on Enter.
fn calc_hit(query: &str) -> Option<Hit> {
    let result = calc::format(calc::evaluate(query)?);
    Some(Hit {
        title: result.clone(),
        subtitle: format!("{} =", calc::pretty(query)),
        subtitle_markup: false,
        icon: None,
        action: Action::Copy(result),
        file_menu: false,
        match_type: None,
    })
}

/// Search file contents once typing has paused for CONTENT_DELAY.
/// "~/Music: NF" → (/home/you/Music, "NF"): search only inside that folder. The folder
/// part starts with ~ or /, and a colon then a space separate it from the name. The
/// directory is matched case-insensitively (`~/music: NF` still scopes to ~/Music).
fn parse_scope(query: &str) -> Option<(PathBuf, &str)> {
    // The popup trims the query, so "~/Music: " arrives as "~/Music:".
    let (dir, inner) = query.split_once(": ").or_else(|| Some((query.strip_suffix(':')?, "")))?;
    let dir = dir.trim();
    let path = if dir == "~" {
        glib::home_dir()
    } else if let Some(rest) = dir.strip_prefix("~/") {
        glib::home_dir().join(rest)
    } else if dir.starts_with('/') {
        PathBuf::from(dir)
    } else {
        return None;
    };
    Some((path, inner.trim()))
}

/// Results for "~/Folder: name": files and folders matching `inner` anywhere inside `dir`
/// (the filter still applies; apps, web and Claude don't belong here). Nothing typed after
/// the colon yet: the folder itself, to open.
fn scoped_hits(ui: &Ui, dir: &std::path::Path, inner: &str) -> Vec<Hit> {
    if inner.is_empty() {
        return if dir.is_dir() {
            vec![file_hit(dir, true, MatchType::Name, None, "")]
        } else {
            Vec::new()
        };
    }
    let show = ui.filter.get();
    let max_files = if show & SHOW_FILES == 0 { 0 } else { MAX_FILTERED };
    let max_dirs = if show & SHOW_FOLDERS == 0 { 0 } else { MAX_DIRS * 2 };
    let (mut files, mut dirs) = files::search(&ui.files, inner, max_files.max(1), max_dirs.max(1), Some(dir));
    files.truncate(max_files);
    dirs.truncate(max_dirs);
    files
        .into_iter()
        .chain(dirs)
        .map(|h| file_hit(&h.path, h.is_dir, MatchType::Name, None, inner))
        .collect()
}

fn cancel_content_backends(ui: &Ui) {
    ui.content_cancel.borrow().store(true, Ordering::SeqCst);
    if let Ok(mut slot) = ui.content_rg.lock() {
        if let Some(mut child) = slot.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn schedule_content_search(ui: &Ui, query: &str, scope: Option<&std::path::Path>) {
    cancel_content_backends(ui);
    let generation = ui.content_gen.get() + 1;
    ui.content_gen.set(generation);
    let key = ContentKey::new(query, scope);
    // Mark in-flight without clearing previously shown hits.
    {
        let mut cache = ui.content.borrow_mut();
        match &mut *cache {
            ContentCache::Active {
                pending_key,
                searching,
                ..
            } => {
                *pending_key = Some(key.clone());
                *searching = true;
            }
            other => {
                *other = ContentCache::Active {
                    key: key.clone(),
                    hits: Vec::new(),
                    searching: true,
                    pending_key: Some(key.clone()),
                    ready_at: None,
                };
            }
        }
    }
    let Some(me) = ui.me.borrow().upgrade() else { return };
    let entry_snapshot = ui.entry.text().trim().to_string();
    let t0 = Instant::now();
    contents_log(format!(
        "timing keystroke/schedule t=0ms delay={}ms query={:?} scope={:?}",
        CONTENT_DELAY.as_millis(),
        key.query,
        key.scope_path()
    ));
    glib::timeout_add_local_once(CONTENT_DELAY, move || {
        contents_log(format!(
            "timing debounce_fired t={}ms query={:?}",
            t0.elapsed().as_millis(),
            key.query
        ));
        if me.content_gen.get() == generation
            && me.entry.text().trim() == entry_snapshot
            && me.window.is_visible()
            && me.contents.is_active()
        {
            start_content_search(&me, &key.query, key.scope_path().as_deref(), t0);
        } else {
            contents_log(format!("schedule dropped (stale) query={:?}", key.query));
            // If no newer search owns the cache, clear searching so the footer can't stick.
            clear_content_searching_if_gen(&me, generation);
        }
    });
}

/// Drop `searching` / `pending_key` when `generation` still owns the cache (no newer schedule).
fn clear_content_searching_if_gen(ui: &Ui, generation: u64) {
    if ui.content_gen.get() != generation {
        return;
    }
    if let ContentCache::Active {
        searching,
        pending_key,
        ..
    } = &mut *ui.content.borrow_mut()
    {
        *searching = false;
        *pending_key = None;
    }
}

/// Resolve search text + optional directory scope from the entry (or an explicit pair).
fn content_search_args(entry_text: &str) -> (String, Option<PathBuf>) {
    match parse_scope(entry_text) {
        Some((dir, inner)) => (inner.to_string(), Some(dir)),
        None => (entry_text.to_string(), None),
    }
}

/// Apply a streamed content batch on the UI thread. Drops stale generations.
fn apply_content_batch(
    ui: &Ui,
    generation: u64,
    key: &ContentKey,
    batch: Vec<ContentHit>,
    done: bool,
    err: Option<String>,
    t0: Instant,
    first: bool,
) {
    if ui.content_gen.get() != generation || !ui.contents.is_active() {
        contents_log(format!("results dropped (stale) query={:?}", key.query));
        return;
    }
    if let Some(message) = err {
        *ui.content.borrow_mut() = ContentCache::Failed {
            key: key.clone(),
            message,
        };
        if !ui.answer_reveal.reveals_child() && ui.window.is_visible() {
            refresh(ui);
        }
        return;
    }
    {
        let mut cache = ui.content.borrow_mut();
        match &mut *cache {
            ContentCache::Active {
                key: shown_key,
                hits,
                searching,
                pending_key,
                ready_at,
            } if pending_key.as_ref() == Some(key) || shown_key == key => {
                if pending_key.as_ref() == Some(key) && shown_key != key {
                    // First batch for the new query: replace prior hits.
                    *shown_key = key.clone();
                    hits.clear();
                    *pending_key = Some(key.clone());
                }
                for h in batch {
                    if !hits.iter().any(|e| e.path == h.path) {
                        hits.push(h);
                    }
                }
                if done {
                    *searching = false;
                    *pending_key = None;
                    *ready_at = Some(Instant::now());
                } else {
                    *searching = true;
                }
            }
            _ => {
                *cache = ContentCache::Active {
                    key: key.clone(),
                    hits: batch,
                    searching: !done,
                    pending_key: if done { None } else { Some(key.clone()) },
                    ready_at: if done { Some(Instant::now()) } else { None },
                };
            }
        }
    }
    if first {
        contents_log(format!(
            "timing first_result t={}ms query={:?}",
            t0.elapsed().as_millis(),
            key.query
        ));
    }
    if done {
        contents_log(format!(
            "timing last_result t={}ms query={:?}",
            t0.elapsed().as_millis(),
            key.query
        ));
    }
    if !ui.answer_reveal.reveals_child() && ui.window.is_visible() {
        refresh(ui);
        contents_log(format!(
            "timing rendered t={}ms query={:?} done={done}",
            t0.elapsed().as_millis(),
            key.query
        ));
        if !done {
            schedule_visible_snippets(ui);
        } else {
            schedule_visible_snippets(ui);
        }
    }
}

/// Fill snippets for on-screen content rows only (off-thread, 1 MB cap).
fn schedule_visible_snippets(ui: &Ui) {
    let Some(me) = ui.me.borrow().upgrade() else { return };
    let expect_gen = ui.content_gen.get();
    let query = match &*ui.content.borrow() {
        ContentCache::Active { key, .. } => key.query.clone(),
        _ => return,
    };
    let paths: Vec<PathBuf> = {
        let hits = ui.hits.borrow();
        let start = ui.list_scroll.vadjustment().value() as usize / ROW_HEIGHT as usize;
        let end = (start + MAX_VISIBLE_ROWS as usize + 2).min(hits.len());
        hits[start..end]
            .iter()
            .filter(|h| matches!(h.match_type, Some(MatchType::Content | MatchType::Both)))
            .filter(|h| !h.subtitle_markup || h.subtitle.is_empty())
            .filter_map(hit_path)
            .collect()
    };
    if paths.is_empty() {
        return;
    }
    let need: Vec<PathBuf> = {
        let cache = ui.content.borrow();
        let ContentCache::Active { hits, .. } = &*cache else {
            return;
        };
        paths
            .into_iter()
            .filter(|p| {
                hits.iter()
                    .find(|h| &h.path == p)
                    .is_some_and(|h| h.snippet.is_none())
            })
            .collect()
    };
    if need.is_empty() {
        return;
    }
    let q = query.clone();
    glib::spawn_future_local(async move {
        let filled = gio::spawn_blocking(move || {
            need.into_iter()
                .filter_map(|path| {
                    let snip = files::content_snippet_for(&path, &q)?;
                    Some((path, snip))
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        if me.content_gen.get() != expect_gen || filled.is_empty() {
            return;
        }
        {
            let mut cache = me.content.borrow_mut();
            let ContentCache::Active { key, hits, .. } = &mut *cache else {
                return;
            };
            if key.query != query {
                return;
            }
            for (path, snip) in &filled {
                if let Some(h) = hits.iter_mut().find(|h| &h.path == path) {
                    h.snippet = Some(snip.clone());
                }
            }
        }
        if !me.answer_reveal.reveals_child() && me.window.is_visible() {
            refresh(&me);
        }
    });
}

struct ContentBatchMsg {
    generation: u64,
    key: ContentKey,
    hits: Vec<ContentHit>,
    done: bool,
    err: Option<String>,
    t0: Instant,
    first: bool,
    entry_snapshot: String,
}

/// Ask LocalSearch + ripgrep off the UI thread; stream batches as they arrive.
fn start_content_search(ui: &Ui, query: &str, scope: Option<&std::path::Path>, t0: Instant) {
    cancel_content_backends(ui);
    let cancel = Arc::new(AtomicBool::new(false));
    *ui.content_cancel.borrow_mut() = cancel.clone();

    let generation = ui.content_gen.get() + 1;
    ui.content_gen.set(generation);
    let key = ContentKey::new(query, scope);
    {
        let mut cache = ui.content.borrow_mut();
        match &mut *cache {
            ContentCache::Active {
                pending_key,
                searching,
                ..
            } => {
                *pending_key = Some(key.clone());
                *searching = true;
            }
            other => {
                *other = ContentCache::Active {
                    key: key.clone(),
                    hits: Vec::new(),
                    searching: true,
                    pending_key: Some(key.clone()),
                    ready_at: None,
                };
            }
        }
    }

    if ui.me.borrow().upgrade().is_none() {
        return;
    }
    let conn = ui.sparql.borrow().clone();
    let entry_snapshot = ui.entry.text().trim().to_string();
    let rg_slot = ui.content_rg.clone();
    let q = key.query.clone();
    let scope_path = key.scope_path();
    let key_for_worker = key.clone();

    contents_log(format!(
        "timing backend_start t={}ms query={:?} scope={:?} backends=rg+localsearch",
        t0.elapsed().as_millis(),
        key.query,
        key.scope_path()
    ));

    // Worker → UI: std mpsc (Send) + short poll on the main loop (Rc<Ui> is !Send).
    let (tx, rx) = std::sync::mpsc::channel::<ContentBatchMsg>();
    let rx = Arc::new(Mutex::new(Some(rx)));
    let weak = ui.me.borrow().clone();
    let rx_poll = rx.clone();
    glib::timeout_add_local(Duration::from_millis(16), move || {
        let Some(ui) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let mut guard = match rx_poll.lock() {
            Ok(g) => g,
            Err(_) => return glib::ControlFlow::Break,
        };
        let Some(rx) = guard.as_mut() else {
            return glib::ControlFlow::Break;
        };
        let mut stop = false;
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    if ui.entry.text().trim() == msg.entry_snapshot {
                        let done = msg.done;
                        apply_content_batch(
                            &ui,
                            msg.generation,
                            &msg.key,
                            msg.hits,
                            msg.done,
                            msg.err,
                            msg.t0,
                            msg.first,
                        );
                        if done {
                            stop = true;
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    stop = true;
                    break;
                }
            }
        }
        if stop {
            *guard = None;
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });

    std::thread::Builder::new()
        .name("omni-content".into())
        .spawn(move || {
            let first_flag = Arc::new(AtomicBool::new(true));
            let (stx, srx) =
                std::sync::mpsc::channel::<(Vec<files::FtsHit>, String, Option<String>)>();

            if let Some(conn) = conn {
                let q2 = q.clone();
                let scope2 = scope_path.clone();
                let cancel2 = cancel.clone();
                std::thread::Builder::new()
                    .name("omni-content-ls".into())
                    .spawn(move || {
                        if cancel2.load(Ordering::Relaxed) {
                            let _ = stx.send((Vec::new(), String::new(), None));
                            return;
                        }
                        let r = files::content_search_localsearch(
                            &conn,
                            &q2,
                            MAX_CONTENT,
                            scope2.as_deref(),
                        );
                        let _ = stx.send(r);
                    })
                    .ok();
            } else {
                let _ = stx.send((Vec::new(), String::new(), None));
            }

            let mut batch: Vec<ContentHit> = Vec::new();
            let mut last_flush = Instant::now();
            let mut total = 0usize;
            let send_batch =
                |chunk: Vec<ContentHit>, first_flag: &AtomicBool, tx: &std::sync::mpsc::Sender<ContentBatchMsg>| {
                    if chunk.is_empty() {
                        return;
                    }
                    let is_first = first_flag.swap(false, Ordering::SeqCst);
                    let _ = tx.send(ContentBatchMsg {
                        generation,
                        key: key_for_worker.clone(),
                        hits: chunk,
                        done: false,
                        err: None,
                        t0,
                        first: is_first,
                        entry_snapshot: entry_snapshot.clone(),
                    });
                };

            files::content_search_rg_stream(
                &q,
                MAX_CONTENT,
                scope_path.as_deref(),
                &cancel,
                &rg_slot,
                |hit| {
                    total += 1;
                    batch.push(ContentHit {
                        path: hit.path,
                        snippet: None,
                    });
                    if total == 1 || last_flush.elapsed() >= CONTENT_BATCH {
                        send_batch(std::mem::take(&mut batch), &first_flag, &tx);
                        last_flush = Instant::now();
                    }
                },
            );
            send_batch(std::mem::take(&mut batch), &first_flag, &tx);
            contents_log(format!(
                "timing rg_done t={}ms count={total} query={:?}",
                t0.elapsed().as_millis(),
                key_for_worker.query
            ));

            let (ls_hits, sparql, ls_err) = srx
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|_| (Vec::new(), String::new(), None));
            contents_log(format!(
                "timing ls_done t={}ms count={} sparql_len={} err={ls_err:?} query={:?}",
                t0.elapsed().as_millis(),
                ls_hits.len(),
                sparql.len(),
                key_for_worker.query
            ));

            if cancel.load(Ordering::Relaxed) {
                let _ = tx.send(ContentBatchMsg {
                    generation,
                    key: key_for_worker,
                    hits: Vec::new(),
                    done: true,
                    err: None,
                    t0,
                    first: false,
                    entry_snapshot,
                });
                return;
            }

            let ls_batch: Vec<ContentHit> = ls_hits
                .into_iter()
                .map(|h| ContentHit {
                    path: h.path,
                    snippet: None,
                })
                .collect();
            let is_first = first_flag.swap(false, Ordering::SeqCst);
            let fail = ls_err.filter(|_| total == 0 && ls_batch.is_empty());
            let _ = tx.send(ContentBatchMsg {
                generation,
                key: key_for_worker,
                hits: ls_batch,
                done: true,
                err: fail,
                t0,
                first: is_first,
                entry_snapshot,
            });
        })
        .expect("content search thread");
}

/// Connect to LocalSearch off the UI thread; then listen for new files (added to the
/// name index right away) and enable contents search.
fn connect_localsearch(ui: &Rc<Ui>) {
    let weak = Rc::downgrade(ui);
    glib::spawn_future_local(async move {
        let conn = match gio::spawn_blocking(sparql::Connection::new).await {
            Ok(Ok(c)) => Arc::new(c),
            Ok(Err(e)) => {
                eprintln!("omni: can't connect to LocalSearch (contents search and live file updates off): {e}");
                contents_log(format!("connect failed: {e}"));
                return;
            }
            Err(_) => return,
        };
        let Some(ui) = weak.upgrade() else { return };
        let notifier = conn.notifier();
        // The UI thread only queues URNs; one worker thread does the lookups (see files.rs).
        let live = files::start_live_worker(conn.clone(), ui.files.clone());
        notifier.connect_local("events", false, move |args| {
            let events = sparql::notifier_events(&args[3]);
            LIVE_EVENTS.fetch_add(events.len(), std::sync::atomic::Ordering::Relaxed);
            let urns: Vec<String> = events
                .into_iter()
                .filter(|(kind, _)| *kind != sparql::EVENT_DELETE) // deletions: dropped when shown
                .map(|(_, urn)| urn)
                .collect();
            if !urns.is_empty() {
                let _ = live.send(urns);
            }
            None
        });
        *ui.notifier.borrow_mut() = Some(notifier);
        *ui.sparql.borrow_mut() = Some(conn);
        contents_log("LocalSearch connected");
        open_log(&ui, "indexer_connection_ready (LocalSearch)");
        // Retry contents search if the toggle was already on before the bus was ready.
        if ui.contents.is_active() && ui.window.is_visible() {
            let q = ui.entry.text();
            let q = q.trim().to_string();
            if !q.is_empty() && !ui.answer_reveal.reveals_child() {
                let (search_q, scope) = content_search_args(&q);
                if !search_q.is_empty() {
                    contents_log(format!(
                        "retry after connect query={search_q:?} scope={scope:?}"
                    ));
                    start_content_search(&ui, &search_q, scope.as_deref(), Instant::now());
                }
            }
        }
    });
}

/// Footer path for the selected hit: full file path, or .desktop path for apps.
fn footer_path_for(hit: &Hit) -> String {
    match &hit.action {
        Action::Open(file) | Action::OpenWith(_, file) => {
            file.path().map(|p| p.display().to_string()).unwrap_or_default()
        }
        Action::App(info) | Action::AppAction(info, _) => info
            .downcast_ref::<gio_unix::DesktopAppInfo>()
            .and_then(|d| d.filename())
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn update_path_footer(ui: &Ui, index: Option<usize>) {
    let mut text = index
        .and_then(|i| ui.hits.borrow().get(i).map(footer_path_for))
        .unwrap_or_default();
    if ui.contents.is_active() && ui.content.borrow().searching() {
        if text.is_empty() {
            text = "searching\u{2026}".into();
        } else {
            text.push_str(" \u{b7} searching\u{2026}");
        }
    }
    let same_full = *ui.path_full.borrow() == text;
    if !same_full {
        *ui.path_full.borrow_mut() = text;
    }
    // Always re-apply: path_full may be correct while the label was cleared (width 0 on hide).
    apply_path_display(ui);
}

/// Pixel width of `text` with the footer label's font (size/weight from CSS).
fn path_text_width(label: &gtk::Label, text: &str) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    let layout = label.create_pango_layout(Some(text));
    layout.pixel_size().0 as f64
}

/// Split into (prefix including trailing slash, filename). Filename bias for truncation.
fn split_path_file(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    }
}

/// Middle-truncate `path` to fit `avail` px. Filename stays fully visible; leftover width
/// goes to the start. Binary-search the kept prefix length against real font metrics.
fn middle_truncate_path(label: &gtk::Label, path: &str, avail: f64) -> String {
    if path.is_empty() || avail <= 0.0 {
        return String::new();
    }
    if path_text_width(label, path) <= avail {
        return path.to_string();
    }

    let (prefix, file) = split_path_file(path);
    let ellipsis = PATH_ELLIPSIS;
    let ell_w = path_text_width(label, ellipsis);

    // Filename must stay fully visible when possible.
    if !file.is_empty() {
        let file_w = path_text_width(label, file);
        if file_w > avail {
            // Filename alone is wider than the strip — keep the end of the name.
            return truncate_end_to_width(label, file, avail, ellipsis, ell_w);
        }
        if prefix.is_empty() {
            return file.to_string();
        }
        // remaining width for prefix + ellipsis
        let budget = avail - file_w;
        if ell_w >= budget {
            // No room for any prefix — show ellipsis + file if that fits, else file only.
            let with_ell = format!("{ellipsis}{file}");
            return if path_text_width(label, &with_ell) <= avail {
                with_ell
            } else {
                file.to_string()
            };
        }
        let chars: Vec<char> = prefix.chars().collect();
        let mut lo = 0usize;
        let mut hi = chars.len();
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            let kept: String = chars[..mid].iter().collect();
            let candidate = format!("{kept}{ellipsis}{file}");
            if path_text_width(label, &candidate) <= avail {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let kept: String = chars[..lo].iter().collect();
        return format!("{kept}{ellipsis}{file}");
    }

    // No filename segment (e.g. "/"): generic middle truncate on the whole string.
    middle_truncate_chars(label, path, avail, ellipsis, ell_w)
}

fn truncate_end_to_width(label: &gtk::Label, text: &str, avail: f64, ellipsis: &str, ell_w: f64) -> String {
    if path_text_width(label, text) <= avail {
        return text.to_string();
    }
    if ell_w >= avail {
        return ellipsis.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let kept: String = chars[chars.len() - mid..].iter().collect();
        let candidate = format!("{ellipsis}{kept}");
        if path_text_width(label, &candidate) <= avail {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let kept: String = chars[chars.len() - lo..].iter().collect();
    format!("{ellipsis}{kept}")
}

fn middle_truncate_chars(label: &gtk::Label, text: &str, avail: f64, ellipsis: &str, ell_w: f64) -> String {
    if ell_w >= avail {
        return ellipsis.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = (lo + hi + 1) / 2;
        let left = mid / 2;
        let right = mid - left;
        let kept_l: String = chars[..left].iter().collect();
        let kept_r: String = chars[n - right..].iter().collect();
        let candidate = format!("{kept_l}{ellipsis}{kept_r}");
        if path_text_width(label, &candidate) <= avail {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let left = lo / 2;
    let right = lo - left;
    let kept_l: String = chars[..left].iter().collect();
    let kept_r: String = chars[n - right..].iter().collect();
    format!("{kept_l}{ellipsis}{kept_r}")
}

/// Apply cached or freshly measured truncation to the footer label.
fn apply_path_display(ui: &Ui) {
    let full = ui.path_full.borrow().clone();
    let box_w = ui.path_box.width();
    if box_w <= 0 {
        if !ui.path_label.text().is_empty() {
            ui.path_label.set_text("");
        }
        return;
    }
    if let Some((ref p, w, ref d)) = *ui.path_trunc_cache.borrow() {
        if p == &full && w == box_w {
            // Already showing this truncation — do not set_text again (avoids redraw).
            if ui.path_label.text().as_str() != d.as_str() {
                ui.path_label.set_text(d);
            }
            return;
        }
    }
    let avail = (box_w as f64) - 2.0 * PATH_FOOTER_PAD_X;
    let display = middle_truncate_path(&ui.path_label, &full, avail);
    if ui.path_label.text().as_str() == display.as_str() {
        *ui.path_trunc_cache.borrow_mut() = Some((full, box_w, display));
        return;
    }
    *ui.path_trunc_cache.borrow_mut() = Some((full, box_w, display.clone()));
    ui.path_label.set_text(&display);
}

fn motion_enabled() -> bool {
    gtk::Settings::default().is_some_and(|s| s.is_gtk_enable_animations())
}

/// Stable identity for keeping selection across list rebuilds.
fn hit_key(hit: &Hit) -> String {
    match &hit.action {
        Action::Open(file) | Action::OpenWith(_, file) => file.uri().to_string(),
        Action::App(info) => format!("app:{}", info.id().unwrap_or_default()),
        Action::AppAction(info, action) => {
            format!("app-action:{}:{}", info.id().unwrap_or_default(), action.name)
        }
        Action::Web(q) => format!("web:{q}"),
        Action::WebIn(q, w) => format!("web-in:{q}:{w:?}"),
        Action::Claude(q) => format!("claude:{q}"),
        Action::Copy(t) => format!("copy:{t}"),
        Action::Service(s) => format!("svc:{}", s.unit),
        Action::ServiceStatus(s) => format!("svc-status:{}", s.unit),
        Action::ServiceVerb(s, verb) => format!("svc-verb:{}:{}", s.unit, verb.label()),
        Action::DockPin(id, on) => format!("dock:{id}:{on}"),
        Action::Info => format!("info:{}", hit.title),
    }
}

/// Measure all row y/height once (list updates / resize). Selection moves read the cache only.
fn cache_row_geometry(ui: &Ui) {
    let n = ui.hits.borrow().len();
    let mut geom = Vec::with_capacity(n);
    for i in 0..n {
        let entry = ui
            .list
            .row_at_index(i as i32)
            .and_then(|row| row.compute_bounds(&ui.list_overlay))
            .map(|b| (b.y() as f64, b.height() as f64))
            .unwrap_or((0.0, 0.0));
        geom.push(entry);
    }
    *ui.row_geom.borrow_mut() = geom;
}

fn cached_sel_geom(ui: &Ui) -> Option<(f64, f64)> {
    if ui.hits.borrow().is_empty() {
        return None;
    }
    let index = ui.selected_index.get().min(ui.hits.borrow().len() - 1);
    ui.selected_index.set(index);
    let (y, h) = *ui.row_geom.borrow().get(index)?;
    (h >= 1.0).then_some((y, h))
}

/// When `compute_bounds` is still 0 after an in-place list diff, estimate from fixed row height.
/// Matches measured index-0 origin (list padding ≈ 4px) used by the overlay chrome.
fn estimate_sel_geom(ui: &Ui) -> Option<(f64, f64)> {
    if ui.hits.borrow().is_empty() {
        return None;
    }
    let index = ui.selected_index.get().min(ui.hits.borrow().len() - 1);
    ui.list.row_at_index(index as i32)?;
    const LIST_PAD_Y: f64 = 4.0;
    let y = LIST_PAD_Y + (index as f64) * ROW_HEIGHT as f64;
    Some((y, ROW_HEIGHT as f64))
}

/// Drive fill (translateY + scaleY) and pill (translateY only) — no layout properties.
fn apply_sel_transform(ui: &Ui, y: f64, h: f64) {
    let h = h.max(1.0);
    // Skip identical updates: CssProvider reloads can re-enter size notifies and freeze GTK.
    if (ui.sel_y.get() - y).abs() < 0.05 && (ui.sel_h.get() - h).abs() < 0.05 {
        return;
    }
    ui.sel_y.set(y);
    ui.sel_h.set(h);
    let scale = h / SEL_BASE_H;
    let pill_y = y + (h - SEL_PILL_H) * 0.5;
    ui.sel_transform_css.load_from_string(&format!(
        ".omni-selection {{\n  transform: translateY({y:.2}px) scaleY({scale:.5});\n}}\n\
         .omni-selection-pill {{\n  transform: translateY({pill_y:.2}px);\n}}\n"
    ));
}

fn set_sel_transition_enabled(ui: &Ui, on: bool) {
    if ui.sel_transition_on.get() == on {
        return;
    }
    ui.sel_transition_on.set(on);
    let css = if on {
        format!(
            ".omni-selection, .omni-selection-pill {{\n  \
             transition: transform {SELECT_MOTION_MS:.0}ms cubic-bezier(0.2, 0, 0, 1);\n  \
             will-change: transform;\n}}\n"
        )
    } else {
        ".omni-selection, .omni-selection-pill {\n  transition: none;\n  will-change: transform;\n}\n"
            .into()
    };
    ui.sel_transition_css.load_from_string(&css);
}

fn hide_selection_chrome(ui: &Ui) {
    ui.selection_fill.set_opacity(0.0);
    ui.selection_pill.set_opacity(0.0);
    ui.selection_fill.set_visible(false);
    ui.selection_pill.set_visible(false);
}

fn show_selection_chrome(ui: &Ui) {
    ui.selection_fill.set_visible(true);
    ui.selection_pill.set_visible(true);
    ui.selection_fill.set_opacity(1.0);
    ui.selection_pill.set_opacity(1.0);
}

/// Place highlight from cached geometry with transition off; re-enable on the next frame.
/// Always refreshes the footer from `selected_index`. Called after every results update,
/// open, resize, and filter/scope/toggle change.
fn update_selection_visual(ui: &Ui) {
    update_selection_visual_attempt(ui, 0, ui.sel_place_gen.get());
}

/// Retry until the selected row has nonzero height. Cap at 60 frames (warn + stop).
/// Transition is re-enabled only after a successful place — never on give-up.
fn update_selection_visual_attempt(ui: &Ui, attempt: u8, place_gen: u64) {
    if place_gen != ui.sel_place_gen.get() {
        sel_log("update_selection_visual: stale place_gen → abort");
        return;
    }
    if !ui.window.is_visible() && !ui.place_while_hidden.get() {
        sel_log("update_selection_visual: window closed → stop");
        hide_selection_chrome(ui);
        ui.is_snapping.set(false);
        ui.sel_layout_pending.set(false);
        return;
    }
    if ui.hits.borrow().is_empty() {
        sel_log("update_selection_visual: empty → hide");
        hide_selection_chrome(ui);
        ui.is_snapping.set(false);
        ui.sel_layout_pending.set(false);
        update_path_footer(ui, None);
        if motion_enabled() && ui.window.is_visible() {
            set_sel_transition_enabled(ui, true);
        }
        flush_sel_needs_replace(ui);
        return;
    }
    let count = ui.hits.borrow().len();
    let idx = ui.selected_index.get().min(count - 1);
    ui.selected_index.set(idx);
    // Selected row gone (list shrunk and clamp still leaves us without a widget).
    if ui.list.row_at_index(idx as i32).is_none() {
        sel_log(format!(
            "update_selection_visual: no row at index={idx} → stop"
        ));
        hide_selection_chrome(ui);
        ui.is_snapping.set(false);
        update_path_footer(ui, None);
        return;
    }
    cache_row_geometry(ui);
    let Some((y, h)) = cached_sel_geom(ui).or_else(|| estimate_sel_geom(ui)) else {
        // Keep prior highlight visible while waiting; hide only if that row is gone.
        let row_gone = ui.list.row_at_index(idx as i32).is_none();
        sel_log(format!(
            "update_selection_visual: no geom attempt={attempt} row_gone={row_gone} → keep prior, retry"
        ));
        ui.is_snapping.set(true);
        set_sel_transition_enabled(ui, false);
        if row_gone {
            hide_selection_chrome(ui);
        } else if ui.selection_fill.is_visible() {
            // Stay at last transform/opacity — do not blink.
            ui.selection_fill.set_opacity(1.0);
            ui.selection_pill.set_opacity(1.0);
        }
        // Footer from selection even while geom retries.
        update_path_footer(ui, Some(idx));
        if attempt >= 60 {
            eprintln!(
                "omni: update_selection_visual gave up after 60 frames (no row geometry)"
            );
            sel_log("update_selection_visual: WARNING 60-frame safety cap hit");
            ui.is_snapping.set(false);
            flush_sel_needs_replace(ui);
            return;
        }
        let weak = ui.me.borrow().clone();
        ui.list_overlay.add_tick_callback(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                update_selection_visual_attempt(&ui, attempt.saturating_add(1), place_gen);
            }
            glib::ControlFlow::Break
        });
        return;
    };
    // Footer always tracks selectedIndex when chrome is placed / confirmed.
    update_path_footer(ui, Some(idx));
    // Skip no-op place when transform already matches and chrome is shown.
    let same_geom =
        (ui.sel_y.get() - y).abs() < 0.05 && (ui.sel_h.get() - h).abs() < 0.05;
    let chrome_ok = ui.selection_fill.is_visible() && ui.selection_fill.opacity() >= 1.0;
    if same_geom && chrome_ok {
        sel_log(format!(
            "update_selection_visual: skip place (geom unchanged) attempt={attempt} y={y:.1}"
        ));
        ui.is_snapping.set(false);
        ui.sel_layout_pending.set(false);
        if motion_enabled() && ui.window.is_visible() && !ui.place_while_hidden.get() {
            set_sel_transition_enabled(ui, true);
        }
        sel_log_verify(ui);
        flush_sel_needs_replace(ui);
        return;
    }
    sel_log(format!(
        "update_selection_visual: place y={y:.1} h={h:.1} opacity→1 (was {:.2}) attempt={attempt} transition=off",
        ui.selection_fill.opacity()
    ));
    ui.is_snapping.set(true);
    set_sel_transition_enabled(ui, false);
    apply_sel_transform(ui, y, h); // may no-op if transform already matches
    show_selection_chrome(ui);
    sel_log_verify(ui);
    let weak = ui.me.borrow().clone();
    ui.list_overlay.add_tick_callback(move |_, _| {
        if let Some(ui) = weak.upgrade() {
            if place_gen != ui.sel_place_gen.get() {
                return glib::ControlFlow::Break;
            }
            ui.is_snapping.set(false);
            if motion_enabled() && ui.window.is_visible() && !ui.place_while_hidden.get() {
                set_sel_transition_enabled(&ui, true);
            }
            sel_log_state(&ui, "update_selection_visual_done");
            flush_sel_needs_replace(&ui);
        }
        glib::ControlFlow::Break
    });
}

/// Cancel deferred snaps/retries and reset highlight session state (hide / reopen).
fn cancel_selection_session(ui: &Ui) {
    ui.sel_place_gen.set(ui.sel_place_gen.get() + 1);
    ui.sel_layout_pending.set(false);
    ui.sel_needs_replace.set(false);
    ui.is_snapping.set(true);
    ui.hover_gen.set(ui.hover_gen.get() + 1);
    ui.hover_frame_pending.set(false);
    ui.hover_pending_index.set(None);
    ui.hover_pointer.set((f64::NAN, f64::NAN));
    set_sel_transition_enabled(ui, false);
    ui.selected_index.set(0);
    ui.list_scroll.vadjustment().set_value(0.0);
    // Reset transform to row-0 slot so reopen never inherits a lower-row y.
    const LIST_PAD_Y: f64 = 4.0;
    apply_sel_transform(ui, LIST_PAD_Y, ROW_HEIGHT as f64);
}

/// If a resize/scale hit during snap, re-place now that snapping has finished.
fn flush_sel_needs_replace(ui: &Ui) {
    if !ui.sel_needs_replace.replace(false) {
        return;
    }
    sel_log("flush_sel_needs_replace → update_selection_visual");
    cache_row_geometry(ui);
    update_selection_visual(ui);
}

/// Snap path used by resize/map — same as update_selection_visual.
fn snap_selection_chrome(ui: &Ui) {
    update_selection_visual(ui);
}

/// Set owned selection; chrome placement updates the footer (no separate selection signal).
fn set_selected_index(ui: &Ui, index: usize, animate: bool) {
    let t0 = std::time::Instant::now();
    let count = ui.hits.borrow().len();
    sel_log(format!(
        "set_selected_index index={index} animate={animate} count={count} prev={}",
        ui.selected_index.get()
    ));
    if count == 0 {
        ui.selected_index.set(0);
        hide_selection_chrome(ui);
        update_path_footer(ui, None);
        return;
    }
    let index = index.min(count - 1);
    if index == ui.selected_index.get() && animate {
        let chrome_ok = ui.selection_fill.is_visible() && ui.selection_fill.opacity() >= 1.0;
        if !chrome_ok {
            sel_log(
                "set_selected_index: unchanged but chrome hidden/dim → update_selection_visual",
            );
            update_selection_visual(ui);
        } else {
            sel_log("set_selected_index: skip (unchanged + animate)");
            update_path_footer(ui, Some(index));
        }
        return;
    }
    ui.selected_index.set(index);
    sync_selection_chrome(ui, animate);
    sel_log_state(ui, "after_set_selected_index");
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    if ms > 2.0 {
        eprintln!("omni: selection handler {ms:.2}ms (want <2ms before animation)");
    }
}

/// Apply highlight from cached row geometry. Animates via CSS transform retarget.
fn sync_selection_chrome(ui: &Ui, animate: bool) {
    if ui.hits.borrow().is_empty() {
        hide_selection_chrome(ui);
        update_path_footer(ui, None);
        return;
    }
    cache_row_geometry(ui);
    let Some((y, h)) = cached_sel_geom(ui).or_else(|| estimate_sel_geom(ui)) else {
        // Keep prior chrome visible; retry via update path — never hide mid-navigation.
        update_selection_visual(ui);
        return;
    };
    let snap = ui.is_snapping.get() || !animate || !motion_enabled();
    if snap {
        snap_selection_chrome(ui);
        return;
    }
    // Retarget: keep transition on and set the new transform (CSS continues from current).
    set_sel_transition_enabled(ui, true);
    apply_sel_transform(ui, y, h);
    show_selection_chrome(ui);
    update_path_footer(ui, Some(ui.selected_index.get()));
}

/// Log-only check after list updates — never on selection moves.
fn verify_selection_geometry(ui: &Ui) {
    if ui.hits.borrow().is_empty() {
        return;
    }
    let index = ui.selected_index.get();
    let Some(row) = ui.list.row_at_index(index as i32) else {
        eprintln!("omni: selection self-check: no row at selected_index={index}");
        return;
    };
    let Some(bounds) = row.compute_bounds(&ui.list_overlay) else {
        return;
    };
    let cached = ui.row_geom.borrow().get(index).copied();
    if let Some((cy, ch)) = cached {
        if (cy - bounds.y() as f64).abs() > 1.0 || (ch - bounds.height() as f64).abs() > 1.0 {
            eprintln!(
                "omni: selection self-check: cache ({cy:.1},{ch:.1}) != row ({:.1},{:.1}) at {index}",
                bounds.y(),
                bounds.height()
            );
        }
    }
    let (y, h) = (ui.sel_y.get(), ui.sel_h.get());
    if (y - bounds.y() as f64).abs() > 1.0 || (h - bounds.height() as f64).abs() > 1.0 {
        eprintln!(
            "omni: selection self-check: highlight ({y:.1},{h:.1}) != row ({:.1},{:.1}) at {index}",
            bounds.y(),
            bounds.height()
        );
    }
}

fn contents_setting_path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| glib::home_dir().join(".local/state"));
    state.join("omni").join("contents")
}

/// The Contents toggle is off unless the user turned it on last time.
fn load_contents_setting() -> bool {
    std::fs::read_to_string(contents_setting_path()).is_ok_and(|s| s.trim() == "on")
}

fn save_contents_setting(on: bool) {
    let path = contents_setting_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, if on { "on\n" } else { "off\n" });
}

fn load_search_provider() -> search_provider::SearchProvider {
    let path = glib::user_config_dir().join("omni/search-engine");
    let id = std::fs::read_to_string(path).unwrap_or_default();
    search_provider::SearchProvider::from_id(id.trim())
}

fn save_search_provider(provider: search_provider::SearchProvider) -> std::io::Result<()> {
    let dir = glib::user_config_dir().join("omni");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("search-engine"), format!("{}\n", provider.id()))
}

fn filter_setting_path() -> PathBuf {
    contents_setting_path().with_file_name("filter")
}

/// Which kinds the filter shows (SHOW_* bits); everything unless set otherwise last time.
fn load_filter_setting() -> u8 {
    let bits = std::fs::read_to_string(filter_setting_path()).ok().and_then(|s| s.trim().parse::<u8>().ok()).unwrap_or(SHOW_ALL) & SHOW_ALL;
    if bits == 0 || bits == OLD_SHOW_ALL { SHOW_ALL } else { bits }
}

fn save_filter_setting(bits: u8) {
    let path = filter_setting_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{bits}\n"));
}

const FILTER_KINDS: [(u8, &str); 4] = [(SHOW_APPS, "Apps"), (SHOW_FILES, "Files"), (SHOW_FOLDERS, "Folders"), (SHOW_SERVICES, "Services")];

/// The filter button's label: "All", or the shown kinds ("Apps + Folders").
fn filter_name(bits: u8) -> String {
    if bits == SHOW_ALL {
        return "All".to_string();
    }
    FILTER_KINDS.iter().filter(|(bit, _)| bits & bit != 0).map(|(_, name)| *name).collect::<Vec<_>>().join(" + ")
}

/// Next filter after clicking `clicked` (SHOW_ALL for the All row). From All, a kind picks
/// only that kind; after that each kind toggles on its own. Nothing left = All again.
fn filter_click(bits: u8, clicked: u8) -> u8 {
    let next = if clicked == SHOW_ALL {
        SHOW_ALL
    } else if bits == SHOW_ALL {
        clicked
    } else {
        bits ^ clicked
    };
    if next == 0 { SHOW_ALL } else { next }
}

/// The filter dropdown: All / Apps / Files / Folders, picked ones lit. It stays open while
/// kinds are picked, and the results update behind it on every click.
fn show_filter_menu(ui: &Ui, keyboard: bool) {
    let Some(me) = ui.me.borrow().upgrade() else { return };
    let menu = gtk::Popover::builder()
        .has_arrow(false)
        .autohide(false)
        .position(gtk::PositionType::Bottom)
        .build();
    menu.add_css_class("omni-menu");
    let items = gtk::Box::new(gtk::Orientation::Vertical, 2);

    // Rows are just the kind names; picked ones are lit (no checkmark column, so the
    // menu fits the button's width).
    let rows: Vec<(u8, &str)> = std::iter::once((SHOW_ALL, "All")).chain(FILTER_KINDS).collect();
    let buttons: Rc<Vec<(u8, gtk::Button)>> = Rc::new(
        rows.iter()
            .map(|(bit, name)| {
                let label = gtk::Label::builder()
                    .label(*name)
                    .xalign(0.0)
                    .halign(gtk::Align::Start)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build();
                (
                    *bit,
                    gtk::Button::builder()
                        .child(&label)
                        .css_classes(["flat", "omni-menu-item"])
                        .build(),
                )
            })
            .collect(),
    );
    let sync = {
        let (buttons, ui) = (buttons.clone(), me.clone());
        move || {
            let bits = ui.filter.get();
            for (bit, button) in buttons.iter() {
                let on = if *bit == SHOW_ALL { bits == SHOW_ALL } else { bits != SHOW_ALL && bits & bit != 0 };
                if on {
                    button.add_css_class("omni-filter-on");
                } else {
                    button.remove_css_class("omni-filter-on");
                }
            }
            if let Some(label) = ui.filter_button.child().and_then(|c| c.first_child()).and_downcast::<gtk::Label>() {
                label.set_label(&filter_name(bits));
            }
        }
    };
    let sync = Rc::new(sync);
    sync();

    for (bit, button) in buttons.iter() {
        let (me, sync, bit) = (me.clone(), sync.clone(), *bit);
        button.connect_clicked(move |_| {
            let bits = filter_click(me.filter.get(), bit);
            me.filter.set(bits);
            save_filter_setting(bits);
            sync();
            // Not while Claude's answer is showing: refreshing would cancel it.
            if !me.answer_reveal.reveals_child() {
                refresh(&me);
            }
        });
        items.append(button);
    }

    // Focus goes into the menu so ↑/↓/Enter work, but the focus ring only shows when it was
    // opened from the keyboard (Ctrl+F); a click shouldn't light up a row nobody picked.
    let first = items.first_child();
    let window = ui.window.clone();
    items.connect_map(move |_| {
        if let Some(first) = &first {
            first.grab_focus();
        }
        window.set_focus_visible(keyboard);
    });
    menu.add_css_class("omni-filter-menu");
    menu.set_child(Some(&items));
    menu.set_parent(&ui.filter_button);
    // Fit-content width, min = All button + 24px. Centered under the button, 6px below;
    // clamp so the popup stays 8px inside the panel's right edge.
    let button = &ui.filter_button;
    let mut widest = 0;
    for (_, b) in buttons.iter() {
        let (_, nat, _, _) = b.measure(gtk::Orientation::Horizontal, -1);
        widest = widest.max(nat);
    }
    let pad = 8; // contents padding 4px × 2
    let width = (widest + pad).max(button.width() + 24).max(1);
    items.set_size_request(width, -1);
    menu.set_halign(gtk::Align::Center);
    let mut ox = 0i32;
    if let (Some(panel), Some(btn)) = (ui.card.compute_bounds(&ui.window), button.compute_bounds(&ui.window)) {
        let btn_cx = btn.x() + btn.width() / 2.0;
        let panel_right = panel.x() + panel.width() - 8.0;
        let pop_right = btn_cx + width as f32 / 2.0;
        if pop_right > panel_right {
            ox = (panel_right - pop_right).round() as i32;
        }
    }
    menu.set_offset(ox, 6);
    menu.set_pointing_to(Some(&gdk::Rectangle::new(0, 0, button.width(), button.height())));

    let weak = Rc::downgrade(&me);
    menu.connect_closed(move |menu| {
        let menu = menu.clone();
        let weak = weak.clone();
        glib::idle_add_local_once(move || {
            if menu.parent().is_some() {
                menu.unparent();
            }
            let Some(ui) = weak.upgrade() else { return };
            if ui.menu.borrow().as_ref() == Some(&menu) {
                ui.menu.replace(None);
                if ui.window.is_visible() {
                    ui.entry.grab_focus();
                }
            }
        });
    });
    if let Some(old) = ui.menu.replace(Some(menu.clone())) {
        old.popdown();
    }
    menu.popup();
}

/// Claude's model and effort dropdown, from the chip in the answer header: two groups
/// with the current choice lit. Stays open while picking; a change applies from the next
/// message (mid-conversation the conversation carries on with the new model).
fn show_model_menu(ui: &Ui) {
    let Some(me) = ui.me.borrow().upgrade() else { return };
    let menu = gtk::Popover::builder()
        .has_arrow(false)
        .autohide(false)
        .position(gtk::PositionType::Bottom)
        .build();
    menu.add_css_class("omni-menu");
    menu.add_css_class("omni-filter-menu");
    menu.add_css_class("omni-model-menu");
    let items = gtk::Box::new(gtk::Orientation::Vertical, 2);

    // (is_model, index, button)
    let mut rows: Vec<(bool, usize, gtk::Button)> = Vec::new();
    for (is_model, heading, names) in [
        (true, "Model", answer::MODELS.iter().map(|m| m.1).collect::<Vec<_>>()),
        (false, "Effort", answer::EFFORTS.iter().map(|e| e.1).collect::<Vec<_>>()),
    ] {
        let header = gtk::Label::builder().label(heading).xalign(0.0).build();
        header.add_css_class("omni-menu-header");
        items.append(&header);
        for (i, name) in names.into_iter().enumerate() {
            let label = gtk::Label::builder().label(name).xalign(0.0).build();
            let button = gtk::Button::builder().child(&label).css_classes(["flat", "omni-menu-item"]).build();
            items.append(&button);
            rows.push((is_model, i, button));
        }
    }
    let rows = Rc::new(rows);
    let sync = {
        let (rows, me) = (rows.clone(), me.clone());
        move || {
            let (model, effort) = me.answer.choice();
            for (is_model, i, button) in rows.iter() {
                if *i == if *is_model { model } else { effort } {
                    button.add_css_class("omni-filter-on");
                } else {
                    button.remove_css_class("omni-filter-on");
                }
            }
        }
    };
    let sync = Rc::new(sync);
    sync();
    for (is_model, i, button) in rows.iter() {
        let (me, sync, is_model, i) = (me.clone(), sync.clone(), *is_model, *i);
        button.connect_clicked(move |_| {
            let (model, effort) = me.answer.choice();
            if is_model {
                me.answer.set_choice(i, effort);
            } else {
                me.answer.set_choice(model, i);
            }
            sync();
        });
    }

    let first = rows.first().map(|r| r.2.clone());
    let window = ui.window.clone();
    items.connect_map(move |_| {
        if let Some(first) = &first {
            first.grab_focus();
        }
        window.set_focus_visible(false);
    });
    menu.set_child(Some(&items));
    menu.set_parent(&ui.answer.chip);

    let weak = Rc::downgrade(&me);
    menu.connect_closed(move |menu| {
        let menu = menu.clone();
        let weak = weak.clone();
        glib::idle_add_local_once(move || {
            if menu.parent().is_some() {
                menu.unparent();
            }
            let Some(ui) = weak.upgrade() else { return };
            if ui.menu.borrow().as_ref() == Some(&menu) {
                ui.menu.replace(None);
                if ui.window.is_visible() {
                    ui.entry.grab_focus();
                }
            }
        });
    });
    if let Some(old) = ui.menu.replace(Some(menu.clone())) {
        old.popdown();
    }
    menu.popup();
}

fn app_hit(info: &gio::AppInfo, name: &str) -> Hit {
    Hit {
        title: name.to_string(),
        subtitle: info.description().map(|d| d.to_string()).unwrap_or_default(),
        subtitle_markup: false,
        icon: info.icon(),
        action: Action::App(info.clone()),
        file_menu: false,
        match_type: None,
    }
}

fn action_hit(app: &apps::App, action: &apps::AppAction) -> Hit {
    Hit {
        title: action.name.clone(),
        subtitle: app.name.clone(),
        subtitle_markup: false,
        icon: app.info.icon(),
        action: Action::AppAction(app.info.clone(), action.clone()),
        file_menu: false,
        match_type: None,
    }
}

/// Nothing typed: the most recent apps (launched from omni) and files/folders (from the
/// shared recent list), newest first. Never queries, prompts or file contents.
fn recent_hits(ui: &Ui) -> Vec<Hit> {
    let show = ui.filter.get();
    let apps = ui.apps.borrow();
    let mut items: Vec<(i64, Hit)> = Vec::new();
    if show & SHOW_APPS != 0 {
        items.extend(apps.recent(MAX_RECENT).into_iter().map(|(app, time)| (time, app_hit(&app.info, &app.name))));
    }
    // Ask for extra: some may be dropped by the filter.
    for f in recent::recent_files(MAX_RECENT * 4) {
        if show & (if f.is_dir { SHOW_FOLDERS } else { SHOW_FILES }) == 0 {
            continue;
        }
        let name = f.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        items.push((
            f.time,
            Hit {
                title: name,
                subtitle: recent::folder_label(&f.path),
                subtitle_markup: false,
                icon: Some(recent::icon(&f.path, f.is_dir)),
                action: Action::Open(f.file),
                file_menu: !f.is_dir,
                match_type: Some(MatchType::Name),
            },
        ));
    }
    items.sort_by(|a, b| b.0.cmp(&a.0));
    items.into_iter().take(MAX_RECENT).map(|(_, h)| h).collect()
}

fn web_hit(ui: &Ui, query: &str) -> Hit {
    Hit {
        title: format!("Search the web for \u{201c}{query}\u{201d}"),
        subtitle: format!("{} \u{b7} Built-in search", ui.search_provider.get().name()),
        subtitle_markup: false,
        icon: Some(ui.search_provider.get().icon()),
        action: Action::Web(query.to_string()),
        file_menu: false,
        match_type: None,
    }
}

/// Row that asks Claude (Claude Code CLI) and shows the answer in the popup.
/// Uses the Claude app's icon when it's installed.
fn service_hit(s: &services::Service) -> Hit {
    let title = if s.description.is_empty() { s.name().to_string() } else { s.description.clone() };
    Hit {
        title,
        subtitle: format!("{} \u{b7} {} \u{b7} right-click for actions", s.unit, s.summary()),
        subtitle_markup: false,
        icon: Some(gio::ThemedIcon::new(if s.active == "failed" { "dialog-warning-symbolic" } else { "system-run-symbolic" }).upcast()),
        action: Action::Service(s.clone()),
        file_menu: false,
        match_type: None,
    }
}

fn claude_hit(ui: &Ui, query: &str) -> Option<Hit> {
    let icon = gio::AppInfo::default_for_uri_scheme("claude")
        .and_then(|a| a.icon())
        .or_else(|| Some(gio::ThemedIcon::new("dialog-question").upcast()));
    Some(Hit {
        title: format!("Ask Claude \u{201c}{query}\u{201d}"),
        subtitle: format!("Quick answer \u{b7} {} \u{b7} Shift+Enter", ui.answer.choice_label()),
        subtitle_markup: false,
        icon,
        action: Action::Claude(query.to_string()),
        file_menu: false,
        match_type: None,
    })
}

fn make_row(ui: &Ui, hit: &Hit) -> gtk::ListBoxRow {
    if matches!(hit.action, Action::Copy(_)) {
        return calc_row(hit);
    }
    let icon = match &hit.action {
        Action::Web(_) | Action::WebIn(..) => search_provider::mark_image(28),
        _ => {
            let image = match &hit.icon {
                Some(i) => gtk::Image::from_gicon(i),
                None => gtk::Image::from_icon_name("application-x-executable"),
            };
            image.set_pixel_size(28);
            image
        }
    };
    icon.set_halign(gtk::Align::Center);
    icon.set_valign(gtk::Align::Center);

    let query = ui.entry.text();
    let title = gtk::Label::builder()
        .use_markup(true)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(1)
        .hexpand(true)
        .build();
    title.add_css_class("omni-title");
    if matches!(hit.action, Action::Info) {
        title.set_text(&hit.title);
    } else {
        title.set_markup(&highlight_title(&hit.title, query.trim()));
    }

    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_valign(gtk::Align::Center);
    text.set_hexpand(true);
    text.append(&title);
    if !hit.subtitle.is_empty() {
        let sub = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(1)
            .hexpand(true)
            .build();
        sub.add_css_class("omni-subtitle");
        if hit.subtitle_markup {
            sub.set_use_markup(true);
            sub.set_markup(&hit.subtitle);
        } else {
            sub.set_text(&hit.subtitle);
        }
        text.append(&sub);
    }

    let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row_box.set_valign(gtk::Align::Center);
    row_box.append(&icon);
    row_box.append(&text);
    if matches!(hit.match_type, Some(MatchType::Content | MatchType::Both)) {
        let badge = gtk::Label::new(Some("content"));
        badge.add_css_class("omni-content-badge");
        badge.set_valign(gtk::Align::Center);
        badge.set_halign(gtk::Align::End);
        row_box.append(&badge);
    }

    let row = gtk::ListBoxRow::builder().child(&row_box).build();
    if matches!(hit.action, Action::Info) {
        row.set_activatable(false);
        row.set_selectable(false);
    }
    row
}

/// Accent-color markup for query matches in a title (weight unchanged).
fn highlight_title(title: &str, query: &str) -> String {
    const ACCENT: &str = "#7aa2f7";
    let words: Vec<Vec<char>> = query
        .split_whitespace()
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase().chars().collect())
        .collect();
    if words.is_empty() {
        return glib::markup_escape_text(title).to_string();
    }
    let title_chars: Vec<char> = title.chars().collect();
    let lower: Vec<char> = title.to_lowercase().chars().collect();
    if title_chars.len() != lower.len() {
        return glib::markup_escape_text(title).to_string();
    }
    let mut mark = vec![false; title_chars.len()];
    for w in &words {
        if w.is_empty() {
            continue;
        }
        let mut i = 0;
        while i + w.len() <= lower.len() {
            if lower[i..i + w.len()] == w[..] {
                for bit in &mut mark[i..i + w.len()] {
                    *bit = true;
                }
                i += w.len();
            } else {
                i += 1;
            }
        }
    }
    let mut out = String::new();
    let mut i = 0;
    while i < title_chars.len() {
        let on = mark[i];
        let mut j = i + 1;
        while j < title_chars.len() && mark[j] == on {
            j += 1;
        }
        let chunk: String = title_chars[i..j].iter().collect();
        let esc = glib::markup_escape_text(&chunk);
        if on {
            out.push_str(&format!("<span foreground=\"{ACCENT}\">{esc}</span>"));
        } else {
            out.push_str(&esc);
        }
        i = j;
    }
    out
}

/// The calculator's answer, laid out like every other row: icon, then the text
/// column (expression small on top, result large underneath), with the shortcut
/// hint on the right. The result can wrap, so long numbers are never cut off.
fn calc_row(hit: &Hit) -> gtk::ListBoxRow {
    let icon = gtk::Image::from_icon_name("accessories-calculator-symbolic");
    icon.set_pixel_size(28);
    icon.set_halign(gtk::Align::Center);
    icon.set_valign(gtk::Align::Center);

    let expression = gtk::Label::builder()
        .label(&hit.subtitle)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(1)
        .hexpand(true)
        .build();
    expression.add_css_class("omni-calc-expression");

    let result = gtk::Label::builder()
        .label(&hit.title)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::Char)
        .max_width_chars(1)
        .hexpand(true)
        .build();
    result.add_css_class("omni-calc-result");

    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_valign(gtk::Align::Center);
    text.set_hexpand(true);
    text.append(&expression);
    text.append(&result);

    let hint = gtk::Label::new(Some("Enter to copy"));
    hint.add_css_class("omni-calc-hint");
    hint.set_valign(gtk::Align::Center);

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    content.set_valign(gtk::Align::Center);
    content.append(&icon);
    content.append(&text);
    content.append(&hint);
    let row = gtk::ListBoxRow::builder().child(&content).build();
    row.add_css_class("omni-calc");
    row
}

fn open_selected_menu(ui: &Ui) {
    if !ui.hits.borrow().is_empty() {
        open_row_menu(ui, ui.selected_index.get(), None);
    }
}

/// A service's menu: the actions that fit its state (no Start for a running one, no
/// Enable for a static one).
fn service_menu_entries(service: &services::Service) -> Vec<MenuEntry> {
    use services::Verb;
    let icon = |name: &str| Some(gio::ThemedIcon::new(name).upcast::<gio::Icon>());
    let status = MenuEntry::Item {
        label: "Status".to_string(),
        detail: None,
        icon: icon("dialog-information-symbolic"),
        action: Action::ServiceStatus(service.clone()),
    };
    std::iter::once(status)
        .chain(Verb::ALL.into_iter()
        .filter(|v| v.applies(service))
        .map(|verb| MenuEntry::Item {
            label: verb.label().to_string(),
            detail: (verb.risky() && service.critical()).then(|| "asks first".to_string()),
            icon: icon(match verb {
                Verb::Start => "media-playback-start-symbolic",
                Verb::Stop => "media-playback-stop-symbolic",
                Verb::Restart => "view-refresh-symbolic",
                Verb::Enable => "emblem-ok-symbolic",
                Verb::Disable => "action-unavailable-symbolic",
            }),
            action: Action::ServiceVerb(service.clone(), verb),
        }))
        .collect()
}

/// Show `systemctl [--user] status UNIT` in the popup in place of the results (read-only,
/// no password). Esc or typing goes back.
fn show_status(ui: &Ui, service: &services::Service) {
    ui.status_gen.set(ui.status_gen.get() + 1);
    let generation = ui.status_gen.get();
    ui.status_title.set_label(&format!("{} \u{b7} {}", service.unit, if service.user { "user service" } else { "system service" }));
    ui.status_body.set_label("Loading\u{2026}");
    ui.status_scroll.vadjustment().set_value(0.0);
    ui.reveal.set_reveal_child(false);
    ui.path_reveal.set_reveal_child(false);
    ui.status_reveal.set_reveal_child(true);

    let Some(me) = ui.me.borrow().upgrade() else { return };
    let service = service.clone();
    glib::spawn_future_local(async move {
        let text = services::status(&service).await;
        if me.status_gen.get() == generation && me.status_reveal.reveals_child() {
            me.status_body.set_label(&text);
        }
    });
}

fn close_status(ui: &Ui) {
    ui.status_gen.set(ui.status_gen.get() + 1);
    ui.status_reveal.set_reveal_child(false);
}

enum MenuEntry {
    Item { label: String, detail: Option<String>, icon: Option<gio::Icon>, action: Action },
    /// A separator and a small heading above the items that follow.
    Header(&'static str),
}

fn dismiss_menu(ui: &Ui) {
    // Leave ui.menu set so connect_closed still matches, unparents, clears, and
    // restores entry focus. take()+popdown() skipped that grab_focus path.
    if let Some(menu) = ui.menu.borrow().as_ref().cloned() {
        menu.popdown();
    }
}

fn click_inside_menu(ui: &Ui, win_x: f64, win_y: f64) -> bool {
    let Some(menu) = ui.menu.borrow().as_ref().cloned() else {
        return false;
    };
    let Some(picked) = ui.window.pick(win_x, win_y, gtk::PickFlags::DEFAULT) else {
        // Popover is often its own surface — pick on the window misses it.
        // Treat that as outside the menu chrome on the main surface.
        return false;
    };
    let mut widget = Some(picked);
    while let Some(w) = widget {
        if w == menu {
            return true;
        }
        widget = w.parent();
    }
    false
}

/// Row under a window-surface point, for right-click. Prefer pick(), fall back to
/// list row_at_y — neither needs a Wayland enter event.
fn row_menu_target(ui: &Ui, win_x: f64, win_y: f64) -> Option<(usize, gdk::Rectangle)> {
    let at = ui
        .window
        .compute_point(&ui.list, &gtk::graphene::Point::new(win_x as f32, win_y as f32))
        .map(|p| gdk::Rectangle::new(p.x().round() as i32, p.y().round() as i32, 1, 1))?;

    let row = ui
        .window
        .pick(win_x, win_y, gtk::PickFlags::DEFAULT)
        .and_then(|picked| {
            let mut widget = Some(picked);
            while let Some(w) = widget {
                if let Ok(r) = w.clone().downcast::<gtk::ListBoxRow>() {
                    return Some(r);
                }
                widget = w.parent();
            }
            None
        })
        .or_else(|| ui.list.row_at_y(at.y()))?;

    Some((row.index() as usize, at))
}

/// The right-click menu of a file or app row, pointing at `at` (list coordinates) or,
/// from the keyboard, the row itself.
/// Files: Open, then "Open with" apps registered for the file's type, guessed from its
/// name so opening the menu never touches the disk. Folders: Open, containing folder,
/// copy path. Apps: Open, then the app's actions. Web search: Search, then the browser's
/// private (and Brave's Tor) window.
/// Returns false when this row has no menu (caller should not swallow the click).
fn open_row_menu(ui: &Ui, index: usize, at: Option<gdk::Rectangle>) -> bool {
    let entries = match ui.hits.borrow().get(index) {
        Some(Hit { action: Action::Open(file), file_menu: true, .. }) => file_menu_entries(file),
        Some(Hit { action: Action::Open(file), file_menu: false, .. }) => folder_menu_entries(file),
        Some(Hit { action: Action::App(info), .. }) => app_menu_entries(ui, info),
        Some(Hit { action: Action::Web(query), .. }) => web_menu_entries(ui, query),
        Some(Hit { action: Action::Service(service), .. }) => service_menu_entries(service),
        _ => return false,
    };
    if entries.is_empty() {
        return false;
    }
    let Some(row) = ui.list.row_at_index(index as i32) else { return false };
    set_selected_index(ui, index, false);
    // From the keyboard there's no cursor: drop from just inside the row's bottom-left.
    let Some(target) = at.or_else(|| {
        let b = row.compute_bounds(&ui.list)?;
        Some(gdk::Rectangle::new(b.x() as i32 + 48, (b.y() + b.height()) as i32 - 8, 1, 1))
    }) else {
        return false;
    };
    show_menu(ui, entries, target);
    true
}

fn file_menu_entries(file: &gio::File) -> Vec<MenuEntry> {
    let Some(path) = file.path() else { return Vec::new() };
    let (content_type, _) = gio::content_type_guess(Some(&path), None);
    let mut apps: Vec<gio::AppInfo> = gio::AppInfo::recommended_for_type(&content_type);
    for app in gio::AppInfo::all_for_type(&content_type) {
        if !apps.iter().any(|a| a.equal(&app)) {
            apps.push(app);
        }
    }
    apps.retain(|a| a.should_show());
    apps.truncate(MAX_OPEN_WITH);

    let default = gio::AppInfo::default_for_type(&content_type, false).map(|a| a.display_name().to_string());
    let mut entries = vec![MenuEntry::Item {
        label: "Open".into(),
        detail: default,
        icon: Some(gio::ThemedIcon::new("document-open-symbolic").upcast()),
        action: Action::Open(file.clone()),
    }];
    if !apps.is_empty() {
        entries.push(MenuEntry::Header("Open with"));
        for app in apps {
            entries.push(MenuEntry::Item {
                label: app.display_name().to_string(),
                detail: None,
                icon: app.icon(),
                action: Action::OpenWith(app.clone(), file.clone()),
            });
        }
    }
    entries
}

/// Folder right-click: Open, open the parent, copy the path (no "Open with" list).
fn folder_menu_entries(file: &gio::File) -> Vec<MenuEntry> {
    let icon = |name: &str| Some(gio::ThemedIcon::new(name).upcast::<gio::Icon>());
    let default = gio::AppInfo::default_for_type("inode/directory", false)
        .map(|a| a.display_name().to_string());
    let mut entries = vec![MenuEntry::Item {
        label: "Open".into(),
        detail: default,
        icon: icon("document-open-symbolic"),
        action: Action::Open(file.clone()),
    }];
    let Some(path) = file.path() else { return entries };
    if let Some(parent) = path.parent() {
        if parent != path {
            entries.push(MenuEntry::Item {
                label: "Open containing folder".into(),
                detail: None,
                icon: icon("folder-symbolic"),
                action: Action::Open(gio::File::for_path(parent)),
            });
        }
    }
    entries.push(MenuEntry::Item {
        label: "Copy path".into(),
        detail: None,
        icon: icon("edit-copy-symbolic"),
        action: Action::Copy(path.display().to_string()),
    });
    entries
}

fn app_menu_entries(ui: &Ui, info: &gio::AppInfo) -> Vec<MenuEntry> {
    let mut entries = vec![MenuEntry::Item {
        label: "Open".into(),
        detail: None,
        icon: info.icon(),
        action: Action::App(info.clone()),
    }];
    let actions = ui.apps.borrow().actions_for(info);
    if !actions.is_empty() {
        entries.push(MenuEntry::Header("Actions"));
        for action in actions {
            entries.push(MenuEntry::Item {
                label: action.name.clone(),
                detail: None,
                icon: Some(gio::ThemedIcon::new("window-new-symbolic").upcast()),
                action: Action::AppAction(info.clone(), action),
            });
        }
    }
    if let Some(id) = dock::id_of(info) {
        let pinned = dock::is_pinned(&id);
        entries.push(MenuEntry::Header("Dock"));
        entries.push(MenuEntry::Item {
            label: if pinned { "Unpin from Dock" } else { "Pin to Dock" }.into(),
            detail: None,
            icon: Some(gio::ThemedIcon::new(if pinned { "list-remove-symbolic" } else { "list-add-symbolic" }).upcast()),
            action: Action::DockPin(id, !pinned),
        });
    }
    entries
}

fn web_menu_entries(ui: &Ui, query: &str) -> Vec<MenuEntry> {
    let engine = ui.web.borrow();
    let mut entries = vec![MenuEntry::Item {
        label: "Search".into(),
        detail: Some(ui.search_provider.get().name().into()),
        icon: Some(gio::ThemedIcon::new("system-search-symbolic").upcast()),
        action: Action::Web(query.to_string()),
    }];
    if !engine.windows().is_empty() {
        entries.push(MenuEntry::Header("Search in"));
        for &window in engine.windows() {
            let (label, icon) = match window {
                web::Window::Private => ("New private window", "window-new-symbolic"),
                web::Window::Tor => ("New private window with Tor", "security-high-symbolic"),
            };
            entries.push(MenuEntry::Item {
                label: label.into(),
                detail: None,
                icon: Some(gio::ThemedIcon::new(icon).upcast()),
                action: Action::WebIn(query.to_string(), window),
            });
        }
    }
    entries
}

fn show_menu(ui: &Ui, entries: Vec<MenuEntry>, target: gdk::Rectangle) {
    let Some(me) = ui.me.borrow().upgrade() else { return };
    let menu = gtk::Popover::builder()
        .has_arrow(false)
        .autohide(false) // we dismiss ourselves; autohide steals the pointer on Wayland
        .position(gtk::PositionType::Bottom)
        .build();
    menu.add_css_class("omni-menu");
    let items = gtk::Box::new(gtk::Orientation::Vertical, 1);
    let fired = Rc::new(Cell::new(false));
    for entry in entries {
        let (label, detail, icon, action) = match entry {
            MenuEntry::Header(text) => {
                items.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                let header = gtk::Label::builder().label(text).xalign(0.0).build();
                header.add_css_class("omni-menu-header");
                items.append(&header);
                continue;
            }
            MenuEntry::Item { label, detail, icon, action } => (label, detail, icon, action),
        };
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        if let Some(icon) = icon {
            let image = gtk::Image::from_gicon(&icon);
            image.set_pixel_size(16);
            content.append(&image);
        }
        // Cap natural width so long labels (e.g. Tor window) don't stretch the menu.
        let title = gtk::Label::builder()
            .label(label)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(22)
            .build();
        content.append(&title);
        if let Some(detail) = detail {
            let detail = gtk::Label::builder()
                .label(detail)
                .css_classes(["omni-menu-detail"])
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .max_width_chars(10)
                .build();
            content.append(&detail);
        }
        let button = gtk::Button::builder().child(&content).css_classes(["flat", "omni-menu-item"]).build();
        let (me, menu, fired) = (me.clone(), menu.clone(), fired.clone());
        button.connect_clicked(move |_| {
            // One action per menu: a double click or a held Enter mustn't run it twice
            // (restart a service twice, launch an app twice).
            if fired.replace(true) {
                return;
            }
            menu.popdown();
            let (me, action) = (me.clone(), action.clone());
            glib::idle_add_local_once(move || run(&me, action));
        });
        items.append(&button);
    }

    // Don't grab_focus the first item — right-click menus shouldn't open with a
    // keyboard focus ring on the top option (looks selected; easy to activate by mistake).
    menu.set_child(Some(&items));
    menu.set_parent(&ui.list);
    // Top-left corner just right of and below the pointer.
    menu.set_halign(gtk::Align::Start);
    menu.set_offset(2, 4);
    menu.set_pointing_to(Some(&target));

    let weak = Rc::downgrade(&me);
    menu.connect_closed(move |menu| {
        let menu = menu.clone();
        let weak = weak.clone();
        // Unparent after "closed" has finished, not while GTK is still emitting it.
        glib::idle_add_local_once(move || {
            if menu.parent().is_some() {
                menu.unparent();
            }
            let Some(ui) = weak.upgrade() else { return };
            if ui.menu.borrow().as_ref() == Some(&menu) {
                ui.menu.replace(None);
                if ui.window.is_visible() {
                    ui.entry.grab_focus();
                }
            }
        });
    });
    if let Some(old) = ui.menu.replace(Some(menu.clone())) {
        old.popdown();
    }
    menu.popup();
}

fn selection_scroll_steps(pending: &Cell<f64>, dy: f64, unit: gdk::ScrollUnit) -> i32 {
    let dy = if unit == gdk::ScrollUnit::Surface { dy / ROW_HEIGHT as f64 } else { dy };
    let total = pending.get() + dy;
    let steps = total.trunc() as i32;
    pending.set(total.fract());
    steps
}

fn move_selection(ui: &Ui, delta: i32) {
    let Some(next) = selection_target(ui.selected_index.get(), ui.hits.borrow().len(), delta) else {
        return;
    };
    // Animation first; scroll (layout) waits for the next idle.
    set_selected_index(ui, next, true);
    let weak = ui.me.borrow().clone();
    glib::idle_add_local_once(move || {
        let Some(ui) = weak.upgrade() else { return };
        scroll_to_cached(&ui, ui.selected_index.get());
    });
}

fn selection_target(current: usize, count: usize, delta: i32) -> Option<usize> {
    if count == 0 {
        return None;
    }
    let next = current.saturating_add_signed(delta as isize).min(count - 1);
    (next != current).then_some(next)
}

/// Scroll using cached row geometry — no layout reads on the selection hot path.
fn scroll_to_cached(ui: &Ui, index: usize) {
    let Some(&(top, h)) = ui.row_geom.borrow().get(index) else {
        return;
    };
    let bottom = top + h;
    let adj = ui.list_scroll.vadjustment();
    if top < adj.value() {
        adj.set_value(top);
    } else if bottom > adj.value() + adj.page_size() {
        adj.set_value(bottom - adj.page_size());
    }
}

fn activate(ui: &Ui, index: usize) {
    probe::log(format!("activate {index}"));
    let Some(action) = ui.hits.borrow().get(index).map(|h| h.action.clone()) else { return };
    if matches!(action, Action::Info) {
        return;
    }
    run(ui, action);
}

fn run(ui: &Ui, action: Action) {
    if matches!(action, Action::Info) {
        return;
    }
    if let Action::Web(query) = &action {
        probe::log("web search: enter");
        set_ui_mode(ui, UiMode::WebSearch, true);
        ui.search_view.search(ui.search_provider.get(), query);
        return;
    }
    if let Action::Claude(query) = action {
        // Stays open: the answer streams into the popup.
        ui.reveal.set_reveal_child(false);
        ui.answer_reveal.set_reveal_child(true);
        ui.answer.ask(&query);
        // The bar becomes the reply box for follow-ups.
        ui.chatting.set(true);
        ui.entry.set_text("");
        ui.entry.set_placeholder_text(Some(REPLY_PLACEHOLDER));
        ui.entry.grab_focus();
        return;
    }
    // A service: Enter or a click only selects it. Its actions are on right-click (or
    // the Menu key), so nothing happens to a service by accident.
    if let Action::Service(_) = action {
        return;
    }
    if let Action::ServiceStatus(service) = action {
        show_status(ui, &service);
        return;
    }
    if let Action::ServiceVerb(service, verb) = action {
        let (unit, critical) = (service.unit.clone(), service.critical());
        let go = move |ui: &Ui| {
            // Close first: system services ask for the password in the desktop's polkit
            // dialog, which needs the keyboard omni is holding.
            hide(ui);
            services::run(&service, verb);
        };
        if verb.risky() && critical {
            let text = format!(
                "{} {unit}?\nIt keeps your session working (display, login, network, sound or remote access).",
                verb.label()
            );
            confirm(ui, &text, "", go);
        } else {
            go(ui);
        }
        return;
    }
    // Close first: launching can block (D-Bus activated apps wait up to 25 s), and the
    // popup holds the keyboard exclusively while it's up.
    let display = gtk::prelude::WidgetExt::display(&ui.window);
    if let Action::Copy(text) = &action {
        display.clipboard().set_text(text);
    }
    let ctx = display.app_launch_context();
    for (key, value) in util::missing_x11_env() {
        ctx.setenv(key, value);
    }
    hide(ui);
    probe::log("hide done, launching");
    match action {
        Action::Web(query) => ui.web.borrow().search(&query),
        Action::WebIn(query, window) => ui.web.borrow().search_in(&query, window),
        Action::App(info) => {
            apps::apply_env_overrides(info.id().as_deref(), ctx.upcast_ref());
            match info.launch(&[], Some(&ctx)) {
                Ok(()) => ui.apps.borrow_mut().record_launch(&info),
                Err(e) => {
                    let name = info.display_name();
                    eprintln!("omni: failed to launch {name}: {e}");
                    notify_user(&format!("Couldn't launch {name}"), &e.to_string());
                }
            }
        }
        Action::AppAction(info, action) => {
            apps::apply_env_overrides(info.id().as_deref(), ctx.upcast_ref());
            match apps::launch_action(&info, &action, ctx.upcast_ref()) {
                Ok(()) => ui.apps.borrow_mut().record_launch(&info),
                Err(e) => {
                    let name = info.display_name();
                    eprintln!("omni: failed to launch {name} ({}): {e}", action.name);
                    notify_user(&format!("Couldn't launch {name}"), &e.to_string());
                }
            }
        }
        Action::Open(file) => {
            let uri = file.uri();
            apps::apply_env_overrides_for_uri(&uri, ctx.upcast_ref());
            match gio::AppInfo::launch_default_for_uri(&uri, Some(&ctx)) {
                Ok(()) => recent::record(&file),
                Err(e) => {
                    eprintln!("omni: couldn't open {uri}: {e}");
                    notify_user("Couldn't open", &e.to_string());
                }
            }
        }
        Action::OpenWith(info, file) => {
            apps::apply_env_overrides(info.id().as_deref(), ctx.upcast_ref());
            match info.launch(&[file.clone()], Some(&ctx)) {
                Ok(()) => recent::record(&file),
                Err(e) => {
                    let name = info.display_name();
                    eprintln!("omni: couldn't open {} with {name}: {e}", file.uri());
                    notify_user(&format!("Couldn't open with {name}"), &e.to_string());
                }
            }
        }
        Action::Copy(_) => {}
        Action::DockPin(id, pinned) => dock::set_pinned(&id, pinned),
        Action::Info => {}
        Action::Claude(_) | Action::Service(_) | Action::ServiceVerb(..) | Action::ServiceStatus(_) => unreachable!(),
    }
    probe::log("launch returned");
}

/// Desktop notification after the popup has closed (same pattern as services.rs).
fn notify_user(title: &str, body: &str) {
    let body = glib::markup_escape_text(body);
    let _ = gio::Subprocess::newv(
        &["notify-send", "-a", "omni", "-i", "dialog-error", "--", title, body.as_str()]
            .map(std::ffi::OsStr::new),
        gio::SubprocessFlags::NONE,
    );
}

fn set_ui_mode(ui: &Ui, mode: UiMode, animate: bool) {
    ui.mode.set(mode);
    ui.main_stack
        .set_transition_duration(if animate && motion_enabled() { 160 } else { 0 });
    ui.main_stack.set_visible_child_name(mode.stack_name());
}

fn show_settings(ui: &Ui) {
    set_ui_mode(ui, ui.mode.get().after(UiModeAction::OpenSettings), true);
    ui.settings_back.grab_focus();
}

fn show_launcher(ui: &Ui) {
    ui.search_view.stop();
    set_ui_mode(ui, ui.mode.get().after(UiModeAction::Back), true);
    ui.entry.grab_focus();
}

fn toggle(ui: &Rc<Ui>) {
    if ui.window.is_visible() {
        hide(ui);
    } else {
        probe::log("toggle enter");
        ui.open_t0.set(Some(Instant::now()));
        let apps_n = ui.apps.borrow().len();
        let sparql_ready = ui.sparql.borrow().is_some();
        open_log(
            ui,
            &format!("show_request apps_cached={apps_n} indexer_ready={sparql_ready}"),
        );
        // Suppress hover until a real pointer move after show (~150ms).
        ui.hover_suppress_until
            .set(glib::monotonic_time() + 150_000);
        ui.hover_pointer.set((f64::NAN, f64::NAN));
        ui.hover_pending_index.set(None);
        ui.hover_frame_pending.set(false);

        // Fresh session: row 0, top scroll, transition off — place before present.
        cancel_selection_session(ui);
        ui.place_while_hidden.set(true);
        open_log(ui, "before_refresh");
        refresh(ui);
        ui.selected_index.set(0);
        ui.list_scroll.vadjustment().set_value(0.0);
        open_log(
            ui,
            &format!(
                "first_results_displayed count={} (apps+icons from cache)",
                ui.hits.borrow().len()
            ),
        );
        // Unconditional snap to row 0 (no animation) + footer from selectedIndex.
        set_sel_transition_enabled(ui, false);
        ui.is_snapping.set(true);
        update_selection_visual(ui);
        reopen_log(
            ui,
            &format!(
                "pre_present snap y={:.1} h={:.1} selectedIndex={} footer={:?} transition_on={}",
                ui.sel_y.get(),
                ui.sel_h.get(),
                ui.selected_index.get(),
                ui.path_full.borrow().clone(),
                ui.sel_transition_on.get(),
            ),
        );
        load_services(ui);
        open_log(ui, "before_place_on_focused_monitor");
        place_on_focused_monitor(ui);
        open_log(ui, "after_place_on_focused_monitor");
        // Re-read the default browser/engine in case it changed since last time.
        let t_web = Instant::now();
        *ui.web.borrow_mut() = web::Engine::detect();
        open_log(
            ui,
            &format!("web_engine_detect done ({:.1}ms)", t_web.elapsed().as_secs_f64() * 1000.0),
        );
        // From here on the popup holds the keyboard: the watchdog must hear from us.
        watchdog::set_open(true);
        let beat = glib::timeout_add_local(Duration::from_millis(100), || {
            watchdog::beat();
            glib::ControlFlow::Continue
        });
        if let Some(old) = ui.heartbeat.replace(Some(beat)) {
            old.remove();
        }
        // hide() drops the grab to None; take it back as the surface is shown.
        ui.window.set_keyboard_mode(KeyboardMode::Exclusive);
        open_log(ui, "before_present");
        ui.window.present();
        ui.place_while_hidden.set(false);
        // Re-apply footer now that the path strip has a real width.
        update_path_footer(ui, if ui.hits.borrow().is_empty() {
            None
        } else {
            Some(ui.selected_index.get())
        });
        open_log(
            ui,
            &format!(
                "window_presented mapped={}",
                ui.window.is_mapped()
            ),
        );
        ui.entry.grab_focus();
        open_log(ui, "search_entry_focused");
        // Next frame: confirm snap, re-enable transition, log assertions.
        let weak = Rc::downgrade(ui);
        let place_gen = ui.sel_place_gen.get();
        ui.window.add_tick_callback(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                if ui.sel_place_gen.get() != place_gen {
                    return glib::ControlFlow::Break;
                }
                open_log(&ui, "first_frame_rendered");
                cache_row_geometry(&ui);
                if let Some((y, h)) = cached_sel_geom(&ui).or_else(|| estimate_sel_geom(&ui)) {
                    set_sel_transition_enabled(&ui, false);
                    apply_sel_transform(&ui, y, h);
                    show_selection_chrome(&ui);
                    update_path_footer(&ui, Some(ui.selected_index.get()));
                }
                reopen_log(
                    &ui,
                    &format!(
                        "first_frame y={:.1} row0={:?} selectedIndex={} footer={:?} transition_on={} query={:?}",
                        ui.sel_y.get(),
                        ui.row_geom.borrow().first().copied(),
                        ui.selected_index.get(),
                        ui.path_full.borrow().clone(),
                        ui.sel_transition_on.get(),
                        ui.entry.text().to_string(),
                    ),
                );
                let weak = ui.me.borrow().clone();
                ui.list_overlay.add_tick_callback(move |_, _| {
                    if let Some(ui) = weak.upgrade() {
                        if ui.sel_place_gen.get() != place_gen {
                            return glib::ControlFlow::Break;
                        }
                        ui.is_snapping.set(false);
                        if motion_enabled() {
                            set_sel_transition_enabled(&ui, true);
                        }
                    }
                    glib::ControlFlow::Break
                });
            }
            glib::ControlFlow::Break
        });
        sel_log_state(ui, "toggle_open");
        maybe_run_sel_diag(ui);
        maybe_run_reopen_diag(ui);
        // AppInfoMonitor keeps the app list current. Do not reload here: AppInfo::all()
        // blocks GTK's UI thread and made the just-presented popup unresponsive.
    }
}

fn reopen_log(ui: &Ui, msg: impl std::fmt::Display) {
    if std::env::var_os("OMNI_LOG_REOPEN").is_none() && std::env::var_os("OMNI_LOG_SEL").is_none()
    {
        return;
    }
    let _ = ui;
    eprintln!("omni[reopen]: {msg}");
}

/// OMNI_REOPEN_DIAG=1 — automated A–D checks for reopen snap + footer.
fn maybe_run_reopen_diag(ui: &Rc<Ui>) {
    if std::env::var_os("OMNI_REOPEN_DIAG").is_none() {
        return;
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return; // only once per process (toggle-from-diag must not re-enter)
    }
    sel_log("REOPEN_DIAG start");
    let weak = Rc::downgrade(ui);
    // Delay so first open settles, then run A→D.
    glib::timeout_add_local_once(Duration::from_millis(200), move || {
        let Some(ui) = weak.upgrade() else { return };
        let n = ui.hits.borrow().len();
        if n == 0 {
            eprintln!("omni[reopen]: DIAG abort — no hits");
            return;
        }
        let bottom = n - 1;
        // Test A: select bottom, close, reopen — highlight must be at row 0, no transition.
        set_selected_index(&ui, bottom, false);
        reopen_log(
            &ui,
            &format!("A_prep selectedIndex={} y={:.1}", ui.selected_index.get(), ui.sel_y.get()),
        );
        let weak = Rc::downgrade(&ui);
        glib::timeout_add_local_once(Duration::from_millis(80), move || {
            let Some(ui) = weak.upgrade() else { return };
            hide(&ui);
            let weak = Rc::downgrade(&ui);
            glib::timeout_add_local_once(Duration::from_millis(80), move || {
                let Some(ui) = weak.upgrade() else { return };
                toggle(&ui);
                let weak = Rc::downgrade(&ui);
                glib::timeout_add_local_once(Duration::from_millis(120), move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let y = ui.sel_y.get();
                    let row0 = ui.row_geom.borrow().first().copied();
                    let footer = ui.path_full.borrow().clone();
                    let idx = ui.selected_index.get();
                    let trans = ui.sel_transition_on.get();
                    let expect_y = row0.map(|(y, _)| y).unwrap_or(4.0);
                    let ok_a = idx == 0 && (y - expect_y).abs() < 1.0 && !trans;
                    let ok_b = !footer.is_empty()
                        || ui
                            .hits
                            .borrow()
                            .first()
                            .map(footer_path_for)
                            .is_some_and(|p| p.is_empty());
                    let footer_matches = ui
                        .hits
                        .borrow()
                        .get(0)
                        .map(|h| footer_path_for(h) == footer)
                        .unwrap_or(footer.is_empty());
                    eprintln!(
                        "omni[reopen]: TEST_A ok={ok_a} y={y:.1} expect={expect_y:.1} selectedIndex={idx} transition_on={trans}"
                    );
                    eprintln!(
                        "omni[reopen]: TEST_B ok={} selectedIndex={idx} footer={footer:?} matches_row0={footer_matches}",
                        ok_b && footer_matches && idx == 0
                    );
                    eprintln!(
                        "omni[reopen]: TEST_C ok={} selectedIndex={idx} (hover suppressed; expect 0)",
                        idx == 0
                    );
                    // Test D: type, close, reopen — empty query, row 0, footer set.
                    ui.entry.set_text("claude");
                    let weak = Rc::downgrade(&ui);
                    glib::timeout_add_local_once(Duration::from_millis(100), move || {
                        let Some(ui) = weak.upgrade() else { return };
                        hide(&ui);
                        let weak = Rc::downgrade(&ui);
                        glib::timeout_add_local_once(Duration::from_millis(80), move || {
                            let Some(ui) = weak.upgrade() else { return };
                            toggle(&ui);
                            let weak = Rc::downgrade(&ui);
                            glib::timeout_add_local_once(Duration::from_millis(120), move || {
                                let Some(ui) = weak.upgrade() else { return };
                                let q = ui.entry.text().to_string();
                                let idx = ui.selected_index.get();
                                let footer = ui.path_full.borrow().clone();
                                let ok_d = q.is_empty() && idx == 0 && (!footer.is_empty()
                                    || ui.hits.borrow().first().map(footer_path_for).is_some_and(|p| p.is_empty()));
                                eprintln!(
                                    "omni[reopen]: TEST_D ok={ok_d} query={q:?} selectedIndex={idx} footer={footer:?}"
                                );
                                eprintln!("omni[reopen]: DIAG done");
                                hide(&ui);
                            });
                        });
                    });
                });
            });
        });
    });
}

fn open_log(ui: &Ui, stage: &str) {
    let ms = ui
        .open_t0
        .get()
        .map(|t| t.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    probe::log(format!("open +{ms:.1}ms {stage}"));
    if std::env::var_os("OMNI_LOG_OPEN").is_none() {
        return;
    }
    eprintln!("omni[open]: +{ms:7.1}ms {stage}");
}

/// OMNI_SEL_DIAG=claude — type letter-by-letter, backspace to empty, retype, then fast paste.
fn maybe_run_sel_diag(ui: &Rc<Ui>) {
    let Some(text) = std::env::var_os("OMNI_SEL_DIAG") else {
        return;
    };
    let text = text.to_string_lossy().into_owned();
    if text.is_empty() {
        return;
    }
    sel_log(format!("SEL_DIAG start typing {text:?}"));
    let weak = Rc::downgrade(ui);
    let chars: Vec<char> = text.chars().collect();
    // Phases: 0=type, 1=backspace, 2=retype slow, 3=clear+fast full string, 4=done
    let phase = Cell::new(0u8);
    let step = Cell::new(0usize);
    glib::timeout_add_local(Duration::from_millis(220), move || {
        let Some(ui) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if !ui.window.is_visible() {
            return glib::ControlFlow::Break;
        }
        let p = phase.get();
        match p {
            0 | 2 => {
                let i = step.get();
                if i >= chars.len() {
                    if p == 0 {
                        phase.set(1);
                        step.set(chars.len());
                        sel_log("SEL_DIAG phase=backspace");
                    } else {
                        phase.set(3);
                        step.set(0);
                        sel_log("SEL_DIAG phase=fast");
                    }
                    return glib::ControlFlow::Continue;
                }
                let so_far: String = chars[..=i].iter().collect();
                step.set(i + 1);
                sel_log(format!(
                    "SEL_DIAG type phase={p} step={} text={so_far:?}",
                    i + 1
                ));
                ui.entry.set_text(&so_far);
            }
            1 => {
                let i = step.get();
                if i == 0 {
                    phase.set(2);
                    step.set(0);
                    sel_log("SEL_DIAG phase=retype");
                    return glib::ControlFlow::Continue;
                }
                let next = i - 1;
                step.set(next);
                let so_far: String = chars[..next].iter().collect();
                sel_log(format!("SEL_DIAG backspace to {so_far:?}"));
                ui.entry.set_text(&so_far);
            }
            3 => {
                // Quick: clear then set full string in one shot (burst typing).
                ui.entry.set_text("");
                sel_log("SEL_DIAG fast clear");
                let full = text.clone();
                let weak = Rc::downgrade(&ui);
                glib::timeout_add_local_once(Duration::from_millis(40), move || {
                    if let Some(ui) = weak.upgrade() {
                        sel_log(format!("SEL_DIAG fast set {full:?}"));
                        ui.entry.set_text(&full);
                        let weak = Rc::downgrade(&ui);
                        glib::timeout_add_local_once(Duration::from_millis(500), move || {
                            if let Some(ui) = weak.upgrade() {
                                sel_log("SEL_DIAG done");
                                hide(&ui);
                            }
                        });
                    }
                });
                phase.set(4);
            }
            _ => return glib::ControlFlow::Break,
        }
        glib::ControlFlow::Continue
    });
}

/// Enter in a conversation: send the bar's text as a follow-up. While Claude is still
/// answering the text stays in the bar, to send once it's done.
fn send_reply(ui: &Ui) {
    let text = ui.entry.text().trim().to_string();
    if ui.answer.reply(&text) {
        ui.entry.set_text("");
    }
}

/// Leave the conversation (it's forgotten) and go back to searching.
fn end_chat(ui: &Ui) {
    ui.chatting.set(false);
    ui.entry.set_placeholder_text(Some(SEARCH_PLACEHOLDER));
    ui.entry.set_text(""); // refreshes: closes the answer, shows recents
    refresh(ui);
}

/// Re-read the service list in the background (~1.5 s); results already on screen are
/// refreshed when it arrives.
fn load_services(ui: &Ui) {
    // One load at a time: toggling quickly shouldn't stack systemctl runs, and a slow old
    // load mustn't overwrite a newer list.
    if ui.services_loading.replace(true) {
        return;
    }
    let Some(me) = ui.me.borrow().upgrade() else { return };
    glib::spawn_future_local(async move {
        let list = gio::spawn_blocking(services::load).await;
        me.services_loading.set(false);
        let Ok(list) = list else { return };
        me.services.replace(list);
        if me.window.is_visible() && !me.chatting.get() && !me.entry.text().trim().is_empty() {
            refresh(&me);
        }
    });
}

fn close_answer(ui: &Ui) {
    ui.answer.cancel();
    ui.answer_reveal.set_reveal_child(false);
}

/// Ask before doing something with consequences, on a dimmed layer over the card: `text`
/// is the question, `detail` a line under it. Yes runs `action`; No (Esc, a click on the
/// dimmed area) just goes back.
fn confirm(ui: &Ui, text: &str, detail: &str, action: impl FnOnce(&Ui) + 'static) {
    show_confirm(ui, text, detail, false);
    ui.confirm_yes.set_label("Yes");
    ui.confirm_no.set_label("No");
    ui.confirm_always.set_visible(false);
    ui.confirm_action.replace(Some(Box::new(action)));
    ui.confirm_no_action.take();
    ui.confirm_always_action.take();
    ui.confirm_yes.grab_focus();
    ui.window.set_focus_visible(false); // no focus ring until the keyboard is used
}

/// Fill in and show the confirmation layer. `code`: the detail is a command or path,
/// shown whole (wrapped, monospace) instead of shortened.
fn show_confirm(ui: &Ui, text: &str, detail: &str, code: bool) {
    ui.confirm_text.set_label(text);
    ui.confirm_link_label.set_label(detail);
    ui.confirm_link_label.set_visible(!detail.is_empty());
    ui.confirm_link_label.set_wrap(code);
    ui.confirm_link_label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    ui.confirm_link_label.set_ellipsize(if code { gtk::pango::EllipsizeMode::None } else { gtk::pango::EllipsizeMode::Middle });
    if code {
        ui.confirm_link_label.add_css_class("omni-confirm-code");
    } else {
        ui.confirm_link_label.remove_css_class("omni-confirm-code");
    }
    ui.confirm.set_visible(true);
    ui.confirm_shown_at.set(glib::monotonic_time());
    ui.confirm_scroll.vadjustment().set_value(0.0);
}

/// Claude Code wants to do something that needs permission: Deny / Allow once / Always
/// allow (for this conversation). Deny has the focus, so an Enter meant for something
/// else can never approve; Esc and a click on the dim also deny.
fn ask_permission(ui: &Ui, ask: answer::PermissionAsk) {
    show_confirm(ui, &ask.title, &ask.detail, true);
    ui.confirm_yes.set_label("Allow once");
    ui.confirm_no.set_label("Deny");
    ui.confirm_always.set_label(ask.remember.unwrap_or("Always allow"));
    ui.confirm_always.set_tooltip_text(Some(if ask.remember == Some("Always allow this file") {
        "Allow edits to this file for the rest of this conversation"
    } else {
        "Allow this kind of action for the rest of this conversation"
    }));
    ui.confirm_always.set_visible(ask.remember.is_some());
    ui.confirm_action.replace(Some(Box::new(|ui: &Ui| ui.answer.respond(answer::Decision::Once))));
    ui.confirm_always_action.replace(Some(Box::new(|ui: &Ui| ui.answer.respond(answer::Decision::Always))));
    ui.confirm_no_action.replace(Some(Box::new(|ui: &Ui| ui.answer.respond(answer::Decision::Deny))));
    ui.confirm_no.grab_focus();
    ui.window.set_focus_visible(false);
}

#[derive(Clone, Copy, PartialEq)]
enum Answered {
    No,
    Yes,
    Always,
}

/// Close the confirmation and run the chosen answer's action (each runs at most once: the
/// others are dropped). Runs on the next idle tick, outside the button's own signal.
fn finish_confirm(ui: &Ui, answered: Answered) {
    // A prompt that popped up under the pointer or mid-keystroke can't be approved by that
    // same click or key: approving needs a moment after it appears (No always works).
    if answered != Answered::No && glib::monotonic_time() - ui.confirm_shown_at.get() < CONFIRM_ARM_US {
        return;
    }
    let yes = ui.confirm_action.take();
    let no = ui.confirm_no_action.take();
    let always = ui.confirm_always_action.take();
    let chosen = match answered {
        Answered::Yes => yes,
        Answered::No => no,
        Answered::Always => always,
    };
    let was_visible = ui.confirm.is_visible();
    ui.confirm.set_visible(false);
    if was_visible && ui.window.is_visible() {
        ui.entry.grab_focus();
    }
    let (Some(action), Some(me)) = (chosen, ui.me.borrow().upgrade()) else { return };
    glib::idle_add_local_once(move || action(&me));
}

/// Text safe to show: invisible and direction-changing characters (which can make a
/// URL display a different host from the one that opens) become visible codes.
/// Newline and tab stay; they don't reorder text.
fn visible(text: &str) -> String {
    text.chars()
        .map(|c| {
            let invisible = matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{00AD}')
                || (c.is_control() && c != '\n' && c != '\t');
            if invisible { format!("\u{27e8}U+{:04X}\u{27e9}", c as u32) } else { c.to_string() }
        })
        .collect()
}

/// Host to show for a link. The scheme is dropped, then everything through the last
/// `@` — so `https://trusted.example/…@evil.example/…` shows `evil.example`, not the
/// name before the `@` — and the host ends at the first `/`, `?`, or `#`.
fn link_host(uri: &str) -> String {
    let rest = uri.split_once("://").map(|(_, rest)| rest).unwrap_or(uri);
    let rest = rest.rsplit_once('@').map(|(_, host)| host).unwrap_or(rest);
    visible(rest.split(['/', '?', '#']).next().unwrap_or(""))
}

/// A link in Claude's answer: opening it closes omni (ending the temporary conversation),
/// so ask first. The host is the question (it can't be ellipsized away); the detail is
/// the whole URL, wrapped, with bidi and invisible characters written out.
fn confirm_link(ui: &Ui, uri: &str) {
    let launch = uri.to_string();
    let host = link_host(&launch);
    let shown = visible(&launch);
    // code=true: wrap the URL, don't middle-ellipsize it (that hides the real host).
    show_confirm(ui, &format!("Open {host}?\nThis will close Omni, continue?"), &shown, true);
    ui.confirm_yes.set_label("Yes");
    ui.confirm_no.set_label("No");
    ui.confirm_always.set_visible(false);
    ui.confirm_action.replace(Some(Box::new(move |ui: &Ui| {
        hide(ui);
        if let Err(e) = gio::AppInfo::launch_default_for_uri(&launch, None::<&gio::AppLaunchContext>) {
            eprintln!("omni: couldn't open {launch}: {e}");
        }
    })));
    ui.confirm_no_action.take();
    ui.confirm_always_action.take();
    ui.confirm_yes.grab_focus();
    ui.window.set_focus_visible(false);
}

/// Take the layer away without running any answer (its question no longer applies).
fn dismiss_confirm(ui: &Ui) {
    ui.confirm_action.take();
    ui.confirm_no_action.take();
    ui.confirm_always_action.take();
    if ui.confirm.is_visible() {
        ui.confirm.set_visible(false);
        if ui.window.is_visible() {
            ui.entry.grab_focus();
        }
    }
}

/// No / Esc / a click on the dim / omni closing: the "no" answer (a permission prompt is
/// denied), then the layer goes away.
fn close_confirm(ui: &Ui) {
    if !ui.confirm.is_visible() {
        return;
    }
    finish_confirm(ui, Answered::No);
}

fn hide(ui: &Ui) {
    probe::log("hide enter");
    ui.search_view.stop();
    // Release the exclusive grab and flush it before unmapping. Launching blocks this
    // thread after hide() returns, and an unflushed unmap can leave the grab held
    // after the watchdog stands down.
    ui.window.set_keyboard_mode(KeyboardMode::None);
    gtk::prelude::WidgetExt::display(&ui.window).flush();
    close_confirm(ui);
    if let Some(menu) = ui.menu.take() {
        menu.popdown();
    }
    close_answer(ui);
    ui.chatting.set(false);
    ui.entry.set_placeholder_text(Some(SEARCH_PLACEHOLDER));
    // Cancel snaps/retries and reset selection chrome so reopen never animates from a lower row.
    cancel_selection_session(ui);
    // Cancel Contents backends and bump gen so in-flight batches can't revive "searching…".
    invalidate_content_cache(ui);
    ui.entry.set_text("");
    // Clear footer so reopen reapplies from selectedIndex (no stale skip / flash).
    *ui.path_full.borrow_mut() = String::new();
    *ui.path_trunc_cache.borrow_mut() = None;
    ui.path_label.set_text("");
    refresh(ui);
    // After clear+refresh, force row 0 + top scroll again (refresh may have kept an id).
    ui.selected_index.set(0);
    ui.list_scroll.vadjustment().set_value(0.0);
    set_sel_transition_enabled(ui, false);
    const LIST_PAD_Y: f64 = 4.0;
    apply_sel_transform(ui, LIST_PAD_Y, ROW_HEIGHT as f64);
    update_path_footer(ui, None);
    ui.reveal.set_reveal_child(false);
    // Keep path strip height reserved when results exist next open; hide with the list.
    ui.path_reveal.set_reveal_child(false);
    set_ui_mode(ui, ui.mode.get().after(UiModeAction::Close), false);
    ui.window.set_visible(false);
    ui.place_while_hidden.set(false);
    watchdog::set_open(false);
    if let Some(beat) = ui.heartbeat.take() {
        beat.remove();
    }
    probe::log("hide done");
}

/// Keyboard safety net. While the popup is open it holds the keyboard exclusively, so a
/// frozen omni would lock the desktop. The UI thread checks in every 100 ms while open; if
/// it goes silent for 2 s, a separate thread kills the process. Hyprland then removes the
/// popup and releases the keyboard, and the next keypress starts a fresh omni.
mod watchdog {
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::time::Duration;

    static OPEN: AtomicBool = AtomicBool::new(false);
    static LAST_BEAT_US: AtomicI64 = AtomicI64::new(0);
    const STALL_US: i64 = 2_000_000;

    pub fn beat() {
        LAST_BEAT_US.store(gtk::glib::monotonic_time(), Ordering::Release);
    }

    pub fn set_open(open: bool) {
        // Timestamp first, then the flag, both Release. The watchdog Acquire-loads
        // the flag and only then the timestamp, so "open" is never seen with a stale beat.
        LAST_BEAT_US.store(gtk::glib::monotonic_time(), Ordering::Release);
        OPEN.store(open, Ordering::Release);
    }

    pub fn start() {
        std::thread::Builder::new()
            .name("omni-watchdog".into())
            .spawn(|| loop {
                std::thread::sleep(Duration::from_millis(250));
                // Timestamp only if the popup is open, so it pairs with this open.
                if OPEN.load(Ordering::Acquire)
                    && gtk::glib::monotonic_time() - LAST_BEAT_US.load(Ordering::Acquire) > STALL_US
                {
                    eprintln!("omni: UI froze while the popup was open; exiting to release the keyboard");
                    // SAFETY: _exit ends the process immediately without running any handlers
                    // (the UI thread is stuck, so nothing else can be trusted to run).
                    unsafe { libc::_exit(70) };
                }
            })
            .expect("can't start the watchdog thread");
    }
}

/// Put the popup on the focused monitor and fit its content to that monitor.
fn place_on_focused_monitor(ui: &Ui) {
    let t_mon = Instant::now();
    let Some(mon) = focused_monitor() else {
        eprintln!("omni: couldn't find the focused monitor");
        open_log(ui, "focused_monitor FAILED");
        return;
    };
    open_log(
        ui,
        &format!(
            "focused_monitor ready ({:.1}ms) name={}",
            t_mon.elapsed().as_secs_f64() * 1000.0,
            mon.name
        ),
    );
    // Put the surface on the monitor that currently has keyboard focus.
    if let Some(display) = gdk::Display::default() {
        let monitors = display.monitors();
        for i in 0..monitors.n_items() {
            let Some(m) = monitors.item(i).and_downcast::<gdk::Monitor>() else { continue };
            if m.connector().as_deref() == Some(mon.name.as_str()) {
                ui.window.set_monitor(Some(&m));
            }
        }
    }

    fit_to_monitor(ui, mon.height);
}

/// Cap the result list (and Claude's answer) so the card ends BOTTOM_GAP above the bottom
/// of a screen `height` logical pixels tall.
fn fit_to_monitor(ui: &Ui, height: i32) {
    let room = (height - CARD_TOP - BOTTOM_GAP - CARD_CHROME).max(200);
    // List: at most 7 rows; shrinks with content below that.
    let list_cap = (MAX_VISIBLE_ROWS * ROW_HEIGHT).min(room);
    ui.list_scroll.set_max_content_height(list_cap);
    ui.answer.set_max_height(room);
    ui.search_view.set_height(room.saturating_sub(60));
    ui.status_scroll.set_max_content_height(room);
}

struct Monitor {
    name: String,
    x: i32,
    y: i32,
    /// Logical size (physical dimensions / scale, rotation-aware).
    width: i32,
    height: i32,
}

fn focused_monitor() -> Option<Monitor> {
    let out = util::output_with_timeout(
        std::process::Command::new("hyprctl").args(["-j", "monitors"]),
        Duration::from_millis(500),
    )?;
    let list: serde_json::Value = serde_json::from_slice(&out).ok()?;
    let m = list.as_array()?.iter().find(|m| m["focused"].as_bool() == Some(true))?;
    let scale = m["scale"].as_f64().unwrap_or(1.0);
    let (mut width, mut height) =
        (m["width"].as_f64()? / scale, m["height"].as_f64()? / scale);
    if m["transform"].as_i64().unwrap_or(0) % 2 == 1 {
        std::mem::swap(&mut width, &mut height);
    }
    Some(Monitor {
        name: m["name"].as_str()?.to_string(),
        x: m["x"].as_i64()? as i32,
        y: m["y"].as_i64()? as i32,
        width: width.round() as i32,
        height: height.round() as i32,
    })
}

/// `$XDG_RUNTIME_DIR/omni.sock`. Unset or empty: nothing. `glib::user_runtime_dir`
/// falls back to ~/.cache, and /tmp is shared, so neither is a stand-in.
fn socket_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(dir).join("omni.sock"))
}

#[derive(Debug, PartialEq, Eq)]
enum SocketRequest {
    Toggle,
    Ping,
    Search(String),
    /// Debug: put text in the bar, as typing would.
    Type(String),
    WatchdogTest,
    Ignore,
}

/// What a control-socket payload asks for. Only exact "toggle" / "ping" (with or
/// without one trailing newline) do anything. "search\t" and "watchdog-test" need
/// OMNI_DEBUG=1; every other payload, including empty, is ignored.
fn socket_request(msg: &[u8], debug: bool) -> SocketRequest {
    if debug {
        if let Some(query) = msg.strip_prefix(b"search\t") {
            return SocketRequest::Search(String::from_utf8_lossy(query).trim().to_string());
        }
        if let Some(text) = msg.strip_prefix(b"type\t") {
            return SocketRequest::Type(String::from_utf8_lossy(text).trim_end_matches('\n').to_string());
        }
        if msg.starts_with(b"watchdog-test") {
            return SocketRequest::WatchdogTest;
        }
    }
    match msg {
        b"toggle" | b"toggle\n" => SocketRequest::Toggle,
        b"ping" | b"ping\n" => SocketRequest::Ping,
        _ => SocketRequest::Ignore,
    }
}

/// Peer uid of a control-socket connection. Missing credentials or a different user
/// is not us: the caller closes and does not toggle.
fn peer_is_us(conn: &gio::SocketConnection) -> bool {
    // SAFETY: getuid only reads this process's real user id.
    let me = unsafe { libc::getuid() };
    match conn.socket().credentials().and_then(|c| c.unix_user()) {
        Ok(uid) if uid == me => true,
        Ok(uid) => {
            eprintln!("omni: ignoring socket connection from uid {uid}");
            false
        }
        Err(e) => {
            eprintln!("omni: ignoring socket connection, no peer uid: {e}");
            false
        }
    }
}

/// Socket the keybind pokes (via `omni-toggle`), so opening the popup doesn't
/// cost a GTK process start. Mode 0600, and the peer uid must be ours. "ping"
/// replies "pong" (no toggle); "toggle" replies "ok" and toggles. Anything else
/// is ignored. "search\t" and "watchdog-test" run only when OMNI_DEBUG=1.
fn listen_for_toggle(ui: &Rc<Ui>) {
    let Some(path) = socket_path() else {
        eprintln!("omni: XDG_RUNTIME_DIR is unset or empty; not listening for toggle");
        return;
    };
    let _ = std::fs::remove_file(&path);
    let service = gio::SocketService::new();
    let addr = gio::UnixSocketAddress::new(&path);
    if let Err(e) = service.add_address(
        &addr,
        gio::SocketType::Stream,
        gio::SocketProtocol::Default,
        None::<&glib::Object>,
    ) {
        eprintln!("omni: can't listen on {}: {e}", path.display());
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        eprintln!("omni: couldn't restrict {} to mode 0600: {e}", path.display());
        let _ = std::fs::remove_file(&path);
        return;
    }
    let ui = ui.clone();
    service.connect_incoming(move |_, conn, _| {
        probe::log("socket accept");
        // Read the message without blocking the UI; a client that sends nothing just
        // gets end-of-stream when it closes.
        let (ui, conn) = (ui.clone(), conn.clone());
        glib::spawn_future_local(async move {
            if !peer_is_us(&conn) {
                let _ = conn.close_future(glib::Priority::DEFAULT).await;
                return;
            }
            let msg = conn.input_stream().read_bytes_future(1024, glib::Priority::DEFAULT).await;
            let msg = msg.map(|b| b.to_vec()).unwrap_or_default();
            // Kind only: a debug search request can carry the user's query.
            let kind = msg.split(|b| *b == b'\t' || *b == b'\n').next().unwrap_or_default();
            probe::log(format!("socket msg {:?}", String::from_utf8_lossy(kind)));
            let debug = std::env::var("OMNI_DEBUG").ok().as_deref() == Some("1");
            match socket_request(&msg, debug) {
                SocketRequest::Search(query) => {
                    // Debug: what the popup would list for this query, without showing it.
                    let (f, d) = match parse_scope(&query) {
                        Some((dir, inner)) => files::search(&ui.files, inner, MAX_FILTERED, MAX_DIRS * 2, Some(&dir)),
                        None => files::search(&ui.files, &query, MAX_FILES, MAX_DIRS, None),
                    };
                    let mut reply = String::new();
                    // Same order as the popup: matching home folders, files, other folders.
                    let home_dirs = d.iter().filter(|h| h.home_folder);
                    for h in home_dirs.chain(&f).chain(d.iter().filter(|h| !h.home_folder)) {
                        reply.push_str(&format!("{} {}\n", if h.is_dir { "dir " } else { "file" }, h.path.display()));
                    }
                    let indexed = ui.files.lock().unwrap().0.as_ref().map_or(0, |i| i.len());
                    let (ev, added) = (LIVE_EVENTS.load(std::sync::atomic::Ordering::Relaxed), LIVE_ADDED.load(std::sync::atomic::Ordering::Relaxed));
                    let connected = ui.sparql.borrow().is_some();
                    reply.push_str(&format!("indexed {indexed}; localsearch connected {connected}; live events {ev}, added {added}\n"));
                    let _ = conn.output_stream().write_all_future(reply.into_bytes(), glib::Priority::DEFAULT).await;
                }
                SocketRequest::WatchdogTest => {
                    // Debug: pretend the popup is open (nothing is shown) and freeze the UI
                    // thread; the watchdog should end the process ~2 s in.
                    watchdog::set_open(true);
                    std::thread::sleep(Duration::from_secs(5));
                    watchdog::set_open(false);
                }
                SocketRequest::Type(text) => {
                    // Debug: drive the first-keystroke path without an input device.
                    ui.entry.set_text(&text);
                }
                SocketRequest::Ping => {
                    let _ = conn.output_stream().write_all_future(b"pong\n".to_vec(), glib::Priority::DEFAULT).await;
                }
                SocketRequest::Toggle => {
                    // Acknowledge first (tells omni-toggle we're alive), then toggle. Not
                    // awaited: the popup shouldn't wait on the client reading the reply.
                    let _ = conn.output_stream().write_all(b"ok\n", gio::Cancellable::NONE);
                    toggle(&ui);
                }
                SocketRequest::Ignore => {}
            }
            let _ = conn.close_future(glib::Priority::DEFAULT).await;
        });
        true
    });
    service.start();
    std::mem::forget(service);
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    #[test]
    fn selection_scroll_surface_pixels_are_converted_to_rows() {
        let pending = Cell::new(0.0);
        assert_eq!(selection_scroll_steps(&pending, 52.0, gdk::ScrollUnit::Surface), 1);
        assert_eq!(selection_scroll_steps(&pending, -104.0, gdk::ScrollUnit::Surface), -2);
    }

    #[test]
    fn selection_scroll_accumulates_fractional_motion_in_both_units() {
        let pending = Cell::new(0.0);
        assert_eq!(selection_scroll_steps(&pending, 0.5, gdk::ScrollUnit::Wheel), 0);
        assert_eq!(selection_scroll_steps(&pending, 0.5, gdk::ScrollUnit::Wheel), 1);
        assert_eq!(selection_scroll_steps(&pending, 26.0, gdk::ScrollUnit::Surface), 0);
        assert_eq!(selection_scroll_steps(&pending, 26.0, gdk::ScrollUnit::Surface), 1);
        assert_eq!(selection_scroll_steps(&pending, -26.0, gdk::ScrollUnit::Surface), 0);
        assert_eq!(selection_scroll_steps(&pending, -26.0, gdk::ScrollUnit::Surface), -1);
    }

    #[test]
    fn selection_target_clamps_large_deltas_and_skips_unchanged_rows() {
        assert_eq!(selection_target(2, 8, i32::MAX), Some(7));
        assert_eq!(selection_target(2, 8, i32::MIN), Some(0));
        assert_eq!(selection_target(7, 8, 1000), None);
        assert_eq!(selection_target(0, 8, -1000), None);
        assert_eq!(selection_target(2, 8, 0), None);
        assert_eq!(selection_target(0, 0, 1), None);
        assert_eq!(selection_target(2, 8, 3), Some(5));
        assert_eq!(selection_target(2, 8, -1), Some(1));
    }

    #[test]
    fn settings_navigation_starts_on_launcher() {
        assert_eq!(UiMode::default(), UiMode::Launcher);
    }

    #[test]
    fn settings_navigation_gear_opens_and_back_returns() {
        let settings = UiMode::Launcher.after(UiModeAction::OpenSettings);
        assert_eq!(settings, UiMode::Settings);
        assert_eq!(settings.after(UiModeAction::Back), UiMode::Launcher);
    }

    #[test]
    fn settings_navigation_escape_is_consumed_only_in_settings() {
        assert!(!UiMode::Launcher.consumes_escape());
        assert!(UiMode::Settings.consumes_escape());
        assert_eq!(UiMode::Settings.after(UiModeAction::Back), UiMode::Launcher);
    }

    #[test]
    fn settings_navigation_close_resets_to_launcher() {
        assert_eq!(UiMode::Settings.after(UiModeAction::Close), UiMode::Launcher);
    }

    #[test]
    fn settings_gear_css_has_rest_hover_and_reduced_motion_states() {
        assert!(DEFAULT_CSS.contains(".omni-settings-slip"));
        assert!(DEFAULT_CSS.contains(".omni-settings-slip:hover .omni-settings-button"));
        assert!(DEFAULT_CSS.contains("padding: 0 40px 0 18px;"));
        assert!(DEFAULT_CSS.contains("color: alpha(white, 0.82);"));
        assert!(DEFAULT_CSS.contains("background-color: transparent;\n  background-image: none;\n  border: none;"));
        assert!(DEFAULT_CSS.contains("prefers-reduced-motion: reduce"));
        assert!(DEFAULT_CSS.contains(".omni-settings-button {\n    transition: none;"));
    }

    #[test]
    fn folder_scope_syntax() {
        let home = glib::home_dir();
        assert_eq!(parse_scope("~/Music: NF"), Some((home.join("Music"), "NF")));
        assert_eq!(parse_scope("/etc/hypr: conf"), Some((PathBuf::from("/etc/hypr"), "conf")));
        assert_eq!(parse_scope("~: notes"), Some((home.clone(), "notes")));
        assert_eq!(parse_scope("~/Music:"), Some((home.join("Music"), "")));
        assert_eq!(parse_scope("~/Music:NF"), None); // needs the space
        assert_eq!(parse_scope("note: buy milk"), None); // not a path
    }

    #[test]
    fn from_all_a_kind_picks_only_it_then_kinds_toggle() {
        assert_eq!(filter_click(SHOW_ALL, SHOW_APPS), SHOW_APPS);
        assert_eq!(filter_click(SHOW_APPS, SHOW_FOLDERS), SHOW_APPS | SHOW_FOLDERS);
        assert_eq!(filter_click(SHOW_APPS | SHOW_FOLDERS, SHOW_APPS), SHOW_FOLDERS);
        assert_eq!(filter_click(SHOW_FOLDERS, SHOW_FOLDERS), SHOW_ALL);
        assert_eq!(filter_click(SHOW_FILES, SHOW_ALL), SHOW_ALL);
        assert_eq!(filter_name(SHOW_ALL), "All");
        assert_eq!(filter_name(SHOW_FILES | SHOW_FOLDERS), "Files + Folders");
    }

    #[test]
    fn link_host_shows_the_real_host() {
        // Userinfo is not the host, including when a slash comes before the '@'.
        assert_eq!(link_host("https://user:pass@evil.example/phish"), "evil.example");
        assert_eq!(link_host("https://trusted.example/secret@evil.example/login"), "evil.example");
        assert_eq!(link_host("https://example.com/path?q=1#top"), "example.com");
        assert_eq!(link_host("https://ex\u{202E}ample.com/path"), "ex\u{27e8}U+202E\u{27e9}ample.com");
    }

    #[test]
    fn visible_escapes_bidi_and_controls() {
        assert_eq!(visible("keep\n\ttabs"), "keep\n\ttabs");
        assert_eq!(visible("\u{200B}\u{200F}"), "\u{27e8}U+200B\u{27e9}\u{27e8}U+200F\u{27e9}");
        assert_eq!(visible("\u{202A}\u{202E}"), "\u{27e8}U+202A\u{27e9}\u{27e8}U+202E\u{27e9}");
        assert_eq!(visible("\u{2060}\u{2069}"), "\u{27e8}U+2060\u{27e9}\u{27e8}U+2069\u{27e9}");
        assert_eq!(visible("\u{FEFF}\u{00AD}"), "\u{27e8}U+FEFF\u{27e9}\u{27e8}U+00AD\u{27e9}");
        assert_eq!(visible("\u{0007}"), "\u{27e8}U+0007\u{27e9}");
    }

    #[test]
    fn inherited_secrets_are_scrubbed() {
        for name in ["CLAUDE_CODE_MESSAGING_TOKEN", "CLAUDE_CODE_OAUTH_SCOPES", "CLAUDECODE", "ANTHROPIC_API_KEY", "GH_TOKEN", "OPENAI_API_KEY", "AWS_SECRET_ACCESS_KEY", "DB_PASSWORD"] {
            assert!(secret_env_name(name), "{name}");
        }
        for name in ["PATH", "HOME", "WAYLAND_DISPLAY", "SSH_AUTH_SOCK", "XDG_RUNTIME_DIR", "HYPRLAND_INSTANCE_SIGNATURE", "API_TIMEOUT_MS"] {
            assert!(!secret_env_name(name), "{name}");
        }
    }

    #[test]
    fn socket_request_is_exact() {
        assert_eq!(socket_request(b"toggle", false), SocketRequest::Toggle);
        assert_eq!(socket_request(b"toggle\n", false), SocketRequest::Toggle);
        assert_eq!(socket_request(b"ping", false), SocketRequest::Ping);
        assert_eq!(socket_request(b"ping\n", false), SocketRequest::Ping);
        assert_eq!(socket_request(b"", false), SocketRequest::Ignore);
        assert_eq!(socket_request(b"toggle\n\n", false), SocketRequest::Ignore);
        assert_eq!(socket_request(b"watchdog-test", false), SocketRequest::Ignore);
        assert_eq!(socket_request(b"search\tNF", false), SocketRequest::Ignore);
        assert_eq!(socket_request(b"watchdog-test", true), SocketRequest::WatchdogTest);
        assert_eq!(socket_request(b"search\tNF\n", true), SocketRequest::Search("NF".into()));
        assert_eq!(socket_request(b"type\tNF", true), SocketRequest::Type("NF".into()));
        assert_eq!(socket_request(b"type\tNF\n", true), SocketRequest::Type("NF".into()));
        assert_eq!(socket_request(b"type\tNF", false), SocketRequest::Ignore);
    }
}
