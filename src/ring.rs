//! The search bar's living rim: cyan and black that flow into each other around the pill,
//! their patches drifting, growing and shrinking (two waves at different speeds and
//! directions mix the colours), and breathing (brightening and dimming), with a soft glow
//! of the same colours.
//!
//! Drawn by the GPU each frame while the popup is open (a tick callback only runs while
//! the widget is on screen), as a stroke of the pill's outline filled with a rotating
//! conic gradient. It sits on top of the entry and never takes clicks.

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

/// Rim line width and glow blur, in logical px.
const WIDTH: f32 = 2.4;
const GLOW_WIDTH: f32 = 4.5;
const GLOW_BLUR: f64 = 7.0;
/// Seconds for the whole pattern to drift once around, and for one breath.
const LAP: f64 = 11.0;
const BREATH: f64 = 4.0;
/// Colour stops around the ring (more = smoother blends).
const STOPS: usize = 48;

const CYAN: (f32, f32, f32) = (0.33, 0.86, 0.93);
const BLACK: (f32, f32, f32) = (0.05, 0.06, 0.07);
const DEEP_BLACK: (f32, f32, f32) = (0.0, 0.0, 0.0);

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct SearchRing;

    #[glib::object_subclass]
    impl ObjectSubclass for SearchRing {
        const NAME: &'static str = "OmniSearchRing";
        type Type = super::SearchRing;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for SearchRing {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_can_target(false);
            obj.set_can_focus(false);
            obj.add_tick_callback(|w, _| {
                w.queue_draw();
                glib::ControlFlow::Continue
            });
        }
    }

    impl WidgetImpl for SearchRing {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (w, h) = (obj.width() as f32, obj.height() as f32);
            if w <= 0.0 || h <= 0.0 {
                return;
            }
            let t = obj.frame_clock().map_or(0, |c| c.frame_time()) as f64 / 1_000_000.0;
            let angle = ((t / LAP).fract() * 360.0) as f32;
            // 0..1, eased in and out like breathing.
            let breath = (0.5 - 0.5 * (t / BREATH * std::f64::consts::TAU).cos()) as f32;

            let bounds = graphene::Rect::new(0.0, 0.0, w, h);
            let center = graphene::Point::new(w / 2.0, h / 2.0);
            // Colour at each point of the ring: how cyan vs black it is comes from two waves
            // travelling at different speeds in opposite directions, so the patches keep
            // merging, splitting and changing size instead of rotating as a fixed pattern.
            let stops = |alpha: f32| -> Vec<gsk::ColorStop> {
                (0..=STOPS)
                    .map(|i| {
                        let p = i as f64 / STOPS as f64;
                        let tau = std::f64::consts::TAU;
                        // Finer waves (3-5 cycles around) that rarely saturate: the ring stays a
                        // shifting blend with only brief pure-cyan or pure-black spots, and leans
                        // dark (mean below half, then curved) so cyan doesn't dominate.
                        let wave = 0.40
                            + 0.20 * (tau * (3.0 * p + t / 5.0)).sin()
                            + 0.16 * (tau * (5.0 * p - t / 3.7) + 1.3).sin()
                            + 0.10 * (tau * (2.0 * p + t / 7.9) + 2.1).sin();
                        let m = (wave.clamp(0.0, 1.0) as f32).powf(1.4); // 1 = cyan, 0 = black
                        let mix = |a: f32, b: f32| b + (a - b) * m;
                        // Black deepens towards the darkest spots.
                        let black = (mix(BLACK.0, DEEP_BLACK.0), mix(BLACK.1, DEEP_BLACK.1), mix(BLACK.2, DEEP_BLACK.2));
                        let (r, g, b) = (mix(CYAN.0, black.0), mix(CYAN.1, black.1), mix(CYAN.2, black.2));
                        let a = 0.75 + 0.25 * m;
                        gsk::ColorStop::new(p as f32, gdk::RGBA::new(r, g, b, a * alpha))
                    })
                    .collect()
            };
            let pill = |inset: f32| {
                let r = graphene::Rect::new(inset, inset, w - 2.0 * inset, h - 2.0 * inset);
                let builder = gsk::PathBuilder::new();
                builder.add_rounded_rect(&gsk::RoundedRect::from_rect(r, (h - 2.0 * inset) / 2.0));
                builder.to_path()
            };

            // Soft glow: a wider, blurred copy of the rim.
            snapshot.push_blur(GLOW_BLUR);
            snapshot.push_stroke(&pill(WIDTH / 2.0), &gsk::Stroke::new(GLOW_WIDTH));
            snapshot.append_conic_gradient(&bounds.inset_r(-12.0, -12.0), &center, angle, &stops(0.25 + 0.35 * breath));
            snapshot.pop();
            snapshot.pop();

            // The rim itself.
            snapshot.push_stroke(&pill(WIDTH / 2.0), &gsk::Stroke::new(WIDTH));
            snapshot.append_conic_gradient(&bounds, &center, angle, &stops(0.60 + 0.40 * breath));
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    pub struct SearchRing(ObjectSubclass<imp::SearchRing>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl SearchRing {
    pub fn new() -> Self {
        glib::Object::new()
    }
}
