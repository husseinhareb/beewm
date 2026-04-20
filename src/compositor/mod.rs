pub mod backend;
mod commands;
mod cursor;
mod feedback;
mod handlers;
mod input;
mod ipc;
mod layering;
mod render;
mod state;

pub use backend::{run_udev, run_winit};
pub use handlers::{constrain_popup_geometry, is_fixed_size, popup_constraint_target};
pub use input::{resize_edges_for_pointer, resized_window_geometry_from_start};
pub use layering::{
    layers_hit_tested_after_windows, layers_hit_tested_before_windows,
    layers_rendered_above_windows, layers_rendered_below_windows,
};
pub use state::{
    DwindleTree, FloatToggleTransition, ResizeEdges, ResizeHorizontalEdge, ResizeVerticalEdge,
    active_workspace_state_contents, expand_by_border, float_toggle_transition,
    root_is_swap_highlighted, visible_border_rectangles, window_border_overlaps_layer,
    workspace_state_contents, write_state_file_atomically,
};
