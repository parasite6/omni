//! Search-field overlay widget. Previously drew a living rim; the flat search chrome
//! uses a CSS focus underline instead, so this paints nothing and stays non-interactive
//! so layout/overlay structure in main.rs is unchanged.

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::glib;

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
        }
    }

    impl WidgetImpl for SearchRing {}
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
