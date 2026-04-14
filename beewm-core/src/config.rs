use serde::Deserialize;
use std::path::PathBuf;

/// A keybinding definition.
#[derive(Debug, Clone, Deserialize)]
pub struct Keybind {
    pub modifiers: Vec<String>,
    pub key: String,
    pub action: Action,
}

/// Actions that can be bound to keys.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    FocusNext,
    FocusPrev,
    CloseWindow,
    SwitchWorkspace(usize),
    MoveToWorkspace(usize),
    Spawn(String),
    Quit,
}

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub border_width: u32,
    pub border_color_focused: u32,
    pub border_color_unfocused: u32,
    pub gap: u32,
    pub num_workspaces: usize,
    pub focus_follows_mouse: bool,
    pub tap_to_click: bool,
    pub natural_scroll: bool,
    pub master_ratio: f64,
    #[serde(default)]
    pub keybinds: Vec<Keybind>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            border_width: 2,
            border_color_focused: 0x5588FF,
            border_color_unfocused: 0x333333,
            gap: 4,
            num_workspaces: 9,
            focus_follows_mouse: true,
            tap_to_click: true,
            natural_scroll: true,
            master_ratio: 0.55,
            keybinds: Self::default_keybinds(),
        }
    }
}

impl Config {
    fn default_keybinds() -> Vec<Keybind> {
        let mut binds = vec![
            Keybind {
                modifiers: vec!["super".into()],
                key: "Return".into(),
                action: Action::Spawn("kitty".into()),
            },
            Keybind {
                modifiers: vec!["super".into()],
                key: "d".into(),
                action: Action::Spawn("wofi --show run".into()),
            },
            Keybind {
                modifiers: vec!["super".into()],
                key: "j".into(),
                action: Action::FocusNext,
            },
            Keybind {
                modifiers: vec!["super".into()],
                key: "k".into(),
                action: Action::FocusPrev,
            },
            Keybind {
                modifiers: vec!["super".into()],
                key: "q".into(),
                action: Action::CloseWindow,
            },
            Keybind {
                modifiers: vec!["super".into(), "shift".into()],
                key: "e".into(),
                action: Action::Quit,
            },
        ];
        // Super+1 through Super+9: switch workspace
        for i in 1..=9 {
            binds.push(Keybind {
                modifiers: vec!["super".into()],
                key: i.to_string(),
                action: Action::SwitchWorkspace(i - 1),
            });
        }
        // Super+Shift+1 through Super+Shift+9: move to workspace
        for i in 1..=9 {
            binds.push(Keybind {
                modifiers: vec!["super".into(), "shift".into()],
                key: i.to_string(),
                action: Action::MoveToWorkspace(i - 1),
            });
        }
        binds
    }
}

impl Config {
    /// Load config from the default path (`~/.config/beewm/config.toml`),
    /// falling back to defaults if the file doesn't exist.
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let path = Self::config_path();
        let config = if path.exists() {
            let contents = std::fs::read_to_string(&path)?;
            toml::from_str(&contents)?
        } else {
            tracing::info!("No config file found at {}, using defaults", path.display());
            Config::default()
        };

        Ok(config.validate()?)
    }

    fn config_path() -> PathBuf {
        let mut path = dirs_or_default();
        path.push("beewm");
        path.push("config.toml");
        path
    }

    fn validate(self) -> Result<Self, std::io::Error> {
        if self.num_workspaces == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "config num_workspaces must be at least 1",
            ));
        }

        if !self.master_ratio.is_finite() || !(0.0..=1.0).contains(&self.master_ratio) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "config master_ratio must be a finite value between 0.0 and 1.0",
            ));
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

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn rejects_zero_workspaces() {
        let err = Config {
            num_workspaces: 0,
            ..Config::default()
        }
        .validate()
        .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rejects_invalid_master_ratio() {
        let err = Config {
            master_ratio: 2.0,
            ..Config::default()
        }
        .validate()
        .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
