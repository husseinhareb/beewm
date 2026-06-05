use std::collections::HashMap;
use std::time::Instant;

use smithay::backend::renderer::ImportAll;
use smithay::backend::renderer::Renderer;
use smithay::backend::renderer::Texture;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::{
    render_elements_from_surface_tree, WaylandSurfaceRenderElement,
};
use smithay::backend::renderer::element::utils::{
    ConstrainAlign, ConstrainScaleBehavior, CropRenderElement, RelocateRenderElement,
    RescaleRenderElement,
};
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::desktop::space::{ConstrainBehavior, ConstrainReference, constrain_space_element};
use smithay::desktop::{Space, Window, layer_map_for_output};
use smithay::output::Output;
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Transform};
use smithay::wayland::session_lock::LockSurface;
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use crate::compositor::animation::AnimationManager;
use crate::compositor::state::Beewm;

/// A window surface that has been scaled/relocated/cropped into an animation's
/// current visual rectangle. This is the niri/cosmic-style transform stack.
pub type AnimatedSurface<R> =
    CropRenderElement<RelocateRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<R>>>>;

/// One window's contribution to a frame: either rendered normally at its real
/// `Space` position, or transformed into an active animation's visual rect.
///
/// Keeping both behind one element type preserves the window z-order in a
/// single pass (a non-animated floating dialog can sit above an animating tiled
/// window without special casing).
pub enum WindowElement<R: Renderer> {
    Surface(WaylandSurfaceRenderElement<R>),
    Animated(AnimatedSurface<R>),
}

impl<R> From<WaylandSurfaceRenderElement<R>> for WindowElement<R>
where
    R: Renderer + ImportAll,
    R::TextureId: Texture + 'static,
{
    fn from(value: WaylandSurfaceRenderElement<R>) -> Self {
        Self::Surface(value)
    }
}

impl<R> From<AnimatedSurface<R>> for WindowElement<R>
where
    R: Renderer + ImportAll,
    R::TextureId: Texture + 'static,
{
    fn from(value: AnimatedSurface<R>) -> Self {
        Self::Animated(value)
    }
}

impl<R> Element for WindowElement<R>
where
    R: Renderer + ImportAll,
    R::TextureId: Texture + 'static,
{
    fn id(&self) -> &Id {
        match self {
            Self::Surface(e) => e.id(),
            Self::Animated(e) => e.id(),
        }
    }

    fn current_commit(&self) -> CommitCounter {
        match self {
            Self::Surface(e) => e.current_commit(),
            Self::Animated(e) => e.current_commit(),
        }
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        match self {
            Self::Surface(e) => e.location(scale),
            Self::Animated(e) => e.location(scale),
        }
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        match self {
            Self::Surface(e) => e.src(),
            Self::Animated(e) => e.src(),
        }
    }

    fn transform(&self) -> Transform {
        match self {
            Self::Surface(e) => e.transform(),
            Self::Animated(e) => e.transform(),
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            Self::Surface(e) => e.geometry(scale),
            Self::Animated(e) => e.geometry(scale),
        }
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        match self {
            Self::Surface(e) => e.damage_since(scale, commit),
            Self::Animated(e) => e.damage_since(scale, commit),
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        match self {
            Self::Surface(e) => e.opaque_regions(scale),
            Self::Animated(e) => e.opaque_regions(scale),
        }
    }

    fn alpha(&self) -> f32 {
        match self {
            Self::Surface(e) => e.alpha(),
            Self::Animated(e) => e.alpha(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            Self::Surface(e) => e.kind(),
            Self::Animated(e) => e.kind(),
        }
    }
}

impl<R> RenderElement<R> for WindowElement<R>
where
    R: Renderer + ImportAll,
    R::TextureId: Texture + 'static,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), R::Error> {
        match self {
            Self::Surface(e) => e.draw(frame, src, dst, damage, opaque_regions),
            Self::Animated(e) => e.draw(frame, src, dst, damage, opaque_regions),
        }
    }

    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        match self {
            Self::Surface(e) => e.underlying_storage(renderer),
            Self::Animated(e) => e.underlying_storage(renderer),
        }
    }
}

/// Build the per-window render elements for an output, applying any active
/// visual animation to each window.
///
/// Windows without an animation are rendered exactly as
/// `Space::render_elements_for_region` would (same z-order, same location
/// formula). Windows with an animation are rendered through
/// `constrain_space_element` into their current interpolated rectangle:
///
/// * **reveal** animations (open/close) use `CutOff` + top-left align, so the
///   real-size content is progressively *clipped/revealed* from the top-left —
///   this is the "expand from top-left" effect and avoids stretching the app
///   content.
/// * **geometry** animations (layout resize) use `Stretch`, scaling the current
///   buffer to fill the interpolated rectangle so there are no gaps while the
///   client catches up to its new configured size.
pub(crate) fn window_render_elements<R>(
    renderer: &mut R,
    space: &Space<Window>,
    output: &Output,
    alpha: f32,
    animations: &AnimationManager,
    now: Instant,
) -> Vec<WindowElement<R>>
where
    R: Renderer + ImportAll,
    R::TextureId: Texture + Clone + 'static,
{
    let Some(region) = space.output_geometry(output) else {
        return Vec::new();
    };
    let scale = Scale::from(output.current_scale().fractional_scale());

    // Fast path: when nothing is animating (the common case, including any
    // fullscreen game) fall back to Smithay's batched space rendering and just
    // wrap the results. This keeps the hot/idle path byte-for-byte identical to
    // the pre-animation code so direct scanout and game performance are
    // unaffected.
    if !animations.has_active() {
        return space
            .render_elements_for_region(renderer, &region, scale, alpha)
            .into_iter()
            .map(WindowElement::Surface)
            .collect();
    }

    let mut elements: Vec<WindowElement<R>> = Vec::new();

    // Front-to-back (topmost first), matching render_elements_for_region.
    for window in space.elements().rev() {
        let visual = Beewm::window_root_surface(window)
            .and_then(|root| animations.active_rect(&root, now));

        // Skip windows fully outside the region that are not being animated.
        if visual.is_none() && !region.overlaps(window.bbox()) {
            continue;
        }

        match visual {
            Some(visual) => {
                // The visual rect is in global-logical space; move it into the
                // output's render region frame.
                let mut constrain = visual.rect;
                constrain.loc -= region.loc;
                let behavior = ConstrainBehavior {
                    reference: ConstrainReference::Geometry,
                    behavior: if visual.reveal {
                        ConstrainScaleBehavior::CutOff
                    } else {
                        ConstrainScaleBehavior::Stretch
                    },
                    align: ConstrainAlign::TOP_LEFT,
                };
                elements.extend(constrain_space_element::<R, Window, WindowElement<R>>(
                    renderer,
                    window,
                    constrain.loc,
                    alpha,
                    scale,
                    constrain,
                    behavior,
                ));
            }
            None => {
                let Some(location) = space.element_location(window) else {
                    continue;
                };
                // Same location formula as Space::render_elements_for_region:
                // render_location = element_location - geometry().loc, then made
                // relative to the output region.
                let render_loc = location - window.geometry().loc - region.loc;
                let phys = render_loc.to_physical_precise_round(scale);
                elements.extend(
                    window
                        .render_elements::<WaylandSurfaceRenderElement<R>>(
                            renderer, phys, scale, alpha,
                        )
                        .into_iter()
                        .map(WindowElement::Surface),
                );
            }
        }
    }

    elements
}

pub(crate) fn layer_render_elements<R>(
    renderer: &mut R,
    output: &Output,
    layers: &[WlrLayer],
    alpha: f32,
) -> Vec<WaylandSurfaceRenderElement<R>>
where
    R: smithay::backend::renderer::Renderer + ImportAll,
    R::TextureId: Texture + Clone + 'static,
{
    let scale = Scale::from(output.current_scale().fractional_scale());
    let layer_map = layer_map_for_output(output);
    let mut render_elements = Vec::new();

    for &layer_kind in layers {
        for layer_surface in layer_map.layers_on(layer_kind) {
            let Some(geo) = layer_map.layer_geometry(layer_surface) else {
                continue;
            };
            let location = geo.loc.to_physical_precise_round(scale);
            render_elements.extend(layer_surface.render_elements(renderer, location, scale, alpha));
        }
    }

    render_elements
}

/// Build the render elements for the session-lock surface covering `output`,
/// if any. Returns an empty vec when the output has no lock surface or the lock
/// client has died — in both cases the caller renders solid black underneath,
/// so the session is never exposed.
// `Output` has interior mutability (Arc<Mutex<…>>) but its Hash/Eq use stable
// identity, so it is a sound HashMap key — the standard smithay pattern.
#[allow(clippy::mutable_key_type)]
pub(crate) fn lock_render_elements<R>(
    renderer: &mut R,
    output: &Output,
    lock_surfaces: &HashMap<Output, LockSurface>,
    alpha: f32,
) -> Vec<WaylandSurfaceRenderElement<R>>
where
    R: Renderer + ImportAll,
    R::TextureId: Texture + Clone + 'static,
{
    let scale = Scale::from(output.current_scale().fractional_scale());
    match lock_surfaces.get(output) {
        Some(lock) if lock.alive() => render_elements_from_surface_tree(
            renderer,
            lock.wl_surface(),
            (0, 0),
            scale,
            alpha,
            Kind::Unspecified,
        ),
        _ => Vec::new(),
    }
}

/// Combined render element for the DRM compositor.
/// Wraps window/layer surfaces and custom elements (borders, cursor).
pub enum OutputRenderElement {
    Surface(Box<WaylandSurfaceRenderElement<GlesRenderer>>),
    Window(Box<WindowElement<GlesRenderer>>),
    Border(SolidColorRenderElement),
    Cursor(Box<MemoryRenderBufferRenderElement<GlesRenderer>>),
}

impl From<WaylandSurfaceRenderElement<GlesRenderer>> for OutputRenderElement {
    fn from(e: WaylandSurfaceRenderElement<GlesRenderer>) -> Self {
        Self::Surface(Box::new(e))
    }
}

impl From<WindowElement<GlesRenderer>> for OutputRenderElement {
    fn from(e: WindowElement<GlesRenderer>) -> Self {
        Self::Window(Box::new(e))
    }
}

impl From<SolidColorRenderElement> for OutputRenderElement {
    fn from(e: SolidColorRenderElement) -> Self {
        Self::Border(e)
    }
}

impl From<MemoryRenderBufferRenderElement<GlesRenderer>> for OutputRenderElement {
    fn from(e: MemoryRenderBufferRenderElement<GlesRenderer>) -> Self {
        Self::Cursor(Box::new(e))
    }
}

impl Element for OutputRenderElement {
    fn id(&self) -> &Id {
        match self {
            Self::Surface(e) => e.id(),
            Self::Window(e) => e.id(),
            Self::Border(e) => e.id(),
            Self::Cursor(e) => e.id(),
        }
    }

    fn current_commit(&self) -> CommitCounter {
        match self {
            Self::Surface(e) => e.current_commit(),
            Self::Window(e) => e.current_commit(),
            Self::Border(e) => e.current_commit(),
            Self::Cursor(e) => e.current_commit(),
        }
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        match self {
            Self::Surface(e) => e.location(scale),
            Self::Window(e) => e.location(scale),
            Self::Border(e) => e.location(scale),
            Self::Cursor(e) => e.location(scale),
        }
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        match self {
            Self::Surface(e) => e.src(),
            Self::Window(e) => e.src(),
            Self::Border(e) => e.src(),
            Self::Cursor(e) => e.src(),
        }
    }

    fn transform(&self) -> Transform {
        match self {
            Self::Surface(e) => e.transform(),
            Self::Window(e) => e.transform(),
            Self::Border(e) => e.transform(),
            Self::Cursor(e) => e.transform(),
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            Self::Surface(e) => e.geometry(scale),
            Self::Window(e) => e.geometry(scale),
            Self::Border(e) => e.geometry(scale),
            Self::Cursor(e) => e.geometry(scale),
        }
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        match self {
            Self::Surface(e) => e.damage_since(scale, commit),
            Self::Window(e) => e.damage_since(scale, commit),
            Self::Border(e) => e.damage_since(scale, commit),
            Self::Cursor(e) => e.damage_since(scale, commit),
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        match self {
            Self::Surface(e) => e.opaque_regions(scale),
            Self::Window(e) => e.opaque_regions(scale),
            Self::Border(e) => e.opaque_regions(scale),
            Self::Cursor(e) => e.opaque_regions(scale),
        }
    }

    fn alpha(&self) -> f32 {
        match self {
            Self::Surface(e) => e.alpha(),
            Self::Window(e) => e.alpha(),
            Self::Border(e) => e.alpha(),
            Self::Cursor(e) => e.alpha(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            Self::Surface(e) => e.kind(),
            Self::Window(e) => e.kind(),
            Self::Border(e) => e.kind(),
            Self::Cursor(e) => e.kind(),
        }
    }
}

impl RenderElement<GlesRenderer> for OutputRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        match self {
            Self::Surface(e) => RenderElement::<GlesRenderer>::draw(
                e.as_ref(),
                frame,
                src,
                dst,
                damage,
                opaque_regions,
            ),
            Self::Window(e) => RenderElement::<GlesRenderer>::draw(
                e.as_ref(),
                frame,
                src,
                dst,
                damage,
                opaque_regions,
            ),
            Self::Border(e) => {
                RenderElement::<GlesRenderer>::draw(e, frame, src, dst, damage, opaque_regions)
            }
            Self::Cursor(e) => RenderElement::<GlesRenderer>::draw(
                e.as_ref(),
                frame,
                src,
                dst,
                damage,
                opaque_regions,
            ),
        }
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        match self {
            Self::Surface(e) => e.as_ref().underlying_storage(renderer),
            Self::Window(e) => e.as_ref().underlying_storage(renderer),
            Self::Border(e) => e.underlying_storage(renderer),
            Self::Cursor(e) => e.as_ref().underlying_storage(renderer),
        }
    }
}
