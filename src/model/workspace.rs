/// An i3-style numbered workspace tracking window indices.
#[derive(Debug, Default)]
pub struct Workspace {
    pub window_count: usize,
    pub focused_idx: Option<usize>,
}

impl Workspace {
    pub fn add_window(&mut self) {
        self.focused_idx = Some(self.window_count);
        self.window_count += 1;
    }

    pub fn remove_window(&mut self, idx: usize) {
        if self.window_count == 0 {
            return;
        }
        self.window_count -= 1;
        if self.window_count == 0 {
            self.focused_idx = None;
        } else if let Some(focused) = self.focused_idx {
            if focused == idx {
                self.focused_idx = Some(self.window_count.saturating_sub(1));
            } else if focused > idx {
                self.focused_idx = Some(focused - 1);
            }
        }
    }

    pub fn focus_next(&mut self) {
        if self.window_count == 0 {
            return;
        }
        let current = self.focused_idx.unwrap_or(0);
        self.focused_idx = Some((current + 1) % self.window_count);
    }

    pub fn focus_prev(&mut self) {
        if self.window_count == 0 {
            return;
        }
        let current = self.focused_idx.unwrap_or(0);
        self.focused_idx = Some(if current == 0 {
            self.window_count - 1
        } else {
            current - 1
        });
    }
}
