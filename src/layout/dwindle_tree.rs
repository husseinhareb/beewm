use crate::model::window::Geometry;

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
pub struct DwindleTree<T> {
    root: Option<DwindleNode<T>>,
}

impl<T> Default for DwindleTree<T> {
    fn default() -> Self {
        Self { root: None }
    }
}

impl<T: Clone + Eq> DwindleTree<T> {
    pub fn insert(&mut self, target: Option<&T>, new_leaf: T) {
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

    pub fn remove(&mut self, target: &T) {
        self.root = self.root.take().and_then(|root| root.remove(target));
    }

    pub fn swap(&mut self, first: &T, second: &T) -> bool {
        if first == second {
            return false;
        }

        let Some(root) = self.root.as_mut() else {
            return false;
        };

        if !root.contains(first) || !root.contains(second) {
            return false;
        }

        root.swap(first, second);
        true
    }

    pub fn geometries(&self, screen: &Geometry, split_ratio: f64) -> Vec<(T, Geometry)> {
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

    fn swap(&mut self, first: &T, second: &T) {
        match self {
            Self::Leaf(window) if *window == *first => *window = second.clone(),
            Self::Leaf(window) if *window == *second => *window = first.clone(),
            Self::Leaf(_) => {}
            Self::Split {
                first: a,
                second: b,
                ..
            } => {
                a.swap(first, second);
                b.swap(first, second);
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
