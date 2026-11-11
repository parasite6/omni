# ⌘ omni

### One glass popup for your apps, files, folders, services, the web and Claude (OpenCode coming soon).

**`Super`＋`Ctrl`＋`Enter`** → type → **`Enter`**

*A GNOME-style launcher for Hyprland · written in Rust with GTK4 and gtk4-layer-shell*

---

## Jump to what you need

| I want to… | Go to |
|---|---|
| 🚀 **get it running** | [Install](#install) |
| 🎹 **remember the keys** | [Cheat sheet](#cheat-sheet) |
| 🔎 **know what I can type** | [Try typing…](#try-typing) |
| 📋 **understand the results list** | [Results](#results) |
| 🧮 **do quick maths** | [Calculator](#calculator) |
| 🚪 **launch app actions (private windows, Tor…)** | [App actions](#app-actions) |
| 📁 **find files and folders** | [Files & folders](#files--folders) |
| ▶️ **run a program or command** | [Programs & commands](#programs--commands) |
| 🌐 **search the web (even privately)** | [Web search](#web-search) |
| 🤖 **ask Claude without leaving the popup** | [Ask Claude](#ask-claude) |
| 🎨 **change how it looks or behaves** | [Customize](#customize) |
| 🔧 **see how it works, or fix it** | [Under the hood](#under-the-hood) |
| 📜 **read what changed and why** | [Status log](#status-log) |

---

## Install

```bash
# 1. build dependencies (Fedora)
sudo dnf install gtk4-devel gtk4-layer-shell-devel tinysparql-devel cef   # + Rust

# 2. build, install and start it
~/Documents/github/omni/install.sh

# 3. press  Super + Ctrl + Enter
```

That's it. The same command **updates** omni later.

**What `install.sh` does:** builds a release (always into this folder's `target/`, even if `CARGO_TARGET_DIR` is set), replaces the installed files atomically, restarts `omni` and checks that it answers.

**Needs:** `gtk4-devel`, `gtk4-layer-shell-devel`, `tinysparql-devel`, `cef`, Rust.

Built-in web search currently uses Brave Search in a GTK view backed by Chromium Embedded Framework (CEF). Select the engine in Settings, then use `Ctrl`＋`Enter` or the web result. Back / `Esc` returns to the launcher. Queries open Brave’s `/ask` AI view. An initial page filter hides the navigation header and history sidebar while retaining answers, sources, and the follow-up prompt. Each submitted search creates an isolated in-memory browser session. Every fresh session starts with usage metrics, anonymous local results, and SafeSearch off. Omni seeds Brave’s `safesearch=off` and `useLocation=0` cookies before navigation, and sets `app.usageMetricsOptOut=true` in Brave’s localStorage before its page scripts run. These preferences remain in memory with the session. Follow-ups share that session until Back, Escape, or popup close destroys it. Cookies, browser history, localStorage, IndexedDB, caches, and service workers are not reused for the next search. Chromium runs in its own process (`omni-browser --omni-host`), not inside `omni`. It starts on the first search and exits **10 s after the last Brave page closes**, so it takes no memory while you're not searching. The next search starts it again (about 1.2 s from `Enter` to the page). CEF installation metadata lives in a private temporary runtime directory, removed when that process exits. Directories left behind by a killed process are removed the next time it starts. This controls local storage; Brave’s own server-side retention is separate. The system CEF library must support API 14600; Fedora’s `cef` package is used by default. Set `OMNI_CEF_DIR` at build time for another CEF installation.

The search header and launcher result use the bundled [official Brave favicon](https://cdn.search.brave.com/serp/v3/_app/immutable/assets/favicon.CVXAaejQ.svg). Page chrome and native and Brave-drawn scrollbar tracks are hidden at document creation; wheel and trackpad scrolling remain available. Searches show a spinner on a blank pane in the page's colour, then crossfade into Brave's page on its first frame, so the only loading animation is Brave's own; reduced-motion preferences are respected. Pages reach omni as frames over a socket and input goes back the same way. The embedded browser disables QUIC so Brave requests use HTTPS over HTTP/2 where available; conversation creation was intermittently rejected with HTTP 400 over HTTP/3 in controlled CEF and Brave tests. Set `OMNI_SEARCH_DIAGNOSTICS=1` for opt-in endpoint status and streaming metadata without query strings, request bodies, or answer text. Set `OMNI_SEARCH_TIMING=1` to log startup and document-load timings without logging queries; `cargo run --example chromium-load-timing -- --local` runs a local sequential-request benchmark, and `--prewarm` starts the Chromium process before the first search.

> The keybind and autostart are wired up through Hyprland / ML4W: see [How it's hooked into Hyprland / ML4W](#how-its-hooked-into-hyprland--ml4w).

---

## Cheat sheet

### Keys

| Key | What it does |
|---|---|
| **type** | search apps (matched like GNOME: word starts, then substrings; loose matching for typos), files, folders and services |
| `~/Folder: name` | search only inside that folder (see the [status log](#status-log)) |
| `↑` / `↓` or scroll wheel | move through results (the wheel scrolls the text while a Claude answer shows) |
| `Enter` | open the selected result |
| `Ctrl`＋`Enter` | search the web for what you typed |
| `Shift`＋`Enter` | ask Claude, answer streams into the popup; then `Enter` / `Shift`＋`Enter` sends follow-ups |
| `Ctrl`＋`T` | toggle **Contents** (also match text inside files) |
| `Ctrl`＋`F` | **filter** dropdown: All / Apps / Files / Folders / Services, any combination |
| `Esc` | back out one level (menu, prompt, status, conversation → search), then close |
| click outside the card | close |

### Right-click menus

Right-click the row, **or** press `Menu` / `Shift`＋`F10` on the selected one.

| On a… | You get |
|---|---|
| 📄 **file** | **Open** and **Open with** (files only, not folders) |
| 🧩 **app** | **Open** and the app's **Actions** (New window, New private window, …) |
| 🌐 **"Search the web" row** | **Search**, or search in a **New private window** / Brave's **New private window with Tor** |
| ⚙️ **service** | **Status**, then Start / Stop / Restart / Enable / Disable as they fit (`Enter` only selects) |

### Query tricks

| Type this | Get this |
|---|---|
| *(nothing)* | your 5 most recent apps, files and folders |
| `frfox` | Firefox — typos are forgiven, for apps |
| `brave tor` · `incognito` · `private window` | the matching app **action** as its own row |
| `~/Music: NF` · `/etc: hosts` · `~/.config/hypr: launcher` | files and folders **only inside that folder** |
| `~/Music:` | the folder itself, to open |
| `bluetooth` (3+ letters) | matching systemd **services**, with state |
| `6x5` · `2^10` · `200*15%` | the **calculator** card (`Enter` copies the result) |
| `img red terminal` | [**image search**](#image-search): pictures ranked by what's in them |
| anything, then `Ctrl`＋`Enter` | web search |
| anything, then `Shift`＋`Enter` | ask Claude |

### Ask Claude: quick facts

| | |
|---|---|
| **Default** | Haiku 5.5 at Medium effort; the header chip switches model and effort |
| **Models** | Haiku / Sonnet / Opus 5.5, Fable 5.1 |
| **Effort** | Low · Medium · High · Extra · Max |
| **Temporary** | nothing is written to disk; the conversation ends on `Esc` back to search or when omni closes |
| **Permission prompts** | **Deny** (focused) · **Allow once** · **Always allow** |
| **Secrets** | unreadable (SSH, GPG, cloud credentials, keyrings, browser profiles, …) |

### Esc ladder

`menu` → `prompt` → `status` → `conversation` → `search` → **close**

---

## Try typing…

Everything shares **one list**, so you never pick a mode first.

| Kind | Shows up when you type… |
|---|---|
| 🧩 **Apps** | an app name — word starts first, then substrings, then loose "did you mean" matching |
| 🚪 **App actions** | an app plus an action: `brave tor`, `brave n`, `incognito` |
| ⚙️ **Services** | a service name, 3+ letters: `bluetooth` |
| ▶️ **Commands** | a program in your `$PATH`: `python`, `git`, `node` |
| 📄 **Files** | part of a file name |
| 📁 **Folders** | part of a folder name |
| 📝 **File contents** | words inside files (turn on with `Ctrl`＋`T`) |
| 🧮 **Calculator** | a complete calculation: `6 × 5` |
| 🌐 **Web** | anything — "Search the web for …" is always offered |
| 🤖 **Claude** | anything — "Ask Claude …" is always offered |

---

## Results

### Nothing typed: recents

The **5 most recent** apps, files and folders, newest first.

- **Apps** come from omni's own launch history (apps opened some other way aren't seen).
- **Files and folders** come from GTK's shared recent list (`~/.local/share/recently-used.xbel`, also written by GNOME apps and file pickers), skipping ones that no longer exist. Anything opened from omni is added to that list.
- Never web queries, Claude prompts or file contents.

### Result order (fixed)

| # | Kind | Up to |
|---|---|---|
| 1 | App actions | 3 |
| 2 | Apps | 6 |
| 3 | Services | 3 |
| 4 | Commands (programs in `$PATH`) | 3 |
| 5 | Files | 5 |
| 6 | Folders | 3 |
| 7 | File contents | 4 |
| 8 | "Search the web for …" | 1 |
| 9 | "Ask Claude …" | 1 |

- A **narrowed filter** allows up to **8 per kind**.
- The web row is first only when nothing above it matches.
- Apps you launch often rank higher among apps.
- A matching home folder (`~/Documents`, …) is listed above the commands and files.
- Commands count as **Files** for the filter.

### The filter (`Ctrl`＋`F`)

A dropdown next to **Contents** with **All / Apps / Files / Folders / Services**. The kinds are independent checkmarks, so any combination works ("Apps + Folders").

- From **All**, picking a kind shows only that kind. After that, each kind toggles on or off. Unchecking the last kind goes back to **All**.
- Applies to recents and to search results. App actions count as Apps, and Contents matches count as Files. The web, Claude and calculator rows are never filtered.
- The choice is remembered in `~/.local/state/omni/filter`.

### Folder-scoped search: `<folder>: <name>`

Searches only inside that folder and its subfolders. Examples: `~/Music: NF`, `/etc: hosts`, `~/.config/hypr: launcher`.

- The folder part starts with `~` or `/`. A colon and a space separate it from the name.
- The folder is matched case-insensitively (`~/music` works), and symlinked folders resolve to their real location.
- Typing only `~/Music:` lists the folder itself, to open.
- Scoped searches show files and folders only (no apps, web or Claude rows) and respect the filter.

### Loose app matching (typos)

**Apps only.** Files and folders still match exactly.

When no app matches by name as typed, every word may instead be loosely similar to a word in an app's name (or, more strictly, its keywords and program name). There's no list of known typos: it's general similarity.

| You type | You get |
|---|---|
| `frfox` | Firefox |
| `ptormaster` | Portmaster |
| `brvae` | Brave |
| `obsidain` | Obsidian |
| `libreofice` | LibreOffice |
| `nautlus` | Files |

- **What counts as close.** Either the word is a few edits away (about a third of its letters, at least 1; swapped neighbours count as one edit), or its letters appear in order in the name word without being scattered ("frfox" in "firefox"). A partly typed word is compared with the start of the name word.
- **Noise control.** It only runs when exact matching finds no app by name. Weak resemblances are ignored, and guesses under half the best guess's score are dropped. Non-app queries like "invoices" or "report2024" show no apps.

---

## Calculator

When the whole query is a **complete calculation** of numbers and symbols, the result replaces the whole list: the expression small on top (`6 × 5 =`), the result large underneath.

- `Enter` copies the result.
- `Ctrl`＋`Enter` / `Shift`＋`Enter` still search the web / ask Claude.
- As soon as the query stops being a complete calculation (backspace `6x5` to `6x`), the normal results are back.

| Works | Notes |
|---|---|
| `6x5` · `3 × 4` · `10÷4` · `2^10` · `(3+3)*2` · `2(3+4)` · `200*15%` | `x` / `×` / `*` multiply, `÷` / `/` divide, `%` after a number divides it by 100 |

| Gets no row | Why |
|---|---|
| `4x`, `3+` | unfinished |
| a bare `42`, `-5` | not a calculation |
| `15% of 80` | words aren't allowed |
| dividing by zero | no result |

Results show up to **10 significant digits**.

---

## App actions

The actions an app's `.desktop` file declares (New window, New private window, …), read through GIO like GNOME does.

- An action gets its own row **only when the query names it**: `brave tor`, `incognito`, `private window`, `brave n`.
- Plain `brave` shows just Brave, and generic words alone (`new`, `window`) show no actions, so they never flood the list.
- Without the app's name, the query needs a distinctive whole word (`incognito`, `tor`).
- Ranked by match, then launches from omni, then the default browser wins ties.
- Right-click an app (or `Menu` / `Shift`＋`F10`) for **Open** plus its actions.

### `TWEAKS`: fixing launchers

`TWEAKS` in `apps.rs` fixes up launchers that don't match the app's own menu, matched by `.desktop` id (a browser's web apps share its executable).

**Brave:**
- actions renamed to its menu's wording (New window / New private window; "incognito" still finds it)
- plus **New private window with Tor** (`--tor`, supported since Brave 1.29; its launcher file leaves it out, though Brave's own menu has it)

---

## Files & folders

### Right-click a file

(or `Menu` / `Shift`＋`F10` on the selected one)

- **Open** with its default app.
- **Open with** any app registered for its type (recommended ones first, up to 10).
- The type is guessed from the name, so opening the menu never touches the disk.
- Folders, apps and the web/Claude rows have no *file* menu (apps and the web row have their own right-click menus, see the [cheat sheet](#cheat-sheet)).
- The menu drops open (fades in while sliding down a few pixels, CSS only: resizing an open popover makes Wayland dismiss it) with its top-left corner just right of and below the pointer; from the keyboard it drops from the row.

### What's searched

- **File and folder names** are indexed everywhere, except *inside* folders that tools manage: package stores (`node_modules`, `.npm`, `.yarn`, …), editor state (`.vscode`, `.cursor`, `.idea`), version control (`.git`, …), toolchains (`.rustup`, `.cargo`, `.gradle`, `.m2`), tool caches, Trash, `flatpak` and `containers`. The folders themselves are still listed. Your dotfiles (`~/.config`, `~/.local`) and project environments (`.venv`) are indexed. The list is `SKIP_INSIDE` in `files.rs`.
- **Contents** only in your home folder and drives.

### Contents search (`Ctrl`＋`T`)

Toggle next to the search bar. **Off by default**, remembered in `~/.local/state/omni/contents`.

- Also lists files whose text matches, from LocalSearch's full-text index.
- Only this search waits for a **0.7 s pause** in typing (LocalSearch is slow); it runs off the UI thread and results slot in when they arrive.
- If you moved the selection with the arrow keys it stays on the row you picked; otherwise the top row stays selected.

### Ranking

1. Whole name / name start / word start / anywhere.
2. Your home folders (`~/Documents`, `~/Downloads`, … from XDG user-dirs) first (a matching one is listed above the file results).
3. Then things inside them, then the rest of home, shallow paths first.
4. Extension matches ("pdf") favour that file type.
5. Hidden and dependency/build folders (`site-packages`, `target`, `Build`, `vcpkg`, `.venv`, …) and what's in them pushed down. (`node_modules`, `.git`, `Trash` and the other `SKIP_INSIDE` folders are listed themselves, but their contents aren't indexed.)

One- and two-letter queries match name starts only. Every shown path is checked, so deleted files never appear. New files are added from LocalSearch's change events as they arrive.

*(How the index is built and why: see [The file-name index](#the-file-name-index).)*

---

## Programs & commands

**Opening an executable file runs it, after you confirm.** That covers a result row, `Enter` or a right-click **Open**.

- **What counts as a program:** a regular file you're allowed to execute that is either a compiled binary (ELF, AppImages included) or a script starting with `#!`. The exec bit alone isn't enough, so a text file marked executable still opens in its app.
- **The confirmation:** a dimmed prompt asks "Run *name*? It's a program: it runs with your permissions." and shows the full path. **Cancel** is focused, so a stray `Enter` doesn't run anything. **Run** does nothing for the first half second after the prompt appears, so a click or key that was already on its way can't approve it. `Esc` cancels.
- **How it runs:** from its own folder (programs often look for files beside themselves), without a terminal, as you. It's added to the recent list. If it can't start, a notification says why.
- **Turning it off:** Settings → **Run programs**. Off: executables open in their default app like any other file. Remembered in `~/.config/omni/run-programs` (on by default).

### Commands

Programs directly inside a `$PATH` folder get their own group: up to **3**, above the files.

- The one your shell would run for that exact name comes first (`python` → `/usr/bin/python`), then other matches. Version numbers compare as numbers, highest first: `python3.15`, `python3.14`, `python3.13`.
- `$PATH` folders inside skipped folders (`~/.cargo/bin`) are still indexed.
- Relative `$PATH` entries are ignored.
- A program installed after the last index walk shows as a plain file until the next walk (every 30 minutes).

---

## Web search

Use `Ctrl`＋`Enter` or activate the web result to open **Brave Search inside Omni**, using Chromium Embedded Framework (CEF). Settings → Search engine currently offers Brave; additional engines need their own AI overview filters. The initial page filter hides Brave’s navigation header and history sidebar while retaining answers, source controls, and the follow-up prompt. Back or `Esc` returns to the launcher.

Private-window actions below continue to use your external browser and its private search engine.

### Private searching

Right-click the web row (or `Menu` / `Shift`＋`F10`) to search in a:

| Option | For | How |
|---|---|---|
| **Private window** | Chromium-family browsers | `--incognito` |
| **Private window with Tor** | Brave | `--tor` (the window says "Untitled" for a few seconds while Tor connects, then loads the search) |

- These use Brave's private-window search engine (`brave.default_private_search_provider_data`), which Brave also uses with Tor.
- The query is inside the percent-encoded URL, never a separate argument.
- **Firefox gets no private items:** omni searches it through its own engine, which can't be pointed at a private window.

---

## Image search

Type `img`, a space, and a description (`img red terminal`, `img cat on a couch`). Results are your pictures ranked by what's *in* them, with thumbnails; `Enter` and right-click work like any file.

- **Model:** [embeddinggemma-2](https://huggingface.co/google/embeddinggemma-2) (ONNX fp16, about 900 MB, downloaded on the first crawl to `~/.local/share/omni/models`). It runs in `omni-embed.py` on the GPU, from its own venv (`~/.local/share/omni/embed-venv`, made by `install.sh` with `uv`; onnxruntime-gpu brings its own CUDA and cuDNN).
- **Querying loads only the text side** (~540 MB). Typing `img` starts it (about 2 s), each query then takes a few ms, and it exits **10 s after the last query**.
- **Indexing** loads the vision encoder too, embeds only new or changed images, then exits. A crawl with nothing new never loads the model. About 0.2 s per image: a couple hundred take ~30 s.
- **When:** quietly 10 s after omni starts, then hourly. Settings → **Image search** turns that off; **Index now** runs it; **Add folder…** adds a folder and indexes it right away.
- **Folders:** `~/Pictures` by default; the list is `~/.config/omni/image-folders` (one per line). Hidden folders are skipped. PNG, JPEG, WebP, GIF (first frame), BMP.
- **Index:** `~/.local/share/omni/images/index.npz` (paths and vectors); thumbnails in `~/.cache/omni/thumbs`.

---

## Ask Claude

Press `Shift`＋`Enter` on anything you typed. omni runs the **Claude Code CLI** (your normal Claude Code login and usage) as a **temporary conversation** in the popup.

### The basics

- **Model:** Haiku 5.5 at Medium effort by default. The chip in the answer header switches model (Haiku / Sonnet / Opus 5.5, Fable 5.1) and effort (Low … Max), remembered between opens.
- **Follow-ups:** the search bar becomes the reply box; long conversations are fine.
- **Answers** render as Markdown. Links (http/https/mailto only) ask "This will close Omni, continue?" before opening. Copy buttons instead of text selection.
- **Continue in Claude** opens a chat in the Claude app with the question pre-filled (the app never auto-sends prompts that arrive by link, by design).
- While Claude starts up, a ball bounces around the answer panel; text is revealed at a steady pace slightly behind the stream so it never stalls or jumps.

### Temporary on purpose

- `--no-session-persistence`: nothing is written to disk.
- The conversation ends when you press `Esc` back to search or omni closes. **Nothing survives omni closing.**
- The question is sent on stdin, never as an argument.
- A turn stops after **90–360 s of silence** (by effort), **20 minutes at most**; the timer pauses while a permission prompt waits.

### Tools and safety

- **Tools:** web search/fetch, read/glob/grep, Bash (in a bubblewrap sandbox), edit and write, plus your claude.ai connectors (AgentMail, Calendar, …).
- **No local settings, plugins or hooks** (`--setting-sources ""`).
- Anything that changes something asks on a dimmed prompt: **Deny** / **Allow once** / **Always allow** (this conversation only, and only for the exact command or file on screen).
- **Secrets are unreadable** (`~/.ssh`, `~/.claude`, keyrings, browser profiles, shell histories, `~/.var/app`, …).
- Shell commands can't write `~/.config`, shell startup files or PATH folders (things that later run outside the sandbox).
- Your agent instructions (`src/agent_prompt.md`) are appended to omni's system prompt.

The full permission, sandbox and connector story is in the [status log](#status-log) (see *Claude tools and permission prompts*, *permission system audit*, *claude.ai connectors*, *agent instructions*, and *audit follow-up*).

### Models and effort

| | |
|---|---|
| Models | Haiku 5.5, Sonnet 5.5, Opus 5.5, Fable 5.1 |
| Effort | Low, Medium, High, Extra (= `xhigh`), Max |
| Default | Haiku 5.5 · Medium |
| Remembered in | `~/.local/state/omni/claude` |
| Timeout by effort | 90 s for Low and Medium, then 150, 240 and 360 s |

---

## Customize

### Look and feel

- **Colours:** omni's own cyan `#54dbed` on black `#07090b` (`@omni_cyan`, `@omni_black` in `style.css`). It no longer follows the wallpaper palette.
- **Search bar rim:** a living cyan/black gradient that slowly morphs around the bar (`ring.rs`).
- **The glass:** the card uses an opaque dark-metal surface. It does not capture the screen behind it, so opening the launcher never waits for the compositor's screenshot path.
- **Override anything** in `~/.config/omni/style.css`.

### Knobs

| What | Where |
|---|---|
| Any style | `~/.config/omni/style.css` |
| Glass dimming over white apps | `MAX_LUMA` / `MAX_DIM` in `glass.rs` |
| Launcher fix-ups (e.g. Brave actions) | `TWEAKS` in `apps.rs` |
| Folders walked for file names | `name_roots` in `files.rs` |
| Folders whose contents aren't indexed | `SKIP_INSIDE` in `files.rs` |
| How long Chromium stays after the last search | `IDLE_EXIT` in `web_client.rs` (10 s) |
| Running programs on open | Settings → **Run programs** |
| Image search folders | `~/.config/omni/image-folders` or Settings → **Add folder…** |
| Claude's agent instructions | `src/agent_prompt.md`, then run `./install.sh` |

### Things omni remembers

| File | Remembers |
|---|---|
| `~/.local/state/omni/contents` | the **Contents** toggle |
| `~/.local/state/omni/filter` | the result **filter** |
| `~/.local/state/omni/claude` | Claude **model and effort** |
| `~/.local/state/omni/usage.tsv` | app launch counts and times |
| `~/.config/omni/run-programs` | **Run programs** on/off |
| `~/.config/omni/search-engine` | the built-in web search engine |
| `~/.config/omni/image-crawl` | Image search's hourly indexing on/off |

### Switch back to rofi

Put `rofi` in `~/.config/ml4w/settings/launcher`.

---

## Under the hood

Everything below is for the curious, or for when something breaks.

| | |
|---|---|
| 📂 [Layout & source files](#layout) | where everything lives |
| 🪝 [Hyprland / ML4W hooks](#how-its-hooked-into-hyprland--ml4w) | how it's wired into the desktop |
| 🫀 [Staying alive](#staying-alive) | health checks, watchdog, floods, GPU |
| 🗂️ [The file-name index](#the-file-name-index) | how name search is so fast |
| 🔍 [LocalSearch](#localsearch) | contents search and live events |
| 🐞 [Debugging](#debugging) | commands and tests |

### Layout

| Path | What |
|---|---|
| `~/Documents/github/omni/` | source (this folder) |
| `~/.local/share/omni/bin/omni` | the background program (GTK window, search, Claude) |
| `~/.local/share/omni/bin/omni-browser` | Chromium, started for web searches and stopped when idle; also Chromium's own helper processes |
| `~/.local/share/omni/bin/omni-toggle` | what the keybind runs: tiny, no GTK, opens/closes the popup in ~1 ms |
| `~/.local/share/omni/bin/omni-start` | starts `omni` through `ensure-running` |
| `~/.local/share/omni/hypr/omni.lua` | Hyprland layer rule for the popup (no Hyprland blur, fade in/out) |

Installed files are read-only. **Two programs instead of one on purpose** (speed first): the keybind helper doesn't load GTK, so pressing the key costs ~1 ms instead of ~60 ms.

### Source files

| File | What it does |
|---|---|
| `src/main.rs` | window, results list, keys, file right-click menu, socket, monitor placement |
| `src/calc.rs` | the calculator (strict parser: complete expressions only) |
| `src/apps.rs` | app list, matching, app actions (+ `TWEAKS`), launch counts and times (`~/.local/state/omni/usage.tsv`) |
| `src/recent.rs` | recent files/folders (GTK's shared recent list) |
| `src/files.rs` | the file-name index (walk, search, ranking), contents search |
| `src/sparql.rs` | minimal bindings to LocalSearch's library (queries, change events) |
| `src/web.rs` | default browser and search engine detection |
| `src/answer.rs` | Ask Claude (conversation process, permission prompts, model chip, Markdown, bouncing ball) |
| `src/images.rs` · `scripts/omni-embed.py` | Image search (`img …`): the helper process, its crawl, and the index |
| `src/agent_prompt.md` | your agent instructions, appended to Claude's system prompt |
| `src/services.rs` | systemd services (list, search, actions, status) |
| `src/ring.rs` | the search bar's animated rim |
| `src/glass.rs` | the opaque glass-card shell |
| `src/util.rs` | running helper programs with a time limit |
| `src/style.css` | the look. Override anything in `~/.config/omni/style.css`. Colours are omni's own cyan/black (`@omni_cyan`, `@omni_black`) |
| `src/search_view.rs` | the web search panel |
| `src/search_provider.rs` | built-in search engines (Brave) |
| `src/search_filters/` | Brave's page style and preference script |
| `src/web_client.rs` | omni's side of the browser: starts and stops the Chromium process, shows its frames, sends input |
| `src/web_protocol.rs` | the messages between omni and the Chromium process |
| `src/chromium.rs` | the Chromium process itself (CEF, private sessions, Brave preferences) |
| `src/browser_frame.rs` | frame size checks |
| `src/dock.rs` | pin or unpin apps on the quickshell dock |
| `src/probe.rs` | opt-in responsiveness diagnostics |
| `src/bin/omni-toggle.rs` | the keybind helper |
| `src/bin/omni-browser.rs` | the Chromium process and its helpers |
| `scripts/omni-start` | starts omni through `ensure-running` |

### How it's hooked into Hyprland / ML4W

- `~/.config/ml4w/settings/launcher` contains `omni`.
- `~/.config/hypr/scripts/launcher.sh` has an `omni` branch that runs `omni-toggle` (falls back to rofi if omni isn't installed). `Super`＋`Ctrl`＋`Enter` already runs this script.
- `~/.config/hypr/conf/autostart.lua` runs `omni-start` at the top level, so it runs on login and on every `hyprctl reload`.
- `~/.config/hypr/custom.lua` loads the layer rule from `~/.local/share/omni/hypr/omni.lua`. Any existing `grim` screen-capture permission is no longer needed by omni (other tools such as quickshell, alttabway, hyprlock or hyprpicker may still use it).

> ⚠️ `launcher.sh` and `autostart.lua` belong to the ML4W dotfiles; an ML4W update could overwrite them. If the old launcher comes back, re-add the `omni` branch / autostart line.

**Switch back to rofi:** put `rofi` in `~/.config/ml4w/settings/launcher`.

### Staying alive

**One instance only** (GTK single-instance app id `dev.omni.Launcher`).

#### Starting and health checks

- **`ensure-running`** starts omni with `--wait-wayland` (GTK hangs if started before Hyprland exports `WAYLAND_DISPLAY`) and checks health: it must have a Wayland display and answer `omni-toggle --ping` (which never opens the popup). Unhealthy, zombie or duplicate instances are replaced; **no more than 3 starts in 5 minutes**. Logs: `journalctl --user -t ensure-running`.
- **Slow Hyprland starts.** At early login Hyprland can launch programs before `WAYLAND_DISPLAY` / `HYPRLAND_INSTANCE_SIGNATURE` exist. omni forces `GDK_BACKEND=wayland` (no X11 fallback that could hang while holding its app id) and fills in missing or empty values from `hyprctl instances`, waiting up to 5 s; if Hyprland still isn't there it exits with an error instead of hanging.
- **Every keypress is checked.** omni replies "ok" to a toggle before doing it (one round trip, ~1.2 ms including starting `omni-toggle`). No reply within 1 s means omni is gone or stuck, and `omni-toggle` runs `omni-start`: ensure-running kills a hung instance, starts a fresh one, and the popup opens. A desktop notification appears if that fails.
- `hyprctl` calls are killed after 0.5 s, so a stuck helper can't freeze omni permanently.
- The popup releases the keyboard and flushes that change before it hides or launches anything, so a slow app can't leave it holding the keyboard.

#### The keyboard watchdog

While open, the popup holds the keyboard exclusively, so a frozen omni would lock the desktop.

- The UI thread checks in every **100 ms** while open.
- If it goes silent for **2 s**, a separate thread kills omni (tested: 2.2 s), Hyprland drops the popup and the keyboard is free.
- The next keypress starts a fresh omni.
- No check-ins while hidden.

#### LocalSearch event floods

A LocalSearch re-index sends an event for every file (~94k a minute).

- The UI thread only queues them; one worker thread resolves them in batches and keeps only files changed since omni's last walk (tested: 94k events, ~10 threads, pings 4–6 ms, nothing re-added).
- More than 20,000 new files triggers an early re-walk.

#### GPU

omni renders on the GPU that drives your screens and loads only its driver.

- ML4W sets `__GLX_VENDOR_LIBRARY_NAME=nvidia` for the session, which made omni load ~33 MB of NVIDIA driver it never used (and could wake the dGPU).
- When every connected screen is on an Intel or AMD GPU, omni re-executes itself once at startup with Mesa's EGL/GLX/Vulkan driver for that GPU (`OMNI_GPU_ENV` marks the second run).
- If a screen is on NVIDIA, nothing changes.

### The file-name index

Names come from omni's **own in-memory index** (like Everything/FSearch), not LocalSearch: LocalSearch returns unranked results in index order at this size, and ranked queries take seconds.

**What it walks:** home, `/usr`, `/etc`, `/opt`, `/var`, `/srv`, `/mnt`, `/run/media/$USER`, plus anything LocalSearch covers (`name_roots` in `files.rs`). Cache folders are skipped, and so is everything inside the `SKIP_INSIDE` folders (see [What's searched](#whats-searched)). Hidden files are included but ranked low.

**How fast:**

| | |
|---|---|
| Walk | in parallel at `nice 10`: about **2.9M entries in ~2.7 s** |
| Refresh | re-walked every **30 minutes** |
| Search | a SIMD substring scan over all distinct names, split across cores: **~1.5–4 ms**, on every keystroke |

**Memory:** the index holds ~42 MB for ~2.9M entries (it was ~70 MB for 4.5M before tool-managed folders were left out).

- Each distinct name is stored once, lowercase only (only 29% of names are distinct).
- The real capitalization of the few results shown is read back from disk, which doubles as the "still exists" check.
- Entries are grouped by name, so each is just a parent link and a flags byte.
- Restoring a file name's capitalisation, and checking that a result still exists, runs on one background thread and gives up after 200 ms. A dead network mount skips that result instead of stalling the popup.

### LocalSearch

LocalSearch (GNOME's indexer) provides the **Contents** search and live new-file events.

- Scoped to `$HOME`, `/run/media/user` and `/mnt/data` (system folders aren't content-indexed: that crawl kept it at ~1.3 cores and blocked its change events).
- Cache folders skipped and `.git` folders skipped (GNOME default; omni indexes their names).
- Removable drives on. Original settings: `localsearch-settings-backup.txt`.
- Use `/usr/bin/gsettings`: linuxbrew's `gsettings` writes somewhere LocalSearch never reads.
- The inotify watch limit was raised to 1048576 (`/etc/sysctl.d/90-omni-inotify.conf`).

### Debugging

```bash
~/.local/share/omni/bin/omni --search firefox      # app (and app action) ranking, no window
~/.local/share/omni/bin/omni --web "some query"    # the URL a web search would open
~/.local/share/omni/bin/omni --recent              # what shows with nothing typed
~/.local/share/omni/bin/omni --files plan readme   # build the file index here, time searches
~/.local/share/omni/bin/omni-toggle --ping; echo $?  # 0 = omni is running and healthy
~/.local/share/omni/bin/omni --check-env           # display/Hyprland env omni resolves + focused monitor
journalctl --user -t ensure-running | grep omni
```

**Run tests:**

```bash
PKG_CONFIG=/usr/bin/pkg-config cargo test --release
```

---

## Status log

Every entry from the original README is kept in full.

### Current state

- `cargo test --release` passes (69 tests, plus 3 in `omni-browser`). `./install.sh` is what replaces the running launcher.
- Chromium runs in `omni-browser` and exits 10 s after the last web search, so it takes no memory while idle. `omni` itself no longer loads CEF.
- `examples/chromium-session-check.rs` drove CEF inside its own process; it's behind the `session-check-unported` feature until it's ported.

### 2026-10-11: keyboard-lock incident (fixed)

- **Cause:** running `localsearch reset -s` made LocalSearch emit about 94k create events. The old live-update code started one thread per batch and did an O(n²) dedupe while holding the search lock. That left 233 threads and a frozen UI thread. The popup is an exclusive-keyboard layer, so opening it locked the keyboard.
- **Fix 1:** the main thread only queues events. A single live worker resolves them in batches and keeps only files whose mtime/ctime is newer than the last walk. Dedupe uses a HashSet. Extras are capped at 20k, and an overflow triggers an early re-walk. Under a real flood (94k events), omni stayed at about 10 threads and pings took 4–6 ms.
- **Fix 2:** a watchdog. While the popup is open, the UI thread heartbeats every 100 ms. If it is silent for 2 s, a watchdog thread calls `_exit(70)`, Hyprland drops the layer and releases the keyboard, and `omni-toggle` restarts omni on the next keypress. Tested with the `watchdog-test` socket message: exit after 2.2 s, then a clean restart.
- **Rule:** anything that can block the UI thread is a keyboard lock. Test changes against LocalSearch event floods.

### 2026-10-11: result filter

- A dropdown next to **Contents** (Ctrl+F) offers All / Apps / Files / Folders. The kinds are independent checkmarks, so any combination works ("Apps + Folders"). From All, picking a kind shows only that kind. After that, each kind toggles on or off. Unchecking the last kind goes back to All.
- The filter applies to recents and to search results. App actions count as Apps, and Contents matches count as Files. The web, Claude and calculator rows are never filtered.
- A narrowed filter allows up to 8 results per kind. The choice is remembered in `~/.local/state/omni/filter`.

### 2026-10-11: loose app matching

- **Apps only.** Files and folders still match exactly. When no app matches by name as typed, every word may instead be loosely similar to a word in an app's name (or, more strictly, its keywords and program name). There's no list of known typos: it's general similarity. Examples: "frfox" → Firefox, "ptormaster" → Portmaster, "brvae" → Brave, "obsidain" → Obsidian, "libreofice" → LibreOffice, "nautlus" → Files.
- **What counts as close.** Either the word is a few edits away (about a third of its letters, at least 1; swapped neighbours count as one edit), or its letters appear in order in the name word without being scattered ("frfox" in "firefox"). A partly typed word is compared with the start of the name word.
- **Noise control.** It only runs when exact matching finds no app by name. Weak resemblances are ignored, and guesses under half the best guess's score are dropped. Non-app queries like "invoices" or "report2024" show no apps.

### 2026-10-11: folder-scoped search and cyan/black theme

- **`<folder>: <name>`** searches only inside that folder and its subfolders. Examples: `~/Music: NF`, `/etc: hosts`, `~/.config/hypr: launcher`.
  - The folder part starts with `~` or `/`. A colon and a space separate it from the name.
  - The folder is matched case-insensitively (`~/music` works), and symlinked folders resolve to their real location.
  - Typing only `~/Music:` lists the folder itself, to open.
  - Scoped searches show files and folders only (no apps, web or Claude rows) and respect the filter.
- **Colours:** omni has its own scheme, cyan `#54dbed` on black `#07090b` (`@omni_cyan`, `@omni_black` in style.css), matching the search bar's rim. It no longer follows the wallpaper palette. This covers the selected row, the Contents toggle when on, menus, the calculator card and Claude's loading ball.

### 2026-10-11: follow-up questions to Claude

- After asking Claude, the search bar becomes the reply box ("Reply to Claude · Esc to go back to search"). Enter or Shift+Enter sends a follow-up. Each turn shows your message, then the answer, with the newest text kept in view (scroll up to reread; it stops following).
- **Still temporary.** One `claude -p --input-format stream-json --no-session-persistence` process stays running for the conversation and takes each message as a JSON line on stdin. Nothing is written to disk. It's killed on Esc (back to search) or when the popup closes.
- **No length limits.** Long conversations are handled instead:
  - each turn has its own labels, so finished turns are never re-rendered and the per-frame cost doesn't grow;
  - messages are written to Claude asynchronously, so a huge paste can't block the UI thread (which would trip the keyboard watchdog);
  - the non-JSON log kept for error messages is capped;
  - Claude Code compacts long contexts itself.
- **Recovery:** each turn has a 90 s timeout. If the process dies or times out, the turn shows the error, and the next reply starts a new process primed with the transcript so far. Enter while Claude is still answering is ignored, and the text stays in the bar to send after.
- **Fixed:** Claude Code's auto-memory leaked into omni (asking it to "remember" something made it print a memory-file tool call as the answer). omni now runs it with `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`.

### 2026-10-11: Markdown in answers, links, ball on follow-ups

- **Markdown:** answers are rendered with pulldown-cmark. This covers headings, bold, italic, strikethrough, inline code and code blocks, bullet and numbered lists (nested), task lists, quotes, rules and tables. Everything is escaped and the output is checked as valid Pango markup, with a fallback to plain text. While text streams in, a half-arrived `**`, `` ` ``, `~~` or `[link](` is hidden until its other half arrives.
- **Links** are cyan and clickable. A click closes the popup (it holds the keyboard) and opens the link in the default browser.
- **No focus outline** when clicking answer text to select it.
- **Ball on follow-ups:** it launches in a 100 px open space under the new question and bounces off the conversation text in view (clipped to the scroll area) as well as the header. The bounce height is sized to the open space.
- **Copy buttons, no selection:** answer and question text isn't selectable. Each of your messages and each answer has a small copy button beside it. It copies the message's raw text (the answer's Markdown source) and briefly shows a checkmark. The header's old Copy button is gone.
- **Links ask first:** clicking a link in an answer shows "Open {host}?" with the host taken from after the last `@`, and the full URL underneath, wrapped, with bidi and control characters written out. **Yes** (or Enter) closes omni, ending the temporary conversation, and opens that same URL. **No** (or Esc) goes back to the conversation. Nothing survives omni closing.
- **Rendered Markdown only:** the system prompt tells Claude its reply is shown rendered. Links and formatting are written once as Markdown, never shown as source in code blocks with a "Rendered:" copy, even when asked for "markdown format". Code blocks are only for real code.

### 2026-10-11: final audit

- **Fixed:** after a failed turn (error or timeout), the failed answer's copy button copied the *next* successful answer. Each turn now owns its answer.
- **Fixed (security):** answers can quote fetched web pages, so links are only clickable for `https://`, `http://` and `mailto:`. Anything else (`file://`, `javascript:`, app schemes like `steam://`) shows as plain text. The confirmation also shows the real destination, not the link text.
- **Checked:**
  - no leftover `claude` processes after closing;
  - clippy reports style lints only;
  - a killed omni closes Claude's stdin, so the Claude process exits too.

### 2026-10-11: Claude model/effort switcher, systemd services

- **Model and effort:** the answer header has a chip, for example "Haiku 5.5 · Medium ▾". Its dropdown lists the models (Haiku 5.5, Sonnet 5.5, Opus 5.5, Fable 5.1) and effort levels (Low, Medium, High, Extra = `xhigh`, Max).
  - The default is Haiku 5.5 at Medium. The choice is remembered in `~/.local/state/omni/claude`.
  - Mid-conversation, the next message goes to a fresh Claude Code with the new settings, primed with the transcript, so the conversation carries on. An answer still being written finishes first.
  - The per-answer timeout grows with effort: 90 s for Low and Medium, then 150, 240 and 360 s.
  - Ultracode was considered and dropped.
- **Services:** typing a service's name (3+ letters) lists matching system and user systemd services with their state, for example "bluetooth" → bluetooth.service · System service · running · enabled.
  - Enter or a click only selects a service. Its actions are on right-click or the Menu key: **Status** (`systemctl status` with the last 15 log lines, shown in the popup, no password; Esc or typing goes back), then Start, Stop, Restart, Enable and Disable, showing only those that fit its state.
  - omni closes first, then runs `systemctl [--user]`. System services ask for the password through the desktop's polkit agent, so omni never sees it. A notification reports success or the error.
  - Stopping, restarting or disabling a session-critical service (display manager, D-Bus, logind, polkit, NetworkManager, sshd, PipeWire, udev, `systemd-*`, `user-runtime-dir@`, ...) asks first, on the dimmed confirmation layer.
  - The list is re-read in the background each time omni opens (~1.5 s). Services are a kind in the filter dropdown. A saved filter of the old "All" is migrated to the new "All".

### 2026-10-11: audit of the Claude switcher and services

- **Fixed:** menu actions could run twice. A double click or a held Enter on a menu item (Restart, Open with, …) could fire it twice. Each menu now runs at most one action.
- **Fixed:** missing session-critical services. Stopping `wayland-wm@hyprland.desktop.service` (your Hyprland session under uwsm) or the `xdg-desktop-portal*` services didn't ask first. They're now on the confirm list, along with `pipewire-pulse`, `at-spi-dbus-bus` and `wayland-session-*`.
- **Fixed:** systemctl calls pass `--` before the unit name, so a unit name can never be read as an option.
- **Fixed:** opening omni repeatedly stacked service loads (each ~1.5 s, 4 systemctl runs), and a slow old load could overwrite a newer list. Only one load runs at a time now.
- **Checked:**
  - Every model and effort combination is accepted by Claude Code.
  - Fable 5.1 currently answers "requires usage credits" on this account; omni shows that as the turn's error, and switching model recovers.
  - Mid-answer model switches wait for the answer, then restart primed with the transcript.
  - Confirm Yes can't fire twice.
  - Esc, Enter and the scroll wheel go to the right place with menus, the status panel, the confirmation or a chat showing.
  - Service actions only run after omni has hidden, so the polkit dialog gets the keyboard.
- Restoring a file name's capitalisation, and checking that a result still exists, runs on one background thread and gives up after 200 ms. A dead network mount skips that result instead of stalling the popup.

### 2026-10-11: Claude tools and permission prompts

- **Tools:** web search, web fetch, read, glob, grep, Bash, edit and write, working from the home folder. No CLAUDE.md or settings are loaded (`--setting-sources ""`).
- **Who decides:** Claude Code's own permission checks decide what needs asking.
  - Read-only work (reading or searching files, read-only commands like `ls` or `cat`) runs freely.
  - Commands that change things, edits, writes and web fetches ask.
  - Web search is pre-approved, since it only sends a query.
  - Fetches ask because a fetched page could otherwise trick Claude into sending your data out in a URL.
- **Prompts:** omni answers them through Claude Code's host permission protocol (`--permission-prompt-tool stdio`, `control_request` / `can_use_tool`). They appear on the dimmed confirmation layer: "Claude wants to run a command", with the exact command, file or URL in monospace.
  - **Deny** is focused, so a stray Enter can't approve. Esc and clicking the dim also deny, and Claude is told not to retry.
  - **Allow once.**
  - **Always allow:** the exact command on screen, or that one file for an edit, sent back with `destination: "session"`. A suggestion containing `*`, `?` or `[`, a different path, or a multiline command is not remembered. It lasts for this conversation only and is never written to any settings file.
  - Several prompts queue and show one at a time. Prompts for a process that died disappear.
  - The answer timeout pauses while a prompt is waiting.
- **Secrets are denied outright** for the Read and Edit tools and shell reads (verified with `cat`):
  - `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.azure`, gcloud, `gh` and `~/.kube`;
  - keyrings and `pass`;
  - Claude's own credentials, `.netrc` and Docker auth;
  - Firefox, Brave and Chrome profiles.

### 2026-10-11: permission system audit

Each attack below was tested against Claude Code with omni's exact flags.

- **Fixed (critical): wildcard bypass of the secret deny rules.** `head -c 40 ~/.s?h/known_hosts` read a file in `~/.ssh` with no prompt: the glob slipped past the `Read()` deny rule, and the command counted as read-only, so it was auto-approved.
  - Shell commands now run in Claude Code's **bubblewrap sandbox** (`--settings`: `sandbox.enabled`, `failIfUnavailable`, `allowUnsandboxedCommands: false`, `filesystem.denyRead` = the secrets list), so the paths don't exist for them.
  - Verified: the glob finds nothing, browser profiles list as empty, and DNS lookups fail. Network access asks (`SandboxNetworkAccess`, showing the host).
- **Fixed (critical, found while fixing the above):** with the sandbox on, Claude Code auto-approves every sandboxed command by default (`autoAllowBashIfSandboxed`), `rm` in your home folder included. That's now off. Verified: `touch` and `rm` ask.
- **Fixed: "Always allow" could grant more than it said.** Suggestions that would add directories or other modes are never sent. (Edits: see the final audit below.)
- **Fixed: the prompt could hide part of the command.** The detail was cut at 400 characters, so a long command could hide something past the cut. Now the full command, file content (Edit shows replace → with, Write shows the content) or URL is shown, scrolling if long.
  - Invisible and direction-changing Unicode characters are shown as `⟨U+202E⟩`-style codes, so text can't display differently from what runs.
  - The detail always comes from the tool's input, never from Claude's own description.
- **Fixed: click-through.** A prompt that pops up under the pointer or mid-keystroke can't be approved by that same click or key. Allow and Always only count 0.5 s after it appears; Deny always works.
- **Fixed: leftover processes.** Claude Code runs in its own process group and systemd scope (`omni-claude-*.scope`). Ending the conversation kills the scope, which ends every command Claude started, even ones that detached with `setsid`. Verified.
- **More secrets:** git credentials, `.npmrc`, `.pypirc`, Cargo credentials, rclone, Thunderbird, Chromium, Bitwarden, Discord and Signal.
- **Checked:**
  - Grep and Glob over `~/.ssh` or the whole home folder find nothing secret.
  - Read and Write through a symlink into `~/.ssh` are denied.
  - `$HOME`-expanded and `base64` reads are denied.
  - Deny has focus, and Enter with no focused button denies.
  - Esc, clicking the dim, closing omni and a Claude crash all deny or dismiss.
  - Queued prompts are answered in order.
  - Unknown control requests get an error reply, so Claude never waits forever.

### 2026-10-11: claude.ai connectors

- omni's Claude used to start with `--strict-mcp-config`, which loaded no MCP servers at all, so "use my AgentMail" had no tool and Claude went looking for a CLI.
- That flag is gone, and `--setting-sources ""` stays. Your **claude.ai connectors** load (AgentMail, Google Calendar, Notion, Dropbox, Spotify, Hugging Face, …), but local plugins and their hooks don't. This costs about 2 s at conversation start.
- Connector calls ask, like any other tool, with a readable title ("Claude wants to use AgentMail: List Inboxes") and the full input. Sending an email always shows what's being sent. "Always allow" for a connector covers that one tool for the rest of the chat, not a different one.
- AgentMail reads of `assistant@example.com` (and list calls that name no inbox) run without a prompt. A read that names any other inbox asks.

### 2026-10-11: agent instructions

- Your agent instructions live in `src/agent_prompt.md`: identity "Contact AI Agent", AgentMail rules, the `assistant@example.com` inbox, the injection protocol, the limit, the files and the fail-safe. omni appends them after its own launcher instructions. Edit that file and run `./install.sh` to change them.
- **Enforced by omni, not just requested:**
  - The AgentMail tools the instructions reserve for you are **denied outright**: `create_inbox`, `update_inbox`, `delete_inbox`, `delete_thread`, `delete_list_entry`, `connect_provider`, `agent_attach_human`, `agent_verify`, plus `connect_app`, the connector's actual "connect" tool. No email or injection can get them used.
  - AgentMail's read-only tools (list, get or search inboxes, threads, messages, drafts, list entries; attachments) run without a prompt only when every inbox field is `assistant@example.com` or absent. Any other inbox asks. Sending, replying, forwarding, drafts and list changes still show omni's prompt, on top of the in-chat confirmation the instructions require.
- **Connector race workaround:** Claude Code sometimes starts before the claude.ai connector list arrives (1 in 4 to 1 in 3 starts in testing) and then has no AgentMail for the whole conversation. When a start reports no connectors, omni restarts it and resends the message before any text appears, up to 3 times. If an account genuinely has none, it stops retrying. Simulated over 6 conversations: 6/6 got their connectors.

### 2026-10-11: final audit (permissions, restarts, crash triggers)

- **Fixed (serious): "Allow all edits" also allowed deleting and moving.** Tested: in Claude Code's accept-edits mode, `rm`, `mv` and `mkdir` ran with no prompt, and an explicit `ask` rule on a file was overridden too.
  - omni never sends that mode. For an edit or write, the third button is now **"Always allow this file"**: a session rule `Edit(//<path>)` for that one file.
  - Verified: the same file's next edit doesn't ask, other files still ask, and nothing else is granted.
- **Fixed: "Always allow" can't approve a whole tool.** Suggested rules naming a built-in tool without specifics (a bare `Bash` would approve every command) are dropped. Connector tool rules stay, since each names one tool.
- **Fixed: busy Claude killed by the timeout.** With commands and connectors, a long multi-step task could hit the 90 s limit while working, and omni would kill it. The timeout now counts *silence* (no output for 90–360 s depending on effort), with a 20-minute hard cap. It still pauses while a permission prompt waits.
- **Checked, no change needed:**
  - Restarts happen only on a model or effort change (between turns, or after the running answer), on a missing connector list (before any text, at most 3 per turn), or after a crash (on the next message).
  - Each restart is primed with the transcript. "Always" rules from that chat are lost then, so you get asked again, which is the safe direction.
  - No answer or prompt callback runs while omni is holding the data it would touch (`RefCell` borrows), so none can trigger a double-borrow panic.
  - Text slicing in the prompt and Markdown code only cuts at ASCII boundaries.
  - The timer arithmetic can't underflow.
  - Answers arriving after a stop, cancel or retry are ignored.
  - Prompts queue and survive Esc, hide and crash paths.

### 2026-10-11: audit follow-up (environment, protected files)

- Another model's audit and patches are in `docs/audit-sanctuary.md`. They were reviewed: it builds, the tests pass, and the key fixes were re-tested against Claude Code.
- **Fixed: inherited tokens.** omni passes its environment to Claude and to every app it opens. Restarted from a Claude Code session, it carried that session's `CLAUDE_CODE_*` tokens, and `printenv` runs without a prompt. omni now drops `CLAUDE_CODE_*`, `ANTHROPIC_*` and anything named like a token, secret, password or key at startup (`scrub_env` in `main.rs`; `SSH_AUTH_SOCK` stays). `install.sh` restarts omni through Hyprland (`hl.dsp.exec_cmd`), so it gets the desktop's environment.
- **Fixed: approved commands could plant code that runs outside the sandbox.** Shell commands can never write to `PROTECTED` in `answer.rs`: shell startup files, oh-my-zsh, `~/.mydotfiles`, **all of `~/.config`**, `.desktop` launchers, user systemd and D-Bus services, Nautilus scripts and extensions, flatpak overrides, Python's user site-packages (`~/.local/lib`), git config, omni's and Prism's files, and the PATH folders in home (`~/.local/bin`, `~/.cargo/bin`, …). This uses the sandbox's `denyWrite`.
  - All of `~/.config`, not single folders: the sandbox protects what a symlink points to, not the symlink. Tested: a command could delete a protected symlink and put a real folder there, and `~/.config/hypr`, `zshrc`, kitty, fish, waybar and quickshell are all symlinks into `~/.mydotfiles`. A read-only `~/.config` blocks that. The secrets inside it stay hidden.
  - Tested with the real list: an approved command can't append to, delete or create protected paths, rename their folder away, or add a symlink in `~/.config`. Other folders (`~/Documents`, `~/.cache`, …) stay writable. No placeholder files are left in your home.
  - Claude's Edit and Write tools can still change these files, always with a prompt. Commands that write to `~/.config` or a PATH folder (`cargo install`, a tool saving its settings) now fail inside omni; run them yourself.
  - **Known gap:** `~/.bashrc` is a symlink directly in home, and home itself can't be read-only without blocking every write. zsh is your login shell, so bash rarely reads it.
- **Still open:** a second keypress during backdrop capture, `ensure-running` matching by process name, the D-Bus toggle, and case-variant paths.

### 2026-10-11: readable over white apps

- **Fixed:** over a white app, the lightly tinted glass stayed nearly white and white text was unreadable. omni now measures the captured backdrop's bright parts (85th percentile of brightness, so half a white window counts) and lays just enough black over it to keep text readable (`dim_for` in `glass.rs`). Dark wallpapers keep the clear glass, and a fully white app gets about 63% dimming (capped at 72%). Tune `MAX_LUMA` / `MAX_DIM` in `glass.rs`.

### 2026-10-11: app list can't go stale

- **Found:** ArtCraft Launcher was missing from omni. AppImage Manager had left a launcher entry in `~/.local/share/applications` for a deleted AppImage. It shared the RPM entry's ID, so it overrode that entry, and GIO drops entries whose program is missing. The app vanished from every launcher. Trashing the stale entry fixed that, but the running omni still didn't show it until restarted: it had missed GIO's "apps changed" signal. Why is unknown; GIO sent it correctly in an isolated repro.
- **Fixed:** omni now also re-reads the installed apps every time the popup opens. It takes about 30 ms, so it runs right after the popup is drawn and doesn't delay it, then refreshes results if text is already typed (`toggle` in `main.rs`). The GIO signal stays as the main path.

