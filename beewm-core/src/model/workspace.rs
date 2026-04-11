use crate::WindowHandle;

/// An i3-style numbered workspace.
#[derive(Debug)]
pub struct Workspace<H: WindowHandle> {
    pub id: usize,
    pub name: String,
    pub windows: Vec<H>,
    pub focused: Option<H>,
}

impl<H: WindowHandle> Workspace<H> {
    pub fn new(id: usize) -> Self {
        Self {
            id,
            name: format!("{}", id + 1),
            windows: Vec::new(),
            focused: None,
        }
    }

    pub fn add_window(&mut self, handle: H) {
        self.windows.push(handle);
        self.focused = Some(handle);
    }

    pub fn remove_window(&mut self, handle: H) {
        self.windows.retain(|&w| w != handle);
        if self.focused == Some(handle) {
            self.focused = self.windows.last().copied();
        }
    }

    pub fn focus_next(&mut self) {
        if self.windows.is_empty() {
            return;
        }
        let current_idx = self
            .focused
            .and_then(|f| self.windows.iter().position(|&w| w == f))
            .unwrap_or(0);
        let next_idx = (current_idx + 1) % self.windows.len();
        self.focused = Some(self.windows[next_idx]);
    }

    pub fn focus_prev(&mut self) {
        if self.windows.is_empty() {
            return;
        }
        let current_idx = self
            .focused
            .and_then(|f| self.windows.iter().position(|&w| w == f))
            .unwrap_or(0);
        let prev_idx = if current_idx == 0 {
            self.windows.len() - 1
        } else {
            current_idx - 1
        };
        self.focused = Some(self.windows[prev_idx]);
    }
}
