use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

use crate::compositor::types::{ResizeEdges, ResizeHorizontalEdge, ResizeVerticalEdge};
use crate::model::window::Geometry;

use super::Layout;
use super::dwindle_tree::{DwindleTree, ResizeEdge};
use super::master_stack::MasterStack;

/// A per-workspace layout manager that owns both the placement algorithm and
/// any persistent structure (e.g. the dwindle split tree).
///
/// This unifies the two code paths that previously diverged on `LayoutKind`:
/// callers interact with one interface regardless of the active layout.
pub trait LayoutManager<Id: Clone + Eq + Hash>: Debug {
    /// Notify the manager that a new window was mapped.
    /// `split_target` is the surface that should be split to make room (may be `None`).
    fn insert(&mut self, workspace: usize, split_target: Option<&Id>, id: Id);

    /// Notify the manager that a window was unmapped.
    fn remove(&mut self, workspace: usize, id: &Id);

    /// Swap two windows in the layout structure.  Returns `false` when the
    /// swap is not possible (e.g. one of the ids is unknown).
    fn swap(&mut self, workspace: usize, first: &Id, second: &Id) -> bool;

    /// Compute geometries for every tiled window on `workspace`.
    /// The returned map is keyed by window id so the caller can look up each
    /// window's position in O(1).
    fn geometries(
        &self,
        workspace: usize,
        screen: &Geometry,
        tiled_ids: &[Id],
    ) -> HashMap<Id, Geometry>;

    /// Compute the expected geometry for a window that is about to be inserted
    /// (before it actually appears in the tree).  Used to send an initial
    /// configure with the right size.
    fn preview_insert(
        &self,
        workspace: usize,
        split_target: Option<&Id>,
        id: Id,
        screen: &Geometry,
    ) -> Option<Geometry>;

    /// Adjust the layout in response to an interactive tiled resize.
    fn resize(
        &mut self,
        workspace: usize,
        screen: &Geometry,
        tiled_ids: &[Id],
        target: &Id,
        edges: ResizeEdges,
        delta: (i32, i32),
    ) -> bool;

    /// Access the positional layout (for index-based fallback in relayout).
    /// Returns `None` for tree-based layouts that produce keyed geometries.
    fn positional_layout(&self) -> Option<&dyn Layout> {
        None
    }
}

// ── Dwindle ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct DwindleManager<Id: Clone + Eq + Hash> {
    trees: Vec<DwindleTree<Id>>,
}

impl<Id: Clone + Eq + Hash> DwindleManager<Id> {
    pub fn new(num_workspaces: usize, split_ratio: f64) -> Self {
        Self {
            trees: (0..num_workspaces)
                .map(|_| DwindleTree::with_split_ratio(split_ratio))
                .collect(),
        }
    }
}

impl<Id: Clone + Eq + Hash + Debug> LayoutManager<Id> for DwindleManager<Id> {
    fn insert(&mut self, workspace: usize, split_target: Option<&Id>, id: Id) {
        self.trees[workspace].insert(split_target, id);
    }

    fn remove(&mut self, workspace: usize, id: &Id) {
        self.trees[workspace].remove(id);
    }

    fn swap(&mut self, workspace: usize, first: &Id, second: &Id) -> bool {
        self.trees[workspace].swap(first, second)
    }

    fn geometries(
        &self,
        workspace: usize,
        screen: &Geometry,
        _tiled_ids: &[Id],
    ) -> HashMap<Id, Geometry> {
        self.trees[workspace]
            .geometries(screen)
            .into_iter()
            .collect()
    }

    fn preview_insert(
        &self,
        workspace: usize,
        split_target: Option<&Id>,
        id: Id,
        screen: &Geometry,
    ) -> Option<Geometry> {
        let mut tree = self.trees[workspace].clone();
        tree.insert(split_target, id.clone());
        tree.geometries(screen)
            .into_iter()
            .find_map(|(candidate, geo)| (candidate == id).then_some(geo))
    }

    fn resize(
        &mut self,
        workspace: usize,
        screen: &Geometry,
        _tiled_ids: &[Id],
        target: &Id,
        edges: ResizeEdges,
        delta: (i32, i32),
    ) -> bool {
        let mut handled = false;

        if delta.0 != 0 {
            handled |= self.trees[workspace].resize(
                target,
                match edges.horizontal {
                    ResizeHorizontalEdge::Left => ResizeEdge::Left,
                    ResizeHorizontalEdge::Right => ResizeEdge::Right,
                },
                delta.0,
                screen,
                1,
            );
        }

        if delta.1 != 0 {
            handled |= self.trees[workspace].resize(
                target,
                match edges.vertical {
                    ResizeVerticalEdge::Top => ResizeEdge::Top,
                    ResizeVerticalEdge::Bottom => ResizeEdge::Bottom,
                },
                delta.1,
                screen,
                1,
            );
        }

        handled
    }
}

// ── MasterStack ──────────────────────────────────────────────────────────────

#[derive(Debug)]
struct MasterStackWorkspace<Id> {
    order: Vec<Id>,
    master_ratio: f64,
    stack_weights: Vec<f64>,
}

#[derive(Debug)]
pub struct MasterStackManager<Id: Clone + Eq + Hash> {
    workspaces: Vec<MasterStackWorkspace<Id>>,
}

impl<Id: Clone + Eq + Hash> MasterStackManager<Id> {
    pub fn new(num_workspaces: usize, master_ratio: f64) -> Self {
        Self {
            workspaces: (0..num_workspaces)
                .map(|_| MasterStackWorkspace {
                    order: Vec::new(),
                    master_ratio: sanitize_ratio(master_ratio),
                    stack_weights: Vec::new(),
                })
                .collect(),
        }
    }
}

impl<Id: Clone + Eq + Hash + Debug> LayoutManager<Id> for MasterStackManager<Id> {
    fn insert(&mut self, workspace: usize, _split_target: Option<&Id>, id: Id) {
        let state = &mut self.workspaces[workspace];
        state.order.push(id);
        ensure_stack_weights_len(state);
    }

    fn remove(&mut self, workspace: usize, id: &Id) {
        let state = &mut self.workspaces[workspace];
        let Some(idx) = state.order.iter().position(|candidate| candidate == id) else {
            return;
        };

        state.order.remove(idx);
        if !state.stack_weights.is_empty() {
            let weight_idx = idx.saturating_sub(1);
            if weight_idx < state.stack_weights.len() {
                state.stack_weights.remove(weight_idx);
            }
        }
    }

    fn swap(&mut self, workspace: usize, first: &Id, second: &Id) -> bool {
        let state = &mut self.workspaces[workspace];
        let Some(first_idx) = state.order.iter().position(|candidate| candidate == first) else {
            return false;
        };
        let Some(second_idx) = state.order.iter().position(|candidate| candidate == second) else {
            return false;
        };
        state.order.swap(first_idx, second_idx);
        true
    }

    fn geometries(
        &self,
        workspace: usize,
        screen: &Geometry,
        tiled_ids: &[Id],
    ) -> HashMap<Id, Geometry> {
        let state = &self.workspaces[workspace];
        master_stack_geometry_map(screen, tiled_ids, state.master_ratio, &state.stack_weights)
    }

    fn preview_insert(
        &self,
        workspace: usize,
        _split_target: Option<&Id>,
        _id: Id,
        screen: &Geometry,
    ) -> Option<Geometry> {
        let state = &self.workspaces[workspace];
        let mut order = state.order.clone();
        order.push(_id);
        master_stack_ordered_geometries(screen, &order, state.master_ratio, &state.stack_weights)
            .into_iter()
            .last()
    }

    fn resize(
        &mut self,
        workspace: usize,
        screen: &Geometry,
        tiled_ids: &[Id],
        target: &Id,
        edges: ResizeEdges,
        delta: (i32, i32),
    ) -> bool {
        let state = &mut self.workspaces[workspace];
        let Some(target_idx) = tiled_ids.iter().position(|candidate| candidate == target) else {
            return false;
        };

        let current_geometries = master_stack_ordered_geometries(
            screen,
            tiled_ids,
            state.master_ratio,
            &state.stack_weights,
        );

        let mut handled = false;

        if delta.0 != 0 && tiled_ids.len() > 1 {
            let master_geo = current_geometries[0];
            let adjusts_master_split = matches!(
                (target_idx, edges.horizontal),
                (0, ResizeHorizontalEdge::Right) | (1.., ResizeHorizontalEdge::Left)
            );

            if adjusts_master_split && screen.width > 1 {
                let new_master_width =
                    (master_geo.width as i32 + delta.0).clamp(1, screen.width as i32 - 1);
                state.master_ratio = sanitize_ratio(new_master_width as f64 / screen.width as f64);
                handled = true;
            }
        }

        if delta.1 != 0 && tiled_ids.len() > 2 && target_idx > 0 {
            let stack_idx = target_idx - 1;
            let pair = match edges.vertical {
                ResizeVerticalEdge::Top if stack_idx > 0 => Some((stack_idx - 1, stack_idx)),
                ResizeVerticalEdge::Bottom if stack_idx + 1 < tiled_ids.len() - 1 => {
                    Some((stack_idx, stack_idx + 1))
                }
                _ => None,
            };

            if let Some((first_idx, second_idx)) = pair {
                let mut new_weights: Vec<f64> = current_geometries
                    .iter()
                    .skip(1)
                    .map(|geometry| geometry.height.max(1) as f64)
                    .collect();
                let first_height = current_geometries[first_idx + 1].height as i32;
                let second_height = current_geometries[second_idx + 1].height as i32;
                if first_height + second_height <= 1 {
                    return handled;
                }
                let new_first_height =
                    (first_height + delta.1).clamp(1, first_height + second_height - 1);

                new_weights[first_idx] = new_first_height as f64;
                new_weights[second_idx] = (first_height + second_height - new_first_height) as f64;
                state.stack_weights = new_weights;
                handled = true;
            }
        }

        handled
    }
}

fn sanitize_ratio(ratio: f64) -> f64 {
    if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        MasterStack::default().master_ratio
    }
}

fn ensure_stack_weights_len<Id>(state: &mut MasterStackWorkspace<Id>) {
    let stack_count = state.order.len().saturating_sub(1);
    if state.stack_weights.len() < stack_count {
        state.stack_weights.extend(std::iter::repeat_n(
            1.0,
            stack_count - state.stack_weights.len(),
        ));
    } else if state.stack_weights.len() > stack_count {
        state.stack_weights.truncate(stack_count);
    }
}

fn normalized_stack_weights(weights: &[f64], stack_count: usize) -> Vec<f64> {
    if stack_count == 0 {
        return Vec::new();
    }

    let mut normalized = if weights.len() == stack_count {
        weights
            .iter()
            .map(|weight| {
                if weight.is_finite() && *weight > 0.0 {
                    *weight
                } else {
                    1.0
                }
            })
            .collect::<Vec<_>>()
    } else {
        vec![1.0; stack_count]
    };

    if normalized.iter().all(|weight| *weight == 0.0) {
        normalized.fill(1.0);
    }

    normalized
}

fn master_stack_geometry_map<Id: Clone + Eq + Hash>(
    screen: &Geometry,
    tiled_ids: &[Id],
    master_ratio: f64,
    stack_weights: &[f64],
) -> HashMap<Id, Geometry> {
    tiled_ids
        .iter()
        .cloned()
        .zip(master_stack_ordered_geometries(
            screen,
            tiled_ids,
            master_ratio,
            stack_weights,
        ))
        .collect()
}

fn master_stack_ordered_geometries<Id>(
    screen: &Geometry,
    tiled_ids: &[Id],
    master_ratio: f64,
    stack_weights: &[f64],
) -> Vec<Geometry> {
    if tiled_ids.is_empty() {
        return Vec::new();
    }

    if tiled_ids.len() == 1 {
        return vec![*screen];
    }

    let master_ratio = sanitize_ratio(master_ratio);
    let master_width = (screen.width as f64 * master_ratio) as u32;
    let stack_width = screen.width.saturating_sub(master_width);
    let stack_count = tiled_ids.len() - 1;
    let weights = normalized_stack_weights(stack_weights, stack_count);
    let total_weight: f64 = weights.iter().sum();

    let mut geometries = Vec::with_capacity(tiled_ids.len());
    geometries.push(Geometry::new(
        screen.x,
        screen.y,
        master_width,
        screen.height,
    ));

    let mut consumed_height = 0u32;
    for (index, weight) in weights.iter().enumerate() {
        let height = if index + 1 == stack_count {
            screen.height.saturating_sub(consumed_height)
        } else {
            ((screen.height as f64) * (*weight / total_weight)) as u32
        };
        let y = screen.y + consumed_height as i32;
        geometries.push(Geometry::new(
            screen.x + master_width as i32,
            y,
            stack_width,
            height,
        ));
        consumed_height = consumed_height.saturating_add(height);
    }

    geometries
}
