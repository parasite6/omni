// Tiny keybind trigger: tells the resident omni to toggle. No GTK linked, so it
// starts in about a millisecond.
//
// If omni doesn't answer, recovery goes through `omni-start` (ensure-running): that also
// replaces an omni that is hung while still holding its app id, which a plain restart
// couldn't (the new process would just forward to the hung one). Then the toggle is sent.
//
// `omni-toggle --ping`: health check for ensure-running. Exits 0 only if omni answers
// "pong" within a second; never toggles the popup.
use std::io::{self, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

const ROUND_TRIP: Duration = Duration::from_secs(1);

/// `OMNI_TRIGGER_LOG=/path`: append wall-clock timings for the whole keybind path.
fn tlog(stage: impl std::fmt::Display) {
    use std::io::Write;
    // An explicit path, or the runtime-directory marker that also turns on omni's
    // own probe logging (see probe.rs).
    let Some(path) = std::env::var_os("OMNI_TRIGGER_LOG").filter(|p| !p.is_empty()).or_else(|| {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty())?;
        let dir = PathBuf::from(dir);
        dir.join("omni-debug").exists().then(|| dir.join("omni-trigger.log").into_os_string())
    }) else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{now} trigger {stage}");
    }
}

fn main() -> ExitCode {
    tlog("omni-toggle start");
    // Same socket as omni. Unset or empty: stop. /tmp is shared, so it is not a fallback.
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) else {
        return ExitCode::FAILURE;
    };
    let socket = PathBuf::from(dir).join("omni.sock");

    if std::env::args().nth(1).as_deref() == Some("--ping") {
        return if ping(&socket) { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }

    if send_toggle(&socket) {
        tlog("toggle acknowledged");
        return ExitCode::SUCCESS;
    }
    tlog("toggle NOT acknowledged; recovering");

    // omni isn't answering: (re)start it, then toggle it open.
    let here = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    let start = here.as_ref().map(|d| d.join("omni-start"));
    match start.filter(|s| s.exists()) {
        Some(start) => {
            let started = Command::new(&start).status().is_ok_and(|s| s.success());
            if started && wait_for(&socket) && send_toggle(&socket) {
                return ExitCode::SUCCESS;
            }
            notify("omni couldn't start", "See: journalctl --user -t ensure-running");
        }
        None => {
            // Not installed with omni-start (e.g. a dev build): start omni shown directly.
            let exe = here.map(|d| d.join("omni")).unwrap_or_else(|| "omni".into());
            if Command::new(&exe).spawn().is_ok() {
                return ExitCode::SUCCESS;
            }
            notify("omni couldn't start", &exe.display().to_string());
        }
    }
    ExitCode::FAILURE
}

/// One round trip per keypress: omni replies "ok" before toggling, so a reply means it
/// is alive and handling it; no reply within a second means it's stuck or gone.
fn send_toggle(socket: &Path) -> bool {
    request(socket, b"toggle\n", b"ok\n")
}

fn ping(socket: &Path) -> bool {
    request(socket, b"ping\n", b"pong")
}

fn request(socket: &Path, msg: &[u8], expect: &[u8]) -> bool {
    let t = std::time::Instant::now();
    let Ok(mut stream) = connect_timeout(socket, ROUND_TRIP) else {
        tlog("connect failed");
        return false;
    };
    tlog(format!("connected ({:.1}ms)", t.elapsed().as_secs_f64() * 1000.0));
    let timeout = Some(ROUND_TRIP);
    if stream.set_read_timeout(timeout).is_err() || stream.set_write_timeout(timeout).is_err() {
        return false;
    }
    if stream.write_all(msg).is_err() {
        return false;
    }
    let mut reply = vec![0u8; expect.len()];
    let ok = stream.read_exact(&mut reply).is_ok() && reply == expect;
    tlog(format!("reply ok={ok} ({:.1}ms)", t.elapsed().as_secs_f64() * 1000.0));
    ok
}

/// `UnixStream::connect` can block forever on a half-dead peer (zombie omni still
/// "listening"). Nonblocking connect + poll caps that to `timeout`.
fn connect_timeout(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if bytes.len() >= addr.sun_path.len() {
        unsafe { libc::close(fd) };
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "socket path too long"));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr() as *const libc::c_char,
            addr.sun_path.as_mut_ptr(),
            bytes.len(),
        );
    }
    // pathname socket: include the trailing NUL in the address length
    let addr_len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;

    let rc = unsafe {
        libc::connect(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            addr_len,
        )
    };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINPROGRESS) {
            unsafe { libc::close(fd) };
            return Err(err);
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        let pr = unsafe { libc::poll(&mut pfd, 1, ms) };
        if pr == 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::new(io::ErrorKind::TimedOut, "connect timed out"));
        }
        if pr < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        let mut so_error: libc::c_int = 0;
        let mut sol = std::mem::size_of_val(&so_error) as libc::socklen_t;
        let g = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut so_error as *mut _ as *mut libc::c_void,
                &mut sol,
            )
        };
        if g < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        if so_error != 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::from_raw_os_error(so_error));
        }
    }

    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags >= 0 {
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) };
    }
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

/// Wait up to 5 s for a freshly started omni to answer.
fn wait_for(socket: &Path) -> bool {
    for _ in 0..50 {
        if ping(socket) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn notify(title: &str, body: &str) {
    // Don't fail silently: the keybind would just appear dead.
    let _ = Command::new("notify-send").args(["-a", "omni", title, body]).spawn();
}
