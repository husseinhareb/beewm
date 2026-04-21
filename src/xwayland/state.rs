use smithay::xwayland::X11Surface;

use crate::compositor::commands::spawn_startup_commands;
use crate::compositor::state::Beewm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingX11Kind {
    Managed,
    OverrideRedirect,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingX11Window {
    pub(crate) kind: PendingX11Kind,
    pub(crate) surface: X11Surface,
}

impl Beewm {
    pub(crate) fn mark_xwayland_starting(&mut self) {
        self.xwayland_start_pending = true;
        self.xdisplay = None;
    }

    pub(crate) fn finish_xwayland_start(&mut self, display_number: u32) {
        self.xwayland_start_pending = false;
        self.xdisplay = Some(display_number);
        self.child_env.set("DISPLAY", format!(":{display_number}"));
        tracing::info!("XWayland ready on DISPLAY=:{}", display_number);
        self.maybe_spawn_startup_commands();
    }

    pub(crate) fn fail_xwayland_start(&mut self) {
        self.xwayland_start_pending = false;
        self.xwm = None;
        self.maybe_spawn_startup_commands();
    }

    pub(crate) fn mark_output_ready(&mut self) {
        self.outputs_ready_for_startup = true;
        self.maybe_spawn_startup_commands();
    }

    pub(crate) fn maybe_spawn_startup_commands(&mut self) {
        if self.startup_commands_spawned
            || !self.outputs_ready_for_startup
            || self.xwayland_start_pending
        {
            return;
        }

        spawn_startup_commands(&self.config.autostart_commands, &self.child_env);
        self.startup_commands_spawned = true;
    }

    pub(crate) fn queue_x11_window(&mut self, surface: X11Surface, kind: PendingX11Kind) {
        if self
            .pending_x11_windows
            .iter()
            .any(|pending| pending.surface == surface)
        {
            return;
        }

        self.pending_x11_windows
            .push(PendingX11Window { kind, surface });
    }

    pub(crate) fn take_pending_x11_window(
        &mut self,
        surface: &X11Surface,
    ) -> Option<PendingX11Window> {
        let idx = self
            .pending_x11_windows
            .iter()
            .position(|pending| pending.surface == *surface)?;
        Some(self.pending_x11_windows.remove(idx))
    }
}
