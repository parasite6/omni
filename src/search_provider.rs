//! Providers available to the built-in browser. Each will eventually own its page filter.
use std::sync::OnceLock;
use std::time::Instant;

use gtk::glib;
use gtk::prelude::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchProvider {
    #[default]
    Brave,
}

const BRAVE_SVG: &[u8] = include_bytes!("search_filters/brave.svg");
static BRAVE_MARK: OnceLock<Option<gtk::gdk::Texture>> = OnceLock::new();

/// Decode the bundled Brave mark into a texture.
fn decode_mark() -> Option<gtk::gdk::Texture> {
    use glib::translate::{from_glib_full, ToGlibPtr};
    let bytes = glib::Bytes::from_static(BRAVE_SVG);
    // SAFETY: this only builds an in-memory texture and is safe to run on a
    // worker; the safe wrapper's main-thread assertion does not apply here.
    unsafe {
        let mut error = std::ptr::null_mut();
        let texture = gtk::gdk::ffi::gdk_texture_new_from_bytes(bytes.to_glib_none().0, &mut error);
        if texture.is_null() {
            if !error.is_null() {
                let error: glib::Error = from_glib_full(error);
                crate::probe::log(format!("icon preload failed: {error}"));
            }
            return None;
        }
        Some(from_glib_full(texture))
    }
}

/// Decode the Brave mark once, in the background.
///
/// The first decode in a process starts GDK's sandboxed image loader and takes
/// about a second; doing it when the web row first appears froze the GTK thread
/// for that long on the first keystroke. Preloading means the row only ever sees
/// the finished texture.
pub fn start_icon_preload() {
    let _ = std::thread::Builder::new()
        .name("omni-icon".into())
        .spawn(|| {
            let t0 = Instant::now();
            let decoded = decode_mark();
            crate::probe::log(format!(
                "icon preload done ({:.0}ms) ok={}",
                t0.elapsed().as_secs_f64() * 1000.0,
                decoded.is_some()
            ));
            let _ = BRAVE_MARK.set(decoded);
        });
}

/// The preloaded Brave mark, once `start_icon_preload` has finished.
pub fn mark_texture() -> Option<gtk::gdk::Texture> {
    BRAVE_MARK.get().and_then(|mark| mark.clone())
}

/// A `GtkImage` showing the Brave mark, or the generic search icon until the
/// background preload finishes.
pub fn mark_image(pixel_size: i32) -> gtk::Image {
    let image = match mark_texture() {
        Some(texture) => gtk::Image::from_paintable(Some(&texture)),
        None => gtk::Image::from_icon_name("system-search-symbolic"),
    };
    image.set_pixel_size(pixel_size);
    image
}

impl SearchProvider {
    pub const ALL: &[Self] = &[Self::Brave];

    pub fn icon(self) -> gtk::gio::Icon {
        match mark_texture() {
            Some(texture) => texture.upcast(),
            None => gtk::gio::ThemedIcon::new("system-search-symbolic").upcast(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Brave => "Brave",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Brave => "brave",
        }
    }

    pub fn from_id(_id: &str) -> Self {
        // Unknown / removed providers safely fall back to the first supported engine.
        Self::Brave
    }

    pub fn search_url(self, query: &str) -> String {
        format!(
            "https://search.brave.com/ask?q={}",
            crate::web::percent_encode(query)
        )
    }

    pub fn style_sheet(self) -> &'static str {
        match self {
            Self::Brave => include_str!("search_filters/brave.css"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_cannot_inject_parameters_or_a_fragment() {
        assert_eq!(
            SearchProvider::Brave.search_url("QLoRA & sources?#é"),
            "https://search.brave.com/ask?q=QLoRA+%26+sources%3F%23%C3%A9"
        );
    }
}
