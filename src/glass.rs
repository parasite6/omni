//! The launcher card and Glass backdrop. Capture once while the surface is hidden.

use std::cell::{Cell, RefCell};
use std::io;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::sync::Once;
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

// Match the Glass card and menu corner token in style.css.
const RADIUS: f32 = 14.0;
/// Filter, settings and model pickers. Right-click menus use CONTEXT_MENU_RADIUS.
const MENU_RADIUS: f32 = 12.0;
const CONTEXT_MENU_RADIUS: f32 = 10.0;
/// Least tint when there is no captured backdrop to blur and dim (the default opacity).
const NO_CAPTURE_MIN_TINT: f32 = 0.85;
/// Right-click menus sit over busy result text, which shows through a thin tint as a
/// mottled texture, so they are near-solid: this share of the card's remaining transparency
/// becomes extra tint (85% card -> ~97% menu, 0% -> 80%) ...
const MENU_EXTRA_TINT: f32 = 0.80;
/// ... and a stronger blur than the card: radius * MENU_BLUR_SCALE + MENU_BLUR_ADD (capped).
const MENU_BLUR_SCALE: f64 = 1.5;
const MENU_BLUR_ADD: f64 = 3.0;
/// Selected-row marker: a short accent pill inside the highlight, left edge.
const MARKER_INSET: f64 = 6.0;
const MARKER_WIDTH: f64 = 3.0;
const MARKER_HEIGHT: f64 = 16.0;
/// Glass accent (#fc4c01), mirrored from style.css --glass-accent.
const ACCENT: (f64, f64, f64) = (252.0 / 255.0, 76.0 / 255.0, 1.0 / 255.0);
const ROW_RADIUS: f64 = 8.0;
pub const TILE_RADIUS: f64 = 6.0;
const SELECTION_DURATION: Duration = Duration::from_millis(130);
const MENU_DURATION: Duration = Duration::from_millis(120);
// Match --glass-card-fill and Look::match_color's CSS accent in style.css.
const CARD_FILL: f32 = 28.0 / 255.0;
const CARD_FILL_BLUE: f32 = 30.0 / 255.0;
/// Default backdrop blur radius (logical px) and the most the Settings slider allows.
pub const DEFAULT_BLUR: u8 = 9;
pub const MAX_BLUR: u8 = 30;
pub const BLEED: i32 = 32;
const MAX_LUMA: f32 = 0.19;
const MAX_DIM: f32 = 0.85;
const BRIGHT_PERCENTILE: usize = 90;
const BLOCK_SIZE: usize = 16;
const SAMPLES_PER_AXIS: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Look {
    #[default]
    Glass,
    Classic,
}

impl Look {
    pub fn save(self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(
            dir.join("look"),
            match self {
                Self::Glass => "glass\n",
                Self::Classic => "classic\n",
            },
        )
    }

    pub fn match_color(self) -> &'static str {
        match self {
            Self::Glass => "#FC4C01",
            Self::Classic => "#7aa2f7",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appearance {
    pub look: Look,
    pub opacity: u8,
    /// Backdrop blur radius in logical px (0 = sharp).
    pub blur: u8,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            look: Look::Glass,
            opacity: 85,
            blur: DEFAULT_BLUR,
        }
    }
}

impl Appearance {
    pub fn load(dir: &Path) -> Self {
        let classic =
            std::fs::read_to_string(dir.join("look")).is_ok_and(|text| text.trim() == "classic");
        let opacity = std::fs::read_to_string(dir.join("opacity"))
            .ok()
            .and_then(|text| text.trim().parse::<u8>().ok())
            .filter(|value| *value <= 100)
            .unwrap_or(Self::default().opacity);
        let blur = std::fs::read_to_string(dir.join("blur"))
            .ok()
            .and_then(|text| text.trim().parse::<u8>().ok())
            .filter(|value| *value <= MAX_BLUR)
            .unwrap_or(Self::default().blur);
        Self {
            look: if classic { Look::Classic } else { Look::Glass },
            opacity,
            blur,
        }
    }
}

pub fn save_blur(dir: &Path, blur: u8) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("blur"), format!("{blur}\n"))
}

pub fn save_opacity(dir: &Path, opacity: u8) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("opacity"), format!("{opacity}\n"))
}

pub(super) struct Backdrop {
    region: (i32, i32, i32, i32),
    texture: gdk::Texture,
    rect: graphene::Rect,
    luma: f32,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct GlassCard {
        pub(super) glass: Cell<bool>,
        pub(super) backdrop: RefCell<Option<Backdrop>>,
        pub(super) opacity: Cell<f32>,
        pub(super) dim: Cell<f32>,
        pub(super) blur: Cell<f64>,
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
            if self.glass.get() {
                let obj = self.obj();
                let card = graphene::Rect::new(0.0, 0.0, obj.width() as f32, obj.height() as f32);
                obj.paint_backdrop(snapshot, &card, RADIUS, Some((0.0, 0.0)), false);
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

    pub fn set_look(&self, look: Look) {
        self.imp().glass.set(look == Look::Glass);
        self.queue_draw();
    }

    /// Backdrop blur radius in logical px; repaints the card (open menus repaint on their next frame).
    pub fn set_blur(&self, blur: u8) {
        self.imp().blur.set(f64::from(blur.min(MAX_BLUR)));
        self.queue_draw();
    }

    pub fn set_tint(&self, opacity: u8) {
        self.imp().opacity.set(tint_alpha(opacity));
        self.update_dim();
    }

    pub fn capture(&self, x: i32, y: i32, w: i32, h: i32, offset: (f32, f32)) {
        debug_assert!(self.imp().glass.get());
        let start = Instant::now();
        let geometry = format!("{x},{y} {w}x{h}");
        let backdrop = crate::util::output_with_timeout(
            Command::new("grim").args(["-g", &geometry, "-t", "png", "-l", "0", "-"]),
            Duration::from_millis(500),
        )
        .and_then(|png| gdk::Texture::from_bytes(&glib::Bytes::from_owned(png)).ok())
        .map(|texture| Backdrop {
            region: (x, y, w, h),
            luma: bright_luma(&texture, w as usize, h as usize),
            texture,
            // grim scales the logical region to device pixels. Map it back to the
            // requested logical rectangle, including fractional-scale rounding.
            rect: graphene::Rect::new(offset.0, offset.1, w as f32, h as f32),
        });
        if backdrop.is_none() {
            log_capture_failure(&geometry);
        }
        crate::probe::log(format!(
            "glass capture {:.1}ms captured={}",
            start.elapsed().as_secs_f64() * 1000.0,
            backdrop.is_some(),
        ));
        self.imp().backdrop.replace(backdrop);
        self.update_dim();
    }

    pub fn has_backdrop(&self) -> bool {
        self.imp().backdrop.borrow().is_some()
    }

    pub fn has_backdrop_for(&self, region: (i32, i32, i32, i32)) -> bool {
        self.imp()
            .backdrop
            .borrow()
            .as_ref()
            .is_some_and(|backdrop| backdrop.region == region)
    }

    pub fn clear(&self) {
        self.imp().backdrop.replace(None);
        self.update_dim();
    }

    fn paint_backdrop(
        &self,
        snapshot: &gtk::Snapshot,
        bounds: &graphene::Rect,
        radius: f32,
        capture_origin: Option<(f32, f32)>,
        menu: bool,
    ) {
        let imp = self.imp();
        let mut opacity = imp.opacity.get();
        let mut blur = imp.blur.get();
        if menu {
            opacity += (1.0 - opacity) * MENU_EXTRA_TINT;
            blur = (blur * MENU_BLUR_SCALE + MENU_BLUR_ADD).min(f64::from(MAX_BLUR));
        }
        snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(*bounds, radius));
        // Without a capture nothing blurs or dims what's behind, so a low opacity would leave
        // text over the raw screen: fall back to at least the default tint.
        let captured = imp.backdrop.borrow().is_some() && capture_origin.is_some();
        if !captured {
            opacity = opacity.max(NO_CAPTURE_MIN_TINT);
        }
        if opacity < 1.0
            && let (Some(backdrop), Some((x, y))) = (imp.backdrop.borrow().as_ref(), capture_origin)
        {
            let rect = backdrop.rect;
            if blur > 0.0 {
                snapshot.push_blur(blur);
            }
            snapshot.append_texture(
                &backdrop.texture,
                &graphene::Rect::new(rect.x() + x, rect.y() + y, rect.width(), rect.height()),
            );
            if blur > 0.0 {
                snapshot.pop();
            }
        }
        if opacity > 0.0 {
            snapshot.append_color(
                &gdk::RGBA::new(CARD_FILL, CARD_FILL, CARD_FILL_BLUE, opacity),
                bounds,
            );
        }
        let dim = imp.dim.get();
        if dim > 0.0 {
            snapshot.append_color(&gdk::RGBA::new(0.0, 0.0, 0.0, dim), bounds);
        }
        snapshot.pop();
    }

    fn update_dim(&self) {
        let imp = self.imp();
        imp.dim.set(
            imp.backdrop
                .borrow()
                .as_ref()
                .map_or(0.0, |backdrop| dim_needed(backdrop.luma, imp.opacity.get())),
        );
        self.queue_draw();
    }
}

mod popover {
    use super::*;

    #[derive(Default)]
    pub struct BackdropPopover {
        pub(super) card: glib::WeakRef<GlassCard>,
        opening: Cell<Option<Instant>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BackdropPopover {
        const NAME: &'static str = "OmniBackdropPopover";
        type Type = super::BackdropPopover;
        type ParentType = gtk::Popover;
    }

    impl ObjectImpl for BackdropPopover {}
    impl PopoverImpl for BackdropPopover {}

    impl WidgetImpl for BackdropPopover {
        fn realize(&self) {
            self.parent_realize();
            let popup = self.obj();
            if let Some(surface) = popup.surface() {
                let weak = popup.downgrade();
                surface.connect_layout(move |_, _, _| {
                    if let Some(popup) = weak.upgrade()
                        && popup
                            .imp()
                            .card
                            .upgrade()
                            .is_some_and(|card| card.imp().glass.get())
                    {
                        // The compositor can flip or slide the popup when its anchor moves.
                        popup.queue_draw();
                    }
                });
            }
        }

        fn map(&self) {
            self.parent_map();
            let popup = self.obj();
            if !popup.settings().is_gtk_enable_animations()
                || !self
                    .card
                    .upgrade()
                    .is_some_and(|card| card.imp().glass.get())
            {
                return;
            }
            self.opening.set(Some(Instant::now()));
            popup.add_tick_callback(|popup, _| {
                let active = popup.settings().is_gtk_enable_animations()
                    && popup
                        .imp()
                        .opening
                        .get()
                        .is_some_and(|start| start.elapsed() < MENU_DURATION);
                popup.queue_draw();
                if active {
                    glib::ControlFlow::Continue
                } else {
                    popup.imp().opening.set(None);
                    glib::ControlFlow::Break
                }
            });
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let popup = self.obj();
            let progress = self
                .opening
                .get()
                .filter(|_| popup.settings().is_gtk_enable_animations())
                .map(|start| ease_out(start.elapsed(), MENU_DURATION));
            if let Some(progress) = progress {
                let centre =
                    graphene::Point::new(popup.width() as f32 / 2.0, popup.height() as f32 / 2.0);
                snapshot.push_opacity(progress as f64);
                snapshot.save();
                snapshot.translate(&centre);
                snapshot.scale(0.98 + 0.02 * progress, 0.98 + 0.02 * progress);
                snapshot.translate(&graphene::Point::new(-centre.x(), -centre.y()));
            }
            if let Some(card) = self.card.upgrade()
                && card.imp().glass.get()
                // GTK places the public child inside the internal CSS contents node.
                && let Some(contents) = popup.child().and_then(|child| child.parent())
                && let Some(bounds) = contents.compute_bounds(&*popup)
            {
                let context = popup.has_css_class("omni-context-menu");
                card.paint_backdrop(
                    snapshot,
                    &bounds,
                    if context { CONTEXT_MENU_RADIUS } else { MENU_RADIUS },
                    super::capture_origin(&popup, &card),
                    context,
                );
            }
            self.parent_snapshot(snapshot);
            if progress.is_some() {
                snapshot.restore();
                snapshot.pop();
            }
        }
    }
}

glib::wrapper! {
    pub struct BackdropPopover(ObjectSubclass<popover::BackdropPopover>)
        @extends gtk::Popover, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Native, gtk::ShortcutManager;
}

pub fn menu_popover(card: &GlassCard) -> gtk::Popover {
    let popup: BackdropPopover = glib::Object::new();
    popup.set_has_arrow(false);
    // Omni dismisses its own menus. Wayland autohide would take over the pointer.
    popup.set_autohide(false);
    popup.set_position(gtk::PositionType::Bottom);
    popup.imp().card.set(Some(card));
    popup.upcast()
}

/// The position of the card's origin in popup-widget coordinates. GDK gives the popup
/// origin relative to its parent surface. Native transforms convert surface to widget units.
fn capture_origin(popup: &BackdropPopover, card: &GlassCard) -> Option<(f32, f32)> {
    let surface = popup.surface()?.dynamic_cast::<gdk::Popup>().ok()?;
    let native = card.native()?;
    if surface.parent()? != native.surface()? {
        return None;
    }
    let card_origin = card.compute_point(&native, &graphene::Point::zero())?;
    let (parent_x, parent_y) = native.surface_transform();
    let (popup_x, popup_y) = popup.surface_transform();
    let x = card_origin.x() - parent_x as f32 - surface.position_x() as f32 + popup_x as f32;
    let y = card_origin.y() - parent_y as f32 - surface.position_y() as f32 + popup_y as f32;
    Some((x, y))
}

pub fn log_capture_failure(reason: &str) {
    static LOGGED: Once = Once::new();
    LOGGED.call_once(|| eprintln!("omni: couldn't capture the Glass backdrop ({reason})"));
}

fn bright_luma(texture: &gdk::Texture, logical_w: usize, logical_h: usize) -> f32 {
    let (w, h) = (texture.width() as usize, texture.height() as usize);
    let stride = w * 4;
    let mut data = vec![0_u8; stride * h];
    texture.download(&mut data, stride);
    block_brightness(w, h, logical_w, logical_h, |x, y| {
        // Cairo ARGB32 stores pixels in native byte order. grim's capture is opaque.
        let offset = y * stride + x * 4;
        let [_, r, g, b] =
            u32::from_ne_bytes(data[offset..offset + 4].try_into().unwrap()).to_be_bytes();
        (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0
    })
}

/// Average perceptual brightness over patches about as wide as the blur footprint.
/// Place a fixed grid in each logical patch, mapping sample centres to device pixels.
fn block_brightness(
    device_w: usize,
    device_h: usize,
    logical_w: usize,
    logical_h: usize,
    luma: impl Fn(usize, usize) -> f32,
) -> f32 {
    let blocks_x = logical_w.div_ceil(BLOCK_SIZE);
    let blocks_y = logical_h.div_ceil(BLOCK_SIZE);
    let mut means = Vec::with_capacity(blocks_x * blocks_y);
    let sample_grid = |block, blocks, device| {
        std::array::from_fn::<_, SAMPLES_PER_AXIS, _>(|sample| {
            ((block * SAMPLES_PER_AXIS + sample) * 2 + 1) * device / (blocks * SAMPLES_PER_AXIS * 2)
        })
    };
    for by in 0..blocks_y {
        let ys = sample_grid(by, blocks_y, device_h);
        for bx in 0..blocks_x {
            let xs = sample_grid(bx, blocks_x, device_w);
            let sum: f32 = ys
                .iter()
                .flat_map(|&y| xs.iter().map(move |&x| (x, y)))
                .map(|(x, y)| luma(x, y))
                .sum();
            means.push(sum / (SAMPLES_PER_AXIS * SAMPLES_PER_AXIS) as f32);
        }
    }
    let rank = (means.len() * BRIGHT_PERCENTILE).div_ceil(100) - 1;
    *means.select_nth_unstable_by(rank, f32::total_cmp).1
}

fn tint_alpha(percent: u8) -> f32 {
    f32::from(percent.min(100)) / 100.0
}

fn dim_needed(luma: f32, opacity: f32) -> f32 {
    let seen = luma * (1.0 - opacity);
    if seen <= MAX_LUMA {
        return 0.0;
    }
    (1.0 - MAX_LUMA / seen).min(MAX_DIM)
}

fn ease_out(elapsed: Duration, duration: Duration) -> f32 {
    let t = (elapsed.as_secs_f32() / duration.as_secs_f32()).min(1.0);
    1.0 - (1.0 - t).powi(3)
}

#[derive(Clone, Copy, PartialEq)]
struct SelectionRect {
    y: f64,
    height: f64,
}

#[derive(Clone, Copy)]
struct Slide {
    from: SelectionRect,
    to: SelectionRect,
    start: Instant,
}

impl Slide {
    /// 0 at the start of a move, 1 once settled (always 1 when nothing moved).
    fn progress(self, now: Instant) -> f64 {
        if self.from == self.to {
            return 1.0;
        }
        f64::from(ease_out(now.saturating_duration_since(self.start), SELECTION_DURATION))
    }

    fn current(self, now: Instant) -> SelectionRect {
        let t = f64::from(ease_out(
            now.saturating_duration_since(self.start),
            SELECTION_DURATION,
        ));
        let mix = |from, to| from + (to - from) * t;
        SelectionRect {
            y: mix(self.from.y, self.to.y),
            height: mix(self.from.height, self.to.height),
        }
    }
}

/// Non-measuring overlay: interpolate bounds and draw the rounded rectangle at its
/// actual size every frame. The corner radius is independent of the row's height.
pub struct Selection {
    area: gtk::DrawingArea,
    slide: Rc<Cell<Option<Slide>>>,
    ticking: Rc<Cell<bool>>,
}

impl Selection {
    pub fn new() -> Self {
        let area = gtk::DrawingArea::builder()
            .can_target(false)
            .visible(false)
            .css_classes(["omni-glass-selection"])
            .build();
        let slide = Rc::new(Cell::<Option<Slide>>::new(None));
        let draw = slide.clone();
        area.set_draw_func(move |widget, cr, w, _| {
            let Some(slide) = draw.get() else { return };
            let rect = slide.current(Instant::now());
            let (x, y, right, bottom) = (0.0, rect.y, f64::from(w), rect.y + rect.height);
            let radius = ROW_RADIUS.min(rect.height / 2.0).min((right - x) / 2.0);
            if radius <= 0.0 {
                return;
            }
            let quarter = std::f64::consts::FRAC_PI_2;
            cr.new_sub_path();
            cr.arc(right - radius, y + radius, radius, -quarter, 0.0);
            cr.arc(right - radius, bottom - radius, radius, 0.0, quarter);
            cr.arc(x + radius, bottom - radius, radius, quarter, 2.0 * quarter);
            cr.arc(x + radius, y + radius, radius, 2.0 * quarter, 3.0 * quarter);
            cr.close_path();
            let color = widget.color();
            cr.set_source_rgba(
                color.red().into(),
                color.green().into(),
                color.blue().into(),
                color.alpha().into(),
            );
            let _ = cr.fill_preserve();
            // Crisp 1px inner edge so the fill reads as a solid surface, not a smudge.
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.06);
            cr.set_line_width(1.0);
            cr.save().ok();
            cr.clip_preserve();
            cr.set_line_width(2.0);
            let _ = cr.stroke();
            cr.restore().ok();
            cr.new_path();
            // Accent marker: grows from 60% to full height as the highlight settles,
            // so it reads as "landing" on the row rather than sliding as a block.
            let settle = slide.progress(Instant::now());
            let marker_h = (MARKER_HEIGHT * (0.6 + 0.4 * settle)).min(rect.height - 2.0 * MARKER_INSET);
            if marker_h > 0.0 {
                let mx = x + MARKER_INSET;
                let my = y + (rect.height - marker_h) / 2.0;
                let r = MARKER_WIDTH / 2.0;
                cr.new_sub_path();
                cr.arc(mx + r, my + r, r, std::f64::consts::PI, 0.0);
                cr.arc(mx + r, my + marker_h - r, r, 0.0, std::f64::consts::PI);
                cr.close_path();
                cr.set_source_rgba(ACCENT.0, ACCENT.1, ACCENT.2, 1.0);
                let _ = cr.fill();
            }
        });
        Self {
            area,
            slide,
            ticking: Rc::new(Cell::new(false)),
        }
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    pub fn clear(&self) {
        self.slide.set(None);
        self.area.queue_draw();
    }

    pub fn place(&self, y: f64, height: f64, animate: bool) {
        let now = Instant::now();
        let to = SelectionRect { y, height };
        let current = self.slide.get();
        if animate && current.is_some_and(|slide| slide.to == to) {
            return;
        }
        let from = current
            .filter(|_| animate && self.area.is_mapped())
            .map_or(to, |slide| slide.current(now));
        self.slide.set(Some(Slide {
            from,
            to,
            start: now,
        }));
        self.area.set_visible(true);
        self.area.queue_draw();
        if from != to && !self.ticking.replace(true) {
            let ticking = self.ticking.clone();
            let state = self.slide.clone();
            self.area.add_tick_callback(move |area, _| {
                let active = state
                    .get()
                    .is_some_and(|slide| slide.start.elapsed() < SELECTION_DURATION);
                area.queue_draw();
                if active && area.settings().is_gtk_enable_animations() {
                    glib::ControlFlow::Continue
                } else {
                    if let Some(mut slide) = state.get() {
                        slide.from = slide.to;
                        state.set(Some(slide));
                    }
                    ticking.set(false);
                    glib::ControlFlow::Break
                }
            });
        }
    }
}

mod fade {
    use super::*;

    #[derive(Default)]
    pub struct ScrollFade {
        pub(super) enabled: Cell<bool>,
        pub(super) adjustment: RefCell<Option<gtk::Adjustment>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ScrollFade {
        const NAME: &'static str = "OmniScrollFade";
        type Type = super::ScrollFade;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for ScrollFade {}
    impl BoxImpl for ScrollFade {}

    impl WidgetImpl for ScrollFade {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let (mut top, mut bottom) = (false, false);
            if self.enabled.get()
                && let Some(adj) = self.adjustment.borrow().as_ref()
            {
                top = adj.value() > adj.lower() + 0.5;
                bottom = adj.value() + adj.page_size() < adj.upper() - 0.5;
            }
            let obj = self.obj();
            let height = obj.height() as f32;
            if !(top || bottom) || height <= 0.0 {
                self.parent_snapshot(snapshot);
                return;
            }
            let edge = (14.0 / height).min(0.5);
            let point = graphene::Point::new(0.0, 0.0);
            let full = gdk::RGBA::WHITE;
            let clear = gdk::RGBA::new(1.0, 1.0, 1.0, 0.0);
            snapshot.push_mask(gsk::MaskMode::Alpha);
            snapshot.append_linear_gradient(
                &graphene::Rect::new(0.0, 0.0, obj.width() as f32, height),
                &point,
                &graphene::Point::new(0.0, height),
                &[
                    gsk::ColorStop::new(0.0, if top { clear } else { full }),
                    gsk::ColorStop::new(edge, full),
                    gsk::ColorStop::new(1.0 - edge, full),
                    gsk::ColorStop::new(1.0, if bottom { clear } else { full }),
                ],
            );
            snapshot.pop();
            self.parent_snapshot(snapshot);
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    pub struct ScrollFade(ObjectSubclass<fade::ScrollFade>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl ScrollFade {
    pub fn new(scroll: &gtk::ScrolledWindow) -> Self {
        let fade: Self = glib::Object::builder().build();
        fade.set_orientation(gtk::Orientation::Vertical);
        fade.append(scroll);
        let adjustment = scroll.vadjustment();
        adjustment.connect_value_changed(glib::clone!(
            #[weak]
            fade,
            move |_| fade.queue_draw()
        ));
        adjustment.connect_changed(glib::clone!(
            #[weak]
            fade,
            move |_| fade.queue_draw()
        ));
        fade.imp().adjustment.replace(Some(adjustment));
        fade
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.imp().enabled.set(enabled);
        self.queue_draw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opacity_is_linear_and_solid_removes_the_need_for_dim() {
        assert_eq!(tint_alpha(0), 0.0);
        assert_eq!(tint_alpha(50), 0.5);
        assert_eq!(tint_alpha(100), 1.0);
        assert_eq!(dim_needed(1.0, tint_alpha(100)), 0.0);
        for percent in 1..=100 {
            assert!(tint_alpha(percent) > tint_alpha(percent - 1));
        }
    }

    #[test]
    fn dark_page_with_white_text_lines_needs_no_extra_dim() {
        for size in [160, 200, 320] {
            let luma = block_brightness(size, size, 160, 160, |x, y| {
                let logical_x = x * 160 / size;
                let logical_y = y * 160 / size;
                if logical_y % 16 < 3 && logical_x % 16 < 14 {
                    1.0
                } else {
                    0.10
                }
            });
            assert!((0.10..=0.30).contains(&luma), "{size}: {luma}");
            assert_eq!(dim_needed(luma, 0.4), 0.0);
        }
    }

    #[test]
    fn solid_white_and_half_white_backdrops_both_drive_dimming() {
        let white = block_brightness(200, 200, 160, 160, |_, _| 1.0);
        let half = block_brightness(200, 200, 160, 160, |x, _| if x < 100 { 1.0 } else { 0.10 });
        assert_eq!(white, 1.0);
        assert_eq!(half, white);
        let dim = dim_needed(half, 0.4);
        assert!(dim > 0.65);
        assert!(half * 0.6 * (1.0 - dim) <= MAX_LUMA + f32::EPSILON);
    }

    #[test]
    fn dim_keeps_white_readable_at_both_opacities() {
        for opacity in [0.0, 0.4] {
            assert_eq!(dim_needed(0.1, opacity), 0.0);
            let dim = dim_needed(1.0, opacity);
            assert!(dim > 0.0 && dim <= MAX_DIM);
            assert!((1.0 - opacity) * (1.0 - dim) <= MAX_LUMA + f32::EPSILON);
        }
    }

    #[test]
    fn dim_increases_with_brightness_and_decreases_with_opacity() {
        for step in 1..=100 {
            let luma = step as f32 / 100.0;
            let previous = (step - 1) as f32 / 100.0;
            assert!(dim_needed(luma, 0.4) >= dim_needed(previous, 0.4));
            assert!(dim_needed(1.0, luma) <= dim_needed(1.0, previous));
        }
    }

    #[test]
    fn settings_round_trip_and_default_on_invalid_files() {
        let dir = std::env::temp_dir().join(format!("omni-look-{}", glib::uuid_string_random()));
        let defaults = Appearance::default();
        assert_eq!(Appearance::load(&dir), defaults);
        for look in [Look::Classic, Look::Glass] {
            look.save(&dir).unwrap();
            save_opacity(&dir, 73).unwrap();
            save_blur(&dir, 14).unwrap();
            assert_eq!(Appearance::load(&dir), Appearance { look, opacity: 73, blur: 14 });
        }
        std::fs::write(dir.join("look"), "unexpected").unwrap();
        for bad in ["garbage", "101", "-1"] {
            std::fs::write(dir.join("opacity"), bad).unwrap();
            std::fs::write(dir.join("blur"), bad).unwrap();
            assert_eq!(Appearance::load(&dir), defaults);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
