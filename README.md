<p align="center">
  <img src="logo.png" width="140" alt="omni logo">
</p>

<h1 align="center">omni</h1>

<p align="center">
  A keyboard-first launcher for Hyprland: apps, files, folders, services, images, the web and Claude in one popup.<br>
  Written in Rust with GTK4 and gtk4-layer-shell.
</p>

<p align="center">
  Press your keybind, type, press <kbd>Enter</kbd>.
</p>

---

## Contents

- [At a glance](#at-a-glance)
- [Install](#install)
- [Keyboard shortcuts](#keyboard-shortcuts)
- [What you can type](#what-you-can-type)
- [Right-click menus](#right-click-menus)
- [Features](#features)
  - [Results and the filter](#results-and-the-filter)
  - [Apps and app actions](#apps-and-app-actions)
  - [Files and folders](#files-and-folders)
  - [Programs and commands](#programs-and-commands)
  - [Services](#services)
  - [Calculator](#calculator)
  - [Web search](#web-search)
  - [Image search](#image-search)
  - [Ask Claude](#ask-claude)
- [Configuration](#configuration)
  - [Settings panel](#settings-panel)
  - [Files omni keeps](#files-omni-keeps)
  - [Source-level settings](#source-level-settings)
  - [Switching back to rofi](#switching-back-to-rofi)
- [Troubleshooting](#troubleshooting)
- [Architecture](#architecture)
  - [Installed files](#installed-files)
  - [Source files](#source-files)
  - [Desktop integration](#desktop-integration)
  - [Reliability](#reliability)
  - [The file-name index](#the-file-name-index)
  - [LocalSearch](#localsearch)
  - [GPU selection](#gpu-selection)
  - [Web search internals](#web-search-internals)
  - [Image search internals](#image-search-internals)
  - [Claude: permissions and sandbox](#claude-permissions-and-sandbox)
- [Changelog](#changelog)

---

## At a glance

| You want to | Do this |
|---|---|
| Open an app | Type part of its name, press <kbd>Enter</kbd>. Typos are forgiven. |
| Open a file or folder | Type part of its name. |
| Search inside one folder | `<folder>: <name>`, for example `~/Documents: report` |
| Search inside files | Turn on **Contents** with <kbd>Ctrl</kbd> + <kbd>T</kbd> |
| Find a picture by what is in it | `img <description>` |
| Do quick maths | `200*15%` (<kbd>Enter</kbd> copies the result) |
| Search the web | Type, then <kbd>Ctrl</kbd> + <kbd>Enter</kbd> |
| Ask Claude | Type, then <kbd>Shift</kbd> + <kbd>Enter</kbd> |
| Manage a systemd service | Type its name, right-click it |
| Close the popup | <kbd>Esc</kbd>, or click outside the card |

Everything shares one list, so there is no mode to pick first.

---

## Install

**Requirements**

- Hyprland and LocalSearch (GNOME's file indexer).
- Rust, and on Fedora: `gtk4-devel`, `gtk4-layer-shell-devel`, `tinysparql-devel`, `cef`.
- Optional: an `ensure-running` supervisor on `PATH`. `omni-start` uses it when present (health checks, a limit on restarts); without it, `omni-start` checks and starts omni itself.
- For image search: `uv` and an NVIDIA GPU with a recent driver.

```bash
sudo dnf install gtk4-devel gtk4-layer-shell-devel tinysparql-devel cef
```

From the folder you cloned omni into:

```bash
./install.sh
```

Run the same script again to update. It builds a release into the repository's `target/` (even when `CARGO_TARGET_DIR` is set), replaces the installed files atomically, restarts omni and checks that it answers. It also builds the image search environment with `uv` when its requirements change.

**Hook it into Hyprland:** bind a key to `~/.local/share/omni/bin/omni-toggle`, start `~/.local/share/omni/bin/omni-start` at login, and load the popup's layer rule. See [Desktop integration](#desktop-integration).

---

## Keyboard shortcuts

| Key | Action |
|---|---|
| Type | Search apps, files, folders, commands and services |
| <kbd>↑</kbd> / <kbd>↓</kbd>, scroll wheel | Move through results (the wheel scrolls the text while a Claude answer is showing) |
| <kbd>Enter</kbd> | Open the selected result |
| <kbd>Ctrl</kbd> + <kbd>Enter</kbd> | Search the web for what you typed |
| <kbd>Shift</kbd> + <kbd>Enter</kbd> | Ask Claude; afterwards <kbd>Enter</kbd> sends follow-ups |
| <kbd>Ctrl</kbd> + <kbd>T</kbd> | Toggle **Contents** (also match text inside files) |
| <kbd>Ctrl</kbd> + <kbd>F</kbd> | Open the result filter |
| <kbd>Menu</kbd> or <kbd>Shift</kbd> + <kbd>F10</kbd> | Right-click menu for the selected result |
| <kbd>Esc</kbd> | Back out one level, then close |

<kbd>Esc</kbd> steps back in this order: menu, prompt, status panel, conversation, search, close.

---

## What you can type

| Input | Result | Example |
|---|---|---|
| *(nothing)* | Your 5 most recent apps, files and folders | |
| `<app name>` | Apps; typos are forgiven | `firefox`, `frfox` |
| `<app name> <action>` | That app action as its own row | `firefox private` |
| `<program>` | Programs in your `$PATH` | `python` |
| `<service name>` (3 or more letters) | Matching systemd services, with their state | `network` |
| `<file or folder name>` | Files and folders anywhere | `report` |
| `<folder>: <name>` | Files and folders inside that folder only | `~/Documents: report` |
| `<folder>:` | The folder itself | `~/Documents:` |
| `<calculation>` | The calculator | `6x5`, `200*15%` |
| `img <description>` | Pictures ranked by what is in them | `img a dog on a beach` |
| `<anything>`, then <kbd>Ctrl</kbd> + <kbd>Enter</kbd> | Web search | |
| `<anything>`, then <kbd>Shift</kbd> + <kbd>Enter</kbd> | Ask Claude | |

---

## Right-click menus

Right-click a row, or press <kbd>Menu</kbd> or <kbd>Shift</kbd> + <kbd>F10</kbd> on the selected one.

| Row | Menu |
|---|---|
| File | **Open**, **Open with** (up to 10 apps registered for the type, recommended first) |
| App | **Open** and the app's own actions (New window, New private window, ...) |
| Search the web | **Search**, **New private window**, and for Brave **New private window with Tor** |
| Service | **Status**, then Start, Stop, Restart, Enable and Disable as they apply |

---

## Features

### Results and the filter

**With nothing typed**, omni shows the 5 most recent apps, files and folders, newest first. Apps come from omni's own launch history; files and folders come from GTK's shared recent list (`~/.local/share/recently-used.xbel`), skipping ones that no longer exist. Web queries, Claude prompts and file contents never appear there.

**Result order is fixed:**

| Order | Kind | Up to |
|---|---|---|
| 1 | App actions | 3 |
| 2 | Apps | 6 |
| 3 | Services | 3 |
| 4 | Commands (programs in `$PATH`) | 3 |
| 5 | Files | 5 |
| 6 | Folders | 3 |
| 7 | File contents | 4 |
| 8 | Search the web | 1 |
| 9 | Ask Claude | 1 |

- The web row is first only when nothing above it matches.
- Apps you launch often rank higher among apps.
- A matching home folder (`~/Documents`, ...) is listed above commands and files.

**The filter** (<kbd>Ctrl</kbd> + <kbd>F</kbd>) offers All, Apps, Files, Folders and Services as independent checkmarks, so any combination works.

- From **All**, picking a kind shows only that kind. After that, each kind toggles on or off. Unchecking the last kind returns to **All**.
- A narrowed filter allows up to 8 results per kind.
- App actions count as Apps; commands and Contents matches count as Files. The web, Claude and calculator rows are never filtered.
- The choice is remembered in `~/.local/state/omni/filter`.

### Apps and app actions

Apps are matched like GNOME does: word starts first, then substrings.

**Loose matching** applies to apps only, and only when nothing matches by name as typed. Each typed word may then be loosely similar to a word in an app's name (or, more strictly, its keywords and program name). There is no list of known typos.

| You type | You get |
|---|---|
| `frfox` | Firefox |
| `brvae` | Brave |
| `obsidain` | Obsidian |
| `libreofice` | LibreOffice |
| `nautlus` | Files |

A word counts as close when it is a few edits away (about a third of its letters, at least one; swapped neighbours count as one edit), or when its letters appear in order in the name word without being scattered. Weak guesses, and guesses under half the best guess's score, are dropped, so queries like `invoices` show no apps.

**App actions** are the actions an app's `.desktop` file declares, read through GIO.

- An action gets its own row only when the query names it, for example `firefox private`. The app's name alone shows just the app.
- Without the app's name, the query needs a distinctive whole word (`incognito`).
- Ranked by match, then launches from omni; the default browser wins ties.
- `TWEAKS` in `apps.rs` corrects launchers that differ from the app's own menu. For Brave it renames the actions to match its menu and adds **New private window with Tor** (`--tor`), which Brave's launcher file leaves out.

### Files and folders

**What is searched**

- **Names** everywhere, except inside folders that tools manage: package stores (`node_modules`, `.npm`, `.yarn`, ...), editor state (`.vscode`, `.cursor`, `.idea`), version control (`.git`, ...), toolchains (`.rustup`, `.cargo`, `.gradle`, `.m2`), tool caches, Trash, `flatpak` and `containers`. Those folders themselves are still listed. Dotfiles (`~/.config`, `~/.local`) and project environments (`.venv`) are indexed. The list is `SKIP_INSIDE` in `files.rs`.
- **Contents** in your home folder and drives, when **Contents** is on.

**Ranking**

1. Whole name, then name start, then word start, then anywhere.
2. Your XDG home folders (`~/Documents`, `~/Downloads`, ...) first, then things inside them, then the rest of home, shallow paths first.
3. Extension matches (`pdf`) favour that file type.
4. Hidden and dependency or build folders (`site-packages`, `target`, `.venv`, ...) and their contents are pushed down.

One- and two-letter queries match name starts only. Every shown path is checked, so deleted files never appear. New files are added as LocalSearch reports them.

**Folder-scoped search:** `<folder>: <name>` searches only inside that folder and its subfolders.

- The folder starts with `~` or `/`, followed by a colon and a space.
- It is matched case-insensitively (`~/music` works), and symlinked folders resolve to their real location.
- Scoped searches show files and folders only, and respect the filter.

**Contents search** (<kbd>Ctrl</kbd> + <kbd>T</kbd>) also lists files whose text matches. It is off by default and remembered in `~/.local/state/omni/contents`. Two sources run side by side, off the UI thread: LocalSearch's full-text index, and ripgrep over `~/Documents` and `~/Projects` (LocalSearch skips git repositories by default; ripgrep honours `.gitignore`). Every word must appear somewhere in the file, the last word also matches as a prefix, and any capital letter makes the search case-sensitive. It waits for a short pause in typing, results slot in as each source answers, and ripgrep's matches are listed above LocalSearch's. A selection you moved with the arrow keys stays where you put it.

**Right-click a file** for **Open** or **Open with**. The file type is guessed from the name, so opening the menu never touches the disk.

### Programs and commands

**Opening an executable file runs it, after you confirm.** This applies to a result row, <kbd>Enter</kbd> and the **Open** menu item.

- A program is a regular file you may execute that is a compiled binary (ELF, including AppImages) or a script starting with `#!`. The exec bit alone is not enough; an executable text file still opens in its editor.
- The confirmation shows the full path. **Cancel** has focus, and **Run** ignores input for the first half second, so a stray key or click cannot approve it.
- The program runs from its own folder in your preferred terminal, as you. Omni reads the desktop-specific or generic `xdg-terminals.list` preference, then falls back to an installed terminal. Output stays visible after exit; press Enter to close the terminal. Files are inspected off the UI thread, checked again against their approved identity at launch, and executed through the checked file descriptor. If a file has changed, execution is refused and the terminal explains why. Terminal process failures or a missing launch acknowledgement produce a notification. Descriptor execution gives scripts a descriptor path in `$0`; their working directory remains the original folder. These checks protect approval against path replacement; they do not sandbox the launched program or freeze its dependencies.
- Turn this off in Settings, **Run programs**. Executables then open in their default app.

**Commands:** programs directly inside a `$PATH` folder form their own group of up to 3, above the files. The one your shell would run for that exact name comes first, then other matches; version numbers compare as numbers (`python3.15` before `python3.9`). A program installed since the last index walk shows as a plain file until the next walk (every 30 minutes).

### Services

Typing a service name (3 or more letters) lists matching system and user systemd services with their state, for example `NetworkManager.service · System service · running · enabled`.

- <kbd>Enter</kbd> or a click only selects a service. Its actions are on the right-click menu: **Status** (`systemctl status` with the last 15 log lines, shown in the popup), then Start, Stop, Restart, Enable and Disable as they fit its state.
- omni closes first, then runs `systemctl`. System services ask for your password through the desktop's polkit agent, so omni never sees it. A notification reports the result.
- Stopping, restarting or disabling a session-critical service (display manager, D-Bus, logind, polkit, NetworkManager, PipeWire, the Hyprland session, portals, ...) asks for confirmation first.

### Calculator

When the whole query is a complete calculation, the result replaces the list: the expression small on top, the result large underneath. <kbd>Enter</kbd> copies the result.

| Works | Notes |
|---|---|
| `6x5`, `3 × 4`, `10÷4`, `2^10`, `(3+3)*2`, `2(3+4)`, `200*15%` | `x`, `×` and `*` multiply; `÷` and `/` divide; `%` after a number divides it by 100 |

Unfinished input (`4x`), a bare number, words (`15% of 80`) and division by zero produce no calculator row. Results show up to 10 significant digits.

### Web search

<kbd>Ctrl</kbd> + <kbd>Enter</kbd>, or the web row, opens **Brave Search inside omni**, showing Brave's AI answer view with its sources and follow-up prompt. Back or <kbd>Esc</kbd> returns to the launcher.

- Every search is a private, in-memory browser session. Nothing (cookies, history, storage, caches) carries over to the next search.
- The browser runs in its own process, starts on the first search (about 1.2 s to the page) and exits 10 seconds after the last one, so it uses no memory while idle.

**Private windows in your own browser:** right-click the web row.

| Option | For | Flag |
|---|---|---|
| **New private window** | Chromium-based browsers | `--incognito` |
| **New private window with Tor** | Brave | `--tor` |

These use Brave's private-window search engine. Firefox gets no private items, because its search engine cannot be pointed at a private window.

Details: [Web search internals](#web-search-internals).

### Image search

Type `img`, a space and a description of the picture, for example `img a dog on a beach`. Your pictures are ranked by what is in them, with thumbnails; <kbd>Enter</kbd> and the right-click menu work as for any file.

- **Folders:** `~/Pictures` and its subfolders by default. Add more in Settings, **Image search**: type a path, turn on **Subdirectories** to include everything under it, then **Add**. Adding a folder again checks it for new images; adding it again with **Subdirectories** on widens it.
- **Indexing** runs quietly 10 seconds after omni starts, then hourly, and only processes new or changed images. Turn it off in Settings, or run it at full speed with **Index now**.
- **Formats:** PNG, JPEG, WebP, GIF (first frame), BMP. Hidden folders are skipped.
- **Resources:** everything runs on the GPU. The search model loads when you type `img` and unloads 10 seconds after your last query.

Details: [Image search internals](#image-search-internals).

### Ask Claude

Press <kbd>Shift</kbd> + <kbd>Enter</kbd> on anything you typed. omni runs the Claude Code CLI with your normal login as a **temporary conversation** in the popup.

- **Follow-ups:** the search bar becomes the reply box.
- **Answers** render as Markdown, with copy buttons. Links (`https`, `http` and `mailto` only) ask before opening, because opening one closes omni.
- **Continue in Claude** opens the question in the Claude app.
- **Nothing is saved.** The conversation ends when you press <kbd>Esc</kbd> back to search or omni closes.
- **Tools:** web search and fetch, reading and searching files, shell commands in a sandbox, edits and writes, and your claude.ai connectors. Anything that changes something asks first.
- **Secrets are unreadable** to Claude: SSH and GPG keys, cloud credentials, keyrings, browser profiles, shell histories and similar.

| Setting | Value |
|---|---|
| Models | Haiku 5.5, Sonnet 5.5, Opus 5.5, Fable 5.1 |
| Effort | Low, Medium, High, Extra (`xhigh`), Max |
| Default | Haiku 5.5, Medium |
| Switch | The chip in the answer header; remembered in `~/.local/state/omni/claude` |
| Timeout | 90 to 360 s of silence depending on effort, 20 minutes at most; paused while a permission prompt waits |

Your own agent instructions live in `src/agent_prompt.md`; run `./install.sh` after editing them.

Details: [Claude: permissions and sandbox](#claude-permissions-and-sandbox).

---

## Configuration

### Settings panel

Hover the top-right corner of the search bar and click the gear.

| Setting | What it does |
|---|---|
| **Search engine** | The built-in web search engine (currently Brave) |
| **Run programs** | Whether opening an executable runs it (after confirmation) or opens it in its default app |
| **Image search** | Hourly indexing on or off, a folder field with **Subdirectories**, **Add** and **Index now** |

### Files omni keeps

| File | Holds |
|---|---|
| `~/.local/state/omni/contents` | The **Contents** toggle |
| `~/.local/state/omni/filter` | The result filter |
| `~/.local/state/omni/claude` | Claude model and effort |
| `~/.local/state/omni/usage.tsv` | App launch counts and times |
| `~/.config/omni/run-programs` | **Run programs** on or off |
| `~/.config/omni/search-engine` | The web search engine |
| `~/.config/omni/image-crawl` | Hourly image indexing on or off |
| `~/.config/omni/image-folders` | Image search folders, one per line; a trailing `/**` includes subfolders |
| `~/.config/omni/style.css` | Your style overrides |
| `~/.local/share/omni/images/index.npz` | The image index |
| `~/.cache/omni/thumbs-160` | Image thumbnails |

### Source-level settings

| What | Where |
|---|---|
| Any style | `~/.config/omni/style.css` overrides `src/style.css` (colours: `@omni_bg`, `@omni_accent`, ...) |
| Launcher corrections (Brave actions) | `TWEAKS` in `apps.rs` |
| Folders walked for file names | `name_roots` in `files.rs` |
| Folders whose contents are not indexed | `SKIP_INSIDE` in `files.rs` |
| How long the browser stays after the last search | `IDLE_EXIT` in `web_client.rs` (10 s) |
| Claude's agent instructions | `src/agent_prompt.md` |

Rebuild with `./install.sh` after changing source files.

### Switching back to rofi

Point your keybind back at your previous launcher. On ML4W dotfiles, put `rofi` in `~/.config/ml4w/settings/launcher`.

---

## Troubleshooting

```bash
~/.local/share/omni/bin/omni-toggle --ping; echo $?   # 0 = omni is running and healthy
~/.local/share/omni/bin/omni --search firefox         # app and app action ranking, no window
~/.local/share/omni/bin/omni --web "some query"       # the URL a web search would open
~/.local/share/omni/bin/omni --recent                 # what shows with nothing typed
~/.local/share/omni/bin/omni --files plan readme      # build the file index here and time searches
~/.local/share/omni/bin/omni --check-env              # display and Hyprland environment, focused monitor
journalctl --user -t ensure-running | grep omni       # start and health-check log
```

| Variable | Effect |
|---|---|
| `OMNI_SEARCH_TIMING=1` | Log web search startup and page-load timings (no queries) |
| `OMNI_SEARCH_DIAGNOSTICS=1` | Log web search endpoint status and streaming metadata (no queries or answers) |
| `OMNI_CEF_DIR` (build time) | Use another CEF installation |

**Tests**

```bash
PKG_CONFIG=/usr/bin/pkg-config cargo test --release
python3 -B scripts/check_embed.py
```

The Python checks use temporary folders and the standard library; they do not download
models, load the GPU runtime, or modify your image library. The regular Rust suite does
not require a display. Run the GTK control/CSS and ripgrep integration checks explicitly
from a desktop session with `rg` installed:

```bash
PKG_CONFIG=/usr/bin/pkg-config cargo test --release --bin omni -- --ignored --test-threads=1
```

These checks do not open a window or simulate pointer input. Visual feel, live Chromium
navigation, and model inference still need separate runtime verification.

---

## Architecture

This section covers how omni works and why. You do not need it to use omni.

### Installed files

| Path | Purpose |
|---|---|
| `~/.local/share/omni/bin/omni` | The background program: window, search, Claude |
| `~/.local/share/omni/bin/omni-toggle` | What the keybind runs. No GTK; opens or closes the popup in about 1 ms |
| `~/.local/share/omni/bin/omni-browser` | The browser process for web search, and its helpers |
| `~/.local/share/omni/bin/omni-start` | Starts omni through `ensure-running` |
| `~/.local/share/omni/bin/omni-embed.py` | The image search helper |
| `~/.local/share/omni/embed-venv` | Its Python environment |
| `~/.local/share/omni/models` | The image search model |
| `~/.local/share/omni/hypr/omni.lua` | Hyprland layer rule for the popup |

Installed files are read-only. The keybind helper is a separate program so a keypress costs about 1 ms instead of the roughly 60 ms it takes to load GTK.

### Source files

| File | Purpose |
|---|---|
| `src/main.rs` | Window, results list, keys, menus, settings, control socket, monitor placement |
| `src/apps.rs` | App list, matching, app actions and `TWEAKS`, launch history |
| `src/recent.rs` | Recent files and folders |
| `src/files.rs` | The file-name index (walk, search, ranking) and contents search |
| `src/sparql.rs` | Minimal LocalSearch bindings (queries, change events) |
| `src/services.rs` | systemd services: list, search, actions, status |
| `src/calc.rs` | The calculator (strict parser; complete expressions only) |
| `src/images.rs` | Image search: helper process, crawl scheduling, results |
| `scripts/omni-embed.py` | Image search helper: model, index, thumbnails |
| `src/answer.rs` | Ask Claude: conversation process, permission prompts, model chip, Markdown |
| `src/agent_prompt.md` | Your agent instructions, appended to Claude's system prompt |
| `src/web.rs` | Default browser and search engine detection |
| `src/search_view.rs` | The web search panel |
| `src/search_provider.rs` | Built-in search engines |
| `src/search_filters/` | Brave's page style and preference script |
| `src/web_client.rs` | omni's side of the browser: starts and stops it, shows frames, sends input |
| `src/web_protocol.rs` | Messages between omni and the browser process |
| `src/chromium.rs` | The browser process itself (CEF, private sessions, Brave preferences) |
| `src/browser_frame.rs` | Frame size checks |
| `src/glass.rs` | The card shell |
| `src/ring.rs` | Search bar overlay (currently draws nothing) |
| `src/style.css` | The look |
| `src/dock.rs` | Pin or unpin apps on the quickshell dock |
| `src/util.rs` | Running helper programs with a time limit |
| `src/probe.rs` | Opt-in responsiveness diagnostics |
| `src/bin/omni-toggle.rs` | The keybind helper |
| `src/bin/omni-browser.rs` | The browser process and its helpers |
| `scripts/omni-start` | Starts omni through `ensure-running` |

### Desktop integration

omni needs three things from your Hyprland configuration (Lua config shown; adapt to your setup):

| What | How |
|---|---|
| **Keybind** | Bind a key of your choice to run `~/.local/share/omni/bin/omni-toggle`. |
| **Autostart** | Run `omni-start` at login: `hl.exec_cmd(os.getenv("HOME") .. "/.local/share/omni/bin/omni-start")` |
| **Layer rule** | Load the installed rule: `dofile(os.getenv("HOME") .. "/.local/share/omni/hypr/omni.lua")` |

Starting omni from the autostart file also restarts it on every `hyprctl reload`, which `ensure-running` turns into a no-op while it is healthy.

**On ML4W dotfiles:** put `omni` in `~/.config/ml4w/settings/launcher`, and add an `omni` branch to `~/.config/hypr/scripts/launcher.sh` that runs `omni-toggle` (with rofi as the fallback). The launcher keybind then opens omni. These files belong to the ML4W dotfiles, so an ML4W update can overwrite them; re-add the changes if the old launcher returns.

### Reliability

The popup holds the keyboard exclusively while open, so a frozen omni would lock the desktop. Several layers prevent that. Only one instance runs (GTK app id `dev.omni.Launcher`).

**Starting and health checks**

- `ensure-running` starts omni with `--wait-wayland` and checks that it has a Wayland display and answers `omni-toggle --ping`. Unhealthy, zombie or duplicate instances are replaced, at most 3 starts in 5 minutes.
- At early login, Hyprland can start programs before `WAYLAND_DISPLAY` and `HYPRLAND_INSTANCE_SIGNATURE` exist. omni forces `GDK_BACKEND=wayland` and fills missing values from `hyprctl instances`, waiting up to 5 s, then exits with an error instead of hanging.
- omni replies to each toggle before acting on it (about 1.2 ms round trip). No reply within 1 s makes `omni-toggle` run `omni-start`, which replaces a hung instance; a notification appears if that fails.
- `hyprctl` calls are killed after 0.5 s.
- The popup releases the keyboard before it hides or launches anything.

**Keyboard watchdog:** while the popup is open, the UI thread checks in every 100 ms. After 2 s of silence, a separate thread exits omni, Hyprland drops the popup and the keyboard is free. The next keypress starts a fresh omni.

**Event floods:** a LocalSearch re-index sends an event for every file (about 94,000 a minute). The UI thread only queues them; one worker resolves them in batches and keeps only files changed since the last walk. The queue is capped at 50,000; past that, omni drops it and re-walks. More than 20,000 new files also triggers an early re-walk.

**Slow disks:** checks that touch the disk run on background threads with time limits. A dead network mount skips a result instead of stalling the popup.

### The file-name index

File names come from omni's own in-memory index, similar to Everything or FSearch, because LocalSearch returns unranked results at this size and ranked queries take seconds.

**What it walks:** home, `/usr`, `/etc`, `/opt`, `/var`, `/srv`, `/mnt`, `/run/media/$USER`, and anything LocalSearch covers (`name_roots` in `files.rs`). Cache folders and the contents of `SKIP_INSIDE` folders are skipped. Hidden files are included but ranked low.

| | |
|---|---|
| Walk | Parallel, at `nice 10`: about 2.9 million entries in about 2.7 s |
| Refresh | Every 30 minutes, plus live updates from LocalSearch |
| Search | SIMD substring scan over all distinct names, split across cores: about 1.5 to 4 ms per keystroke |
| Memory | About 42 MB for 2.9 million entries |

- Each distinct name is stored once, in lowercase (only 29% of names are distinct). Entries are grouped by name, so each is a parent link and a flags byte.
- The real capitalisation of the few results shown is read back from disk, which also confirms the file still exists. This runs on a background thread and gives up after 200 ms.

### LocalSearch

LocalSearch (GNOME's indexer) provides **Contents** search and live new-file events.

Recommended setup:

- Index your home folder and data drives (for example `/run/media/$USER` or a storage mount), not system folders. Indexing system folders kept LocalSearch at about 1.3 cores in testing and delayed its change events.
- Skip cache and `.git` folders (omni still indexes their names), and turn on removable drives if you want them searched.
- Raise the inotify watch limit for large homes, for example `fs.inotify.max_user_watches = 1048576` in a file under `/etc/sysctl.d/`.
- Change LocalSearch settings with the system `gsettings` (`/usr/bin/gsettings`). Another `gsettings` on your `PATH`, such as Homebrew's, may write somewhere LocalSearch never reads.

### GPU selection

omni draws on the GPU that drives your screens and loads only that driver. Some desktop setups (ML4W among them) set `__GLX_VENDOR_LIBRARY_NAME=nvidia` for the whole session, which made omni load about 33 MB of unused NVIDIA driver and could wake the discrete GPU. When every connected screen is on an Intel or AMD GPU, omni re-executes itself once at startup with Mesa's EGL, GLX and Vulkan drivers (`OMNI_GPU_ENV` marks the second run). If a screen is on NVIDIA, nothing changes.

The image search helper is the exception: it uses CUDA on the NVIDIA GPU, and omni removes its display-driver overrides from the helper's environment.

### Web search internals

- **Engine:** Brave Search in a GTK view backed by the Chromium Embedded Framework (CEF), opening Brave's `/ask` view. A page filter hides Brave's navigation header and history sidebar while keeping answers, sources and the follow-up prompt. Other engines would need their own filters.
- **Process:** Chromium runs in `omni-browser --omni-host`, not inside omni. Pages reach omni as frames over a socket, and input goes back the same way. It exits 10 s after the last Brave page closes; the next search starts it again in about 1.2 s.
- **Privacy:** each search is an isolated in-memory session, shared by its follow-ups until Back, <kbd>Esc</kbd> or closing the popup. Before navigation omni sets Brave's `safesearch=off` and `useLocation=0` cookies and `app.usageMetricsOptOut=true` in localStorage. Cookies, history, localStorage, IndexedDB, caches and service workers are never reused. CEF's installation metadata lives in a private temporary directory removed on exit, and leftovers from a killed process are removed on the next start. Brave's own server-side retention is outside omni's control.
- **Network:** QUIC is disabled, so requests use HTTP/2. Conversation creation was intermittently rejected with HTTP 400 over HTTP/3 in testing.
- **Appearance:** page chrome and scrollbar tracks are hidden at document creation; wheel and trackpad scrolling still work. A spinner shows on a blank pane in the page's colour, then crossfades into Brave's first frame. Reduced-motion preferences are respected. The search header uses the bundled official Brave favicon.
- **Requirements:** the system CEF library must support API 14600; Fedora's `cef` package is used by default.
- **Benchmarks:** `cargo run --example chromium-load-timing -- --local` runs a local sequential-request benchmark; `--prewarm` starts the browser process before the first search.

### Image search internals

- **Model:** [embeddinggemma-2](https://huggingface.co/google/embeddinggemma-2) in ONNX form (about 1.05 GB), downloaded on the first crawl to `~/.local/share/omni/models`. It runs in `omni-embed.py` from its own virtual environment, built by `install.sh` with `uv`; onnxruntime-gpu brings its own CUDA and cuDNN.
- **Indexing** uses the fp16 vision and text models on the GPU, embeds only new or changed images, then exits. A crawl with nothing new never loads the model.
  - The hourly crawl runs at low priority, one image at a time (about 0.23 s each).
  - **Index now** and adding a folder run at full speed, decoding on worker threads while the GPU encodes (about 0.13 s each), when at least 1.8 GB of VRAM and 3 GB of RAM are free. Otherwise they use the gentle pace.
- **Querying** uses the 4-bit text model (174 MB on disk) on the GPU. It returned the same top results as fp16 in testing. Typing `img` starts the helper (about 2 to 3 s); each query then takes about 7 ms; it exits 10 s after the last query.
- **Memory while querying:** about 290 MB of VRAM and about 960 MB of RAM. Most of that RAM is CUDA's own runtime (cuBLAS loads its GPU kernels into memory on the first query), not the model, and about 340 MB of it is shared library pages the kernel can reclaim under pressure. A CPU version would need about 250 MB but would cost CPU time on every keystroke.
- **Storage:** the index is `~/.local/share/omni/images/index.npz` (paths, modification times and vectors), written atomically. Thumbnails are 160 px PNGs in `~/.cache/omni/thumbs-160`. A corrupt index is set aside and rebuilt, and vectors for an unmounted folder are kept until it returns.

### Claude: permissions and sandbox

omni runs `claude -p --input-format stream-json --no-session-persistence`, one process per conversation, with each message sent as a JSON line on stdin. It loads no local settings, plugins or hooks (`--setting-sources ""`), and runs with `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`.

**Permission prompts** use Claude Code's host permission protocol (`--permission-prompt-tool stdio`).

- Read-only work runs freely. Commands that change things, edits, writes and web fetches ask. Web search is pre-approved. Fetches ask because a fetched page could otherwise trick Claude into sending data out in a URL.
- Each prompt shows the exact command, file content or URL from the tool's input, never Claude's own description. Invisible and direction-changing Unicode characters are shown as codes such as `⟨U+202E⟩`.
- **Deny** has focus; **Allow once** and **Always allow** ignore input for the first 0.5 s. **Always allow** covers only the exact command or file on screen, for this conversation only, and is never written to a settings file. For an edit, it covers that one file.
- <kbd>Esc</kbd>, clicking the dim area, closing omni and a Claude crash all deny. Prompts queue and are answered in order.

**Sandbox**

- Shell commands run in Claude Code's bubblewrap sandbox with no unsandboxed fallback, and sandboxed commands still ask.
- Secrets are hidden from shell commands and denied to the Read and Edit tools: SSH, GPG, cloud and Kubernetes credentials, keyrings and `pass`, Claude's own credentials, `.netrc`, Docker, git, npm, PyPI, Cargo, Maven, Gradle, Terraform, Vault and database credentials, password managers, browser profiles, Thunderbird, Bitwarden, Discord, Signal and shell histories.
- Shell commands cannot write to `~/.config`, shell startup files, `.desktop` launchers, user systemd and D-Bus services, Python's user site-packages, git config, omni's files or the PATH folders in your home, since those run outside the sandbox later. Claude's Edit and Write tools can still change them, always with a prompt.
- omni drops `CLAUDE_CODE_*`, `ANTHROPIC_*` and anything named like a token, secret, password or key from its environment at startup.
- Claude runs in its own systemd scope (`omni-claude-*.scope`). Ending the conversation kills the scope, including commands that detached.

**Connectors:** your claude.ai connectors load (Calendar, Notion and so on) and ask like any other tool. Claude Code sometimes starts before the connector list arrives; when a start reports no connectors, omni restarts it and resends the message before any text appears, up to 3 times.

**Known gap:** `~/.bashrc` is a symlink directly in your home folder, and home itself cannot be read-only without blocking every write. If your login shell is not bash, it rarely reads that file.

---