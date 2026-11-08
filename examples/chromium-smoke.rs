//! Structural renderer check; does not open Omni or simulate user input.
#[path = "../src/browser_frame.rs"]
mod browser_frame;
#[path = "../src/chromium.rs"]
mod chromium;
use gtk::{gdk, glib, prelude::*};
use std::{cell::Cell, rc::Rc, time::Duration};

fn main() {
    gtk::init().unwrap();
    let status = gtk::Label::new(None);
    let surface = chromium::Surface::new("", &status).unwrap();
    surface.load("data:text/html,<body><script>setTimeout(()=>document.body.style.backgroundColor='rgb(17,34,51)',100)</script></body>");
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
                println!(
                    "Chromium → GTK frame {}x{}; JavaScript-rendered pixel verified",
                    texture.width(),
                    texture.height()
                );
                result.set(true);
                done.quit();
            }
        }
        glib::ControlFlow::Continue
    });
    let done = main_loop.clone();
    let deadline = glib::timeout_add_local_once(Duration::from_secs(10), move || done.quit());
    main_loop.run();
    check.remove();
    if passed.get() {
        deadline.remove();
    }
    surface.stop();
    drop(surface);
    chromium::finish().expect("Chromium orderly shutdown");
    assert!(passed.get(), "Chromium frame did not reach GTK");
}
