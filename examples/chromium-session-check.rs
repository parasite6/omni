//! Check local session isolation without opening Omni or simulating input.
#[path = "../src/browser_frame.rs"]
mod browser_frame;
#[path = "../src/chromium.rs"]
mod chromium;
use cef::*;
use gtk::{gdk, glib, prelude::*};
use std::{
    cell::Cell,
    io::{Read, Write},
    net::TcpListener,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

// Serve a local fixture at Brave's actual origin, without contacting Brave.
// Its first inline script must see the preferences before any site code runs.
wrap_scheme_handler_factory! {
    struct PreferencePage { seed: bool }
    impl SchemeHandlerFactory {
        fn create(&self, _browser:Option<&mut Browser>, _frame:Option<&mut Frame>,
            _scheme:Option<&CefString>, _request:Option<&mut Request>)->Option<ResourceHandler> {
            let html = format!(r#"<!doctype html><body><script>
                const cookies=Object.fromEntries(document.cookie.split('; ').map(c=>c.split('=')));
                const ok=cookies.safesearch==='off' && cookies.useLocation==='0'
                    && getComputedStyle(document.documentElement).scrollbarWidth==='none'
                    && localStorage.getItem('app.usageMetricsOptOut')==='true'
                    && localStorage.getItem('oldConversation')===null;
                if({}) localStorage.setItem('oldConversation','private');
                document.body.style.backgroundColor=ok?'rgb(17,34,51)':'rgb(255,0,0)';
                </script></body>"#,self.seed);
            Some(PreferenceResource::new(Arc::new(Mutex::new((html.into_bytes(),0)))))
        }
    }
}
wrap_resource_handler! {
    struct PreferenceResource { data: Arc<Mutex<(Vec<u8>,usize)>> }
    impl ResourceHandler {
        fn open(&self, _request:Option<&mut Request>, handle:Option<&mut i32>, _callback:Option<&mut Callback>)->i32 {
            if let Some(handle)=handle {*handle=1;} 1
        }
        fn response_headers(&self, response:Option<&mut Response>, length:Option<&mut i64>, _redirect:Option<&mut CefString>) {
            if let Some(response)=response {response.set_status(200);response.set_mime_type(Some(&CefString::from("text/html")));}
            if let Some(length)=length {*length=self.data.lock().unwrap().0.len() as i64;}
        }
        fn read(&self, out:*mut u8, capacity:i32, read:Option<&mut i32>, _callback:Option<&mut ResourceReadCallback>)->i32 {
            let mut data=self.data.lock().unwrap();
            let count=(data.0.len()-data.1).min(capacity.max(0) as usize);
            if count>0 {
                // SAFETY: CEF provides capacity writable bytes, and count bounds the source.
                unsafe {std::ptr::copy_nonoverlapping(data.0.as_ptr().add(data.1),out,count);}
                data.1+=count;
            }
            if let Some(read)=read {*read=count as i32;}
            i32::from(count>0)
        }
    }
}

fn check_preferences(surface: &chromium::Surface, id: i32, seed: bool) {
    let browser = browser_host_get_browser_by_identifier(id).expect("Fixture browser");
    let context = browser.host().unwrap().request_context().unwrap();
    let mut factory = PreferencePage::new(seed);
    assert_eq!(
        context.register_scheme_handler_factory(
            Some(&CefString::from("https")),
            Some(&CefString::from("search.brave.com")),
            Some(&mut factory)
        ),
        1
    );
    let revealed = Rc::new(Cell::new(false));
    let loaded = revealed.clone();
    surface.on_ready(move || {
        let url = CefString::from(&browser.main_frame().unwrap().url()).to_string();
        assert_eq!(
            url, "https://search.brave.com/ask",
            "Loading animation ended on the initial blank page"
        );
        loaded.set(true);
    });
    surface.load("https://search.brave.com/ask");
    wait_for_page(surface);
    assert!(
        revealed.get(),
        "Loaded page did not end the loading animation"
    );
    println!(
        "Session {id}: Brave preferences present before first page script; no old conversation"
    );
}

fn wait_for_page(surface: &chromium::Surface) {
    let main_loop = glib::MainLoop::new(None, false);
    let passed = Rc::new(Cell::new(false));
    let result = passed.clone();
    let picture = surface.picture.clone();
    let done = main_loop.clone();
    let check = glib::timeout_add_local(Duration::from_millis(50), move || {
        if let Some(texture) = picture.paintable().and_downcast::<gdk::Texture>() {
            let mut data = vec![0; texture.width() as usize * texture.height() as usize * 4];
            texture.download(&mut data, texture.width() as usize * 4);
            if data.starts_with(&[51, 34, 17, 255]) {
                result.set(true);
                done.quit();
            }
            if data.starts_with(&[0, 0, 255, 255]) {
                done.quit();
            }
        }
        glib::ControlFlow::Continue
    });
    let done = main_loop.clone();
    let expired = Rc::new(Cell::new(false));
    let timeout = expired.clone();
    let deadline = glib::timeout_add_local_once(Duration::from_secs(15), move || {
        timeout.set(true);
        done.quit();
    });
    main_loop.run();
    check.remove();
    if !expired.get() {
        deadline.remove();
    }
    assert!(
        passed.get(),
        "Session preferences, storage or follow-up prompt check failed"
    );
}

fn runtime_dirs() -> std::collections::BTreeSet<std::path::PathBuf> {
    std::fs::read_dir(glib::user_runtime_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("omni-chromium-")
        })
        .map(|e| e.path())
        .collect()
}

fn wait_for_storage_report(
    surface: &chromium::Surface,
    reports: &Arc<Mutex<Vec<String>>>,
    seed: bool,
) {
    let index = usize::from(!seed);
    let deadline = Instant::now() + Duration::from_secs(15);
    while reports.lock().unwrap().len() <= index && Instant::now() < deadline {
        while glib::MainContext::default().pending() {
            glib::MainContext::default().iteration(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let reports = reports.lock().unwrap().clone();
    let report = reports.get(index).expect("Storage page did not report");
    let encoded = report
        .split("&state=")
        .nth(1)
        .expect("Storage page reported an error");
    let json = glib::uri_unescape_string(encoded, None::<&str>).unwrap();
    let state: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(state["cookie"], seed);
    assert_eq!(state["preferencesScoped"], true);
    assert_eq!(state["scrollbarHidden"], true);
    assert_eq!(state["customScrollbarHidden"], true);
    assert_eq!(state["scrollable"], true);
    assert_eq!(state["prompt"], true);
    for key in ["local", "session"] {
        assert_eq!(
            state[key],
            if seed {
                serde_json::json!("probe")
            } else {
                serde_json::Value::Null
            }
        );
    }
    for key in ["databases", "caches"] {
        assert_eq!(
            state[key],
            if seed {
                serde_json::json!(["omni-probe"])
            } else {
                serde_json::json!([])
            }
        );
    }
    assert_eq!(state["workers"].as_u64().unwrap() > 0, seed);
    drop(reports);
    wait_for_page(surface);
}

fn main() {
    gtk::init().unwrap();
    let before = runtime_dirs();
    chromium::warm_up().unwrap();
    let reports = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = reports.clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let css = include_str!("../src/search_filters/brave.css");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                break;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut request = vec![0; 16384];
            let Ok(n) = stream.read(&mut request) else {
                continue;
            };
            let request = String::from_utf8_lossy(&request[..n]);
            let path = request
                .lines()
                .next()
                .and_then(|s| s.split_whitespace().nth(1))
                .unwrap_or("/");
            let (kind, body) = if path.starts_with("/report") {
                recorded.lock().unwrap().push(path.to_string());
                ("text/plain", "ok".to_string())
            } else if path == "/worker.js" {
                (
                    "application/javascript",
                    "self.addEventListener('install',()=>self.skipWaiting());".to_string(),
                )
            } else {
                let seed = path.starts_with("/seed");
                (
                    "text/html",
                    format!(
                        r#"<!doctype html><html><head><style>{css}</style></head><body>
                <div class="ask-center-footer"><div class="tap-input-field-wrapper"><textarea>Follow up</textarea></div></div>
                <div id="scroll-probe" style="height:40px;overflow:auto"><div style="height:200px"></div></div>
                <div class="vscroll-track visible"><div class="vscroll-thumb" style="height:100px"></div></div>
                <script>(async()=>{{try{{
                    const seed={seed};
                    if(seed){{
                        document.cookie='omni_probe=present; Max-Age=3600; Path=/';
                        localStorage.setItem('chat','probe');sessionStorage.setItem('chat','probe');
                        await new Promise((resolve,reject)=>{{let r=indexedDB.open('omni-probe',1);r.onsuccess=()=>{{r.result.close();resolve()}};r.onerror=reject;}});
                        await caches.open('omni-probe');
                        await navigator.serviceWorker.register('/worker.js');
                    }}
                    const scroller=document.getElementById('scroll-probe');scroller.scrollTop=30;
                    const state={{cookie:document.cookie.includes('omni_probe=present'),
                        scrollable:scroller.scrollTop===30 && scroller.scrollHeight>scroller.clientHeight,
                        scrollbarHidden: getComputedStyle(document.documentElement).scrollbarWidth==='none',
                        customScrollbarHidden:getComputedStyle(document.querySelector('.vscroll-track')).display==='none',
                        preferencesScoped: !document.cookie.includes('safesearch=') && !document.cookie.includes('useLocation=')
                            && localStorage.getItem('app.usageMetricsOptOut')===null,
                        local:localStorage.getItem('chat'),session:sessionStorage.getItem('chat'),
                        databases:(await indexedDB.databases()).map(d=>d.name),
                        caches:await caches.keys(),workers:(await navigator.serviceWorker.getRegistrations()).length,
                        prompt:getComputedStyle(document.querySelector('.tap-input-field-wrapper')).display!=='none'}};
                    await fetch('/report?stage='+(seed?'seed':'check')+'&state='+encodeURIComponent(JSON.stringify(state)));
                    const ok=seed ? state.cookie&&state.local==='probe'&&state.session==='probe'&&state.databases.includes('omni-probe')&&state.caches.includes('omni-probe')&&state.workers>0
                        : !state.cookie&&state.local===null&&state.session===null&&state.databases.length===0&&state.caches.length===0&&state.workers===0;
                    document.body.style.backgroundColor=ok&&state.prompt&&state.preferencesScoped?'rgb(17,34,51)':'rgb(255,0,0)';
                }}catch(e){{await fetch('/report?error='+encodeURIComponent(String(e)));document.body.style.backgroundColor='rgb(255,0,0)';}}}})();</script></body></html>"#
                    ),
                )
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let status = gtk::Label::new(None);
        {
            let first = chromium::Surface::new(css, &status).unwrap();
            check_preferences(&first, 1, true);
            first.load(&format!("{origin}/seed"));
            wait_for_storage_report(&first, &reports, true);
            first.stop();
        }
        {
            let second = chromium::Surface::new(css, &status).unwrap();
            assert!(
                second.picture.paintable().is_none(),
                "New session has a previous rendered frame"
            );
            check_preferences(&second, 2, false);
            second.load(&format!("{origin}/check"));
            wait_for_storage_report(&second, &reports, false);
            second.stop();
        }
    }));
    for report in reports.lock().unwrap().iter() {
        println!("{report}");
    }
    let start = Instant::now();
    chromium::finish().expect("Orderly shutdown");
    assert_eq!(
        before,
        runtime_dirs(),
        "Temporary runtime metadata was not removed"
    );
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
    println!(
        "Cookies, localStorage, sessionStorage, IndexedDB, caches and service workers did not cross sessions; prompt visible; fresh frame; runtime metadata removed (shutdown {:?})",
        start.elapsed()
    );
}
