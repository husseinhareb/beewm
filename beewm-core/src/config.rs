use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// A keybinding definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keybind {
    pub modifiers: Vec<String>,
    pub key: String,
    pub action: Action,
}

/// Actions that can be bound to keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    FocusNext,
    FocusPrev,
    CloseWindow,
    ToggleFullscreen,
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
    pub keybinds: Vec<Keybind>,
}

impl Default for Config {
    fn default() -> Self {
        let num_workspaces = 9;
        Self {
            layout: LayoutKind::Dwindle,
            split_ratio: 0.55,
            border_width: 2,
            border_color_focused: 0x5588FF,
            border_color_unfocused: 0x333333,
            gap: 4,
            num_workspaces,
            focus_follows_mouse: true,
            tap_to_click: true,
            natural_scroll: true,
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
            Self::Parse { line, message } => write!(f, "config parse error on line {}: {}", line, message),
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
                action: Action::Spawn("wofi --show run".into()),
            },
            Keybind {
                modifiers: vec!["mod4".into()],
                key: "j".into(),
                action: Action::FocusNext,
            },
            Keybind {
                modifiers: vec!["mod4".into()],
                key: "k".into(),
                action: Action::FocusPrev,
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
        ];

        for workspace in 1..=num_workspaces.min(9) {
            binds.push(Keybind {
                modifiers: vec!["mod4".into()],
                key: workspace.to_string(),
                action: Action::SwitchWorkspace(workspace - 1),
            });
        }

        for workspace in 1..=num_workspaces.min(9) {
            binds.push(Keybind {
                modifiers: vec!["mod4".into(), "shift".into()],
                key: workspace.to_string(),
                action: Action::MoveToWorkspace(workspace - 1),
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

    fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
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
        let defaults = Config::default();
        let mut config = Config {
            keybinds: Vec::new(),
            ..defaults
        };
        let mut variables = HashMap::<String, String>::new();
        let mut custom_keybinds = false;

        for (index, raw_line) in contents.lines().enumerate() {
            let line_no = index + 1;
            let trimmed = raw_line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("set ") {
                parse_variable(rest, line_no, &mut variables)?;
                continue;
            }

            let line = substitute_variables(trimmed, &variables);
            let mut parts = line.split_whitespace();
            let directive = parts.next().unwrap();

            match directive {
                "layout" => {
                    let value = expect_single_argument(parts, line_no, "layout")?;
                    config.layout = LayoutKind::parse(value).ok_or_else(|| ConfigError::Parse {
                        line: line_no,
                        message: format!("unknown layout '{}'", value),
                    })?;
                }
                "split_ratio" | "master_ratio" => {
                    let value = expect_single_argument(parts, line_no, directive)?;
                    config.split_ratio = parse_number(value, line_no, directive)?;
                }
                "border_width" => {
                    let value = expect_single_argument(parts, line_no, directive)?;
                    config.border_width = parse_number(value, line_no, directive)?;
                }
                "border_color_focused" => {
                    let value = expect_single_argument(parts, line_no, directive)?;
                    config.border_color_focused = parse_color(value, line_no, directive)?;
                }
                "border_color_unfocused" => {
                    let value = expect_single_argument(parts, line_no, directive)?;
                    config.border_color_unfocused = parse_color(value, line_no, directive)?;
                }
                "gap" => {
                    let value = expect_single_argument(parts, line_no, directive)?;
                    config.gap = parse_number(value, line_no, directive)?;
                }
                "workspaces" | "num_workspaces" => {
                    let value = expect_single_argument(parts, line_no, directive)?;
                    config.num_workspaces = parse_number(value, line_no, directive)?;
                }
                "focus_follows_mouse" => {
                    let value = expect_single_argument(parts, line_no, directive)?;
                    config.focus_follows_mouse = parse_bool(value, line_no, directive)?;
                }
                "tap_to_click" => {
                    let value = expect_single_argument(parts, line_no, directive)?;
                    config.tap_to_click = parse_bool(value, line_no, directive)?;
                }
                "natural_scroll" => {
                    let value = expect_single_argument(parts, line_no, directive)?;
                    config.natural_scroll = parse_bool(value, line_no, directive)?;
                }
                "bind" | "bindsym" => {
                    if !custom_keybinds {
                        config.keybinds.clear();
                        custom_keybinds = true;
                    }
                    let rest = line[directive.len()..].trim();
                    config.keybinds.push(parse_keybind(rest, line_no)?);
                }
                _ => {
                    return Err(ConfigError::Parse {
                        line: line_no,
                        message: format!("unknown directive '{}'", directive),
                    });
                }
            }
        }

        if !custom_keybinds {
            config.keybinds = Self::default_keybinds_for(config.num_workspaces);
        }

        config.validate()
    }

    pub fn default_text() -> String {
        let default = Self::default();
        let mut text = String::new();
        text.push_str("# beewm configuration\n");
        text.push_str("# i3-style line-based config.\n");
        text.push_str("# Lines beginning with # are comments.\n\n");
        text.push_str("set $mod mod4\n");
        text.push_str("set $terminal kitty\n");
        text.push_str("set $launcher wofi --show run\n\n");
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
        text.push_str(&format!("natural_scroll {}\n\n", default.natural_scroll));
        text.push_str("bindsym $mod+Return exec $terminal\n");
        text.push_str("bindsym $mod+q exec $launcher\n");
        text.push_str("bindsym $mod+j focus_next\n");
        text.push_str("bindsym $mod+k focus_prev\n");
        text.push_str("bindsym $mod+Shift+q kill\n");
        text.push_str("bindsym $mod+Shift+e exit\n");
        text.push_str("bindsym $mod+f fullscreen\n");
        for workspace in 1..=default.num_workspaces.min(9) {
            text.push_str(&format!("bindsym $mod+{} workspace {}\n", workspace, workspace));
        }
        for workspace in 1..=default.num_workspaces.min(9) {
            text.push_str(&format!(
                "bindsym $mod+Shift+{} move_to_workspace {}\n",
                workspace, workspace
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

fn parse_variable(
    rest: &str,
    line_no: usize,
    variables: &mut HashMap<String, String>,
) -> Result<(), ConfigError> {
    let split_at = rest.find(char::is_whitespace).ok_or_else(|| ConfigError::Parse {
        line: line_no,
        message: "set requires a variable name and a value".into(),
    })?;
    let name = rest[..split_at].trim();
    let value = rest[split_at..].trim();

    if !name.starts_with('$') {
        return Err(ConfigError::Parse {
            line: line_no,
            message: "variable names must start with '$'".into(),
        });
    }
    if value.is_empty() {
        return Err(ConfigError::Parse {
            line: line_no,
            message: "set requires a non-empty value".into(),
        });
    }

    variables.insert(name.to_string(), value.to_string());
    Ok(())
}

fn substitute_variables(line: &str, variables: &HashMap<String, String>) -> String {
    let mut output = line.to_string();
    let mut ordered = variables.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|(name, _)| Reverse(name.len()));

    for (name, value) in ordered {
        output = output.replace(name.as_str(), value);
    }

    output
}

fn expect_single_argument<'a>(
    mut parts: impl Iterator<Item = &'a str>,
    line_no: usize,
    directive: &str,
) -> Result<&'a str, ConfigError> {
    let value = parts.next().ok_or_else(|| ConfigError::Parse {
        line: line_no,
        message: format!("{} requires exactly one value", directive),
    })?;

    if parts.next().is_some() {
        return Err(ConfigError::Parse {
            line: line_no,
            message: format!("{} requires exactly one value", directive),
        });
    }

    Ok(value)
}

fn parse_number<T>(value: &str, line_no: usize, directive: &str) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
    T::Err: fmt::Display,
{
    value.parse::<T>().map_err(|error| ConfigError::Parse {
        line: line_no,
        message: format!("invalid {} '{}': {}", directive, value, error),
    })
}

fn parse_bool(value: &str, line_no: usize, directive: &str) -> Result<bool, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(ConfigError::Parse {
            line: line_no,
            message: format!("invalid {} '{}': expected true/false", directive, value),
        }),
    }
}

fn parse_color(value: &str, line_no: usize, directive: &str) -> Result<u32, ConfigError> {
    let value = value
        .strip_prefix('#')
        .or_else(|| value.strip_prefix("0x"))
        .unwrap_or(value);

    if value.len() != 6 {
        return Err(ConfigError::Parse {
            line: line_no,
            message: format!("invalid {} '{}': expected 6 hex digits", directive, value),
        });
    }

    u32::from_str_radix(value, 16).map_err(|error| ConfigError::Parse {
        line: line_no,
        message: format!("invalid {} '{}': {}", directive, value, error),
    })
}

fn parse_keybind(rest: &str, line_no: usize) -> Result<Keybind, ConfigError> {
    let split_at = rest.find(char::is_whitespace).ok_or_else(|| ConfigError::Parse {
        line: line_no,
        message: "binding requires a key combination and an action".into(),
    })?;
    let combo = rest[..split_at].trim();
    let action_text = rest[split_at..].trim();
    if action_text.is_empty() {
        return Err(ConfigError::Parse {
            line: line_no,
            message: "binding requires an action".into(),
        });
    }

    let mut combo_parts = combo
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if combo_parts.is_empty() {
        return Err(ConfigError::Parse {
            line: line_no,
            message: "binding must contain a key".into(),
        });
    }

    let key = combo_parts.pop().unwrap().to_string();
    let modifiers = combo_parts.into_iter().map(ToString::to_string).collect();
    let action = parse_action(action_text, line_no)?;

    Ok(Keybind {
        modifiers,
        key,
        action,
    })
}

fn parse_action(action_text: &str, line_no: usize) -> Result<Action, ConfigError> {
    let mut parts = action_text.split_whitespace();
    let name = parts.next().ok_or_else(|| ConfigError::Parse {
        line: line_no,
        message: "missing binding action".into(),
    })?;

    match name {
        "spawn" | "exec" => {
            let command = action_text[name.len()..].trim();
            if command.is_empty() {
                return Err(ConfigError::Parse {
                    line: line_no,
                    message: format!("{} requires a command", name),
                });
            }
            Ok(Action::Spawn(command.to_string()))
        }
        "focus_next" => Ok(Action::FocusNext),
        "focus_prev" => Ok(Action::FocusPrev),
        "close_window" | "kill" => Ok(Action::CloseWindow),
        "fullscreen" | "toggle_fullscreen" => Ok(Action::ToggleFullscreen),
        "quit" | "exit" => Ok(Action::Quit),
        "workspace" | "switch_workspace" => {
            let workspace = parse_workspace_argument(parts.next(), line_no, name)?;
            if parts.next().is_some() {
                return Err(ConfigError::Parse {
                    line: line_no,
                    message: format!("{} takes exactly one workspace number", name),
                });
            }
            Ok(Action::SwitchWorkspace(workspace))
        }
        "move_to_workspace" => {
            let workspace = parse_workspace_argument(parts.next(), line_no, name)?;
            if parts.next().is_some() {
                return Err(ConfigError::Parse {
                    line: line_no,
                    message: format!("{} takes exactly one workspace number", name),
                });
            }
            Ok(Action::MoveToWorkspace(workspace))
        }
        _ => Err(ConfigError::Parse {
            line: line_no,
            message: format!("unknown binding action '{}'", name),
        }),
    }
}

fn parse_workspace_argument(
    value: Option<&str>,
    line_no: usize,
    directive: &str,
) -> Result<usize, ConfigError> {
    let value = value.ok_or_else(|| ConfigError::Parse {
        line: line_no,
        message: format!("{} requires a workspace number", directive),
    })?;
    let workspace: usize = parse_number(value, line_no, directive)?;
    if workspace == 0 {
        return Err(ConfigError::Parse {
            line: line_no,
            message: format!("{} workspace numbers are 1-based", directive),
        });
    }
    Ok(workspace - 1)
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
mod tests {
    use super::{Action, Config, ConfigError, LayoutKind};

    #[test]
    fn parses_i3_style_config() {
        let config = Config::parse(
            r#"
            set $mod mod4
            set $term kitty --single-instance

            layout dwindle
            split_ratio 0.60
            border_width 3
            border_color_focused #112233
            border_color_unfocused 0x445566
            gap 8
            workspaces 5
            focus_follows_mouse no
            tap_to_click yes
            natural_scroll off

            bindsym $mod+Return exec $term
            bindsym $mod+1 workspace 1
            bindsym $mod+Shift+1 move_to_workspace 1
            bindsym $mod+q kill
            "#,
        )
        .unwrap();

        assert_eq!(config.layout, LayoutKind::Dwindle);
        assert_eq!(config.split_ratio, 0.60);
        assert_eq!(config.border_width, 3);
        assert_eq!(config.border_color_focused, 0x112233);
        assert_eq!(config.border_color_unfocused, 0x445566);
        assert_eq!(config.gap, 8);
        assert_eq!(config.num_workspaces, 5);
        assert!(!config.focus_follows_mouse);
        assert!(config.tap_to_click);
        assert!(!config.natural_scroll);
        assert_eq!(config.keybinds.len(), 4);
        assert_eq!(config.keybinds[0].action, Action::Spawn("kitty --single-instance".into()));
        assert_eq!(config.keybinds[1].action, Action::SwitchWorkspace(0));
        assert_eq!(config.keybinds[2].action, Action::MoveToWorkspace(0));
        assert_eq!(config.keybinds[3].action, Action::CloseWindow);
    }

    #[test]
    fn fills_default_keybinds_for_custom_workspace_count() {
        let config = Config::parse("workspaces 4\n").unwrap();
        assert!(config
            .keybinds
            .iter()
            .all(|bind| !matches!(bind.action, Action::SwitchWorkspace(index) if index >= 4)));
        assert!(config
            .keybinds
            .iter()
            .all(|bind| !matches!(bind.action, Action::MoveToWorkspace(index) if index >= 4)));
    }

    #[test]
    fn rejects_zero_workspaces() {
        let err = Config::parse("workspaces 0\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn rejects_invalid_split_ratio() {
        let err = Config::parse("split_ratio 2.0\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn rejects_workspace_bindings_out_of_bounds() {
        let err = Config::parse(
            r#"
            workspaces 2
            bindsym mod4+3 workspace 3
            "#,
        )
        .unwrap_err();

        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn writes_default_config_file_when_missing() {
        let mut root = std::env::temp_dir();
        root.push(format!("beewm-config-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let path = root.join("config");
        let config = Config::load_from_path(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();

        assert_eq!(config.layout, LayoutKind::Dwindle);
        assert!(written.contains("layout dwindle"));
        assert!(written.contains("bindsym $mod+Return exec $terminal"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
