use std::path::Path;

use beewm::config::{
    Action, Config, ConfigError, FocusDirection, LayoutKind, OutputConfig, OutputModeSpec,
};

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
    assert!(config.keybinds.iter().any(|bind| {
        bind.key == "Left" && matches!(bind.action, Action::FocusDirection(FocusDirection::Left))
    }));
    assert!(config.keybinds.iter().any(|bind| {
        bind.key == "Right" && matches!(bind.action, Action::FocusDirection(FocusDirection::Right))
    }));
    assert!(config.keybinds.iter().any(|bind| {
        bind.key == "Up" && matches!(bind.action, Action::FocusDirection(FocusDirection::Up))
    }));
    assert!(config.keybinds.iter().any(|bind| {
        bind.key == "Down" && matches!(bind.action, Action::FocusDirection(FocusDirection::Down))
    }));
    assert!(
        !config
            .keybinds
            .iter()
            .any(|bind| bind.key == "j" && matches!(bind.action, Action::FocusNext))
    );
    assert!(
        !config
            .keybinds
            .iter()
            .any(|bind| bind.key == "k" && matches!(bind.action, Action::FocusPrev))
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
fn parses_directional_focus_actions() {
    let config = Config::parse(
        r#"
        bindsym mod4+Right focus_right
        bindsym mod4+Left focus_left
        bindsym mod4+Up focus_up
        bindsym mod4+Down focus_down
        "#,
    )
    .unwrap();

    assert_eq!(config.keybinds.len(), 4);
    assert_eq!(
        config.keybinds[0].action,
        Action::FocusDirection(FocusDirection::Right)
    );
    assert_eq!(
        config.keybinds[1].action,
        Action::FocusDirection(FocusDirection::Left)
    );
    assert_eq!(
        config.keybinds[2].action,
        Action::FocusDirection(FocusDirection::Up)
    );
    assert_eq!(
        config.keybinds[3].action,
        Action::FocusDirection(FocusDirection::Down)
    );
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
    assert!(written.contains("bindsym $mod+Left focus_left"));
    assert!(written.contains("bindsym $mod+Right focus_right"));
    assert!(written.contains("bindsym $mod+Up focus_up"));
    assert!(written.contains("bindsym $mod+Down focus_down"));
    assert!(!written.contains("bindsym $mod+j focus_next"));
    assert!(!written.contains("bindsym $mod+k focus_prev"));
    assert!(written.contains("bindsym $mod+0 workspace 10"));
    assert!(written.contains("bindsym $mod+Shift+0 move_to_workspace 10"));
    // Animation defaults are written and round-trip back into the config.
    assert!(written.contains("enable_animations true"));
    assert!(written.contains("animation_easing ease_out"));
    assert!(config.enable_animations);
    assert_eq!(config.open_animation_duration_ms, 180);

    remove_dir_all_if_exists(&root);
}

#[test]
fn parses_animation_settings() {
    let config = Config::parse(
        r#"
        enable_animations no
        window_open_animation off
        window_close_animation true
        layout_animation yes
        disable_animations_for_fullscreen false
        open_animation_duration_ms 200
        close_animation_duration_ms 120
        layout_animation_duration_ms 240
        animation_easing ease_in_out
        "#,
    )
    .expect("animation config should parse");

    assert!(!config.enable_animations);
    assert!(!config.window_open_animation);
    assert!(config.window_close_animation);
    assert!(config.layout_animation);
    assert!(!config.disable_animations_for_fullscreen);
    assert_eq!(config.open_animation_duration_ms, 200);
    assert_eq!(config.close_animation_duration_ms, 120);
    assert_eq!(config.layout_animation_duration_ms, 240);
    assert_eq!(config.animation_easing, "ease_in_out");
}

#[test]
fn animation_duration_ms_sets_all_three() {
    let config = Config::parse("animation_duration_ms 100\n").expect("should parse");
    assert_eq!(config.open_animation_duration_ms, 100);
    assert_eq!(config.close_animation_duration_ms, 100);
    assert_eq!(config.layout_animation_duration_ms, 100);
}

#[test]
fn parses_output_directives() {
    let config = Config::parse(
        r#"
        output eDP-1 position 0 0
        output DP-3 position 2560 0 mode 2560x1440@165
        output HDMI-A-1 disable
        "#,
    )
    .expect("output config should parse");

    assert_eq!(
        config.outputs,
        vec![
            OutputConfig {
                name: "eDP-1".into(),
                enabled: true,
                position: Some((0, 0)),
                mode: None,
            },
            OutputConfig {
                name: "DP-3".into(),
                enabled: true,
                position: Some((2560, 0)),
                mode: Some(OutputModeSpec {
                    width: 2560,
                    height: 1440,
                    refresh: Some(165),
                }),
            },
            OutputConfig {
                name: "HDMI-A-1".into(),
                enabled: false,
                position: None,
                mode: None,
            },
        ]
    );
}

#[test]
fn output_mode_without_refresh_parses() {
    let config = Config::parse("output DP-1 mode 1920x1080\n").expect("should parse");
    assert_eq!(
        config.outputs[0].mode,
        Some(OutputModeSpec {
            width: 1920,
            height: 1080,
            refresh: None,
        })
    );
}

#[test]
fn later_output_stanza_replaces_earlier_for_same_connector() {
    let config = Config::parse("output DP-1 position 0 0\noutput DP-1 position 100 200 disable\n")
        .expect("should parse");
    assert_eq!(config.outputs.len(), 1);
    assert_eq!(config.outputs[0].position, Some((100, 200)));
    assert!(!config.outputs[0].enabled);
}

#[test]
fn rejects_unknown_output_option() {
    let error = Config::parse("output DP-1 bogus\n").unwrap_err();
    assert!(matches!(error, ConfigError::Parse { .. }));
}

#[test]
fn tray_is_enabled_by_default_and_can_be_disabled() {
    let config = Config::parse("layout dwindle\n").expect("should parse");
    assert!(config.tray_enabled);

    let off = Config::parse("tray disable\n").expect("should parse");
    assert!(!off.tray_enabled);

    // `key value` form (consistent with the rest of the config) is also accepted.
    let kv_off = Config::parse("tray_enabled false\n").expect("should parse");
    assert!(!kv_off.tray_enabled);
}

#[test]
fn parses_tray_enable() {
    let config = Config::parse("tray enable\n").expect("should parse");
    assert!(config.tray_enabled);
}

#[test]
fn tray_disable_turns_it_off() {
    let config = Config::parse("tray enable\ntray disable\n").expect("should parse");
    assert!(!config.tray_enabled);
}

#[test]
fn rejects_removed_tray_corner_directives() {
    let error = Config::parse("tray corner bottom_left\n").unwrap_err();
    assert!(matches!(
        error,
        ConfigError::Parse {
            message,
            ..
        } if message.contains("tray corner was removed")
    ));

    let error = Config::parse("tray_corner bottom_left\n").unwrap_err();
    assert!(matches!(
        error,
        ConfigError::Parse {
            message,
            ..
        } if message.contains("tray_corner was removed")
    ));
}

#[test]
fn rejects_unknown_tray_subcommand() {
    let error = Config::parse("tray bogus\n").unwrap_err();
    assert!(matches!(error, ConfigError::Parse { .. }));
}

#[test]
fn screen_timeout_defaults_and_parses() {
    assert_eq!(Config::default().screen_timeout, 600);
    let config = Config::parse("screen_timeout 120\n").expect("should parse");
    assert_eq!(config.screen_timeout, 120);
    let off = Config::parse("screen_timeout 0\n").expect("should parse");
    assert_eq!(off.screen_timeout, 0);
}

#[test]
fn lock_settings_default_and_parse() {
    let default = Config::default();
    assert_eq!(default.lock_command, "beelock");
    assert!(default.lock_on_suspend);
    assert!(default.lock_on_resume);

    let config = Config::parse(
        "lock_command swaylock --daemonize\n\
         lock_on_suspend false\n\
         lock_on_resume true\n",
    )
    .expect("should parse");
    assert_eq!(config.lock_command, "swaylock --daemonize");
    assert!(!config.lock_on_suspend);
    assert!(config.lock_on_resume);
}
