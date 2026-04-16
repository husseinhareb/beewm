use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::desktop::space::SpaceRenderElements;
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Transform};

type SpaceElem = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>;

/// Combined render element for the DRM compositor.
/// Wraps space elements (windows, layer surfaces) and custom elements (borders, cursor).
pub enum OutputRenderElement {
    Space(Box<SpaceElem>),
    Border(SolidColorRenderElement),
    Cursor(Box<MemoryRenderBufferRenderElement<GlesRenderer>>),
}

impl From<SpaceElem> for OutputRenderElement {
    fn from(e: SpaceElem) -> Self {
        Self::Space(Box::new(e))
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
            Self::Space(e) => e.id(),
            Self::Border(e) => e.id(),
            Self::Cursor(e) => e.id(),
        }
    }

    fn current_commit(&self) -> CommitCounter {
        match self {
            Self::Space(e) => e.current_commit(),
            Self::Border(e) => e.current_commit(),
            Self::Cursor(e) => e.current_commit(),
        }
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        match self {
            Self::Space(e) => e.location(scale),
            Self::Border(e) => e.location(scale),
            Self::Cursor(e) => e.location(scale),
        }
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        match self {
            Self::Space(e) => e.src(),
            Self::Border(e) => e.src(),
            Self::Cursor(e) => e.src(),
        }
    }

    fn transform(&self) -> Transform {
        match self {
            Self::Space(e) => e.transform(),
            Self::Border(e) => e.transform(),
            Self::Cursor(e) => e.transform(),
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            Self::Space(e) => e.geometry(scale),
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
            Self::Space(e) => e.damage_since(scale, commit),
            Self::Border(e) => e.damage_since(scale, commit),
            Self::Cursor(e) => e.damage_since(scale, commit),
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        match self {
            Self::Space(e) => e.opaque_regions(scale),
            Self::Border(e) => e.opaque_regions(scale),
            Self::Cursor(e) => e.opaque_regions(scale),
        }
    }

    fn alpha(&self) -> f32 {
        match self {
            Self::Space(e) => e.alpha(),
            Self::Border(e) => e.alpha(),
            Self::Cursor(e) => e.alpha(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            Self::Space(e) => e.kind(),
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
            Self::Space(e) => RenderElement::<GlesRenderer>::draw(
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
            Self::Space(e) => e.as_ref().underlying_storage(renderer),
            Self::Border(e) => e.underlying_storage(renderer),
            Self::Cursor(e) => e.as_ref().underlying_storage(renderer),
        }
    }
}
