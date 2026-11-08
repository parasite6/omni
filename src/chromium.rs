//! Chromium Embedded Framework rendered directly into a GTK texture.
//! CEF owns HTML, layout, streaming, and input. GTK owns only the containing panel.
use cef::*;
use gtk::{gdk, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static PUMP: RefCell<Option<Arc<MessagePump>>> = const { RefCell::new(None) };
    static CLOSING: RefCell<Vec<Arc<AtomicBool>>> = const { RefCell::new(Vec::new()) };
    static SCRATCH: RefCell<Option<std::path::PathBuf>> = const { RefCell::new(None) };
    static SCANNED_UP_TO: Cell<u32> = const { Cell::new(0) };
}

fn trace_timing(event: &str, since: std::time::Instant) {
    let line = format!(
        "omni-search {event}: {:.1} ms",
        since.elapsed().as_secs_f64() * 1000.0
    );
    if std::env::var_os("OMNI_SEARCH_TIMING").is_some() {
        eprintln!("{line}");
    }
    debug_log(&line);
}

/// The installed build's opt-in log: the same `omni-debug` marker and file as the
/// launcher's probe, so timings survive `install.sh` restarting the daemon.
fn debug_log(line: &str) {
    use std::io::Write;
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    if !dir.join("omni-debug").exists() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("omni-probe.log"))
    {
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let _ = writeln!(f, "{wall} {line}");
    }
}

fn pump_work() {
    let started=std::time::Instant::now();
    do_message_loop_work();
    let elapsed=started.elapsed();
    if elapsed>=Duration::from_millis(50) {
        let line=format!("omni[open]: CEF main-thread work blocked for {:.1}ms",elapsed.as_secs_f64()*1000.0);
        if std::env::var_os("OMNI_LOG_OPEN").is_some() {eprintln!("{line}");}
        debug_log(&line);
    }
}

/// CEF asks for immediate work very often, including while nothing is loading. Running
/// `DoMessageLoopWork` on a 0 ms timer then saturates the GTK thread for as long as the
/// process lives. Coalesce requests to the page's 60 fps frame interval, bounded by
/// CEF's 33 ms fallback ceiling: Brave's own streaming animation plays at full frame
/// rate, and the pump only runs while a page exists, never while the launcher is idle.
const MIN_PUMP_MS: i64 = 16;
/// Pages or their close callbacks exist. With none, the pump is paused entirely: an idle
/// engine must not keep the GTK thread busy.
static SURFACES: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct PumpState {
    pending: Option<glib::Source>,
    generation: u64,
    stopped: bool,
}
#[derive(Default)]
struct MessagePump {
    state: Mutex<PumpState>,
    enabled: AtomicBool,
}

impl MessagePump {
    // CEF may schedule from any thread. GLib dispatches the source on GTK's
    // main context, never inline or recursively inside DoMessageLoopWork.
    fn schedule(self: &Arc<Self>, delay_ms: i64) {
        self.schedule_inner(delay_ms, true);
    }
    fn schedule_inner(self: &Arc<Self>, delay_ms: i64, replace: bool) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if state.stopped {
            return;
        }
        if !replace && state.pending.is_some() {
            return;
        }
        if let Some(old) = state.pending.take() {
            old.destroy();
        }
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        let pump = self.clone();
        let source = glib::timeout_source_new(
            Duration::from_millis(delay_ms.clamp(MIN_PUMP_MS, 33) as u64),
            Some("omni-cef-work"),
            glib::Priority::DEFAULT,
            move || {
                let run = {
                    let mut state = pump.state.lock().unwrap();
                    if state.stopped || state.generation != generation {
                        false
                    } else {
                        state.pending.take();
                        true
                    }
                };
                if run {
                    pump_work();
                    // CEF's callbacks request earlier work; they don't cover all
                    // tasks. Keep its documented maximum 33 ms fallback interval.
                    pump.schedule_inner(33, false);
                }
                glib::ControlFlow::Break
            },
        );
        source.attach(Some(&glib::MainContext::default()));
        state.pending = Some(source);
    }
    fn resume(self: &Arc<Self>) {
        self.enabled.store(true, Ordering::Release);
        self.schedule(0);
    }
    fn pause(&self) {
        self.enabled.store(false, Ordering::Release);
        let mut state = self.state.lock().unwrap();
        state.generation = state.generation.wrapping_add(1);
        if let Some(source) = state.pending.take() {
            source.destroy();
        }
    }
    fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        state.stopped = true;
        if let Some(source) = state.pending.take() {
            source.destroy();
        }
    }
}

wrap_browser_process_handler! {
    struct BrowserWork { pump:Arc<MessagePump> }
    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, delay_ms:i64) {self.pump.schedule(delay_ms);}
    }
}
wrap_app! {
    struct BrowserApp { pump:Arc<MessagePump> }
    impl App {
        fn on_before_command_line_processing(&self, _process_type:Option<&CefString>, command_line:Option<&mut CommandLine>) {
            // Brave's conversation endpoint returned 400 over HTTP/3 in both
            // CEF and Brave controls, while the same requests succeeded over h2.
            // Limit the workaround to Omni's embedded browser network stack.
            if let Some(command_line)=command_line {
                command_line.append_switch(Some(&CefString::from("disable-quic")));
            }
        }
        fn browser_process_handler(&self)->Option<BrowserProcessHandler> {Some(BrowserWork::new(self.pump.clone()))}
    }
}

/// Attach and detach an idle source to bracket an interval on the main context.
/// Source ids increase monotonically, so ids between two brackets belong to work done
/// inside the interval.
fn context_bracket() -> u32 {
    // SAFETY: GLib owns both references; destroy detaches and unref releases ours.
    unsafe {
        let source = glib::ffi::g_idle_source_new();
        let id = glib::ffi::g_source_attach(source, glib::MainContext::default().as_ptr());
        glib::ffi::g_source_destroy(source);
        glib::ffi::g_source_unref(source);
        id
    }
}

fn libcef_mappings() -> Vec<(usize, usize)> {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
        return Vec::new();
    };
    let mut ranges = Vec::new();
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let Some(range) = fields.next() else {
            continue;
        };
        // Match on the path, which may have a " (deleted)" suffix after an upgrade.
        let Some(path) = fields.last() else {
            continue;
        };
        if !path.contains("libcef.so") {
            continue;
        }
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        if let (Ok(start), Ok(end)) = (
            usize::from_str_radix(start, 16),
            usize::from_str_radix(end, 16),
        ) {
            ranges.push((start, end));
        }
    }
    ranges
}

/// libcef attaches two GLib sources to the process-wide main context. One of them asks
/// the loop for immediate work even when Chromium has none, which keeps GTK's thread in
/// a zero-timeout iteration (a full core) for as long as the process lives. Omni drives
/// Chromium itself with `DoMessageLoopWork`, so those sources are redundant: detach the
/// ones libcef created.
fn detach_cef_glib_sources() {
    let mappings = libcef_mappings();
    if mappings.is_empty() {
        return;
    }
    let max_id = context_bracket();
    let first_id = SCANNED_UP_TO.get();
    SCANNED_UP_TO.set(max_id);
    if max_id <= first_id {
        return;
    }
    let context = glib::MainContext::default();
    for id in first_id.saturating_add(1)..=max_id {
        // SAFETY: the returned source is borrowed from the context; destroying it only
        // detaches it and leaves libcef's own reference untouched.
        unsafe {
            let source = glib::ffi::g_main_context_find_source_by_id(context.as_ptr(), id);
            if source.is_null() {
                continue;
            }
            let funcs = (*source).source_funcs as usize;
            if mappings.iter().any(|&(start, end)| funcs >= start && funcs < end) {
                glib::ffi::g_source_destroy(source);
            }
        }
    }
}

fn start() -> Result<(), String> {
    if ACTIVE.get() {
        return Ok(());
    }
    if api_hash(14600, 0).is_null() {
        return Err("Installed CEF does not support API 14600".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let helper = std::env::var_os("OMNI_BROWSER_SUBPROCESS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| exe.with_file_name("omni-browser"))
        .canonicalize()
        .map_err(|e| format!("Chromium helper is missing: {e}"))?;
    let dir = env!("OMNI_CEF_DIR");
    // CEF needs a root for installation metadata even with off-the-record contexts.
    // Keep it private and temporary; no search profile is created beneath it.
    let root = glib::mkdtemp_full(glib::user_runtime_dir().join("omni-chromium-XXXXXX"), 0o700)
        .ok_or("Couldn’t create temporary Chromium runtime directory")?;
    let settings = Settings {
        windowless_rendering_enabled: 1,
        external_message_pump: 1,
        resources_dir_path: CefString::from(dir),
        locales_dir_path: CefString::from(format!("{dir}/locales").as_str()),
        root_cache_path: CefString::from(root.to_string_lossy().as_ref()),
        // An empty cache_path selects CEF's in-memory/incognito storage mode.
        cache_path: CefString::default(),
        browser_subprocess_path: CefString::from(helper.to_string_lossy().as_ref()),
        accept_language_list: CefString::from("en-US,en"),
        background_color: 0xff1b1b1f,
        // GLib handles daemon termination so browsers close before process exit.
        disable_signal_handlers: 1,
        ..Default::default()
    };
    let args = cef::args::Args::new();
    let pump = Arc::new(MessagePump::default());
    let mut app = BrowserApp::new(pump.clone());
    let initialized = initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) == 1;
    if !initialized {
        pump.stop();
        let _ = std::fs::remove_dir_all(&root);
        return Err("Chromium initialization failed".into());
    }
    detach_cef_glib_sources();
    SCRATCH.set(Some(root));
    ACTIVE.set(true);
    PUMP.set(Some(pump));
    Ok(())
}

/// Start servicing CEF while at least one page exists.
fn resume_pump() {
    PUMP.with_borrow(|pump| {
        if let Some(pump) = pump {
            pump.resume();
        }
    });
}

/// The last page is closing. Keep servicing CEF briefly so `on_before_close` runs, then
/// stop touching the GTK thread until the next search opens a page.
fn pause_after_close() {
    if SURFACES.load(Ordering::Acquire) != 0 {
        return;
    }
    let pump = PUMP.with_borrow(|pump| pump.clone());
    let Some(pump) = pump else { return };
    glib::timeout_add_local_once(Duration::from_secs(1), move || {
        if SURFACES.load(Ordering::Acquire) == 0 {
            pump.pause();
        }
    });
}

/// Initialize the engine without creating a page, profile, or search session.
/// Kept for the loading-time diagnostic; the application starts it on first search.
#[allow(dead_code)]
pub fn warm_up() -> Result<(), String> {
    start()
}

pub fn finish() -> Result<(), String> {
    if !ACTIVE.replace(false) {
        return Ok(());
    }
    if let Some(pump) = PUMP.take() {
        pump.stop();
    }
    // CloseBrowser is asynchronous. CEF must outlive every OnBeforeClose callback.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !CLOSING.with_borrow(|browsers| browsers.iter().all(|b| b.load(Ordering::Acquire))) {
        if std::time::Instant::now() >= deadline {
            // Calling CefShutdown with a live browser can crash. On this failure path,
            // leave teardown to process exit and report it instead of claiming success.
            return Err("Chromium did not finish closing; CefShutdown skipped".into());
        }
        pump_work();
        std::thread::sleep(Duration::from_millis(10));
    }
    CLOSING.with_borrow_mut(Vec::clear);
    shutdown();
    if let Some(root) = SCRATCH.take() {
        std::fs::remove_dir_all(root)
            .map_err(|e| format!("Temporary Chromium metadata cleanup failed: {e}"))?;
    }
    Ok(())
}

#[derive(Default)]
struct State {
    size: (i32, i32),
    frame: Option<(i32, i32, Vec<u8>)>,
    loading: bool,
    error: Option<String>,
    preferences_pending: usize,
    preferences_error: Option<String>,
    queued_url: Option<String>,
    navigation_started: Option<std::time::Instant>,
    /// The search page itself has started loading; its frames are no longer about:blank.
    committed: bool,
    document_ready: bool,
    /// Text the page copied, for the GTK clipboard (Chromium's own never reaches it).
    copied: Option<String>,
    /// Last click or key the user sent the page. Copies count only right after one.
    last_input: Option<std::time::Instant>,
}
type Shared = Arc<Mutex<State>>;

wrap_set_cookie_callback! {
    struct PreferenceCookie { state: Shared }
    impl SetCookieCallback {
        fn on_complete(&self, success:i32) {
            let mut state=self.state.lock().unwrap();
            state.preferences_pending=state.preferences_pending.saturating_sub(1);
            if success==0 {state.preferences_error=Some("Couldn’t set Brave Search preferences".into());}
        }
    }
}
wrap_completion_callback! {
    struct PreferenceStorage { context:RequestContext, state:Shared }
    impl CompletionCallback {
        fn on_complete(&self) {
            let Some(manager)=self.context.cookie_manager(None) else {
                self.state.lock().unwrap().preferences_error=Some("Couldn’t initialize Brave Search preferences".into());
                return;
            };
            for (name,value) in [("safesearch","off"),("useLocation","0")] {
                // Host-only, session-only cookies in this Surface's private context.
                let cookie=Cookie {name:CefString::from(name),value:CefString::from(value),
                    path:CefString::from("/"),secure:1,..Default::default()};
                let mut callback=PreferenceCookie::new(self.state.clone());
                if manager.set_cookie(Some(&CefString::from("https://search.brave.com/")),Some(&cookie),Some(&mut callback))==0 {
                    self.state.lock().unwrap().preferences_error=Some("Couldn’t set Brave Search preferences".into());
                }
            }
        }
    }
}

wrap_render_handler! {
    struct Render { state: Shared }
    impl RenderHandler {
        fn view_rect(&self, _browser:Option<&mut Browser>, rect:Option<&mut Rect>) {
            if let Some(rect)=rect { let s=self.state.lock().unwrap(); *rect=Rect{x:0,y:0,width:s.size.0.max(1),height:s.size.1.max(1)}; }
        }
        fn on_paint(&self, _browser:Option<&mut Browser>, kind:PaintElementType, _dirty:Option<&[Rect]>, buffer:*const u8, width:i32, height:i32) {
            if kind!=PaintElementType::VIEW || buffer.is_null() { return; }
            let Some(len)=crate::browser_frame::byte_len(width,height) else { return; };
            // SAFETY: CEF guarantees width*height BGRA pixels valid for this callback.
            let pixels=unsafe { std::slice::from_raw_parts(buffer,len) }.to_vec();
            let mut s=self.state.lock().unwrap();
            s.frame=Some((width,height,pixels));
            // Hand over on the page's first frame so Brave's own loading animation is
            // what the user sees, whole, instead of a stand-in until the DOM is parsed.
            if s.committed {s.document_ready=true;}
        }
    }
}
wrap_load_handler! {
    struct Load { state: Shared, css:String }
    impl LoadHandler {
        fn on_load_start(&self, _browser:Option<&mut Browser>, frame:Option<&mut Frame>, _transition:TransitionType) {
            let Some(frame)=frame.filter(|f|f.is_main()!=0) else {return;};
            let mut s=self.state.lock().unwrap();s.loading=true;s.error=None;
            if CefString::from(&frame.url()).to_string()!="about:blank" {s.committed=true;}
            if let Some(t)=s.navigation_started {trace_timing("navigation committed",t);}
        }
        fn on_load_end(&self, _browser:Option<&mut Browser>, frame:Option<&mut Frame>, _status:i32) {
            let Some(frame)=frame.filter(|f|f.is_main()!=0) else {return;};
            if CefString::from(&frame.url()).to_string()=="about:blank" {return;}
            {let mut state=self.state.lock().unwrap();state.loading=false;
                if !CefString::from(&frame.url()).to_string().starts_with("https://search.brave.com/ask") {state.document_ready=true;}}
            if let Some(t)=self.state.lock().unwrap().navigation_started {trace_timing("document loaded",t);}
            let css=serde_json::to_string(&self.css).unwrap();
            // Normally already installed at document start; rewriting identical text would
            // restyle the whole page once more (a visible flicker of the prompt box).
            let js=format!("if(location.origin==='https://search.brave.com' && location.pathname==='/ask'){{let s=document.getElementById('omni-page-filter');if(!s){{s=document.createElement('style');s.id='omni-page-filter';document.head.append(s);}}if(s.textContent!=={css})s.textContent={css};}}");
            frame.execute_java_script(Some(&CefString::from(js.as_str())),None,0);
        }
        fn on_load_error(&self, _browser:Option<&mut Browser>, frame:Option<&mut Frame>, code:Errorcode, text:Option<&CefString>, _url:Option<&CefString>) {
            if code==Errorcode::ABORTED || !frame.is_some_and(|f|f.is_main()!=0) {return;}
            let mut s=self.state.lock().unwrap();s.loading=false;s.error=Some(text.map(ToString::to_string).unwrap_or_else(||"Page load failed".into()));
        }
    }
}
wrap_display_handler! {
    struct PageDisplay { state:Shared }
    impl DisplayHandler {
        fn on_console_message(&self, _browser:Option<&mut Browser>, _level:LogSeverity, message:Option<&CefString>, source:Option<&CefString>, _line:i32)->i32 {
            let Some(message)=message.map(ToString::to_string) else {return 0;};
            if message=="omni-search-page-ready" {
                self.state.lock().unwrap().document_ready=true;
                return 1;
            }
            if let Some(json)=message.strip_prefix("omni-copy:") {
                // Any script can log this. Honour it only from Brave's own origin (or the
                // injected script, which has no source) and right after a user click or
                // key, so nothing on the page can fill the clipboard by itself.
                let source=source.map(ToString::to_string).unwrap_or_default();
                let trusted=source.is_empty() || source.starts_with("https://search.brave.com/");
                let mut s=self.state.lock().unwrap();
                let recent=s.last_input.is_some_and(|t|t.elapsed()<Duration::from_millis(1500));
                if let (true,true,Ok(text))=(trusted,recent,serde_json::from_str::<String>(json)) {s.copied=Some(text);}
                return 1;
            }
            if std::env::var_os("OMNI_SEARCH_DIAGNOSTICS").is_some() {
                if message.starts_with("omni-search-diagnostic:") {eprintln!("{message}");return 1;}
            }
            0
        }
    }
}
wrap_client! {
    struct ClientImpl { render:RenderHandler, load:LoadHandler, life:LifeSpanHandler, display:DisplayHandler }
    impl Client {
        fn render_handler(&self)->Option<RenderHandler>{Some(self.render.clone())}
        fn load_handler(&self)->Option<LoadHandler>{Some(self.load.clone())}
        fn life_span_handler(&self)->Option<LifeSpanHandler>{Some(self.life.clone())}
        fn display_handler(&self)->Option<DisplayHandler>{Some(self.display.clone())}
    }
}

wrap_life_span_handler! {
    struct Life { closed:Arc<AtomicBool> }
    impl LifeSpanHandler {
        fn on_before_close(&self, _browser:Option<&mut Browser>){self.closed.store(true,Ordering::Release);}
        fn on_before_popup(&self, browser:Option<&mut Browser>, _frame:Option<&mut Frame>, _popup_id:i32,
            url:Option<&CefString>, _name:Option<&CefString>, _disposition:WindowOpenDisposition,
            _gesture:i32, _features:Option<&PopupFeatures>, _window:Option<&mut WindowInfo>,
            _client:Option<&mut Option<Client>>, _settings:Option<&mut BrowserSettings>,
            _extra:Option<&mut Option<DictionaryValue>>, _no_js:Option<&mut i32>)->i32 {
            // Keep source links in this surface instead of creating unmanaged CEF windows.
            if let (Some(browser),Some(url))=(browser,url) {
                let text=url.to_string();
                if text.starts_with("https://") || text.starts_with("http://") {
                    if let Some(frame)=browser.main_frame(){frame.load_url(Some(url));}
                }
            }
            1
        }
    }
}

pub struct Surface {
    pub picture: gtk::Picture,
    browser: Browser,
    tick: Option<glib::SourceId>,
    state: Shared,
    ready_callback: ReadyCallback,
}
type ReadyCallback = Rc<RefCell<Option<Box<dyn FnOnce()>>>>;

impl Surface {
    pub fn new(css: &str, status: &gtk::Label) -> Result<Self, String> {
        let creation_started = std::time::Instant::now();
        start()?;
        trace_timing("CEF initialized", creation_started);
        let picture = gtk::Picture::builder()
            .hexpand(true)
            .vexpand(true)
            .can_shrink(true)
            .focusable(true)
            .build();
        picture.set_content_fit(gtk::ContentFit::Fill);
        let state = Arc::new(Mutex::new(State {
            size: (704, 400),
            preferences_pending: 2,
            ..Default::default()
        }));
        let closed = Arc::new(AtomicBool::new(false));
        let mut client = ClientImpl::new(
            Render::new(state.clone()),
            Load::new(state.clone(), css.into()),
            Life::new(closed.clone()),
            PageDisplay::new(state.clone()),
        );
        let info = WindowInfo::default().set_as_windowless(0);
        // Each Surface gets a distinct off-the-record context. Cookies, localStorage,
        // IndexedDB, service workers and history cannot carry into the next search.
        let mut context =
            request_context_create_context(Some(&RequestContextSettings::default()), None)
                .ok_or("Couldn’t create temporary Chromium session")?;
        let browser = browser_host_create_browser_sync(
            Some(&info),
            Some(&mut client),
            Some(&CefString::from("about:blank")),
            Some(&BrowserSettings {
                windowless_frame_rate: 60,
                background_color: 0xff1b1b1f,
                ..Default::default()
            }),
            None,
            Some(&mut context),
        )
        .ok_or("Couldn’t create Chromium view")?;
        trace_timing("private browser created", creation_started);
        CLOSING.with_borrow_mut(|browsers| {
            browsers.retain(|b| !b.load(Ordering::Acquire));
            browsers.push(closed);
        });
        SURFACES.fetch_add(1, Ordering::AcqRel);
        detach_cef_glib_sources();
        resume_pump();
        let mut ready = PreferenceStorage::new(context.clone(), state.clone());
        if context.cookie_manager(Some(&mut ready)).is_none() {
            if let Some(host) = browser.host() {
                host.close_browser(1);
            }
            if SURFACES.fetch_sub(1, Ordering::AcqRel) == 1 {
                pause_after_close();
            }
            return Err("Couldn’t initialize Brave Search preferences".into());
        }
        let weak = picture.downgrade();
        let ready_callback: ReadyCallback = Rc::new(RefCell::new(None));
        let ready = ready_callback.clone();
        let b = browser.clone();
        let label = status.clone();
        let shared = state.clone();
        let preference_deadline = std::time::Instant::now() + Duration::from_secs(5);
        let tick = glib::timeout_add_local(Duration::from_millis(16), move || {
            let Some(p) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let (frame, resize, loading, error, navigate, document_ready) = {
                let mut s = shared.lock().unwrap();
                // Lay the page out at its final size while it is still hidden behind
                // the loading pane. The parent stack is allocated even when this child
                // isn't, so the page doesn't re-flow (and jump) the moment it is shown.
                let size = if p.is_mapped() {
                    (p.width(), p.height())
                } else {
                    p.parent().map_or((0, 0), |parent| (parent.width(), parent.height()))
                };
                let resize = size.0 > 0 && size.1 > 0 && size != s.size;
                if resize {
                    s.size = size;
                }
                if s.preferences_pending > 0 && std::time::Instant::now() >= preference_deadline {
                    s.preferences_error
                        .get_or_insert_with(|| "Brave Search preferences timed out".into());
                }
                let navigate = if s.preferences_error.is_none() && s.preferences_pending == 0 {
                    let url = s.queued_url.take();
                    if url.is_some() {
                        s.navigation_started = Some(std::time::Instant::now());
                        trace_timing("navigation ready", creation_started);
                    }
                    url
                } else {
                    None
                };
                (
                    s.frame.take(),
                    resize,
                    s.loading,
                    s.preferences_error.clone().or_else(|| s.error.clone()),
                    navigate,
                    s.document_ready,
                )
            };
            // SetCookie is asynchronous: never send the first request before both
            // completion callbacks succeed. Fail visibly instead of using defaults.
            if let Some(url) = navigate {
                if let Some(frame) = b.main_frame() {
                    frame.load_url(Some(&CefString::from(url.as_str())));
                }
            }
            if resize {
                if let Some(h) = b.host() {
                    h.was_resized();
                }
            }
            if let Some(text) = shared.lock().unwrap().copied.take() {
                p.clipboard().set_text(&text);
            }
            if let Some((w, h, data)) = frame {
                let bytes = glib::Bytes::from_owned(data);
                let texture = gdk::MemoryTexture::new(
                    w,
                    h,
                    gdk::MemoryFormat::B8g8r8a8Premultiplied,
                    &bytes,
                    w as usize * 4,
                );
                p.set_paintable(Some(&texture));
            }
            if error.is_some() || (document_ready && p.paintable().is_some()) {
                if let Some(callback) = ready.borrow_mut().take() {
                    trace_timing("page shown", creation_started);
                    callback();
                    p.grab_focus();
                }
            }
            label.set_text(error.as_deref().unwrap_or(if loading {
                "Loading Brave Search…"
            } else {
                "Brave Search"
            }));
            glib::ControlFlow::Continue
        });
        let b = browser.clone();
        picture.connect_map(move |_| {
            if let Some(h) = b.host() {
                h.was_hidden(0);
                h.was_resized();
            }
        });
        let b = browser.clone();
        picture.connect_unmap(move |_| {
            if let Some(h) = b.host() {
                h.set_focus(0);
                h.was_hidden(1);
            }
        });
        connect_input(&picture, &browser, &state);
        Ok(Self {
            picture,
            browser,
            tick: Some(tick),
            state,
            ready_callback,
        })
    }

    pub fn load(&self, url: &str) {
        if let Some(host) = self.browser.host() {
            host.was_hidden(0);
            host.set_focus(1);
        }
        let mut state = self.state.lock().unwrap();
        state.queued_url = Some(url.into());
        state.loading = true;
        state.committed = false;
        state.document_ready = false;
        drop(state);
        self.picture.grab_focus();
    }
    pub fn on_ready(&self, callback: impl FnOnce() + 'static) {
        *self.ready_callback.borrow_mut() = Some(Box::new(callback));
    }
    pub fn stop(&self) {
        self.state.lock().unwrap().queued_url = None;
        self.browser.stop_load();
        if let Some(h) = self.browser.host() {
            h.set_focus(0);
            h.was_hidden(1);
        }
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        if let Some(tick) = self.tick.take() {
            tick.remove();
        }
        if let Some(h) = self.browser.host() {
            h.close_browser(1);
        }
        if SURFACES.fetch_sub(1, Ordering::AcqRel) == 1 {
            pause_after_close();
        }
    }
}

fn modifiers(m: gdk::ModifierType) -> u32 {
    let mut flags = 0;
    for (mask, bit) in [
        (gdk::ModifierType::SHIFT_MASK, 2),
        (gdk::ModifierType::CONTROL_MASK, 4),
        (gdk::ModifierType::ALT_MASK, 8),
        (gdk::ModifierType::BUTTON1_MASK, 16),
        (gdk::ModifierType::BUTTON2_MASK, 32),
        (gdk::ModifierType::BUTTON3_MASK, 64),
        (gdk::ModifierType::SUPER_MASK, 128),
    ] {
        if m.contains(mask) {
            flags |= bit;
        }
    }
    flags
}

fn connect_input(picture: &gtk::Picture, browser: &Browser, state: &Shared) {
    let pointer = Rc::new(Cell::new((0.0, 0.0)));
    let position = pointer.clone();
    let motion = gtk::EventControllerMotion::new();
    let b = browser.clone();
    motion.connect_motion(move |controller, x, y| {
        position.set((x, y));
        if let Some(h) = b.host() {
            h.send_mouse_move_event(
                Some(&MouseEvent {
                    x: x as i32,
                    y: y as i32,
                    modifiers: modifiers(controller.current_event_state()),
                }),
                0,
            );
        }
    });
    let b = browser.clone();
    motion.connect_leave(move |_| {
        if let Some(h) = b.host() {
            h.send_mouse_move_event(Some(&MouseEvent::default()), 1);
        }
    });
    picture.add_controller(motion);
    let click = gtk::GestureClick::new();
    click.set_button(0);
    let b = browser.clone();
    let weak = picture.downgrade();
    let input = state.clone();
    click.connect_pressed(move |g, n, x, y| {
        input.lock().unwrap().last_input = Some(std::time::Instant::now());
        if let Some(p) = weak.upgrade() {
            p.grab_focus();
        }
        if let Some(h) = b.host() {
            h.set_focus(1);
            h.send_mouse_click_event(
                Some(&MouseEvent {
                    x: x as i32,
                    y: y as i32,
                    modifiers: modifiers(g.current_event_state()),
                }),
                mouse_button(g.current_button()),
                0,
                n,
            );
        }
    });
    let b = browser.clone();
    click.connect_released(move |g, n, x, y| {
        if let Some(h) = b.host() {
            h.send_mouse_click_event(
                Some(&MouseEvent {
                    x: x as i32,
                    y: y as i32,
                    modifiers: modifiers(g.current_event_state()),
                }),
                mouse_button(g.current_button()),
                1,
                n,
            );
        }
    });
    picture.add_controller(click);
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let b = browser.clone();
    scroll.connect_scroll(move |c, dx, dy| {
        if let Some(h) = b.host() {
            let (x, y) = pointer.get();
            h.send_mouse_wheel_event(
                Some(&MouseEvent {
                    x: x as i32,
                    y: y as i32,
                    modifiers: modifiers(c.current_event_state()),
                }),
                (-dx * 48.) as i32,
                (-dy * 48.) as i32,
            );
        }
        glib::Propagation::Stop
    });
    picture.add_controller(scroll);
    let keys = gtk::EventControllerKey::new();
    let b = browser.clone();
    let input = state.clone();
    keys.connect_key_pressed(move |_, key, code, mods| {
        input.lock().unwrap().last_input = Some(std::time::Instant::now());
        if key == gdk::Key::Escape {
            return glib::Propagation::Proceed;
        }
        if let Some(h) = b.host() {
            let mut event = key_event(key, code, mods, KeyEventType::RAWKEYDOWN);
            h.send_key_event(Some(&event));
            if !mods.intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::SUPER_MASK,
            ) {
                if let Some(ch) = key.to_unicode() {
                    for unit in ch.encode_utf16(&mut [0; 2]) {
                        event.type_ = KeyEventType::CHAR;
                        event.character = *unit;
                        event.unmodified_character = *unit;
                        h.send_key_event(Some(&event));
                    }
                }
            }
        }
        glib::Propagation::Stop
    });
    let b = browser.clone();
    keys.connect_key_released(move |_, key, code, mods| {
        if let Some(h) = b.host() {
            h.send_key_event(Some(&key_event(key, code, mods, KeyEventType::KEYUP)));
        }
    });
    picture.add_controller(keys);
}
fn mouse_button(button: u32) -> MouseButtonType {
    match button {
        2 => MouseButtonType::MIDDLE,
        3 => MouseButtonType::RIGHT,
        _ => MouseButtonType::LEFT,
    }
}
fn key_event(key: gdk::Key, code: u32, mods: gdk::ModifierType, kind: KeyEventType) -> KeyEvent {
    let virtual_key = match key {
        gdk::Key::BackSpace => 8,
        gdk::Key::Tab => 9,
        gdk::Key::Return | gdk::Key::KP_Enter => 13,
        gdk::Key::Escape => 27,
        gdk::Key::Left => 37,
        gdk::Key::Up => 38,
        gdk::Key::Right => 39,
        gdk::Key::Down => 40,
        gdk::Key::Delete => 46,
        gdk::Key::Home => 36,
        gdk::Key::End => 35,
        gdk::Key::Page_Up => 33,
        gdk::Key::Page_Down => 34,
        _ => key
            .to_unicode()
            .map(|c| c.to_ascii_uppercase() as i32)
            .unwrap_or(0),
    };
    KeyEvent {
        type_: kind,
        windows_key_code: virtual_key,
        native_key_code: code as i32,
        modifiers: modifiers(mods),
        ..Default::default()
    }
}
