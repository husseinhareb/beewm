use std::path::Path;

use beewm::config::{Action, Config, ConfigError, LayoutKind};

fn remove_dir_all_if_exists(path: &Path) {
    if let Err(error) = std::fs::remove_dir_all(path) {
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "failed to remove {}: {}",
            path.display(),
            error
        );
    }
}

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
        exec waybar
        autostart mako

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
    assert_eq!(config.autostart_commands, vec!["waybar", "mako"]);
    assert_eq!(config.keybinds.len(), 4);
    assert_eq!(
        config.keybinds[0].action,
        Action::Spawn("kitty --single-instance".into())
    );
    assert_eq!(config.keybinds[1].action, Action::SwitchWorkspace(0));
    assert_eq!(config.keybinds[2].action, Action::MoveToWorkspace(0));
    assert_eq!(config.keybinds[3].action, Action::CloseWindow);
}

#[test]
fn fills_default_keybinds_for_custom_workspace_count() {
    let config = Config::parse("workspaces 4\n").unwrap();
    assert!(
        config
            .keybinds
            .iter()
            .all(|bind| !matches!(bind.action, Action::SwitchWorkspace(index) if index >= 4))
    );
    assert!(
        config
            .keybinds
            .iter()
            .all(|bind| !matches!(bind.action, Action::MoveToWorkspace(index) if index >= 4))
    );
}

#[test]
fn default_keybinds_include_zero_for_workspace_ten() {
    let config = Config::parse("workspaces 12\n").unwrap();

    let switch_bind_count = config
        .keybinds
        .iter()
        .filter(|bind| matches!(bind.action, Action::SwitchWorkspace(_)))
        .count();
    let move_bind_count = config
        .keybinds
        .iter()
        .filter(|bind| matches!(bind.action, Action::MoveToWorkspace(_)))
        .count();

    assert_eq!(switch_bind_count, 10);
    assert_eq!(move_bind_count, 10);
    assert!(
        config
            .keybinds
            .iter()
            .any(|bind| { bind.key == "0" && matches!(bind.action, Action::SwitchWorkspace(9)) })
    );
    assert!(
        config
            .keybinds
            .iter()
            .any(|bind| { bind.key == "0" && matches!(bind.action, Action::MoveToWorkspace(9)) })
    );
    assert!(
        config
            .keybinds
            .iter()
            .all(|bind| !matches!(bind.action, Action::SwitchWorkspace(index) if index >= 10))
    );
    assert!(
        config
            .keybinds
            .iter()
            .all(|bind| !matches!(bind.action, Action::MoveToWorkspace(index) if index >= 10))
    );
}

#[test]
fn parses_layout_aliases_and_command_synonyms() {
    let config = Config::parse(
        r#"
        set $term footclient
        layout master-stack
        master_ratio 0.75
        focus_follows_mouse 1
        tap_to_click 0
        natural_scroll on
        exec_once waybar
        bind $mod+Return exec $term
        "#,
    )
    .unwrap();

    assert_eq!(config.layout, LayoutKind::MasterStack);
    assert_eq!(config.split_ratio, 0.75);
    assert!(config.focus_follows_mouse);
    assert!(!config.tap_to_click);
    assert!(config.natural_scroll);
    assert_eq!(config.autostart_commands, vec!["waybar"]);
    assert_eq!(config.keybinds.len(), 1);
    assert_eq!(
        config.keybinds[0].action,
        Action::Spawn("footclient".into())
    );
}

#[test]
fn variable_substitution_prefers_the_longest_matching_name() {
    let config = Config::parse(
        r#"
        set $mod Mod4
        set $modShift Mod4+Shift
        bindsym $modShift+q kill
        "#,
    )
    .unwrap();

    assert_eq!(config.keybinds.len(), 1);
    assert_eq!(config.keybinds[0].modifiers, vec!["Mod4", "Shift"]);
    assert_eq!(config.keybinds[0].key, "q");
    assert_eq!(config.keybinds[0].action, Action::CloseWindow);
}

#[test]
fn custom_keybinds_replace_the_default_bind_set() {
    let config = Config::parse("bindsym mod4+x exec fuzzel\n").unwrap();

    assert_eq!(config.keybinds.len(), 1);
    assert_eq!(config.keybinds[0].key, "x");
    assert_eq!(config.keybinds[0].action, Action::Spawn("fuzzel".into()));
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
fn rejects_invalid_colors() {
    let err = Config::parse("border_color_focused #12345\n").unwrap_err();
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
    remove_dir_all_if_exists(&root);
    std::fs::create_dir_all(&root).unwrap();

    let path = root.join("config");
    let config = Config::load_from_path(&path).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();

    assert_eq!(config.layout, LayoutKind::Dwindle);
    assert_eq!(config.num_workspaces, 10);
    assert!(written.contains("layout dwindle"));
    assert!(written.contains("workspaces 10"));
    assert!(written.contains("# exec waybar"));
    assert!(written.contains("bindsym $mod+Return exec $terminal"));
    assert!(written.contains("bindsym $mod+0 workspace 10"));
    assert!(written.contains("bindsym $mod+Shift+0 move_to_workspace 10"));

    remove_dir_all_if_exists(&root);
}
