pub mod backend;
pub(crate) mod commands;
mod cursor;
mod feedback;
mod handlers;
mod input;
mod ipc;
mod layering;
mod render;
mod screencopy;
pub(crate) mod state;
pub mod types;

pub use backend::{run_udev, run_winit};
pub use input::{resize_edges_for_pointer, resized_window_geometry_from_start};
pub use layering::{
    layers_hit_tested_after_windows, layers_hit_tested_before_windows,
    layers_rendered_above_windows, layers_rendered_below_windows,
};
pub use state::{
    FloatToggleTransition, active_workspace_state_contents, constrain_popup_geometry,
    expand_by_border, float_toggle_transition, is_fixed_size, popup_constraint_target,
    root_is_swap_highlighted, visible_border_rectangles, window_border_overlaps_layer,
    workspace_state_contents, write_state_file_atomically,
};
pub use types::{
    ActiveGrab, FloatingWindowData, MoveGrab, ResizeEdges, ResizeGrab, ResizeHorizontalEdge,
    ResizeVerticalEdge, ResolvedKeybind, TiledResizeGrab, TiledSwapGrab,
};
