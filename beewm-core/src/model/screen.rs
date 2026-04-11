use crate::model::window::Geometry;

/// A physical screen/monitor.
#[derive(Debug, Clone)]
pub struct Screen {
    pub index: usize,
    pub geometry: Geometry,
}

impl Screen {
    pub fn new(index: usize, geometry: Geometry) -> Self {
        Self { index, geometry }
    }
}
