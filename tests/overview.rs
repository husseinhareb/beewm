use beewm::compositor::overview::{OverviewNav, cell_rects, grid_columns, nav_target};
use smithay::utils::{Logical, Rectangle};

fn screen(w: i32, h: i32) -> Rectangle<i32, Logical> {
    Rectangle::new((0, 0).into(), (w, h).into())
}

#[test]
fn ten_windows_on_a_widescreen_are_five_columns_by_two_rows() {
    // The shape asked for: 10 windows fill the screen as 2 rows of 5.
    assert_eq!(grid_columns(10, 1920.0 / 1080.0), 5);
    let cells = cell_rects(10, screen(1920, 1080), 16);
    assert_eq!(cells.len(), 10);
    let rows: Vec<i32> = {
        let mut ys: Vec<i32> = cells.iter().map(|cell| cell.loc.y).collect();
        ys.dedup();
        ys
    };
    assert_eq!(rows.len(), 2);
}

#[test]
fn column_count_tracks_the_window_count_and_screen_shape() {
    let wide = 1920.0 / 1080.0;
    assert_eq!(grid_columns(0, wide), 0);
    assert_eq!(grid_columns(1, wide), 1);
    assert_eq!(grid_columns(2, wide), 2);
    assert_eq!(grid_columns(3, wide), 3);
    assert_eq!(grid_columns(9, wide), 4);
    // A portrait screen stacks instead of spreading.
    assert_eq!(grid_columns(4, 1080.0 / 1920.0), 2);
    // A degenerate aspect ratio must not produce a zero-column grid.
    assert_eq!(grid_columns(4, f64::NAN), 2);
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
    // 7 windows on 16:9 give 4 columns: a full row of 4 and a centered row of 3.
    let cells = cell_rects(7, screen(1920, 1080), 16);
    assert_eq!(grid_columns(7, 1920.0 / 1080.0), 4);
    let first_row_left = cells[0].loc.x;
    let last_row_left = cells[4].loc.x;
    assert!(
        last_row_left > first_row_left,
        "short last row should be indented: {last_row_left} vs {first_row_left}",
    );
    let first_row_right = cells[3].loc.x + cells[3].size.w;
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
    // 7 cells, 4 columns: the last row holds 4, 5, 6 only.
    for nav in [
        OverviewNav::Next,
        OverviewNav::Prev,
        OverviewNav::Left,
        OverviewNav::Right,
        OverviewNav::Up,
        OverviewNav::Down,
    ] {
        for selected in 0..7 {
            assert!(nav_target(selected, 7, 4, nav) < 7);
        }
    }
    // Down from the last column of the first row has nowhere to go.
    assert_eq!(nav_target(3, 7, 4, OverviewNav::Down), 3);
    assert_eq!(nav_target(2, 7, 4, OverviewNav::Down), 6);
    // Empty grid must not panic or index out of bounds.
    assert_eq!(nav_target(0, 0, 0, OverviewNav::Next), 0);
}
