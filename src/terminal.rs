//! Launch programs in the user's preferred terminal without shell-interpolating paths.
use gtk::{gio, glib};
use gio::prelude::*;
use std::ffi::OsString;
use std::path::Path;

#[cfg(test)]
const RUN_AND_WAIT: &str = "\"$1\"; status=$?; printf '\\nProcess exited (%s). Press Enter to close…' \"$status\"; IFS= read -r reply; exit \"$status\"";

#[cfg(test)]
fn program_args(path: &Path) -> Vec<OsString> {
    vec!["/bin/sh".into(), "-c".into(), RUN_AND_WAIT.into(), "omni-run".into(), path.as_os_str().to_owned()]
}

struct Terminal {
    info: gio_unix::DesktopAppInfo,
    action: Option<String>,
}

fn terminal_entry(id: &str) -> Option<Terminal> {
    let (id, action) = id.split_once(':').map_or((id, None), |(id, action)| (id, Some(action.to_owned())));
    let info = gio_unix::DesktopAppInfo::new(id)?;
    if let Some(action) = &action {
        if !info.list_actions().iter().any(|a| a.as_str() == action) { return None; }
    }
    Some(Terminal { info, action })
}

fn candidates(lists: &[String], fallback: &[String]) -> Vec<String> {
    let mut candidates = Vec::new();
    let excluded: std::collections::HashSet<_> = lists.iter().flat_map(|s| s.lines())
        .filter_map(|line| line.trim().strip_prefix('-')).map(str::to_owned).collect();
    for id in lists.iter().flat_map(|s| s.lines()).map(str::trim).chain(fallback.iter().map(String::as_str)) {
        if id.is_empty() || id.starts_with(['#', '-']) { continue; }
        let id = id.strip_prefix('+').unwrap_or(id);
        if excluded.contains(id) || excluded.contains(id.split(':').next().unwrap_or(id)) { continue; }
        candidates.push(id.to_owned());
    }
    candidates
}

fn preferred_terminal() -> Result<Terminal, String> {
    let mut names = Vec::new();
    for desktop in std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':').filter(|s| !s.is_empty() && !s.contains('/')) {
        names.push(format!("{}-xdg-terminals.list", desktop.to_lowercase()));
    }
    names.push("xdg-terminals.list".into());
    let mut dirs = vec![glib::user_config_dir()]; dirs.extend(glib::system_config_dirs());
    dirs.push(glib::user_data_dir().join("xdg-terminal-exec"));
    dirs.extend(glib::system_data_dirs().into_iter().map(|dir| dir.join("xdg-terminal-exec")));
    let lists: Vec<_> = dirs.iter().flat_map(|dir| names.iter().filter_map(move |name| std::fs::read_to_string(dir.join(name)).ok())).collect();
    let mut fallback: Vec<String> = ["kitty.desktop", "org.gnome.Terminal.desktop", "org.kde.konsole.desktop", "xfce4-terminal.desktop", "Alacritty.desktop", "foot.desktop"].into_iter().map(str::to_owned).collect();
    let mut discovered: Vec<_> = gio::AppInfo::all().into_iter().filter_map(|info| {
        let desktop = info.downcast_ref::<gio_unix::DesktopAppInfo>()?;
        desktop.categories()?.split(';').any(|c| c == "TerminalEmulator").then(|| info.id().map(|id| id.to_string())).flatten()
    }).collect();
    discovered.sort(); fallback.extend(discovered);
    candidates(&lists, &fallback).iter().find_map(|id| terminal_entry(id)).ok_or_else(|| "No terminal found. Set your preferred terminal in ~/.config/xdg-terminals.list.".into())
}

fn terminal_command(program: Vec<OsString>) -> Result<Vec<OsString>, String> {
    let terminal = preferred_terminal()?;
    terminal_command_for(terminal, program)
}

fn terminal_command_for(terminal: Terminal, program: Vec<OsString>) -> Result<Vec<OsString>, String> {
    let info = terminal.info;
    let exec = if let Some(action) = terminal.action {
        let keyfile = glib::KeyFile::new();
        keyfile.load_from_file(info.filename().ok_or("Terminal has no desktop file")?, glib::KeyFileFlags::NONE).map_err(|e| e.to_string())?;
        keyfile.string(&format!("Desktop Action {action}"), "Exec").map_err(|e| e.to_string())?.to_string()
    } else { info.commandline().ok_or("Terminal has no launch command")?.to_string_lossy().into_owned() };
    let mut args = glib::shell_parse_argv(&exec).map_err(|e| e.to_string())?;
    args.retain(|arg| !matches!(arg.to_str(), Some("%f" | "%F" | "%u" | "%U" | "%i" | "%c" | "%k")));
    if args.is_empty() { return Err("Terminal has an empty launch command".into()); }
    let exec_arg = info.string("X-TerminalArgExec").map(|s| s.to_string()).unwrap_or_else(|| {
        match info.id().as_deref() {
            Some("org.gnome.Terminal.desktop" | "org.gnome.Ptyxis.desktop") => "--",
            Some("xfce4-terminal.desktop") => "-x",
            _ => "-e",
        }.into()
    });
    if !exec_arg.is_empty() { args.extend(glib::shell_parse_argv(&exec_arg).map_err(|e| e.to_string())?); }
    args.extend(program);
    Ok(args)
}

pub fn approved_command(path: &Path, identity: &str, receipt: &Path) -> Result<Vec<OsString>, String> {
    let helper = std::env::current_exe().map_err(|e| e.to_string())?;
    terminal_command(approval_args(path, identity, helper, receipt))
}

fn approval_args(path: &Path, identity: &str, helper: std::path::PathBuf, receipt: &Path) -> Vec<OsString> {
    // A fixed shell wrapper only holds the terminal; all executable data is argv.
    let args = vec!["/bin/sh".into(), "-c".into(),
        "\"$1\" --run-approved \"$2\" \"$3\" \"$4\"; status=$?; printf '\\nProcess exited (%s). Press Enter to close…' \"$status\"; IFS= read -r reply; exit 0".into(),
        "omni-run".into(), helper.into_os_string(), path.as_os_str().to_owned(), identity.into(), receipt.as_os_str().to_owned()];
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "opens a real desktop terminal"]
    fn live_terminal_launch() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::util::private_test_dir("omni-terminal-check");
        let script = dir.join("test ' program.sh");
        std::fs::write(&script, "#!/bin/sh\n[ -t 0 ] && [ -t 1 ] || exit 4\npwd > verified\nprintf 'Omni terminal launch verified.\\n'\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let identity = crate::program::inspect(&script).unwrap().unwrap();
        let helper = std::env::var_os("OMNI_TEST_BINARY").expect("set OMNI_TEST_BINARY to the built omni binary");
        let args = terminal_command(approval_args(&script, identity.encode(), helper.into(), Path::new(""))).unwrap();
        println!("Terminal command: {:?}", args);
        let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::NONE);
        launcher.set_cwd(&dir);
        let _child = launcher.spawn(&args.iter().map(|s| s.as_os_str()).collect::<Vec<_>>()).unwrap();
        let marker = dir.join("verified");
        for _ in 0..100 {
            if marker.exists() { break; }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), dir.to_str().unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exclusions_apply_to_fallback_and_actions_survive() {
        assert_eq!(candidates(&["-kitty.desktop\nother.desktop:New\n".into()], &["kitty.desktop".into(), "extra.desktop".into()]), vec!["other.desktop:New", "extra.desktop"]);
    }

    #[test]
    fn desktop_action_exec_is_used() {
        let dir = crate::util::private_test_dir("omni-terminal-action");
        let file = dir.join("terminal.desktop");
        std::fs::write(&file, "[Desktop Entry]\nType=Application\nName=Test\nExec=/bin/true\nActions=Window;\nX-TerminalArgExec=--\n[Desktop Action Window]\nName=Window\nExec=/bin/echo --new-window\n").unwrap();
        let info = gio_unix::DesktopAppInfo::from_filename(&file).unwrap();
        let args = terminal_command_for(Terminal { info, action: Some("Window".into()) }, vec!["/bin/true".into()]).unwrap();
        assert_eq!(args, vec![OsString::from("/bin/echo"), "--new-window".into(), "--".into(), "/bin/true".into()]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn path_is_a_literal_argument() {
        let path = Path::new("/tmp/a b;'$(touch nope)");
        let args = program_args(path);
        assert_eq!(args.last().unwrap(), path.as_os_str());
        assert!(!args[2].to_string_lossy().contains("touch nope"));
    }

    #[test]
    fn wrapper_runs_in_working_directory_and_preserves_exit_status() {
        let args = program_args(Path::new("/bin/pwd"));
        let output = std::process::Command::new(&args[0]).args(&args[1..])
            .current_dir("/tmp").output().unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).starts_with("/tmp\n"));
        let args = program_args(Path::new("/bin/false"));
        let output = std::process::Command::new(&args[0]).args(&args[1..]).output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stdout).contains("Process exited (1)"));
    }
}
