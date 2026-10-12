//! systemd services: find them by name ("bluetooth" → bluetooth.service) and start, stop,
//! restart, enable or disable them.
//!
//! The list (system and user services, with state) comes from `systemctl -o json`, read in
//! the background each time the popup opens (it takes ~1.5 s, too slow for a keystroke).
//! Actions run `systemctl` too: system services ask for the admin password through the
//! desktop's polkit agent (omni never sees it), user services need none. omni has already
//! closed by then, so the result is reported with a notification.

use std::time::Duration;

use gtk::{gio, glib};

#[derive(Clone, Debug)]
pub struct Service {
    /// "bluetooth.service"
    pub unit: String,
    /// A user service (`systemctl --user`), not a system one.
    pub user: bool,
    pub description: String,
    /// "active", "inactive", "failed", ... (empty if not loaded)
    pub active: String,
    /// "enabled", "disabled", "static", ...
    pub enabled: String,
}

impl Service {
    /// "bluetooth" (the unit name without ".service").
    pub fn name(&self) -> &str {
        self.unit.strip_suffix(".service").unwrap_or(&self.unit)
    }

    /// "System service · running · enabled"
    pub fn summary(&self) -> String {
        let kind = if self.user { "User service" } else { "System service" };
        let state = match self.active.as_str() {
            "active" => "running",
            "inactive" | "" => "stopped",
            other => other,
        };
        let mut s = format!("{kind} \u{b7} {state}");
        if !self.enabled.is_empty() {
            s.push_str(&format!(" \u{b7} {}", self.enabled));
        }
        s
    }

    /// Stopping, restarting or disabling this could break the session (display, input,
    /// network, login, auth, sound, remote access): ask first.
    pub fn critical(&self) -> bool {
        // Exact names, or prefixes ending in '@' or '-' (template instances, families).
        const CRITICAL: [&str; 22] = [
            "display-manager", "gdm", "sddm", "lightdm", "greetd", "dbus", "dbus-broker",
            "systemd-", "polkit", "NetworkManager", "sshd", "wpa_supplicant", "pipewire",
            "pipewire-pulse", "wireplumber", "user@", "user-runtime-dir@", "wayland-wm@",
            "wayland-session-", "xdg-desktop-portal", "at-spi-dbus-bus", "graphical-session",
        ];
        let name = self.name();
        CRITICAL
            .iter()
            .any(|c| name == *c || ((c.ends_with('@') || c.ends_with('-')) && name.starts_with(c)) || (*c == "xdg-desktop-portal" && name.starts_with(c)))
    }
}

/// What can be done to a service.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Verb {
    Start,
    Stop,
    Restart,
    Enable,
    Disable,
}

impl Verb {
    pub const ALL: [Verb; 5] = [Verb::Start, Verb::Stop, Verb::Restart, Verb::Enable, Verb::Disable];

    pub fn label(self) -> &'static str {
        match self {
            Verb::Start => "Start",
            Verb::Stop => "Stop",
            Verb::Restart => "Restart",
            Verb::Enable => "Enable",
            Verb::Disable => "Disable",
        }
    }

    fn command(self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Restart => "restart",
            Verb::Enable => "enable",
            Verb::Disable => "disable",
        }
    }

    /// Makes sense for a service in this state (no "Start" for a running one, ...).
    pub fn applies(self, s: &Service) -> bool {
        let running = s.active == "active";
        match self {
            Verb::Start => !running,
            Verb::Stop => running,
            Verb::Restart => running || s.active == "failed",
            // static / indirect / generated / alias units can't be enabled or disabled.
            Verb::Enable => s.enabled == "disabled",
            Verb::Disable => s.enabled == "enabled",
        }
    }

    /// Could cut the user off if done to a critical service.
    pub fn risky(self) -> bool {
        matches!(self, Verb::Stop | Verb::Restart | Verb::Disable)
    }
}

/// All services, system then user. Blocking (~1.5 s): call off the UI thread.
pub fn load() -> Vec<Service> {
    let mut out = list(false);
    out.extend(list(true));
    out
}

fn list(user: bool) -> Vec<Service> {
    let json = |args: &[&str]| -> Vec<serde_json::Value> {
        let mut cmd = std::process::Command::new("systemctl");
        if user {
            cmd.arg("--user");
        }
        cmd.args(args).args(["--type=service", "--no-pager", "-o", "json"]);
        crate::util::output_with_timeout(&mut cmd, Duration::from_secs(5))
            .and_then(|o| serde_json::from_slice(&o).ok())
            .unwrap_or_default()
    };
    let files = json(&["list-unit-files"]);
    let units = json(&["list-units", "--all"]);
    let mut services: Vec<Service> = files
        .iter()
        .filter_map(|f| {
            let unit = f["unit_file"].as_str()?;
            // Templates (getty@.service) aren't services by themselves.
            (unit.ends_with(".service") && !unit.ends_with("@.service")).then(|| Service {
                unit: unit.to_string(),
                user,
                description: String::new(),
                active: String::new(),
                enabled: f["state"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect();
    for u in &units {
        let Some(unit) = u["unit"].as_str() else { continue };
        let description = u["description"].as_str().unwrap_or_default().to_string();
        let active = u["active"].as_str().unwrap_or_default().to_string();
        match services.iter_mut().find(|s| s.unit == unit) {
            Some(s) => {
                s.description = description;
                s.active = active;
            }
            // Loaded but without a unit file of its own (e.g. a template instance).
            None if u["load"] == "loaded" => services.push(Service { unit: unit.to_string(), user, description, active, enabled: String::new() }),
            None => {}
        }
    }
    services
}

/// Services whose name (or description) matches every word of `query`, best first.
/// Needs 3+ letters: shorter queries would list services for nearly anything typed.
pub fn search<'a>(services: &'a [Service], query: &str, max: usize) -> Vec<&'a Service> {
    let q = query.trim().to_lowercase();
    if q.chars().count() < 3 {
        return Vec::new();
    }
    let words: Vec<&str> = q.split_whitespace().collect();
    let mut hits: Vec<(u32, &Service)> = services
        .iter()
        .filter_map(|s| {
            let name = s.name().to_lowercase();
            let desc = s.description.to_lowercase();
            let mut score = 0;
            for w in &words {
                score += if name == *w {
                    100
                } else if name.starts_with(w) {
                    70
                } else if name.split(['-', '_', '.', '@']).any(|p| p.starts_with(w)) {
                    50
                } else if desc.split_whitespace().any(|d| d.starts_with(w)) {
                    20
                } else {
                    return None;
                };
            }
            // Running services and system ones first on ties.
            score += u32::from(s.active == "active") * 5 + u32::from(!s.user) * 2;
            Some((score, s))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.unit.len().cmp(&b.1.unit.len())));
    hits.truncate(max);
    hits.into_iter().map(|(_, s)| s).collect()
}

/// Run `systemctl [--user] VERB UNIT` in the background and report how it went in a
/// notification. System services prompt for the admin password via polkit.
pub fn run(service: &Service, verb: Verb) {
    let mut argv = vec!["systemctl"];
    if service.user {
        argv.push("--user");
    }
    argv.push(verb.command());
    // "--": a unit name can never be read as an option.
    argv.extend(["--", service.unit.as_str()]);
    let what = format!("{} {}", verb.label(), service.unit);
    let (unit, user) = (service.unit.clone(), service.user);
    let proc = match gio::Subprocess::newv(
        &argv.iter().map(std::ffi::OsStr::new).collect::<Vec<_>>(),
        gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_PIPE,
    ) {
        Ok(p) => p,
        Err(e) => return notify(&format!("Couldn't {}", what.to_lowercase()), &e.to_string()),
    };
    glib::spawn_future_local(async move {
        let result = proc.communicate_utf8_future(None).await;
        let ok = proc.is_successful();
        let err = result.ok().and_then(|(_, e)| e).map(|e| e.trim().to_string()).unwrap_or_default();
        if !ok {
            notify(&format!("{what}: failed"), if err.is_empty() { "systemctl reported an error" } else { &err });
        } else if matches!(verb, Verb::Stop | Verb::Disable) {
            // systemctl succeeding doesn't mean it stays that way: a service another
            // program can request (over D-Bus, a socket, ...) is started straight back up.
            // Look again 5 s later and report what's actually true.
            glib::timeout_future(Duration::from_secs(5)).await;
            let (title, body) = if verb == Verb::Stop {
                if is_active(&unit, user).await {
                    (format!("{what}: restarted"), format!("It stopped, but was started again: {}.", restarter(&unit, user).await))
                } else {
                    (format!("{what}: stopped"), String::new())
                }
            } else if unit_property(&unit, user, "UnitFileState").await == "enabled" {
                (format!("{what}: enabled again"), "Something re-enabled it after it was disabled.".to_string())
            } else if is_active(&unit, user).await {
                (format!("{what}: disabled"), "It won't start at boot. It's still running until you stop it.".to_string())
            } else {
                (format!("{what}: disabled"), String::new())
            };
            notify(&title, &body);
        } else {
            notify(&format!("{what}: done"), "");
        }
    });
}

/// `systemctl [--user] status UNIT` output (with the last journal lines), or why it
/// couldn't be read. Exit code 3 just means "not running": its output is still the status.
pub async fn status(service: &Service) -> String {
    let mut argv = vec!["systemctl"];
    if service.user {
        argv.push("--user");
    }
    argv.extend(["status", "--no-pager", "--lines=15", "--full", "--", service.unit.as_str()]);
    let proc = match gio::Subprocess::newv(
        &argv.iter().map(std::ffi::OsStr::new).collect::<Vec<_>>(),
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE,
    ) {
        Ok(p) => p,
        Err(e) => return format!("Couldn't run systemctl: {e}"),
    };
    // Never hang the panel on a stuck systemctl.
    let timeout = glib::timeout_future(Duration::from_secs(5));
    let output = proc.communicate_utf8_future(None);
    finish_status(output, timeout, &proc).await
}

async fn finish_status(
    output: impl std::future::Future<Output = Result<(Option<glib::GString>, Option<glib::GString>), glib::Error>>,
    timeout: impl std::future::Future<Output = ()>,
    proc: &gio::Subprocess,
) -> String {
    let output = std::pin::pin!(output);
    let timeout = std::pin::pin!(timeout);
    match first_of(output, timeout).await {
        Some(Ok((out, _))) => {
            let out = out.map(|s| s.trim_end().to_string()).unwrap_or_default();
            if out.is_empty() { "No status output.".to_string() } else { out }
        }
        Some(Err(e)) => format!("Couldn't read the status: {e}"),
        None => {
            proc.force_exit();
            "systemctl didn't answer within 5 seconds.".to_string()
        }
    }
}

/// The first of two futures to finish: Some(output), or None if the timeout won.
async fn first_of<T>(
    mut a: std::pin::Pin<&mut impl std::future::Future<Output = T>>,
    mut b: std::pin::Pin<&mut impl std::future::Future<Output = ()>>,
) -> Option<T> {
    std::future::poll_fn(|cx| {
        if let std::task::Poll::Ready(v) = a.as_mut().poll(cx) {
            return std::task::Poll::Ready(Some(v));
        }
        if b.as_mut().poll(cx).is_ready() {
            return std::task::Poll::Ready(None);
        }
        std::task::Poll::Pending
    })
    .await
}

/// Whether `systemctl [--user] is-active UNIT` says it's running (false if that can't run).
async fn is_active(unit: &str, user: bool) -> bool {
    let mut argv = vec!["systemctl"];
    if user {
        argv.push("--user");
    }
    argv.extend(["is-active", "--quiet", "--", unit]);
    let Ok(proc) = gio::Subprocess::newv(
        &argv.iter().map(std::ffi::OsStr::new).collect::<Vec<_>>(),
        gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_SILENCE,
    ) else {
        return false;
    };
    proc.wait_future().await.is_ok() && proc.is_successful()
}

/// One `systemctl show` property's value ("" if it can't be read).
async fn unit_property(unit: &str, user: bool, property: &str) -> String {
    let mut argv = vec!["systemctl"];
    if user {
        argv.push("--user");
    }
    let prop = format!("--property={property}");
    argv.extend(["show", "--value", prop.as_str(), "--", unit]);
    let Ok(proc) = gio::Subprocess::newv(
        &argv.iter().map(std::ffi::OsStr::new).collect::<Vec<_>>(),
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    ) else {
        return String::new();
    };
    match proc.communicate_utf8_future(None).await {
        Ok((Some(out), _)) => out.trim().to_string(),
        _ => String::new(),
    }
}

/// What can start the unit again on its own, as far as systemd knows. Which program
/// made the request isn't recorded anywhere, so this names the mechanism: the units
/// that trigger it (sockets, timers, paths), or the D-Bus name a program asked for.
async fn restarter(unit: &str, user: bool) -> String {
    let triggers = unit_property(unit, user, "TriggeredBy").await;
    if !triggers.is_empty() {
        return format!("triggered by {}", triggers.split_whitespace().collect::<Vec<_>>().join(", "));
    }
    let bus = unit_property(unit, user, "BusName").await;
    if !bus.is_empty() {
        return format!("a program asked for it over D-Bus ({bus})");
    }
    "something started it (systemd doesn't record what)".to_string()
}

fn notify(title: &str, body: &str) {
    // notify-send parses these as Pango markup; systemctl's stderr is not markup.
    let title = glib::markup_escape_text(title);
    let body = glib::markup_escape_text(body);
    let _ = gio::Subprocess::newv(
        &["notify-send", "-a", "omni", "-i", "system-run", title.as_str(), body.as_str()].map(std::ffi::OsStr::new),
        gio::SubprocessFlags::NONE,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(unit: &str, desc: &str, active: &str, enabled: &str) -> Service {
        Service { unit: unit.into(), user: false, description: desc.into(), active: active.into(), enabled: enabled.into() }
    }

    #[test]
    fn finds_by_name_then_description() {
        let all = vec![
            svc("bluetooth.service", "Bluetooth service", "active", "enabled"),
            svc("obex.service", "Bluetooth OBEX service", "inactive", "static"),
            svc("cups.service", "CUPS Scheduler", "active", "enabled"),
        ];
        let names: Vec<&str> = search(&all, "bluetooth", 5).iter().map(|s| s.unit.as_str()).collect();
        assert_eq!(names, ["bluetooth.service", "obex.service"]);
        assert!(search(&all, "bl", 5).is_empty()); // too short
        assert!(search(&all, "zzz", 5).is_empty());
    }

    #[test]
    fn verbs_fit_the_state() {
        let running = svc("cups.service", "", "active", "enabled");
        let stopped = svc("cups.service", "", "inactive", "static");
        assert!(Verb::Stop.applies(&running) && !Verb::Start.applies(&running) && Verb::Disable.applies(&running));
        assert!(Verb::Start.applies(&stopped) && !Verb::Enable.applies(&stopped)); // static
        assert!(svc("sddm.service", "", "", "").critical() && svc("systemd-resolved.service", "", "", "").critical());
        // The Hyprland session itself (uwsm) and its portals.
        assert!(svc("wayland-wm@hyprland.desktop.service", "", "", "").critical());
        assert!(svc("xdg-desktop-portal-hyprland.service", "", "", "").critical());
        // Stopping this removes /run/user/1000 and kills the session. `user@` does not match it.
        assert!(svc("user-runtime-dir@1000.service", "", "", "").critical());
        assert!(!svc("cups.service", "", "", "").critical());
        assert!(!running.critical());
    }
}
