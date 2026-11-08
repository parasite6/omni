//! Opt-in responsiveness diagnostics.
//!
//! `OMNI_STALL_LOG=/path/to/log` appends main-loop stall reports, control-socket
//! arrivals and popup stage timings to that file, each prefixed with the wall-clock
//! time in milliseconds so lines can be lined up with `omni-toggle` and Hyprland's
//! event socket. Nothing runs when the variable is unset.
//!
//! `OMNI_STALL_MS` (default 250) sets how often the loop probe ticks.

use std::cell::Cell;
use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gtk::glib;

static LOG: OnceLock<String> = OnceLock::new();

/// Read the log path once, before `scrub_env` can drop it.
///
/// `OMNI_STALL_LOG` wins. Otherwise a `omni-debug` marker in the runtime directory
/// (which lasts until logout) turns logging on for the installed build.
pub fn init() {
    if let Some(path) = std::env::var_os("OMNI_STALL_LOG").filter(|p| !p.is_empty()) {
        let _ = LOG.set(path.to_string_lossy().into_owned());
        return;
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        let dir = std::path::PathBuf::from(dir);
        if dir.join("omni-debug").exists() {
            let _ = LOG.set(dir.join("omni-probe.log").to_string_lossy().into_owned());
        }
    }
}

pub fn wall_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

/// Append one line; a no-op (and no file open) when diagnostics are off.
pub fn log(msg: impl std::fmt::Display) {
    let Some(path) = LOG.get() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {msg}", wall_ms());
    }
}

/// Monotonic milliseconds for the sampler.
fn mono_ms() -> u64 {
    let d = Instant::now();
    // Only differences are used, so the epoch does not matter.
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = *BASE.get_or_init(|| d);
    d.duration_since(base).as_millis() as u64
}

static LAST_TICK_MS: AtomicU64 = AtomicU64::new(0);
static MAIN_TID: AtomicI32 = AtomicI32::new(0);

/// While the main loop is stuck, sample what it is doing: running (CPU) or blocked in
/// a syscall, and on what. A second thread does this so nothing on the frozen thread
/// is needed.
fn start_stall_sampler() {
    let tid = unsafe { libc::gettid() };
    MAIN_TID.store(tid, Ordering::Release);
    LAST_TICK_MS.store(mono_ms(), Ordering::Release);
    std::thread::Builder::new()
        .name("omni-stall-sampler".into())
        .spawn(move || {
            let mut reported = 0u32;
            let mut last_stat = String::new();
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let lag = mono_ms().saturating_sub(LAST_TICK_MS.load(Ordering::Acquire));
                if lag < 400 {
                    reported = 0;
                    last_stat.clear();
                    continue;
                }
                if reported > 40 {
                    continue; // don't fill the log during a suspend
                }
                reported += 1;
                let tid = MAIN_TID.load(Ordering::Acquire);
                let read = |name: &str| {
                    std::fs::read_to_string(format!("/proc/self/task/{tid}/{name}"))
                        .unwrap_or_default()
                        .trim()
                        .to_string()
                };
                let stat = read("stat");
                // fields after the comm: state is field 3; utime/stime are 14/15.
                let rest = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
                let f: Vec<&str> = rest.split_whitespace().collect();
                let state = f.first().copied().unwrap_or("?");
                let ticks: u64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0)
                    + f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
                let syscall = read("syscall");
                let wchan = read("wchan");
                let cpu_delta = last_stat
                    .rsplit_once(')')
                    .map(|(_, r)| {
                        let g: Vec<&str> = r.split_whitespace().collect();
                        let p: u64 = g.get(11).and_then(|v| v.parse().ok()).unwrap_or(0);
                        let q: u64 = g.get(12).and_then(|v| v.parse().ok()).unwrap_or(0);
                        p + q
                    })
                    .map(|prev| ticks.saturating_sub(prev));
                log(format!(
                    "frozen lag={lag}ms state={state} cpu_ticks_total={ticks} cpu_delta={} syscall=[{}] wchan={wchan}",
                    cpu_delta.map_or("-".to_string(), |d| d.to_string()),
                    syscall.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
                ));
                // Once per stall: what every other thread is doing, so the lock holder
                // (or the blocking call) can be identified by name.
                if reported == 1 {
                    dump_threads(tid);
                }
                last_stat = stat;
            }
        })
        .expect("can't start the stall sampler");
}

fn dump_threads(main_tid: i32) {
    let Ok(entries) = std::fs::read_dir("/proc/self/task") else { return };
    for entry in entries.flatten() {
        let Ok(tid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
        if tid == main_tid {
            continue;
        }
        let read = |name: &str| {
            std::fs::read_to_string(format!("/proc/self/task/{tid}/{name}"))
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        let comm = read("comm");
        let stat = read("stat");
        let state = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("").split_whitespace().next().unwrap_or("?").to_string();
        let syscall = read("syscall");
        let sys = syscall.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
        let wchan = read("wchan");
        if state != "S" || sys != "0 0x0" {
            log(format!("  thread {tid:>7} {comm:<20} state={state} sys={sys} wchan={wchan}"));
        }
    }
}

/// Report any gap between main-loop ticks. A blocked GTK thread shows up here as a
/// single long gap at the moment of the interaction.
pub fn start_loop_probe() {
    let Some(path) = LOG.get() else { return };
    let _ = path;
    let interval = std::env::var("OMNI_STALL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v >= 20)
        .unwrap_or(250);
    let t0 = Instant::now();
    let last = Cell::new(Instant::now());
    log(format!("probe start interval={interval}ms"));
    start_stall_sampler();
    glib::timeout_add_local(Duration::from_millis(interval), move || {
        LAST_TICK_MS.store(mono_ms(), Ordering::Release);
        let now = Instant::now();
        let gap = now.duration_since(last.get());
        last.set(now);
        // One interval of slack; anything beyond that is the loop being held up.
        if gap > Duration::from_millis(interval + 100) {
            log(format!(
                "stall uptime={:.0}ms gap={:.0}ms",
                t0.elapsed().as_secs_f64() * 1000.0,
                gap.as_secs_f64() * 1000.0
            ));
        }
        glib::ControlFlow::Continue
    });
}
