//! Cyan Omni artwork beneath a softly tethered, optically magnifying glass.
use std::cell::RefCell;
use std::f64::consts::{PI, TAU};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::cairo::{Context, LinearGradient, RadialGradient};
use gtk::glib;
use gtk::prelude::*;

#[derive(Default)]
struct Motion {
    x: f64,
    y: f64,
    target_x: f64,
    target_y: f64,
    hover: f64,
    target_hover: f64,
    opened: Option<Instant>,
    last_frame: Option<Instant>,
    clicked: Option<Instant>,
    spin_base: f64,
    pull_entered: Option<Instant>,
    pending_pull: Option<(f64, f64, f64)>,
}

impl Motion {
    fn aim(&mut self, now: Instant, target: (f64, f64, f64), delayed: bool) {
        if delayed {
            let entered = *self.pull_entered.get_or_insert(now);
            if now.duration_since(entered) < Duration::from_millis(120) {
                self.pending_pull = Some(target);
                (self.target_x, self.target_y, self.target_hover) = (0.0, 0.0, 0.0);
                return;
            }
        } else {
            self.pull_entered = None;
        }
        self.pending_pull = None;
        (self.target_x, self.target_y, self.target_hover) = target;
    }

    fn finish_dwell(&mut self, now: Instant) {
        if self
            .pull_entered
            .is_some_and(|t| now.duration_since(t) >= Duration::from_millis(120))
        {
            if let Some(target) = self.pending_pull.take() {
                (self.target_x, self.target_y, self.target_hover) = target;
            }
        }
    }
}

// A continuous distance field replaces the icon's rectangular hover boundary.
// Saturation bounds travel nearby; Gaussian falloff removes distant influence.
fn proximity(distance: f64) -> f64 {
    (-(distance / 140.0).powi(2)).exp()
}
fn tether(x: f64, y: f64) -> (f64, f64) {
    let distance = x.hypot(y);
    if distance < f64::EPSILON {
        return (0.0, 0.0);
    }
    let radius = 7.0 * (distance / 17.0).tanh() * proximity(distance);
    (radius * x / distance, radius * y / distance)
}

type Tick = Rc<RefCell<Option<gtk::TickCallbackId>>>;

fn wake(area: &gtk::DrawingArea, state: &Rc<RefCell<Motion>>, tick: &Tick) {
    if !area.is_mapped() || tick.borrow().is_some() {
        return;
    }
    state.borrow_mut().last_frame = None;
    let state = state.clone();
    let active = Rc::downgrade(tick);
    *tick.borrow_mut() = Some(area.add_tick_callback(move |area, _| {
        let now = Instant::now();
        let mut s = state.borrow_mut();
        s.finish_dwell(now);
        let dt = s.last_frame.replace(now).map_or(1.0 / 60.0, |last| {
            now.duration_since(last).as_secs_f64().min(0.05)
        });
        let enabled = area.settings().is_gtk_enable_animations();
        if !enabled {
            s.clicked = None;
        }
        let settling = s.target_hover == 0.0 || (s.target_x - s.x).hypot(s.target_y - s.y) < 2.5;
        let response = if settling { 14.0 } else { 18.0 };
        let blend = if enabled {
            1.0 - (-response * dt).exp()
        } else {
            1.0
        };
        s.x += (s.target_x - s.x) * blend;
        s.y += (s.target_y - s.y) * blend;
        s.hover += (s.target_hover - s.hover) * blend;
        let intro = enabled && s.opened.is_some_and(|t| t.elapsed().as_secs_f64() < 0.8);
        let moving =
            (s.x - s.target_x).abs() + (s.y - s.target_y).abs() + (s.hover - s.target_hover).abs()
                > 0.002;
        if !moving {
            s.x = s.target_x;
            s.y = s.target_y;
            s.hover = s.target_hover;
        }
        let waiting = s.pending_pull.is_some();
        drop(s);
        area.queue_draw();
        let clicking = enabled
            && state
                .borrow()
                .clicked
                .is_some_and(|t| t.elapsed().as_secs_f64() < 0.60);
        if intro || moving || clicking || waiting {
            glib::ControlFlow::Continue
        } else {
            if let Some(active) = active.upgrade() {
                active.borrow_mut().take();
            }
            glib::ControlFlow::Break
        }
    }));
}

pub fn new(search: &gtk::Overlay, strip: &gtk::Overlay, host: &gtk::Overlay) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder()
        .content_width(64)
        .content_height(56)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Center)
        .can_target(false)
        .can_focus(false)
        .css_classes(["omni-search-brand"])
        .build();
    let state = Rc::new(RefCell::new(Motion::default()));
    let tick: Tick = Rc::new(RefCell::new(None));
    let motion = state.clone();
    area.set_draw_func(move |widget, cr, width, height| {
        cr.translate(
            (f64::from(width) - 37.0) / 2.0,
            (f64::from(height) - 37.0) / 2.0,
        );
        let color = widget.color();
        let s = motion.borrow();
        let progress = if widget.settings().is_gtk_enable_animations() {
            s.opened
                .map_or(1.0, |t| (t.elapsed().as_secs_f64() / 0.8).min(1.0))
        } else {
            1.0
        };
        draw(
            cr,
            &s,
            progress,
            (
                color.red().into(),
                color.green().into(),
                color.blue().into(),
            ),
        );
    });
    let motion = state.clone();
    let clock = tick.clone();
    area.connect_map(move |area| {
        *motion.borrow_mut() = Motion {
            opened: Some(Instant::now()),
            ..Default::default()
        };
        wake(area, &motion, &clock);
    });
    let clock = tick.clone();
    area.connect_unmap(move |_| {
        if let Some(id) = clock.borrow_mut().take() {
            id.remove();
        }
    });

    // Observe the outer card so the header's top/side padding is included too.
    let controller = gtk::EventControllerMotion::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = area.downgrade();
    let motion = state.clone();
    let clock = tick.clone();
    let strip_bounds = strip.downgrade();
    // Use the editable child's actual horizontal allocation, excluding the icon.
    let mut child = search.child().and_then(|entry| entry.first_child());
    let mut text_field = None;
    while let Some(widget) = child {
        if widget.is::<gtk::Text>() {
            text_field = Some(widget.downgrade());
            break;
        }
        child = widget.next_sibling();
    }
    controller.connect_motion(move |controller, x, y| {
        let Some(area) = weak.upgrade() else {
            return;
        };
        let Some(parent) = controller.widget() else {
            return;
        };
        let Some(bounds) = area.compute_bounds(&parent) else {
            return;
        };
        let local_x = x - f64::from(bounds.x());
        let local_y = y - f64::from(bounds.y());
        let dx = local_x - 32.0;
        let dy = local_y - 28.0;
        // Include the outer inset above the header, while retaining the existing
        // return-home behavior in the result list below it.
        let in_header = strip_bounds
            .upgrade()
            .and_then(|strip| strip.compute_bounds(&parent))
            .is_some_and(|b| y <= f64::from(b.y() + b.height()));
        let text_rect = text_field
            .as_ref()
            .and_then(|weak| weak.upgrade())
            .and_then(|text| text.compute_bounds(&parent));
        let over_text = text_rect.as_ref().is_some_and(|b| {
            // Keep the padding above/below the central input strip responsive.
            let inset = ((b.height() - 40.0) / 2.0).max(0.0);
            x >= f64::from(b.x())
                && x <= f64::from(b.x() + b.width())
                && y >= f64::from(b.y() + inset)
                && y <= f64::from(b.y() + b.height() - inset)
        });
        let lower_band = in_header
            && text_rect.as_ref().is_some_and(|b| {
                let inset = ((b.height() - 40.0) / 2.0).max(0.0);
                x >= f64::from(b.x())
                    && x <= f64::from(b.x() + b.width())
                    && y > f64::from(b.y() + b.height() - inset)
            });
        let active = in_header && !over_text;
        let influence = if active { proximity(dx.hypot(dy)) } else { 0.0 };
        let enabled = area.settings().is_gtk_enable_animations();
        let (tx, ty) = if enabled && active {
            tether(dx, dy)
        } else {
            (0.0, 0.0)
        };
        let mut s = motion.borrow_mut();
        let previous = (s.target_x, s.target_y, s.target_hover);
        s.aim(Instant::now(), (tx, ty, influence), lower_band);
        if previous == (s.target_x, s.target_y, s.target_hover) && s.pending_pull.is_none() {
            return;
        }
        drop(s);
        wake(&area, &motion, &clock);
    });
    let click = gtk::GestureClick::new();
    click.set_button(1);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = area.downgrade();
    let motion = state.clone();
    let clock = tick.clone();
    click.connect_pressed(move |gesture, _, x, y| {
        if let Some(area) = weak.upgrade() {
            if let Some(bounds) = area
                .parent()
                .and_then(|parent| area.compute_bounds(&parent))
            {
                let inside = (0.0..64.0).contains(&(x - f64::from(bounds.x())))
                    && (0.0..56.0).contains(&(y - f64::from(bounds.y())));
                if inside && area.settings().is_gtk_enable_animations() {
                    let mut s = motion.borrow_mut();
                    let previous = s
                        .clicked
                        .map_or(0.0, |t| click_rotation(t.elapsed().as_secs_f64()));
                    s.spin_base = (s.spin_base + previous) % TAU;
                    s.clicked = Some(Instant::now());
                    drop(s);
                    wake(&area, &motion, &clock);
                }
            }
        }
        // Observe only: let SearchEntry's own gesture handle focus and selection.
        gesture.set_state(gtk::EventSequenceState::Denied);
    });
    search.add_controller(click);

    let weak = area.downgrade();
    controller.connect_leave(move |_| {
        let Some(area) = weak.upgrade() else {
            return;
        };
        let mut s = state.borrow_mut();
        s.aim(Instant::now(), (0.0, 0.0, 0.0), false);
        drop(s);
        wake(&area, &state, &tick);
    });
    host.add_controller(controller);
    area
}

// A curved blade traced from logo.png, repeated around the common center.
fn blades(cr: &Context) {
    for _ in 0..3 {
        cr.move_to(0.0, 1.7);
        cr.curve_to(-8.0, 1.5, -12.0, -5.7, -7.6, -11.4);
        cr.curve_to(-9.6, -5.5, -4.4, -1.9, -1.25, -0.8);
        cr.close_path();
        let _ = cr.fill();
        cr.rotate(TAU / 3.0);
    }
}

fn rounded_tile(cr: &Context) {
    cr.new_sub_path();
    for (x, y, start) in [
        (26.0, 11.0, -PI / 2.0),
        (26.0, 26.0, 0.0),
        (11.0, 26.0, PI / 2.0),
        (11.0, 11.0, PI),
    ] {
        cr.arc(x, y, 7.0, start, start + PI / 2.0);
    }
    cr.close_path();
}

fn draw_mark(cr: &Context, progress: f64, hover: f64, spin: f64) {
    let _ = cr.save();
    // Increase the whole logo from 29px to 34px, keeping its center fixed.
    cr.translate(18.5, 18.5);
    cr.scale(34.0 / 29.0, 34.0 / 29.0);
    cr.translate(-18.5, -18.5);
    // The backing stays stationary while the blades turn.
    rounded_tile(cr);
    let backing = LinearGradient::new(5.0, 4.0, 30.0, 33.0);
    backing.add_color_stop_rgb(0.0, 0.15, 0.18, 0.21);
    backing.add_color_stop_rgb(1.0, 0.075, 0.095, 0.12);
    let _ = cr.set_source(&backing);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(0.06, 0.73, 0.83, 0.48 + 0.16 * hover);
    cr.set_line_width(0.8);
    let _ = cr.stroke();

    cr.translate(18.5, 18.5);
    cr.rotate(TAU * (1.0 - (1.0 - progress).powi(3)) + spin);
    cr.scale(0.92, 0.92);
    let cyan = LinearGradient::new(-9.0, -12.0, 10.0, 12.0);
    cyan.add_color_stop_rgb(0.0, 0.15 + 0.1 * hover, 0.87, 0.95);
    cyan.add_color_stop_rgb(0.55, 0.04, 0.73, 0.85);
    cyan.add_color_stop_rgb(1.0, 0.02, 0.53, 0.69);
    let _ = cr.set_source(&cyan);
    blades(cr);
    let _ = cr.restore();
}

// Inverse radial mapping: 1.65x at the optical center, smoothly joining 1x at
// the rim. The monotonic mapping avoids folds as artwork moves through the lens.
fn click_rotation(seconds: f64) -> f64 {
    let t = (seconds / 0.6).clamp(0.0, 1.0);
    // Preserve initial angular speed, adding time to the final deceleration.
    TAU * (1.0 - (1.0 - t).powf(3.6))
}

fn click_pulse(seconds: f64) -> f64 {
    if !(0.0..0.60).contains(&seconds) {
        return 0.0;
    }
    let t = if seconds < 0.10 {
        seconds / 0.10
    } else {
        (0.60 - seconds) / 0.50
    };
    t * t * (3.0 - 2.0 * t)
}

fn fisheye_radius(radius: f64, boost: f64) -> f64 {
    radius / (1.0 + (0.65 + boost) * (1.0 - radius * radius).powi(2))
}

fn fisheye_artwork(
    cx: f64,
    cy: f64,
    progress: f64,
    hover: f64,
    boost: f64,
    spin: f64,
) -> Result<gtk::cairo::ImageSurface, gtk::cairo::Error> {
    use gtk::cairo::{Format, ImageSurface};
    const SCALE: f64 = 4.0;
    const SIZE: i32 = 192;
    let mut source = ImageSurface::create(Format::ARgb32, SIZE, SIZE)?;
    {
        let context = Context::new(&source)?;
        context.scale(SCALE, SCALE);
        draw_mark(&context, progress, hover, spin);
    }
    source.flush();
    let stride = source.stride() as usize;
    let mut output = ImageSurface::create(Format::ARgb32, SIZE, SIZE)?;
    // These surfaces have no live Cairo contexts or patterns when data is borrowed.
    {
        let input = source
            .data()
            .map_err(|_| gtk::cairo::Error::InvalidStatus)?;
        let mut pixels = output
            .data()
            .map_err(|_| gtk::cairo::Error::InvalidStatus)?;
        for y in 0..SIZE as usize {
            for x in 0..SIZE as usize {
                let dx = (x as f64 + 0.5) / SCALE - cx;
                let dy = (y as f64 + 0.5) / SCALE - cy;
                let radius = dx.hypot(dy) / 17.0;
                if radius > 1.01 {
                    continue;
                }
                let ratio = if radius < 1e-8 {
                    1.0 / (1.65 + boost)
                } else {
                    fisheye_radius(radius.min(1.0), boost) / radius
                };
                let sample_x = (cx + dx * ratio) * SCALE - 0.5;
                let sample_y = (cy + dy * ratio) * SCALE - 0.5;
                let left = sample_x.floor() as i32;
                let top = sample_y.floor() as i32;
                let fx = sample_x - f64::from(left);
                let fy = sample_y - f64::from(top);
                // Bilinear sampling of premultiplied channels preserves smooth edges
                // and transparency at both standard and high display scales.
                for channel in 0..4 {
                    let mut value = 0.0;
                    for (ox, oy, weight) in [
                        (0, 0, (1.0 - fx) * (1.0 - fy)),
                        (1, 0, fx * (1.0 - fy)),
                        (0, 1, (1.0 - fx) * fy),
                        (1, 1, fx * fy),
                    ] {
                        let px = left + ox;
                        let py = top + oy;
                        if (0..SIZE).contains(&px) && (0..SIZE).contains(&py) {
                            value +=
                                f64::from(input[py as usize * stride + px as usize * 4 + channel])
                                    * weight;
                        }
                    }
                    pixels[y * stride + x * 4 + channel] = value.round() as u8;
                }
            }
        }
    }
    output.mark_dirty();
    Ok(output)
}

fn draw(cr: &Context, s: &Motion, progress: f64, (r, g, b): (f64, f64, f64)) {
    let elapsed = s.clicked.map(|t| t.elapsed().as_secs_f64());
    let spin = s.spin_base + elapsed.map_or(0.0, click_rotation);
    draw_mark(cr, progress, s.hover, spin);

    let cx = 18.5 + s.x;
    let cy = 18.5 + s.y;
    let pulse = s
        .clicked
        .map_or(0.0, |t| click_pulse(t.elapsed().as_secs_f64()));
    let sx = (1.0 + s.x * 0.005) * (1.0 + 0.12 * pulse);
    let sy = (1.0 + s.y * 0.005) * (1.0 + 0.12 * pulse);
    let _ = cr.save();
    cr.translate(cx, cy);
    cr.scale(sx, sy);
    cr.arc(0.0, 0.0, 17.0, 0.0, TAU);
    cr.clip();
    if let Ok(lens) = fisheye_artwork(cx, cy, progress, s.hover, pulse, spin) {
        // Replace only the clipped interior, including transparent source pixels.
        cr.set_operator(gtk::cairo::Operator::Source);
        cr.scale(0.25, 0.25);
        let _ = cr.set_source_surface(&lens, -cx * 4.0, -cy * 4.0);
        let _ = cr.paint();
    } else {
        // Retain a useful lens even if an offscreen surface cannot be allocated.
        cr.scale(1.5, 1.5);
        cr.translate(-cx, -cy);
        draw_mark(cr, progress, s.hover, spin);
    }
    let _ = cr.restore();

    cr.set_line_cap(gtk::cairo::LineCap::Round);
    let _ = cr.save();
    cr.translate(cx, cy);
    cr.scale(sx, sy);
    let glass = RadialGradient::new(-4.0 + s.x * 0.6, -5.0 + s.y * 0.6, 0.0, 0.0, 0.0, 17.3);
    glass.add_color_stop_rgba(0.0, 0.86, 0.97, 1.0, 0.10 + 0.12 * s.hover);
    glass.add_color_stop_rgba(0.6, 0.30, 0.65, 0.75, 0.015);
    glass.add_color_stop_rgba(1.0, 0.40, 0.82, 0.90, 0.09 + 0.10 * s.hover);
    cr.arc(0.0, 0.0, 17.0, 0.0, TAU);
    let _ = cr.set_source(&glass);
    let _ = cr.fill();
    let metal = LinearGradient::new(-8.0 + s.x, -10.0 + s.y, 8.0, 10.0);
    metal.add_color_stop_rgba(0.0, 0.94, 0.98, 1.0, 0.96);
    metal.add_color_stop_rgba(0.4, r, g, b, 0.75);
    metal.add_color_stop_rgba(0.75, r, g, b, 0.90);
    metal.add_color_stop_rgba(1.0, 0.75, 0.93, 1.0, 0.95);
    let _ = cr.set_source(&metal);
    cr.set_line_width(2.4);
    cr.move_to(13.9, 9.8);
    cr.line_to(22.0, 15.5);
    let _ = cr.stroke();
    cr.set_line_width(1.0);
    cr.arc(0.0, 0.0, 17.0, 0.0, TAU);
    let _ = cr.stroke();
    cr.set_source_rgba(0.95, 0.99, 1.0, 0.45 + 0.3 * s.hover + 0.22 * pulse);
    cr.set_line_width(0.65);
    let angle = s.x * 0.045 + s.y * 0.035;
    cr.arc(0.0, 0.0, 15.7, PI * 1.07 + angle, PI * 1.49 + angle);
    let _ = cr.stroke();
    let _ = cr.restore();
}

#[cfg(test)]
mod tests {
    use super::{click_pulse, click_rotation, fisheye_artwork, fisheye_radius, tether};

    #[test]
    fn click_spin_finishes_one_turn_and_settles() {
        assert_eq!(click_rotation(0.0), 0.0);
        assert_eq!(click_rotation(0.6), std::f64::consts::TAU);
        assert_eq!(click_rotation(1.0), std::f64::consts::TAU);
        let mut previous = 0.0;
        for i in 1..=600 {
            let angle = click_rotation(f64::from(i) / 1000.0);
            assert!(angle >= previous && angle <= std::f64::consts::TAU);
            previous = angle;
        }
        assert!(click_rotation(0.6) - click_rotation(0.59) < 0.001);
    }

    #[test]
    fn click_feedback_is_bounded_and_finishes_quickly() {
        assert_eq!(click_pulse(-1.0), 0.0);
        assert_eq!(click_pulse(0.0), 0.0);
        assert!((click_pulse(0.10) - 1.0).abs() < 1e-9);
        assert_eq!(click_pulse(0.60), 0.0);
        assert_eq!(click_pulse(1.0), 0.0);
        for i in 0..600 {
            let pulse = click_pulse(f64::from(i) / 1000.0);
            assert!((0.0..=1.0).contains(&pulse));
            let normal = fisheye_radius(0.2, 0.0);
            assert!(fisheye_radius(0.2, pulse) <= normal);
            assert_eq!(fisheye_radius(1.0, pulse), 1.0);
        }
    }

    #[test]
    fn fisheye_renders_opaque_artwork_and_keeps_surroundings_transparent() {
        for (cx, cy) in [(11.5, 18.5), (18.5, 18.5), (25.5, 18.5), (18.5, 25.5)] {
            let mut lens = fisheye_artwork(cx, cy, 1.0, 0.5, 0.0, 0.0).unwrap();
            let stride = lens.stride() as usize;
            let bytes = lens.data().unwrap();
            let offset = (cy * 4.0) as usize * stride + (cx * 4.0) as usize * 4;
            let pixel = u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap());
            assert_eq!(pixel >> 24, 255, "lens center must contain artwork");
            assert_eq!(&bytes[..4], &[0, 0, 0, 0]);
        }
    }

    #[test]
    fn fisheye_center_magnifies_and_rim_joins_without_a_jump() {
        assert_eq!(fisheye_radius(0.0, 0.0), 0.0);
        assert!((0.001 / fisheye_radius(0.001, 0.0) - 1.65).abs() < 0.001);
        assert_eq!(fisheye_radius(1.0, 0.0), 1.0);
        let mut previous = 0.0;
        for i in 1..=1000 {
            let r = f64::from(i) / 1000.0;
            let mapped = fisheye_radius(r, 0.0);
            assert!(mapped > previous && mapped <= r);
            previous = mapped;
        }
        assert!((fisheye_radius(1.0, 0.0) - fisheye_radius(0.999, 0.0)) < 0.00101);
    }
    #[test]
    fn strip_pull_fades_smoothly_beyond_the_icon() {
        let near = tether(40.0, 0.0).0;
        let middle = tether(140.0, 0.0).0;
        let far = tether(500.0, 0.0).0;
        assert!(near > 5.0 && middle > 1.0 && middle < near);
        assert!(far < 0.001);
        // Crossing the former icon hitbox must not drop the target to zero.
        assert!(tether(33.0, 0.0).0 > 5.0);
        assert!((tether(33.0, 0.0).0 - tether(32.0, 0.0).0).abs() < 0.1);
    }

    #[test]
    fn tether_is_continuous_radial_and_bounded() {
        assert_eq!(tether(0.0, 0.0), (0.0, 0.0));
        let small = tether(0.001, 0.0);
        assert!(small.0 > 0.0 && small.0 < 0.001);
        for (x, y) in [(20.0, 0.0), (20.0, 20.0), (1e6, -1e6)] {
            let (a, b) = tether(x, y);
            assert!(a.hypot(b) <= 7.0);
            assert_eq!(tether(-x, -y), (-a, -b));
        }
        assert!(tether(20.0, 0.0).0 < tether(40.0, 0.0).0);
    }
    #[test]
    fn crossing_the_lower_band_does_not_engage_or_leave_a_delayed_pull() {
        let start = std::time::Instant::now();
        let mut motion = super::Motion::default();
        motion.aim(start, (4.0, 2.0, 0.8), true);
        motion.finish_dwell(start + std::time::Duration::from_millis(80));
        assert_eq!(
            (motion.target_x, motion.target_y, motion.target_hover),
            (0.0, 0.0, 0.0)
        );
        motion.aim(
            start + std::time::Duration::from_millis(90),
            (0.0, 0.0, 0.0),
            false,
        );
        motion.finish_dwell(start + std::time::Duration::from_millis(200));
        assert_eq!(
            (motion.target_x, motion.target_y, motion.target_hover),
            (0.0, 0.0, 0.0)
        );
        assert!(motion.pending_pull.is_none());
    }

    #[test]
    fn lingering_in_the_band_engages_latest_target_without_another_motion_event() {
        let start = std::time::Instant::now();
        let mut motion = super::Motion::default();
        motion.aim(start, (4.0, 2.0, 0.8), true);
        motion.aim(
            start + std::time::Duration::from_millis(60),
            (5.0, 3.0, 0.7),
            true,
        );
        motion.finish_dwell(start + std::time::Duration::from_millis(120));
        assert_eq!(
            (motion.target_x, motion.target_y, motion.target_hover),
            (5.0, 3.0, 0.7)
        );
        motion.aim(
            start + std::time::Duration::from_millis(130),
            (6.0, 2.0, 0.6),
            true,
        );
        assert_eq!(
            motion.target_x, 6.0,
            "engaged band must track without repeated delays"
        );
        motion.aim(
            start + std::time::Duration::from_millis(140),
            (-3.0, 1.0, 1.0),
            false,
        );
        assert_eq!(motion.target_x, -3.0, "direct icon hover remains immediate");
    }
}
