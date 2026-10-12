// Tiny keybind trigger: tells the resident omni to toggle. No GTK linked, so it
// starts in about a millisecond.
//
// If omni doesn't answer, recovery goes through `omni-start` (ensure-running): that also
// replaces an omni that is hung while still holding its app id, which a plain restart
// couldn't (the new process would just forward to the hung one). Then the toggle is sent.
//
// `omni-toggle --ping`: health check for ensure-running. Exits 0 only if omni answers
// "pong" within a second; never toggles the popup.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

fn main() -> ExitCode {
    // Same socket as omni. Unset or empty: stop. /tmp is shared, so it is not a fallback.
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) else {
        return ExitCode::FAILURE;
    };
    let socket = PathBuf::from(dir).join("omni.sock");

    if std::env::args().nth(1).as_deref() == Some("--ping") {
        return if ping(&socket) { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }

    if send_toggle(&socket) {
        return ExitCode::SUCCESS;
    }

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
    let Ok(mut stream) = UnixStream::connect(socket) else { return false };
    let timeout = Some(Duration::from_secs(1));
    if stream.set_read_timeout(timeout).is_err() || stream.set_write_timeout(timeout).is_err() {
        return false;
    }
    if stream.write_all(msg).is_err() {
        return false;
    }
    let mut reply = vec![0u8; expect.len()];
    stream.read_exact(&mut reply).is_ok() && reply == expect
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
