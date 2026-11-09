//! Omni's Chromium process. `--omni-host` runs the engine for the launcher (see
//! `chromium.rs`); with Chromium's `--type=` it is a renderer/GPU/utility helper. Kept
//! separate from the resident launcher so the launcher never loads CEF itself.
#[path = "../browser_frame.rs"]
mod browser_frame;
#[path = "../chromium.rs"]
mod chromium;
#[path = "../web_protocol.rs"]
mod web_protocol;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--omni-host") {
        std::process::exit(chromium::host_main());
    }
    if !std::env::args().any(|arg| arg.starts_with("--type=")) {
        eprintln!("omni-browser is an internal Chromium process started by omni");
        std::process::exit(64);
    }
    if cef::api_hash(14600, 0).is_null() {
        std::process::exit(1);
    }
    let args = cef::args::Args::new();
    let mut app = chromium::BrowserApp::new();
    let code = cef::execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    std::process::exit(if code >= 0 { code } else { 1 });
}
