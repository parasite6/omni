//! Opaque launcher card shell. Capture hooks remain for show/fit, but the backdrop
//! is not painted — the panel fill is fully opaque CSS (`#1b1b1f`).

use std::cell::{Cell, RefCell};
use std::process::Command;
use std::time::Duration;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib};

/// Must match `--radius-panel` / `.omni-card` border-radius in style.css.
#[allow(dead_code)]
pub const RADIUS: f32 = 10.0;
/// Unused while the panel is fully opaque (no backdrop draw).
#[allow(dead_code)]
const BLUR: f64 = 12.0;
#[allow(dead_code)]
const RIM: f32 = 0.0;
#[allow(dead_code)]
const RIM_ZOOM: f32 = 1.0;
/// Extra area still requested by show/fit code paths.
pub const BLEED: i32 = 32;
#[allow(dead_code)]
const MAX_LUMA: f32 = 0.20;
#[allow(dead_code)]
const MAX_DIM: f32 = 0.85;
/// Fully opaque `.omni-glass` (`#1b1b1f`); no see-through.
const CSS_TINT: f32 = 1.0;
#[allow(dead_code)]
const BRIGHT_PERCENTILE: usize = 97;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct GlassCard {
        pub texture: RefCell<Option<gdk::Texture>>,
        /// Where the captured image's top-left sits, relative to the card.
        pub offset: Cell<(f32, f32)>,
        /// Black laid over the backdrop so white text stays readable on bright apps.
        pub dim: Cell<f32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GlassCard {
        const NAME: &'static str = "OmniGlassCard";
        type Type = super::GlassCard;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for GlassCard {}
    impl BoxImpl for GlassCard {}

    impl WidgetImpl for GlassCard {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            // Fully opaque CSS panel — do not paint the captured backdrop.
            let _ = (
                &self.texture,
                self.offset.get(),
                self.dim.get(),
                BLUR,
                RIM,
                RIM_ZOOM,
            );
            self.parent_snapshot(snapshot);
        }
    }
}

glib::wrapper! {
    pub struct GlassCard(ObjectSubclass<imp::GlassCard>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl GlassCard {
    pub fn new() -> Self {
        let card: Self = glib::Object::builder().build();
        card.set_orientation(gtk::Orientation::Vertical);
        card
    }

    /// Capture the screen area `(x, y, w, h)` (global logical coordinates, including bleed)
    /// that will sit behind the card. `offset` is where that area's top-left lands relative
    /// to the card. On failure the card just shows its CSS tint.
    pub fn capture(&self, x: i32, y: i32, w: i32, h: i32, offset: (f32, f32)) {
        let geometry = format!("{x},{y} {w}x{h}");
        // Time-limited: a hung grim would otherwise freeze omni for good.
        let texture = crate::util::output_with_timeout(
            Command::new("grim").args(["-g", &geometry, "-t", "png", "-l", "0", "-"]),
            Duration::from_millis(500),
        )
        .and_then(|png| gdk::Texture::from_bytes(&glib::Bytes::from_owned(png)).ok());
        if texture.is_none() {
            eprintln!("omni: couldn't capture the background ({geometry})");
        }
        self.imp().dim.set(texture.as_ref().map_or(0.0, dim_for));
        self.imp().texture.replace(texture);
        self.imp().offset.set(offset);
        self.queue_draw();
    }

    pub fn clear(&self) {
        self.imp().texture.replace(None);
    }
}

/// How much black the backdrop needs. Judged by its bright tail (see BRIGHT_PERCENTILE),
/// not the average: sparse white text on a dark page must still force a dim.
fn dim_for(tex: &gdk::Texture) -> f32 {
    let (w, h) = (tex.width() as usize, tex.height() as usize);
    if w == 0 || h == 0 {
        return 0.0;
    }
    // Cairo ARGB32: B, G, R, A per pixel. grim's capture is opaque.
    let stride = w * 4;
    let mut data = vec![0u8; stride * h];
    tex.download(&mut data, stride);
    let mut lumas: Vec<f32> = Vec::with_capacity((w / 4 + 1) * (h / 4 + 1));
    for y in (0..h).step_by(4) {
        for x in (0..w).step_by(4) {
            let i = y * stride + x * 4;
            let (b, g, r) = (data[i] as f32, data[i + 1] as f32, data[i + 2] as f32);
            lumas.push((0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0);
        }
    }
    let k = (lumas.len() * BRIGHT_PERCENTILE / 100).min(lumas.len() - 1);
    let bright = *lumas.select_nth_unstable_by(k, f32::total_cmp).1;
    dim_needed(bright)
}

/// Black alpha that brings `luma` (seen through the CSS tint) down to MAX_LUMA.
fn dim_needed(luma: f32) -> f32 {
    let seen = luma * (1.0 - CSS_TINT);
    if seen <= MAX_LUMA {
        return 0.0;
    }
    (1.0 - MAX_LUMA / seen).min(MAX_DIM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dims_only_bright_backdrops() {
        // Near-opaque CSS tint already keeps seen luma under MAX_LUMA for any backdrop.
        assert_eq!(dim_needed(0.1), 0.0);
        assert_eq!(dim_needed(1.0), 0.0);
    }
}
