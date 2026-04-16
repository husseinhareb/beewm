use beewm::layout::Layout;
use beewm::layout::dwindle::Dwindle;
use beewm::layout::master_stack::MasterStack;
use beewm::model::window::Geometry;

#[test]
fn dwindle_empty_windows() {
    let layout = Dwindle::default();
    let screen = Geometry::new(0, 0, 1920, 1080);
    let result = layout.apply(&screen, 0);
    assert!(result.is_empty());
}

#[test]
fn dwindle_single_window_fills_screen() {
    let layout = Dwindle::default();
    let screen = Geometry::new(0, 0, 1920, 1080);
    let result = layout.apply(&screen, 1);
    assert_eq!(result, vec![screen]);
}

#[test]
fn dwindle_two_windows_split_horizontally_first() {
    let layout = Dwindle::default();
    let screen = Geometry::new(0, 0, 1920, 1080);
    let result = layout.apply(&screen, 2);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0], Geometry::new(0, 0, 960, 1080));
    assert_eq!(result[1], Geometry::new(960, 0, 960, 1080));
}

#[test]
fn dwindle_three_windows_dwindle() {
    let layout = Dwindle::default();
    let screen = Geometry::new(0, 0, 1920, 1080);
    let result = layout.apply(&screen, 3);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0], Geometry::new(0, 0, 960, 1080));
    assert_eq!(result[1], Geometry::new(960, 0, 960, 540));
    assert_eq!(result[2], Geometry::new(960, 540, 960, 540));
}

#[test]
fn dwindle_four_windows_alternate_axis() {
    let layout = Dwindle { split_ratio: 0.5 };
    let screen = Geometry::new(0, 0, 100, 80);
    let result = layout.apply(&screen, 4);
    assert_eq!(
        result,
        vec![
            Geometry::new(0, 0, 50, 80),
            Geometry::new(50, 0, 50, 40),
            Geometry::new(50, 40, 25, 40),
            Geometry::new(75, 40, 25, 40),
        ]
    );
}

#[test]
fn master_stack_empty_windows() {
    let layout = MasterStack::default();
    let screen = Geometry::new(0, 0, 1920, 1080);
    let result = layout.apply(&screen, 0);
    assert!(result.is_empty());
}

#[test]
fn master_stack_single_window_fills_screen() {
    let layout = MasterStack::default();
    let screen = Geometry::new(0, 0, 1920, 1080);
    let result = layout.apply(&screen, 1);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], screen);
}

#[test]
fn master_stack_two_windows_split() {
    let layout = MasterStack::default();
    let screen = Geometry::new(0, 0, 1920, 1080);
    let result = layout.apply(&screen, 2);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].width, 960);
    assert_eq!(result[0].height, 1080);
    assert_eq!(result[1].x, 960);
    assert_eq!(result[1].width, 960);
    assert_eq!(result[1].height, 1080);
}

#[test]
fn master_stack_three_windows_stacked() {
    let layout = MasterStack::default();
    let screen = Geometry::new(0, 0, 1920, 1080);
    let result = layout.apply(&screen, 3);
    assert_eq!(result.len(), 3);
    assert_eq!(result[1].height, 540);
    assert_eq!(result[2].height, 540);
    assert_eq!(result[2].y, 540);
}

#[test]
fn master_stack_invalid_ratio_is_clamped() {
    let layout = MasterStack { master_ratio: 2.0 };
    let screen = Geometry::new(0, 0, 1920, 1080);
    let result = layout.apply(&screen, 2);
    assert_eq!(result[0].width, 1920);
    assert_eq!(result[1].width, 0);
}
