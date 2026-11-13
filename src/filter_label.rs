//! Smooth header layout changes when the selected filter summary grows or shrinks.
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::glib;
use gtk::prelude::*;

const RESIZE: Duration = Duration::from_millis(180);

#[derive(Clone, Copy)]
struct Tween {
    from: f64,
    to: f64,
    started: Instant,
}

impl Tween {
    fn value(self, now: Instant) -> f64 {
        let t = (now.duration_since(self.started).as_secs_f64() / RESIZE.as_secs_f64()).min(1.0);
        self.from + (self.to - self.from) * (1.0 - (1.0 - t).powi(3))
    }
}

#[derive(Default)]
struct State {
    tween: Option<Tween>,
    tick: Option<gtk::TickCallbackId>,
}

fn desired(label: &gtk::Label) -> i32 {
    std::iter::once("All")
        .chain(crate::FILTER_KINDS.iter().map(|(_, name)| *name))
        .chain(std::iter::once(label.text().as_str()))
        .map(|name| label.create_pango_layout(Some(name)).pixel_size().0)
        .max()
        .unwrap_or(0)
}

fn snap(label: &gtk::Label, state: &Rc<RefCell<State>>) {
    let mut state = state.borrow_mut();
    if let Some(tick) = state.tick.take() {
        tick.remove();
    }
    state.tween = None;
    label.set_size_request(desired(label), -1);
}

pub fn configure(label: &gtk::Label) {
    // The width request must own layout during the transition; a long new label
    // must not force its full natural width before the animation can run.
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(1);
    let state = Rc::new(RefCell::new(State::default()));
    let sizing = state.clone();
    label.connect_map(move |label| snap(label, &sizing));
    let sizing = state.clone();
    label.connect_unmap(move |label| snap(label, &sizing));
    label.connect_label_notify(move |label| {
        if !label.is_mapped() || !label.settings().is_gtk_enable_animations() {
            snap(label, &state);
            return;
        }
        let now = Instant::now();
        let target = f64::from(desired(label));
        let mut sizing = state.borrow_mut();
        let current = sizing
            .tween
            .map_or(f64::from(label.width()), |t| t.value(now));
        sizing.tween = Some(Tween {
            from: current,
            to: target,
            started: now,
        });
        label.set_size_request(current.round() as i32, -1);
        if sizing.tick.is_some() {
            return;
        }
        let active = state.clone();
        sizing.tick = Some(label.add_tick_callback(move |label, _| {
            let now = Instant::now();
            let mut state = active.borrow_mut();
            let Some(tween) = state.tween else {
                state.tick.take();
                return glib::ControlFlow::Break;
            };
            let finished = now.duration_since(tween.started) >= RESIZE
                || !label.settings().is_gtk_enable_animations();
            let width = if finished { tween.to } else { tween.value(now) };
            label.set_size_request(width.round() as i32, -1);
            if finished {
                state.tween = None;
                state.tick.take();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_changes_are_bounded_and_settle_in_both_directions() {
        let now = Instant::now();
        for (from, to) in [(50.0, 180.0), (180.0, 50.0)] {
            let tween = Tween {
                from,
                to,
                started: now,
            };
            assert_eq!(tween.value(now), from);
            for ms in 0..=180 {
                let width = tween.value(now + Duration::from_millis(ms));
                assert!(width >= from.min(to) && width <= from.max(to));
            }
            assert_eq!(tween.value(now + RESIZE), to);
            assert_eq!(tween.value(now + Duration::from_secs(1)), to);
        }
    }

    #[test]
    fn a_new_selection_continues_from_the_in_progress_width() {
        let now = Instant::now();
        let first = Tween {
            from: 50.0,
            to: 180.0,
            started: now,
        };
        let changed = now + Duration::from_millis(70);
        let current = first.value(changed);
        let second = Tween {
            from: current,
            to: 80.0,
            started: changed,
        };
        assert!(current > 50.0 && current < 180.0);
        assert_eq!(second.value(changed), current);
        assert_eq!(second.value(changed + RESIZE), 80.0);
    }
}
