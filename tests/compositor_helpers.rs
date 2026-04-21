use std::collections::HashMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use beewm::compositor::{
    FloatToggleTransition, ResizeEdges, ResizeHorizontalEdge, ResizeVerticalEdge,
    active_workspace_state_contents, constrain_popup_geometry, expand_by_border,
    float_toggle_transition, is_fixed_size, layers_hit_tested_after_windows,
    layers_hit_tested_before_windows, layers_rendered_above_windows, layers_rendered_below_windows,
    popup_constraint_target, resize_edges_for_pointer, resized_window_geometry_from_start,
    root_is_swap_highlighted, visible_border_rectangles, window_border_overlaps_layer,
    workspace_state_contents, write_state_file_atomically,
};
use beewm::layout::dwindle_tree::DwindleTree;
use beewm::model::window::Geometry;
use beewm::model::workspace::Workspace;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_positioner;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;
use smithay::wayland::shell::xdg::PositionerState;

fn rect(x: i32, y: i32, width: i32, height: i32) -> Rectangle<i32, Logical> {
    Rectangle::new((x, y).into(), (width, height).into())
}

fn rect_within(inner: Rectangle<i32, Logical>, outer: Rectangle<i32, Logical>) -> bool {
    inner.loc.x >= outer.loc.x
        && inner.loc.y >= outer.loc.y
        && inner.loc.x + inner.size.w <= outer.loc.x + outer.size.w
        && inner.loc.y + inner.size.h <= outer.loc.y + outer.size.h
}

fn geometry_map(entries: Vec<(u8, Geometry)>) -> HashMap<u8, Geometry> {
    entries.into_iter().collect()
}

fn workspaces(count: usize) -> Vec<Workspace> {
    std::iter::repeat_with(Workspace::default)
        .take(count)
        .collect()
}

#[test]
fn normal_layer_order_keeps_top_surfaces_above_windows() {
    assert_eq!(
        layers_rendered_above_windows(false),
        &[WlrLayer::Overlay, WlrLayer::Top]
    );
    assert_eq!(
        layers_rendered_below_windows(false),
        &[WlrLayer::Bottom, WlrLayer::Background]
    );
    assert_eq!(
        layers_hit_tested_before_windows(false),
        &[WlrLayer::Overlay, WlrLayer::Top]
    );
    assert_eq!(
        layers_hit_tested_after_windows(false),
        &[WlrLayer::Bottom, WlrLayer::Background]
    );
}

#[test]
fn fullscreen_moves_all_layer_surfaces_behind_windows() {
    assert!(layers_rendered_above_windows(true).is_empty());
    assert_eq!(
        layers_rendered_below_windows(true),
        &[
            WlrLayer::Overlay,
            WlrLayer::Top,
            WlrLayer::Bottom,
            WlrLayer::Background,
        ]
    );
    assert!(layers_hit_tested_before_windows(true).is_empty());
    assert!(layers_hit_tested_after_windows(true).is_empty());
}

#[test]
fn zero_size_is_not_treated_as_fixed() {
    assert!(!is_fixed_size(Size::<i32, Logical>::from((0, 480))));
    assert!(!is_fixed_size(Size::<i32, Logical>::from((640, 0))));
}

#[test]
fn non_zero_size_can_be_treated_as_fixed() {
    assert!(is_fixed_size(Size::<i32, Logical>::from((640, 480))));
}

#[test]
fn popup_constraint_target_is_translated_into_parent_space() {
    let parent_geometry = Rectangle::<i32, Logical>::new((240, 96).into(), (640, 32).into());
    let output_geometry = Rectangle::<i32, Logical>::new((0, 0).into(), (1920, 1080).into());

    assert_eq!(
        popup_constraint_target(parent_geometry, output_geometry),
        Rectangle::<i32, Logical>::new((-240, -96).into(), (1920, 1080).into()),
    );
}

#[test]
fn popup_geometry_stays_within_output_for_layer_shell_parent() {
    let parent_geometry = Rectangle::<i32, Logical>::new((0, 0).into(), (1920, 32).into());
    let output_geometry = Rectangle::<i32, Logical>::new((0, 0).into(), (1920, 1080).into());
    let positioner = PositionerState {
        rect_size: Size::from((320, 420)),
        anchor_rect: Rectangle::new((1888, 0).into(), (24, 32).into()),
        anchor_edges: xdg_positioner::Anchor::BottomRight,
        gravity: xdg_positioner::Gravity::BottomRight,
        constraint_adjustment: xdg_positioner::ConstraintAdjustment::FlipX
            | xdg_positioner::ConstraintAdjustment::SlideX
            | xdg_positioner::ConstraintAdjustment::FlipY
            | xdg_positioner::ConstraintAdjustment::SlideY,
        reactive: true,
        ..Default::default()
    };

    let popup_geometry = constrain_popup_geometry(positioner, parent_geometry, output_geometry);
    let popup_global_geometry = Rectangle::new(
        parent_geometry.loc + popup_geometry.loc,
        popup_geometry.size,
    );

    assert_eq!(popup_geometry.size, Size::from((320, 420)));
    assert!(rect_within(popup_global_geometry, output_geometry));
    assert!(popup_global_geometry.loc.x < 1888);
}

#[test]
fn popup_geometry_uses_parent_global_offset_when_constraining() {
    let parent_geometry = Rectangle::<i32, Logical>::new((300, 180).into(), (420, 240).into());
    let output_geometry = Rectangle::<i32, Logical>::new((0, 0).into(), (1280, 720).into());
    let positioner = PositionerState {
        rect_size: Size::from((480, 260)),
        anchor_rect: Rectangle::new((360, 200).into(), (24, 24).into()),
        anchor_edges: xdg_positioner::Anchor::BottomRight,
        gravity: xdg_positioner::Gravity::BottomRight,
        constraint_adjustment: xdg_positioner::ConstraintAdjustment::FlipX
            | xdg_positioner::ConstraintAdjustment::SlideX
            | xdg_positioner::ConstraintAdjustment::FlipY
            | xdg_positioner::ConstraintAdjustment::SlideY,
        ..Default::default()
    };

    let popup_geometry = constrain_popup_geometry(positioner, parent_geometry, output_geometry);
    let popup_global_geometry = Rectangle::new(
        parent_geometry.loc + popup_geometry.loc,
        popup_geometry.size,
    );

    assert_eq!(popup_geometry.size, Size::from((480, 260)));
    assert!(rect_within(popup_global_geometry, output_geometry));
}

#[test]
fn fullscreened_floating_window_stays_floating_when_toggling_float() {
    assert_eq!(
        float_toggle_transition(true, true),
        FloatToggleTransition::KeepFloating
    );
}

#[test]
fn fullscreened_tiled_window_becomes_floating_when_toggling_float() {
    assert_eq!(
        float_toggle_transition(true, false),
        FloatToggleTransition::MakeFloating
    );
}

#[test]
fn non_fullscreen_floating_window_sinks_back_to_tiling() {
    assert_eq!(
        float_toggle_transition(false, true),
        FloatToggleTransition::SinkToTiling
    );
}

#[test]
fn resize_edges_use_the_window_center_as_the_anchor_split() {
    let edges = resize_edges_for_pointer(
        Point::<i32, Logical>::from((100, 200)),
        Size::<i32, Logical>::from((300, 200)),
        Point::<f64, Logical>::from((120.0, 220.0)),
    );
    assert_eq!(
        edges,
        ResizeEdges {
            horizontal: ResizeHorizontalEdge::Left,
            vertical: ResizeVerticalEdge::Top,
        }
    );

    let edges = resize_edges_for_pointer(
        Point::<i32, Logical>::from((100, 200)),
        Size::<i32, Logical>::from((300, 200)),
        Point::<f64, Logical>::from((399.0, 399.0)),
    );
    assert_eq!(
        edges,
        ResizeEdges {
            horizontal: ResizeHorizontalEdge::Right,
            vertical: ResizeVerticalEdge::Bottom,
        }
    );
}

#[test]
fn resizing_from_the_bottom_right_grows_width_and_height_only() {
    let (pos, size) = resized_window_geometry_from_start(
        Point::<i32, Logical>::from((100, 200)),
        Size::<i32, Logical>::from((300, 150)),
        Point::<f64, Logical>::from((400.0, 350.0)),
        Point::<f64, Logical>::from((460.0, 390.0)),
        ResizeEdges {
            horizontal: ResizeHorizontalEdge::Right,
            vertical: ResizeVerticalEdge::Bottom,
        },
    );
    assert_eq!(pos, Point::from((100, 200)));
    assert_eq!(size, Size::from((360, 190)));
}

#[test]
fn resizing_from_the_top_left_keeps_the_bottom_right_corner_fixed() {
    let (pos, size) = resized_window_geometry_from_start(
        Point::<i32, Logical>::from((100, 200)),
        Size::<i32, Logical>::from((300, 150)),
        Point::<f64, Logical>::from((100.0, 200.0)),
        Point::<f64, Logical>::from((70.0, 170.0)),
        ResizeEdges {
            horizontal: ResizeHorizontalEdge::Left,
            vertical: ResizeVerticalEdge::Top,
        },
    );
    assert_eq!(pos, Point::from((70, 170)));
    assert_eq!(size, Size::from((330, 180)));
}

#[test]
fn resizing_from_left_and_top_clamps_at_one_pixel() {
    let (pos, size) = resized_window_geometry_from_start(
        Point::<i32, Logical>::from((100, 200)),
        Size::<i32, Logical>::from((300, 150)),
        Point::<f64, Logical>::from((100.0, 200.0)),
        Point::<f64, Logical>::from((500.0, 500.0)),
        ResizeEdges {
            horizontal: ResizeHorizontalEdge::Left,
            vertical: ResizeVerticalEdge::Top,
        },
    );
    assert_eq!(pos, Point::from((399, 349)));
    assert_eq!(size, Size::from((1, 1)));
}

#[test]
fn splits_the_focused_leaf_instead_of_the_remaining_screen() {
    let mut tree = DwindleTree::default();
    let screen = Geometry::new(0, 0, 100, 100);

    tree.insert(None, 1);
    tree.insert(Some(&1), 2);
    tree.insert(Some(&1), 3);
    tree.insert(Some(&2), 4);

    let geometries = geometry_map(tree.geometries(&screen, 0.5));

    assert_eq!(geometries[&1], Geometry::new(0, 0, 50, 50));
    assert_eq!(geometries[&2], Geometry::new(50, 0, 50, 50));
    assert_eq!(geometries[&3], Geometry::new(0, 50, 50, 50));
    assert_eq!(geometries[&4], Geometry::new(50, 50, 50, 50));
}

#[test]
fn swapping_two_leaves_exchanges_their_geometries() {
    let mut tree = DwindleTree::default();
    let screen = Geometry::new(0, 0, 100, 100);

    tree.insert(None, 1);
    tree.insert(Some(&1), 2);
    tree.insert(Some(&1), 3);
    assert!(tree.swap(&1, &2));

    let geometries = geometry_map(tree.geometries(&screen, 0.5));

    assert_eq!(geometries[&1], Geometry::new(50, 0, 50, 100));
    assert_eq!(geometries[&2], Geometry::new(0, 0, 50, 50));
    assert_eq!(geometries[&3], Geometry::new(0, 50, 50, 50));
}

#[test]
fn active_workspace_export_uses_one_based_numbers() {
    assert_eq!(active_workspace_state_contents(0), "1");
    assert_eq!(active_workspace_state_contents(4), "5");
}

#[test]
fn workspace_state_export_lists_active_and_occupied_workspaces() {
    let mut workspaces = workspaces(5);
    workspaces[0].add_window(());
    workspaces[2].add_window(());
    workspaces[4].add_window(());

    let state = workspace_state_contents(2, &workspaces);

    assert_eq!(state, "active=3\noccupied=1,3,5\n");
}

#[test]
fn workspace_state_export_handles_no_occupied_workspaces() {
    let workspaces = workspaces(3);

    let state = workspace_state_contents(1, &workspaces);

    assert_eq!(state, "active=2\noccupied=\n");
}

#[test]
fn state_file_writes_are_atomic_and_replace_previous_contents() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("beewm-state-test-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("workspaces");

    write_state_file_atomically(&path, "active=1\noccupied=1\n").unwrap();
    write_state_file_atomically(&path, "active=2\noccupied=2,3\n").unwrap();

    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "active=2\noccupied=2,3\n"
    );

    let leftovers = fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
        .count();
    assert_eq!(leftovers, 0);

    fs::remove_file(&path).unwrap();
    fs::remove_dir(&dir).unwrap();
}

#[test]
fn reserved_top_bar_does_not_hide_borders() {
    let window = rect(4, 34, 400, 300);
    let top_bar = rect(0, 0, 1920, 30);

    assert!(!window_border_overlaps_layer(window, top_bar, 2));
}

#[test]
fn fullscreen_overlay_hides_borders() {
    let window = rect(100, 100, 400, 300);
    let overlay = rect(0, 0, 1920, 1080);

    assert!(window_border_overlaps_layer(window, overlay, 2));
}

#[test]
fn centered_popup_does_not_hide_borders() {
    let window = rect(100, 100, 400, 300);
    let popup = rect(180, 160, 120, 80);

    assert!(!window_border_overlaps_layer(window, popup, 2));
}

#[test]
fn popup_crossing_border_hides_borders() {
    let window = rect(100, 100, 400, 300);
    let popup = rect(98, 120, 24, 80);

    assert!(window_border_overlaps_layer(window, popup, 2));
}

#[test]
fn swap_highlight_matches_dragged_and_target_roots() {
    assert!(root_is_swap_highlighted(&1, Some(&1), Some(&2)));
    assert!(root_is_swap_highlighted(&2, Some(&1), Some(&2)));
    assert!(!root_is_swap_highlighted(&3, Some(&1), Some(&2)));
}

#[test]
fn floating_window_clips_the_overlapped_border_segments() {
    let window = rect(100, 100, 400, 300);
    let floating = rect(180, 98, 120, 40);

    let visible = visible_border_rectangles(window, 2, &[floating]);

    assert!(!visible.is_empty());
    assert!(visible.iter().all(|border| !border.overlaps(floating)));
    assert!(visible.iter().any(|border| border.loc.y == 98));
}

#[test]
fn non_overlapping_floating_window_keeps_all_four_borders() {
    let window = rect(100, 100, 400, 300);
    let floating = rect(180, 160, 120, 80);

    let visible = visible_border_rectangles(window, 2, &[floating]);

    assert_eq!(visible.len(), 4);
}

#[test]
fn floating_window_border_also_clips_the_window_behind_it() {
    let window = rect(100, 100, 400, 300);
    let floating_client = rect(180, 100, 120, 40);
    let floating_with_border = expand_by_border(floating_client, 2);

    let visible = visible_border_rectangles(window, 2, &[floating_with_border]);

    assert!(!visible.is_empty());
    assert!(
        visible
            .iter()
            .all(|border| !border.overlaps(floating_with_border))
    );
    assert!(visible.iter().any(|border| border.loc.y == 98));
}
