use beewm::model::workspace::Workspace;

#[test]
fn add_window_focuses_the_newest_window() {
    let mut workspace = Workspace::default();

    workspace.add_window();
    assert_eq!(workspace.window_count, 1);
    assert_eq!(workspace.focused_idx, Some(0));

    workspace.add_window();
    assert_eq!(workspace.window_count, 2);
    assert_eq!(workspace.focused_idx, Some(1));
}

#[test]
fn remove_window_clears_focus_when_last_window_is_removed() {
    let mut workspace = Workspace::default();
    workspace.add_window();

    workspace.remove_window(0);

    assert_eq!(workspace.window_count, 0);
    assert_eq!(workspace.focused_idx, None);
}

#[test]
fn removing_the_focused_window_moves_focus_to_the_new_tail() {
    let mut workspace = Workspace::default();
    for _ in 0..3 {
        workspace.add_window();
    }

    workspace.remove_window(2);

    assert_eq!(workspace.window_count, 2);
    assert_eq!(workspace.focused_idx, Some(1));
}

#[test]
fn removing_a_window_before_focus_shifts_focus_left() {
    let mut workspace = Workspace::default();
    for _ in 0..4 {
        workspace.add_window();
    }
    workspace.focused_idx = Some(3);

    workspace.remove_window(1);

    assert_eq!(workspace.window_count, 3);
    assert_eq!(workspace.focused_idx, Some(2));
}

#[test]
fn removing_an_out_of_bounds_window_is_a_noop() {
    let mut workspace = Workspace::default();
    for _ in 0..2 {
        workspace.add_window();
    }

    workspace.remove_window(3);

    assert_eq!(workspace.window_count, 2);
    assert_eq!(workspace.focused_idx, Some(1));
}

#[test]
fn removing_a_window_after_focus_keeps_focus_on_the_same_index() {
    let mut workspace = Workspace::default();
    for _ in 0..4 {
        workspace.add_window();
    }
    workspace.focused_idx = Some(1);

    workspace.remove_window(3);

    assert_eq!(workspace.window_count, 3);
    assert_eq!(workspace.focused_idx, Some(1));
}

#[test]
fn focus_navigation_wraps_in_both_directions() {
    let mut workspace = Workspace::default();
    for _ in 0..3 {
        workspace.add_window();
    }

    workspace.focus_next();
    assert_eq!(workspace.focused_idx, Some(0));

    workspace.focus_prev();
    assert_eq!(workspace.focused_idx, Some(2));
}

#[test]
fn focus_navigation_on_empty_workspace_keeps_no_focus() {
    let mut workspace = Workspace::default();

    workspace.focus_next();
    workspace.focus_prev();

    assert_eq!(workspace.window_count, 0);
    assert_eq!(workspace.focused_idx, None);
}

#[test]
fn removing_from_an_empty_workspace_is_a_noop() {
    let mut workspace = Workspace::default();

    workspace.remove_window(0);

    assert_eq!(workspace.window_count, 0);
    assert_eq!(workspace.focused_idx, None);
}

#[test]
fn single_window_focus_navigation_stays_on_that_window() {
    let mut workspace = Workspace::default();
    workspace.add_window();

    workspace.focus_next();
    assert_eq!(workspace.focused_idx, Some(0));

    workspace.focus_prev();
    assert_eq!(workspace.focused_idx, Some(0));
}
