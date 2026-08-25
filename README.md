# beewm

A tiling Wayland compositor written in Rust.

## Features

- Native DRM/KMS backend, plus a nested (winit) backend for development
- Dwindle tiling layout
- Optional master-stack layout
- i3-style numbered workspaces
- i3-style text configuration
- Hold-Super window overview (task view) across all workspaces

## Building

```sh
cargo build --release
```

## Project Layout

The codebase now lives in a single crate and is organized by product modules
instead of by hypothetical backend targets:

- `src/config.rs` for configuration parsing and defaults
- `src/layout/` for tiling algorithms
- `src/model/` for shared window/workspace data structures
- `src/compositor/` for Smithay compositor logic, input handling, rendering, and runtime backends
- `tests/` for integration tests that exercise the public crate API

This keeps the repository aligned with the current scope: one Wayland compositor
with internal modules that can scale without carrying an artificial Xorg/Wayland
split.

## Configuration

Configuration is loaded from `~/.config/beewm/config`.
If the file does not exist, beewm writes a starter config automatically.

Startup commands can be declared with top-level `exec`, `exec_once`, or `autostart`
directives. Each command is launched once when beewm starts.

```text
exec waybar
exec nm-applet

bindsym $mod+Return exec kitty
```

## Window overview

Hold **Super** on its own for ~200 ms and every open window — across all
workspaces — is laid out as a grid of live thumbnails sized to the screen
(10 windows on a 16:9 display come out as 5 columns × 2 rows). While Super is
held, `Tab`/`Shift+Tab`, the arrow keys or the pointer move the selection;
releasing Super switches to the selected window (changing workspace if it lives
on another one) and the grid disappears. `Esc` dismisses it without switching.

Pressing any other key or a mouse button while Super is down cancels it, so
`$mod+…` keybindings and `$mod+drag` never see the grid. Turn it off with:

```text
overview_enabled false
```

## Screen sharing & recording

beewm supports screen sharing/recording (OBS, Chromium/Firefox WebRTC,
Discord/Slack/Zoom, etc.) through `xdg-desktop-portal` + PipeWire, backed by
`xdg-desktop-portal-wlr` and beewm's `zwlr_screencopy` implementation. beewm
exports the session environment to the D-Bus/systemd activation environment at
startup so the (bus-activated) portal can find the display.

One-time setup: install the portal stack and run `./portal/install.sh`. See
[`docs/screen-sharing.md`](docs/screen-sharing.md) for the full setup, testing
(OBS/browsers), and troubleshooting guide.

## Troubleshooting

- **Low in-game FPS / low GPU usage** — see
  [`docs/diagnosing-low-fps.md`](docs/diagnosing-low-fps.md) for the frame-pacing
  model, the `beewm::presentation` / `beewm::commit` / `beewm::dmabuf` /
  `beewm::sync` / `beewm::frame` diagnostic logs, and an A/B procedure to confirm
  the pacing fix on your hardware.
