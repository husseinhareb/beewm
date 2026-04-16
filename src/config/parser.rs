use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt;

use super::{Action, Config, ConfigError, Keybind, LayoutKind};

pub(super) fn parse_config(contents: &str) -> Result<Config, ConfigError> {
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
            "exec" | "exec_once" | "autostart" => {
                config
                    .autostart_commands
                    .push(parse_command_value(&line, directive, line_no)?.to_string());
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
        config.keybinds = Config::default_keybinds_for(config.num_workspaces);
    }

    config.validate()
}

fn parse_command_value<'a>(
    line: &'a str,
    directive: &str,
    line_no: usize,
) -> Result<&'a str, ConfigError> {
    let command = line[directive.len()..].trim();
    if command.is_empty() {
        return Err(ConfigError::Parse {
            line: line_no,
            message: format!("{} requires a command", directive),
        });
    }

    Ok(command)
}

fn parse_variable(
    rest: &str,
    line_no: usize,
    variables: &mut HashMap<String, String>,
) -> Result<(), ConfigError> {
    let split_at = rest
        .find(char::is_whitespace)
        .ok_or_else(|| ConfigError::Parse {
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
    let split_at = rest
        .find(char::is_whitespace)
        .ok_or_else(|| ConfigError::Parse {
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
            let command = parse_command_value(action_text, name, line_no)?;
            Ok(Action::Spawn(command.to_string()))
        }
        "focus_next" => Ok(Action::FocusNext),
        "focus_prev" => Ok(Action::FocusPrev),
        "close_window" | "kill" => Ok(Action::CloseWindow),
        "fullscreen" | "toggle_fullscreen" => Ok(Action::ToggleFullscreen),
        "float" | "toggle_float" => Ok(Action::ToggleFloat),
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
