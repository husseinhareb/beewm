mod parser;

use std::fmt;
use std::path::{Path, PathBuf};

const DEFAULT_WORKSPACE_KEYS: [&str; 10] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"];
const DEFAULT_KEYBOARD_LAYOUT: &str = "us";

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
    pub refresh_rate: Option<u32>,
    /// Publish the settings tray icon as a StatusNotifierItem on the session bus.
    /// On by default; disable with
    /// `tray disable` in the config. (`BEEWM_TRAY=1` can also force it on.)
    pub tray_enabled: bool,
    /// Seconds of inactivity before the screen blanks (DPMS off). `0` disables
    /// blanking entirely. Also settable live from the tray's Screen-timeout menu.
    pub screen_timeout: u32,
    /// Per-output configuration from `output <name> …` directives.
    pub outputs: Vec<OutputConfig>,
    pub autostart_commands: Vec<String>,
    pub keybinds: Vec<Keybind>,

    // Animation settings. See `compositor::animation`.
    /// Master switch for all compositor-driven window animations.
    pub enable_animations: bool,
    /// Animate newly mapped tiled windows (expand from top-left).
    pub window_open_animation: bool,
    /// Reserved closing animation toggle (see animation module limitations).
    pub window_close_animation: bool,
    /// Animate tiled windows when the layout reassigns their geometry.
    pub layout_animation: bool,
    /// Skip animations while a window owns the whole screen (fullscreen games).
    pub disable_animations_for_fullscreen: bool,
    pub open_animation_duration_ms: u64,
    pub close_animation_duration_ms: u64,
    pub layout_animation_duration_ms: u64,
    /// Easing curve name: linear | ease_in | ease_out | ease_in_out.
    pub animation_easing: String,
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
            refresh_rate: None,
            tray_enabled: true,
            screen_timeout: 600,
            outputs: Vec::new(),
            autostart_commands: Vec::new(),
            keybinds: Self::default_keybinds_for(num_workspaces),
            enable_animations: true,
            window_open_animation: true,
            window_close_animation: true,
            layout_animation: true,
            disable_animations_for_fullscreen: true,
            open_animation_duration_ms: 180,
            close_animation_duration_ms: 150,
            layout_animation_duration_ms: 200,
            animation_easing: "ease_out".to_string(),
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
        let path = Self::config_path();
        let mut config = Self::load_from_path(&path)?;
        config.apply_state_overrides();
        Ok(config)
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

    /// Overlay the small set of tray-settable keys from `state.conf`, if present.
    fn apply_state_overrides(&mut self) {
        let Ok(contents) = std::fs::read_to_string(Self::state_path()) else {
            return;
        };
        let overrides = parse_state_overrides(&contents);
        if let Some(gap) = overrides.gap {
            self.gap = gap;
        }
        if let Some(secs) = overrides.screen_timeout {
            self.screen_timeout = secs;
        }
        for (name, mode) in overrides.output_modes {
            self.set_output_mode_override(name, mode);
        }
    }

    pub fn runtime_output_modes() -> Vec<(String, OutputModeSpec)> {
        let Ok(contents) = std::fs::read_to_string(Self::state_path()) else {
            return Vec::new();
        };
        parse_state_overrides(&contents).output_modes
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
    pub fn write_state_overrides(
        gap: u32,
        screen_timeout: u32,
        output_modes: &[(String, OutputModeSpec)],
    ) {
        let path = Self::state_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut contents = format!(
            "# beewm runtime settings — written by the settings tray.\n\
             # Overrides the matching keys in your main config; delete to revert.\n\
             gap {gap}\n\
             screen_timeout {screen_timeout}\n",
        );
        let mut output_modes = output_modes.to_vec();
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
        text.push_str("# refresh_rate 165   # set display refresh rate in Hz (default: use monitor preferred)\n");
        text.push('\n');
        text.push_str("# Settings tray: publishes a StatusNotifierItem for your tray host.\n");
        text.push_str("# Shown by default; uncomment to hide it.\n");
        text.push_str("# tray_enabled false\n");
        text.push_str("# Seconds of inactivity before the screen blanks (DPMS off); 0 = never.\n");
        text.push_str(&format!("screen_timeout {}\n", default.screen_timeout));
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
        text.push_str("# animation_easing: linear | ease_in | ease_out | ease_in_out\n");
        text.push_str(&format!(
            "animation_easing {}\n\n",
            default.animation_easing
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

    fn validate(self) -> Result<Self, ConfigError> {
        if self.num_workspaces == 0 {
            return Err(ConfigError::Parse {
                line: 0,
                message: "workspaces must be at least 1".into(),
            });
        }

        if !self.split_ratio.is_finite() || !(0.0..=1.0).contains(&self.split_ratio) {
            return Err(ConfigError::Parse {
                line: 0,
                message: "split_ratio must be a finite value between 0.0 and 1.0".into(),
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
#[derive(Debug, Default, PartialEq, Eq)]
struct StateOverrides {
    gap: Option<u32>,
    screen_timeout: Option<u32>,
    output_modes: Vec<(String, OutputModeSpec)>,
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
