//! Embedded search browser with provider-specific page styles.
use gtk::prelude::*;
use std::cell::RefCell;

pub struct SearchView {
    pub root: gtk::Box,
    pub back: gtk::Button,
    status: gtk::Label,
    host: gtk::Stack,
    loading_spinner: gtk::Spinner,
    view: RefCell<Option<crate::chromium::Surface>>,
}

impl SearchView {
    pub fn new() -> Self {
        // Chromium starts on the first search, not while the launcher is idle: its
        // message pump shares GTK's main thread, and an idle engine must never compete
        // with typing. A blank pane in the page's colour covers the one-time startup; the
        // page itself replaces it on its first frame, so the only loading animation the
        // user sees is Brave's own.
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root.add_css_class("omni-glass");
        root.add_css_class("omni-web-plane");
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let back = gtk::Button::with_label("← Back");
        back.add_css_class("omni-settings-back");
        let status = gtk::Label::builder()
            .label("Brave Search")
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        toolbar.append(&back);
        let icon = crate::search_provider::mark_image(20);
        let identity = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        identity.set_hexpand(true);
        identity.append(&icon);
        identity.append(&status);
        toolbar.append(&identity);
        root.append(&toolbar);
        let host = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        host.set_hexpand(true);
        host.set_vexpand(true);
        host.set_height_request(400);
        let loading = gtk::Box::new(gtk::Orientation::Vertical, 0);
        loading.add_css_class("omni-web-loading");
        let loading_spinner = gtk::Spinner::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .vexpand(true)
            .build();
        loading.append(&loading_spinner);
        host.add_named(&loading, Some("loading"));
        root.append(&host);
        Self {
            root,
            back,
            status,
            host,
            loading_spinner,
            view: RefCell::new(None),
        }
    }

    pub fn search(&self, provider: crate::search_provider::SearchProvider, query: &str) {
        // A submitted query starts a new private session. Never show the previous
        // page/texture while Chromium navigates to the next question.
        self.close();
        self.loading_spinner.start();
        self.status.set_text("Loading Brave Search…");
        if self.view.borrow().is_none() {
            match crate::chromium::Surface::new(provider.style_sheet(), &self.status) {
                Ok(view) => {
                    self.host.add_named(&view.picture, Some("browser"));
                    let host = self.host.downgrade();
                    let spinner = self.loading_spinner.downgrade();
                    view.on_ready(move || {
                        if let Some(host) = host.upgrade() {
                            host.set_transition_duration(if crate::motion_enabled() {
                                240
                            } else {
                                0
                            });
                            host.set_visible_child_name("browser");
                        }
                        if let Some(spinner) = spinner.upgrade() {
                            spinner.stop();
                        }
                    });
                    *self.view.borrow_mut() = Some(view);
                }
                Err(error) => {
                    self.loading_spinner.stop();
                    self.status.set_text(&error);
                    return;
                }
            }
        }
        if let Some(view) = self.view.borrow().as_ref() {
            view.load(&provider.search_url(query));
        }
    }

    pub fn stop(&self) {
        // Back and popup close both end the conversation, including its browser state.
        self.close();
    }

    pub fn close(&self) {
        self.host.set_transition_duration(0);
        self.host.set_visible_child_name("loading");
        self.loading_spinner.stop();
        if let Some(view) = self.view.borrow_mut().take() {
            view.stop();
            view.picture.set_paintable(None::<&gtk::gdk::Paintable>);
            self.host.remove(&view.picture);
        }
        self.status.set_text("Brave Search");
    }

    pub fn set_height(&self, height: i32) {
        self.host.set_height_request(height.max(200));
    }
}
