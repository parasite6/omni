//! Loading timings without opening Omni or simulating user input.
#[path = "../src/browser_frame.rs"]
mod browser_frame;
#[path = "../src/probe.rs"]
#[allow(dead_code)]
mod probe;
#[path = "../src/web_client.rs"]
mod chromium;
#[path = "../src/web_protocol.rs"]
mod web_protocol;
use gtk::glib;
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::{Duration, Instant},
};

fn local_fixture() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut started = Instant::now();
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = [0; 4096];
            let n = stream.read(&mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..n]);
            let path = request.split_whitespace().nth(1).unwrap_or("/");
            let body = if path == "/" {
                started = Instant::now();
                "<script>(async()=>{for(let i=0;i<30;i++) await fetch('/ping?'+i);await fetch('/done');})();</script>"
            } else {
                if path == "/done" {
                    eprintln!(
                        "LOCAL 30 sequential fetches: {:.1} ms",
                        started.elapsed().as_secs_f64() * 1000.0
                    );
                }
                "ok"
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    origin
}

fn main() {
    gtk::init().unwrap();
    if std::env::args().any(|arg| arg == "--prewarm") {
        chromium::warm_up().unwrap();
        let ready = glib::MainLoop::new(None, false);
        let done = ready.clone();
        glib::timeout_add_local_once(Duration::from_millis(250), move || done.quit());
        ready.run();
    }
    let observe = std::env::args().any(|arg| arg == "--observe");
    let args: Vec<_> = std::env::args().skip(1).collect();
    let runs: usize = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--runs="))
        .map(|value| value.parse().unwrap())
        .unwrap_or(2);
    let wait: Option<u64> = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--wait="))
        .map(|value| value.parse().unwrap());
    assert!((1..=10).contains(&runs));
    assert!(wait.is_none_or(|seconds| (1..=60).contains(&seconds)));
    let arg = args.iter().find(|arg| !arg.starts_with("--")).cloned();
    let local = args.iter().any(|arg| arg == "--local");
    let url = if local {
        local_fixture()
    } else {
        arg.unwrap_or_else(|| "https://search.brave.com/ask?q=How+old+is+the+universe".into())
    };
    for session in 1..=runs {
        eprintln!("SESSION {session}");
        let status = gtk::Label::new(None);
        let surface =
            chromium::Surface::new(include_str!("../src/search_filters/brave.css"), &status)
                .unwrap();
        surface.load(&url);
        let shown = Instant::now();
        surface.on_ready(move || {
            eprintln!("READY {:.1} ms", shown.elapsed().as_secs_f64() * 1000.0)
        });
        let main_loop = glib::MainLoop::new(None, false);
        let done = main_loop.clone();
        glib::timeout_add_local_once(
            Duration::from_secs(wait.unwrap_or(if observe {
                45
            } else if local {
                4
            } else {
                12
            })),
            move || done.quit(),
        );
        main_loop.run();
        eprintln!("Final status: {}", status.text());
        surface.stop();
        drop(surface);
    }
    // --linger=N keeps the launcher side alive with no page open, to watch the
    // Chromium host exit on its idle timer.
    if let Some(seconds) = args.iter().find_map(|arg| arg.strip_prefix("--linger=")) {
        let main_loop = glib::MainLoop::new(None, false);
        let done = main_loop.clone();
        glib::timeout_add_local_once(Duration::from_secs(seconds.parse().unwrap()), move || done.quit());
        main_loop.run();
    }
    chromium::finish().unwrap();
}
