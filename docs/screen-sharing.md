# Screen sharing & screen recording on beewm

This explains how screen sharing/recording works under beewm, how to set it up,
and how to debug it. It covers OBS, Chromium/Firefox WebRTC, Discord/Slack/Zoom,
and any other app that uses the `org.freedesktop.portal.ScreenCast` portal.

## How it works (and why "no screen was detected" happens)

On Wayland an app cannot read the screen directly. Screen sharing goes through a
fixed pipeline:

1. The app calls `org.freedesktop.portal.ScreenCast` on **xdg-desktop-portal**.
2. xdg-desktop-portal forwards the request to an
   `org.freedesktop.impl.portal.ScreenCast` **backend** for the current desktop.
3. The user picks an output. The backend captures frames from the compositor.
4. The backend publishes the frames as a **PipeWire** stream the app reads.

beewm's role is step 3's compositor side. beewm already implements the
wlroots screen-capture protocol **`zwlr_screencopy_manager_v1` (v3)** plus
**`xdg-output`**, which is exactly what the **`xdg-desktop-portal-wlr`** backend
consumes. So beewm does **not** need its own PipeWire/portal backend — it reuses
the maintained `xdg-desktop-portal-wlr` (xdpw) backend, the same one sway and
other wlroots-style compositors use.

The reason a fresh beewm session shows **no screen** to apps is almost never a
missing protocol. It is that **xdg-desktop-portal and PipeWire are
D-Bus/systemd-activated services**, not children of beewm, so they never inherit
`WAYLAND_DISPLAY` / `XDG_CURRENT_DESKTOP` and have no idea which display to
capture. beewm now fixes this at startup by pushing the session environment into
the D-Bus activation environment and the systemd user manager (see
"What beewm does automatically" below).

### Why capture never shows black frames

beewm's capture path **re-composites an independent frame** for the capture
buffer (windows + layers + borders + optional cursor), rather than reading the
scanned-out framebuffer. So even when a fullscreen game is on a hardware plane
via direct scanout, the capture still renders the same content into its own
buffer — no black frames, and no need to disable direct scanout while recording.
When nothing is capturing, the path is completely inert (no extra repaints, no
FPS impact on games).

## One-time setup

### 1. Install the portal stack

You need xdg-desktop-portal plus the `wlr` backend (ScreenCast) and a fallback
backend (gtk) for the other portals:

```sh
# Arch
sudo pacman -S xdg-desktop-portal xdg-desktop-portal-wlr xdg-desktop-portal-gtk pipewire wireplumber
# Fedora
sudo dnf install xdg-desktop-portal xdg-desktop-portal-wlr xdg-desktop-portal-gtk pipewire wireplumber
# Debian/Ubuntu
sudo apt install xdg-desktop-portal xdg-desktop-portal-wlr xdg-desktop-portal-gtk pipewire wireplumber
```

### 2. Install the beewm portal routing

This tells xdg-desktop-portal to use the `wlr` backend for ScreenCast when
running under beewm:

```sh
./portal/install.sh
```

Or by hand:

```sh
mkdir -p ~/.config/xdg-desktop-portal ~/.config/xdg-desktop-portal-wlr
cp portal/beewm-portals.conf       ~/.config/xdg-desktop-portal/beewm-portals.conf
cp portal/xdg-desktop-portal-wlr.conf ~/.config/xdg-desktop-portal-wlr/config
```

`beewm-portals.conf` routes ScreenCast/RemoteDesktop to `wlr`; the
xdg-desktop-portal-wlr `config` controls how the output is chosen (default:
auto-pick on a single monitor — edit it for a multi-monitor picker).

### 3. Restart the portal stack (first time only)

The portal may already be running from before you set the routing/env. Restart
it once (or just log out and back in):

```sh
systemctl --user restart xdg-desktop-portal xdg-desktop-portal-wlr pipewire wireplumber
```

## What beewm does automatically

At startup (once outputs are ready and XWayland has settled) beewm:

- sets `XDG_CURRENT_DESKTOP=beewm` and `XDG_SESSION_DESKTOP=beewm` for its
  children, and
- runs `dbus-update-activation-environment --systemd …` and
  `systemctl --user import-environment …` to push
  `WAYLAND_DISPLAY`, `DISPLAY`, `XDG_CURRENT_DESKTOP`, `XDG_SESSION_DESKTOP`,
  `XDG_SESSION_TYPE`, `XDG_RUNTIME_DIR` into the D-Bus/systemd environment.

You can watch this happen:

```sh
RUST_LOG=beewm::portal=info beewm   # logs the exported keys at startup
```

To disable the export (debugging only): `BEEWM_NO_SESSION_ENV_EXPORT=1`.

## Verifying

```sh
# The portal and PipeWire are up
systemctl --user status xdg-desktop-portal xdg-desktop-portal-wlr pipewire wireplumber

# The portal saw the right desktop + env (should list ScreenCast)
busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop \
  | grep -i screencast

# Which impl backend got selected for ScreenCast (look for "wlr")
RUST_LOG= systemctl --user status xdg-desktop-portal | grep -i screencast

# The env actually reached the activation environment
systemctl --user show-environment | grep -E 'WAYLAND_DISPLAY|XDG_CURRENT_DESKTOP'

# PipeWire nodes appear while a capture is active
pw-cli ls Node ; pw-dump
```

A ready-made portal test client:

```sh
# from xdg-desktop-portal's tests, or the standalone package
ld-portal-test screencast    # or: /usr/libexec/xdg-desktop-portal-validate-icon ...
```

## Per-app testing

- **OBS**: Add a "Screen Capture (PipeWire)" source → pick the monitor → it
  should preview the desktop and update smoothly. Removing the source releases
  the stream.
- **Chromium/Chrome**: a "Share your screen" prompt should list the monitor as
  an "Entire screen" source. Ensure Chromium uses Wayland
  (`--ozone-platform-hint=auto`; beewm sets `ELECTRON_OZONE_PLATFORM_HINT` and
  `NIXOS_OZONE_WL` for children already).
- **Firefox**: screen-share picker should list the monitor (Firefox uses the
  portal automatically on Wayland).
- **Discord/Slack (Electron)**: Go Live / screen share should list the monitor.

## Diagnostics / log targets

beewm logs under these targets (enable with `RUST_LOG`):

| target                | what it shows                                          |
|-----------------------|--------------------------------------------------------|
| `beewm::portal`       | session-env export to D-Bus/systemd, exported keys     |
| `beewm::screencast`   | capture frame requests, geometry/stride/scale/format, frame submission, output-removal cleanup |
| `beewm::output`       | output add/remove/mode (existing)                      |

Example:

```sh
RUST_LOG=beewm::portal=trace,beewm::screencast=trace beewm
```

PipeWire/portal side:

```sh
RUST_LOG= dbus-monitor --session "interface='org.freedesktop.portal.ScreenCast'"
WAYLAND_DEBUG=1 obs    # or your capture client
journalctl --user -u xdg-desktop-portal -u xdg-desktop-portal-wlr -f
```

## Troubleshooting

**"No screen / nothing to share" in the picker**
- Confirm the env reached the bus: `systemctl --user show-environment | grep WAYLAND_DISPLAY`.
  If empty, beewm's export didn't run or was disabled — check
  `RUST_LOG=beewm::portal=info` output and that `BEEWM_NO_SESSION_ENV_EXPORT` is unset.
- Restart the portal once after the first login under beewm
  (`systemctl --user restart xdg-desktop-portal xdg-desktop-portal-wlr`); a
  portal started earlier won't have the env.
- Confirm `xdg-desktop-portal-wlr` is installed and that
  `~/.config/xdg-desktop-portal/beewm-portals.conf` exists.

**Picker appears but capture is black / fails**
- Check `pipewire`/`wireplumber` are running (`systemctl --user status pipewire`).
- Watch `RUST_LOG=beewm::screencast=trace` — you should see "frame requested"
  then "frame submitted" lines while capturing.

**Wrong monitor / multi-monitor selection**
- Set a chooser in `~/.config/xdg-desktop-portal-wlr/config`
  (`chooser_type=dmenu`, `chooser_cmd=slurp -f %o -or`).

**Chromium/Electron shows X11 capture or nothing**
- Ensure it runs on Wayland (Ozone). beewm exports the hint vars to children;
  for manually launched apps add `--ozone-platform-hint=auto`.

## Scope & limitations

- **Monitor capture** is the supported milestone. Multi-monitor works to the
  extent beewm exposes multiple `wl_output`s; the active output is always
  capturable.
- **Window/toplevel capture** is not implemented yet. The architecture does not
  preclude it (it would need a foreign-toplevel + per-toplevel capture protocol
  or a native portal backend), but it is out of scope for now.
- **Zero-copy dmabuf** capture is not implemented; capture uses an offscreen
  re-composite + readback into a PipeWire shm buffer. Correct first, optimized
  later. This is per-frame work only while a capture is active.
- Screenshot tooling that speaks `zwlr_screencopy` directly (grim, wf-recorder,
  `grimblast`) keeps working unchanged — it uses the same protocol.
