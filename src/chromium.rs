//! The Chromium host: `omni-browser --omni-host`, a short-lived process that owns the CEF
//! engine and renders Brave Search off screen for the launcher.
//!
//! CEF can only be initialized once per process, so an engine inside the resident launcher
//! could never give its memory back: the GPU process, network service, zygotes and
//! renderers stayed for the daemon's lifetime. Here the launcher starts this process on
//! the first search and closes its socket 10 s after the last page closes; the host then
//! shuts CEF down and exits, and every Chromium process goes with it.
//!
//! The launcher's socket is fd 3. Commands are read on a thread and run on CEF's UI thread
//! as tasks; frames and page events are written back by a writer thread that only ever
//! keeps the newest frame, so a busy launcher costs dropped frames, never a backlog.
use crate::web_protocol as proto;
use cef::*;
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

thread_local! {
    static PAGES: RefCell<HashMap<u32, Page>> = RefCell::new(HashMap::new());
    static CLOSING: RefCell<Vec<Arc<AtomicBool>>> = const { RefCell::new(Vec::new()) };
}
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static OUTBOX: OnceLock<Outbox> = OnceLock::new();
static INBOX: Mutex<VecDeque<Command>> = Mutex::new(VecDeque::new());

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

// ---------------------------------------------------------------------------------------
// Outgoing messages

#[derive(Default)]
struct OutState {
    messages: VecDeque<Vec<u8>>,
    /// Only the newest frame is worth sending; an older unsent one is replaced.
    frame: Option<Vec<u8>>,
    closed: bool,
}
#[derive(Default)]
struct Outbox {
    state: Mutex<OutState>,
    ready: Condvar,
}

fn send(message: Vec<u8>) {
    if let Some(out) = OUTBOX.get() {
        out.state.lock().unwrap().messages.push_back(message);
        out.ready.notify_one();
    }
}
fn send_frame(message: Vec<u8>) {
    if let Some(out) = OUTBOX.get() {
        out.state.lock().unwrap().frame = Some(message);
        out.ready.notify_one();
    }
}

fn writer(mut stream: UnixStream) {
    let out = OUTBOX.get().unwrap();
    loop {
        let next = {
            let mut s = out.state.lock().unwrap();
            loop {
                if let Some(m) = s.messages.pop_front() {
                    break Some(m);
                }
                if let Some(f) = s.frame.take() {
                    break Some(f);
                }
                if s.closed {
                    break None;
                }
                s = out.ready.wait(s).unwrap();
            }
        };
        let Some(message) = next else { return };
        if stream.write_all(&message).is_err() {
            // The launcher is gone; the reader sees the same and shuts down.
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------
// Incoming commands, run on CEF's UI thread

enum Command {
    Open { id: u32, css: String },
    Close(u32),
    Load { id: u32, url: String },
    Stop(u32),
    Resize { id: u32, w: i32, h: i32 },
    Visible { id: u32, shown: bool },
    Focus { id: u32, focused: bool },
    MouseMove { id: u32, event: MouseEvent, leave: bool },
    MouseClick { id: u32, event: MouseEvent, button: i32, up: bool, count: i32 },
    Wheel { id: u32, event: MouseEvent, dx: i32, dy: i32 },
    Key { id: u32, event: KeyEvent },
    TryNavigate(u32),
    PreferencesTimeout(u32),
    Shutdown,
}

wrap_task! {
    struct Drain;
    impl Task {
        fn execute(&self) {
            loop {
                let Some(command) = INBOX.lock().unwrap().pop_front() else { break };
                handle(command);
            }
        }
    }
}

fn post(command: Command) {
    INBOX.lock().unwrap().push_back(command);
    post_task(ThreadId::UI, Some(&mut Drain::new()));
}

fn post_delayed(command: Command, delay: Duration) {
    // Runs after the delay; the queue may hold earlier commands by then, which is fine:
    // they are all due.
    wrap_task! {
        struct Delayed { command: Arc<Mutex<Option<Command>>> }
        impl Task {
            fn execute(&self) {
                if let Some(command) = self.command.lock().unwrap().take() {
                    post(command);
                }
            }
        }
    }
    let mut task = Delayed::new(Arc::new(Mutex::new(Some(command))));
    post_delayed_task(ThreadId::UI, Some(&mut task), delay.as_millis() as i64);
}

fn mouse(m: &mut proto::Message) -> Option<MouseEvent> {
    Some(MouseEvent {
        x: m.int()?,
        y: m.int()?,
        modifiers: m.int()? as u32,
    })
}

fn decode(mut m: proto::Message) -> Option<Command> {
    let id = m.id;
    Some(match m.tag {
        proto::OPEN => Command::Open { id, css: m.str()? },
        proto::CLOSE => Command::Close(id),
        proto::LOAD => Command::Load { id, url: m.str()? },
        proto::STOP => Command::Stop(id),
        proto::RESIZE => Command::Resize { id, w: m.int()?, h: m.int()? },
        proto::VISIBLE => Command::Visible { id, shown: m.flag()? },
        proto::FOCUS => Command::Focus { id, focused: m.flag()? },
        proto::MOUSE_MOVE => Command::MouseMove { id, event: mouse(&mut m)?, leave: m.flag()? },
        proto::MOUSE_CLICK => Command::MouseClick {
            id,
            event: mouse(&mut m)?,
            button: m.int()?,
            up: m.flag()?,
            count: m.int()?,
        },
        proto::WHEEL => Command::Wheel { id, event: mouse(&mut m)?, dx: m.int()?, dy: m.int()? },
        proto::KEY => {
            let kind = match m.int()? {
                proto::KEY_RAWDOWN => KeyEventType::RAWKEYDOWN,
                proto::KEY_UP => KeyEventType::KEYUP,
                proto::KEY_CHAR => KeyEventType::CHAR,
                _ => return None,
            };
            let windows_key_code = m.int()?;
            let native_key_code = m.int()?;
            let modifiers = m.int()? as u32;
            let character = m.int()? as u16;
            Command::Key {
                id,
                event: KeyEvent {
                    type_: kind,
                    windows_key_code,
                    native_key_code,
                    modifiers,
                    character,
                    unmodified_character: character,
                    ..Default::default()
                },
            }
        }
        _ => return None,
    })
}

fn reader(mut stream: UnixStream) {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        let mut messages = Vec::new();
        let Ok(used) = proto::parse(&buf, &mut messages) else { break };
        let commands: Vec<Command> = messages.into_iter().filter_map(decode).collect();
        buf.drain(..used);
        if !commands.is_empty() {
            INBOX.lock().unwrap().extend(commands);
            post_task(ThreadId::UI, Some(&mut Drain::new()));
        }
    }
    // The launcher closed the socket (idle, or it exited): close every page, shut CEF
    // down and exit. If a page never finishes closing, don't linger as an orphan.
    post(Command::Shutdown);
    std::thread::sleep(Duration::from_secs(10));
    debug_log("omni-browser: CEF did not shut down in time; exiting");
    // SAFETY: plain process exit without running CEF's (unfinished) teardown.
    unsafe { libc::_exit(0) };
}

// ---------------------------------------------------------------------------------------
// Pages

#[derive(Default)]
struct State {
    id: u32,
    size: (i32, i32),
    preferences_pending: usize,
    preferences_error: Option<String>,
    queued_url: Option<String>,
    navigation_started: Option<std::time::Instant>,
    created: Option<std::time::Instant>,
    /// The search page itself has started loading; its frames are no longer about:blank.
    committed: bool,
    document_ready: bool,
    /// Last click or key the user sent the page. Copies count only right after one.
    last_input: Option<std::time::Instant>,
}
type Shared = Arc<Mutex<State>>;

struct Page {
    browser: Browser,
    state: Shared,
}

fn mark_ready(state: &mut State) {
    if !state.document_ready {
        state.document_ready = true;
        if let Some(t) = state.created {
            trace_timing("page ready", t);
        }
        send(proto::Writer::new(proto::READY, state.id).finish());
    }
}

fn send_error(id: u32, error: Option<&str>) {
    send(proto::Writer::new(proto::ERROR, id).str(error.unwrap_or("")).finish());
}

wrap_set_cookie_callback! {
    struct PreferenceCookie { state: Shared }
    impl SetCookieCallback {
        fn on_complete(&self, success:i32) {
            let id={
                let mut state=self.state.lock().unwrap();
                state.preferences_pending=state.preferences_pending.saturating_sub(1);
                if success==0 && state.preferences_error.is_none() {
                    state.preferences_error=Some("Couldn’t set Brave Search preferences".into());
                    send_error(state.id,state.preferences_error.as_deref());
                }
                state.id
            };
            post(Command::TryNavigate(id));
        }
    }
}
wrap_completion_callback! {
    struct PreferenceStorage { context:RequestContext, state:Shared }
    impl CompletionCallback {
        fn on_complete(&self) {
            let fail=|message:&str|{
                let mut state=self.state.lock().unwrap();
                if state.preferences_error.is_none() {
                    state.preferences_error=Some(message.into());
                    send_error(state.id,Some(message));
                }
            };
            let Some(manager)=self.context.cookie_manager(None) else {
                fail("Couldn’t initialize Brave Search preferences");
                return;
            };
            for (name,value) in [("safesearch","off"),("useLocation","0")] {
                // Host-only, session-only cookies in this page's private context.
                let cookie=Cookie {name:CefString::from(name),value:CefString::from(value),
                    path:CefString::from("/"),secure:1,..Default::default()};
                let mut callback=PreferenceCookie::new(self.state.clone());
                if manager.set_cookie(Some(&CefString::from("https://search.brave.com/")),Some(&cookie),Some(&mut callback))==0 {
                    fail("Couldn’t set Brave Search preferences");
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
            let pixels=unsafe { std::slice::from_raw_parts(buffer,len) };
            let mut s=self.state.lock().unwrap();
            send_frame(proto::Writer::with_capacity(proto::FRAME,s.id,len+8).int(width).int(height).bytes(pixels).finish());
            // Hand over on the page's first frame so Brave's own loading animation is
            // what the user sees, whole, instead of a stand-in until the DOM is parsed.
            if s.committed {mark_ready(&mut s);}
        }
    }
}
wrap_load_handler! {
    struct Load { state: Shared, css:String }
    impl LoadHandler {
        fn on_load_start(&self, _browser:Option<&mut Browser>, frame:Option<&mut Frame>, _transition:TransitionType) {
            let Some(frame)=frame.filter(|f|f.is_main()!=0) else {return;};
            let s=&mut *self.state.lock().unwrap();
            send(proto::Writer::new(proto::LOADING,s.id).flag(true).finish());
            if s.preferences_error.is_none() {send_error(s.id,None);}
            if CefString::from(&frame.url()).to_string()!="about:blank" {s.committed=true;}
            if let Some(t)=s.navigation_started {trace_timing("navigation committed",t);}
        }
        fn on_load_end(&self, _browser:Option<&mut Browser>, frame:Option<&mut Frame>, _status:i32) {
            let Some(frame)=frame.filter(|f|f.is_main()!=0) else {return;};
            let url=CefString::from(&frame.url()).to_string();
            if url=="about:blank" {return;}
            {
                let s=&mut *self.state.lock().unwrap();
                send(proto::Writer::new(proto::LOADING,s.id).flag(false).finish());
                if !url.starts_with("https://search.brave.com/ask") {mark_ready(s);}
                if let Some(t)=s.navigation_started {trace_timing("document loaded",t);}
            }
            let css=serde_json::to_string(&self.css).unwrap();
            // Normally already installed at document start; rewriting identical text would
            // restyle the whole page once more (a visible flicker of the prompt box).
            let js=format!("if(location.origin==='https://search.brave.com' && location.pathname==='/ask'){{let s=document.getElementById('omni-page-filter');if(!s){{s=document.createElement('style');s.id='omni-page-filter';document.head.append(s);}}if(s.textContent!=={css})s.textContent={css};}}");
            frame.execute_java_script(Some(&CefString::from(js.as_str())),None,0);
        }
        fn on_load_error(&self, _browser:Option<&mut Browser>, frame:Option<&mut Frame>, code:Errorcode, text:Option<&CefString>, _url:Option<&CefString>) {
            if code==Errorcode::ABORTED || !frame.is_some_and(|f|f.is_main()!=0) {return;}
            let id=self.state.lock().unwrap().id;
            send(proto::Writer::new(proto::LOADING,id).flag(false).finish());
            let text=text.map(ToString::to_string).unwrap_or_else(||"Page load failed".into());
            send_error(id,Some(&text));
        }
    }
}
wrap_display_handler! {
    struct PageDisplay { state:Shared }
    impl DisplayHandler {
        fn on_console_message(&self, _browser:Option<&mut Browser>, _level:LogSeverity, message:Option<&CefString>, source:Option<&CefString>, _line:i32)->i32 {
            let Some(message)=message.map(ToString::to_string) else {return 0;};
            if message=="omni-search-page-ready" {
                mark_ready(&mut self.state.lock().unwrap());
                return 1;
            }
            if let Some(json)=message.strip_prefix("omni-copy:") {
                // Any script can log this. Honour it only from Brave's own origin (or the
                // injected script, which has no source) and right after a user click or
                // key, so nothing on the page can fill the clipboard by itself.
                let source=source.map(ToString::to_string).unwrap_or_default();
                let trusted=source.is_empty() || source.starts_with("https://search.brave.com/");
                let s=self.state.lock().unwrap();
                let recent=s.last_input.is_some_and(|t|t.elapsed()<Duration::from_millis(1500));
                if let (true,true,Ok(text))=(trusted,recent,serde_json::from_str::<String>(json)) {
                    send(proto::Writer::new(proto::COPIED,s.id).str(&text).finish());
                }
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

fn all_closed() -> bool {
    CLOSING.with_borrow(|browsers| browsers.iter().all(|b| b.load(Ordering::Acquire)))
}

wrap_life_span_handler! {
    struct Life { closed:Arc<AtomicBool> }
    impl LifeSpanHandler {
        fn on_before_close(&self, _browser:Option<&mut Browser>){
            self.closed.store(true,Ordering::Release);
            if SHUTTING_DOWN.load(Ordering::Acquire) && all_closed() {quit_message_loop();}
        }
        fn on_before_popup(&self, browser:Option<&mut Browser>, _frame:Option<&mut Frame>, _popup_id:i32,
            url:Option<&CefString>, _name:Option<&CefString>, _disposition:WindowOpenDisposition,
            _gesture:i32, _features:Option<&PopupFeatures>, _window:Option<&mut WindowInfo>,
            _client:Option<&mut Option<Client>>, _settings:Option<&mut BrowserSettings>,
            _extra:Option<&mut Option<DictionaryValue>>, _no_js:Option<&mut i32>)->i32 {
            // Keep source links in this page instead of creating unmanaged CEF windows.
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

fn open(id: u32, css: String) -> Result<(), String> {
    let created = std::time::Instant::now();
    let state = Arc::new(Mutex::new(State {
        id,
        size: (704, 400),
        preferences_pending: 2,
        created: Some(created),
        ..Default::default()
    }));
    let closed = Arc::new(AtomicBool::new(false));
    let mut client = ClientImpl::new(
        Render::new(state.clone()),
        Load::new(state.clone(), css),
        Life::new(closed.clone()),
        PageDisplay::new(state.clone()),
    );
    let info = WindowInfo::default().set_as_windowless(0);
    // Each page gets a distinct off-the-record context. Cookies, localStorage,
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
    trace_timing("private browser created", created);
    CLOSING.with_borrow_mut(|browsers| {
        browsers.retain(|b| !b.load(Ordering::Acquire));
        browsers.push(closed);
    });
    let mut ready = PreferenceStorage::new(context.clone(), state.clone());
    if context.cookie_manager(Some(&mut ready)).is_none() {
        if let Some(host) = browser.host() {
            host.close_browser(1);
        }
        return Err("Couldn’t initialize Brave Search preferences".into());
    }
    PAGES.with_borrow_mut(|pages| pages.insert(id, Page { browser, state }));
    // SetCookie is asynchronous: never send the first request before both completion
    // callbacks succeed. Fail visibly instead of using defaults.
    post_delayed(Command::PreferencesTimeout(id), Duration::from_secs(5));
    Ok(())
}

fn with_page(id: u32, f: impl FnOnce(&Page)) {
    let page = PAGES.with_borrow(|pages| pages.get(&id).map(|p| (p.browser.clone(), p.state.clone())));
    if let Some((browser, state)) = page {
        f(&Page { browser, state });
    }
}

fn handle(command: Command) {
    match command {
        Command::Open { id, css } => {
            if SHUTTING_DOWN.load(Ordering::Acquire) {
                return;
            }
            if let Err(error) = open(id, css) {
                send_error(id, Some(&error));
            }
        }
        Command::Close(id) => {
            if let Some(page) = PAGES.with_borrow_mut(|pages| pages.remove(&id)) {
                if let Some(h) = page.browser.host() {
                    h.close_browser(1);
                }
            }
        }
        Command::Load { id, url } => with_page(id, |page| {
            if let Some(host) = page.browser.host() {
                host.was_hidden(0);
                host.set_focus(1);
            }
            let mut state = page.state.lock().unwrap();
            state.queued_url = Some(url);
            state.committed = false;
            state.document_ready = false;
            drop(state);
            send(proto::Writer::new(proto::LOADING, id).flag(true).finish());
            post(Command::TryNavigate(id));
        }),
        Command::TryNavigate(id) => with_page(id, |page| {
            let url = {
                let mut s = page.state.lock().unwrap();
                if s.preferences_error.is_some() || s.preferences_pending > 0 {
                    return;
                }
                let url = s.queued_url.take();
                if url.is_some() {
                    s.navigation_started = Some(std::time::Instant::now());
                    if let Some(t) = s.created {
                        trace_timing("navigation ready", t);
                    }
                }
                url
            };
            if let (Some(url), Some(frame)) = (url, page.browser.main_frame()) {
                frame.load_url(Some(&CefString::from(url.as_str())));
            }
        }),
        Command::PreferencesTimeout(id) => with_page(id, |page| {
            let mut s = page.state.lock().unwrap();
            if s.preferences_pending > 0 && s.preferences_error.is_none() {
                s.preferences_error = Some("Brave Search preferences timed out".into());
                send_error(id, s.preferences_error.as_deref());
            }
        }),
        Command::Stop(id) => with_page(id, |page| {
            page.state.lock().unwrap().queued_url = None;
            page.browser.stop_load();
            if let Some(h) = page.browser.host() {
                h.set_focus(0);
                h.was_hidden(1);
            }
        }),
        Command::Resize { id, w, h } => with_page(id, |page| {
            if w <= 0 || h <= 0 {
                return;
            }
            page.state.lock().unwrap().size = (w, h);
            if let Some(host) = page.browser.host() {
                host.was_resized();
            }
        }),
        Command::Visible { id, shown } => with_page(id, |page| {
            if let Some(h) = page.browser.host() {
                h.was_hidden(!shown as i32);
                if shown {
                    h.was_resized();
                } else {
                    h.set_focus(0);
                }
            }
        }),
        Command::Focus { id, focused } => with_page(id, |page| {
            if let Some(h) = page.browser.host() {
                h.set_focus(focused as i32);
            }
        }),
        Command::MouseMove { id, event, leave } => with_page(id, |page| {
            if let Some(h) = page.browser.host() {
                h.send_mouse_move_event(Some(&event), leave as i32);
            }
        }),
        Command::MouseClick { id, event, button, up, count } => with_page(id, |page| {
            if !up {
                page.state.lock().unwrap().last_input = Some(std::time::Instant::now());
            }
            let button = match button {
                proto::BUTTON_MIDDLE => MouseButtonType::MIDDLE,
                proto::BUTTON_RIGHT => MouseButtonType::RIGHT,
                _ => MouseButtonType::LEFT,
            };
            if let Some(h) = page.browser.host() {
                if !up {
                    h.set_focus(1);
                }
                h.send_mouse_click_event(Some(&event), button, up as i32, count);
            }
        }),
        Command::Wheel { id, event, dx, dy } => with_page(id, |page| {
            if let Some(h) = page.browser.host() {
                h.send_mouse_wheel_event(Some(&event), dx, dy);
            }
        }),
        Command::Key { id, event } => with_page(id, |page| {
            if event.type_ == KeyEventType::RAWKEYDOWN {
                page.state.lock().unwrap().last_input = Some(std::time::Instant::now());
            }
            if let Some(h) = page.browser.host() {
                h.send_key_event(Some(&event));
            }
        }),
        Command::Shutdown => {
            SHUTTING_DOWN.store(true, Ordering::Release);
            let pages: Vec<Page> = PAGES.with_borrow_mut(|pages| pages.drain().map(|(_, p)| p).collect());
            for page in pages {
                if let Some(h) = page.browser.host() {
                    h.close_browser(1);
                }
            }
            if all_closed() {
                quit_message_loop();
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Engine

wrap_render_process_handler! {
    struct Preferences;
    impl RenderProcessHandler {
        fn on_context_created(&self, _browser:Option<&mut Browser>, frame:Option<&mut Frame>, context:Option<&mut V8Context>) {
            let (Some(frame),Some(context))=(frame,context) else {return;};
            let url=CefString::from(&frame.url()).to_string();
            if frame.is_main()!=0 && url.starts_with("https://search.brave.com/") {
                // Context creation runs before the document's scripts, so metrics
                // are opted out before Brave initializes its telemetry code.
                // Leave an in-session settings change alone until the next search.
                let css=serde_json::to_string(include_str!("search_filters/brave.css")).unwrap();
                let mut script=include_str!("search_filters/brave.js").replace("__OMNI_BRAVE_CSS__",&css);
                if std::env::var_os("OMNI_SEARCH_DIAGNOSTICS").is_some() {
                    script.push_str(include_str!("search_filters/brave-diagnostics.js"));
                }
                let mut result=None;
                let mut exception=None;
                if context.eval(Some(&CefString::from(script.as_str())),None,0,Some(&mut result),Some(&mut exception))==0 {
                    let message=exception.map(|error|CefString::from(&error.message()).to_string()).unwrap_or_default();
                    eprintln!("Couldn’t initialize Brave Search preferences and style: {message}");
                }
            }
        }
    }
}
wrap_app! {
    pub struct BrowserApp;
    impl App {
        fn on_before_command_line_processing(&self, process_type:Option<&CefString>, command_line:Option<&mut CommandLine>) {
            // Brave's conversation endpoint returned 400 over HTTP/3 in both
            // CEF and Brave controls, while the same requests succeeded over h2.
            // Limit the workaround to Omni's embedded browser network stack.
            let browser_process=process_type.is_none_or(|t|t.to_string().is_empty());
            if let (true,Some(command_line))=(browser_process,command_line) {
                command_line.append_switch(Some(&CefString::from("disable-quic")));
            }
        }
        fn render_process_handler(&self)->Option<RenderProcessHandler> {Some(Preferences::new())}
    }
}

const SCRATCH_PREFIX: &str = "omni-chromium-";

/// Remove scratch directories left by hosts that were killed before cleaning up. They
/// live on tmpfs, so each one is RAM. A directory is in use while some process (one of
/// its Chromium helpers) names it in `--user-data-dir`.
fn remove_stale_scratch(runtime: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(runtime) else { return };
    let candidates: Vec<std::path::PathBuf> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(SCRATCH_PREFIX))
        .map(|e| e.path())
        .collect();
    if candidates.is_empty() {
        return;
    }
    let mut in_use = Vec::new();
    if let Ok(procs) = std::fs::read_dir("/proc") {
        for p in procs.flatten() {
            if let Ok(cmdline) = std::fs::read(p.path().join("cmdline")) {
                for arg in cmdline.split(|&b| b == 0) {
                    if let Some(dir) = arg.strip_prefix(b"--user-data-dir=") {
                        in_use.push(String::from_utf8_lossy(dir).into_owned());
                    }
                }
            }
        }
    }
    for dir in candidates {
        if !in_use.iter().any(|u| std::path::Path::new(u) == dir) {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// `omni-browser --omni-host`: serve the launcher on fd 3 until it closes the socket.
pub fn host_main() -> i32 {
    // If the launcher dies without closing the socket cleanly, don't outlive it.
    // SAFETY: plain prctl with constant arguments.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
    // SAFETY: fd 3 is the socket the launcher passed; nothing else in this process owns it.
    // Move it off 3 (Chromium's own IPC slot in its children) and mark it close-on-exec so
    // no Chromium helper inherits the launcher's socket.
    let stream = unsafe {
        use std::os::fd::FromRawFd;
        let fd = libc::fcntl(3, libc::F_DUPFD_CLOEXEC, 10);
        if fd < 0 {
            eprintln!("omni-browser: no launcher socket on fd 3");
            return 64;
        }
        libc::close(3);
        UnixStream::from_raw_fd(fd)
    };
    let fatal = |message: &str| {
        eprintln!("omni-browser: {message}");
        let mut s = &stream;
        let _ = s.write_all(&proto::Writer::new(proto::FATAL, 0).str(message).finish());
        1
    };
    if api_hash(14600, 0).is_null() {
        return fatal("Installed CEF does not support API 14600");
    }
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) else {
        return fatal("XDG_RUNTIME_DIR is not set");
    };
    let runtime = std::path::PathBuf::from(runtime);
    remove_stale_scratch(&runtime);
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return fatal(&e.to_string()),
    };
    let dir = env!("OMNI_CEF_DIR");
    // CEF needs a root for installation metadata even with off-the-record contexts.
    // Keep it private and temporary; no search profile is created beneath it.
    let root = match tempdir(&runtime) {
        Ok(root) => root,
        Err(e) => return fatal(&format!("Couldn’t create temporary Chromium runtime directory: {e}")),
    };
    let started = std::time::Instant::now();
    let settings = Settings {
        windowless_rendering_enabled: 1,
        resources_dir_path: CefString::from(dir),
        locales_dir_path: CefString::from(format!("{dir}/locales").as_str()),
        root_cache_path: CefString::from(root.to_string_lossy().as_ref()),
        // An empty cache_path selects CEF's in-memory/incognito storage mode.
        cache_path: CefString::default(),
        browser_subprocess_path: CefString::from(exe.to_string_lossy().as_ref()),
        accept_language_list: CefString::from("en-US,en"),
        background_color: 0xff1b1b1f,
        ..Default::default()
    };
    let args = cef::args::Args::new();
    let mut app = BrowserApp::new();
    if initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()) != 1 {
        let _ = std::fs::remove_dir_all(&root);
        return fatal("Chromium initialization failed");
    }
    trace_timing("CEF initialized", started);
    let _ = OUTBOX.set(Outbox::default());
    let (read_half, write_half) = match stream.try_clone() {
        Ok(w) => (stream, w),
        Err(e) => return fatal(&e.to_string()),
    };
    let writer_thread = std::thread::spawn(move || writer(write_half));
    std::thread::spawn(move || reader(read_half));
    run_message_loop();
    shutdown();
    let _ = std::fs::remove_dir_all(&root);
    if let Some(out) = OUTBOX.get() {
        out.state.lock().unwrap().closed = true;
        out.ready.notify_one();
    }
    let _ = writer_thread.join();
    0
}

fn tempdir(runtime: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let seed = std::process::id() as u64 ^ std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    for n in 0..100u64 {
        let path = runtime.join(format!("{SCRATCH_PREFIX}{:x}", seed.wrapping_add(n)));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::ErrorKind::AlreadyExists.into())
}
