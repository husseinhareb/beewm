/// An i3-style numbered workspace tracking window indices.
#[derive(Debug, Default)]
pub struct Workspace<W = ()> {
    pub windows: Vec<W>,
    pub focused_idx: Option<usize>,
}

impl<W> Workspace<W> {
    pub fn new() -> Self {
        Workspace {
            windows: Vec::new(),
            focused_idx: None,
        }
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    pub fn add_window(&mut self, window: W) {
        self.focused_idx = Some(self.windows.len());
        self.windows.push(window);
    }

    pub fn remove_window(&mut self, idx: usize) -> Option<W> {
        if self.windows.is_empty() || idx >= self.windows.len() {
            return None;
        }
        let removed = self.windows.remove(idx);
        if self.windows.is_empty() {
            self.focused_idx = None;
        } else if let Some(focused) = self.focused_idx {
            if focused == idx {
                self.focused_idx = Some(self.windows.len().saturating_sub(1));
            } else if focused > idx {
                self.focused_idx = Some(focused - 1);
            }
        }
        Some(removed)
    }

    pub fn swap_windows(&mut self, first_idx: usize, second_idx: usize) {
        if self.windows.is_empty()
            || first_idx >= self.windows.len()
            || second_idx >= self.windows.len()
            || first_idx == second_idx
        {
            return;
        }

        self.windows.swap(first_idx, second_idx);

        if let Some(focused_idx) = self.focused_idx {
            self.focused_idx = Some(if focused_idx == first_idx {
                second_idx
            } else if focused_idx == second_idx {
                first_idx
            } else {
                focused_idx
            });
        }
    }

    pub fn focus_next(&mut self) {
        if self.windows.is_empty() {
            return;
        }
        let current = self.focused_idx.unwrap_or(0);
        self.focused_idx = Some((current + 1) % self.windows.len());
    }

    pub fn focus_prev(&mut self) {
        if self.windows.is_empty() {
            return;
        }
        let current = self.focused_idx.unwrap_or(0);
        self.focused_idx = Some(if current == 0 {
            self.windows.len() - 1
        } else {
            current - 1
        });
    }
}
