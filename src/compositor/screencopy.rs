use std::time::{SystemTime, UNIX_EPOCH};

use smithay::backend::allocator::Buffer as _;
use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::render_elements;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{GlesError, GlesRenderer, GlesTarget, GlesTexture};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::utils::draw_render_elements;
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Frame, Offscreen, Renderer};
use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1, zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_output::WlOutput, wl_shm};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, GlobalId},
};
use smithay::utils::{Buffer, Logical, Physical, Rectangle, Size, Transform};
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::wayland::shm::with_buffer_contents_mut;
use tracing::warn;

use crate::compositor::layering::{layers_rendered_above_windows, layers_rendered_below_windows};
use crate::compositor::overview::overview_elements;
use crate::compositor::render::{
    WindowElement, layer_render_elements, lock_render_elements, window_render_elements,
};
use crate::compositor::state::Beewm;

render_elements! {
    pub(crate) ScreencopyRenderElement<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Window=WindowElement<GlesRenderer>,
    Border=SolidColorRenderElement,
    Cursor=MemoryRenderBufferRenderElement<GlesRenderer>,
    Shader=PixelShaderElement,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScreencopyGeometry {
    pub render_size: Size<i32, Physical>,
    pub output_scale: f64,
    pub logical_output_size: Size<i32, Logical>,
    pub buffer_size: Size<i32, Buffer>,
    pub logical_region: Rectangle<i32, Logical>,
    pub buffer_region: Rectangle<i32, Buffer>,
    pub shm_format: wl_shm::Format,
    pub shm_stride: i32,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingScreencopyFrame {
    pub frame: ZwlrScreencopyFrameV1,
    pub output: Output,
    pub geometry: ScreencopyGeometry,
    pub overlay_cursor: bool,
    pub buffer: Option<wl_buffer::WlBuffer>,
    pub copy_with_damage: bool,
    /// Whether a GPU buffer was offered for this frame (see `create_screencopy_frame`).
    pub dmabuf_offered: bool,
    /// Set when the client attached a GPU buffer: the frame is drawn straight
    /// into it, with no readback to system memory.
    pub dmabuf: Option<Dmabuf>,
}

/// The GPU buffer format to offer screencopy clients: one the renderer can draw
/// into, XRGB first (no alpha to carry for a screen image), then ARGB.
pub(crate) fn pick_dmabuf_format(render_formats: &FormatSet) -> Option<Fourcc> {
    [Fourcc::Xrgb8888, Fourcc::Argb8888]
        .into_iter()
        .find(|code| render_formats.iter().any(|format| format.code == *code))
}

pub(crate) fn create_screencopy_global<D>(display: &DisplayHandle) -> GlobalId
where
    D: GlobalDispatch<ZwlrScreencopyManagerV1, ()>
        + Dispatch<ZwlrScreencopyManagerV1, ()>
        + Dispatch<ZwlrScreencopyFrameV1, ()>
        + 'static,
{
    display.create_global::<D, ZwlrScreencopyManagerV1, _>(3, ())
}

impl Beewm {
    fn create_screencopy_frame(
        &mut self,
        frame_resource: New<ZwlrScreencopyFrameV1>,
        output_resource: WlOutput,
        overlay_cursor: bool,
        requested_region: Option<Rectangle<i32, Logical>>,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let frame = data_init.init(frame_resource, ());
        if let Some(client) = frame.client() {
            self.note_capture_frame(&client);
        }

        let Some(output) = Output::from_resource(&output_resource) else {
            frame.failed();
            return;
        };

        let Some(geometry) = geometry_for_output(&output, requested_region) else {
            frame.failed();
            return;
        };

        frame.buffer(
            geometry.shm_format,
            geometry.buffer_region.size.w as u32,
            geometry.buffer_region.size.h as u32,
            geometry.shm_stride as u32,
        );
        frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
        // Offer a GPU buffer too, so a capable client (xdg-desktop-portal-wlr)
        // gets frames that never leave video memory. Whole-output captures only:
        // a region would need its own offset render, and the portal always
        // captures whole outputs.
        let whole_output = geometry.buffer_region == Rectangle::from_size(geometry.buffer_size);
        let dmabuf_format = self
            .screencopy_dmabuf_format
            .filter(|_| frame.version() >= 3 && whole_output && !self.screencopy_dmabuf_broken);
        if let Some(format) = dmabuf_format {
            frame.linux_dmabuf(
                format as u32,
                geometry.buffer_region.size.w as u32,
                geometry.buffer_region.size.h as u32,
            );
        }
        if frame.version() >= 3 {
            frame.buffer_done();
        }

        tracing::debug!(
            target = "beewm::screencast",
            output = output.name(),
            buffer_w = geometry.buffer_region.size.w,
            buffer_h = geometry.buffer_region.size.h,
            stride = geometry.shm_stride,
            scale = geometry.output_scale,
            ?geometry.shm_format,
            region = ?geometry.logical_region,
            overlay_cursor,
            "screencopy frame requested (source advertised to capture client)",
        );

        self.pending_screencopy_frames.push(PendingScreencopyFrame {
            frame,
            output,
            geometry,
            overlay_cursor,
            buffer: None,
            copy_with_damage: false,
            dmabuf_offered: dmabuf_format.is_some(),
            dmabuf: None,
        });
    }

    fn queue_screencopy_copy(
        &mut self,
        frame: &ZwlrScreencopyFrameV1,
        buffer: wl_buffer::WlBuffer,
        copy_with_damage: bool,
    ) {
        let Some(pending) = self
            .pending_screencopy_frames
            .iter_mut()
            .find(|pending| pending.frame.id() == frame.id())
        else {
            return;
        };

        if pending.buffer.is_some() {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "screencopy frame has already been copied",
            );
            return;
        }

        match get_dmabuf(&buffer) {
            Ok(dmabuf) => {
                if !pending.dmabuf_offered || dmabuf.size() != pending.geometry.buffer_region.size {
                    frame.post_error(
                        zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                        "invalid screencopy dmabuf",
                    );
                    self.drop_screencopy_frame(frame);
                    return;
                }
                pending.dmabuf = Some(dmabuf.clone());
            }
            Err(_) if !validate_shm_buffer(&buffer, &pending.geometry) => {
                frame.post_error(
                    zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                    "invalid screencopy wl_shm buffer",
                );
                self.drop_screencopy_frame(frame);
                return;
            }
            Err(_) => {}
        }

        pending.buffer = Some(buffer);
        pending.copy_with_damage = copy_with_damage;
        // `copy_with_damage` asks for the next frame *once something changes*,
        // so it is served by the next redraw that happens anyway. Forcing one
        // here made a stream of a static screen redraw it at full frame rate.
        // A plain `copy` (a screenshot) wants a frame now.
        if !copy_with_damage {
            self.needs_render = true;
        }
    }

    pub(crate) fn drop_screencopy_frame(&mut self, frame: &ZwlrScreencopyFrameV1) {
        self.pending_screencopy_frames
            .retain(|pending| pending.frame.id() != frame.id());
    }
}

impl GlobalDispatch<ZwlrScreencopyManagerV1, (), Beewm> for Beewm {
    fn bind(
        _state: &mut Beewm,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Beewm>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, (), Beewm> for Beewm {
    fn request(
        state: &mut Beewm,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Beewm>,
    ) {
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => {
                state.create_screencopy_frame(frame, output, overlay_cursor != 0, None, data_init);
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => {
                let requested_region = Rectangle::new((x, y).into(), (width, height).into());
                state.create_screencopy_frame(
                    frame,
                    output,
                    overlay_cursor != 0,
                    Some(requested_region),
                    data_init,
                );
            }
            zwlr_screencopy_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, (), Beewm> for Beewm {
    fn request(
        state: &mut Beewm,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Beewm>,
    ) {
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
                state.queue_screencopy_copy(frame, buffer, false);
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                state.queue_screencopy_copy(frame, buffer, true);
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {
                state.drop_screencopy_frame(frame);
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(state: &mut Beewm, _client: ClientId, frame: &ZwlrScreencopyFrameV1, _data: &()) {
        state.drop_screencopy_frame(frame);
    }
}

pub(crate) fn process_pending_screencopies(
    state: &mut Beewm,
    renderer: &mut GlesRenderer,
    output: &Output,
) {
    // Fail and drop any frames whose output is no longer mapped (unplugged
    // mid-capture). Otherwise the client would wait forever for a `ready`/
    // `failed` that can never come, and the entry would leak.
    let live_outputs: Vec<Output> = state.space.outputs().cloned().collect();
    state.pending_screencopy_frames.retain(|pending| {
        if live_outputs.contains(&pending.output) {
            return true;
        }
        tracing::debug!(
            target = "beewm::screencast",
            output = pending.output.name(),
            "output gone while capture pending; failing screencopy frame",
        );
        pending.frame.failed();
        if let Some(buffer) = &pending.buffer {
            buffer.release();
        }
        false
    });

    let mut index = 0;
    while index < state.pending_screencopy_frames.len() {
        let should_process = state.pending_screencopy_frames[index].output == *output
            && state.pending_screencopy_frames[index].buffer.is_some();
        if !should_process {
            index += 1;
            continue;
        }

        let pending = state.pending_screencopy_frames.remove(index);
        process_screencopy_frame(state, renderer, pending);
    }
}

fn process_screencopy_frame(
    state: &mut Beewm,
    renderer: &mut GlesRenderer,
    pending: PendingScreencopyFrame,
) {
    let Some(buffer) = pending.buffer.clone() else {
        return;
    };

    let elements =
        screencopy_render_elements(state, renderer, &pending.output, pending.overlay_cursor);
    let result = match pending.dmabuf.clone() {
        // Straight into the client's GPU buffer: no readback, no copy.
        Some(mut dmabuf) => (|| -> Result<bool, GlesError> {
            let mut framebuffer = renderer.bind(&mut dmabuf)?;
            let sync = draw_capture(renderer, &mut framebuffer, &elements, &pending.geometry)?;
            // The client may read the buffer the moment it sees `ready`.
            let _ = sync.wait();
            Ok(true)
        })()
        .inspect_err(|error| {
            // Most likely a buffer this driver cannot render into. Stop
            // offering GPU buffers so the client falls back to shm.
            warn!(
                "Rendering into a screencopy dmabuf failed ({error:?}); offering shm only from now on"
            );
            state.screencopy_dmabuf_broken = true;
        }),
        None => (|| -> Result<bool, GlesError> {
            let mut target: GlesTexture =
                renderer.create_buffer(Fourcc::Argb8888, pending.geometry.buffer_size)?;
            let mut framebuffer = renderer.bind(&mut target)?;
            let _ = draw_capture(renderer, &mut framebuffer, &elements, &pending.geometry)?;
            let read_region = readback_region(&pending.geometry);
            let mapping = renderer.copy_framebuffer(&framebuffer, read_region, Fourcc::Argb8888)?;
            let pixels = renderer.map_texture(&mapping)?;
            Ok(write_pixels_into_buffer(&buffer, pixels, &pending.geometry))
        })(),
    };

    match result {
        Ok(true) => {
            if pending.copy_with_damage && pending.frame.version() >= 2 {
                pending.frame.damage(
                    0,
                    0,
                    pending.geometry.buffer_region.size.w as u32,
                    pending.geometry.buffer_region.size.h as u32,
                );
            }
            let (secs, nsecs) = monotonic_time();
            pending.frame.ready((secs >> 32) as u32, secs as u32, nsecs);
            buffer.release();
            tracing::trace!(
                target = "beewm::screencast",
                output = pending.output.name(),
                with_damage = pending.copy_with_damage,
                "screencopy frame submitted to capture client",
            );
        }
        Ok(false) => {
            warn!("Failed to write screencopy pixels into client buffer");
            pending.frame.failed();
            buffer.release();
        }
        Err(error) => {
            warn!("Failed to service screencopy request: {:?}", error);
            pending.frame.failed();
            buffer.release();
        }
    }
}

/// Draw one capture of the output into `framebuffer`.
fn draw_capture(
    renderer: &mut GlesRenderer,
    framebuffer: &mut GlesTarget<'_>,
    elements: &[ScreencopyRenderElement],
    geometry: &ScreencopyGeometry,
) -> Result<SyncPoint, GlesError> {
    let mut frame = renderer.render(framebuffer, geometry.render_size, Transform::Normal)?;
    let render_area = Rectangle::from_size(geometry.render_size);
    frame.clear(Color32F::from([0.1, 0.1, 0.1, 1.0]), &[render_area])?;
    let _ = draw_render_elements::<GlesRenderer, _, ScreencopyRenderElement>(
        &mut frame,
        geometry.output_scale,
        elements,
        &[render_area],
    )?;
    frame.finish()
}

fn screencopy_render_elements(
    state: &mut Beewm,
    renderer: &mut GlesRenderer,
    output: &Output,
    overlay_cursor: bool,
) -> Vec<ScreencopyRenderElement> {
    // A locked session must never be captured. The render path in the backend
    // already drops everything but the lock surface (see `render_surface`);
    // screencopy has to make the same cut, or any client that held a
    // zwlr_screencopy_manager_v1 before the lock keeps recording the desktop
    // behind the lock screen. Outputs with no live lock surface capture the
    // black clear color, exactly like they display.
    if state.locked {
        let lock_surface = state.lock_surfaces.get(output);
        return lock_render_elements(renderer, output, lock_surface, 1.0)
            .into_iter()
            .map(ScreencopyRenderElement::from)
            .collect();
    }

    let fullscreen_active = state.screen_owned_by_window(output);
    let window_elements = window_render_elements(
        renderer,
        &state.space,
        output,
        1.0,
        &state.animations,
        std::time::Instant::now(),
        |root| state.is_root_floating(root),
    );
    let border_elements = state.border_elements(output);
    let cursor_elements = if overlay_cursor {
        state.cursor_elements_for_renderer(renderer, output)
    } else {
        Vec::new()
    };
    let layers_below = layer_render_elements(
        renderer,
        output,
        layers_rendered_below_windows(fullscreen_active),
        1.0,
    );
    let layers_above = layer_render_elements(
        renderer,
        output,
        layers_rendered_above_windows(fullscreen_active),
        1.0,
    );

    // The overview grid is drawn on screen above everything else, so capture
    // it too, or a screen share shows the plain desktop while it is open.
    let overview = overview_elements(state, renderer, output);

    let mut elements = Vec::new();
    elements.extend(
        cursor_elements
            .into_iter()
            .map(ScreencopyRenderElement::from),
    );
    elements.extend(overview.into_ordered::<ScreencopyRenderElement>());
    elements.extend(layers_above.into_iter().map(ScreencopyRenderElement::from));
    elements.extend(
        border_elements
            .into_iter()
            .map(ScreencopyRenderElement::from),
    );
    elements.extend(
        window_elements
            .into_iter()
            .map(ScreencopyRenderElement::from),
    );
    elements.extend(layers_below.into_iter().map(ScreencopyRenderElement::from));
    elements
}

fn geometry_for_output(
    output: &Output,
    requested_region: Option<Rectangle<i32, Logical>>,
) -> Option<ScreencopyGeometry> {
    let mode = output.current_mode()?;
    capture_geometry(
        mode.size,
        output.current_scale().fractional_scale(),
        output.current_transform(),
        requested_region,
    )
}

fn capture_geometry(
    render_size: Size<i32, Physical>,
    output_scale: f64,
    output_transform: Transform,
    requested_region: Option<Rectangle<i32, Logical>>,
) -> Option<ScreencopyGeometry> {
    let transformed_render_size = output_transform.transform_size(render_size);
    let logical_output_size = transformed_render_size
        .to_f64()
        .to_logical(output_scale)
        .to_i32_ceil();
    let logical_output_rect = Rectangle::from_size(logical_output_size);
    let logical_region = requested_region.unwrap_or(logical_output_rect);
    let logical_region = logical_region.intersection(logical_output_rect)?;
    if logical_region.size.w <= 0 || logical_region.size.h <= 0 {
        return None;
    }

    let buffer_size_physical = logical_output_size
        .to_f64()
        .to_physical(output_scale)
        .to_i32_round();
    let buffer_size = Size::from((buffer_size_physical.w, buffer_size_physical.h));
    let buffer_region_physical = logical_region
        .to_f64()
        .to_physical(output_scale)
        .to_i32_round();
    let buffer_region = Rectangle::new(
        (buffer_region_physical.loc.x, buffer_region_physical.loc.y).into(),
        (buffer_region_physical.size.w, buffer_region_physical.size.h).into(),
    );
    if buffer_region.size.w <= 0 || buffer_region.size.h <= 0 {
        return None;
    }

    Some(ScreencopyGeometry {
        render_size: transformed_render_size,
        output_scale,
        logical_output_size,
        buffer_size,
        logical_region,
        buffer_region,
        shm_format: wl_shm::Format::Argb8888,
        shm_stride: buffer_region.size.w * 4,
    })
}

fn validate_shm_buffer(buffer: &wl_buffer::WlBuffer, geometry: &ScreencopyGeometry) -> bool {
    with_buffer_contents_mut(buffer, |_ptr, len, data| {
        if data.format != geometry.shm_format
            || data.width != geometry.buffer_region.size.w
            || data.height != geometry.buffer_region.size.h
            || data.stride < geometry.shm_stride
            || data.offset < 0
        {
            return false;
        }

        let total_bytes = match (data.stride as usize).checked_mul(data.height as usize) {
            Some(total_bytes) => total_bytes,
            None => return false,
        };
        let end = match (data.offset as usize).checked_add(total_bytes) {
            Some(end) => end,
            None => return false,
        };

        end <= len
    })
    .unwrap_or(false)
}

fn write_pixels_into_buffer(
    buffer: &wl_buffer::WlBuffer,
    pixels: &[u8],
    geometry: &ScreencopyGeometry,
) -> bool {
    let row_bytes = geometry.shm_stride as usize;
    let rows = geometry.buffer_region.size.h as usize;
    let needed = row_bytes.saturating_mul(rows);
    if pixels.len() < needed {
        return false;
    }

    with_buffer_contents_mut(buffer, |ptr, _len, data| {
        let dst_stride = data.stride as usize;
        let offset = data.offset as usize;
        let total_bytes = dst_stride * data.height as usize;
        // Safety: smithay guarantees the pointer is valid for the duration of the callback.
        let dst = unsafe { std::slice::from_raw_parts_mut(ptr.add(offset), total_bytes) };

        for row in 0..rows {
            let src_start = row * row_bytes;
            let dst_start = row * dst_stride;
            dst[dst_start..dst_start + row_bytes]
                .copy_from_slice(&pixels[src_start..src_start + row_bytes]);
        }
        true
    })
    .unwrap_or(false)
}

fn readback_region(geometry: &ScreencopyGeometry) -> Rectangle<i32, Buffer> {
    // Smithay's GlesRenderer applies a Y-flip in its projection matrix to account
    // for GL's bottom-to-top convention. As a result, physical screen y=0 (top)
    // maps directly to GL framebuffer row 0. glReadPixels therefore returns rows in
    geometry.buffer_region
}

fn monotonic_time() -> (u64, u32) {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } == 0 {
        (ts.tv_sec as u64, ts.tv_nsec as u32)
    } else {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        (now.as_secs(), now.subsec_nanos())
    }
}

#[cfg(test)]
mod tests {
    use super::{capture_geometry, pick_dmabuf_format, readback_region};
    use smithay::backend::allocator::format::FormatSet;
    use smithay::backend::allocator::{Format, Fourcc, Modifier};

    fn formats(codes: &[Fourcc]) -> FormatSet {
        codes
            .iter()
            .map(|&code| Format {
                code,
                modifier: Modifier::Linear,
            })
            .collect()
    }

    #[test]
    fn offers_a_gpu_buffer_format_the_renderer_can_draw_into() {
        assert_eq!(
            pick_dmabuf_format(&formats(&[Fourcc::Argb8888, Fourcc::Xrgb8888])),
            Some(Fourcc::Xrgb8888)
        );
        assert_eq!(
            pick_dmabuf_format(&formats(&[Fourcc::Abgr8888, Fourcc::Argb8888])),
            Some(Fourcc::Argb8888)
        );
        assert_eq!(pick_dmabuf_format(&formats(&[Fourcc::Nv12])), None);
        assert_eq!(pick_dmabuf_format(&FormatSet::default()), None);
    }

    use smithay::utils::{Logical, Physical, Rectangle, Size, Transform};

    #[test]
    fn capture_region_is_clipped_to_output_bounds() {
        let geometry = capture_geometry(
            Size::<i32, Physical>::from((1920, 1080)),
            1.0,
            Transform::Normal,
            Some(Rectangle::<i32, Logical>::new(
                (-10, -20).into(),
                (100, 100).into(),
            )),
        )
        .expect("geometry should be valid");

        assert_eq!(geometry.logical_region.loc.x, 0);
        assert_eq!(geometry.logical_region.loc.y, 0);
        assert_eq!(geometry.logical_region.size.w, 90);
        assert_eq!(geometry.logical_region.size.h, 80);
        assert_eq!(geometry.buffer_region.size.w, 90);
        assert_eq!(geometry.buffer_region.size.h, 80);
    }

    #[test]
    fn readback_region_matches_buffer_region() {
        let geometry = capture_geometry(
            Size::<i32, Physical>::from((100, 80)),
            1.0,
            Transform::Normal,
            Some(Rectangle::<i32, Logical>::new(
                (10, 20).into(),
                (30, 40).into(),
            )),
        )
        .expect("geometry should be valid");

        let readback = readback_region(&geometry);
        // Smithay's renderer already accounts for GL's Y-inversion in its projection
        // matrix, so the readback region is identical to the buffer region (no Y-flip).
        assert_eq!(readback.loc.x, geometry.buffer_region.loc.x);
        assert_eq!(readback.loc.y, geometry.buffer_region.loc.y);
        assert_eq!(readback.size.w, geometry.buffer_region.size.w);
        assert_eq!(readback.size.h, geometry.buffer_region.size.h);
    }

    #[test]
    fn rotated_outputs_produce_transformed_buffer_regions() {
        let geometry = capture_geometry(
            Size::<i32, Physical>::from((1920, 1080)),
            1.0,
            Transform::_90,
            Some(Rectangle::<i32, Logical>::new(
                (0, 0).into(),
                (200, 100).into(),
            )),
        )
        .expect("geometry should be valid");

        assert_eq!(geometry.logical_output_size, Size::from((1080, 1920)));
        assert_eq!(geometry.buffer_size, Size::from((1080, 1920)));
        assert_eq!(geometry.buffer_region.size, Size::from((200, 100)));
    }
}
