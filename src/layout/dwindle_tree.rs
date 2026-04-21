use crate::model::window::Geometry;

#[derive(Debug, Clone, Copy)]
enum SplitAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeEdge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone)]
enum DwindleNode<T> {
    Leaf(T),
    Split {
        axis: SplitAxis,
        ratio: f64,
        first: Box<DwindleNode<T>>,
        second: Box<DwindleNode<T>>,
    },
}

#[derive(Debug, Clone)]
pub struct DwindleTree<T> {
    root: Option<DwindleNode<T>>,
    default_split_ratio: f64,
}

impl<T> Default for DwindleTree<T> {
    fn default() -> Self {
        Self {
            root: None,
            default_split_ratio: 0.5,
        }
    }
}

impl<T: Clone + Eq> DwindleTree<T> {
    pub fn with_split_ratio(split_ratio: f64) -> Self {
        Self {
            root: None,
            default_split_ratio: sanitize_split_ratio(split_ratio),
        }
    }

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
            root.insert_at(&split_target, new_leaf, 0, self.default_split_ratio);
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

    pub fn geometries(&self, screen: &Geometry) -> Vec<(T, Geometry)> {
        let mut geometries = Vec::new();
        if let Some(root) = self.root.as_ref() {
            root.collect_geometries(screen, &mut geometries);
        }
        geometries
    }

    pub fn resize(
        &mut self,
        target: &T,
        edge: ResizeEdge,
        delta: i32,
        screen: &Geometry,
        min_leaf_span: u32,
    ) -> bool {
        let Some(root) = self.root.as_mut() else {
            return false;
        };

        matches!(
            root.resize(target, edge, delta, screen, min_leaf_span.max(1)),
            ResizeSearch::Handled
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeSearch {
    NotFound,
    Found,
    Handled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildBranch {
    First,
    Second,
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

    fn insert_at(&mut self, target: &T, new_leaf: T, depth: usize, default_ratio: f64) -> bool {
        match self {
            Self::Leaf(existing) if existing == target => {
                let existing = existing.clone();
                *self = Self::Split {
                    axis: split_axis_for_depth(depth),
                    ratio: default_ratio,
                    first: Box::new(Self::Leaf(existing)),
                    second: Box::new(Self::Leaf(new_leaf)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.insert_at(target, new_leaf.clone(), depth + 1, default_ratio)
                    || second.insert_at(target, new_leaf, depth + 1, default_ratio)
            }
        }
    }

    fn remove(self, target: &T) -> Option<Self> {
        match self {
            Self::Leaf(window) => (window != *target).then_some(Self::Leaf(window)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let first = first.remove(target);
                let second = second.remove(target);
                match (first, second) {
                    (Some(first), Some(second)) => Some(Self::Split {
                        axis,
                        ratio,
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

    fn collect_geometries(&self, screen: &Geometry, geometries: &mut Vec<(T, Geometry)>) {
        match self {
            Self::Leaf(window) => geometries.push((window.clone(), *screen)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (first_geo, second_geo) = split_geometry(screen, *axis, *ratio);
                first.collect_geometries(&first_geo, geometries);
                second.collect_geometries(&second_geo, geometries);
            }
        }
    }

    fn resize(
        &mut self,
        target: &T,
        edge: ResizeEdge,
        delta: i32,
        container: &Geometry,
        min_leaf_span: u32,
    ) -> ResizeSearch {
        match self {
            Self::Leaf(window) => {
                if window == target {
                    ResizeSearch::Found
                } else {
                    ResizeSearch::NotFound
                }
            }
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (first_geo, second_geo) = split_geometry(container, *axis, *ratio);

                match first.resize(target, edge, delta, &first_geo, min_leaf_span) {
                    ResizeSearch::Handled => return ResizeSearch::Handled,
                    ResizeSearch::Found => {
                        if edge_matches_branch(edge, *axis, ChildBranch::First) {
                            adjust_split_ratio(
                                axis,
                                ratio,
                                first,
                                second,
                                &first_geo,
                                &second_geo,
                                delta,
                                min_leaf_span,
                            );
                            return ResizeSearch::Handled;
                        }
                        return ResizeSearch::Found;
                    }
                    ResizeSearch::NotFound => {}
                }

                match second.resize(target, edge, delta, &second_geo, min_leaf_span) {
                    ResizeSearch::Handled => ResizeSearch::Handled,
                    ResizeSearch::Found => {
                        if edge_matches_branch(edge, *axis, ChildBranch::Second) {
                            adjust_split_ratio(
                                axis,
                                ratio,
                                first,
                                second,
                                &first_geo,
                                &second_geo,
                                delta,
                                min_leaf_span,
                            );
                            ResizeSearch::Handled
                        } else {
                            ResizeSearch::Found
                        }
                    }
                    ResizeSearch::NotFound => ResizeSearch::NotFound,
                }
            }
        }
    }

    fn min_width(&self, min_leaf_span: u32) -> u32 {
        match self {
            Self::Leaf(_) => min_leaf_span,
            Self::Split {
                axis: SplitAxis::Vertical,
                first,
                second,
                ..
            } => first
                .min_width(min_leaf_span)
                .saturating_add(second.min_width(min_leaf_span)),
            Self::Split {
                axis: SplitAxis::Horizontal,
                first,
                second,
                ..
            } => first
                .min_width(min_leaf_span)
                .max(second.min_width(min_leaf_span)),
        }
    }

    fn min_height(&self, min_leaf_span: u32) -> u32 {
        match self {
            Self::Leaf(_) => min_leaf_span,
            Self::Split {
                axis: SplitAxis::Horizontal,
                first,
                second,
                ..
            } => first
                .min_height(min_leaf_span)
                .saturating_add(second.min_height(min_leaf_span)),
            Self::Split {
                axis: SplitAxis::Vertical,
                first,
                second,
                ..
            } => first
                .min_height(min_leaf_span)
                .max(second.min_height(min_leaf_span)),
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
    let split_ratio = sanitize_split_ratio(split_ratio);
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

fn sanitize_split_ratio(split_ratio: f64) -> f64 {
    if split_ratio.is_finite() {
        split_ratio.clamp(0.0, 1.0)
    } else {
        0.5
    }
}

fn edge_matches_branch(edge: ResizeEdge, axis: SplitAxis, branch: ChildBranch) -> bool {
    matches!(
        (edge, axis, branch),
        (ResizeEdge::Right, SplitAxis::Vertical, ChildBranch::First)
            | (ResizeEdge::Left, SplitAxis::Vertical, ChildBranch::Second)
            | (
                ResizeEdge::Bottom,
                SplitAxis::Horizontal,
                ChildBranch::First
            )
            | (ResizeEdge::Top, SplitAxis::Horizontal, ChildBranch::Second)
    )
}

fn adjust_split_ratio<T: Clone + Eq>(
    axis: &SplitAxis,
    ratio: &mut f64,
    first: &DwindleNode<T>,
    second: &DwindleNode<T>,
    first_geo: &Geometry,
    second_geo: &Geometry,
    delta: i32,
    min_leaf_span: u32,
) {
    let (current_first_span, total_span, min_first, min_second) = match axis {
        SplitAxis::Vertical => (
            first_geo.width,
            first_geo.width.saturating_add(second_geo.width),
            first.min_width(min_leaf_span),
            second.min_width(min_leaf_span),
        ),
        SplitAxis::Horizontal => (
            first_geo.height,
            first_geo.height.saturating_add(second_geo.height),
            first.min_height(min_leaf_span),
            second.min_height(min_leaf_span),
        ),
    };

    if total_span == 0 {
        return;
    }

    if min_first.saturating_add(min_second) > total_span {
        return;
    }

    let lower = min_first as i32;
    let upper = total_span as i32 - min_second as i32;
    if upper < lower {
        return;
    }

    let new_first_span = (current_first_span as i32 + delta).clamp(lower, upper);
    *ratio = sanitize_split_ratio(new_first_span as f64 / total_span as f64);
}
