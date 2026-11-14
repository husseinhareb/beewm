use std::collections::HashMap;
use std::fmt;

use smithay::input::keyboard::xkb;

use super::{
    Action, Config, ConfigError, FocusDirection, Keybind, LayoutKind, OutputConfig, OutputModeSpec,
};

/// Built-in binding actions; must match `parse_action`. Any other action word
/// starts a command.
const BUILTIN_ACTIONS: &[&str] = &[
    "focus_next",
    "focus_prev",
    "focus_left",
    "focus_right",
    "focus_up",
    "focus_down",
    "focus_output_left",
    "focus_output_right",
    "focus_output_up",
    "focus_output_down",
    "move_to_output_left",
    "move_to_output_right",
    "move_to_output_up",
    "move_to_output_down",
    "close_window",
    "kill",
    "fullscreen",
    "toggle_fullscreen",
    "float",
    "toggle_float",
    "sticky",
    "toggle_sticky",
    "quit",
    "exit",
    "workspace",
    "switch_workspace",
    "move_to_workspace",
];

/// Modifier names a binding may use; must match `resolve_keybinds`.
const KNOWN_MODIFIERS: &[&str] = &[
    "super", "mod4", "logo", "shift", "ctrl", "control", "alt", "mod1",
];

pub(super) fn parse_config(contents: &str) -> Result<Config, ConfigError> {
    let defaults = Config::default();
    let mut config = Config {
        keybinds: Vec::new(),
        ..defaults
    };
    let mut variables = HashMap::<String, String>::new();
    let mut custom_keybinds = false;
    let mut bound_combos = HashMap::<(Vec<&'static str>, u32), usize>::new();

    for (index, raw_line) in contents.lines().enumerate() {
        let line_no = index + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Command text is handed to the shell, which treats `#` and `$VAR`
        // itself; everything else gets trailing comments stripped and must not
        // reference an undefined variable.
        let command_line = is_command_line(trimmed);
        let trimmed = if command_line {
            trimmed
        } else {
            strip_inline_comment(trimmed)
        };

        if let Some(rest) = trimmed.strip_prefix("set ") {
            parse_variable(rest, line_no, &mut variables)?;
            continue;
        }

        let line = substitute_variables(trimmed, &variables);
        let checked = if is_binding(&line) {
            // Only a binding's key combination is checked here: a command is the
            // shell's business, and built-in actions validate their own arguments.
            line.split_whitespace().nth(1).unwrap_or("")
        } else if command_line {
            ""
        } else {
            line.as_str()
        };
        if let Some(name) = first_unexpanded_variable(checked) {
            return Err(ConfigError::Parse {
                line: line_no,
                message: format!("undefined variable '{}'", name),
            });
        }
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
            "keyboard_layout" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.keyboard_layout = value.to_string();
            }
            "keyboard_variant" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.keyboard_variant = value.to_string();
            }
            "keyboard_options" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.keyboard_options = value.to_string();
            }
            "keyboard_model" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.keyboard_model = value.to_string();
            }
            "repeat_delay" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.repeat_delay = parse_number(value, line_no, directive)?;
            }
            "repeat_rate" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.repeat_rate = parse_number(value, line_no, directive)?;
            }
            "refresh_rate" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                let hz: u32 = parse_number(value, line_no, directive)?;
                config.refresh_rate = Some(hz);
            }
            "screen_timeout" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.screen_timeout = parse_number(value, line_no, directive)?;
            }
            "lock_command" => {
                config.lock_command = parse_command_value(&line, directive, line_no)?.to_string();
            }
            "lock_on_suspend" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.lock_on_suspend = parse_bool(value, line_no, directive)?;
            }
            "lock_on_resume" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.lock_on_resume = parse_bool(value, line_no, directive)?;
            }
            "overview_enabled" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.overview_enabled = parse_bool(value, line_no, directive)?;
            }
            // Primary, `key value` form — consistent with the rest of the config.
            "tray_enabled" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.tray_enabled = parse_bool(value, line_no, directive)?;
            }
            "tray_corner" => {
                return Err(ConfigError::Parse {
                    line: line_no,
                    message: "tray_corner was removed; tray placement is controlled by the \
                              StatusNotifier host"
                        .into(),
                });
            }
            // Alias subcommand form: `tray enable | disable`.
            "tray" => {
                let sub = parts.next().ok_or_else(|| ConfigError::Parse {
                    line: line_no,
                    message: "tray requires an argument: enable | disable".into(),
                })?;
                match sub {
                    "enable" | "on" | "true" => config.tray_enabled = true,
                    "disable" | "off" | "false" => config.tray_enabled = false,
                    "corner" => {
                        return Err(ConfigError::Parse {
                            line: line_no,
                            message: "tray corner was removed; tray placement is controlled by \
                                      the StatusNotifier host"
                                .into(),
                        });
                    }
                    other => {
                        return Err(ConfigError::Parse {
                            line: line_no,
                            message: format!("unknown tray subcommand '{}'", other),
                        });
                    }
                }
            }
            "enable_animations" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.enable_animations = parse_bool(value, line_no, directive)?;
            }
            "window_open_animation" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.window_open_animation = parse_bool(value, line_no, directive)?;
            }
            "window_close_animation" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.window_close_animation = parse_bool(value, line_no, directive)?;
            }
            "layout_animation" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.layout_animation = parse_bool(value, line_no, directive)?;
            }
            "disable_animations_for_fullscreen" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.disable_animations_for_fullscreen = parse_bool(value, line_no, directive)?;
            }
            "open_animation_duration_ms" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.open_animation_duration_ms = parse_number(value, line_no, directive)?;
            }
            "close_animation_duration_ms" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.close_animation_duration_ms = parse_number(value, line_no, directive)?;
            }
            "layout_animation_duration_ms" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                config.layout_animation_duration_ms = parse_number(value, line_no, directive)?;
            }
            "animation_duration_ms" => {
                // Convenience: set all three durations at once.
                let value = expect_single_argument(parts, line_no, directive)?;
                let ms: u64 = parse_number(value, line_no, directive)?;
                config.open_animation_duration_ms = ms;
                config.close_animation_duration_ms = ms;
                config.layout_animation_duration_ms = ms;
            }
            "animation_easing"
            | "open_animation_easing"
            | "close_animation_easing"
            | "layout_animation_easing" => {
                let value = expect_single_argument(parts, line_no, directive)?;
                if crate::compositor::animation::Easing::parse(value).is_none() {
                    return Err(ConfigError::Parse {
                        line: line_no,
                        message: format!(
                            "unknown {} '{}': expected linear | ease_in | ease_out | ease_in_out",
                            directive, value
                        ),
                    });
                }
                let value = value.to_string();
                match directive {
                    "open_animation_easing" => config.open_animation_easing = value,
                    "close_animation_easing" => config.close_animation_easing = value,
                    "layout_animation_easing" => config.layout_animation_easing = value,
                    // Convenience: set all three curves at once.
                    _ => {
                        config.open_animation_easing = value.clone();
                        config.close_animation_easing = value.clone();
                        config.layout_animation_easing = value;
                    }
                }
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
                let bind = parse_keybind(rest, line_no)?;
                // The key handler fires the first match, so a repeat would be
                // dead. Report it rather than silently ignoring one of them.
                if let Some(first) = bound_combos.insert(combo_identity(&bind), line_no) {
                    return Err(ConfigError::Parse {
                        line: line_no,
                        message: format!(
                            "{} is already bound on line {}",
                            rest.split_whitespace().next().unwrap_or(rest),
                            first
                        ),
                    });
                }
                config.keybinds.push(bind);
            }
            "output" => {
                let output = parse_output_config(parts, line_no)?;
                // Last stanza for a given connector wins.
                config
                    .outputs
                    .retain(|existing| existing.name != output.name);
                config.outputs.push(output);
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

/// Parse `output <name> [position X Y] [mode WxH[@Hz]] [disable|enable]`.
fn parse_output_config(
    mut parts: std::str::SplitWhitespace,
    line_no: usize,
) -> Result<OutputConfig, ConfigError> {
    let err = |message: String| ConfigError::Parse {
        line: line_no,
        message,
    };
    let name = parts
        .next()
        .ok_or_else(|| err("output requires a connector name".into()))?;
    let mut output = OutputConfig {
        name: name.to_string(),
        enabled: true,
        position: None,
        mode: None,
    };

    while let Some(key) = parts.next() {
        match key {
            "position" | "pos" => {
                let x = parts
                    .next()
                    .and_then(|v| v.parse::<i32>().ok())
                    .ok_or_else(|| err("output position needs integer X Y".into()))?;
                let y = parts
                    .next()
                    .and_then(|v| v.parse::<i32>().ok())
                    .ok_or_else(|| err("output position needs integer X Y".into()))?;
                output.position = Some((x, y));
            }
            "mode" | "resolution" => {
                let spec = parts
                    .next()
                    .ok_or_else(|| err("output mode needs a WxH[@Hz] value".into()))?;
                output.mode = Some(parse_mode_spec(spec, line_no)?);
            }
            "disable" | "off" => output.enabled = false,
            "enable" | "on" => output.enabled = true,
            other => {
                return Err(err(format!("unknown output option '{}'", other)));
            }
        }
    }

    Ok(output)
}

/// Parse a `WxH` or `WxH@Hz` mode string.
fn parse_mode_spec(value: &str, line_no: usize) -> Result<OutputModeSpec, ConfigError> {
    let err = |message: String| ConfigError::Parse {
        line: line_no,
        message,
    };
    let (dims, refresh) = match value.split_once('@') {
        Some((dims, hz)) => {
            let hz = hz
                .parse::<u32>()
                .map_err(|_| err(format!("invalid refresh rate in mode '{}'", value)))?;
            (dims, Some(hz))
        }
        None => (value, None),
    };
    let (w, h) = dims
        .split_once(['x', 'X'])
        .ok_or_else(|| err(format!("mode '{}' must be WxH[@Hz]", value)))?;
    let width = w
        .parse::<i32>()
        .map_err(|_| err(format!("invalid width in mode '{}'", value)))?;
    let height = h
        .parse::<i32>()
        .map_err(|_| err(format!("invalid height in mode '{}'", value)))?;
    if width <= 0 || height <= 0 || refresh == Some(0) {
        return Err(err(format!(
            "mode '{}' needs a positive size and refresh rate",
            value
        )));
    }
    Ok(OutputModeSpec {
        width,
        height,
        refresh,
    })
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

    let valid_name = name
        .strip_prefix('$')
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(is_variable_char));
    if !valid_name {
        return Err(ConfigError::Parse {
            line: line_no,
            message: format!(
                "invalid variable name '{}': use '$' followed by letters, digits or '_'",
                name
            ),
        });
    }
    if value.is_empty() {
        return Err(ConfigError::Parse {
            line: line_no,
            message: "set requires a non-empty value".into(),
        });
    }

    // Values may build on earlier variables (`set $alt_term $term --class x`).
    let value = substitute_variables(value, variables);
    variables.insert(name.to_string(), value);
    Ok(())
}

fn is_variable_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Length of the variable name starting right after a `$`.
fn variable_name_len(after_dollar: &str) -> usize {
    after_dollar
        .find(|c: char| !is_variable_char(c))
        .unwrap_or(after_dollar.len())
}

/// Replace each whole `$name` that is defined. Matching whole names (not
/// substrings) keeps `$h` from rewriting `$HOME`, and leaves an undefined
/// `$mod2` intact so it can be reported instead of becoming `mod42`.
fn substitute_variables(line: &str, variables: &HashMap<String, String>) -> String {
    let mut output = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find('$') {
        output.push_str(&rest[..pos]);
        let end = pos + 1 + variable_name_len(&rest[pos + 1..]);
        let name = &rest[pos..end];
        output.push_str(variables.get(name).map(String::as_str).unwrap_or(name));
        rest = &rest[end..];
    }
    output.push_str(rest);
    output
}

/// The first `$name` left after substitution, i.e. an undefined variable.
fn first_unexpanded_variable(text: &str) -> Option<&str> {
    let pos = text.find('$')?;
    let end = pos + 1 + variable_name_len(&text[pos + 1..]);
    Some(&text[pos..end])
}

/// A binding's key combination with modifier aliases and key case folded, so
/// `Mod4+Q` and `super+q` compare equal.
fn combo_identity(bind: &Keybind) -> (Vec<&'static str>, u32) {
    let mut modifiers: Vec<&'static str> = bind
        .modifiers
        .iter()
        .map(|modifier| match modifier.to_ascii_lowercase().as_str() {
            "super" | "mod4" | "logo" => "logo",
            "ctrl" | "control" => "ctrl",
            "alt" | "mod1" => "alt",
            _ => "shift",
        })
        .collect();
    modifiers.sort_unstable();
    modifiers.dedup();
    let key = xkb::keysym_from_name(&bind.key.to_ascii_lowercase(), xkb::KEYSYM_CASE_INSENSITIVE);
    (modifiers, key.raw())
}

/// Whether the line's text is (or ends in) a shell command, which must reach
/// the shell untouched.
fn is_command_line(line: &str) -> bool {
    let mut tokens = line.split_whitespace();
    match tokens.next() {
        Some("exec" | "exec_once" | "autostart" | "lock_command") => true,
        // `exec`/`spawn`, or a bare command (anything that is not a built-in).
        Some("bind" | "bindsym") => tokens
            .nth(1)
            .is_some_and(|action| !BUILTIN_ACTIONS.contains(&action)),
        _ => false,
    }
}

fn is_binding(line: &str) -> bool {
    matches!(line.split_whitespace().next(), Some("bind" | "bindsym"))
}

/// Drop a trailing `# comment`. The `#` must follow whitespace and be followed
/// by whitespace or the end of the line, so colors like `#5588ff` survive.
fn strip_inline_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (idx, &byte) in bytes.iter().enumerate() {
        if byte != b'#' || idx == 0 || !bytes[idx - 1].is_ascii_whitespace() {
            continue;
        }
        if bytes.get(idx + 1).is_none_or(u8::is_ascii_whitespace) {
            return line[..idx].trim_end();
        }
    }
    line
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
    for modifier in &combo_parts {
        if !KNOWN_MODIFIERS.contains(&modifier.to_ascii_lowercase().as_str()) {
            return Err(ConfigError::Parse {
                line: line_no,
                message: format!(
                    "unknown modifier '{}': expected super/mod4/logo, shift, ctrl/control or alt/mod1",
                    modifier
                ),
            });
        }
    }
    if xkb::keysym_from_name(&key, xkb::KEYSYM_CASE_INSENSITIVE).raw() == xkb::keysyms::KEY_NoSymbol
    {
        return Err(ConfigError::Parse {
            line: line_no,
            message: format!(
                "unknown key '{}' (use an XKB keysym name such as Return or F1)",
                key
            ),
        });
    }
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
        "focus_left" => Ok(Action::FocusDirection(FocusDirection::Left)),
        "focus_right" => Ok(Action::FocusDirection(FocusDirection::Right)),
        "focus_up" => Ok(Action::FocusDirection(FocusDirection::Up)),
        "focus_down" => Ok(Action::FocusDirection(FocusDirection::Down)),
        "focus_output_left" => Ok(Action::FocusOutput(FocusDirection::Left)),
        "focus_output_right" => Ok(Action::FocusOutput(FocusDirection::Right)),
        "focus_output_up" => Ok(Action::FocusOutput(FocusDirection::Up)),
        "focus_output_down" => Ok(Action::FocusOutput(FocusDirection::Down)),
        "move_to_output_left" => Ok(Action::MoveWindowToOutput(FocusDirection::Left)),
        "move_to_output_right" => Ok(Action::MoveWindowToOutput(FocusDirection::Right)),
        "move_to_output_up" => Ok(Action::MoveWindowToOutput(FocusDirection::Up)),
        "move_to_output_down" => Ok(Action::MoveWindowToOutput(FocusDirection::Down)),
        "close_window" | "kill" => Ok(Action::CloseWindow),
        "fullscreen" | "toggle_fullscreen" => Ok(Action::ToggleFullscreen),
        "float" | "toggle_float" => Ok(Action::ToggleFloat),
        "sticky" | "toggle_sticky" => Ok(Action::ToggleSticky),
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
        // A lone unknown word is almost always a misspelled action
        // (`fulscreen`), so reject it rather than trying to run it. Anything
        // longer is a bare command, kept for compatibility (prefer `exec`).
        other if !action_text.contains(char::is_whitespace) => Err(ConfigError::Parse {
            line: line_no,
            message: format!(
                "unknown action '{}': to run a program write 'exec {}'",
                other, action_text
            ),
        }),
        _ => Ok(Action::Spawn(action_text.to_string())),
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
