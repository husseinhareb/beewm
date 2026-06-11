# beewm Tray Icon & Settings Menu

Status: implemented as a freedesktop StatusNotifierItem.

beewm publishes one settings tray item on the session bus. A StatusNotifier host
such as beebar owns placement and rendering of the tray slot; beewm owns the icon,
the dbusmenu tree, and the actions behind each menu row.

## Architecture

- `src/compositor/tray/mod.rs` serves two D-Bus objects on a dedicated thread:
  `/StatusNotifierItem` for `org.kde.StatusNotifierItem` and `/MenuBar` for
  `com.canonical.dbusmenu`.
- The compositor keeps the current menu tree in `SharedMenu`. The D-Bus thread
  reads it when the host calls `GetLayout`.
- Menu clicks are sent back to the compositor over a calloop channel as
  `MenuAction`; actions are applied on the main loop.
- The tray thread has a shutdown handle. Config reload can start or stop the
  StatusNotifierItem without restarting beewm.

## Config

```
tray enable          # publish the settings tray item
tray disable         # hide it
screen_timeout 600   # seconds; 0 = never blank
```

`tray_enabled true|false` is also accepted. Placement is intentionally not a
beewm setting; the StatusNotifier host decides where tray items appear.

`BEEWM_TRAY=1` force-enables the tray even when config disables it.

## Menu

The menu currently exposes:

- Resolution
- Refresh rate
- Gaps
- Screen timeout
- Sign out

Current values are marked with a check. Resolution and refresh rows are disabled
when the backend cannot provide live mode data, for example in nested winit.

## Persistence

Tray-written runtime settings are stored in `state.conf`, separate from the
hand-edited config. The overlay currently persists:

- `gap`
- `screen_timeout`
- tray-applied output modes as `output <name> mode WxH@Hz`

Live DRM mode changes are gated by `BEEWM_LIVE_MODESET=1`. When the gate is off,
resolution and refresh clicks are logged and ignored, and no output mode is
persisted. Nested winit does not support live output mode changes.
