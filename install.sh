#!/usr/bin/env bash
# Build omni and install it to ~/.local/share/omni, then restart the background process.
# Safe to re-run: files are replaced atomically and installed read-only.
set -euo pipefail

src="$(cd "$(dirname "$0")" && pwd)"
dest="$HOME/.local/share/omni"

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

# Restart: stop the old instance, start the new one through ensure-running.
# A deliberate install isn't a crash loop, so it doesn't count toward ensure-running's
# "3 starts in 5 minutes" guard (which stays in force for login/reload/keypress starts).
rm -f "${XDG_RUNTIME_DIR:-/tmp}/ensure-running/omni.starts"
pkill -x omni || true
for _ in $(seq 20); do pgrep -x omni >/dev/null || break; sleep 0.1; done
# Started by Hyprland, so omni gets the desktop's environment rather than this shell's
# (run from a Claude Code session, that would include its tokens). omni also scrubs
# secret-looking variables itself. Without Hyprland, start it directly.
if ! hyprctl dispatch "hl.dsp.exec_cmd(\"$dest/bin/omni-start\")" >/dev/null 2>&1; then
    "$dest/bin/omni-start"
fi

for _ in $(seq 30); do
    if "$dest/bin/omni-toggle" --ping; then
        echo "omni installed to $dest and running"
        exit 0
    fi
    sleep 0.2
done
echo "omni installed to $dest but is not answering; see: journalctl --user -t ensure-running" >&2
exit 1
