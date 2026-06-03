#!/usr/bin/env bash
#
# Launch beewm with the frame-pacing diagnostics enabled, capture them to a
# timestamped log, and print the decisive lines when beewm exits.
#
# Run this from a bare TTY (Ctrl+Alt+F3, log in), NOT from inside an existing
# desktop session — the udev/DRM backend (the one games use) only runs without
# a DISPLAY/WAYLAND_DISPLAY. Then open Steam, run the game for ~20-30s while
# moving around, and quit beewm. The summary it prints (and the full log path)
# is what to share.
#
# Usage:
#   ./scripts/beewm-fps-diag.sh                 # uses ./target/release/beewm
#   ./scripts/beewm-fps-diag.sh /path/to/beewm  # explicit binary
#   BEEWM_FRAME_CALLBACK_AT_VBLANK=1 ./scripts/beewm-fps-diag.sh   # A/B: fix OFF
#
set -u

# --- locate the beewm binary -------------------------------------------------
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
REPO_DIR="$(dirname -- "$SCRIPT_DIR")"

if [ "${1:-}" != "" ]; then
    BEEWM_BIN="$1"
elif [ -x "$REPO_DIR/target/release/beewm" ]; then
    BEEWM_BIN="$REPO_DIR/target/release/beewm"
elif command -v beewm >/dev/null 2>&1; then
    BEEWM_BIN="$(command -v beewm)"
else
    echo "error: beewm binary not found (built it with 'cargo build --release'?)" >&2
    echo "       pass the path explicitly: $0 /path/to/beewm" >&2
    exit 1
fi

# --- diagnostics -------------------------------------------------------------
LOG="/tmp/beewm-fps-$(date +%Y%m%d-%H%M%S).log"

# Each target answers one question (see docs/diagnosing-low-fps.md):
#   presentation -> is the compositor presenting at the real refresh rate?
#   commit       -> how fast does the game commit / respond to callbacks, dmabuf?
#   dmabuf       -> did it fall back to slow shm buffers?
#   sync         -> are explicit-sync fences stalling it?
#   frame        -> direct scanout vs GPU composition, XWayland or not?
export RUST_LOG="${RUST_LOG:-beewm::presentation=info,beewm::commit=info,beewm::dmabuf=warn,beewm::sync=info,beewm::frame=info}"

echo "beewm binary : $BEEWM_BIN"
echo "RUST_LOG     : $RUST_LOG"
echo "frame cb mode: ${BEEWM_FRAME_CALLBACK_AT_VBLANK:+LEGACY at-vblank (fix OFF)}"
echo "log file     : $LOG"
echo
echo "Launching beewm. Open Steam + your game, play ~20-30s, then quit beewm."
echo

# Run it. stderr (where tracing writes) goes to the log; stdout still shows on
# screen. We don't `tee` because beewm takes over the display.
"$BEEWM_BIN" 2>"$LOG"
STATUS=$?

# --- summary on exit ---------------------------------------------------------
echo
echo "================= beewm exited (status $STATUS) ================="
echo "Full log: $LOG"
echo
echo "----- decisive lines (share these) -----"
grep -E "selected output mode|present cadence|surface commit rate|frame-stats|primary-plane path changed|explicit-sync fence|shm buffers" "$LOG" \
    | tail -n 80
echo "----------------------------------------"
echo "If that looks empty, the game may not have rendered long enough, or the"
echo "udev backend didn't start (are you on a bare TTY?). See the full log."
