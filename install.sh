#!/usr/bin/env bash
# Build omni, install it to ~/.local/share/omni and restart it. Safe to re-run.
set -euo pipefail

src="$(cd "$(dirname "$0")" && pwd)"
dest="$HOME/.local/share/omni"
runtime="${XDG_RUNTIME_DIR:-}"

die() { echo "install: $*" >&2; exit 1; }

command -v cargo >/dev/null 2>&1 || die "cargo not found; install Rust from https://rustup.rs"

# A pkg-config from Homebrew/linuxbrew can't see the system GTK .pc files: prefer the system one.
if [ -z "${PKG_CONFIG:-}" ] && [ -x /usr/bin/pkg-config ]; then
    export PKG_CONFIG=/usr/bin/pkg-config
fi

# --target-dir: a CARGO_TARGET_DIR from the environment would leave the copied binaries stale.
cargo build --release --manifest-path "$src/Cargo.toml" --target-dir "$src/target" \
    || die "build failed; see Requirements in README.md for the system packages"

mkdir -p "$dest/bin" "$dest/hypr"

# Temp name then rename: never a half-written file, and the running daemon keeps its old one.
put() { # put SRC DEST MODE
    install -m "$3" "$1" "$2.new"
    mv -f "$2.new" "$2"
}
put "$src/target/release/omni"         "$dest/bin/omni"          0555
put "$src/target/release/omni-browser" "$dest/bin/omni-browser"  0555
put "$src/target/release/omni-toggle"  "$dest/bin/omni-toggle"   0555
put "$src/scripts/omni-start"          "$dest/bin/omni-start"    0555
put "$src/hypr/omni.lua"               "$dest/hypr/omni.lua"     0444
put "$src/scripts/omni-embed.py"       "$dest/bin/omni-embed.py" 0444

# Image search venv (~2.6 GB), rebuilt only when the pinned requirements change.
venv="$dest/embed-venv"
reqs="$src/scripts/omni-embed.requirements.txt"
if ! cmp -s "$reqs" "$venv/omni-requirements.txt" 2>/dev/null; then
    if command -v uv >/dev/null 2>&1; then
        echo "install: setting up image search venv (one time, a few minutes)"
        rm -rf "$venv.new"
        if uv venv -q --python 3.12 "$venv.new" && VIRTUAL_ENV="$venv.new" uv pip install -q -r "$reqs"; then
            cp "$reqs" "$venv.new/omni-requirements.txt"
            rm -rf "$venv"
            mv "$venv.new" "$venv"
        else
            echo "install: image search venv failed; img search stays off until the next install" >&2
            rm -rf "$venv.new"
        fi
    else
        echo "install: uv not found, skipping image search (needs uv: https://docs.astral.sh/uv/)" >&2
    fi
fi

if [ "${OMNI_INSTALL_NO_RESTART:-0}" = 1 ]; then
    echo "omni installed to $dest (restart managed by caller)"
    exit 0
fi

omni_pids() { # omni_pids live|zombie
    local pid state
    for pid in $(pgrep -x omni 2>/dev/null || true); do
        state=$(awk '{print $3}' "/proc/$pid/stat" 2>/dev/null || true)
        case "$1:$state" in
            zombie:Z) echo "$pid" ;;
            live:Z | live:X | live:) ;;
            live:*) echo "$pid" ;;
        esac
    done
}

# Quit cleanly, then TERM, then KILL. Never start a second instance next to a zombie:
# it can't get the D-Bus name and wedges.
stop_omni() {
    if command -v gapplication >/dev/null 2>&1; then
        gapplication quit dev.omni.Launcher 2>/dev/null || true
    fi
    local pids
    pids=$(omni_pids live | tr '\n' ' ')
    if [ -n "${pids// /}" ]; then
        # shellcheck disable=SC2086
        kill -TERM $pids 2>/dev/null || true
        for _ in $(seq 50); do
            [ -z "$(omni_pids live)" ] && break
            sleep 0.1
        done
        pids=$(omni_pids live | tr '\n' ' ')
        if [ -n "${pids// /}" ]; then
            echo "install: omni still alive after TERM, sending KILL" >&2
            # shellcheck disable=SC2086
            kill -KILL $pids 2>/dev/null || true
            sleep 0.2
        fi
    fi
    [ -n "$runtime" ] && rm -f "$runtime/omni.sock"

    local zombies
    zombies=$(omni_pids zombie | tr '\n' ' ')
    if [ -n "${zombies// /}" ]; then
        die "binaries updated, but a zombie omni is still present (${zombies}). Restart the session, then run: $dest/bin/omni-start"
    fi
    [ -z "$(omni_pids live)" ] || die "omni still running after SIGKILL; not restarting"
}

wait_for_ping() { # wait_for_ping TRIES
    for _ in $(seq "$1"); do
        timeout 2 "$dest/bin/omni-toggle" --ping >/dev/null 2>&1 && return 0
        sleep 0.2
    done
    return 1
}

# A deliberate install isn't a crash loop: don't count it against ensure-running's start limit.
rm -f "${runtime:-/tmp}/ensure-running/omni.starts"
stop_omni

# Start through Hyprland so omni gets the desktop's environment, not this shell's (which may
# hold tokens from wherever install.sh was run). hyprctl can exit 0 on a dispatcher it doesn't
# know, so fall back to starting directly if omni doesn't answer.
started=0
if command -v hyprctl >/dev/null 2>&1 \
    && hyprctl dispatch "hl.dsp.exec_cmd(\"$dest/bin/omni-start\")" >/dev/null 2>&1 \
    && wait_for_ping 15; then
    started=1
fi
if [ "$started" = 0 ]; then
    "$dest/bin/omni-start" && wait_for_ping 30 && started=1
fi

if [ "$started" = 1 ]; then
    echo "omni installed to $dest and running"
    exit 0
fi
die "omni installed to $dest but is not answering; run $dest/bin/omni --daemon to see why"
