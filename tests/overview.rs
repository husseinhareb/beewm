use beewm::compositor::overview::{OverviewNav, cell_rects, fit, grid_columns, nav_target};
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

#[test]
fn tab_wraps_around_the_grid_and_arrows_stop_at_the_edges() {
    // 10 cells, 5 columns.
    assert_eq!(nav_target(9, 10, 5, OverviewNav::Next), 0);
    assert_eq!(nav_target(0, 10, 5, OverviewNav::Prev), 9);
    assert_eq!(nav_target(0, 10, 5, OverviewNav::Left), 0);
    assert_eq!(nav_target(1, 10, 5, OverviewNav::Left), 0);
    assert_eq!(nav_target(4, 10, 5, OverviewNav::Right), 4);
    assert_eq!(nav_target(3, 10, 5, OverviewNav::Right), 4);
    assert_eq!(nav_target(2, 10, 5, OverviewNav::Up), 2);
    assert_eq!(nav_target(7, 10, 5, OverviewNav::Up), 2);
    assert_eq!(nav_target(2, 10, 5, OverviewNav::Down), 7);
    assert_eq!(nav_target(7, 10, 5, OverviewNav::Down), 7);
}

#[test]
fn navigation_stays_in_range_on_a_partial_last_row() {
    // 7 cells, 3 columns: the last row holds 6 only.
    for nav in [
        OverviewNav::Next,
        OverviewNav::Prev,
        OverviewNav::Left,
        OverviewNav::Right,
        OverviewNav::Up,
        OverviewNav::Down,
    ] {
        for selected in 0..7 {
            assert!(nav_target(selected, 7, 3, nav) < 7);
        }
    }
    // Down from the last column of the middle row has nowhere to go.
    assert_eq!(nav_target(5, 7, 3, OverviewNav::Down), 5);
    assert_eq!(nav_target(3, 7, 3, OverviewNav::Down), 6);
    // Empty grid must not panic or index out of bounds.
    assert_eq!(nav_target(0, 0, 0, OverviewNav::Next), 0);
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
