use std::collections::HashMap;

use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::config::LayoutKind;
use crate::model::window::Geometry;

use super::{Beewm, root_surface};

#[derive(Debug, Clone, Copy)]
enum SplitAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone)]
enum DwindleNode<T> {
    Leaf(T),
    Split {
        axis: SplitAxis,
        first: Box<DwindleNode<T>>,
        second: Box<DwindleNode<T>>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct DwindleTree<T> {
    root: Option<DwindleNode<T>>,
}

impl<T> Default for DwindleTree<T> {
    fn default() -> Self {
        Self { root: None }
    }
}

impl<T: Clone + Eq> DwindleTree<T> {
    pub(crate) fn insert(&mut self, target: Option<&T>, new_leaf: T) {
        if self.root.is_none() {
            self.root = Some(DwindleNode::Leaf(new_leaf));
            return;
        }

        let split_target = target
            .filter(|target| self.root.as_ref().is_some_and(|root| root.contains(target)))
            .cloned()
            .or_else(|| self.root.as_ref().and_then(DwindleNode::last_leaf).cloned());

        if let (Some(root), Some(split_target)) = (self.root.as_mut(), split_target) {
            root.insert_at(&split_target, new_leaf, 0);
        }
    }

    pub(crate) fn remove(&mut self, target: &T) {
        self.root = self.root.take().and_then(|root| root.remove(target));
    }

    pub(crate) fn geometries(&self, screen: &Geometry, split_ratio: f64) -> Vec<(T, Geometry)> {
        let mut geometries = Vec::new();
        let split_ratio = if split_ratio.is_finite() {
            split_ratio.clamp(0.0, 1.0)
        } else {
            0.5
        };
        if let Some(root) = self.root.as_ref() {
            root.collect_geometries(screen, split_ratio, &mut geometries);
        }
        geometries
    }
}

impl<T: Clone + Eq> DwindleNode<T> {
    fn contains(&self, target: &T) -> bool {
        match self {
            Self::Leaf(window) => window == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }

    fn last_leaf(&self) -> Option<&T> {
        match self {
            Self::Leaf(window) => Some(window),
            Self::Split { second, .. } => second.last_leaf(),
        }
    }

    fn insert_at(&mut self, target: &T, new_leaf: T, depth: usize) -> bool {
        match self {
            Self::Leaf(existing) if existing == target => {
                let existing = existing.clone();
                *self = Self::Split {
                    axis: split_axis_for_depth(depth),
                    first: Box::new(Self::Leaf(existing)),
                    second: Box::new(Self::Leaf(new_leaf)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.insert_at(target, new_leaf.clone(), depth + 1)
                    || second.insert_at(target, new_leaf, depth + 1)
            }
        }
    }

    fn remove(self, target: &T) -> Option<Self> {
        match self {
            Self::Leaf(window) => (window != *target).then_some(Self::Leaf(window)),
            Self::Split {
                axis,
                first,
                second,
            } => {
                let first = first.remove(target);
                let second = second.remove(target);
                match (first, second) {
                    (Some(first), Some(second)) => Some(Self::Split {
                        axis,
                        first: Box::new(first),
                        second: Box::new(second),
                    }),
                    (Some(first), None) => Some(first),
                    (None, Some(second)) => Some(second),
                    (None, None) => None,
                }
            }
        }
    }

    fn collect_geometries(
        &self,
        screen: &Geometry,
        split_ratio: f64,
        geometries: &mut Vec<(T, Geometry)>,
    ) {
        match self {
            Self::Leaf(window) => geometries.push((window.clone(), *screen)),
            Self::Split {
                axis,
                first,
                second,
            } => {
                let (first_geo, second_geo) = split_geometry(screen, *axis, split_ratio);
                first.collect_geometries(&first_geo, split_ratio, geometries);
                second.collect_geometries(&second_geo, split_ratio, geometries);
            }
        }
    }
}

fn split_axis_for_depth(depth: usize) -> SplitAxis {
    if depth % 2 == 0 {
        SplitAxis::Vertical
    } else {
        SplitAxis::Horizontal
    }
}

fn split_geometry(screen: &Geometry, axis: SplitAxis, split_ratio: f64) -> (Geometry, Geometry) {
    match axis {
        SplitAxis::Vertical => {
            let first_width = (screen.width as f64 * split_ratio) as u32;
            let second_width = screen.width.saturating_sub(first_width);
            (
                Geometry::new(screen.x, screen.y, first_width, screen.height),
                Geometry::new(
                    screen.x + first_width as i32,
                    screen.y,
                    second_width,
                    screen.height,
                ),
            )
        }
        SplitAxis::Horizontal => {
            let first_height = (screen.height as f64 * split_ratio) as u32;
            let second_height = screen.height.saturating_sub(first_height);
            (
                Geometry::new(screen.x, screen.y, screen.width, first_height),
                Geometry::new(
                    screen.x,
                    screen.y + first_height as i32,
                    screen.width,
                    second_height,
                ),
            )
        }
    }
}

impl Beewm {
    pub(crate) fn window_root_surface(window: &Window) -> Option<WlSurface> {
        window
            .toplevel()
            .map(|toplevel| root_surface(toplevel.wl_surface()))
    }

    pub(crate) fn is_root_floating(&self, root: &WlSurface) -> bool {
        self.floating_windows.contains_key(root)
    }

    pub(crate) fn is_root_fullscreen(&self, root: &WlSurface) -> bool {
        self.fullscreen_window
            .as_ref()
            .and_then(Self::window_root_surface)
            .map(|fullscreen_root| fullscreen_root == *root)
            .unwrap_or(false)
    }

    pub(crate) fn focused_tiled_window_root(&self, workspace_idx: usize) -> Option<WlSurface> {
        let keyboard_focus = (workspace_idx == self.active_workspace)
            .then(|| {
                self.seat
                    .get_keyboard()
                    .and_then(|keyboard| keyboard.current_focus())
            })
            .flatten()
            .and_then(|surface| {
                self.window_index_for_surface(workspace_idx, &surface)
                    .and_then(|idx| self.workspace_windows[workspace_idx].get(idx))
                    .and_then(Self::window_root_surface)
            });

        keyboard_focus
            .or_else(|| {
                self.workspaces[workspace_idx]
                    .focused_idx
                    .and_then(|idx| self.workspace_windows[workspace_idx].get(idx))
                    .and_then(Self::window_root_surface)
            })
            .filter(|root| !self.is_root_floating(root) && !self.is_root_fullscreen(root))
    }

    pub(crate) fn insert_tiled_window(
        &mut self,
        workspace_idx: usize,
        window: &Window,
        split_target: Option<&WlSurface>,
    ) {
        if self.config.layout != LayoutKind::Dwindle {
            return;
        }

        let Some(root) = Self::window_root_surface(window) else {
            return;
        };

        if self.is_root_floating(&root) || self.is_root_fullscreen(&root) {
            return;
        }

        self.dwindle_trees[workspace_idx].insert(split_target, root);
    }

    pub(crate) fn remove_tiled_window(&mut self, workspace_idx: usize, surface: &WlSurface) {
        if self.config.layout != LayoutKind::Dwindle {
            return;
        }

        self.dwindle_trees[workspace_idx].remove(&root_surface(surface));
    }

    pub(crate) fn dwindle_geometries(
        &self,
        workspace_idx: usize,
        screen: &Geometry,
    ) -> HashMap<WlSurface, Geometry> {
        if self.config.layout != LayoutKind::Dwindle {
            return HashMap::new();
        }

        self.dwindle_trees[workspace_idx]
            .geometries(screen, self.config.split_ratio)
            .into_iter()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::model::window::Geometry;

    use super::DwindleTree;

    fn geometry_map(entries: Vec<(u8, Geometry)>) -> HashMap<u8, Geometry> {
        entries.into_iter().collect()
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
}
