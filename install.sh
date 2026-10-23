#!/usr/bin/env bash
# Build omni and install it to ~/.local/share/omni, then restart the background process.
# Safe to re-run: files are replaced atomically and installed read-only.
set -euo pipefail

src="$(cd "$(dirname "$0")" && pwd)"
dest="$HOME/.local/share/omni"
runtime="${XDG_RUNTIME_DIR:-}"

# linuxbrew's pkg-config shadows Fedora's and can't see the system GTK .pc files.
# --target-dir: a CARGO_TARGET_DIR from the environment would build elsewhere and leave
# the binaries copied below stale.
PKG_CONFIG=/usr/bin/pkg-config cargo build --release --manifest-path "$src/Cargo.toml" --target-dir "$src/target"

mkdir -p "$dest/bin" "$dest/hypr"

# Install to a temp name then rename: never a half-written binary, and the running
# daemon keeps its old (unlinked) file until it restarts.
put() { # put SRC DEST MODE
    install -m "$3" "$1" "$2.new"
    mv -f "$2.new" "$2"
}
put "$src/target/release/omni"        "$dest/bin/omni"        0555
put "$src/target/release/omni-toggle" "$dest/bin/omni-toggle" 0555
put "$src/scripts/omni-start"         "$dest/bin/omni-start"  0555
put "$src/hypr/omni.lua"              "$dest/hypr/omni.lua"   0444

omni_state() { awk '{print $3}' "/proc/$1/stat" 2>/dev/null || true; }

# Live (non-zombie) omni PIDs. pgrep alone also matches zombies — those must not
# be treated as "still shutting down" or as a successful wait.
omni_live_pids() {
    local pid state
    for pid in $(pgrep -x omni 2>/dev/null || true); do
        state=$(omni_state "$pid")
        case "$state" in
            ""|Z|X) ;;
            *) printf '%s\n' "$pid" ;;
        esac
    done
}

omni_zombie_pids() {
    local pid
    for pid in $(pgrep -x omni 2>/dev/null || true); do
        case "$(omni_state "$pid")" in
            Z) printf '%s\n' "$pid" ;;
        esac
    done
}

# Stop the resident daemon without leaving a zombie + wedged D-Bus name.
# Prefer a clean GApplication quit, then TERM, then KILL. Never start a
# replacement if a zombie remains — that is what broke the last install.
stop_omni() {
    # Ask GTK to quit cleanly so the app-id and socket are released.
    if command -v gapplication >/dev/null 2>&1; then
        gapplication quit dev.omni.Launcher 2>/dev/null || true
    fi

    local pids
    pids=$(omni_live_pids | tr '\n' ' ')
    if [ -n "${pids// /}" ]; then
        # shellcheck disable=SC2086
        kill -TERM $pids 2>/dev/null || true
        for _ in $(seq 50); do
            [ -z "$(omni_live_pids)" ] && break
            sleep 0.1
        done
        pids=$(omni_live_pids | tr '\n' ' ')
        if [ -n "${pids// /}" ]; then
            echo "install: omni still alive after TERM, sending KILL" >&2
            # shellcheck disable=SC2086
            kill -KILL $pids 2>/dev/null || true
            sleep 0.2
        fi
    fi

    # Drop a stale control socket so the next ping cannot hang on a dead peer.
    if [ -n "$runtime" ]; then
        rm -f "$runtime/omni.sock"
    fi

    local zombies
    zombies=$(omni_zombie_pids | tr '\n' ' ')
    if [ -n "${zombies// /}" ]; then
        echo "install: binaries updated at $dest, but zombie omni still present (${zombies})." >&2
        echo "install: not starting a second instance (that wedges D-Bus). Reap or restart the session, then run: $dest/bin/omni-start" >&2
        exit 1
    fi

    if [ -n "$(omni_live_pids)" ]; then
        echo "install: omni still running after SIGKILL; aborting restart" >&2
        exit 1
    fi
}

# Restart: stop the old instance, start the new one through ensure-running.
# A deliberate install isn't a crash loop, so it doesn't count toward ensure-running's
# "3 starts in 5 minutes" guard (which stays in force for login/reload/keypress starts).
rm -f "${runtime:-/tmp}/ensure-running/omni.starts"
stop_omni

# Started by Hyprland, so omni gets the desktop's environment rather than this shell's
# (run from a Claude Code session, that would include its tokens). omni also scrubs
# secret-looking variables itself. Without Hyprland, start it directly.
if ! hyprctl dispatch "hl.dsp.exec_cmd(\"$dest/bin/omni-start\")" >/dev/null 2>&1; then
    "$dest/bin/omni-start"
fi

for _ in $(seq 30); do
    # Belt and suspenders: even with connect timeouts in omni-toggle, cap the wait.
    if timeout 2 "$dest/bin/omni-toggle" --ping >/dev/null 2>&1; then
        echo "omni installed to $dest and running"
        exit 0
    fi
    sleep 0.2
done
echo "omni installed to $dest but is not answering; see: journalctl --user -t ensure-running" >&2
exit 1
