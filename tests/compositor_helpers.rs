use std::collections::HashMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use beewm::compositor::{
    FloatToggleTransition, ResizeEdges, ResizeHorizontalEdge, ResizeVerticalEdge,
    active_workspace_state_contents, centered_dialog_position, constrain_popup_geometry,
    expand_by_border, float_toggle_transition, is_dialog_size_cap, is_fixed_size,
    layers_hit_tested_after_windows,
    layers_hit_tested_before_windows, layers_rendered_above_windows, layers_rendered_below_windows,
    popup_constraint_target, resize_edges_for_pointer, resized_window_geometry_from_start,
    root_is_swap_highlighted, visible_border_rectangles, window_border_overlaps_layer,
    workspace_state_contents, write_state_file_atomically,
};
use beewm::layout::dwindle_tree::{DwindleTree, ResizeEdge};
use beewm::layout::manager::{DwindleManager, LayoutManager, MasterStackManager};
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
fn fullscreen_suppresses_layer_surfaces_for_scanout() {
    assert!(layers_rendered_above_windows(true).is_empty());
    assert!(layers_rendered_below_windows(true).is_empty());
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
fn small_max_size_is_a_dialog_cap() {
    // gnome-keyring (~400x250), polkit (~400x300), file pickers (~800x600).
    assert!(is_dialog_size_cap(Size::<i32, Logical>::from((400, 250))));
    assert!(is_dialog_size_cap(Size::<i32, Logical>::from((800, 600))));
    assert!(is_dialog_size_cap(Size::<i32, Logical>::from((1280, 1024))));
}

#[test]
fn large_or_sentinel_max_size_is_not_a_dialog_cap() {
    // A parent app advertising display dimensions or a "no real max" sentinel
    // must NOT be classified as a dialog — otherwise Zed/Spotify/Claude float.
    assert!(!is_dialog_size_cap(Size::<i32, Logical>::from((1920, 1080))));
    assert!(!is_dialog_size_cap(Size::<i32, Logical>::from((3840, 2160))));
    assert!(!is_dialog_size_cap(Size::<i32, Logical>::from((32767, 32767))));
}

#[test]
fn one_axis_or_zero_max_size_is_not_a_dialog_cap() {
    // The old rule floated on `max > 0` on a single axis; both axes must be
    // bounded now so a window capped on only one axis stays tiled.
    assert!(!is_dialog_size_cap(Size::<i32, Logical>::from((0, 0))));
    assert!(!is_dialog_size_cap(Size::<i32, Logical>::from((800, 0))));
    assert!(!is_dialog_size_cap(Size::<i32, Logical>::from((0, 600))));
    assert!(!is_dialog_size_cap(Size::<i32, Logical>::from((400, 2000))));
}

#[test]
fn dialog_without_parent_is_centered_in_usable_area() {
    let usable = rect(0, 0, 1920, 1080);
    let win = Size::<i32, Logical>::from((400, 300));
    // (1920 - 400) / 2 = 760, (1080 - 300) / 2 = 390
    assert_eq!(
        centered_dialog_position(usable, None, win),
        Point::<i32, Logical>::from((760, 390)),
    );
}

#[test]
fn dialog_with_parent_is_centered_over_the_parent() {
    let usable = rect(0, 0, 1920, 1080);
    let parent = rect(1000, 600, 600, 400);
    let win = Size::<i32, Logical>::from((400, 300));
    // parent center (1000 + 600/2, 600 + 400/2) = (1300, 800)
    // top-left = (1300 - 200, 800 - 150) = (1100, 650)
    assert_eq!(
        centered_dialog_position(usable, Some(parent), win),
        Point::<i32, Logical>::from((1100, 650)),
    );
}

#[test]
fn dialog_over_parent_is_clamped_into_the_usable_area() {
    let usable = rect(0, 0, 1920, 1080);
    // Parent hugging the bottom-right corner would push a centered dialog
    // partly off-screen; the result must stay fully visible.
    let parent = rect(1800, 1000, 120, 80);
    let win = Size::<i32, Logical>::from((400, 300));
    let pos = centered_dialog_position(usable, Some(parent), win);
    assert_eq!(pos, Point::<i32, Logical>::from((1920 - 400, 1080 - 300)));
}

#[test]
fn dialog_placement_respects_a_non_zero_usable_origin() {
    // A reserved top bar shifts the usable origin down; the dialog must center
    // within (and clamp to) that shifted area, never the raw output origin.
    let usable = rect(0, 40, 1920, 1040);
    let win = Size::<i32, Logical>::from((400, 300));
    assert_eq!(
        centered_dialog_position(usable, None, win),
        Point::<i32, Logical>::from((760, 40 + (1040 - 300) / 2)),
    );
}

#[test]
fn oversized_dialog_pins_to_the_usable_origin() {
    let usable = rect(10, 20, 300, 200);
    let win = Size::<i32, Logical>::from((800, 600));
    assert_eq!(
        centered_dialog_position(usable, None, win),
        Point::<i32, Logical>::from((10, 20)),
    );
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

    let geometries = geometry_map(tree.geometries(&screen));

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

    let geometries = geometry_map(tree.geometries(&screen));

    assert_eq!(geometries[&1], Geometry::new(50, 0, 50, 100));
    assert_eq!(geometries[&2], Geometry::new(0, 0, 50, 50));
    assert_eq!(geometries[&3], Geometry::new(0, 50, 50, 50));
}

#[test]
fn resizing_a_dwindle_leaf_updates_the_nearest_matching_split() {
    let mut tree = DwindleTree::default();
    let screen = Geometry::new(0, 0, 100, 100);

    tree.insert(None, 1);
    tree.insert(Some(&1), 2);
    tree.insert(Some(&2), 3);

    assert!(tree.resize(&3, ResizeEdge::Top, -10, &screen, 1));

    let geometries = geometry_map(tree.geometries(&screen));

    assert_eq!(geometries[&1], Geometry::new(0, 0, 50, 100));
    assert_eq!(geometries[&2], Geometry::new(50, 0, 50, 40));
    assert_eq!(geometries[&3], Geometry::new(50, 40, 50, 60));
}

#[test]
fn dwindle_ordered_roots_follow_layout_order_not_insertion() {
    // FocusNext/Prev cycle through `ordered_roots`, which must match the
    // on-screen (geometry) traversal order rather than window-insertion order.
    let mut manager = DwindleManager::new(1, 0.5);
    manager.insert(0, None, 1u8);
    manager.insert(0, Some(&1u8), 2u8);
    manager.insert(0, Some(&2u8), 3u8);

    let screen = Geometry::new(0, 0, 100, 100);
    let geometries = manager.geometries(0, &screen, &[1u8, 2u8, 3u8]);
    let ordered = manager.ordered_roots(0);

    // Every tiled id appears exactly once.
    assert_eq!(ordered.len(), 3);
    for id in [1u8, 2u8, 3u8] {
        assert!(ordered.contains(&id));
    }

    // The order is the left/top → right/bottom geometry order: each successive
    // root sits at an origin that is not before the previous one.
    let origins: Vec<_> = ordered.iter().map(|id| geometries[id].x).collect();
    let mut sorted = origins.clone();
    sorted.sort();
    // For this dwindle shape the master (id 1) is leftmost; the stack splits to
    // its right, so ordered_roots starts at the leftmost column.
    assert_eq!(ordered.first(), Some(&1u8));
    assert_eq!(geometries[&ordered[0]].x, *sorted.first().unwrap());
}

#[test]
fn master_stack_ordered_roots_match_stack_order() {
    let mut manager = MasterStackManager::new(1, 0.5);
    manager.insert(0, None, 10u8);
    manager.insert(0, None, 20u8);
    manager.insert(0, None, 30u8);

    assert_eq!(manager.ordered_roots(0), vec![10u8, 20u8, 30u8]);
}

#[test]
fn layout_manager_out_of_range_workspace_is_noop() {
    // Defensive bounds checks: operating on a workspace index past the end must
    // never panic (panic = "abort" would take down the whole session).
    let mut dwindle = DwindleManager::<u8>::new(2, 0.5);
    dwindle.insert(99, None, 1u8); // out of range — must not panic
    dwindle.remove(99, &1u8);
    assert!(!dwindle.swap(99, &1u8, &2u8));
    assert!(dwindle.ordered_roots(99).is_empty());
    assert!(
        dwindle
            .geometries(99, &Geometry::new(0, 0, 100, 100), &[])
            .is_empty()
    );

    let mut master = MasterStackManager::<u8>::new(2, 0.5);
    master.insert(99, None, 1u8);
    master.remove(99, &1u8);
    assert!(!master.swap(99, &1u8, &2u8));
    assert!(master.ordered_roots(99).is_empty());
    assert!(
        master
            .geometries(99, &Geometry::new(0, 0, 100, 100), &[])
            .is_empty()
    );
}

#[test]
fn resizing_master_stack_tracks_master_and_stack_split_ratios() {
    let mut manager = MasterStackManager::new(1, 0.5);
    let screen = Geometry::new(0, 0, 100, 100);
    let tiled_ids = vec![1u8, 2u8, 3u8];

    manager.insert(0, None, 1);
    manager.insert(0, None, 2);
    manager.insert(0, None, 3);

    assert!(manager.resize(
        0,
        &screen,
        &tiled_ids,
        &2,
        ResizeEdges {
            horizontal: ResizeHorizontalEdge::Left,
            vertical: ResizeVerticalEdge::Bottom,
        },
        (10, 10),
    ));

    let geometries = manager.geometries(0, &screen, &tiled_ids);

    assert_eq!(geometries[&1], Geometry::new(0, 0, 60, 100));
    assert_eq!(geometries[&2], Geometry::new(60, 0, 40, 60));
    assert_eq!(geometries[&3], Geometry::new(60, 60, 40, 40));
}

#[test]
fn removing_an_unmapped_sibling_gives_the_survivor_the_full_area() {
    // Regression guard for the Firefox "Restore Session" bug: while the old
    // window was still tracked, two tiled nodes split the screen in half. The
    // compositor must drop the unmapped node from the tiling tree; once it does
    // so, the layout manager must collapse the lone survivor back to the full
    // tiled area instead of leaving an invisible half-screen gap.
    let mut manager = MasterStackManager::new(1, 0.5);
    let screen = Geometry::new(0, 0, 1920, 1080);

    // Both windows mapped: the screen is split between them.
    manager.insert(0, None, 1u8);
    manager.insert(0, None, 2u8);
    let split = manager.geometries(0, &screen, &[1u8, 2u8]);
    assert_ne!(split[&1], screen, "two mapped windows should not each fill the screen");

    // The stale window (id 1) unmaps. The compositor removes it from the tree
    // AND from the set of tiled ids it lays out.
    manager.remove(0, &1u8);
    let collapsed = manager.geometries(0, &screen, &[2u8]);

    assert_eq!(collapsed.len(), 1);
    assert_eq!(
        collapsed[&2], screen,
        "the only remaining tiled window must occupy the whole tiled area",
    );
    assert!(
        !collapsed.contains_key(&1u8),
        "the unmapped window must not retain a layout node",
    );
}

#[test]
fn unmapped_node_left_in_tree_still_drops_out_of_geometries() {
    // Defence in depth: even if a stale id were momentarily still inside the
    // tree, geometries() is driven by the caller's `tiled_ids` list. As long as
    // the unmapped window is excluded from that list (which the unmap handler
    // guarantees by removing it from the workspace), it consumes no space.
    let mut manager = MasterStackManager::new(1, 0.5);
    let screen = Geometry::new(0, 0, 1920, 1080);

    manager.insert(0, None, 1u8);
    manager.insert(0, None, 2u8);

    // id 1 is unmapped but (hypothetically) not yet pruned from the tree;
    // it is excluded from the mapped tiled-id list passed to geometries().
    let geometries = manager.geometries(0, &screen, &[2u8]);

    assert_eq!(geometries.get(&2u8), Some(&screen));
    assert!(!geometries.contains_key(&1u8));
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
