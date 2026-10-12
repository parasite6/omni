//! Web search through the default browser's own search engine.
//!
//! - Firefox-family browsers get `--search "query"`, which uses their default engine.
//! - Chromium-family browsers (Brave, Chrome, Chromium, Edge, Vivaldi) keep the default
//!   engine's URL template in the profile's Preferences; we fill it in and open the URL.
//! - Anything else, or a profile we can't read: DuckDuckGo.

use std::path::PathBuf;

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

const FALLBACK: (&str, &str) = ("DuckDuckGo", "https://duckduckgo.com/?q={searchTerms}");

const FIREFOX_LIKE: &[&str] = &["firefox", "librewolf", "waterfox", "floorp", "zen"];

/// Desktop-id fragment → config dir (under ~/.config) for Chromium-family browsers.
const CHROMIUM_LIKE: &[(&str, &str)] = &[
    ("brave", "BraveSoftware/Brave-Browser"),
    ("google-chrome", "google-chrome"),
    ("chromium", "chromium"),
    ("microsoft-edge", "microsoft-edge"),
    ("vivaldi", "vivaldi"),
];

#[derive(Clone)]
pub struct Engine {
    pub browser: Option<gio::AppInfo>,
    /// Shown in the row, e.g. "DuckDuckGo".
    pub name: String,
    kind: Kind,
    /// Chromium-family: the windows it can search in besides a normal one, and the engine
    /// for them (Brave has its own setting for private windows, also used with Tor).
    private: Option<(Vec<Window>, String)>,
}

/// A non-default window to search in (Chromium-family browsers).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Window {
    /// `--incognito`
    Private,
    /// Brave's private window with Tor: `--tor`
    Tor,
}

#[derive(Clone)]
enum Kind {
    /// Firefox-family: pass the query to the browser itself.
    FirefoxSearch,
    /// URL template containing `{searchTerms}`.
    Template(String),
}

impl Engine {
    /// Look up the default browser and its search engine. Cheap enough to call each time
    /// the popup opens, so a changed default is picked up without restarting omni.
    pub fn detect() -> Self {
        let browser = gio::AppInfo::default_for_uri_scheme("https");
        let id = browser.as_ref().and_then(|b| b.id()).map(|s| s.to_lowercase()).unwrap_or_default();

        if FIREFOX_LIKE.iter().any(|f| id.contains(f)) {
            return Self { name: "default search".into(), kind: Kind::FirefoxSearch, browser, private: None };
        }
        if let Some((frag, dir)) = CHROMIUM_LIKE.iter().find(|(frag, _)| id.contains(frag)) {
            if let Some(engines) = chromium_engines(&glib::user_config_dir().join(dir)) {
                let brave = *frag == "brave";
                let windows = if brave { vec![Window::Private, Window::Tor] } else { vec![Window::Private] };
                let private_url = engines.private.unwrap_or_else(|| engines.url.clone());
                return Self {
                    name: engines.name,
                    kind: Kind::Template(engines.url),
                    browser,
                    private: Some((windows, private_url)),
                };
            }
        }
        Self { name: FALLBACK.0.into(), kind: Kind::Template(FALLBACK.1.into()), browser, private: None }
    }

    /// Windows `search_in` can use besides a normal one (none for Firefox or unknown browsers).
    pub fn windows(&self) -> &[Window] {
        self.private.as_ref().map_or(&[], |(w, _)| w.as_slice())
    }

    /// Search in a private or Tor window of the default browser.
    pub fn search_in(&self, query: &str, window: Window) {
        let (Some(browser), Some((windows, template))) = (&self.browser, &self.private) else { return };
        if !windows.contains(&window) {
            return;
        }
        let argv = window_argv(&browser.executable(), window, template, query);
        let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_os_str()).collect();
        if let Err(e) = gio::Subprocess::newv(&argv, gio::SubprocessFlags::NONE) {
            eprintln!("omni: web search ({window:?} window) failed: {e}");
        }
    }

    /// What `search` would do, for debugging.
    pub fn describe(&self, query: &str) -> String {
        let browser = self.browser.as_ref().map(|b| b.id().unwrap_or_default().to_string()).unwrap_or_default();
        match &self.kind {
            Kind::FirefoxSearch => format!("{browser} --search {query:?}"),
            Kind::Template(t) => format!("{browser} {}", fill_template(t, query)),
        }
    }

    pub fn search(&self, query: &str) {
        match &self.kind {
            Kind::FirefoxSearch => {
                // Firefox scans every argument for flags, so `--kiosk` or `-P` as the
                // query would be taken as an option. Open those as a URL instead.
                if query.trim().starts_with('-') {
                    let url = format!("https://duckduckgo.com/?q={}", percent_encode(query.trim()));
                    open_url(self.browser.as_ref(), &url);
                } else {
                    let exe = self.browser.as_ref().map(|b| b.executable()).unwrap_or_else(|| "firefox".into());
                    // gio::Subprocess reaps the child; a bare std spawn would leave a zombie
                    // in this long-running process after every search.
                    let argv: [&std::ffi::OsStr; 3] = [exe.as_os_str(), "--search".as_ref(), query.as_ref()];
                    if let Err(e) = gio::Subprocess::newv(&argv, gio::SubprocessFlags::NONE) {
                        eprintln!("omni: web search failed: {e}");
                    }
                }
            }
            Kind::Template(template) => open_url(self.browser.as_ref(), &fill_template(template, query)),
        }
    }
}

fn open_url(browser: Option<&gio::AppInfo>, url: &str) {
    let result = match browser {
        Some(b) => b.launch_uris(&[url], None::<&gio::AppLaunchContext>),
        None => gio::AppInfo::launch_default_for_uri(url, None::<&gio::AppLaunchContext>),
    };
    if let Err(e) = result {
        eprintln!("omni: couldn't open {url}: {e}");
    }
}

struct Engines {
    name: String,
    url: String,
    /// Brave's private-window engine, if set.
    private: Option<String>,
}

/// Default engine (name, URL template) from a Chromium-family profile, plus Brave's
/// private-window engine.
fn chromium_engines(config: &std::path::Path) -> Option<Engines> {
    let profile = std::fs::read(config.join("Local State"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["profile"]["last_used"].as_str().map(str::to_string))
        .unwrap_or_else(|| "Default".into());
    let prefs: serde_json::Value =
        serde_json::from_slice(&std::fs::read(PathBuf::from(config).join(profile).join("Preferences")).ok()?).ok()?;
    let data = &prefs["default_search_provider_data"]["template_url_data"];
    let url = clean_template(data["url"].as_str()?)?;
    let name = data["short_name"].as_str().unwrap_or("Web").to_string();
    let private = prefs["brave"]["default_private_search_provider_data"]["url"].as_str().and_then(clean_template);
    Some(Engines { name, url, private })
}

/// A search URL, or None. After trim it must be `http://` or `https://` and contain
/// `{searchTerms}`. `javascript:`, `file:`, `steam:`, a leading `-` (a Chromium switch)
/// and anything with whitespace or a control character are not templates. Brave tags
/// DuckDuckGo searches with `&t=brave`; leave it off.
fn clean_template(url: &str) -> Option<String> {
    let url = url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) || !url.contains("{searchTerms}") {
        return None;
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    if url.contains("duckduckgo.com") {
        return Some(url.replace("&t=brave", "").replace("?t=brave&", "?"));
    }
    Some(url.to_string())
}

/// The browser command for a search in a private or Tor window. `--` ends switch
/// parsing so the template can't be a Chromium flag; the URL is then one argument
/// (the query is percent-encoded inside it).
fn window_argv(exe: &std::path::Path, window: Window, template: &str, query: &str) -> Vec<std::ffi::OsString> {
    let flag = match window {
        Window::Private => "--incognito",
        Window::Tor => "--tor",
    };
    vec![exe.into(), flag.into(), "--".into(), fill_template(template, query).into()]
}

/// Fill `{searchTerms}` and drop Chromium-only placeholders like `{google:baseURL}`.
fn fill_template(template: &str, query: &str) -> String {
    let filled = template.replace("{searchTerms}", &percent_encode(query));
    let mut out = String::with_capacity(filled.len());
    let mut depth = 0;
    for c in filled.chars() {
        match c {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_window_commands() {
        let exe = std::path::Path::new("/usr/bin/brave-browser-stable");
        let ddg = "https://duckduckgo.com/?q={searchTerms}";
        assert_eq!(
            window_argv(exe, Window::Private, ddg, "rust gtk"),
            ["/usr/bin/brave-browser-stable", "--incognito", "--", "https://duckduckgo.com/?q=rust+gtk"]
        );
        assert_eq!(
            window_argv(exe, Window::Tor, ddg, "--version"),
            ["/usr/bin/brave-browser-stable", "--tor", "--", "https://duckduckgo.com/?q=--version"]
        );
    }

    #[test]
    fn clean_template_accepts_only_http_search_urls() {
        assert_eq!(
            clean_template("https://example.com/search?q={searchTerms}").as_deref(),
            Some("https://example.com/search?q={searchTerms}")
        );
        assert_eq!(
            clean_template("https://duckduckgo.com/?q={searchTerms}&t=brave").as_deref(),
            Some("https://duckduckgo.com/?q={searchTerms}")
        );
        assert_eq!(clean_template("javascript:alert(1){searchTerms}"), None);
        assert_eq!(clean_template("file:///tmp/{searchTerms}"), None);
        assert_eq!(clean_template("--proxy-server={searchTerms}"), None);
        assert_eq!(clean_template("steam://open/{searchTerms}"), None);
        assert_eq!(clean_template("https://example.com/search?q={searchTerms}&x=a b"), None);
        assert_eq!(clean_template("https://example.com/\n{searchTerms}"), None);
    }

    #[test]
    fn fills_template() {
        assert_eq!(
            fill_template("https://duckduckgo.com/?q={searchTerms}&t=brave", "rust & gtk?"),
            "https://duckduckgo.com/?q=rust+%26+gtk%3F&t=brave"
        );
        assert_eq!(
            fill_template("{google:baseURL}search?q={searchTerms}{google:originalQueryForSuggestion}", "a"),
            "search?q=a"
        );
    }
}
