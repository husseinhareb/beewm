mod parser;

use std::fmt;
use std::path::{Path, PathBuf};

const DEFAULT_WORKSPACE_KEYS: [&str; 10] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"];

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
    CloseWindow,
    ToggleFullscreen,
    ToggleFloat,
    SwitchWorkspace(usize),
    MoveToWorkspace(usize),
    Spawn(String),
    Quit,
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
    pub autostart_commands: Vec<String>,
    pub keybinds: Vec<Keybind>,
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
            autostart_commands: Vec::new(),
            keybinds: Self::default_keybinds_for(num_workspaces),
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
    /// If it does not exist, write a starter config first.
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::config_path();
        Self::load_from_path(&path)
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

    fn config_path() -> PathBuf {
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
                Action::SwitchWorkspace(index) | Action::MoveToWorkspace(index) => {
                    if index >= self.num_workspaces {
                        return Err(ConfigError::Parse {
                            line: 0,
                            message: format!(
                                "workspace binding points at workspace {} but only {} workspaces exist",
                                index + 1,
                                self.num_workspaces
                            ),
                        });
                    }
                }
                _ => {}
            }
        }

        Ok(self)
    }
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
