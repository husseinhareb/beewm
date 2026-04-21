use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

use crate::model::window::Geometry;

use super::dwindle_tree::DwindleTree;
use super::master_stack::MasterStack;
use super::Layout;

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
        tiled_count: usize,
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

    /// Access the positional layout (for index-based fallback in relayout).
    /// Returns `None` for tree-based layouts that produce keyed geometries.
    fn positional_layout(&self) -> Option<&dyn Layout> {
        None
    }
}

// ── Dwindle ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct DwindleManager<Id: Clone + Eq + Hash> {
    pub split_ratio: f64,
    trees: Vec<DwindleTree<Id>>,
}

impl<Id: Clone + Eq + Hash> DwindleManager<Id> {
    pub fn new(num_workspaces: usize, split_ratio: f64) -> Self {
        Self {
            split_ratio,
            trees: (0..num_workspaces)
                .map(|_| DwindleTree::default())
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
        _tiled_count: usize,
    ) -> HashMap<Id, Geometry> {
        self.trees[workspace]
            .geometries(screen, self.split_ratio)
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
        tree.geometries(screen, self.split_ratio)
            .into_iter()
            .find_map(|(candidate, geo)| (candidate == id).then_some(geo))
    }
}

// ── MasterStack ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct MasterStackManager {
    layout: MasterStack,
}

impl MasterStackManager {
    pub fn new(master_ratio: f64) -> Self {
        Self {
            layout: MasterStack { master_ratio },
        }
    }
}

impl<Id: Clone + Eq + Hash + Debug> LayoutManager<Id> for MasterStackManager {
    fn insert(&mut self, _workspace: usize, _split_target: Option<&Id>, _id: Id) {
        // MasterStack is purely positional — nothing to track.
    }

    fn remove(&mut self, _workspace: usize, _id: &Id) {}

    fn swap(&mut self, _workspace: usize, _first: &Id, _second: &Id) -> bool {
        // Positional layout: swap is always done at the Vec level by the caller.
        true
    }

    fn geometries(
        &self,
        _workspace: usize,
        _screen: &Geometry,
        _tiled_count: usize,
    ) -> HashMap<Id, Geometry> {
        // MasterStack produces geometries via the positional Layout trait.
        HashMap::new()
    }

    fn preview_insert(
        &self,
        _workspace: usize,
        _split_target: Option<&Id>,
        _id: Id,
        _screen: &Geometry,
    ) -> Option<Geometry> {
        // Positional fallback: the caller uses Layout::apply.
        None
    }

    fn positional_layout(&self) -> Option<&dyn Layout> {
        Some(&self.layout)
    }
}
