//! "Ask Claude": quick answers streamed into the popup via the Claude Code CLI, with
//! follow-up questions in the same temporary conversation.
//!
//! The Claude app's `claude://claude.ai/new?q=` link only pre-fills the chat box (by design,
//! links can't send prompts), so omni runs `claude -p` instead:
//! Haiku 5.5 at medium effort by default (model and effort switchable from the answer's
//! header), the user's claude.ai connectors but no local plugins, hooks or auto-memory, and
//! no saved session. Uses the user's normal Claude Code login.
//!
//! Tools and permissions: Claude can search the web, read and search files, run commands
//! and edit or write files, working from the home folder. Claude Code's own permission
//! checks decide what needs asking (read-only work runs freely; commands that change
//! things, edits, writes and web fetches ask), and omni answers its prompts
//! (`--permission-prompt-tool stdio`) with Deny / Allow once / Always allow. "Always" is
//! scoped to this conversation (destination "session"), never written to settings.
//! Secrets (SSH and GPG keys, cloud and git credentials, keyrings, browser profiles,
//! Claude's own credentials) are denied outright, for reads and shell reads alike.
//!
//! Conversation: one `claude -p --input-format stream-json` process stays running per
//! conversation and takes each message as a JSON line on stdin, so follow-ups keep the
//! context without anything being written to disk (`--no-session-persistence`). The process
//! is killed when the popup closes or the conversation is left. If it dies mid-conversation
//! (timeout, crash), the next reply starts a fresh one primed with the transcript so far.
//!
//! Display: a bouncing ball plays until a small buffer of text has arrived, then the text
//! is revealed at a steady per-frame pace, a little behind the stream, so network bursts
//! never show up as stalls or jumps.

use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{cairo, gdk, gio, glib};

/// Models and effort levels offered in the answer header's dropdown: (CLI value, label).
pub const MODELS: [(&str, &str); 4] = [
    ("claude-haiku-5-5", "Haiku 5.5"),
    ("claude-sonnet-5-5", "Sonnet 5.5"),
    ("claude-opus-5-5", "Opus 5.5"),
    ("claude-fable-5-1", "Fable 5.1"),
];
pub const EFFORTS: [(&str, &str); 5] = [("low", "Low"), ("medium", "Medium"), ("high", "High"), ("xhigh", "Extra"), ("max", "Max")];
/// The user's own agent instructions (identity, AgentMail rules, files, fail-safe),
/// appended after the launcher instructions above. Edit src/agent_prompt.md and reinstall.
const AGENT_PROMPT: &str = include_str!("agent_prompt.md");

/// AgentMail tools the agent instructions reserve for the user. Denied outright, so no
/// email or prompt injection can get them used, whatever the model decides.
const AGENTMAIL_USER_ONLY: [&str; 9] = [
    "create_inbox", "update_inbox", "delete_inbox", "delete_thread", "delete_list_entry",
    "connect_provider", "connect_app", "agent_attach_human", "agent_verify",
];

/// AgentMail tools that only read. Not pre-approved: a prompt still appears unless every
/// inbox field is the contactai inbox (or there is no inbox field). Sends and changes are
/// in AGENTMAIL_USER_ONLY and stay disallowed. See `agentmail_read_is_contactai`.
const AGENTMAIL_READ_ONLY: [&str; 14] = [
    "list_inboxes", "get_inbox", "search_inboxes", "list_threads", "get_thread", "search_threads",
    "list_messages", "get_message", "search_messages", "get_attachment", "list_drafts", "get_draft",
    "list_list_entries", "get_list_entry",
];
const AGENTMAIL: &str = "mcp__claude_ai_AgentMail__";
/// The only inbox a read may touch without asking. The agent prompt names this address.
const CONTACTAI_INBOX: &str = "assistant@example.com";

/// Tools Claude may use. WebSearch alone is pre-approved (it only sends a query); the rest
/// go through Claude Code's permission checks and omni's prompt.
const TOOLS: &str = "WebSearch,WebFetch,Read,Glob,Grep,Bash,Edit,Write";
const PRE_APPROVED: &str = "WebSearch";
/// Never readable or editable, whatever is approved: a fetched page could otherwise talk
/// Claude into reading a key and sending it off in a URL.
/// Paths under the home folder: denied to the file tools by permission rules and to shell
/// commands by the sandbox (permission rules alone miss globs: a shell read of
/// `~/.s?h/id_ed25519` slipped past them in testing; the sandbox hides the path itself).
/// Whole directories, not the one file inside them: a rule for `~/.docker/config.json` still
/// left the rest of `~/.docker` readable. A secret that is itself a file stays listed too.
const SECRETS: [&str; 88] = [
    "~/.ssh", "~/.gnupg", "~/.pki", "~/.local/share/pki", "~/.cert", "~/.config/wireguard", "~/.config/kdeconnect",
    "~/.aws", "~/.azure", "~/.config/gcloud", "~/.config/gh", "~/.kube",
    "~/.local/share/keyrings", "~/.local/share/kwalletd", "~/.config/kwalletrc", "~/.password-store",
    "~/.claude", "~/.claude/.credentials.json", "~/.claude.json", "~/.claude.json.backup", "~/.config/anthropic",
    "~/.gemini", "~/.codex", "~/.config/github-copilot", "~/.copilot", "~/.codeium", "~/.local/share/opencode",
    "~/.config/cursor", "~/.config/Cursor", "~/.cursor", "~/.local/state/cursor",
    "~/.config/Code", "~/.config/Code - Insiders", "~/.vscode", "~/.vscode-oss", "~/.vscode-insiders",
    "~/.netrc", "~/.docker", "~/.docker/config.json", "~/.git-credentials", "~/.config/git/credentials",
    "~/.npmrc", "~/.yarnrc", "~/.pypirc", "~/.cargo/credentials.toml", "~/.config/rclone", "~/.config/ngrok", "~/.dropbox",
    "~/.mozilla", "~/.config/mozilla", "~/.thunderbird", "~/.config/evolution", "~/.local/share/evolution", "~/.config/goa-1.0",
    "~/.config/BraveSoftware", "~/.config/google-chrome", "~/.config/google-chrome-beta", "~/.config/google-chrome-unstable",
    "~/.config/chromium", "~/.config/microsoft-edge", "~/.config/vivaldi", "~/.config/opera", "~/.config/librewolf",
    "~/.config/torbrowser", "~/.local/share/torbrowser",
    "~/.config/Bitwarden", "~/.config/discord", "~/.config/Signal", "~/.config/Slack", "~/.config/Element",
    "~/.local/share/TelegramDesktop",
    "~/.config/protonmail", "~/.config/protonvpn", "~/.config/Proton", "~/.config/Proton Mail",
    "~/.var/app",
    "~/.zsh_history", "~/.bash_history", "~/.histfile", "~/.viminfo", "~/.python_history", "~/.node_repl_history",
    "~/.lesshst", "~/.wget-hsts", "~/.mysql_history", "~/.psql_history", "~/.sqlite_history",
    "~/.cache/huggingface",
];

/// Never writable by shell commands, even approved ones: files that run later as code
/// outside the sandbox (shell startup, the Hyprland session, autostart, user services,
/// launchers, default apps, folders on PATH, omni itself). Tested: an approved command
/// can't append to, delete or create these, or rename their folder away. Claude's Edit
/// and Write tools aren't sandboxed; they can still change them, always with a prompt.
/// The sandbox protects what a symlink points to, not the symlink: a command could delete
/// `~/.config/hypr` (a symlink into ~/.mydotfiles) and make a real folder there. So all of
/// `~/.config` is read-only to commands, which also covers kitty, zshrc, fish, waybar, ...
/// Known gap: `~/.bashrc` is a symlink directly in home, which can't be made read-only
/// without the whole home folder; zsh is the login shell, so bash rarely reads it.
const PROTECTED: [&str; 35] = [
    "~/.bashrc", "~/.bash_profile", "~/.bash_login", "~/.bash_logout", "~/.profile",
    "~/.zshrc", "~/.zprofile", "~/.zshenv", "~/.zlogin", "~/.zlogout", "~/.xprofile", "~/.pam_environment",
    "~/.oh-my-zsh", "~/.vimrc", "~/.gitconfig", "~/.mydotfiles", "~/.config",
    "~/.local/share/applications", "~/.local/share/systemd", "~/.local/share/dbus-1",
    "~/.local/share/nautilus", "~/.local/share/nautilus-python", "~/.local/share/flatpak/overrides",
    "~/.local/share/omni", "~/.local/share/prism",
    // Python runs any .pth file in the user site-packages on every start.
    "~/.local/lib",
    "~/bin", "~/.local/bin", "~/.cargo/bin", "~/go/bin", "~/.bun/bin", "~/.photon/bin", "~/.lmstudio/bin",
    "~/.spicetify", "~/.local/share/JetBrains/Toolbox/scripts",
];

/// A permission prompt from Claude Code, for the popup to show.
pub struct PermissionAsk {
    /// "Run a command", "Edit a file", ...
    pub title: String,
    /// What exactly: the command, the file, the URL.
    pub detail: String,
    /// The third button's label when there's something to remember for this conversation:
    /// "Always allow" (a rule for this kind of action) or "Always allow this file" (edits).
    pub remember: Option<&'static str>,
}

/// The user's answer to a permission prompt.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Decision {
    Deny,
    Once,
    Always,
}

/// Haiku 5.5 at Medium, until the user picks something else (then remembered).
const DEFAULT_MODEL: usize = 0;
const DEFAULT_EFFORT: usize = 1;
const SYSTEM: &str = "You are answering a quick question typed into a desktop app launcher. \
Answer directly and concisely: lead with the answer, keep it short unless detail is asked for. \
Use web search when the question needs current information. \
Your reply is shown with its Markdown rendered: links become clickable, lists and bold display \
as formatting. So write links and formatting as plain Markdown in your reply, once. Never put \
Markdown in code blocks to show its source, and never add a separate 'rendered' version or a \
'Rendered:' line. If the user asks for something in Markdown format, just use the Markdown; \
it is rendered for them. Use code blocks only for actual code or commands. \
You can read and search the user's files, run commands and edit files, working from their home \
folder; anything that changes things asks the user for approval in the launcher first. The user's \
connected apps (claude.ai connectors such as email, calendar or notes) are available as tools: when \
asked to use one, call its tool directly instead of looking for a command-line program. Only do \
that when the question needs it, and keep it minimal.";

/// Keep the ball rolling until this much text is buffered, or this long after the first token.
const START_CHARS: usize = 24;
const START_DELAY_US: i64 = 600_000;
/// Reveal pace: at least this many chars/second, faster when behind so the backlog
/// drains within CATCH_UP seconds.
const MIN_CPS: f64 = 70.0;
const CATCH_UP: f64 = 0.6;
/// Give up (kill Claude, say so) after this long with no output mid-answer; more effort
/// thinks longer between outputs, so it gets longer.
/// Written by the omni-mailcount system service (scripts/omni-mailcount): the unread
/// count only. The API key never comes near omni.
const MAIL_COUNT: &str = "/run/omni-mail/count";
/// Asking omni-mailcount for a fresh check (answers with the count; can do nothing else).
const MAIL_TRIGGER: &str = "/run/omni-mail/trigger";
/// What the mail tile asks, once the current turn can take another message.
const MAIL_ASK: &str = "Check my unread AgentMail emails and summarize them briefly.";

/// Longest a single answer may run, however busy, before it's stopped.
const HARD_LIMIT: Duration = Duration::from_secs(20 * 60);

fn timeout_for(effort: usize) -> Duration {
    Duration::from_secs([90, 90, 150, 240, 360][effort.min(4)])
}

/// Widgets of the answer panel plus the running request.
pub struct Answer {
    /// Panel with the bouncing ball drawn over it.
    pub panel: gtk::Overlay,
    title: gtk::Label,
    stack: gtk::Stack,
    ball: gtk::DrawingArea,
    ball_state: Rc<RefCell<Ball>>,
    /// Header widgets the ball bounces off.
    obstacles: Vec<gtk::Widget>,
    /// One question label + answer label per turn. Finished turns are never re-rendered, so a
    /// long conversation costs no more per frame than a short one.
    turns: gtk::Box,
    /// The answer label of the turn being typed out.
    body: RefCell<gtk::Label>,
    scroller: gtk::ScrolledWindow,
    proc: RefCell<Option<gio::Subprocess>>,
    stdin: RefCell<Option<gio::OutputStream>>,
    /// JSON lines not yet written. One write at a time: two `write_all_future` calls on
    /// this stream can interleave.
    out_queue: RefCell<std::collections::VecDeque<Vec<u8>>>,
    writing: Cell<bool>,
    /// claude.ai connectors are expected (so a process without them lost a race inside
    /// Claude Code, see `handle`), and restarts spent on that this turn.
    connectors_seen: Cell<bool>,
    connector_retries: Cell<u8>,
    /// When Claude Code last said anything (the timeout counts silence, see `watch`).
    last_activity: Cell<i64>,
    /// The running process's systemd scope (see `spawn`).
    scope: RefCell<Option<String>>,
    /// Bumped for every new process (and on cancel) so output from a dead one is ignored.
    generation: Cell<u64>,
    /// Bumped every turn, so a turn's timeout can't fire on a later turn.
    turn: Cell<u64>,
    /// Finished turns: (question, answer). Primes a replacement process if one dies.
    history: RefCell<Vec<(String, String)>>,
    /// A turn is waiting for or receiving its answer.
    busy: Cell<bool>,
    /// Keep the newest text in view while it's typed out (off once the user scrolls up).
    follow: Rc<Cell<bool>>,
    /// AgentMail unread badge: the tile (hidden without the service), its button, the
    /// number, and the refresh timer that runs while a conversation is open.
    mail: gtk::Overlay,
    mail_button: gtk::Button,
    mail_badge: gtk::Label,
    mail_timer: RefCell<Option<glib::SourceId>>,
    /// One mail-tile question waiting out a busy turn: (that turn, the question).
    /// A second click replaces it; they don't pile up.
    mail_followup: Cell<Option<(u64, &'static str)>>,
    /// Header chip showing the model and effort ("Haiku 5.5 · Medium ▾"); the popup opens
    /// the dropdown from it.
    pub chip: gtk::Button,
    chip_label: gtk::Label,
    /// Chosen model and effort (indexes into MODELS / EFFORTS).
    model: Cell<usize>,
    effort: Cell<usize>,
    /// Changed while Claude was answering: restart with the new choice before the next turn.
    restart_pending: Cell<bool>,
    /// The finished answer of the current turn, shared with its copy button.
    turn_answer: RefCell<Rc<RefCell<String>>>,
    /// Permission prompts waiting for an answer, oldest first: (request id, the request).
    pending: RefCell<std::collections::VecDeque<(String, serde_json::Value)>>,
    /// Shows a permission prompt (set by the popup), and takes it away when it goes stale.
    on_permission: RefCell<Option<Rc<dyn Fn(PermissionAsk)>>>,
    on_permission_gone: RefCell<Option<Rc<dyn Fn()>>>,
    /// What clicking a link in an answer does (set by the popup: close, then open it).
    on_link: RefCell<Option<Rc<dyn Fn(&str)>>>,
    question: RefCell<String>,
    /// This turn's answer so far.
    text: RefCell<String>,
    /// How many chars of `text` are on screen (fractional, advanced per frame).
    shown: Cell<f64>,
    first_token_at: Cell<Option<i64>>,
    revealing: Cell<bool>,
    finished: Cell<bool>,
    /// A question is running or its answer is on screen (false after cancel).
    active: Cell<bool>,
    failed: Cell<bool>,
    tick: RefCell<Option<gtk::TickCallbackId>>,
}

impl Answer {
    pub fn new() -> Rc<Self> {
        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .max_width_chars(1)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        title.add_css_class("omni-answer-title");

        let icon = match gio::AppInfo::default_for_uri_scheme("claude").and_then(|a| a.icon()) {
            Some(i) => gtk::Image::from_gicon(&i),
            None => gtk::Image::from_icon_name("dialog-question"),
        };
        icon.set_pixel_size(18);
        icon.add_css_class("omni-answer-icon");

        let cont = gtk::Button::with_label("Continue in Claude");
        cont.add_css_class("omni-answer-button");

        let (model, effort) = load_choice();
        let chip_label = gtk::Label::new(Some(&choice_label(model, effort)));
        let chip_content = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        chip_content.set_halign(gtk::Align::Center);
        chip_content.append(&chip_label);
        chip_content.append(&gtk::Image::from_icon_name("pan-down-symbolic"));
        let chip = gtk::Button::builder()
            .child(&chip_content)
            .tooltip_text("Model and effort")
            .css_classes(["omni-answer-button", "omni-model-chip"])
            .build();

        // AgentMail: logo tile with an unread badge (see refresh_mail). Hidden unless the
        // omni-mailcount service is installed.
        let logo = gdk::Texture::from_bytes(&glib::Bytes::from_static(include_bytes!("../agentmaillogo.png"))).ok();
        let mail_image = gtk::Image::builder().pixel_size(14).build();
        if let Some(logo) = &logo {
            mail_image.set_paintable(Some(logo));
        }
        let mail_button = gtk::Button::builder().child(&mail_image).css_classes(["omni-mail"]).valign(gtk::Align::Center).build();
        let mail_badge = gtk::Label::builder().halign(gtk::Align::End).valign(gtk::Align::Start).xalign(0.5).justify(gtk::Justification::Center).can_target(false).visible(false).build();
        mail_badge.add_css_class("omni-mail-badge");
        let mail = gtk::Overlay::builder().child(&mail_button).visible(false).valign(gtk::Align::Center).build();
        mail.add_overlay(&mail_badge);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        header.append(&icon);
        header.append(&title);
        header.append(&mail);
        header.append(&chip);
        header.append(&cont);

        // max_width_chars(1): a wrapping label otherwise asks for the width of its whole
        // text and would widen the popup; this way it wraps to the card's fixed width.
        let turns = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(460)
            .child(&turns)
            .build();
        // Scrolling up to reread stops the view from chasing new text.
        let follow = Rc::new(Cell::new(true));
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let f = follow.clone();
        scroll.connect_scroll(move |_, _, dy| {
            if dy < 0.0 {
                f.set(false);
            }
            glib::Propagation::Proceed
        });
        scroller.add_controller(scroll);

        // While waiting: empty room for the ball (drawn on an overlay over the whole panel).
        let waiting = gtk::Box::builder().height_request(100).build();

        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(320)
            .interpolate_size(true)
            .vhomogeneous(false)
            .build();
        stack.add_named(&waiting, Some("loading"));
        stack.add_named(&scroller, Some("text"));

        let inner = gtk::Box::new(gtk::Orientation::Vertical, 10);
        inner.add_css_class("omni-answer");
        inner.append(&header);
        inner.append(&stack);

        // The ball's colour comes from CSS (`color:` on .omni-ball = omni cyan).
        let ball_state = Rc::new(RefCell::new(Ball::default()));
        let ball = gtk::DrawingArea::builder().can_target(false).build();
        ball.add_css_class("omni-ball");
        let state = ball_state.clone();
        ball.set_draw_func(move |area, cr, _, _| draw_ball(area, cr, &state.borrow()));

        let panel = gtk::Overlay::builder().child(&inner).build();
        panel.add_overlay(&ball);

        let answer = Rc::new(Self {
            panel,
            title: title.clone(),
            stack,
            ball,
            ball_state,
            obstacles: vec![icon.clone().upcast(), title.clone().upcast(), mail.clone().upcast(), chip.clone().upcast(), cont.clone().upcast()],
            turns,
            body: RefCell::new(answer_label()),
            scroller,
            proc: RefCell::default(),
            stdin: RefCell::default(),
            out_queue: RefCell::default(),
            writing: Cell::new(false),
            scope: RefCell::default(),
            // Assume the account has connectors until a turn shows otherwise (see `handle`).
            connectors_seen: Cell::new(true),
            connector_retries: Cell::new(0),
            last_activity: Cell::new(0),
            generation: Cell::new(0),
            turn: Cell::new(0),
            history: RefCell::default(),
            busy: Cell::new(false),
            follow,
            on_link: RefCell::default(),
            pending: RefCell::default(),
            on_permission: RefCell::default(),
            on_permission_gone: RefCell::default(),
            turn_answer: RefCell::default(),
            chip: chip.clone(),
            mail: mail.clone(),
            mail_button: mail_button.clone(),
            mail_badge,
            mail_timer: RefCell::default(),
            mail_followup: Cell::new(None),
            chip_label,
            model: Cell::new(model),
            effort: Cell::new(effort),
            restart_pending: Cell::new(false),
            question: RefCell::default(),
            text: RefCell::default(),
            shown: Cell::new(0.0),
            first_token_at: Cell::new(None),
            revealing: Cell::new(false),
            finished: Cell::new(false),
            active: Cell::new(false),
            failed: Cell::new(false),
            tick: RefCell::default(),
        });

        // Clicking the mail tile asks Claude about the unread mail (as a follow-up).
        // reply() drops the text while a turn is busy; the tile has nowhere else to keep
        // it, so hold that one question until this turn's result.
        let a = answer.clone();
        mail_button.connect_clicked(move |_| {
            if a.reply(MAIL_ASK) {
                return;
            }
            if a.active.get() && a.busy.get() {
                a.mail_followup.set(Some((a.turn.get(), MAIL_ASK)));
            }
        });
        let a = answer.clone();
        cont.connect_clicked(move |_| {
            let url = format!("claude://claude.ai/new?q={}", crate::web::percent_encode(&a.question.borrow()));
            if let Err(e) = gio::AppInfo::launch_default_for_uri(&url, None::<&gio::AppLaunchContext>) {
                eprintln!("omni: couldn't open Claude: {e}");
            }
        });
        answer
    }

    /// The chosen (model, effort), as indexes into MODELS / EFFORTS.
    pub fn choice(&self) -> (usize, usize) {
        (self.model.get(), self.effort.get())
    }

    /// "Haiku 5.5 · Medium"
    pub fn choice_label(&self) -> String {
        choice_label(self.model.get(), self.effort.get())
    }

    /// Switch model and/or effort, remembered for next time. Mid-conversation, the next
    /// message goes to a fresh Claude Code with the new settings, primed with the
    /// transcript, so the conversation carries on (a running answer finishes first).
    pub fn set_choice(&self, model: usize, effort: usize) {
        let (model, effort) = (model.min(MODELS.len() - 1), effort.min(EFFORTS.len() - 1));
        if (model, effort) == self.choice() {
            return;
        }
        self.model.set(model);
        self.effort.set(effort);
        self.chip_label.set_label(&choice_label(model, effort));
        save_choice(model, effort);
        if self.busy.get() {
            // The running answer finishes on its current process. The next message
            // restarts — unless a connector retry already spawned with these cells
            // (that path clears the flag, so we don't restart twice).
            self.restart_pending.set(true);
        } else {
            self.stop_process();
        }
    }

    /// Claude Code asking permission calls `f` with what to show; answer with `respond`.
    pub fn set_permission_handler(&self, f: impl Fn(PermissionAsk) + 'static) {
        self.on_permission.replace(Some(Rc::new(f)));
    }

    /// The prompt on screen no longer applies (Claude stopped): `f` takes it away.
    pub fn set_permission_gone_handler(&self, f: impl Fn() + 'static) {
        self.on_permission_gone.replace(Some(Rc::new(f)));
    }

    /// Answer the permission prompt on screen (the oldest waiting one), then show the next.
    pub fn respond(self: &Rc<Self>, decision: Decision) {
        let Some((id, request)) = self.pending.borrow_mut().pop_front() else { return };
        let response = match decision {
            Decision::Deny => serde_json::json!({
                "behavior": "deny",
                "message": "The user denied this in omni. Don't retry it; ask or take another approach.",
            }),
            Decision::Once => serde_json::json!({ "behavior": "allow", "updatedInput": request["input"] }),
            Decision::Always => {
                // Claude Code's suggestions that the button described, kept for this
                // conversation only (never written to a settings file).
                let mut rules = remembered(&request);
                for r in &mut rules {
                    r["destination"] = serde_json::json!("session");
                }
                serde_json::json!({ "behavior": "allow", "updatedInput": request["input"], "updatedPermissions": rules })
            }
        };
        self.send_line(serde_json::json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": id, "response": response },
        }));
        // The wait for Allow/Deny was the user, not Claude going quiet. Silence
        // starts again from this click; otherwise a slow Allow trips the timeout.
        self.last_activity.set(glib::monotonic_time());
        self.show_next_permission();
    }

    fn show_next_permission(&self) {
        let Some(ask) = self.pending.borrow().front().map(|(_, r)| describe(r)) else { return };
        if let Some(f) = self.on_permission.borrow().clone() {
            f(ask);
        }
    }

    /// A permission prompt is waiting for the user.
    pub fn asking(&self) -> bool {
        !self.pending.borrow().is_empty()
    }

    /// Clicking a link in an answer calls `f` with its URL.
    pub fn set_link_handler(&self, f: impl Fn(&str) + 'static) {
        self.on_link.replace(Some(Rc::new(f)));
    }

    /// Start a new conversation with `question`, streaming the answer into the panel.
    pub fn ask(self: &Rc<Self>, question: &str) {
        // Same first question of this conversation, still in flight: don't kill it and
        // ask again. History stays empty until that answer lands, and cancel clears both
        // history and `active`, so this does not swallow the opening question of a later
        // conversation. `turn` is not part of the check: it only climbs (resetting it to 0
        // made the previous conversation's silence timer match the next one's turn 1).
        let fresh = self.history.borrow().is_empty();
        if self.active.get() && !self.failed.get() && fresh && *self.question.borrow() == question {
            return;
        }
        self.cancel();
        self.active.set(true);
        // The unread badge: now, then every 10 s while the conversation is open (it's a tiny
        // file in /run; the service itself checks AgentMail every 45 s).
        self.refresh_mail();
        let weak = Rc::downgrade(self);
        let timer = glib::timeout_add_local(Duration::from_secs(10), move || match weak.upgrade() {
            Some(this) if this.active.get() => {
                this.refresh_mail();
                glib::ControlFlow::Continue
            }
            _ => glib::ControlFlow::Break,
        });
        if let Some(old) = self.mail_timer.replace(Some(timer)) {
            old.remove();
        }
        self.stack.set_visible_child_name("loading");
        self.start_turn(question);
    }

    /// Ask a follow-up in the current conversation. Ignored while Claude is still answering
    /// (the caller keeps the text) or when there is no conversation.
    pub fn reply(self: &Rc<Self>, text: &str) -> bool {
        if !self.active.get() || self.busy.get() || text.trim().is_empty() {
            return false;
        }
        self.start_turn(text);
        true
    }

    fn start_turn(self: &Rc<Self>, question: &str) {
        self.turn.set(self.turn.get() + 1);
        let turn = self.turn.get();
        self.failed.set(false);
        self.busy.set(true);
        self.question.replace(question.to_string());
        self.text.replace(String::new());
        self.shown.set(0.0);
        self.first_token_at.set(None);
        self.revealing.set(false);
        self.finished.set(false);
        self.follow.set(true);
        self.connector_retries.set(0);
        self.title.set_text("Claude");
        // Every prompt: a fresh unread count.
        self.poke_mail();
        // Settings changed while the last answer was running: this turn gets them.
        if self.restart_pending.replace(false) {
            self.stop_process();
        }

        // This turn's labels: the question, then the answer as it arrives.
        let asked = gtk::Label::builder()
            .label(question)
            .xalign(0.0)
            .max_width_chars(1)
            .hexpand(true)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .build();
        asked.add_css_class("omni-answer-question");
        let body = answer_label();
        if let Some(f) = self.on_link.borrow().clone() {
            body.connect_activate_link(move |_, uri| {
                f(uri);
                glib::Propagation::Stop
            });
        }
        // Follow-ups: room below the question for the ball until text arrives (the first
        // turn has the loading page for that).
        if turn > 1 {
            body.set_size_request(-1, FOLLOWUP_ROOM);
        }
        // Each message gets its own copy button (text isn't selectable).
        let q = question.to_string();
        let asked_row = with_copy(&asked, move || q.clone());
        if turn > 1 {
            asked_row.add_css_class("omni-answer-followup");
        }
        self.turns.append(&asked_row);
        // The answer's copy button copies this turn's own answer: filled in when the turn
        // finishes; before that, whatever has arrived (only while it's still the current turn).
        let done = Rc::new(RefCell::new(String::new()));
        self.turn_answer.replace(done.clone());
        let weak = Rc::downgrade(self);
        self.turns.append(&with_copy(&body, move || {
            if !done.borrow().is_empty() {
                return done.borrow().clone();
            }
            match weak.upgrade() {
                Some(this) if this.turn.get() == turn && this.busy.get() => this.text.borrow().clone(),
                _ => String::new(),
            }
        }));
        self.body.replace(body);
        self.follow_newest(); // bring the new question and the ball's room into view

        *self.ball_state.borrow_mut() = Ball::launch();
        self.ball.set_opacity(1.0);
        self.ball.set_visible(true);
        self.start_ticking();

        // A running process already has the conversation; a new one (first turn, or the
        // old one died) gets the transcript so far ahead of the new message.
        let message = if self.proc.borrow().is_some() {
            question.to_string()
        } else {
            if let Err(e) = self.start_process() {
                return self.fail(&format!("Couldn't start Claude Code: {e}"));
            }
            primed(&self.history.borrow(), question)
        };
        self.send(&message);

        self.arm_timeout(turn, timeout_for(self.effort.get()));
    }

    /// Give up on `turn` when Claude has been silent for `timeout` (or after HARD_LIMIT in
    /// all): a long task that keeps working (tool calls, streaming) isn't cut off, a hung
    /// one is. A permission prompt pauses both clocks: the user is the one who's quiet,
    /// and `respond` restarts the silence window when they answer. `turn` is checked so a
    /// timer from an earlier conversation can't fail a later one.
    fn arm_timeout(self: &Rc<Self>, turn: u64, timeout: Duration) {
        let started = glib::monotonic_time();
        self.last_activity.set(started);
        self.watch(turn, timeout, started, timeout);
    }

    fn watch(self: &Rc<Self>, turn: u64, timeout: Duration, started: i64, wait: Duration) {
        let this = self.clone();
        glib::timeout_add_local_once(wait, move || {
            if this.turn.get() != turn || !this.busy.get() {
                return;
            }
            let now = glib::monotonic_time();
            let silent = Duration::from_micros((now - this.last_activity.get()).max(0) as u64);
            let total = Duration::from_micros((now - started).max(0) as u64);
            // Don't fail while a prompt is up, even if the hard limit or the silence
            // window already passed. The 30s is just how often we look again.
            if this.asking() {
                return this.watch(turn, timeout, started, Duration::from_secs(30));
            }
            if total >= HARD_LIMIT {
                this.stop_process();
                this.fail(&format!("Claude was still working after {} minutes, so it was stopped.", HARD_LIMIT.as_secs() / 60));
                return;
            }
            if silent >= timeout {
                this.stop_process();
                this.fail(&format!("Claude stopped responding for {} seconds.", timeout.as_secs()));
                return;
            }
            // Wake for whichever limit comes first. Sleeping the whole silence window
            // let the 20-minute cap overshoot by up to that window (360s at max effort).
            let until_silence = timeout.saturating_sub(silent);
            let until_hard = HARD_LIMIT.saturating_sub(total);
            let next = until_silence.min(until_hard).max(Duration::from_secs(1));
            this.watch(turn, timeout, started, next);
        });
    }

    /// Spawn Claude Code and read its events until it exits.
    fn start_process(self: &Rc<Self>) -> Result<(), glib::Error> {
        let (proc, scope) = spawn(MODELS[self.model.get()].0, EFFORTS[self.effort.get()].0)?;
        self.scope.replace(scope);
        self.generation.set(self.generation.get() + 1);
        let generation = self.generation.get();
        self.stdin.replace(proc.stdin_pipe());
        let stdout = proc.stdout_pipe().expect("stdout is piped");
        self.proc.replace(Some(proc));

        let this = self.clone();
        let lines = gio::DataInputStream::new(&stdout);
        glib::spawn_future_local(async move {
            let mut log = String::new(); // non-JSON output (errors, warnings)
            loop {
                let line = match lines.read_line_utf8_future(glib::Priority::DEFAULT).await {
                    Ok(Some(l)) => l,
                    _ => break,
                };
                if this.generation.get() != generation {
                    return; // cancelled or replaced
                }
                this.last_activity.set(glib::monotonic_time());
                match serde_json::from_str::<serde_json::Value>(&line) {
                    Ok(event) => this.handle(&event),
                    Err(_) => {
                        // Keep only the end: enough to explain a failure, bounded however
                        // long the process runs.
                        log.push_str(&line);
                        log.push('\n');
                        trim_log(&mut log);
                    }
                }
            }
            if this.generation.get() != generation {
                return;
            }
            // The pipe closed. Dropping proc/stdin here used to leave the systemd scope
            // (and any prompt) behind; the next start replaced the scope name and the
            // old one was never killed. Same reap as stop_process, then say so if this
            // turn was still waiting. A generation that already moved on is not a failure
            // — stop_process bumps it, so this won't run twice.
            let busy = this.busy.get();
            let why = if log.trim().is_empty() { "Claude Code stopped before answering.".to_string() } else { log.trim().to_string() };
            this.stop_process();
            if busy {
                this.fail(&why);
            }
        });
        Ok(())
    }

    /// Send one user message as a stream-json line. Asynchronous: a long message must not
    /// block the UI thread while Claude isn't reading (the popup holds the keyboard).
    fn send(self: &Rc<Self>, text: &str) {
        if self.stdin.borrow().is_none() {
            return self.fail("Couldn't send the message to Claude Code: it isn't running.");
        }
        self.send_line(serde_json::json!({ "type": "user", "message": { "role": "user", "content": text } }));
    }

    /// Write one JSON line to Claude Code's stdin (a message or a control response).
    /// Queued behind any write already in flight so two `write_all_future`s can't overlap.
    fn send_line(self: &Rc<Self>, value: serde_json::Value) {
        if self.stdin.borrow().is_none() {
            return;
        }
        let line = value.to_string() + "\n";
        self.out_queue.borrow_mut().push_back(line.into_bytes());
        self.pump_writes();
    }

    fn pump_writes(self: &Rc<Self>) {
        if self.writing.get() {
            return;
        }
        let Some(stdin) = self.stdin.borrow().clone() else { return };
        let Some(line) = self.out_queue.borrow_mut().pop_front() else { return };
        self.writing.set(true);
        let (this, generation) = (self.clone(), self.generation.get());
        glib::spawn_future_local(async move {
            let result = stdin.write_all_future(line, glib::Priority::DEFAULT).await;
            // A newer process owns `writing` and the queue. Don't clear its flag.
            if this.generation.get() != generation {
                return;
            }
            this.writing.set(false);
            if let Err((_, e)) = result {
                this.out_queue.borrow_mut().clear();
                if this.busy.get() {
                    this.stop_process();
                    this.fail(&format!("Couldn't send the message to Claude Code: {e}"));
                }
                return;
            }
            this.pump_writes();
        });
    }

    fn stop_process(&self) {
        // Detach the reader first so the process's end isn't reported as a failure.
        // Idempotent: the reader also reaps on EOF, and a second call finds nothing left.
        self.generation.set(self.generation.get() + 1);
        self.out_queue.borrow_mut().clear();
        // The in-flight write, if any, is the old stream. The next process may write.
        self.writing.set(false);
        // Prompts for a dead process can't be answered: take any on screen away.
        let had = !self.pending.borrow().is_empty();
        self.pending.borrow_mut().clear();
        if had {
            if let Some(f) = self.on_permission_gone.borrow().clone() {
                f();
            }
        }
        self.stdin.replace(None);
        if let Some(p) = self.proc.take() {
            if let Some(pid) = p.identifier().and_then(|id| id.parse::<i32>().ok()) {
                // The whole group: Claude Code and every command it started.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
            p.force_exit();
        }
        // ... and anything that left the group: the scope holds them all.
        if let Some(scope) = self.scope.take() {
            let _ = gio::Subprocess::newv(
                &["systemctl", "--user", "kill", "--signal=SIGKILL", "--", scope.as_str()].map(std::ffi::OsStr::new),
                gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_SILENCE,
            );
        }
    }

    fn handle(self: &Rc<Self>, event: &serde_json::Value) {
        if event["type"] == "control_request" {
            return self.control_request(event);
        }
        if !self.busy.get() {
            return; // stray output between turns
        }
        // Claude Code sometimes starts before the claude.ai connector list arrives (about one
        // start in four in testing) and then has no AgentMail, Calendar, ... for the whole
        // conversation. If connectors worked before, restart and resend before any text.
        if event["type"] == "system" && event["subtype"] == "init" {
            let has = event["mcp_servers"].as_array().is_some_and(|s| s.iter().any(|x| x["name"].as_str().is_some_and(|n| n.starts_with("claude.ai "))));
            if has {
                self.connectors_seen.set(true);
            } else if self.connectors_seen.get() && self.connector_retries.get() < 3 && self.text.borrow().is_empty() {
                // Per turn only. Leaving connectors_seen set means the next conversation
                // tries again; three misses just stops this turn (retries reset in start_turn).
                self.connector_retries.set(self.connector_retries.get() + 1);
                self.stop_process();
                if let Err(e) = self.start_process() {
                    return self.fail(&format!("Couldn't start Claude Code: {e}"));
                }
                // start_process already read the model/effort cells. A switch made while
                // the previous process was up is in this one; restarting again next
                // message would drop the user into a second, empty restart.
                self.restart_pending.set(false);
                let question = self.question.borrow().clone();
                self.send(&primed(&self.history.borrow(), &question));
            }
            return;
        }
        match event["type"].as_str() {
            Some("stream_event") => {
                let ev = &event["event"];
                if ev["type"] == "content_block_delta" && ev["delta"]["type"] == "text_delta" {
                    if let Some(t) = ev["delta"]["text"].as_str() {
                        self.text.borrow_mut().push_str(t);
                        if self.first_token_at.get().is_none() {
                            self.first_token_at.set(Some(glib::monotonic_time()));
                        }
                    }
                }
                // A new text block after a tool call (e.g. web search): keep blocks apart.
                if ev["type"] == "content_block_start"
                    && ev["content_block"]["type"] == "text"
                    && !self.text.borrow().is_empty()
                {
                    self.text.borrow_mut().push_str("\n\n");
                }
                if ev["type"] == "content_block_start" && ev["content_block"]["type"] == "server_tool_use" {
                    self.title.set_text("Claude \u{b7} searching the web\u{2026}");
                }
            }
            // End of this turn (the process stays up for the next one).
            Some("result") => {
                self.title.set_text("Claude");
                if event["is_error"].as_bool() == Some(true) {
                    let msg = event["result"].as_str().unwrap_or("Claude returned an error");
                    return self.fail(msg);
                }
                if self.text.borrow().is_empty() {
                    if let Some(r) = event["result"].as_str() {
                        self.text.replace(r.to_string());
                    }
                }
                if self.text.borrow().is_empty() {
                    return self.fail("No answer came back.");
                }
                self.busy.set(false);
                self.finished.set(true); // the reveal loop finishes typing out what's buffered
                self.history.borrow_mut().push((self.question.borrow().clone(), self.text.borrow().clone()));
                self.turn_answer.borrow().replace(self.text.borrow().clone());
                self.send_mail_followup();
            }
            _ => {}
        }
    }

    /// Claude Code asks the host something. Permission prompts are queued and shown one at
    /// a time; anything else gets an error reply right away, so Claude never waits on an
    /// answer that won't come.
    fn control_request(self: &Rc<Self>, event: &serde_json::Value) {
        let id = event["request_id"].as_str().unwrap_or_default().to_string();
        let request = &event["request"];
        if request["subtype"] == "can_use_tool" && self.busy.get() && !id.is_empty() {
            let tool_name = request["tool_name"].as_str().unwrap_or_default();
            // Reads of the contactai inbox (and list calls that name no inbox) skip the
            // prompt. Any other inbox is shown like a normal request. Same payload as Once.
            if agentmail_read_is_contactai(tool_name, &request["input"]) {
                self.send_line(serde_json::json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": id,
                        "response": { "behavior": "allow", "updatedInput": request["input"] },
                    },
                }));
                return;
            }
            let first = self.pending.borrow().is_empty();
            self.pending.borrow_mut().push_back((id, request.clone()));
            if first {
                self.show_next_permission();
            }
            return;
        }
        self.send_line(serde_json::json!({
            "type": "control_response",
            "response": { "subtype": "error", "request_id": id, "error": "omni doesn't handle this request" },
        }));
    }

    /// One frame: animate the ball, decide when to switch to text, type text out.
    fn start_ticking(self: &Rc<Self>) {
        let this = self.clone();
        let last = Cell::new(None::<i64>);
        let id = self.panel.add_tick_callback(move |_, clock| {
            let now = clock.frame_time();
            let dt = last.get().map_or(0.0, |l| (now - l) as f64 / 1e6);
            last.set(Some(now));

            this.step_ball(dt);

            if !this.revealing.get() {
                let buffered = this.text.borrow().chars().count();
                let waited = this.first_token_at.get().is_some_and(|t| glib::monotonic_time() - t >= START_DELAY_US);
                let ready = buffered > 0 && (buffered >= START_CHARS || waited || this.finished.get());
                if !ready {
                    return glib::ControlFlow::Continue;
                }
                this.revealing.set(true);
                this.body.borrow().set_size_request(-1, -1);
                this.stack.set_visible_child_name("text");
            }

            let total = this.text.borrow().chars().count() as f64;
            let shown = this.shown.get();
            if shown < total {
                let cps = MIN_CPS.max((total - shown) / CATCH_UP);
                let next = (shown + cps * dt).min(total);
                this.shown.set(next);
                if next.floor() > shown.floor() {
                    let partial: String = this.text.borrow().chars().take(next as usize).collect();
                    let done = this.finished.get() && next >= total;
                    set_markup_safe(&this.body.borrow(), &partial, !done);
                    this.follow_newest();
                }
            } else if this.finished.get() {
                set_markup_safe(&this.body.borrow(), &this.text.borrow(), false);
                this.follow_newest();
                this.tick.replace(None);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
        if let Some(old) = self.tick.replace(Some(id)) {
            old.remove();
        }
    }

    /// Move the ball one frame; after the reveal starts, fade it out.
    fn step_ball(&self, dt: f64) {
        if !self.ball.is_visible() {
            return;
        }
        if self.revealing.get() {
            let o = self.ball.opacity() - dt / BALL_FADE;
            if o <= 0.0 {
                self.ball.set_visible(false);
                return;
            }
            self.ball.set_opacity(o);
        }
        let w = self.ball.width() as f64;
        let h = self.ball.height() as f64;
        if w < 1.0 || h < 1.0 {
            return;
        }
        // Solid boxes: the header widgets, and on follow-ups the conversation's text in view
        // (clipped to the scrolled area). Labels only as wide as their text.
        let as_rect = |o: &gtk::Widget| -> Option<(f64, f64, f64, f64)> {
            if !o.is_drawable() {
                return None;
            }
            let b = o.compute_bounds(&self.panel)?;
            let mut width = b.width() as f64;
            if let Some(label) = o.downcast_ref::<gtk::Label>() {
                width = width.min(label.layout().pixel_size().0 as f64);
            }
            (width > 0.0).then(|| (b.x() as f64, b.y() as f64, width, b.height() as f64))
        };
        let mut rects: Vec<(f64, f64, f64, f64)> = self.obstacles.iter().filter_map(as_rect).collect();
        // Where the open space starts: below the header, and below the lowest text in view.
        let mut top = rects.iter().map(|r| r.1 + r.3).fold(0.0f64, f64::max);
        if let Some(view) = self.scroller.compute_bounds(&self.panel) {
            let (vy0, vy1) = (view.y() as f64, (view.y() + view.height()) as f64);
            // Each turn row: a label and its copy button.
            let mut row = self.turns.first_child();
            while let Some(r) = row {
                let mut child = r.first_child();
                while let Some(c) = child {
                    if let Some((x, y, cw, ch)) = as_rect(&c) {
                        let (y0, y1) = (y.max(vy0), (y + ch).min(vy1));
                        if y1 > y0 {
                            rects.push((x, y0, cw, y1 - y0));
                            top = top.max(y1);
                        }
                    }
                    child = c.next_sibling();
                }
                row = r.next_sibling();
            }
        }
        let mut ball = self.ball_state.borrow_mut();
        if ball.fresh {
            // Wait until there's room (a follow-up's open space appears once the view has
            // scrolled to it), then start somewhere in it, never inside the text.
            if h - top < BALL_RADIUS * 6.0 {
                return;
            }
            ball.place(w, h, top);
        }
        ball.step(dt.min(1.0 / 30.0), w, h, top, &rects);
        drop(ball);
        self.ball.queue_draw();
    }

    fn stop_ticking(&self) {
        if let Some(id) = self.tick.take() {
            id.remove();
        }
    }

    /// Scroll to the newest text unless the user scrolled up to reread.
    fn follow_newest(&self) {
        if !self.follow.get() || self.turn.get() <= 1 {
            return;
        }
        let adj = self.scroller.vadjustment();
        // After this frame's layout, so the new height is known.
        glib::idle_add_local_once(move || adj.set_value(adj.upper() - adj.page_size()));
    }

    /// The mail tile's question, held while this turn was busy. Only the turn it was
    /// clicked on, and only if that conversation is still the open one.
    fn send_mail_followup(self: &Rc<Self>) {
        let Some((turn, text)) = self.mail_followup.take() else { return };
        if turn != self.turn.get() || !self.active.get() || self.failed.get() {
            return;
        }
        // The result can land before the type-out finishes, and the next turn replaces
        // the label. Leave the finished answer whole on the label that's staying.
        let body = self.body.borrow();
        body.set_size_request(-1, -1);
        set_markup_safe(&body, &self.text.borrow(), false);
        drop(body);
        self.start_turn(text);
    }

    /// This turn failed: say why in its answer slot. Earlier turns stay, and the user can
    /// reply again (a dead process is replaced, primed with the transcript).
    fn fail(&self, message: &str) {
        self.failed.set(true);
        self.mail_followup.set(None);
        self.body.borrow().set_size_request(-1, -1);
        self.busy.set(false);
        self.ball.set_visible(false);
        self.stop_ticking();
        self.stack.set_visible_child_name("text");
        self.body.borrow().set_markup(&format!(
            "<span foreground=\"#ff9a8a\">{}</span>",
            glib::markup_escape_text(message)
        ));
        self.follow_newest();
    }

    /// Tallest the answer text may get before it scrolls (fits the card to the screen).
    pub fn set_max_height(&self, height: i32) {
        self.scroller.set_max_content_height(height.min(460));
    }

    /// Update the AgentMail badge from omni-mailcount's file (a tiny file in /run, read in
    /// microseconds). No file: the service isn't installed, so no tile at all.
    fn refresh_mail(&self) {
        let Ok(text) = std::fs::read_to_string(MAIL_COUNT) else {
            self.mail.set_visible(false);
            return;
        };
        let state: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        let unread = state["unread"].as_u64().unwrap_or(0);
        let more = state["more"].as_bool().unwrap_or(false);
        let (badge, tip, warn) = match state["state"].as_str() {
            Some("ok") if unread == 0 => (None, "No unread email in assistant@example.com".to_string(), false),
            Some("ok") => {
                let n = if more || unread > 99 { "99+".to_string() } else { unread.to_string() };
                let tip = format!("{n} unread conversation{} in assistant@example.com \u{b7} click to ask Claude", if unread == 1 && !more { "" } else { "s" });
                (Some(n), tip, false)
            }
            Some("expired") => (Some("!".to_string()), "The AgentMail key was rejected (expired or revoked)".to_string(), true),
            _ => (None, "Couldn't check AgentMail right now".to_string(), false),
        };
        self.mail.set_visible(true);
        self.mail_button.set_tooltip_text(Some(&tip));
        self.mail_badge.set_visible(badge.is_some());
        self.mail_badge.set_label(badge.as_deref().unwrap_or(""));
        if warn {
            self.mail_badge.add_css_class("omni-mail-warn");
        } else {
            self.mail_badge.remove_css_class("omni-mail-warn");
        }
    }

    /// Ask omni-mailcount to check AgentMail now, off the UI thread; the badge updates when
    /// it answers (or stays as it was if the service is slow or missing).
    fn poke_mail(self: &Rc<Self>) {
        if !std::path::Path::new(MAIL_TRIGGER).exists() {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let client = gio::SocketClient::new();
            client.set_timeout(25);
            let address = gio::UnixSocketAddress::new(std::path::Path::new(MAIL_TRIGGER));
            let Ok(conn) = client.connect_future(&address).await else { return };
            // The service checks, writes its file, sends the same JSON and closes.
            let _ = conn.input_stream().read_bytes_future(4096, glib::Priority::DEFAULT).await;
            if let Some(this) = weak.upgrade() {
                this.refresh_mail();
            }
        });
    }

    /// End the conversation (Esc, new search, popup closed): stop Claude and forget it.
    pub fn cancel(&self) {
        self.active.set(false);
        self.mail_followup.set(None);
        if let Some(timer) = self.mail_timer.take() {
            timer.remove();
        }
        self.busy.set(false);
        self.stop_process();
        self.stop_ticking();
        self.ball.set_visible(false);
        self.history.borrow_mut().clear();
        while let Some(child) = self.turns.first_child() {
            self.turns.remove(&child);
        }
        self.scroller.vadjustment().set_value(0.0);
    }
}

/// How long the ball takes to fade out once text starts appearing (s).
const BALL_FADE: f64 = 0.3;
const BALL_RADIUS: f64 = 8.0;
/// Open space under a follow-up question for the ball to bounce in until the answer starts.
const FOLLOWUP_ROOM: i32 = 130;
const GRAVITY: f64 = 1400.0; // px/s²
const SIDE_SPEED: f64 = 260.0; // px/s
/// Inset from the panel's rounded border so the ball bounces on the visible edge line.
const WALL: f64 = 3.0;

/// The bouncing ball: plain physics inside the answer panel.
#[derive(Default, Clone, Copy)]
struct Ball {
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
    /// Squash from the last impact (0..1), decays back to round.
    squash: f64,
    /// Direction of the last impact: squash along this axis.
    squash_vertical: bool,
    /// Just launched: placed into the open space on the first frame.
    fresh: bool,
}

impl Ball {
    /// A new ball; it's placed at a random spot in the open space on its first frame.
    fn launch() -> Self {
        Self { x: 40.0, y: 60.0, vx: SIDE_SPEED, vy: -120.0, squash: 0.0, squash_vertical: true, fresh: true }
    }

    /// Somewhere random in the open space between `top` and the floor, heading left or
    /// right with a random toss, so no two launches look alike.
    fn place(&mut self, w: f64, h: f64, top: f64) {
        let r = BALL_RADIUS;
        let (x0, x1) = (WALL + r * 2.0, (w - WALL - r * 2.0).max(WALL + r * 2.0 + 1.0));
        let (y0, y1) = (top + r * 2.0, (h - WALL - r * 2.0).max(top + r * 2.0 + 1.0));
        self.x = glib::random_double_range(x0, x1);
        self.y = glib::random_double_range(y0, y1);
        let dir = if glib::random_int_range(0, 2) == 0 { -1.0 } else { 1.0 };
        self.vx = dir * SIDE_SPEED * glib::random_double_range(0.85, 1.15);
        self.vy = -glib::random_double_range(60.0, 360.0);
        self.fresh = false;
    }

    /// `top`: where the open space starts (below the text on follow-ups, else 0); the
    /// bounce height is sized to it.
    fn step(&mut self, dt: f64, w: f64, h: f64, top: f64, rects: &[(f64, f64, f64, f64)]) {
        let r = BALL_RADIUS;
        self.vy += GRAVITY * dt;
        self.x += self.vx * dt;
        self.y += self.vy * dt;
        self.squash = (self.squash - dt * 5.0).max(0.0);

        // Floor: the panel's bottom edge. Always bounce back high enough to use the
        // whole box, so it never settles into small hops.
        let floor = h - WALL - r;
        if self.y > floor {
            self.y = floor;
            let reach = (2.0 * GRAVITY * ((h - top) * 0.75)).sqrt();
            self.vy = -(self.vy.abs() * 0.9).max(reach);
            self.hit(true);
        }
        if self.y < WALL + r {
            self.y = WALL + r;
            self.vy = self.vy.abs();
            self.hit(true);
        }
        if self.x < WALL + r {
            self.x = WALL + r;
            self.vx = self.vx.abs();
            self.hit(false);
        }
        if self.x > w - WALL - r {
            self.x = w - WALL - r;
            self.vx = -self.vx.abs();
            self.hit(false);
        }

        // Header text and buttons: push out along the shallowest side and reflect.
        for &(rx, ry, rw, rh) in rects {
            let cx = self.x.clamp(rx, rx + rw);
            let cy = self.y.clamp(ry, ry + rh);
            let (dx, dy) = (self.x - cx, self.y - cy);
            if dx * dx + dy * dy >= r * r {
                continue;
            }
            let push_left = self.x + r - rx;
            let push_right = rx + rw - (self.x - r);
            let push_up = self.y + r - ry;
            let push_down = ry + rh - (self.y - r);
            let min = push_left.min(push_right).min(push_up).min(push_down);
            if min == push_up {
                self.y -= push_up;
                self.vy = -self.vy.abs();
                self.hit(true);
            } else if min == push_down {
                self.y += push_down;
                self.vy = self.vy.abs();
                self.hit(true);
            } else if min == push_left {
                self.x -= push_left;
                self.vx = -self.vx.abs();
                self.hit(false);
            } else {
                self.x += push_right;
                self.vx = self.vx.abs();
                self.hit(false);
            }
        }
    }

    fn hit(&mut self, vertical: bool) {
        self.squash = 1.0;
        self.squash_vertical = vertical;
    }
}

/// A plain round 2D ball in omni's cyan, squashing along the direction it hit.
fn draw_ball(area: &gtk::DrawingArea, cr: &cairo::Context, ball: &Ball) {
    if ball.fresh {
        return; // not placed yet
    }
    let c = area.color();
    let s = ball.squash * 0.28;
    let (sx, sy) = if ball.squash_vertical { (1.0 + s, 1.0 - s) } else { (1.0 - s, 1.0 + s) };
    cr.save().ok();
    cr.translate(ball.x, ball.y);
    cr.scale(sx, sy);
    cr.set_source_rgb(c.red() as f64, c.green() as f64, c.blue() as f64);
    cr.arc(0.0, 0.0, BALL_RADIUS, 0.0, 2.0 * PI);
    let _ = cr.fill();
    cr.restore().ok();
}

fn claude_binary() -> PathBuf {
    glib::find_program_in_path("claude").unwrap_or_else(|| glib::home_dir().join(".local/bin/claude"))
}

/// Start Claude Code. Returns the process and, when it runs in its own systemd scope, the
/// scope's name: stopping the scope ends everything it started, even commands that
/// detached themselves (`setsid`, double forks) out of its process group.
fn spawn(model: &str, effort: &str) -> Result<(gio::Subprocess, Option<String>), glib::Error> {
    let launcher = gio::SubprocessLauncher::new(
        gio::SubprocessFlags::STDIN_PIPE | gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE,
    );
    // Works from the home folder (relative paths, "my Documents", ...). No CLAUDE.md or
    // settings are loaded from it: --setting-sources is empty.
    launcher.set_cwd(glib::home_dir());
    // Its own process group, so stopping it also stops any command it started (a server,
    // a `sleep`) instead of leaving it running after the conversation ends.
    launcher.set_child_setup(|| unsafe {
        libc::setsid();
    });
    // Claude Code's auto-memory would otherwise try to save "remember X" requests into
    // its memory files.
    launcher.setenv("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1", true);
    let claude = claude_binary();
    let denied: String = SECRETS
        .iter()
        .flat_map(|p| [format!("Read({p})"), format!("Read({p}/**)"), format!("Edit({p})"), format!("Edit({p}/**)")])
        .chain(AGENTMAIL_USER_ONLY.iter().map(|t| format!("{AGENTMAIL}{t}")))
        .collect::<Vec<_>>()
        .join(",");
    let system = format!("{SYSTEM}\n\n{AGENT_PROMPT}");
    // Shell commands run in Claude Code's bubblewrap sandbox: the secrets don't exist for
    // them, network access asks, and they never fall back to running unsandboxed (or start
    // at all if the sandbox can't). The sandbox doesn't approve commands by itself
    // (autoAllowBashIfSandboxed is on by default and would run any command that stays in
    // the home folder, `rm` included, without asking).
    let sandbox = serde_json::json!({
        "sandbox": {
            "enabled": true,
            "failIfUnavailable": true,
            "allowUnsandboxedCommands": false,
            "autoAllowBashIfSandboxed": false,
            // A slice: serde_json only implements Serialize for arrays up to 32.
            "filesystem": { "denyRead": SECRETS.as_slice(), "denyWrite": PROTECTED.as_slice() },
        }
    })
    .to_string();
    static SCOPES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let scope = glib::find_program_in_path("systemd-run").map(|_| {
        format!("omni-claude-{}-{}", std::process::id(), SCOPES.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    });
    let unit_arg = scope.clone().unwrap_or_default();
    let run_in_scope: Vec<&std::ffi::OsStr> = if scope.is_some() {
        ["systemd-run", "--user", "--scope", "--quiet", "--collect", "--unit", unit_arg.as_str(), "--"].map(std::ffi::OsStr::new).to_vec()
    } else {
        Vec::new()
    };
    let claude_argv: Vec<&std::ffi::OsStr> = [
        claude.as_os_str(),
        "-p".as_ref(),
        "--model".as_ref(),
        model.as_ref(),
        "--effort".as_ref(),
        effort.as_ref(),
        "--no-session-persistence".as_ref(),
        "--tools".as_ref(),
        TOOLS.as_ref(),
        "--allowedTools".as_ref(),
        PRE_APPROVED.as_ref(),
        "--disallowedTools".as_ref(),
        denied.as_ref(),
        // Permission prompts come to omni as control requests on stdout.
        "--permission-prompt-tool".as_ref(),
        "stdio".as_ref(),
        // No --strict-mcp-config: the user's claude.ai connectors (AgentMail, Calendar,
        // Notion, ...) load, each call asking permission like any other tool. Local plugins
        // and their hooks don't: --setting-sources stays empty.
        "--setting-sources".as_ref(),
        "".as_ref(),
        "--settings".as_ref(),
        sandbox.as_ref(),
        "--append-system-prompt".as_ref(),
        system.as_ref(),
        // One JSON message per line on stdin, for as many turns as the conversation has.
        "--input-format".as_ref(),
        "stream-json".as_ref(),
        "--output-format".as_ref(),
        "stream-json".as_ref(),
        "--include-partial-messages".as_ref(),
        "--verbose".as_ref(),
    ]
    .into();
    let argv: Vec<&std::ffi::OsStr> = run_in_scope.into_iter().chain(claude_argv).collect();
    let proc = launcher.spawn(&argv)?;
    Ok((proc, scope.map(|s| format!("{s}.scope"))))
}

/// `label` with a small copy button beside it; `text` gives what to copy at click time.
/// The icon turns into a checkmark for a moment to confirm.
fn with_copy(label: &gtk::Label, text: impl Fn() -> String + 'static) -> gtk::Box {
    let button = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy")
        .valign(gtk::Align::Start)
        .css_classes(["flat", "omni-copy"])
        .build();
    button.connect_clicked(move |b| {
        b.clipboard().set_text(&text());
        b.set_icon_name("object-select-symbolic");
        let b = b.clone();
        glib::timeout_add_local_once(Duration::from_millis(1200), move || b.set_icon_name("edit-copy-symbolic"));
    });
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.append(label);
    row.append(&button);
    row
}

fn choice_label(model: usize, effort: usize) -> String {
    format!("{} \u{b7} {}", MODELS[model].1, EFFORTS[effort].1)
}

fn choice_path() -> PathBuf {
    let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| glib::home_dir().join(".local/state"));
    state.join("omni").join("claude")
}

/// The remembered model and effort ("claude-haiku-5-5 medium"), else the defaults.
fn load_choice() -> (usize, usize) {
    let saved = std::fs::read_to_string(choice_path()).unwrap_or_default();
    let mut parts = saved.split_whitespace();
    let model = parts.next().and_then(|m| MODELS.iter().position(|(id, _)| *id == m)).unwrap_or(DEFAULT_MODEL);
    let effort = parts.next().and_then(|e| EFFORTS.iter().position(|(id, _)| *id == e)).unwrap_or(DEFAULT_EFFORT);
    (model, effort)
}

fn save_choice(model: usize, effort: usize) {
    let path = choice_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{} {}\n", MODELS[model].0, EFFORTS[effort].0));
}

/// What to show for a permission request: a short title and the exact thing asked for,
/// taken from the tool's input (never from Claude's own description of it, which could
/// say anything). `visible` caps a huge write so laying it out can't stall the UI thread
/// (the popup holds the keyboard). `dangerouslyDisableSandbox` is ignored on purpose:
/// showing or acting on it would let the model talk the prompt into leaving the sandbox.
fn describe(request: &serde_json::Value) -> PermissionAsk {
    let tool = request["tool_name"].as_str().unwrap_or("a tool");
    let input = &request["input"];
    let s = |k: &str| input[k].as_str().unwrap_or_default().to_string();
    let (title, detail) = match tool {
        "Bash" => {
            let mut detail = s("command");
            // Otherwise a background command looks like it finishes with the turn.
            if input["run_in_background"].as_bool() == Some(true) {
                detail.push_str("\n\u{2014} runs in the background \u{2014}");
            }
            ("Claude wants to run a command".to_string(), detail)
        }
        "Edit" => {
            let mut detail = format!(
                "{}\n\u{2014} replace \u{2014}\n{}\n\u{2014} with \u{2014}\n{}",
                s("file_path"),
                s("old_string"),
                s("new_string")
            );
            if input["replace_all"].as_bool() == Some(true) {
                detail.push_str("\n\u{2014} replaces every match \u{2014}");
            }
            ("Claude wants to edit a file".to_string(), detail)
        }
        "Write" => ("Claude wants to write a file".to_string(), format!("{}\n\u{2014} content \u{2014}\n{}", s("file_path"), s("content"))),
        "Read" => ("Claude wants to read a file".to_string(), s("file_path")),
        "Glob" | "Grep" => ("Claude wants to search files".to_string(), format!("{} {}", s("pattern"), s("path")).trim().to_string()),
        "WebFetch" => ("Claude wants to open a web page".to_string(), s("url")),
        "SandboxNetworkAccess" => ("A command Claude is running wants to reach the network".to_string(), s("host")),
        // A connector's tool, "mcp__claude_ai_AgentMail__send_message": "AgentMail: send message".
        other if other.starts_with("mcp__") => {
            let rest = &other[5..];
            let (server, tool) = rest.split_once("__").unwrap_or((rest, ""));
            let server = server.strip_prefix("claude_ai_").unwrap_or(server).replace('_', " ");
            let tool = request["display_name"].as_str().filter(|d| *d != other).map(str::to_string).unwrap_or_else(|| tool.replace('_', " "));
            (format!("Claude wants to use {server}: {tool}"), String::new())
        }
        other => (format!("Claude wants to use {other}"), String::new()),
    };
    // Nothing specific: the raw input, so the user still sees exactly what's asked.
    let detail = if detail.trim().is_empty() { serde_json::to_string_pretty(input).unwrap_or_default() } else { detail };
    let remember = (!remembered(request).is_empty()).then_some(if matches!(tool, "Edit" | "Write") { "Always allow this file" } else { "Always allow" });
    PermissionAsk { title, detail: visible(&detail), remember }
}

/// `*`, `?`, or `[` in a suggestion: Claude's `bash *` and `firstline *` match every
/// later command, not the one on screen.
fn rule_has_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// One exact command, safe to remember. A newline or a glob character would either be a
/// prefix rule or hide the rest of the script.
fn command_can_always(command: &str) -> bool {
    !command.is_empty() && !command.contains('\n') && !rule_has_glob(command)
}

/// An absolute path with no glob and no `..` component. `!` and `\` are rejected too:
/// they change what a shell would do with the path if the rule is ever matched loosely.
fn path_can_always(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains(['*', '?', '[', ']', '!', '\\'])
        && !path.split('/').any(|c| c == "..")
}

/// What "Always" may send back, only ever as narrow as its label:
/// - suggested allow rules that are exactly the command or file on screen (a glob, a bare
///   `Bash`, `setMode`, and `addDirectories` are dropped — accept-edits also ran `rm`);
/// - a bare mcp tool name only when it is exactly the tool being asked;
/// - otherwise one synthesized rule: the full Bash command, or `/{path}` for one absolute
///   edit/write (`/x` → `//x`, the absolute form in rule syntax).
/// No Always for a multiline command, a command with a glob character, or an unsafe path.
fn remembered(request: &serde_json::Value) -> Vec<serde_json::Value> {
    let tool = request["tool_name"].as_str().unwrap_or_default();
    let command = request["input"]["command"].as_str().unwrap_or_default();
    let path = request["input"]["file_path"].as_str().unwrap_or_default();
    // The only Edit/Write rule that matches "Always allow this file".
    let edit_rule = format!("/{path}");
    let all = request["permission_suggestions"].as_array().cloned().unwrap_or_default();
    let keep = |rule: &serde_json::Value| {
        let name = rule["toolName"].as_str().unwrap_or_default();
        let content = rule["ruleContent"].as_str().unwrap_or_default();
        if rule_has_glob(name) || rule_has_glob(content) {
            return false;
        }
        if content.trim().is_empty() {
            // A bare name approves every use. A connector tool is already one action,
            // so keep it only when it is the tool on screen.
            return name.starts_with("mcp__") && name == tool;
        }
        if name == "Bash" {
            return command_can_always(command) && content == command;
        }
        if matches!(name, "Edit" | "Write") {
            // A suggestion for a different path would still be labeled "this file".
            return path_can_always(path) && content == edit_rule;
        }
        if name.starts_with("mcp__") && name != tool {
            return false;
        }
        true
    };
    let rules: Vec<_> = all
        .iter()
        .filter(|s| s["type"] == "addRules" && s["behavior"] != "deny")
        .filter_map(|s| {
            let kept: Vec<_> = s["rules"].as_array()?.iter().filter(|r| keep(r)).cloned().collect();
            (!kept.is_empty()).then(|| {
                let mut s = s.clone();
                s["rules"] = serde_json::Value::Array(kept);
                s
            })
        })
        .collect();
    if !rules.is_empty() {
        return rules;
    }
    if tool == "Bash" && command_can_always(command) {
        return vec![serde_json::json!({
            "type": "addRules",
            "rules": [{ "toolName": "Bash", "ruleContent": command }],
            "behavior": "allow",
        })];
    }
    if let ("Edit" | "Write", Some(path)) = (tool, request["input"]["file_path"].as_str()) {
        if path_can_always(path) {
            // "//" = an absolute path in rule syntax, so "/x" is sent as "//x".
            return vec![serde_json::json!({
                "type": "addRules",
                "rules": [{ "toolName": "Edit", "ruleContent": format!("/{path}") }],
                "behavior": "allow",
            })];
        }
    }
    Vec::new()
}

/// Shown detail stops here. Past this, laying the label out on the UI thread (the popup
/// holds the keyboard) can run long enough to trip the 2s keyboard watchdog.
const DETAIL_CAP: usize = 32_000;

/// Text safe to show in a prompt: invisible and direction-changing characters (which
/// could make a command display differently from what runs) become visible codes.
/// One string, not one allocation per character, and cut off past DETAIL_CAP.
fn visible(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(DETAIL_CAP + 24));
    let mut n = 0usize;
    for c in text.chars() {
        let invisible = matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{00AD}')
            || (c.is_control() && c != '\n' && c != '\t');
        // The escape is ⟨U+XXXX⟩ (8 chars) for every code point we actually rewrite.
        let add = if invisible { 8 } else { 1 };
        if n + add > DETAIL_CAP {
            out.push_str("\n… (truncated)");
            break;
        }
        if invisible {
            use std::fmt::Write;
            let _ = write!(out, "\u{27e8}U+{:04X}\u{27e9}", c as u32);
        } else {
            out.push(c);
        }
        n += add;
    }
    out
}

/// Keep the tail of non-JSON output. `drain` panics unless the cut is a char boundary,
/// and a multibyte warning can sit on the 4096-byte mark.
fn trim_log(log: &mut String) {
    if log.len() > 8192 {
        let cut = log.floor_char_boundary(log.len() - 4096);
        log.drain(..cut);
    }
}

/// True when this AgentMail read may run without a prompt: the tool only reads, and
/// every string under a key that contains "inbox" is absent or the contactai address.
/// `list_inboxes` has no such field, so it passes. A send/delete tool does not.
fn agentmail_read_is_contactai(tool: &str, input: &serde_json::Value) -> bool {
    let Some(name) = tool.strip_prefix(AGENTMAIL) else { return false };
    AGENTMAIL_READ_ONLY.contains(&name) && inbox_only_contactai(input, false)
}

/// `from_inbox_key`: `value` is the contents of a field whose name contains "inbox"
/// (`inbox_id`, `inbox`, `inboxId`, …). Strings there must be the contactai inbox;
/// arrays under that key are held to the same rule. Other keys are only walked.
fn inbox_only_contactai(value: &serde_json::Value, from_inbox_key: bool) -> bool {
    match value {
        // A value under an inbox key has to be the address itself. An object or array
        // there is not that address, even when its nested keys don't say "inbox".
        serde_json::Value::Object(map) if !from_inbox_key => map.iter().all(|(k, v)| {
            inbox_only_contactai(v, k.to_ascii_lowercase().contains("inbox"))
        }),
        serde_json::Value::Array(items) if !from_inbox_key => items.iter().all(|v| inbox_only_contactai(v, false)),
        serde_json::Value::String(s) => !from_inbox_key || s == CONTACTAI_INBOX,
        _ => !from_inbox_key,
    }
}

/// An answer label: wraps to the card's width, Markdown rendered as markup (links clickable).
fn answer_label() -> gtk::Label {
    // max_width_chars(1): a wrapping label otherwise asks for the width of its whole
    // text and would widen the popup; this way it wraps to the card's fixed width.
    let label = gtk::Label::builder()
        .xalign(0.0)
        .yalign(0.0)
        .max_width_chars(1)
        .hexpand(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .use_markup(true)
        .build();
    label.add_css_class("omni-answer-body");
    label
}

/// The first message for a new process: the new question, preceded by the conversation
/// so far when an earlier process died mid-conversation.
fn primed(history: &[(String, String)], question: &str) -> String {
    if history.is_empty() {
        return question.to_string();
    }
    let mut out = String::from(
        "(This continues an earlier conversation; here it is so far.)\n\n",
    );
    for (q, a) in history {
        out.push_str(&format!("User: {q}\n\nAssistant: {a}\n\n"));
    }
    out.push_str(&format!("(Now the user says:)\n\n{question}"));
    out
}

/// Render Markdown, falling back to plain text if it produced invalid markup
/// (e.g. overlapping `**bold `code** x`` would otherwise blank the label).
fn set_markup_safe(label: &gtk::Label, md: &str, partial: bool) {
    let markup = markdown_to_pango(md, partial);
    if valid_markup(&markup) {
        label.set_markup(&markup);
    } else {
        label.set_text(md);
    }
}

/// Markdown to Pango markup, via pulldown-cmark (CommonMark + tables, strikethrough, task
/// lists): headings, **bold**, *italic*, ~~strike~~, `code`, code blocks, links (clickable),
/// bullet and numbered lists (nested), quotes, rules and tables. Everything is escaped.
/// `partial`: text is still being typed out, so a marker whose closing half hasn't arrived
/// yet is hidden instead of flashing raw (`**`, `` ` ``, a half-typed `[link](`).
pub fn markdown_to_pango(md: &str, partial: bool) -> String {
    use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

    let src = if partial { hide_unfinished(md) } else { md.to_string() };
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let esc = |s: &str| glib::markup_escape_text(s).to_string();
    // Start a block on a fresh line.
    fn line(out: &mut String) {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
    }

    let mut out = String::new();
    let mut lists: Vec<Option<u64>> = Vec::new(); // per nesting level: next number, or None for bullets
    let mut quote = 0usize;
    let mut in_code = false;
    let mut cell = 0usize;
    let mut head = false;
    let mut links: Vec<bool> = Vec::new(); // open links: clickable or not
    for event in Parser::new_ext(&src, opts) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if quote > 0 {
                        line(&mut out);
                        out.push_str(&format!("<span foreground=\"{QUOTE_BAR}\">\u{258e}</span> <i>"));
                    }
                }
                Tag::Heading { level, .. } => {
                    line(&mut out);
                    let size = if level <= HeadingLevel::H2 { " size=\"larger\"" } else { "" };
                    out.push_str(&format!("<span weight=\"bold\"{size}>"));
                }
                Tag::BlockQuote(_) => quote += 1,
                Tag::CodeBlock(_) => {
                    line(&mut out);
                    in_code = true;
                    out.push_str(CODE_OPEN);
                }
                Tag::List(start) => {
                    line(&mut out);
                    lists.push(start);
                }
                Tag::Item => {
                    line(&mut out);
                    let depth = lists.len().max(1);
                    out.push_str(&"    ".repeat(depth - 1));
                    let marker = match lists.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}.");
                            *n += 1;
                            m
                        }
                        _ => ["\u{2022}", "\u{25e6}", "\u{25aa}"][(depth - 1) % 3].to_string(),
                    };
                    out.push_str(&format!("  {marker} "));
                }
                Tag::Emphasis => out.push_str("<i>"),
                Tag::Strong => out.push_str("<b>"),
                Tag::Strikethrough => out.push_str("<s>"),
                // Only web and mail links are clickable: an answer can quote fetched pages,
                // and a file:// or app-scheme link shouldn't be one click from running.
                Tag::Link { ref dest_url, .. } | Tag::Image { ref dest_url, .. } => {
                    let image = matches!(tag, Tag::Image { .. });
                    if safe_link(&dest_url) {
                        out.push_str(&format!("<a href=\"{}\">", esc(&dest_url)));
                    } else {
                        out.push_str("<span underline=\"none\">");
                    }
                    links.push(safe_link(&dest_url));
                    if image {
                        out.push_str("\u{1f5bc} ");
                    }
                }
                Tag::Table(_) => line(&mut out),
                Tag::TableHead => {
                    head = true;
                    cell = 0;
                }
                Tag::TableRow => cell = 0,
                Tag::TableCell => {
                    if cell > 0 {
                        out.push_str(&format!("<span foreground=\"{QUOTE_BAR}\">  \u{2502}  </span>"));
                    }
                    cell += 1;
                    if head {
                        out.push_str("<b>");
                    }
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if quote > 0 {
                        out.push_str("</i>");
                    }
                    // Loose paragraphs get a blank line after; inside a list item, just a break.
                    out.push_str(if lists.is_empty() { "\n\n" } else { "\n" });
                }
                TagEnd::Heading(_) => out.push_str("</span>\n"),
                TagEnd::BlockQuote(_) => quote = quote.saturating_sub(1),
                TagEnd::CodeBlock => {
                    while out.ends_with('\n') {
                        out.pop();
                    }
                    out.push_str("</span>\n");
                    if lists.is_empty() {
                        out.push('\n');
                    }
                    in_code = false;
                }
                TagEnd::List(_) => {
                    lists.pop();
                    line(&mut out);
                    if lists.is_empty() {
                        out.push('\n');
                    }
                }
                TagEnd::Item => line(&mut out),
                TagEnd::Emphasis => out.push_str("</i>"),
                TagEnd::Strong => out.push_str("</b>"),
                TagEnd::Strikethrough => out.push_str("</s>"),
                TagEnd::Link | TagEnd::Image => out.push_str(if links.pop().unwrap_or(false) { "</a>" } else { "</span>" }),
                TagEnd::TableCell => {
                    if head {
                        out.push_str("</b>");
                    }
                }
                TagEnd::TableHead => {
                    head = false;
                    out.push('\n');
                }
                TagEnd::TableRow => out.push('\n'),
                TagEnd::Table => out.push('\n'),
                _ => {}
            },
            Event::Text(text) => out.push_str(&esc(&text)),
            Event::Code(code) => out.push_str(&format!("{CODE_OPEN}{}</span>", esc(&code))),
            Event::InlineMath(m) | Event::DisplayMath(m) => out.push_str(&esc(&m)),
            Event::Html(h) | Event::InlineHtml(h) => out.push_str(&esc(&h)),
            Event::SoftBreak => out.push(if in_code { '\n' } else { ' ' }),
            Event::HardBreak => out.push('\n'),
            Event::Rule => {
                line(&mut out);
                out.push_str(&format!("<span foreground=\"{QUOTE_BAR}\">\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}</span>\n"));
            }
            Event::TaskListMarker(done) => out.push_str(if done { "\u{2611} " } else { "\u{2610} " }),
            Event::FootnoteReference(r) => out.push_str(&format!("[{}]", esc(&r))),
        }
    }
    out.trim_end().to_string()
}

/// Markup GtkLabel will accept. Links (`<a href>`) are GtkLabel's own addition, unknown to
/// Pango's parser, so they're taken out before asking Pango about the rest.
fn valid_markup(markup: &str) -> bool {
    let mut plain = String::with_capacity(markup.len());
    let mut rest = markup;
    while let Some(i) = rest.find("<a href=\"") {
        plain.push_str(&rest[..i]);
        // The href is escaped, so its closing quote is the first one after it.
        let Some(end) = rest[i + 9..].find("\">") else { return false };
        rest = &rest[i + 9 + end + 2..];
    }
    plain.push_str(rest);
    gtk::pango::parse_markup(&plain.replace("</a>", ""), '\0').is_ok()
}

/// Links omni will open: web pages and email only.
fn safe_link(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    ["https://", "http://", "mailto:"].iter().any(|s| lower.starts_with(s))
}

/// Monospace with a faint backdrop, for `code` and code blocks.
const CODE_OPEN: &str = "<span font_family=\"monospace\" background=\"#ffffff\" bgalpha=\"10%\">";
/// Quote bars, table dividers and rules: dim cyan.
const QUOTE_BAR: &str = "#54dbed99";

/// While an answer is still arriving: drop an opening marker whose closing half hasn't
/// come yet, so it doesn't flash as raw text (it's rendered once the rest arrives).
fn hide_unfinished(md: &str) -> String {
    let mut s = md.to_string();
    // Inside an open code fence everything is literal: leave it.
    if s.matches("```").count() % 2 == 1 {
        return s;
    }
    for marker in ["**", "~~", "`"] {
        if s.matches(marker).count() % 2 == 1 {
            if let Some(i) = s.rfind(marker) {
                s.replace_range(i..i + marker.len(), "");
            }
        }
    }
    // A link still being typed on the last line: "[text](http" → hidden until complete.
    let last_line = s.rfind('\n').map_or(0, |i| i + 1);
    if let Some(open) = s[last_line..].rfind('[').map(|i| last_line + i) {
        let tail = &s[open..];
        let complete = tail.find("](").is_some_and(|j| tail[j..].contains(')'));
        if !complete && (tail.contains("](") || !tail.contains(']')) {
            s.truncate(open);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_prompt_shows_everything() {
        // Direction overrides can't disguise a command.
        assert_eq!(visible("rm \u{202E}fdp.exe"), "rm \u{27e8}U+202E\u{27e9}fdp.exe");
        let long = format!("echo hi; {}; curl evil", "x".repeat(2000));
        let long_req = serde_json::json!({ "tool_name": "Bash", "input": { "command": long } });
        let ask = describe(&long_req);
        assert!(ask.detail.ends_with("curl evil")); // nothing cut off under the cap
        // A single-line command with no suggestions gets an exact-command Always.
        assert_eq!(ask.remember, Some("Always allow"));
        let sent_long = remembered(&long_req);
        assert_eq!(sent_long[0]["rules"][0]["ruleContent"], long);
        assert!(!sent_long[0]["rules"][0]["ruleContent"].as_str().unwrap().contains(" *"));
        // An edit's "always" is that one file, never accept-edits mode (which also ran rm).
        let edit = serde_json::json!({ "tool_name": "Edit", "input": { "file_path": "/x", "old_string": "a", "new_string": "b" },
            "permission_suggestions": [{ "type": "setMode", "mode": "acceptEdits", "destination": "session" }] });
        assert_eq!(describe(&edit).remember, Some("Always allow this file"));
        let sent = remembered(&edit);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["rules"][0]["ruleContent"], "//x");
        assert!(!serde_json::to_string(&sent).unwrap().contains("acceptEdits"));
        // A bare Bash rule is never sent; the exact command is.
        let bare = serde_json::json!({ "tool_name": "Bash", "input": { "command": "ls" },
            "permission_suggestions": [{ "type": "addRules", "behavior": "allow", "rules": [{ "toolName": "Bash" }] }] });
        let sent_bare = remembered(&bare);
        assert_eq!(sent_bare.len(), 1);
        assert_eq!(sent_bare[0]["rules"][0]["toolName"], "Bash");
        assert_eq!(sent_bare[0]["rules"][0]["ruleContent"], "ls");
        assert_eq!(describe(&bare).remember, Some("Always allow"));
        // Fields the prompt used to hide, and the sandbox-disable flag which stays ignored.
        let bg = serde_json::json!({ "tool_name": "Bash", "input": { "command": "sleep 9", "run_in_background": true, "dangerouslyDisableSandbox": true } });
        assert!(describe(&bg).detail.contains("runs in the background"));
        assert!(!describe(&bg).detail.contains("dangerouslyDisableSandbox"));
        let all = serde_json::json!({ "tool_name": "Edit", "input": { "file_path": "/x", "old_string": "a", "new_string": "b", "replace_all": true } });
        assert!(describe(&all).detail.contains("replaces every match"));
        // Extra directories are never granted by "Always".
        let mail = serde_json::json!({ "tool_name": "mcp__claude_ai_AgentMail__send_message", "input": { "to": "a@b.c" } });
        assert_eq!(describe(&mail).title, "Claude wants to use AgentMail: send message");
        assert!(describe(&mail).detail.contains("a@b.c")); // the input is shown
        let dirs = serde_json::json!({ "tool_name": "Read", "input": {}, "permission_suggestions": [{ "type": "addDirectories", "directories": ["/"] }] });
        assert!(describe(&dirs).remember.is_none() && remembered(&dirs).is_empty());
    }

    #[test]
    fn remembered_rules_stay_as_narrow_as_the_prompt() {
        let multi = serde_json::json!({
            "tool_name": "Bash",
            "input": { "command": "bash\nrm x" },
            "permission_suggestions": [{ "type": "addRules", "behavior": "allow", "rules": [{ "toolName": "Bash", "ruleContent": "bash *" }] }],
        });
        assert!(remembered(&multi).is_empty());
        assert!(describe(&multi).remember.is_none());

        let cmd = "bash -c 'echo hi'";
        let star = serde_json::json!({
            "tool_name": "Bash",
            "input": { "command": cmd },
            "permission_suggestions": [{ "type": "addRules", "behavior": "allow", "rules": [{ "toolName": "Bash", "ruleContent": "bash *" }] }],
        });
        let sent = remembered(&star);
        assert_eq!(sent[0]["rules"][0]["ruleContent"], cmd);
        assert!(!serde_json::to_string(&sent).unwrap().contains("bash *"));
        assert!(!sent[0]["rules"][0]["ruleContent"].as_str().unwrap().contains(" *"));

        assert!(remembered(&serde_json::json!({ "tool_name": "Edit", "input": { "file_path": "/home/u/**" } })).is_empty());
        assert!(remembered(&serde_json::json!({ "tool_name": "Write", "input": { "file_path": "/home/u/../.ssh/x" } })).is_empty());
        assert_eq!(
            remembered(&serde_json::json!({ "tool_name": "Edit", "input": { "file_path": "/x" } }))[0]["rules"][0]["ruleContent"],
            "//x"
        );

        let asking = format!("{AGENTMAIL}get_message");
        let other = format!("{AGENTMAIL}list_inboxes");
        let wrong = serde_json::json!({
            "tool_name": asking,
            "input": {},
            "permission_suggestions": [{ "type": "addRules", "behavior": "allow", "rules": [{ "toolName": other }] }],
        });
        assert!(remembered(&wrong).is_empty());
        let same = serde_json::json!({
            "tool_name": asking,
            "input": {},
            "permission_suggestions": [{ "type": "addRules", "behavior": "allow", "rules": [{ "toolName": asking }] }],
        });
        assert_eq!(remembered(&same)[0]["rules"][0]["toolName"], asking);

        // "Always allow this file" must not keep a suggestion for a different path.
        let sneaky = serde_json::json!({
            "tool_name": "Edit",
            "input": { "file_path": "/tmp/notes" },
            "permission_suggestions": [{ "type": "addRules", "behavior": "allow", "rules": [
                { "toolName": "Edit", "ruleContent": "//home/u/.bashrc" }
            ] }],
        });
        assert_eq!(remembered(&sneaky)[0]["rules"][0]["ruleContent"], "//tmp/notes");
        assert!(!serde_json::to_string(&remembered(&sneaky)).unwrap().contains(".bashrc"));
    }

    #[test]
    fn agentmail_reads_only_the_contactai_inbox() {
        let list = format!("{AGENTMAIL}list_inboxes");
        assert!(agentmail_read_is_contactai(&list, &serde_json::json!({})));
        let get = format!("{AGENTMAIL}get_message");
        assert!(agentmail_read_is_contactai(&get, &serde_json::json!({ "inbox_id": CONTACTAI_INBOX })));
        assert!(agentmail_read_is_contactai(&get, &serde_json::json!({})));
        assert!(!agentmail_read_is_contactai(&get, &serde_json::json!({ "inbox": "other@example.com" })));
        assert!(!agentmail_read_is_contactai(&get, &serde_json::json!({ "inboxId": "other@example.com" })));
        assert!(!agentmail_read_is_contactai(&get, &serde_json::json!({ "nested": { "inbox_id": "other@example.com" } })));
        assert!(!agentmail_read_is_contactai(&get, &serde_json::json!({ "inbox_id": { "id": "other@example.com" } })));
        assert!(!agentmail_read_is_contactai(&get, &serde_json::json!({ "inbox_id": ["other@example.com"] })));
        assert!(!agentmail_read_is_contactai(&get, &serde_json::json!({ "inbox_id": null })));
        let del = format!("{AGENTMAIL}delete_inbox");
        assert!(!agentmail_read_is_contactai(&del, &serde_json::json!({ "inbox_id": CONTACTAI_INBOX })));
        assert!(!agentmail_read_is_contactai("Bash", &serde_json::json!({})));
    }

    #[test]
    fn visible_caps_a_huge_prompt() {
        let huge = "z".repeat(50_000);
        let ask = describe(&serde_json::json!({ "tool_name": "Write", "input": { "file_path": "/tmp/a", "content": huge } }));
        assert!(ask.detail.contains("(truncated)"));
        assert!(ask.detail.chars().count() < 40_000);
        assert!(!ask.detail.contains(&huge));
    }

    #[test]
    fn log_trim_stays_on_a_char_boundary() {
        // 'x' then two-byte 'é's, so the 4096-from-the-end cut lands inside a character.
        let mut log = String::from("x");
        log.push_str(&"é".repeat(4500));
        log.push('y');
        assert!(log.len() > 8192);
        assert!(!log.is_char_boundary(log.len() - 4096));
        trim_log(&mut log);
        assert!(log.len() <= 8192);
        assert!(log.ends_with('y'));
    }

    #[test]
    fn secrets_cover_the_local_stores() {
        for p in [
            "~/.ssh", "~/.claude", "~/.claude/.credentials.json", "~/.claude.json", "~/.claude.json.backup",
            "~/.config/Cursor", "~/.cursor", "~/.config/Code", "~/.vscode", "~/.vscode-oss",
            "~/.cache/huggingface", "~/.local/share/kwalletd", "~/.local/share/keyrings", "~/.var/app",
            "~/.zsh_history", "~/.bash_history", "~/.python_history", "~/.node_repl_history", "~/.lesshst",
            "~/.wget-hsts", "~/.mysql_history", "~/.psql_history", "~/.sqlite_history",
            "~/.pki", "~/.config/evolution", "~/.config/microsoft-edge", "~/.config/vivaldi", "~/.config/opera",
            "~/.config/librewolf", "~/.local/share/TelegramDesktop", "~/.docker", "~/.docker/config.json",
            "~/.config/protonmail", "~/.config/protonvpn", "~/.config/Slack", "~/.config/Element", "~/.config/Bitwarden",
            "~/.config/wireguard", "~/.gemini", "~/.codex", "~/.config/Proton", "~/.config/Proton Mail",
        ] {
            assert!(SECRETS.contains(&p), "{p}");
        }
        assert!(SECRETS.iter().all(|p| *p != "~" && *p != "~/" && *p != "$HOME" && !p.is_empty()));
    }

    #[test]
    fn primed_message_carries_the_transcript() {
        assert_eq!(primed(&[], "hi"), "hi");
        let m = primed(&[("What's 2+2?".into(), "4".into())], "And times 3?");
        assert!(m.contains("User: What's 2+2?") && m.contains("Assistant: 4") && m.ends_with("And times 3?"));
    }

    #[test]
    fn markdown() {
        let md = |s: &str| markdown_to_pango(s, false);
        assert_eq!(md("a **b** and `c<d`"), format!("a <b>b</b> and {CODE_OPEN}c&lt;d</span>"));
        assert_eq!(md("2 * 3"), "2 * 3");
        assert_eq!(md("- one\n- two"), "  \u{2022} one\n  \u{2022} two");
        assert_eq!(md("1. one\n2. two"), "  1. one\n  2. two");
        assert_eq!(md("- a\n  - b"), "  \u{2022} a\n      \u{25e6} b");
        assert_eq!(md("[Rust](https://rust-lang.org?a=1&b=2)"), "<a href=\"https://rust-lang.org?a=1&amp;b=2\">Rust</a>");
        assert_eq!(md("~~old~~ *new*"), "<s>old</s> <i>new</i>");
        // Non-web links show their text but aren't clickable.
        assert_eq!(md("[keys](file:///home/u/.ssh/id_rsa)"), "<span underline=\"none\">keys</span>");
        assert_eq!(md("[x](javascript:alert(1))"), "<span underline=\"none\">x</span>");
        // Every output must be valid Pango markup.
        for s in ["1. [torvalds/linux](https://github.com/torvalds/linux) — **kernel**", "# Title\n\nSome **bold** text.\n\n> quoted\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n---\n\n```rust\nfn x() {}\n```", "- [x] done\n- [ ] todo", "<script>&</script>"] {
            assert!(valid_markup(&md(s)), "{s}");
        }
    }

    #[test]
    fn half_arrived_markers_are_hidden() {
        let partial = |s: &str| markdown_to_pango(s, true);
        assert_eq!(partial("this is **impor"), "this is impor");
        assert_eq!(partial("see [the docs](https://exa"), "see");
        assert_eq!(partial("2 * 3"), "2 * 3");
        assert_eq!(markdown_to_pango("this is **impor", false), "this is **impor");
    }
}
