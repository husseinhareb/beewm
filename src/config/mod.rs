mod parser;

use std::fmt;
use std::path::{Path, PathBuf};

const DEFAULT_WORKSPACE_KEYS: [&str; 10] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"];
const DEFAULT_KEYBOARD_LAYOUT: &str = "us";
/// Upper bound for `gap` / `border_width`, far past any sane value but small
/// enough that the compositor's `i32` geometry math never wraps.
const MAX_PIXELS: u32 = 1000;
const MAX_REFRESH_HZ: u32 = 1000;
const MAX_REPEAT_RATE: u32 = 1000;
/// Upper bound for animation durations and `repeat_delay`, in milliseconds.
const MAX_ANIMATION_OR_REPEAT_MS: u32 = 10_000;

/// A keybinding definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keybind {
    pub modifiers: Vec<String>,
    pub key: String,
    pub action: Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDirection {
    Left,
    Right,
    Up,
    Down,
}

/// Actions that can be bound to keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    FocusNext,
    FocusPrev,
    FocusDirection(FocusDirection),
    /// Move keyboard focus to the nearest output in a direction.
    FocusOutput(FocusDirection),
    /// Move the focused window to the nearest output in a direction.
    MoveWindowToOutput(FocusDirection),
    CloseWindow,
    ToggleFullscreen,
    ToggleFloat,
    ToggleSticky,
    SwitchWorkspace(usize),
    MoveToWorkspace(usize),
    Spawn(String),
    Quit,
}

/// A `WxH` or `WxH@Hz` mode spec parsed from an `output` directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputModeSpec {
    pub width: i32,
    pub height: i32,
    pub refresh: Option<u32>,
}

/// Per-connector configuration from `output <name> …` directives, matched
/// against the human-readable connector name (e.g. `DP-3`, `eDP-1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputConfig {
    pub name: String,
    /// `output <name> disable` turns the display off (no surface is created).
    pub enabled: bool,
    /// Explicit top-left in the global layout; otherwise outputs auto-pack
    /// left-to-right.
    pub position: Option<(i32, i32)>,
    /// Force a specific mode; otherwise the connector's preferred mode is used.
    pub mode: Option<OutputModeSpec>,
}

/// The active tiling layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutKind {
    Dwindle,
    MasterStack,
}

impl LayoutKind {
    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "dwindle" => Some(Self::Dwindle),
            "master" | "master_stack" | "master-stack" => Some(Self::MasterStack),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Dwindle => "dwindle",
            Self::MasterStack => "master_stack",
        }
    }
}

/// Top-level configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub layout: LayoutKind,
    pub split_ratio: f64,
    pub border_width: u32,
    pub border_color_focused: u32,
    pub border_color_unfocused: u32,
    pub gap: u32,
    pub num_workspaces: usize,
    pub focus_follows_mouse: bool,
    pub tap_to_click: bool,
    pub natural_scroll: bool,
    pub keyboard_layout: String,
    /// XKB variants, one per layout (comma separated); empty for the default.
    pub keyboard_variant: String,
    /// XKB options such as `caps:escape,grp:alt_shift_toggle`; empty for none.
    pub keyboard_options: String,
    /// XKB keyboard model; empty for the XKB default.
    pub keyboard_model: String,
    /// Milliseconds a key is held before it starts repeating.
    pub repeat_delay: u32,
    /// Key repeats per second while held; `0` disables repeat.
    pub repeat_rate: u32,
    pub refresh_rate: Option<u32>,
    /// Publish the settings tray icon as a StatusNotifierItem on the session bus.
    /// On by default; disable with
    /// `tray disable` in the config. (`BEEWM_TRAY=1` can also force it on.)
    pub tray_enabled: bool,
    /// Seconds of inactivity before the screen blanks (DPMS off). `0` disables
    /// blanking entirely. Also settable live from the tray's Screen-timeout menu.
    pub screen_timeout: u32,
    /// Command used to start the ext-session-lock client. The compositor enters
    /// its secure locked state before spawning this process, so a failed locker
    /// leaves a black locked screen rather than exposing the session.
    pub lock_command: String,
    /// Lock before system suspend is allowed to continue.
    pub lock_on_suspend: bool,
    /// Keep the resumed session locked until the lock client authenticates.
    pub lock_on_resume: bool,
    /// Show the hold-Super window overview. See
    /// [`crate::compositor::overview`]; disable with `overview_enabled false`
    /// if you would rather Super never did anything on its own.
    pub overview_enabled: bool,
    /// Per-output configuration from `output <name> …` directives.
    pub outputs: Vec<OutputConfig>,
    pub autostart_commands: Vec<String>,
    pub keybinds: Vec<Keybind>,

    // Animation settings. See `compositor::animation`.
    /// Master switch for all compositor-driven window animations.
    pub enable_animations: bool,
    /// Animate newly mapped tiled windows (expand from top-left).
    pub window_open_animation: bool,
    /// Animate closing tiled windows (shrink toward top-left and fade out).
    pub window_close_animation: bool,
    /// Animate tiled windows when the layout reassigns their geometry.
    pub layout_animation: bool,
    /// Slide workspaces horizontally when switching between them.
    pub workspace_animation: bool,
    /// Skip animations while a window owns the whole screen (fullscreen games).
    pub disable_animations_for_fullscreen: bool,
    pub open_animation_duration_ms: u64,
    pub close_animation_duration_ms: u64,
    pub layout_animation_duration_ms: u64,
    pub workspace_animation_duration_ms: u64,
    /// Easing curve names (linear | ease_in | ease_out | ease_in_out), one per
    /// animation kind. `animation_easing` in the config file sets all of them.
    pub open_animation_easing: String,
    pub close_animation_easing: String,
    pub layout_animation_easing: String,
    pub workspace_animation_easing: String,
}

impl Default for Config {
    fn default() -> Self {
        let num_workspaces = 10;
        Self {
            layout: LayoutKind::Dwindle,
            split_ratio: 0.50,
            border_width: 2,
            border_color_focused: 0x5588FF,
            border_color_unfocused: 0x333333,
            gap: 4,
            num_workspaces,
            focus_follows_mouse: true,
            tap_to_click: true,
            natural_scroll: false,
            keyboard_layout: DEFAULT_KEYBOARD_LAYOUT.to_string(),
            keyboard_variant: String::new(),
            keyboard_options: String::new(),
            keyboard_model: String::new(),
            repeat_delay: 200,
            repeat_rate: 25,
            refresh_rate: None,
            tray_enabled: true,
            screen_timeout: 600,
            lock_command: "beelock".to_string(),
            lock_on_suspend: true,
            lock_on_resume: true,
            overview_enabled: true,
            outputs: Vec::new(),
            autostart_commands: Vec::new(),
            keybinds: Self::default_keybinds_for(num_workspaces),
            enable_animations: true,
            window_open_animation: true,
            window_close_animation: true,
            layout_animation: true,
            workspace_animation: true,
            disable_animations_for_fullscreen: true,
            open_animation_duration_ms: 180,
            close_animation_duration_ms: 150,
            layout_animation_duration_ms: 200,
            workspace_animation_duration_ms: 250,
            open_animation_easing: "ease_out".to_string(),
            close_animation_easing: "ease_in_out".to_string(),
            layout_animation_easing: "ease_in_out".to_string(),
            workspace_animation_easing: "ease_out".to_string(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse { line: usize, message: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "config I/O error: {}", error),
            Self::Parse { line, message } => {
                write!(f, "config parse error on line {}: {}", line, message)
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Parse { .. } => None,
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl Config {
    fn default_keybinds_for(num_workspaces: usize) -> Vec<Keybind> {
        let mut binds = vec![
            Keybind {
                modifiers: vec!["mod4".into()],
                key: "Return".into(),
                action: Action::Spawn("kitty".into()),
            },
            Keybind {
                modifiers: vec!["mod4".into()],
                key: "q".into(),
                action: Action::Spawn("wofi --show drun".into()),
            },
            Keybind {
                modifiers: vec!["mod4".into()],
                key: "Left".into(),
                action: Action::FocusDirection(FocusDirection::Left),
            },
            Keybind {
                modifiers: vec!["mod4".into()],
                key: "Right".into(),
                action: Action::FocusDirection(FocusDirection::Right),
            },
            Keybind {
                modifiers: vec!["mod4".into()],
                key: "Up".into(),
                action: Action::FocusDirection(FocusDirection::Up),
            },
            Keybind {
                modifiers: vec!["mod4".into()],
                key: "Down".into(),
                action: Action::FocusDirection(FocusDirection::Down),
            },
            Keybind {
                modifiers: vec!["mod4".into(), "shift".into()],
                key: "q".into(),
                action: Action::CloseWindow,
            },
            Keybind {
                modifiers: vec!["mod4".into(), "shift".into()],
                key: "e".into(),
                action: Action::Quit,
            },
            Keybind {
                modifiers: vec!["mod4".into()],
                key: "f".into(),
                action: Action::ToggleFullscreen,
            },
            Keybind {
                modifiers: vec!["mod4".into()],
                key: "v".into(),
                action: Action::ToggleFloat,
            },
            Keybind {
                modifiers: vec!["mod4".into()],
                key: "s".into(),
                action: Action::ToggleSticky,
            },
        ];

        for (index, key) in DEFAULT_WORKSPACE_KEYS
            .iter()
            .copied()
            .enumerate()
            .take(num_workspaces.min(DEFAULT_WORKSPACE_KEYS.len()))
        {
            binds.push(Keybind {
                modifiers: vec!["mod4".into()],
                key: key.into(),
                action: Action::SwitchWorkspace(index),
            });
        }

        for (index, key) in DEFAULT_WORKSPACE_KEYS
            .iter()
            .copied()
            .enumerate()
            .take(num_workspaces.min(DEFAULT_WORKSPACE_KEYS.len()))
        {
            binds.push(Keybind {
                modifiers: vec!["mod4".into(), "shift".into()],
                key: key.into(),
                action: Action::MoveToWorkspace(index),
            });
        }

        binds
    }

    /// Load config from the default path (`~/.config/beewm/config`).
    /// If it does not exist, write a starter config first. Runtime settings
    /// changed from the tray (`state.conf`) are layered on top.
    pub fn load() -> Result<Self, ConfigError> {
        Ok(Self::load_base()?.with_state_overrides())
    }

    /// Load only the hand-edited config file, without the `state.conf` overlay.
    pub fn load_base() -> Result<Self, ConfigError> {
        Self::load_from_path(&Self::config_path())
    }

    /// This config with the `state.conf` overlay applied on top.
    pub fn with_state_overrides(self) -> Self {
        self.with_overrides(&Self::read_state_overrides())
    }

    /// This config with `overrides` applied on top.
    pub fn with_overrides(mut self, overrides: &StateOverrides) -> Self {
        if let Some(gap) = overrides.gap {
            self.gap = gap;
        }
        if let Some(secs) = overrides.screen_timeout {
            self.screen_timeout = secs;
        }
        for (name, mode) in &overrides.output_modes {
            self.set_output_mode_override(name.clone(), *mode);
        }
        self
    }

    /// The `output <name> mode` spec configured for `name`, if any.
    pub fn output_mode_for(&self, name: &str) -> Option<OutputModeSpec> {
        self.outputs
            .iter()
            .find(|output| output.name == name)
            .and_then(|output| output.mode)
    }

    /// Drop the overrides in `overrides` that a config edit (`old_base` to
    /// `new_base`) has superseded. The hand-edited config is the source of
    /// truth: editing a key in it beats an older tray choice for that key.
    /// Returns whether anything was dropped.
    pub fn drop_superseded_overrides(
        old_base: &Config,
        new_base: &Config,
        overrides: &mut StateOverrides,
    ) -> bool {
        let mut changed = false;
        if old_base.gap != new_base.gap && overrides.gap.take().is_some() {
            changed = true;
        }
        if old_base.screen_timeout != new_base.screen_timeout
            && overrides.screen_timeout.take().is_some()
        {
            changed = true;
        }
        let superseded = Self::superseded_runtime_modes(
            old_base,
            new_base,
            overrides.output_modes.iter().map(|(name, _)| name.as_str()),
        );
        if !superseded.is_empty() {
            overrides
                .output_modes
                .retain(|(name, _)| !superseded.contains(name));
            changed = true;
        }
        changed
    }

    /// Which of the runtime (tray) mode overrides in `runtime_names` a config
    /// edit has superseded. The hand-edited config is the source of truth: when
    /// the mode it asks for changes between `old_base` and `new_base` (the
    /// output's own `mode`, or the global `refresh_rate` for an output without
    /// one), the stale tray choice must give way so the edit takes effect.
    pub fn superseded_runtime_modes<'a>(
        old_base: &Config,
        new_base: &Config,
        runtime_names: impl IntoIterator<Item = &'a str>,
    ) -> Vec<String> {
        runtime_names
            .into_iter()
            .filter(|name| {
                let old_spec = old_base.output_mode_for(name);
                let new_spec = new_base.output_mode_for(name);
                old_spec != new_spec
                    || (new_spec.is_none() && old_base.refresh_rate != new_base.refresh_rate)
            })
            .map(str::to_string)
            .collect()
    }

    /// Path of the machine-managed runtime-settings overlay written by the tray.
    /// Kept separate from the hand-edited config so menu changes never clobber
    /// the user's comments/variables; it is parsed *after* the main config and
    /// overrides the matching keys.
    pub fn state_path() -> PathBuf {
        let mut path = dirs_or_default();
        path.push("beewm");
        path.push("state.conf");
        path
    }

    /// The tray-written overrides currently in `state.conf` (empty if absent).
    pub fn read_state_overrides() -> StateOverrides {
        std::fs::read_to_string(Self::state_path())
            .map(|contents| parse_state_overrides(&contents))
            .unwrap_or_default()
    }

    pub fn set_output_mode_override(&mut self, name: String, mode: OutputModeSpec) {
        if let Some(output) = self.outputs.iter_mut().find(|output| output.name == name) {
            output.mode = Some(mode);
            return;
        }
        self.outputs.push(OutputConfig {
            name,
            enabled: true,
            position: None,
            mode: Some(mode),
        });
    }

    /// Persist the tray-settable runtime values to `state.conf` (best-effort).
    /// Called when the tray changes a setting so it survives a restart.
    pub fn write_state_overrides(overrides: &StateOverrides) {
        let path = Self::state_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut contents = String::from(
            "# beewm runtime settings, written by the settings tray.\n\
             # Overrides the matching keys in your main config until you edit\n\
             # that key there; delete this file to revert.\n",
        );
        if let Some(gap) = overrides.gap {
            contents.push_str(&format!("gap {gap}\n"));
        }
        if let Some(secs) = overrides.screen_timeout {
            contents.push_str(&format!("screen_timeout {secs}\n"));
        }
        let mut output_modes = overrides.output_modes.clone();
        output_modes.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        for (name, mode) in output_modes {
            let refresh = mode.refresh.map(|hz| format!("@{hz}")).unwrap_or_default();
            contents.push_str(&format!(
                "output {name} mode {}x{}{refresh}\n",
                mode.width, mode.height
            ));
        }
        if let Err(error) = std::fs::write(&path, contents) {
            tracing::warn!("Failed to write {}: {}", path.display(), error);
        }
    }

    /// Load config from an explicit path.
    /// If it does not exist, write a starter config first.
    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, Self::default_text())?;
            tracing::info!("Wrote default config to {}", path.display());
        }

        let contents = std::fs::read_to_string(path)?;
        Self::parse(&contents)
    }

    pub fn parse(contents: &str) -> Result<Self, ConfigError> {
        parser::parse_config(contents)
    }

    pub fn default_text() -> String {
        let default = Self::default();
        let mut text = String::new();
        text.push_str("# beewm configuration\n");
        text.push_str("# i3-style line-based config.\n");
        text.push_str("# Lines beginning with # are comments.\n\n");
        text.push_str("set $mod mod4\n");
        text.push_str("set $terminal kitty\n");
        text.push_str("set $launcher wofi --show drun\n\n");
        text.push_str(&format!("layout {}\n", default.layout.as_str()));
        text.push_str(&format!("split_ratio {:.2}\n\n", default.split_ratio));
        text.push_str(&format!("border_width {}\n", default.border_width));
        text.push_str(&format!(
            "border_color_focused #{:06x}\n",
            default.border_color_focused
        ));
        text.push_str(&format!(
            "border_color_unfocused #{:06x}\n",
            default.border_color_unfocused
        ));
        text.push_str(&format!("gap {}\n", default.gap));
        text.push_str(&format!("workspaces {}\n", default.num_workspaces));
        text.push_str(&format!(
            "focus_follows_mouse {}\n",
            default.focus_follows_mouse
        ));
        text.push_str(&format!("tap_to_click {}\n", default.tap_to_click));
        text.push_str(&format!("natural_scroll {}\n", default.natural_scroll));
        text.push_str(&format!("keyboard_layout {}\n", default.keyboard_layout));
        text.push_str("# keyboard_variant intl\n");
        text.push_str("# keyboard_options caps:escape,grp:alt_shift_toggle\n");
        text.push_str(&format!("repeat_delay {}\n", default.repeat_delay));
        text.push_str(&format!("repeat_rate {}\n", default.repeat_rate));
        text.push_str("# refresh_rate 165   # set display refresh rate in Hz (default: use monitor preferred)\n");
        text.push('\n');
        text.push_str("# Settings tray: publishes a StatusNotifierItem for your tray host.\n");
        text.push_str("# Shown by default; uncomment to hide it.\n");
        text.push_str("# tray_enabled false\n");
        text.push_str("# Seconds of inactivity before the screen blanks (DPMS off); 0 = never.\n");
        text.push_str(&format!("screen_timeout {}\n", default.screen_timeout));
        text.push_str("# Lock command used on suspend/resume; must support ext-session-lock-v1.\n");
        text.push_str(&format!("lock_command {}\n", default.lock_command));
        text.push_str(&format!("lock_on_suspend {}\n", default.lock_on_suspend));
        text.push_str(&format!("lock_on_resume {}\n", default.lock_on_resume));
        text.push('\n');
        text.push_str("# Hold Super (with nothing else pressed) to show a grid of every open\n");
        text.push_str(
            "# window; Tab/arrows or the mouse pick one, releasing Super switches to it.\n",
        );
        text.push_str(&format!("overview_enabled {}\n", default.overview_enabled));
        text.push('\n');
        text.push_str(
            "# Multi-monitor: arrange outputs by connector name (DP-3, eDP-1, HDMI-A-1).\n",
        );
        text.push_str("# Requires BEEWM_MULTI_OUTPUT=1 while multi-head is experimental.\n");
        text.push_str("# output eDP-1 position 0 0\n");
        text.push_str("# output DP-3 position 2560 0 mode 2560x1440@165\n");
        text.push_str("# output HDMI-A-1 disable\n");
        text.push('\n');
        text.push_str("# Window animations (compositor-driven; layout stays exact).\n");
        text.push_str(&format!(
            "enable_animations {}\n",
            default.enable_animations
        ));
        text.push_str(&format!(
            "window_open_animation {}\n",
            default.window_open_animation
        ));
        text.push_str(&format!(
            "window_close_animation {}\n",
            default.window_close_animation
        ));
        text.push_str(&format!("layout_animation {}\n", default.layout_animation));
        text.push_str(&format!(
            "workspace_animation {}\n",
            default.workspace_animation
        ));
        text.push_str(&format!(
            "disable_animations_for_fullscreen {}\n",
            default.disable_animations_for_fullscreen
        ));
        text.push_str(&format!(
            "open_animation_duration_ms {}\n",
            default.open_animation_duration_ms
        ));
        text.push_str(&format!(
            "close_animation_duration_ms {}\n",
            default.close_animation_duration_ms
        ));
        text.push_str(&format!(
            "layout_animation_duration_ms {}\n",
            default.layout_animation_duration_ms
        ));
        text.push_str(&format!(
            "workspace_animation_duration_ms {}\n",
            default.workspace_animation_duration_ms
        ));
        text.push_str("# Easing: linear | ease_in | ease_out | ease_in_out\n");
        text.push_str("# (animation_easing <curve> sets all of them at once)\n");
        text.push_str(&format!(
            "open_animation_easing {}\n",
            default.open_animation_easing
        ));
        text.push_str(&format!(
            "close_animation_easing {}\n",
            default.close_animation_easing
        ));
        text.push_str(&format!(
            "layout_animation_easing {}\n",
            default.layout_animation_easing
        ));
        text.push_str(&format!(
            "workspace_animation_easing {}\n\n",
            default.workspace_animation_easing
        ));
        text.push_str("# Start commands once when beewm launches.\n");
        text.push_str("# exec waybar\n");
        text.push_str("# exec nm-applet\n\n");
        text.push_str("bindsym $mod+Return exec $terminal\n");
        text.push_str("bindsym $mod+q exec $launcher\n");
        text.push_str("bindsym $mod+Left focus_left\n");
        text.push_str("bindsym $mod+Right focus_right\n");
        text.push_str("bindsym $mod+Up focus_up\n");
        text.push_str("bindsym $mod+Down focus_down\n");
        text.push_str("bindsym $mod+Shift+q kill\n");
        text.push_str("bindsym $mod+Shift+e exit\n");
        text.push_str("bindsym $mod+f fullscreen\n");
        text.push_str("bindsym $mod+v float\n");
        text.push_str("bindsym $mod+s sticky\n");
        for (index, key) in DEFAULT_WORKSPACE_KEYS
            .iter()
            .copied()
            .enumerate()
            .take(default.num_workspaces.min(DEFAULT_WORKSPACE_KEYS.len()))
        {
            text.push_str(&format!("bindsym $mod+{} workspace {}\n", key, index + 1));
        }
        for (index, key) in DEFAULT_WORKSPACE_KEYS
            .iter()
            .copied()
            .enumerate()
            .take(default.num_workspaces.min(DEFAULT_WORKSPACE_KEYS.len()))
        {
            text.push_str(&format!(
                "bindsym $mod+Shift+{} move_to_workspace {}\n",
                key,
                index + 1
            ));
        }
        text
    }

    pub fn config_path() -> PathBuf {
        let mut path = dirs_or_default();
        path.push("beewm");
        path.push("config");
        path
    }

    /// The XKB keymap names this config asks for.
    pub fn xkb_config(&self) -> smithay::input::keyboard::XkbConfig<'_> {
        smithay::input::keyboard::XkbConfig {
            rules: "",
            model: &self.keyboard_model,
            layout: &self.keyboard_layout,
            variant: &self.keyboard_variant,
            options: (!self.keyboard_options.is_empty()).then(|| self.keyboard_options.clone()),
        }
    }

    /// Whether any XKB keymap setting differs from `other`.
    pub fn keymap_differs(&self, other: &Config) -> bool {
        self.keyboard_layout != other.keyboard_layout
            || self.keyboard_variant != other.keyboard_variant
            || self.keyboard_options != other.keyboard_options
            || self.keyboard_model != other.keyboard_model
    }

    /// A warning when the config file (or the directory holding it) can be
    /// modified by other users. The config runs commands as you (`exec`,
    /// bindings), so that would let them run code in your session.
    pub fn insecure_permissions_warning(path: &Path) -> Option<String> {
        use std::os::unix::fs::PermissionsExt;
        let resolved = std::fs::canonicalize(path).ok()?;
        let mut writable = Vec::new();
        for candidate in [Some(resolved.as_path()), resolved.parent()]
            .into_iter()
            .flatten()
        {
            let Ok(metadata) = std::fs::metadata(candidate) else {
                continue;
            };
            if metadata.permissions().mode() & 0o022 != 0 {
                writable.push(candidate.display().to_string());
            }
        }
        if writable.is_empty() {
            return None;
        }
        Some(format!(
            "{} can be modified by other users, who could then run commands in your session; \
             fix with: chmod go-w {}",
            writable.join(" and "),
            writable.join(" "),
        ))
    }

    fn validate(self) -> Result<Self, ConfigError> {
        if self.num_workspaces == 0 {
            return Err(ConfigError::Parse {
                line: 0,
                message: "workspaces must be at least 1".into(),
            });
        }

        let out_of_range = |message: String| Err(ConfigError::Parse { line: 0, message });
        if self.gap > MAX_PIXELS {
            return out_of_range(format!("gap must be at most {MAX_PIXELS}"));
        }
        if self.border_width > MAX_PIXELS {
            return out_of_range(format!("border_width must be at most {MAX_PIXELS}"));
        }
        if let Some(hz) = self.refresh_rate
            && !(1..=MAX_REFRESH_HZ).contains(&hz)
        {
            return out_of_range(format!(
                "refresh_rate must be between 1 and {MAX_REFRESH_HZ}"
            ));
        }
        if self.repeat_delay > MAX_ANIMATION_OR_REPEAT_MS {
            return out_of_range(format!(
                "repeat_delay must be at most {MAX_ANIMATION_OR_REPEAT_MS}"
            ));
        }
        if self.repeat_rate > MAX_REPEAT_RATE {
            return out_of_range(format!("repeat_rate must be at most {MAX_REPEAT_RATE}"));
        }
        for (name, ms) in [
            (
                "open_animation_duration_ms",
                self.open_animation_duration_ms,
            ),
            (
                "close_animation_duration_ms",
                self.close_animation_duration_ms,
            ),
            (
                "layout_animation_duration_ms",
                self.layout_animation_duration_ms,
            ),
            (
                "workspace_animation_duration_ms",
                self.workspace_animation_duration_ms,
            ),
        ] {
            if ms > u64::from(MAX_ANIMATION_OR_REPEAT_MS) {
                return out_of_range(format!(
                    "{name} must be at most {MAX_ANIMATION_OR_REPEAT_MS}"
                ));
            }
        }

        // The extremes would give one side of every split zero width.
        if !self.split_ratio.is_finite() || !(0.05..=0.95).contains(&self.split_ratio) {
            return Err(ConfigError::Parse {
                line: 0,
                message: "split_ratio must be between 0.05 and 0.95".into(),
            });
        }

        for bind in &self.keybinds {
            match bind.action {
                Action::SwitchWorkspace(index) | Action::MoveToWorkspace(index)
                    if index >= self.num_workspaces =>
                {
                    return Err(ConfigError::Parse {
                        line: 0,
                        message: format!(
                            "workspace binding points at workspace {} but only {} workspaces exist",
                            index + 1,
                            self.num_workspaces
                        ),
                    });
                }
                _ => {}
            }
        }

        Ok(self)
    }
}

/// The subset of settings the tray can persist to `state.conf`.
/// Each key is optional: only values the tray actually set are written, so
/// everything else keeps following the hand-edited config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateOverrides {
    pub gap: Option<u32>,
    pub screen_timeout: Option<u32>,
    pub output_modes: Vec<(String, OutputModeSpec)>,
}

/// Parse the tiny `state.conf` overlay (a few `key value` lines). Unknown keys
/// and malformed values are ignored so a stale overlay never breaks startup.
fn parse_state_overrides(contents: &str) -> StateOverrides {
    let mut overrides = StateOverrides::default();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("gap"), Some(value)) => overrides.gap = value.parse().ok(),
            (Some("screen_timeout"), Some(value)) => overrides.screen_timeout = value.parse().ok(),
            (Some("output"), Some(name)) => {
                while let Some(key) = parts.next() {
                    if matches!(key, "mode" | "resolution") {
                        if let Some(mode) = parts.next().and_then(parse_state_mode_spec) {
                            overrides.output_modes.push((name.to_string(), mode));
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    overrides
}

fn parse_state_mode_spec(value: &str) -> Option<OutputModeSpec> {
    let (dims, refresh) = match value.split_once('@') {
        Some((dims, hz)) => (dims, Some(hz.parse().ok()?)),
        None => (value, None),
    };
    let (width, height) = dims.split_once(['x', 'X'])?;
    Some(OutputModeSpec {
        width: width.parse().ok()?,
        height: height.parse().ok()?,
        refresh,
    })
}

fn dirs_or_default() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
            home.push(".config");
            home
        })
}

#[cfg(test)]
mod state_override_tests {
    use super::parse_state_overrides;

    #[test]
    fn parses_known_keys_and_ignores_the_rest() {
        let overrides = parse_state_overrides(
            "# tray-written\n\
             gap 12\n\
             screen_timeout 300\n\
             output DP-1 mode 2560x1440@165\n\
             bogus 5\n",
        );
        assert_eq!(overrides.gap, Some(12));
        assert_eq!(overrides.screen_timeout, Some(300));
        assert_eq!(overrides.output_modes.len(), 1);
        assert_eq!(overrides.output_modes[0].0, "DP-1");
        assert_eq!(
            overrides.output_modes[0].1,
            super::OutputModeSpec {
                width: 2560,
                height: 1440,
                refresh: Some(165)
            }
        );
    }

    #[test]
    fn empty_or_malformed_yields_no_overrides() {
        let overrides =
            parse_state_overrides("\n# only comments\ngap notanumber\noutput DP-1 mode nope\n");
        assert_eq!(overrides.gap, None);
        assert_eq!(overrides.screen_timeout, None);
        assert!(overrides.output_modes.is_empty());
    }
}
