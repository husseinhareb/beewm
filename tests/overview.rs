use beewm::compositor::overview::{
    OverviewNav, cell_rects, fit, grid_columns, nav_spatial, place_in_panel,
};
use smithay::utils::{Logical, Rectangle};

fn screen(w: i32, h: i32) -> Rectangle<i32, Logical> {
    Rectangle::new((0, 0).into(), (w, h).into())
}

#[test]
fn cells_keep_the_screen_aspect_ratio_so_thumbnails_do_not_letterbox() {
    // The bug this guards: cells stretched to fill the area left every
    // full-screen window sitting in a thick letterbox inside its card.
    let area = Rectangle::new((48, 48).into(), (1824, 984).into());
    for count in 1..=24 {
        for cell in cell_rects(count, area, 16) {
            let cell_aspect = cell.size.w as f64 / cell.size.h as f64;
            let screen_aspect = area.size.w as f64 / area.size.h as f64;
            assert!(
                (cell_aspect - screen_aspect).abs() < 0.02,
                "count={count}: cell {cell:?} is {cell_aspect:.3}, want {screen_aspect:.3}",
            );
        }
    }
}

#[test]
fn the_grid_is_centered_in_the_leftover_vertical_space() {
    let area = Rectangle::new((48, 48).into(), (1824, 984).into());
    let cells = cell_rects(10, area, 16);
    let top = cells.iter().map(|c| c.loc.y).min().unwrap();
    let bottom = cells.iter().map(|c| c.loc.y + c.size.h).max().unwrap();
    let (above, below) = (top - area.loc.y, (area.loc.y + area.size.h) - bottom);
    assert!(
        (above - below).abs() <= 1,
        "slack should be even, give or take a rounded pixel: {above} vs {below}",
    );
}

#[test]
fn column_count_keeps_the_grid_as_square_as_possible() {
    assert_eq!(grid_columns(0), 0);
    assert_eq!(grid_columns(1), 1);
    assert_eq!(grid_columns(2), 2);
    assert_eq!(grid_columns(3), 2);
    assert_eq!(grid_columns(4), 2);
    assert_eq!(grid_columns(6), 3);
    assert_eq!(grid_columns(9), 3);
    // 4 x 3 over 5 x 2: same row count budget, 26% larger cells.
    assert_eq!(grid_columns(10), 4);
    assert_eq!(grid_columns(11), 4);
}

#[test]
fn cells_stay_inside_the_area_and_never_overlap() {
    let area = Rectangle::new((48, 48).into(), (1824, 984).into());
    for count in 1..=24 {
        let cells = cell_rects(count, area, 16);
        assert_eq!(cells.len(), count);
        for (i, cell) in cells.iter().enumerate() {
            assert!(cell.size.w > 0 && cell.size.h > 0, "count={count} i={i}");
            assert!(
                cell.loc.x >= area.loc.x
                    && cell.loc.y >= area.loc.y
                    && cell.loc.x + cell.size.w <= area.loc.x + area.size.w
                    && cell.loc.y + cell.size.h <= area.loc.y + area.size.h,
                "cell {i} of {count} escapes the area: {cell:?}",
            );
            for (j, other) in cells.iter().enumerate().skip(i + 1) {
                assert!(
                    !cell.overlaps(*other),
                    "cells {i} and {j} of {count} overlap",
                );
            }
        }
    }
}

#[test]
fn a_partial_last_row_is_centered() {
    // 7 windows give 3 columns: two full rows of 3 and a centered row of 1.
    let cells = cell_rects(7, screen(1920, 1080), 16);
    assert_eq!(grid_columns(7), 3);
    let first_row_left = cells[0].loc.x;
    let last_row_left = cells[6].loc.x;
    assert!(
        last_row_left > first_row_left,
        "short last row should be indented: {last_row_left} vs {first_row_left}",
    );
    let first_row_right = cells[2].loc.x + cells[2].size.w;
    let last_row_right = cells[6].loc.x + cells[6].size.w;
    assert_eq!(
        last_row_left - first_row_left,
        first_row_right - last_row_right,
        "the last row should be centered",
    );
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
    Rectangle::new((x, y).into(), (w, h).into())
}

#[test]
fn windows_keep_their_workspace_arrangement_inside_the_panel() {
    let screen = rect(0, 0, 1920, 1080);
    let panel = rect(100, 50, 480, 270); // quarter scale
    // Left and right halves with a 20px gap between them.
    let left = place_in_panel(panel, screen, rect(0, 0, 950, 1080), 0);
    let right = place_in_panel(panel, screen, rect(970, 0, 950, 1080), 0);
    assert_eq!(left, rect(100, 50, 238, 270));
    assert_eq!(right.loc.y, left.loc.y);
    assert_eq!(right.size.h, left.size.h);
    assert!(right.loc.x > left.loc.x + left.size.w, "halves stay apart");
    assert_eq!(
        right.loc.x + right.size.w,
        100 + 480,
        "flush with the right edge"
    );

    // The inset pulls every side in by the same amount.
    let inset = place_in_panel(panel, screen, rect(0, 0, 950, 1080), 3);
    assert_eq!(inset, rect(103, 53, 232, 264));

    // A screen that is not at the origin (second monitor) maps the same way.
    let second = rect(1920, 0, 1920, 1080);
    assert_eq!(
        place_in_panel(panel, second, rect(1920, 0, 950, 1080), 0),
        left
    );

    // A window hanging off the screen is clamped to the panel, and a tiny one
    // with a big inset still comes out at least a pixel.
    let off = place_in_panel(panel, screen, rect(1800, 900, 600, 600), 0);
    assert!(off.loc.x + off.size.w <= 580 && off.loc.y + off.size.h <= 320);
    let tiny = place_in_panel(panel, screen, rect(10, 10, 4, 4), 3);
    assert!(tiny.size.w >= 1 && tiny.size.h >= 1);
}

#[test]
fn inset_cards_keep_their_window_proportions() {
    // The bug this guards: a short, wide window (a half-height tile) lost the
    // same inset on both axes, came out proportionally wider than the window,
    // and the window drawn inside left a thick band of outline down each side.
    let screen = rect(0, 0, 1920, 1030);
    let panel = rect(0, 0, 580, 311);
    for window in [
        rect(966, 571, 944, 499),
        rect(10, 60, 944, 1010),
        rect(0, 0, 1900, 200),
    ] {
        let card = place_in_panel(panel, screen, window, 3);
        let scaled_h = card.size.w as f64 * window.size.h as f64 / window.size.w as f64;
        assert!(
            (scaled_h - card.size.h as f64).abs() <= 1.0,
            "{window:?} -> {card:?} is out of proportion",
        );
    }
}

#[test]
fn arrows_move_to_the_nearest_window_in_that_direction() {
    // Two workspaces side by side: [A | B] and [C over D].
    let cards = [
        rect(0, 0, 100, 200),    // 0 A
        rect(110, 0, 100, 200),  // 1 B
        rect(300, 0, 200, 95),   // 2 C
        rect(300, 105, 200, 95), // 3 D
    ];
    assert_eq!(nav_spatial(&cards, 0, OverviewNav::Right), 1);
    assert_eq!(nav_spatial(&cards, 1, OverviewNav::Right), 2);
    assert_eq!(nav_spatial(&cards, 2, OverviewNav::Down), 3);
    assert_eq!(nav_spatial(&cards, 3, OverviewNav::Up), 2);
    assert_eq!(nav_spatial(&cards, 3, OverviewNav::Left), 1);
    // Nothing further that way: stay put.
    assert_eq!(nav_spatial(&cards, 0, OverviewNav::Left), 0);
    assert_eq!(nav_spatial(&cards, 0, OverviewNav::Up), 0);
    assert_eq!(nav_spatial(&cards, 3, OverviewNav::Down), 3);
    // Tab wraps both ways.
    assert_eq!(nav_spatial(&cards, 3, OverviewNav::Next), 0);
    assert_eq!(nav_spatial(&cards, 0, OverviewNav::Prev), 3);
    // Empty and out-of-range input must not panic.
    assert_eq!(nav_spatial(&[], 0, OverviewNav::Next), 0);
    assert!(nav_spatial(&cards, 99, OverviewNav::Right) < cards.len());
}

#[test]
fn a_card_takes_its_window_shape_inside_its_slot() {
    let slot = Rectangle::new((100, 200).into(), (400, 225).into()); // 16:9
    // A window the slot's own shape fills it exactly.
    assert_eq!(fit(slot, 400.0 / 225.0), slot);

    // A half-tiled (tall) window is pillared inside the slot and centred.
    let tall = fit(slot, 0.5);
    assert!(tall.size.w < slot.size.w);
    assert_eq!(tall.size.h, slot.size.h);
    let left = tall.loc.x - slot.loc.x;
    let right = (slot.loc.x + slot.size.w) - (tall.loc.x + tall.size.w);
    assert!(
        (left - right).abs() <= 1,
        "centred give or take a pixel: {left} vs {right}"
    );

    // A wide strip is pinned to the slot's width instead.
    let wide = fit(slot, 6.0);
    assert_eq!(wide.size.w, slot.size.w);
    assert!(wide.size.h < slot.size.h);

    // Every fit stays inside its slot and keeps the asked-for aspect.
    for aspect in [0.2, 0.5, 1.0, 1.78, 3.0, 8.0] {
        let card = fit(slot, aspect);
        assert!(
            card.size.w <= slot.size.w && card.size.h <= slot.size.h,
            "{aspect}"
        );
        let got = card.size.w as f64 / card.size.h as f64;
        assert!((got - aspect).abs() / aspect < 0.02, "{aspect} -> {got}");
    }

    // A degenerate window size must not produce an inverted card.
    assert_eq!(fit(slot, f64::NAN), slot);
    assert_eq!(fit(slot, 0.0), slot);
}
