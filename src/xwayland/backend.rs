use std::process::Stdio;

use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::xwayland_shell::XWaylandShellHandler;
use smithay::xwayland::{X11Wm, XWayland, XWaylandEvent, XwmHandler};

use crate::compositor::state::Beewm;

pub(crate) trait XWaylandStateAccess {
    fn xwayland_state(&mut self) -> &mut Beewm;
}

macro_rules! delegate_backend_xwayland {
    ($ty:ty, $field:ident) => {
        impl $crate::xwayland::backend::XWaylandStateAccess for $ty {
            fn xwayland_state(&mut self) -> &mut $crate::compositor::state::Beewm {
                &mut self.$field
            }
        }

        impl smithay::wayland::xwayland_shell::XWaylandShellHandler for $ty {
            fn xwayland_shell_state(
                &mut self,
            ) -> &mut smithay::wayland::xwayland_shell::XWaylandShellState {
                <$crate::compositor::state::Beewm as smithay::wayland::xwayland_shell::XWaylandShellHandler>::xwayland_shell_state(&mut self.$field)
            }

            fn surface_associated(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                wl_surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
                surface: smithay::xwayland::X11Surface,
            ) {
                <$crate::compositor::state::Beewm as smithay::wayland::xwayland_shell::XWaylandShellHandler>::surface_associated(
                    &mut self.$field,
                    xwm,
                    wl_surface,
                    surface,
                );
            }
        }

        impl smithay::xwayland::XwmHandler for $ty {
            fn xwm_state(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
            ) -> &mut smithay::xwayland::X11Wm {
                <$crate::compositor::state::Beewm as smithay::xwayland::XwmHandler>::xwm_state(
                    &mut self.$field,
                    xwm,
                )
            }

            fn new_window(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                window: smithay::xwayland::X11Surface,
            ) {
                <$crate::compositor::state::Beewm as smithay::xwayland::XwmHandler>::new_window(
                    &mut self.$field,
                    xwm,
                    window,
                );
            }

            fn new_override_redirect_window(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                window: smithay::xwayland::X11Surface,
            ) {
                <$crate::compositor::state::Beewm as smithay::xwayland::XwmHandler>::new_override_redirect_window(
                    &mut self.$field,
                    xwm,
                    window,
                );
            }

            fn map_window_request(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                window: smithay::xwayland::X11Surface,
            ) {
                <$crate::compositor::state::Beewm as smithay::xwayland::XwmHandler>::map_window_request(
                    &mut self.$field,
                    xwm,
                    window,
                );
            }

            fn mapped_override_redirect_window(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                window: smithay::xwayland::X11Surface,
            ) {
                <$crate::compositor::state::Beewm as smithay::xwayland::XwmHandler>::mapped_override_redirect_window(
                    &mut self.$field,
                    xwm,
                    window,
                );
            }

            fn unmapped_window(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                window: smithay::xwayland::X11Surface,
            ) {
                <$crate::compositor::state::Beewm as smithay::xwayland::XwmHandler>::unmapped_window(
                    &mut self.$field,
                    xwm,
                    window,
                );
            }

            fn destroyed_window(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                window: smithay::xwayland::X11Surface,
            ) {
                <$crate::compositor::state::Beewm as smithay::xwayland::XwmHandler>::destroyed_window(
                    &mut self.$field,
                    xwm,
                    window,
                );
            }

            fn configure_request(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                window: smithay::xwayland::X11Surface,
                x: Option<i32>,
                y: Option<i32>,
                w: Option<u32>,
                h: Option<u32>,
                reorder: Option<smithay::xwayland::xwm::Reorder>,
            ) {
                <$crate::compositor::state::Beewm as smithay::xwayland::XwmHandler>::configure_request(
                    &mut self.$field,
                    xwm,
                    window,
                    x,
                    y,
                    w,
                    h,
                    reorder,
                );
            }

            fn configure_notify(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                window: smithay::xwayland::X11Surface,
                geometry: smithay::utils::Rectangle<i32, smithay::utils::Logical>,
                above: Option<u32>,
            ) {
                <$crate::compositor::state::Beewm as smithay::xwayland::XwmHandler>::configure_notify(
                    &mut self.$field,
                    xwm,
                    window,
                    geometry,
                    above,
                );
            }

            fn resize_request(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                window: smithay::xwayland::X11Surface,
                button: u32,
                resize_edge: smithay::xwayland::xwm::ResizeEdge,
            ) {
                <$crate::compositor::state::Beewm as smithay::xwayland::XwmHandler>::resize_request(
                    &mut self.$field,
                    xwm,
                    window,
                    button,
                    resize_edge,
                );
            }

            fn move_request(
                &mut self,
                xwm: smithay::xwayland::xwm::XwmId,
                window: smithay::xwayland::X11Surface,
                button: u32,
            ) {
                <$crate::compositor::state::Beewm as smithay::xwayland::XwmHandler>::move_request(
                    &mut self.$field,
                    xwm,
                    window,
                    button,
                );
            }

            fn disconnected(&mut self, xwm: smithay::xwayland::xwm::XwmId) {
                <$crate::compositor::state::Beewm as smithay::xwayland::XwmHandler>::disconnected(
                    &mut self.$field,
                    xwm,
                );
            }
        }
    };
}

pub(crate) use delegate_backend_xwayland;

pub(crate) fn start_xwayland<D>(
    loop_handle: LoopHandle<'static, D>,
    display_handle: &DisplayHandle,
    state: &mut Beewm,
) where
    D: XWaylandStateAccess + XwmHandler + XWaylandShellHandler + 'static,
{
    state.mark_xwayland_starting();

    match XWayland::spawn(
        display_handle,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        Stdio::null(),
        Stdio::null(),
        |_| {},
    ) {
        Ok((xwayland, client)) => {
            if let Err(error) =
                loop_handle
                    .clone()
                    .insert_source(xwayland, move |event, _, data: &mut D| match event {
                        XWaylandEvent::Ready {
                            x11_socket,
                            display_number,
                        } => match X11Wm::start_wm(loop_handle.clone(), x11_socket, client.clone())
                        {
                            Ok(wm) => {
                                let state = data.xwayland_state();
                                state.finish_xwayland_start(display_number);
                                state.xwm = Some(wm);
                            }
                            Err(error) => {
                                tracing::warn!("Failed to attach XWayland WM: {}", error);
                                data.xwayland_state().fail_xwayland_start();
                            }
                        },
                        XWaylandEvent::Error => {
                            tracing::warn!("XWayland crashed during startup");
                            data.xwayland_state().fail_xwayland_start();
                        }
                    })
            {
                tracing::warn!("Failed to insert XWayland event source: {}", error);
                state.fail_xwayland_start();
            }
        }
        Err(error) => {
            tracing::warn!("Failed to start XWayland: {}", error);
            state.fail_xwayland_start();
        }
    }
}
