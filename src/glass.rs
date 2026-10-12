//! The liquid-glass card: draws a lightly blurred copy of the screen behind it,
//! with a magnified rim to fake light bending at the glass edge.
//!
//! Hyprland's blur is global and far too strong (text behind turns into a flat wash),
//! so omni captures the area behind the card with `grim` just before showing and
//! blurs it itself. The capture is static while the popup is open.

use std::cell::{Cell, RefCell};
use std::process::Command;
use std::time::Duration;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

/// Must match `border-radius` of `.omni-card` / `.omni-glass` in style.css.
pub const RADIUS: f32 = 32.0;
/// Blur radius for the background, in logical px. Small enough that text stays recognisable.
const BLUR: f64 = 9.0;
/// Width of the refracting rim and how much it magnifies.
const RIM: f32 = 9.0;
const RIM_ZOOM: f32 = 1.07;
/// Extra area captured around the card so the blur doesn't fade out at its edges.
pub const BLEED: i32 = 32;
/// White text needs the backdrop (after the CSS tint) to be about this dark at most.
const MAX_LUMA: f32 = 0.30;
/// Never dim more than this: past it the card stops reading as glass.
const MAX_DIM: f32 = 0.72;
/// `.omni-glass`'s own black tint (style.css), already darkening the backdrop.
const CSS_TINT: f32 = 0.18;

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
            if let Some(tex) = self.texture.borrow().as_ref() {
                let w = self.obj().width() as f32;
                let h = self.obj().height() as f32;
                let (ox, oy) = self.offset.get();
                // Texture is in device pixels; draw it at logical size.
                let scale = self.obj().scale_factor() as f32;
                let tex_rect = graphene::Rect::new(
                    ox,
                    oy,
                    tex.width() as f32 / scale,
                    tex.height() as f32 / scale,
                );
                let card = graphene::Rect::new(0.0, 0.0, w, h);

                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(card, RADIUS));

                // Rim: the background slightly magnified around the card's centre,
                // so things behind the edge appear bent, like thick curved glass.
                snapshot.push_blur(BLUR);
                snapshot.save();
                snapshot.translate(&graphene::Point::new(w / 2.0, h / 2.0));
                snapshot.scale(RIM_ZOOM, RIM_ZOOM);
                snapshot.translate(&graphene::Point::new(-w / 2.0, -h / 2.0));
                snapshot.append_texture(tex, &tex_rect);
                snapshot.restore();
                snapshot.pop();

                // Body: the background in place, inside the rim. The hard line where they
                // meet is deliberate (a feathered edge was tried and looked worse).
                let inner = graphene::Rect::new(RIM, RIM, w - 2.0 * RIM, h - 2.0 * RIM);
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(inner, RADIUS - RIM));
                snapshot.push_blur(BLUR);
                snapshot.append_texture(tex, &tex_rect);
                snapshot.pop();
                snapshot.pop();

                // Bright backdrop (a white app behind): darken it, the rim included.
                let dim = self.dim.get();
                if dim > 0.0 {
                    snapshot.append_color(&gdk::RGBA::new(0.0, 0.0, 0.0, dim), &card);
                }

                snapshot.pop();
            }
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

/// How much black the backdrop needs. Judged by its bright parts (85th percentile of
/// luminance), not the average: half a white window behind the card still hides text.
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
    let k = (lumas.len() * 85 / 100).min(lumas.len() - 1);
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
        assert_eq!(dim_needed(0.1), 0.0); // dark wallpaper: clear glass
        assert_eq!(dim_needed(0.3), 0.0);
        let white = dim_needed(1.0);
        assert!(white > 0.6 && white <= MAX_DIM, "{white}");
        assert!(dim_needed(0.6) < white);
    }
}
