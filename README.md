# beewm

A Wayland tiling window manager written in Rust.

## Features

- Wayland compositor backend
- Dwindle tiling layout
- Optional master-stack layout
- i3-style numbered workspaces
- i3-style text configuration

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
