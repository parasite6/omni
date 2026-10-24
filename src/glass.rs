//! Opaque launcher card shell. The panel fill is fully opaque CSS (`#1b1b1f`).

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct GlassCard;

    #[glib::object_subclass]
    impl ObjectSubclass for GlassCard {
        const NAME: &'static str = "OmniGlassCard";
        type Type = super::GlassCard;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for GlassCard {}
    impl BoxImpl for GlassCard {}
    impl WidgetImpl for GlassCard {}
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
}
