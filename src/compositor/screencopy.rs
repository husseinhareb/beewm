use std::time::{SystemTime, UNIX_EPOCH};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::render_elements;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::utils::draw_render_elements;
use smithay::backend::renderer::{
    Bind, Color32F, ExportMem, Frame, ImportAll, ImportMem, Offscreen, Renderer, Texture,
};
use smithay::backend::renderer::gles::GlesTexture;
use smithay::output::Output;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
    zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_output::WlOutput, wl_shm};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, GlobalId},
};
use smithay::utils::{Buffer, Logical, Physical, Rectangle, Size, Transform};
use smithay::wayland::shm::with_buffer_contents_mut;
use tracing::warn;

use crate::compositor::layering::{layers_rendered_above_windows, layers_rendered_below_windows};
use crate::compositor::render::{layer_render_elements, window_render_elements};
use crate::compositor::state::Beewm;

render_elements! {
    pub(crate) ScreencopyRenderElement<R> where R: ImportAll + ImportMem;
    Surface=WaylandSurfaceRenderElement<R>,
    Border=SolidColorRenderElement,
    Cursor=MemoryRenderBufferRenderElement<R>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScreencopyGeometry {
    pub render_size: Size<i32, Physical>,
    pub output_scale: f64,
    pub output_transform: Transform,
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
        if frame.version() >= 3 {
            frame.buffer_done();
        }

        self.pending_screencopy_frames.push(PendingScreencopyFrame {
            frame,
            output,
            geometry,
            overlay_cursor,
            buffer: None,
            copy_with_damage: false,
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

        if !validate_shm_buffer(&buffer, &pending.geometry) {
            frame.post_error(
                zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                "invalid screencopy wl_shm buffer",
            );
            self.drop_screencopy_frame(frame);
            return;
        }

        pending.buffer = Some(buffer);
        pending.copy_with_damage = copy_with_damage;
        self.needs_render = true;
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
                state.create_screencopy_frame(
                    frame,
                    output,
                    overlay_cursor != 0,
                    None,
                    data_init,
                );
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

pub(crate) fn process_pending_screencopies<R>(
    state: &mut Beewm,
    renderer: &mut R,
    output: &Output,
) where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesTexture> + Bind<GlesTexture>,
    R::TextureId: Texture + Clone + Send + 'static,
    R::Error: std::fmt::Debug,
{
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

fn process_screencopy_frame<R>(
    state: &mut Beewm,
    renderer: &mut R,
    pending: PendingScreencopyFrame,
) where
    R: Renderer + ImportAll + ImportMem + ExportMem + Offscreen<GlesTexture> + Bind<GlesTexture>,
    R::TextureId: Texture + Clone + Send + 'static,
    R::Error: std::fmt::Debug,
{
    let Some(buffer) = pending.buffer.clone() else {
        return;
    };

    let result = (|| -> Result<bool, R::Error> {
        let elements =
            screencopy_render_elements(state, renderer, &pending.output, pending.overlay_cursor);
        let mut target = renderer.create_buffer(Fourcc::Argb8888, pending.geometry.buffer_size)?;
        let mut framebuffer = renderer.bind(&mut target)?;

        let mut frame = renderer.render(
            &mut framebuffer,
            pending.geometry.render_size,
            pending.geometry.output_transform.invert(),
        )?;

        let render_area =
            Rectangle::from_size(pending.geometry.output_transform.transform_size(pending.geometry.render_size));
        frame.clear(Color32F::from([0.1, 0.1, 0.1, 1.0]), &[render_area])?;
        let _ = draw_render_elements::<R, _, ScreencopyRenderElement<R>>(
            &mut frame,
            pending.geometry.output_scale,
            &elements,
            &[render_area],
        )?;
        let _ = frame.finish()?;

        let read_region = readback_region(&pending.geometry);
        let mapping = renderer.copy_framebuffer(&framebuffer, read_region, Fourcc::Argb8888)?;
        let pixels = renderer.map_texture(&mapping)?;
        Ok(write_pixels_into_buffer(&buffer, pixels, &pending.geometry))
    })();

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
            pending
                .frame
                .flags(zwlr_screencopy_frame_v1::Flags::YInvert);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let secs = now.as_secs();
            pending
                .frame
                .ready((secs >> 32) as u32, secs as u32, now.subsec_nanos());
            buffer.release();
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

fn screencopy_render_elements<R>(
    state: &mut Beewm,
    renderer: &mut R,
    output: &Output,
    overlay_cursor: bool,
) -> Vec<ScreencopyRenderElement<R>>
where
    R: Renderer + ImportAll + ImportMem,
    R::TextureId: Texture + Clone + Send + 'static,
{
    let fullscreen_active = state.fullscreen_window.is_some();
    let window_elements = window_render_elements(renderer, &state.space, output, 1.0);
    let border_elements = state.border_elements();
    let cursor_elements = if overlay_cursor {
        state.cursor_elements_for_renderer(renderer)
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

    let mut elements = Vec::new();
    elements.extend(cursor_elements.into_iter().map(ScreencopyRenderElement::from));
    elements.extend(layers_above.into_iter().map(ScreencopyRenderElement::from));
    elements.extend(border_elements.into_iter().map(ScreencopyRenderElement::from));
    elements.extend(window_elements.into_iter().map(ScreencopyRenderElement::from));
    elements.extend(layers_below.into_iter().map(ScreencopyRenderElement::from));
    elements
}

fn geometry_for_output(
    output: &Output,
    requested_region: Option<Rectangle<i32, Logical>>,
) -> Option<ScreencopyGeometry> {
    let mode = output.current_mode()?;
    Some(capture_geometry(
        mode.size,
        output.current_scale().fractional_scale(),
        output.current_transform(),
        requested_region,
    )?)
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
        .to_i32_round();
    let logical_output_rect = Rectangle::from_size(logical_output_size);
    let logical_region = requested_region.unwrap_or(logical_output_rect);
    let logical_region = logical_region.intersection(logical_output_rect)?;
    if logical_region.size.w <= 0 || logical_region.size.h <= 0 {
        return None;
    }

    let buffer_size = logical_output_size
        .to_f64()
        .to_buffer(output_scale, output_transform)
        .to_i32_round();
    let buffer_region = logical_region
        .to_f64()
        .to_buffer(output_scale, output_transform, &logical_output_size.to_f64())
        .to_i32_round();
    if buffer_region.size.w <= 0 || buffer_region.size.h <= 0 {
        return None;
    }

    Some(ScreencopyGeometry {
        render_size,
        output_scale,
        output_transform,
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
    Rectangle::new(
        (
            geometry.buffer_region.loc.x,
            geometry.buffer_size.h - geometry.buffer_region.loc.y - geometry.buffer_region.size.h,
        )
            .into(),
        geometry.buffer_region.size,
    )
}

#[cfg(test)]
mod tests {
    use super::{capture_geometry, readback_region};
    use smithay::utils::{Logical, Physical, Rectangle, Size, Transform};

    #[test]
    fn capture_region_is_clipped_to_output_bounds() {
        let geometry = capture_geometry(
            Size::<i32, Physical>::from((1920, 1080)),
            1.0,
            Transform::Normal,
            Some(Rectangle::<i32, Logical>::new((-10, -20).into(), (100, 100).into())),
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
    fn readback_region_flips_y_for_gl() {
        let geometry = capture_geometry(
            Size::<i32, Physical>::from((100, 80)),
            1.0,
            Transform::Normal,
            Some(Rectangle::<i32, Logical>::new((10, 20).into(), (30, 40).into())),
        )
        .expect("geometry should be valid");

        let readback = readback_region(&geometry);
        assert_eq!(readback.loc.x, 10);
        assert_eq!(readback.loc.y, 20);
        assert_eq!(readback.size.w, 30);
        assert_eq!(readback.size.h, 40);
    }

    #[test]
    fn rotated_outputs_produce_transformed_buffer_regions() {
        let geometry = capture_geometry(
            Size::<i32, Physical>::from((1920, 1080)),
            1.0,
            Transform::_90,
            Some(Rectangle::<i32, Logical>::new((0, 0).into(), (200, 100).into())),
        )
        .expect("geometry should be valid");

        assert_eq!(geometry.logical_output_size, Size::from((1080, 1920)));
        assert_eq!(geometry.buffer_size, Size::from((1080, 1920)));
        assert_eq!(geometry.buffer_region.size, Size::from((100, 200)));
    }
}
