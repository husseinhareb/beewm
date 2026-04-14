# beewm

A tiling window manager written in Rust.

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

## Configuration

Configuration is loaded from `~/.config/beewm/config`.
If the file does not exist, beewm writes a starter config automatically.
