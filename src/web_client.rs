//! The launcher's side of the embedded browser. Chromium runs in `omni-browser
//! --omni-host` (see `chromium.rs`), started on the first search and stopped once no page
//! has been open for [`IDLE_EXIT`]: CEF can't be restarted inside one process, so this is
//! the only way its memory (GPU process, network service, renderers) goes back to zero.
//! Pages arrive here as BGRA frames over a socket and are shown in a GTK texture; input
//! goes back the same way.
use crate::web_protocol as proto;
use gtk::{gdk, gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, BorrowedFd, OwnedFd},
        unix::net::UnixStream,
    },
    rc::{Rc, Weak},
    time::Duration,
};

/// How long the Chromium host stays after the last page closes. A search within this
/// window reuses it; the first one after it pays for starting Chromium again.
pub const IDLE_EXIT: Duration = Duration::from_secs(10);

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
    static PAGES: RefCell<HashMap<u32, Weak<Page>>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u32> = const { Cell::new(1) };
    static IDLE: RefCell<Option<glib::SourceId>> = const { RefCell::new(None) };
}

struct Host {
    stream: UnixStream,
    watch: Option<glib::SourceId>,
    buffer: Vec<u8>,
}

fn trace_timing(event: &str, since: std::time::Instant) {
    let line = format!(
        "omni-search {event}: {:.1} ms",
        since.elapsed().as_secs_f64() * 1000.0
    );
    if std::env::var_os("OMNI_SEARCH_TIMING").is_some() {
        eprintln!("{line}");
    }
    crate::probe::log(&line);
}

fn helper_path() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::env::var_os("OMNI_BROWSER_SUBPROCESS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| exe.with_file_name("omni-browser"))
        .canonicalize()
        .map_err(|e| format!("Chromium helper is missing: {e}"))
}

fn spawn_host() -> Result<Host, String> {
    let helper = helper_path()?;
    let (ours, theirs) = UnixStream::pair().map_err(|e| e.to_string())?;
    let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::NONE);
    // SAFETY: 3 is only the target number in the child; nothing is borrowed here.
    launcher.take_fd(OwnedFd::from(theirs), unsafe { BorrowedFd::borrow_raw(3) });
    let process = launcher
        .spawn(&[helper.as_os_str(), std::ffi::OsStr::new("--omni-host")])
        .map_err(|e| format!("Couldn’t start Chromium: {e}"))?;
    // Reap it whenever it exits, so no zombie is left.
    process.wait_async(None::<&gio::Cancellable>, |_| {});
    let watch = glib_unix::unix_fd_add_local(
        ours.as_raw_fd(),
        glib::IOCondition::IN | glib::IOCondition::HUP | glib::IOCondition::ERR,
        |_, _| receive(),
    );
    Ok(Host {
        stream: ours,
        watch: Some(watch),
        buffer: Vec::new(),
    })
}

/// Send to the host. A dead host is noticed here or by the read watch, whichever is first.
fn send(message: Vec<u8>) {
    let failed = HOST.with_borrow_mut(|host| match host {
        Some(h) => h.stream.write_all(&message).is_err(),
        None => false,
    });
    if failed {
        host_gone("Brave Search stopped unexpectedly", true);
    }
}

/// The host exited or broke the stream: every open page shows the error, and the next
/// search starts a new host.
fn host_gone(error: &str, remove_watch: bool) {
    if let Some(mut host) = HOST.take() {
        if let (true, Some(watch)) = (remove_watch, host.watch.take()) {
            watch.remove();
        }
    }
    let pages: Vec<Rc<Page>> = PAGES.with_borrow(|p| p.values().filter_map(Weak::upgrade).collect());
    for page in pages {
        page.set_error(Some(error.into()));
    }
}

fn receive() -> glib::ControlFlow {
    // A socket read returns at most the socket buffer (~200 KiB), so a frame takes several
    // calls. Reuse one buffer for them instead of allocating per call on the GTK thread.
    thread_local! {
        static CHUNK: RefCell<Vec<u8>> = RefCell::new(vec![0u8; 256 * 1024]);
    }
    CHUNK.with_borrow_mut(|chunk| receive_into(chunk))
}

fn receive_into(chunk: &mut [u8]) -> glib::ControlFlow {
    let read = HOST.with_borrow_mut(|host| host.as_mut().map(|h| h.stream.read(chunk)));
    let n = match read {
        Some(Ok(n)) if n > 0 => n,
        Some(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => return glib::ControlFlow::Continue,
        Some(_) => {
            host_gone("Brave Search stopped unexpectedly", false);
            return glib::ControlFlow::Break;
        }
        None => return glib::ControlFlow::Break,
    };
    // Take the buffer out so page callbacks can send (and so re-enter HOST) safely.
    let Some(mut buffer) = HOST.with_borrow_mut(|h| h.as_mut().map(|h| std::mem::take(&mut h.buffer))) else {
        return glib::ControlFlow::Break;
    };
    buffer.extend_from_slice(&chunk[..n]);
    let mut messages = Vec::new();
    let Ok(used) = proto::parse(&buffer, &mut messages) else {
        host_gone("Brave Search stopped unexpectedly", false);
        return glib::ControlFlow::Break;
    };
    // Of several frames for one page in this batch only the last is shown.
    let mut last_frame: HashMap<u32, usize> = HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        if m.tag == proto::FRAME {
            last_frame.insert(m.id, i);
        }
    }
    let mut fatal = None;
    for (i, mut m) in messages.into_iter().enumerate() {
        if m.tag == proto::FATAL {
            fatal = Some(m.str().unwrap_or_default());
            continue;
        }
        if m.tag == proto::FRAME && last_frame.get(&m.id) != Some(&i) {
            continue;
        }
        let page = PAGES.with_borrow(|p| p.get(&m.id).and_then(Weak::upgrade));
        if let Some(page) = page {
            page.receive(m);
        }
    }
    buffer.drain(..used);
    if let Some(error) = fatal {
        host_gone(&error, false);
        return glib::ControlFlow::Break;
    }
    HOST.with_borrow_mut(|h| {
        if let Some(h) = h {
            h.buffer = buffer;
        }
    });
    glib::ControlFlow::Continue
}

fn cancel_idle_exit() {
    if let Some(source) = IDLE.take() {
        source.remove();
    }
}

fn schedule_idle_exit() {
    cancel_idle_exit();
    let source = glib::timeout_add_local_once(IDLE_EXIT, || {
        IDLE.take();
        if PAGES.with_borrow(|p| p.is_empty()) {
            stop_host();
        }
    });
    IDLE.set(Some(source));
}

/// Close the socket; the host shuts Chromium down and exits on its own.
fn stop_host() {
    if let Some(mut host) = HOST.take() {
        if let Some(watch) = host.watch.take() {
            watch.remove();
        }
        crate::probe::log("omni-search: Chromium host stopped");
    }
}

/// Start the Chromium host ahead of a search. Used by the load-timing diagnostic.
#[allow(dead_code)]
pub fn warm_up() -> Result<(), String> {
    if HOST.with_borrow(Option::is_none) {
        HOST.set(Some(spawn_host()?));
    }
    Ok(())
}

/// The launcher is exiting: let the host go now instead of waiting for the idle timer.
pub fn finish() -> Result<(), String> {
    cancel_idle_exit();
    stop_host();
    Ok(())
}

struct Page {
    id: u32,
    picture: glib::WeakRef<gtk::Picture>,
    status: gtk::Label,
    ready_callback: RefCell<Option<Box<dyn FnOnce()>>>,
    document_ready: Cell<bool>,
    loading: Cell<bool>,
    error: RefCell<Option<String>>,
    created: std::time::Instant,
}

impl Page {
    fn receive(&self, mut m: proto::Message) {
        match m.tag {
            proto::FRAME => {
                let (Some(w), Some(h)) = (m.int(), m.int()) else { return };
                let pixels = m.rest();
                if crate::browser_frame::byte_len(w, h) != Some(pixels.len()) {
                    return;
                }
                if let Some(p) = self.picture.upgrade() {
                    let bytes = glib::Bytes::from(pixels);
                    let texture = gdk::MemoryTexture::new(
                        w,
                        h,
                        gdk::MemoryFormat::B8g8r8a8Premultiplied,
                        &bytes,
                        w as usize * 4,
                    );
                    p.set_paintable(Some(&texture));
                }
                self.check_ready();
            }
            proto::LOADING => {
                if let Some(loading) = m.flag() {
                    self.loading.set(loading);
                    self.update_status();
                }
            }
            proto::ERROR => {
                let error = m.str().filter(|e| !e.is_empty());
                self.set_error(error);
            }
            proto::READY => {
                self.document_ready.set(true);
                self.check_ready();
            }
            proto::COPIED => {
                if let (Some(text), Some(p)) = (m.str(), self.picture.upgrade()) {
                    p.clipboard().set_text(&text);
                }
            }
            _ => {}
        }
    }
    fn set_error(&self, error: Option<String>) {
        let has_error = error.is_some();
        *self.error.borrow_mut() = error;
        self.update_status();
        if has_error {
            self.check_ready();
        }
    }
    fn update_status(&self) {
        let error = self.error.borrow();
        self.status.set_text(error.as_deref().unwrap_or(if self.loading.get() {
            "Loading Brave Search…"
        } else {
            "Brave Search"
        }));
    }
    fn check_ready(&self) {
        let Some(p) = self.picture.upgrade() else { return };
        let shown = self.error.borrow().is_some() || (self.document_ready.get() && p.paintable().is_some());
        if !shown {
            return;
        }
        let callback = self.ready_callback.borrow_mut().take();
        if let Some(callback) = callback {
            trace_timing("page shown", self.created);
            callback();
            p.grab_focus();
        }
    }
}

pub struct Surface {
    pub picture: gtk::Picture,
    page: Rc<Page>,
    tick: Option<glib::SourceId>,
}

impl Surface {
    pub fn new(css: &str, status: &gtk::Label) -> Result<Self, String> {
        let created = std::time::Instant::now();
        cancel_idle_exit();
        if HOST.with_borrow(Option::is_none) {
            HOST.set(Some(spawn_host()?));
            trace_timing("Chromium host started", created);
        }
        let picture = gtk::Picture::builder()
            .hexpand(true)
            .vexpand(true)
            .can_shrink(true)
            .focusable(true)
            .build();
        picture.set_content_fit(gtk::ContentFit::Fill);
        let id = NEXT_ID.get();
        NEXT_ID.set(id.wrapping_add(1).max(1));
        let page = Rc::new(Page {
            id,
            picture: picture.downgrade(),
            status: status.clone(),
            ready_callback: RefCell::new(None),
            document_ready: Cell::new(false),
            loading: Cell::new(false),
            error: RefCell::new(None),
            created,
        });
        PAGES.with_borrow_mut(|p| p.insert(id, Rc::downgrade(&page)));
        send(proto::Writer::new(proto::OPEN, id).str(css).finish());
        let weak = picture.downgrade();
        let size = Cell::new((704, 400));
        let tick = glib::timeout_add_local(Duration::from_millis(16), move || {
            let Some(p) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            // Lay the page out at its final size while it is still hidden behind
            // the loading pane. The parent stack is allocated even when this child
            // isn't, so the page doesn't re-flow (and jump) the moment it is shown.
            let now = if p.is_mapped() {
                (p.width(), p.height())
            } else {
                p.parent().map_or((0, 0), |parent| (parent.width(), parent.height()))
            };
            if now.0 > 0 && now.1 > 0 && now != size.get() {
                size.set(now);
                send(proto::Writer::new(proto::RESIZE, id).int(now.0).int(now.1).finish());
            }
            glib::ControlFlow::Continue
        });
        picture.connect_map(move |_| {
            send(proto::Writer::new(proto::VISIBLE, id).flag(true).finish());
        });
        picture.connect_unmap(move |_| {
            send(proto::Writer::new(proto::VISIBLE, id).flag(false).finish());
        });
        connect_input(&picture, id);
        Ok(Self {
            picture,
            page,
            tick: Some(tick),
        })
    }

    pub fn load(&self, url: &str) {
        self.page.document_ready.set(false);
        self.page.loading.set(true);
        self.page.update_status();
        send(proto::Writer::new(proto::LOAD, self.page.id).str(url).finish());
        self.picture.grab_focus();
    }
    pub fn on_ready(&self, callback: impl FnOnce() + 'static) {
        *self.page.ready_callback.borrow_mut() = Some(Box::new(callback));
        // An early failure (the host couldn't start Chromium) may already be here.
        self.page.check_ready();
    }
    pub fn stop(&self) {
        send(proto::Writer::new(proto::STOP, self.page.id).finish());
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        if let Some(tick) = self.tick.take() {
            tick.remove();
        }
        send(proto::Writer::new(proto::CLOSE, self.page.id).finish());
        let empty = PAGES.with_borrow_mut(|p| {
            p.remove(&self.page.id);
            p.is_empty()
        });
        if empty && HOST.with_borrow(Option::is_some) {
            schedule_idle_exit();
        }
    }
}

fn modifiers(m: gdk::ModifierType) -> i32 {
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

fn mouse(tag: u8, id: u32, x: f64, y: f64, mods: gdk::ModifierType) -> proto::Writer {
    proto::Writer::new(tag, id)
        .int(x as i32)
        .int(y as i32)
        .int(modifiers(mods))
}

fn connect_input(picture: &gtk::Picture, id: u32) {
    let pointer = Rc::new(Cell::new((0.0, 0.0)));
    let position = pointer.clone();
    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion(move |controller, x, y| {
        position.set((x, y));
        send(
            mouse(proto::MOUSE_MOVE, id, x, y, controller.current_event_state())
                .flag(false)
                .finish(),
        );
    });
    motion.connect_leave(move |_| {
        send(
            mouse(proto::MOUSE_MOVE, id, 0.0, 0.0, gdk::ModifierType::empty())
                .flag(true)
                .finish(),
        );
    });
    picture.add_controller(motion);
    let click = gtk::GestureClick::new();
    click.set_button(0);
    let weak = picture.downgrade();
    click.connect_pressed(move |g, n, x, y| {
        if let Some(p) = weak.upgrade() {
            p.grab_focus();
        }
        send(
            mouse(proto::MOUSE_CLICK, id, x, y, g.current_event_state())
                .int(mouse_button(g.current_button()))
                .flag(false)
                .int(n)
                .finish(),
        );
    });
    click.connect_released(move |g, n, x, y| {
        send(
            mouse(proto::MOUSE_CLICK, id, x, y, g.current_event_state())
                .int(mouse_button(g.current_button()))
                .flag(true)
                .int(n)
                .finish(),
        );
    });
    picture.add_controller(click);
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    scroll.connect_scroll(move |c, dx, dy| {
        let (x, y) = pointer.get();
        send(
            mouse(proto::WHEEL, id, x, y, c.current_event_state())
                .int((-dx * 48.) as i32)
                .int((-dy * 48.) as i32)
                .finish(),
        );
        glib::Propagation::Stop
    });
    picture.add_controller(scroll);
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(move |_, key, code, mods| {
        if key == gdk::Key::Escape {
            return glib::Propagation::Proceed;
        }
        send(key_event(id, key, code, mods, proto::KEY_RAWDOWN, 0));
        if !mods.intersects(
            gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK,
        ) {
            if let Some(ch) = key.to_unicode() {
                for unit in ch.encode_utf16(&mut [0; 2]) {
                    send(key_event(id, key, code, mods, proto::KEY_CHAR, *unit));
                }
            }
        }
        glib::Propagation::Stop
    });
    keys.connect_key_released(move |_, key, code, mods| {
        send(key_event(id, key, code, mods, proto::KEY_UP, 0));
    });
    picture.add_controller(keys);
}

fn mouse_button(button: u32) -> i32 {
    match button {
        2 => proto::BUTTON_MIDDLE,
        3 => proto::BUTTON_RIGHT,
        _ => proto::BUTTON_LEFT,
    }
}

fn key_event(id: u32, key: gdk::Key, code: u32, mods: gdk::ModifierType, kind: i32, character: u16) -> Vec<u8> {
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
    proto::Writer::new(proto::KEY, id)
        .int(kind)
        .int(virtual_key)
        .int(code as i32)
        .int(modifiers(mods))
        .int(character as i32)
        .finish()
}
