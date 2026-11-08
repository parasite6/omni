//! Measures decoding the bundled Brave SVG (a) on the main thread and (b) on a
//! worker thread through the raw GDK entry point. Used to decide whether the
//! launcher can preload the icon off the UI thread.

use gtk::glib;
use gtk::prelude::*;
use gtk::glib::translate::ToGlibPtr;

fn decode(bytes: &glib::Bytes) -> Option<gtk::gdk::Texture> {
    unsafe {
        let mut err = std::ptr::null_mut();
        let ret = gtk::gdk::ffi::gdk_texture_new_from_bytes(bytes.to_glib_none().0, &mut err);
        if ret.is_null() {
            eprintln!("decode failed, error set: {}", !err.is_null());
            return None;
        }
        Some(glib::translate::from_glib_full(ret))
    }
}

fn main() {
    if gtk::init().is_err() {
        eprintln!("gtk init failed");
        return;
    }
    let bytes = glib::Bytes::from_static(include_bytes!("../src/search_filters/brave.svg"));

    let t = std::time::Instant::now();
    match decode(&bytes) {
        Some(tex) => {
            println!("main-thread decode {:?} -> {}x{}", t.elapsed(), tex.width(), tex.height());
            let path = std::env::temp_dir().join("omni-brave-texture.png");
            match tex.save_to_png(&path) {
                Ok(()) => println!("saved {}", path.display()),
                Err(e) => println!("could not save: {e}"),
            }
        }
        None => println!("main-thread decode failed after {:?}", t.elapsed()),
    }

    let worker = bytes.clone();
    let handle = std::thread::spawn(move || {
        let t = std::time::Instant::now();
        let out = decode(&worker).map(|tex| (tex.width(), tex.height()));
        println!("worker decode {:?} -> {out:?}", t.elapsed());
    });
    handle.join().unwrap();

    let worker = bytes.clone();
    let handle = std::thread::spawn(move || {
        let t = std::time::Instant::now();
        let out = decode(&worker).map(|tex| (tex.width(), tex.height()));
        println!("worker decode (again) {:?} -> {out:?}", t.elapsed());
    });
    handle.join().unwrap();
}
