//! CEF renderer/GPU helper, kept separate from the resident launcher's process name.
use cef::*;

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
                let css=serde_json::to_string(include_str!("../search_filters/brave.css")).unwrap();
                let mut script=include_str!("../search_filters/brave.js").replace("__OMNI_BRAVE_CSS__",&css);
                if std::env::var_os("OMNI_SEARCH_DIAGNOSTICS").is_some() {
                    script.push_str(include_str!("../search_filters/brave-diagnostics.js"));
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
    struct BrowserApp;
    impl App {
        fn render_process_handler(&self)->Option<RenderProcessHandler> {Some(Preferences::new())}
    }
}

fn main() {
    if !std::env::args().any(|arg| arg.starts_with("--type=")) {
        eprintln!("omni-browser is an internal Chromium subprocess helper");
        std::process::exit(64);
    }
    if cef::api_hash(14600, 0).is_null() {
        std::process::exit(1);
    }
    let args = cef::args::Args::new();
    let mut app = BrowserApp::new();
    let code = cef::execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    std::process::exit(if code >= 0 { code } else { 1 });
}
