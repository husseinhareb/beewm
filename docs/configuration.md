# Configuration reference

beewm reads an i3-style, line-based config from `~/.config/beewm/config`
(`$XDG_CONFIG_HOME/beewm/config` when that variable is set). If the file does
not exist, beewm writes a commented starter config there on first launch.

Each non-empty line is one `directive value...`. Unknown directives are errors.

## Contents

- [Syntax](#syntax)
- [Errors and reloading](#errors-and-reloading)
- [Layout and appearance](#layout-and-appearance)
- [Keyboard](#keyboard)
- [Pointer and touchpad](#pointer-and-touchpad)
- [Display, power and lock](#display-power-and-lock)
- [Tray and overview](#tray-and-overview)
- [Animations](#animations)
- [Outputs (multi-monitor)](#outputs-multi-monitor)
- [Autostart](#autostart)
- [Key bindings](#key-bindings)
- [Tray settings file (state.conf)](#tray-settings-file-stateconf)
- [Environment variables](#environment-variables)

## Syntax

```text
# A full-line comment.
set $mod mod4
set $term kitty
set $float_term $term --class floating   # variables can use earlier ones

gap 8                  # trailing comments are allowed
border_color_focused #5588ff

bindsym $mod+Return exec $term
```

- **Comments.** A line starting with `#` is a comment. A trailing comment
  starts at a `#` that has whitespace on both sides, so `#5588ff` is a color,
  not a comment. Command text (see below) is never stripped: the shell
  handles `#` there itself.
- **Variables.** `set $name value` defines a variable. Names are `$` followed
  by letters, digits or `_`. Later lines replace each whole `$name`, so
  `$mod` never matches part of `$mod2`. Using an undefined variable is an error,
  except inside command text, where `$HOME`, `$(...)` and similar are left for
  the shell.
- **Command text** is everything after `exec`, `exec_once`, `autostart`,
  `lock_command`, and the command part of a binding that runs a program.
- **Booleans** accept `true`, `yes`, `on`, `1` and `false`, `no`, `off`, `0`.
- **Colors** are six hex digits: `#5588ff`, `0x5588ff` or `5588ff`.

## Errors and reloading

beewm watches the config file and applies it as soon as it is saved. This also
works when the config is a symlink (for example from stow or chezmoi).

- **A bad save** is rejected as a whole: the previous config stays active.
- **A bad config at startup** does not stop the session. beewm starts with the
  built-in defaults (which bind `$mod+Return` to a terminal) so you can fix the
  file.
- In both cases the error, with its line number, is written to the log and shown
  as a desktop notification (requires `notify-send` and a notification daemon).
- beewm also warns at startup if the config file or its directory can be
  modified by other users, since the config runs commands as you. Fix it with
  `chmod go-w ~/.config/beewm/config`.

Almost every setting applies live. The exceptions:

| Setting | On save |
|---|---|
| `exec` / `exec_once` / `autostart` | Not re-run. New lines run at the next start. |
| `split_ratio` with the dwindle layout | Applies to new splits only; existing splits keep their sizes. |
| `gap`, `screen_timeout`, output `mode` | Apply, unless the tray has set that key (see [state.conf](#tray-settings-file-stateconf)). Changing the key in the config wins over the tray again. |
| `BEEWM_*` environment variables | Read once at startup. |

Lowering `workspaces` live moves the windows of the removed workspaces onto
the new last workspace. It is refused (with a warning) if it would leave fewer
workspaces than connected monitors.

## Layout and appearance

| Directive | Default | Notes |
|---|---|---|
| `layout` | `dwindle` | `dwindle` or `master_stack` (also `master`, `master-stack`). |
| `split_ratio` | `0.50` | 0.05 to 0.95. Alias: `master_ratio`. In master_stack it is the master's share. |
| `gap` | `4` | Pixels around and between tiles, 0 to 1000. |
| `border_width` | `2` | Pixels, 0 to 1000. |
| `border_color_focused` | `#5588ff` | |
| `border_color_unfocused` | `#333333` | |
| `workspaces` | `10` | At least 1. Alias: `num_workspaces`. |

## Keyboard

| Directive | Default | Notes |
|---|---|---|
| `keyboard_layout` | `us` | XKB layouts, comma separated for several (`us,ara`). |
| `keyboard_variant` | (none) | XKB variants, one per layout (`,mac`). |
| `keyboard_options` | (none) | XKB options, comma separated (`grp:alt_shift_toggle,caps:escape`). |
| `keyboard_model` | (none) | XKB model (`pc105`). |
| `repeat_delay` | `200` | Milliseconds before a held key repeats, up to 10000. |
| `repeat_rate` | `25` | Repeats per second, up to 1000. `0` turns repeat off. |

To switch between several layouts, list them in `keyboard_layout` and pick a
switch key with a `grp:` option, for example:

```text
keyboard_layout us,ara
keyboard_options grp:alt_shift_toggle
```

If the keymap cannot be loaded (for example a misspelled layout), beewm uses
the default layout at startup, or keeps the current keymap on reload, and shows
a notification.

## Pointer and touchpad

| Directive | Default | Notes |
|---|---|---|
| `focus_follows_mouse` | `true` | Focus the window under the pointer. |
| `tap_to_click` | `true` | Touchpads only. |
| `natural_scroll` | `false` | Touchpads only. |

The cursor theme and size come from the `XCURSOR_THEME` and `XCURSOR_SIZE`
environment variables.

## Display, power and lock

| Directive | Default | Notes |
|---|---|---|
| `refresh_rate` | monitor's preferred | Hz, 1 to 1000. Used for outputs without their own `mode`. |
| `screen_timeout` | `600` | Seconds idle before the screen blanks. `0` never blanks. |
| `lock_command` | `beelock` | Locker to run; must support `ext-session-lock-v1`. Takes the rest of the line. |
| `lock_on_suspend` | `true` | Lock before the system suspends. |
| `lock_on_resume` | `true` | Keep the session locked after resume until the locker unlocks. |

## Tray and overview

| Directive | Default | Notes |
|---|---|---|
| `tray_enabled` | `true` | Publish the settings tray icon (StatusNotifierItem). Also written `tray enable` / `tray disable`. |
| `overview_enabled` | `true` | Hold Super alone to show every window as a grid. See the README. |

## Animations

| Directive | Default | Notes |
|---|---|---|
| `enable_animations` | `true` | Master switch. |
| `window_open_animation` | `true` | New tiled windows grow from their top-left corner. |
| `window_close_animation` | `true` | Closing tiled windows shrink toward their top-left corner and fade out. |
| `layout_animation` | `true` | Animate tiles when the layout changes. |
| `workspace_animation` | `true` | Slide workspaces sideways when switching between them. |
| `disable_animations_for_fullscreen` | `true` | No animations while a window owns the whole screen (games). |
| `open_animation_duration_ms` | `180` | Up to 10000. |
| `close_animation_duration_ms` | `150` | Up to 10000. |
| `layout_animation_duration_ms` | `200` | Up to 10000. |
| `workspace_animation_duration_ms` | `250` | Up to 10000. |
| `animation_duration_ms` | (none) | Sets every duration at once. |
| `open_animation_easing` | `ease_out` | `linear`, `ease_in`, `ease_out` or `ease_in_out`. |
| `close_animation_easing` | `ease_in_out` | Same curves. |
| `layout_animation_easing` | `ease_in_out` | Same curves. |
| `workspace_animation_easing` | `ease_out` | Same curves. |
| `animation_easing` | (none) | Sets every curve at once. A later per-kind line overrides it. |

## Outputs (multi-monitor)

```text
output <connector> [position X Y] [mode WxH[@Hz]] [disable | enable]
```

Outputs are matched by connector name (`eDP-1`, `DP-3`, `HDMI-A-1`).

```text
output eDP-1 position 0 0
output DP-3 position 2560 0 mode 2560x1440@165
output HDMI-A-1 disable
```

- `position` (alias `pos`) places the output's top-left corner. Outputs without
  one are packed left to right.
- `mode` (alias `resolution`) forces a mode. If the monitor does not offer it,
  its preferred mode is used.
- `disable` (alias `off`) turns the monitor off; `enable` (alias `on`) turns it
  back on. beewm never disables every connected monitor.
- If a connector appears on several lines, the last line wins.

Driving more than one monitor is still experimental and requires starting
beewm with `BEEWM_MULTI_OUTPUT=1`. Without it, only the first connected
monitor is used.

## Autostart

```text
exec waybar
exec_once nm-applet
autostart swaybg -i ~/wall.png
```

The three spellings are identical: each command runs once, after the first
output is up. Commands with shell syntax (`$VAR`, pipes, quotes, `~`) are run
through `sh -c`.

## Key bindings

```text
bindsym <modifiers+key> <action>
```

`bind` is an alias for `bindsym`.

**Your first `bindsym` replaces all default bindings**, so copy any defaults you
want to keep (the starter config lists them all).

- **Modifiers:** `super` / `mod4` / `logo`, `shift`, `ctrl` / `control`,
  `alt` / `mod1`. Case does not matter.
- **Keys** are XKB keysym names: `a`, `Return`, `space`, `F1`, `Left`,
  `XF86AudioRaiseVolume`. Case does not matter.
- **Bindings follow the physical key position on a US keyboard**, not the
  character your layout produces. `$mod+a` fires on the key labelled A on a US
  keyboard, which is Q on AZERTY. This keeps bindings working when you switch
  layouts. Keys a US keyboard does not have (media keys) match by keysym.
- Unknown modifiers or key names, and binding the same combination twice, are
  errors.

### Actions

| Action | Effect |
|---|---|
| `exec <command>` (alias `spawn`) | Run a command. |
| `kill` (alias `close_window`) | Close the focused window. |
| `fullscreen` (alias `toggle_fullscreen`) | Toggle fullscreen. |
| `float` (alias `toggle_float`) | Toggle floating. |
| `sticky` (alias `toggle_sticky`) | Show the window on every workspace. |
| `focus_left` / `focus_right` / `focus_up` / `focus_down` | Focus the nearest window in that direction. |
| `focus_next` / `focus_prev` | Cycle focus in layout order. |
| `focus_output_left` / `_right` / `_up` / `_down` | Focus the nearest monitor in that direction. |
| `move_to_output_left` / `_right` / `_up` / `_down` | Move the focused window to that monitor. |
| `workspace N` (alias `switch_workspace`) | Show workspace N (numbered from 1). |
| `move_to_workspace N` | Move the focused window to workspace N. |
| `exit` (alias `quit`) | Quit beewm. |

A binding whose action is a single unknown word (`fulscreen`) is an error. An
action of several words that is not a built-in runs as a command for
compatibility, but prefer writing `exec`.

### Default bindings

Used only when the config has no `bindsym` lines at all.

| Keys | Action |
|---|---|
| `Super+Return` | `exec kitty` |
| `Super+q` | `exec wofi --show drun` |
| `Super+Arrows` | `focus_left` / `focus_right` / `focus_up` / `focus_down` |
| `Super+Shift+q` | `kill` |
| `Super+Shift+e` | `exit` |
| `Super+f` | `fullscreen` |
| `Super+v` | `float` |
| `Super+s` | `sticky` |
| `Super+1` ... `Super+0` | `workspace 1` ... `workspace 10` |
| `Super+Shift+1` ... `Super+Shift+0` | `move_to_workspace 1` ... `move_to_workspace 10` |

## Tray settings file (state.conf)

Settings changed from the tray menu are saved to
`~/.config/beewm/state.conf`, never to your config. It holds only `gap`,
`screen_timeout` and `output <name> mode WxH@Hz` lines, and it is applied on
top of the config. Editing one of those keys in your config later replaces the
tray's value. Delete the file to drop all tray choices; this takes effect at
the next save of the config or the next start.

## Environment variables

Read once at startup. Any non-empty value enables them.

| Variable | Effect |
|---|---|
| `BEEWM_MULTI_OUTPUT` | Drive every connected monitor (experimental). |
| `BEEWM_TRAY` | Force the settings tray on regardless of the config. |
| `BEEWM_DISABLE_ANIMATIONS` | Turn off all animations regardless of the config. |
| `BEEWM_FRAME_CALLBACK_AT_VBLANK` | Old frame-callback timing; see [diagnosing-low-fps.md](diagnosing-low-fps.md). |
| `BEEWM_WEDGE_TRACE` | Log (and sync to disk) around each GPU call, to find hard GPU hangs. |
| `BEEWM_NO_DMABUF` | Disable dmabuf buffer sharing (clients fall back to shared memory). |
| `BEEWM_NO_EXPLICIT_SYNC` | Disable explicit GPU sync. |
| `BEEWM_NO_SESSION_ENV_EXPORT` | Do not export the session environment to D-Bus/systemd (breaks screen sharing). |
| `BEEWM_NO_FOCUS_IPC` | Do not publish the focused window name to bars. |
| `BEEWM_NO_WORKSPACE_PUBLISH` | Do not publish workspace state to bars. |
| `BEEWM_NO_EVENT_BROADCASTER` | Do not start the event broadcaster thread. |
| `BEEWM_NO_KEYBOARD_LEDS` | Never set Caps/Num/Scroll Lock LEDs. |

The `BEEWM_NO_*` variables exist to isolate bugs and are not meant for everyday
use.
