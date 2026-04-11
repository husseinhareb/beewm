# beewm

A tiling window manager written in Rust.

## Features

- X11 support (via x11rb)
- Wayland support (planned)
- Master-stack tiling layout
- i3-style numbered workspaces
- TOML configuration

## Building

```sh
cargo build --release
```

## Configuration

Configuration is loaded from `~/.config/beewm/config.toml`.
