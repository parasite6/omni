mod answer;
mod apps;
mod calc;
mod dock;
mod files;
mod glass;
mod recent;
mod ring;
mod services;
mod sparql;
mod util;
mod web;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

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
const MAX_CONTENT: usize = 4;
const MAX_OPEN_WITH: usize = 10;
/// File *contents* are searched once typing pauses this long (LocalSearch is slow; names,
/// apps, web and Claude rows update on every keystroke).
const CONTENT_DELAY: Duration = Duration::from_millis(700);
/// Re-walk the disk this often (new files arrive in between via LocalSearch's events).
const REWALK: Duration = Duration::from_secs(30 * 60);

/// Card geometry on screen (logical px). The backdrop capture depends on it.
const CARD_WIDTH: i32 = 680;
const CARD_TOP: i32 = 180;
/// Tallest the card can get (full result list + path strip); the backdrop capture covers
/// this much, clamped to the bottom of the monitor.
const CARD_MAX_HEIGHT: i32 = 2000;
/// The card stops this far above the bottom of the screen; longer lists scroll inside it.
const BOTTOM_GAP: i32 = 96;
/// Card height that isn't the result list: padding, search bar, path strip room.
const CARD_CHROME: i32 = 150;

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
}

struct Hit {
    title: String,
    subtitle: String,
    icon: Option<gio::Icon>,
    action: Action,
    /// A file (not a folder): right-click / Menu key opens Open and Open with.
    /// (App rows always have a menu: Open and the app's actions.)
    file_menu: bool,
}

struct Ui {
    window: gtk::ApplicationWindow,
    card: glass::GlassCard,
    entry: gtk::SearchEntry,
    list: gtk::ListBox,
    list_scroll: gtk::ScrolledWindow,
    reveal: gtk::Revealer,
    apps: RefCell<apps::Apps>,
    web: RefCell<web::Engine>,
    hits: RefCell<Vec<Hit>>,
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
    /// Contents results and the query they belong to.
    content: RefCell<(String, Vec<PathBuf>)>,
    content_gen: Cell<u64>,
    path_reveal: gtk::Revealer,
    path_label: gtk::Label,
    /// Bumped on every hover change, so a delayed "hide" doesn't undo a newer hover.
    hover_gen: Cell<u64>,
    /// Query the list was last built for (to keep the selection when results fill in).
    shown_query: RefCell<String>,
    /// Heartbeat timer for the watchdog; runs only while the popup is open.
    heartbeat: RefCell<Option<glib::SourceId>>,
    /// The user moved the selection with the arrow keys for this query.
    user_moved: Cell<bool>,
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

    let slot = ui.clone();
    app.connect_startup(move |app| {
        let built = build_ui(app);
        listen_for_toggle(&built);
        *slot.borrow_mut() = Some(built);
        std::mem::forget(app.hold()); // keep running while hidden
    });

    app.connect_command_line(move |_, cmd| {
        let daemon = cmd.arguments().iter().any(|a| a == "--daemon");
        if !daemon {
            if let Some(ui) = ui.borrow().as_ref() {
                toggle(ui);
            }
        }
        0.into()
    });

    app.run()
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
        .build();

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    bar.add_css_class("omni-bar");
    // The entry wears a living cyan/black rim (ring.rs) drawn over its edge.
    let search = gtk::Overlay::builder().child(&entry).hexpand(true).build();
    search.add_overlay(&ring::SearchRing::new());
    bar.append(&search);
    bar.append(&filter_button);
    bar.append(&contents);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Browse);
    list.add_css_class("omni-results");

    // Results slide open below the search bar as soon as there is something to show.
    // Long lists scroll inside the card instead of running off the screen (height capped
    // per monitor when the popup opens, see fit_to_monitor).
    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        // Scrolls (wheel, keys, selection) but never shows a scrollbar.
        .vscrollbar_policy(gtk::PolicyType::External)
        .propagate_natural_height(true)
        .max_content_height(600)
        .child(&list)
        .build();
    let reveal = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .transition_duration(220)
        .child(&list_scroll)
        .build();

    // Card = blurred backdrop (drawn by GlassCard) + tinted glass surface + content.
    // Glass surface = padded content + a path strip attached edge to edge at the bottom.
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

    // Full path of the hovered file/folder: wraps to the card's width, grows downward.
    let path_label = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .max_width_chars(1)
        .hexpand(true)
        .build();
    path_label.add_css_class("omni-path");
    let path_reveal = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .transition_duration(160)
        .child(&path_label)
        .build();
    surface.append(&path_reveal);

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
    let layers = gtk::Overlay::builder().child(&surface).build();
    layers.add_overlay(&confirm);
    card.append(&layers);
    window.set_child(Some(&card));

    let ui = Rc::new(Ui {
        window: window.clone(),
        card: card.clone(),
        entry: entry.clone(),
        list: list.clone(),
        list_scroll: list_scroll.clone(),
        reveal: reveal.clone(),
        apps: RefCell::new(apps::Apps::load()),
        web: RefCell::new(web::Engine::detect()),
        hits: RefCell::default(),
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
        path_reveal: path_reveal.clone(),
        path_label: path_label.clone(),
        hover_gen: Cell::new(0),
        shown_query: RefCell::default(),
        heartbeat: RefCell::default(),
        user_moved: Cell::new(false),
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

    let u = ui.clone();
    confirm_yes.connect_clicked(move |_| finish_confirm(&u, Answered::Yes));
    let u = ui.clone();
    confirm_always.connect_clicked(move |_| finish_confirm(&u, Answered::Always));
    let u = ui.clone();
    confirm_no.connect_clicked(move |_| close_confirm(&u));

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

    // Contents toggle: remembered between opens; flipping it re-runs the search.
    contents.set_active(load_contents_setting());
    let u = ui.clone();
    contents.connect_toggled(move |b| {
        save_contents_setting(b.is_active());
        if !u.answer_reveal.reveals_child() {
            refresh(&u); // not while Claude's answer is showing: that would cancel it
        }
    });

    // Filter dropdown: remembered between opens.
    filter_label.set_label(&filter_name(ui.filter.get()));
    let u = ui.clone();
    filter_button.connect_clicked(move |_| show_filter_menu(&u, false));

    // Re-read apps when something is installed or removed. GIO stops sending "changed"
    // once the last reference to the monitor is gone, so it is kept for the process.
    let weak = Rc::downgrade(&ui);
    let app_monitor = gio::AppInfoMonitor::get();
    app_monitor.connect_changed(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.apps.borrow_mut().reload();
            if ui.window.is_visible() && !ui.chatting.get() && !ui.entry.text().trim().is_empty() {
                refresh(&ui);
            }
        }
    });
    std::mem::forget(app_monitor);

    let u = ui.clone();
    entry.connect_search_changed(move |_| refresh(&u));

    let u = ui.clone();
    entry.connect_activate(move |_| {
        if u.chatting.get() {
            return send_reply(&u);
        }
        if u.answer_reveal.reveals_child() || u.status_reveal.reveals_child() {
            return; // answer or status showing: Enter has nothing to pick
        }
        let row = u.list.selected_row().or_else(|| u.list.row_at_index(0));
        if let Some(row) = row {
            activate(&u, row.index() as usize);
        }
    });

    // Act on the next idle tick: activating rebuilds this list, which shouldn't happen
    // inside the list's own signal.
    let u = ui.clone();
    list.connect_row_activated(move |_, row| {
        let (u, index) = (u.clone(), row.index() as usize);
        glib::idle_add_local_once(move || activate(&u, index));
    });

    // Esc hides, ↑/↓ move through results while typing stays in the entry, Ctrl+T flips Contents.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    keys.connect_key_pressed(move |_, key, _, mods| {
        if u.menu.borrow().is_some() {
            return glib::Propagation::Proceed;
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
            gdk::Key::t if mods.contains(gdk::ModifierType::CONTROL_MASK) => {
                contents.set_active(!contents.is_active())
            }
            gdk::Key::f if mods.contains(gdk::ModifierType::CONTROL_MASK) => show_filter_menu(&u, true),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(keys);

    // Scroll wheel / touchpad moves the selection like ↑/↓. Touchpad deltas are summed so a
    // swipe moves one row per unit, not one per event. With a Claude answer showing, the
    // scroll goes to the answer text instead.
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    // Before the list's own scroller sees it: the wheel moves the selection, and the list
    // follows the selection.
    scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    let u = ui.clone();
    let pending = Cell::new(0.0f64);
    scroll.connect_scroll(move |_, _, dy| {
        if u.answer_reveal.reveals_child() || u.status_reveal.reveals_child() || u.menu.borrow().is_some() {
            return glib::Propagation::Proceed;
        }
        let total = pending.get() + dy;
        let steps = total.trunc() as i32;
        pending.set(total - steps as f64);
        for _ in 0..steps.abs() {
            move_selection(&u, steps.signum());
        }
        glib::Propagation::Stop
    });
    window.add_controller(scroll);

    // Click anywhere outside the card closes the popup.
    let click = gtk::GestureClick::new();
    click.set_button(0);
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
        // files::search matches the directory case-insensitively.
        hits = scoped_hits(ui, &dir, inner);
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
        let mut shown: Vec<PathBuf> = Vec::new();
        for h in home_dirs.into_iter().chain(file_hits).chain(other_dirs) {
            hits.push(file_hit(&h.path, h.is_dir, None));
            shown.push(h.path);
        }
        // File contents (LocalSearch, slow): once typing pauses, then slotted in.
        if ui.contents.is_active() && show & SHOW_FILES != 0 {
            let content = ui.content.borrow();
            if content.0 == query {
                for p in content.1.iter().filter(|p| !shown.contains(p)) {
                    hits.push(file_hit(p, false, Some(query)));
                }
            } else {
                drop(content);
                schedule_content_search(ui, query);
            }
        }
        hits.push(web_hit(ui, query));
        hits.extend(claude_hit(ui, query));
    }

    // Same query, results filling in: if the user picked a row with the arrows, keep that
    // row selected (by what it is, since rows above it may have appeared); otherwise the
    // top row, so Enter opens the best match.
    let same = *ui.shown_query.borrow() == query;
    if !same {
        ui.user_moved.set(false);
    }
    let keep: Option<(String, String)> = (same && ui.user_moved.get())
        .then(|| {
            let i = ui.list.selected_row()?.index() as usize;
            ui.hits.borrow().get(i).map(|h| (h.title.clone(), h.subtitle.clone()))
        })
        .flatten();
    *ui.shown_query.borrow_mut() = query.to_string();

    // Rows are being replaced: the hovered one is gone.
    ui.hover_gen.set(ui.hover_gen.get() + 1);
    ui.path_reveal.set_reveal_child(false);
    while let Some(row) = ui.list.row_at_index(0) {
        ui.list.remove(&row);
    }
    for hit in &hits {
        ui.list.append(&make_row(ui, hit));
    }
    ui.list_scroll.vadjustment().set_value(0.0);
    ui.reveal.set_reveal_child(!hits.is_empty());
    let keep_index = keep.and_then(|(t, st)| hits.iter().position(|h| h.title == t && h.subtitle == st));
    let select = keep_index.and_then(|i| ui.list.row_at_index(i as i32)).or_else(|| ui.list.row_at_index(0));
    if let Some(row) = select {
        ui.list.select_row(Some(&row));
        if keep_index.is_some() {
            // Results filled in around a row the user picked: keep it in view.
            let (me, row) = (ui.me.borrow().upgrade(), row.clone());
            glib::idle_add_local_once(move || {
                if let Some(me) = me {
                    scroll_to_row(&me, &row);
                }
            });
        }
    }
    *ui.hits.borrow_mut() = hits;
}

/// A file or folder row. `content_of`: it matched by contents, for this query.
fn file_hit(path: &std::path::Path, is_dir: bool, content_of: Option<&str>) -> Hit {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string());
    let kind = match (is_dir, content_of) {
        (true, _) => "Folder".to_string(),
        (false, Some(q)) => format!("Contains \u{201c}{q}\u{201d}"),
        (false, None) => "File".to_string(),
    };
    Hit {
        title: name,
        subtitle: format!("{kind} \u{b7} {}", recent::folder_label(path)),
        icon: Some(recent::icon(path, is_dir)),
        action: Action::Open(gio::File::for_path(path)),
        file_menu: !is_dir,
    }
}

/// The query is a complete calculation (see calc.rs): its result, copied on Enter.
fn calc_hit(query: &str) -> Option<Hit> {
    let result = calc::format(calc::evaluate(query)?);
    Some(Hit {
        title: result.clone(),
        subtitle: format!("{} =", calc::pretty(query)),
        icon: None,
        action: Action::Copy(result),
        file_menu: false,
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
        return if dir.is_dir() { vec![file_hit(dir, true, None)] } else { Vec::new() };
    }
    let show = ui.filter.get();
    let max_files = if show & SHOW_FILES == 0 { 0 } else { MAX_FILTERED };
    let max_dirs = if show & SHOW_FOLDERS == 0 { 0 } else { MAX_DIRS * 2 };
    let (mut files, mut dirs) = files::search(&ui.files, inner, max_files.max(1), max_dirs.max(1), Some(dir));
    files.truncate(max_files);
    dirs.truncate(max_dirs);
    files.into_iter().chain(dirs).map(|h| file_hit(&h.path, h.is_dir, None)).collect()
}

fn schedule_content_search(ui: &Ui, query: &str) {
    let generation = ui.content_gen.get() + 1;
    ui.content_gen.set(generation);
    let Some(me) = ui.me.borrow().upgrade() else { return };
    let query = query.to_string();
    glib::timeout_add_local_once(CONTENT_DELAY, move || {
        if me.content_gen.get() == generation && me.entry.text().trim() == query && me.window.is_visible() {
            start_content_search(&me, &query);
        }
    });
}

/// Ask LocalSearch for files whose contents match, off the UI thread. Results for an
/// outdated query are dropped; current ones are slotted in.
fn start_content_search(ui: &Ui, query: &str) {
    let generation = ui.content_gen.get() + 1;
    ui.content_gen.set(generation);
    let (Some(me), Some(conn)) = (ui.me.borrow().upgrade(), ui.sparql.borrow().clone()) else { return };
    let query = query.to_string();
    glib::spawn_future_local(async move {
        let q = query.clone();
        let found = gio::spawn_blocking(move || files::content_search(&conn, &q, MAX_CONTENT)).await.unwrap_or_default();
        if me.content_gen.get() != generation || me.entry.text().trim() != query {
            return;
        }
        *me.content.borrow_mut() = (query, found);
        if !me.answer_reveal.reveals_child() && me.window.is_visible() {
            refresh(&me);
        }
    });
}

/// Connect to LocalSearch off the UI thread; then listen for new files (added to the
/// name index right away) and enable contents search.
fn connect_localsearch(ui: &Rc<Ui>) {
    let weak = Rc::downgrade(ui);
    glib::spawn_future_local(async move {
        let conn = match gio::spawn_blocking(sparql::Connection::new).await {
            Ok(Ok(c)) => Arc::new(c),
            Ok(Err(e)) => return eprintln!("omni: can't connect to LocalSearch (contents search and live file updates off): {e}"),
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
    });
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
    let menu = gtk::Popover::builder().has_arrow(false).position(gtk::PositionType::Bottom).build();
    menu.add_css_class("omni-menu");
    let items = gtk::Box::new(gtk::Orientation::Vertical, 2);

    // Rows are just the kind names; picked ones are lit (no checkmark column, so the
    // menu fits the button's width).
    let rows: Vec<(u8, &str)> = std::iter::once((SHOW_ALL, "All")).chain(FILTER_KINDS).collect();
    let buttons: Rc<Vec<(u8, gtk::Button)>> = Rc::new(
        rows.iter()
            .map(|(bit, name)| {
                let label = gtk::Label::builder().label(*name).ellipsize(gtk::pango::EllipsizeMode::End).build();
                (*bit, gtk::Button::builder().child(&label).css_classes(["flat", "omni-menu-item"]).build())
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
    // Exactly as wide as the filter button, centred under it. Centring (not start-aligning)
    // because the popover's surface has invisible shadow margins. The popover can't be
    // resized once open (Wayland dismisses it), so the width is fixed here.
    let button = &ui.filter_button;
    items.set_size_request((button.width() - 10).max(1), -1); // minus the menu's padding + border
    menu.set_halign(gtk::Align::Center);
    menu.set_offset(0, 6);
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
    let menu = gtk::Popover::builder().has_arrow(false).position(gtk::PositionType::Bottom).build();
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
        icon: info.icon(),
        action: Action::App(info.clone()),
        file_menu: false,
    }
}

fn action_hit(app: &apps::App, action: &apps::AppAction) -> Hit {
    Hit {
        title: action.name.clone(),
        subtitle: app.name.clone(),
        icon: app.info.icon(),
        action: Action::AppAction(app.info.clone(), action.clone()),
        file_menu: false,
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
        let kind = if f.is_dir { "Folder" } else { "File" };
        items.push((
            f.time,
            Hit {
                title: name,
                subtitle: format!("{kind} \u{b7} {}", recent::folder_label(&f.path)),
                icon: Some(recent::icon(&f.path, f.is_dir)),
                action: Action::Open(f.file),
                file_menu: !f.is_dir,
            },
        ));
    }
    items.sort_by(|a, b| b.0.cmp(&a.0));
    items.into_iter().take(MAX_RECENT).map(|(_, h)| h).collect()
}

fn web_hit(ui: &Ui, query: &str) -> Hit {
    let engine = ui.web.borrow();
    let browser = engine.browser.as_ref().map(|b| b.display_name().to_string());
    Hit {
        title: format!("Search the web for \u{201c}{query}\u{201d}"),
        subtitle: match browser {
            Some(b) => format!("{} \u{b7} {b}", engine.name),
            None => engine.name.clone(),
        },
        icon: engine.browser.as_ref().and_then(|b| b.icon()).or_else(|| Some(gio::ThemedIcon::new("web-browser").upcast())),
        action: Action::Web(query.to_string()),
        file_menu: false,
    }
}

/// Row that asks Claude (Claude Code CLI) and shows the answer in the popup.
/// Uses the Claude app's icon when it's installed.
fn service_hit(s: &services::Service) -> Hit {
    let title = if s.description.is_empty() { s.name().to_string() } else { s.description.clone() };
    Hit {
        title,
        subtitle: format!("{} \u{b7} {} \u{b7} right-click for actions", s.unit, s.summary()),
        icon: Some(gio::ThemedIcon::new(if s.active == "failed" { "dialog-warning-symbolic" } else { "system-run-symbolic" }).upcast()),
        action: Action::Service(s.clone()),
        file_menu: false,
    }
}

fn claude_hit(ui: &Ui, query: &str) -> Option<Hit> {
    let icon = gio::AppInfo::default_for_uri_scheme("claude")
        .and_then(|a| a.icon())
        .or_else(|| Some(gio::ThemedIcon::new("dialog-question").upcast()));
    Some(Hit {
        title: format!("Ask Claude \u{201c}{query}\u{201d}"),
        subtitle: format!("Quick answer \u{b7} {} \u{b7} Shift+Enter", ui.answer.choice_label()),
        icon,
        action: Action::Claude(query.to_string()),
        file_menu: false,
    })
}

fn make_row(ui: &Ui, hit: &Hit) -> gtk::ListBoxRow {
    if matches!(hit.action, Action::Copy(_)) {
        return calc_row(hit);
    }
    let icon = match &hit.icon {
        Some(i) => gtk::Image::from_gicon(i),
        None => gtk::Image::from_icon_name("application-x-executable"),
    };
    icon.set_pixel_size(36);

    // Long names/paths never widen the popup: labels ask for (almost) no natural width,
    // fill whatever the fixed-width card gives them, and end in "…".
    let label = |text: &str, class: &str| {
        let l = gtk::Label::builder()
            .label(text)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(1)
            .hexpand(true)
            .build();
        l.add_css_class(class);
        l
    };
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_valign(gtk::Align::Center);
    text.set_hexpand(true);
    text.append(&label(&hit.title, "omni-title"));
    if !hit.subtitle.is_empty() {
        text.append(&label(&hit.subtitle, "omni-subtitle"));
    }

    let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row_box.append(&icon);
    row_box.append(&text);

    let row = gtk::ListBoxRow::builder().child(&row_box).build();
    // Files and folders: hovering shows the full path in the strip at the bottom of the card.
    if let Action::Open(file) = &hit.action {
        if let Some(path) = file.path() {
            let hover = gtk::EventControllerMotion::new();
            let (me, path) = (ui.me.borrow().clone(), path.to_string_lossy().to_string());
            hover.connect_enter(move |_, _, _| {
                if let Some(ui) = me.upgrade() {
                    ui.hover_gen.set(ui.hover_gen.get() + 1);
                    ui.path_label.set_text(&path);
                    ui.path_reveal.set_reveal_child(true);
                }
            });
            let me = ui.me.borrow().clone();
            hover.connect_leave(move |_| {
                let Some(ui) = me.upgrade() else { return };
                // Hide a moment later, unless the pointer has moved onto another file.
                let generation = ui.hover_gen.get() + 1;
                ui.hover_gen.set(generation);
                let me = me.clone();
                glib::timeout_add_local_once(Duration::from_millis(120), move || {
                    if let Some(ui) = me.upgrade() {
                        if ui.hover_gen.get() == generation {
                            ui.path_reveal.set_reveal_child(false);
                        }
                    }
                });
            });
            row.add_controller(hover);
        }
    }
    if hit.file_menu || matches!(hit.action, Action::App(_) | Action::Web(_) | Action::Service(_)) {
        let right_click = gtk::GestureClick::new();
        right_click.set_button(gdk::BUTTON_SECONDARY);
        let me = ui.me.borrow().clone();
        right_click.connect_pressed(move |gesture, _, x, y| {
            let (Some(ui), Some(row)) = (me.upgrade(), gesture.widget()) else { return };
            let Some(row) = row.downcast_ref::<gtk::ListBoxRow>() else { return };
            let at = row.compute_point(&ui.list, &gtk::graphene::Point::new(x as f32, y as f32));
            let at = at.map(|p| gdk::Rectangle::new(p.x() as i32, p.y() as i32, 1, 1));
            open_row_menu(&ui, row.index() as usize, at);
        });
        row.add_controller(right_click);
    }
    row
}

/// The calculator's result, shown as the answer: expression small on top, result large
/// underneath (it can wrap, so long numbers are never cut off).
fn calc_row(hit: &Hit) -> gtk::ListBoxRow {
    let expression = gtk::Label::builder().label(&hit.subtitle).xalign(0.0).hexpand(true).build();
    expression.add_css_class("omni-calc-expression");
    let hint = gtk::Label::new(Some("Enter to copy"));
    hint.add_css_class("omni-calc-hint");
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    top.append(&expression);
    top.append(&hint);

    let result = gtk::Label::builder()
        .label(&hit.title)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::Char)
        .max_width_chars(1)
        .hexpand(true)
        .build();
    result.add_css_class("omni-calc-result");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
    content.append(&top);
    content.append(&result);
    let row = gtk::ListBoxRow::builder().child(&content).build();
    row.add_css_class("omni-calc");
    row
}

fn open_selected_menu(ui: &Ui) {
    if let Some(row) = ui.list.selected_row() {
        open_row_menu(ui, row.index() as usize, None);
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

/// The right-click menu of a file or app row, pointing at `at` (list coordinates) or,
/// from the keyboard, the row itself.
/// Files: Open, then "Open with" apps registered for the file's type, guessed from its
/// name so opening the menu never touches the disk. Apps: Open, then the app's actions.
/// Web search: Search, then the browser's private (and Brave's Tor) window.
fn open_row_menu(ui: &Ui, index: usize, at: Option<gdk::Rectangle>) {
    let entries = match ui.hits.borrow().get(index) {
        Some(Hit { action: Action::Open(file), file_menu: true, .. }) => file_menu_entries(file),
        Some(Hit { action: Action::App(info), .. }) => app_menu_entries(ui, info),
        Some(Hit { action: Action::Web(query), .. }) => web_menu_entries(ui, query),
        Some(Hit { action: Action::Service(service), .. }) => service_menu_entries(service),
        _ => return,
    };
    let Some(row) = ui.list.row_at_index(index as i32) else { return };
    ui.list.select_row(Some(&row));
    // From the keyboard there's no cursor: drop from just inside the row's bottom-left.
    let Some(target) = at.or_else(|| {
        let b = row.compute_bounds(&ui.list)?;
        Some(gdk::Rectangle::new(b.x() as i32 + 48, (b.y() + b.height()) as i32 - 8, 1, 1))
    }) else {
        return;
    };
    show_menu(ui, entries, target);
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
        detail: Some(engine.name.clone()),
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
    let menu = gtk::Popover::builder().has_arrow(false).position(gtk::PositionType::Bottom).build();
    menu.add_css_class("omni-menu");
    let items = gtk::Box::new(gtk::Orientation::Vertical, 2);
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
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        if let Some(icon) = icon {
            let image = gtk::Image::from_gicon(&icon);
            image.set_pixel_size(20);
            content.append(&image);
        }
        content.append(&gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build());
        if let Some(detail) = detail {
            let detail = gtk::Label::new(Some(&detail));
            detail.add_css_class("omni-menu-detail");
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

    // The drop-open animation is CSS only (style.css, omni-menu-in): resizing an open
    // popover makes Wayland dismiss it, so the menu must appear at its full size.
    let first = items.first_child();
    items.connect_map(move |_| {
        if let Some(first) = &first {
            first.grab_focus();
        }
    });
    menu.set_child(Some(&items));
    menu.set_parent(&ui.list);
    // Top-left corner just right of and below the pointer.
    menu.set_halign(gtk::Align::Start);
    menu.set_offset(4, 6);
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

fn move_selection(ui: &Ui, delta: i32) {
    let count = ui.hits.borrow().len() as i32;
    if count == 0 {
        return;
    }
    let cur = ui.list.selected_row().map(|r| r.index()).unwrap_or(0);
    let next = (cur + delta).rem_euclid(count);
    if let Some(row) = ui.list.row_at_index(next) {
        ui.list.select_row(Some(&row));
        ui.user_moved.set(true);
        scroll_to_row(ui, &row);
    }
}

/// Scroll the result list just enough that `row` is fully in view.
fn scroll_to_row(ui: &Ui, row: &gtk::ListBoxRow) {
    let Some(bounds) = row.compute_bounds(&ui.list) else { return };
    let adj = ui.list_scroll.vadjustment();
    let (top, bottom) = (bounds.y() as f64, (bounds.y() + bounds.height()) as f64);
    if top < adj.value() {
        adj.set_value(top);
    } else if bottom > adj.value() + adj.page_size() {
        adj.set_value(bottom - adj.page_size());
    }
}

fn activate(ui: &Ui, index: usize) {
    let Some(action) = ui.hits.borrow().get(index).map(|h| h.action.clone()) else { return };
    run(ui, action);
}

fn run(ui: &Ui, action: Action) {
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
    match action {
        Action::Web(query) => ui.web.borrow().search(&query),
        Action::WebIn(query, window) => ui.web.borrow().search_in(&query, window),
        Action::App(info) => match info.launch(&[], Some(&ctx)) {
            Ok(()) => ui.apps.borrow_mut().record_launch(&info),
            Err(e) => eprintln!("omni: failed to launch {}: {e}", info.display_name()),
        },
        Action::AppAction(info, action) => match apps::launch_action(&info, &action, ctx.upcast_ref()) {
            Ok(()) => ui.apps.borrow_mut().record_launch(&info),
            Err(e) => eprintln!("omni: failed to launch {} ({}): {e}", info.display_name(), action.name),
        },
        Action::Open(file) => match gio::AppInfo::launch_default_for_uri(&file.uri(), Some(&ctx)) {
            Ok(()) => recent::record(&file),
            Err(e) => eprintln!("omni: couldn't open {}: {e}", file.uri()),
        },
        Action::OpenWith(info, file) => match info.launch(&[file.clone()], Some(&ctx)) {
            Ok(()) => recent::record(&file),
            Err(e) => eprintln!("omni: couldn't open {} with {}: {e}", file.uri(), info.display_name()),
        },
        Action::Copy(_) => {}
        Action::DockPin(id, pinned) => dock::set_pinned(&id, pinned),
        Action::Claude(_) | Action::Service(_) | Action::ServiceVerb(..) | Action::ServiceStatus(_) => unreachable!(),
    }
}

fn toggle(ui: &Rc<Ui>) {
    if ui.window.is_visible() {
        hide(ui);
    } else {
        // Fresh recents each time it opens.
        refresh(ui);
        load_services(ui);
        capture_backdrop(ui);
        // Re-read the default browser/engine in case it changed since last time.
        *ui.web.borrow_mut() = web::Engine::detect();
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
        ui.window.present();
        ui.entry.grab_focus();
        // Re-read the installed apps too. GIO's "changed" signal normally keeps the
        // list current, but it has been missed at least once (an app stayed hidden
        // until omni restarted). This is ~30 ms, so it runs after the popup is drawn,
        // not before; results already showing for typed text are refreshed.
        let weak = Rc::downgrade(ui);
        glib::idle_add_local_full(glib::Priority::LOW, move || {
            if let Some(ui) = weak.upgrade() {
                ui.apps.borrow_mut().reload();
                if ui.window.is_visible() && !ui.chatting.get() && !ui.entry.text().trim().is_empty() {
                    refresh(&ui);
                }
            }
            glib::ControlFlow::Break
        });
    }
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
    *ui.content.borrow_mut() = Default::default();
    ui.entry.set_text("");
    refresh(ui);
    ui.reveal.set_reveal_child(false);
    ui.window.set_visible(false);
    ui.card.clear();
    watchdog::set_open(false);
    if let Some(beat) = ui.heartbeat.take() {
        beat.remove();
    }
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

/// Put the popup on the focused monitor and grab the screen area behind the card,
/// before the popup is visible (otherwise we'd capture ourselves).
fn capture_backdrop(ui: &Ui) {
    let Some(mon) = focused_monitor() else {
        eprintln!("omni: couldn't find the focused monitor");
        return;
    };
    // Put the surface on that monitor so the card lands where we captured.
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

    let card_x = (mon.width - CARD_WIDTH) / 2;
    // Area behind the card plus bleed, clamped to the monitor.
    let x0 = (card_x - glass::BLEED).max(0);
    let y0 = (CARD_TOP - glass::BLEED).max(0);
    let x1 = (card_x + CARD_WIDTH + glass::BLEED).min(mon.width);
    let y1 = (CARD_TOP + CARD_MAX_HEIGHT + glass::BLEED).min(mon.height);
    ui.card.capture(
        mon.x + x0,
        mon.y + y0,
        x1 - x0,
        y1 - y0,
        ((x0 - card_x) as f32, (y0 - CARD_TOP) as f32),
    );
}

/// Cap the result list (and Claude's answer) so the card ends BOTTOM_GAP above the bottom
/// of a screen `height` logical pixels tall.
fn fit_to_monitor(ui: &Ui, height: i32) {
    let room = (height - CARD_TOP - BOTTOM_GAP - CARD_CHROME).max(200);
    ui.list_scroll.set_max_content_height(room);
    ui.answer.set_max_height(room);
    ui.status_scroll.set_max_content_height(room);
}

struct Monitor {
    name: String,
    x: i32,
    y: i32,
    /// Logical size (physical / scale, rotated if needed).
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
    let (mut w, mut h) = (m["width"].as_f64()? / scale, m["height"].as_f64()? / scale);
    if m["transform"].as_i64().unwrap_or(0) % 2 == 1 {
        std::mem::swap(&mut w, &mut h);
    }
    Some(Monitor {
        name: m["name"].as_str()?.to_string(),
        x: m["x"].as_i64()? as i32,
        y: m["y"].as_i64()? as i32,
        width: w.round() as i32,
        height: h.round() as i32,
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
    }
}
