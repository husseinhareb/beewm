//! Drop shadows for the overview grid: a GLES pixel shader, because a gradient
//! cannot be made from axis-aligned quads.
//!
//! There was a matching shader here that rounded the thumbnails' corners by
//! painting the backdrop colour over them. It only worked while the backdrop
//! was nearly opaque — over a translucent one it double-darkens each corner
//! into a visible patch. Rounding a *live surface* properly means masking its
//! texture (a texture-shader override keyed off `gl_FragCoord`, or an offscreen
//! pass), which is a different piece of work; until then the cards are square.

use std::borrow::BorrowMut;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{
    GlesPixelProgram, GlesRenderer, Uniform, UniformName, UniformType,
};
use smithay::utils::{Logical, Rectangle};

/// Corner radius of the *shadow*, in logical pixels. The cards themselves are
/// square; a slightly rounded shadow under a square card reads as soft rather
/// than as a mismatch, since it has no hard edge of its own.
const RADIUS: f32 = 10.0;
/// How far the shadow reaches beyond a cell, in logical pixels. The element has
/// to be grown by this much for the gradient to have somewhere to fall off.
pub const SHADOW_SPREAD: i32 = 18;
/// Peak shadow opacity, directly under the card's edge.
const SHADOW_ALPHA: f32 = 0.55;
/// How far the shadow is pushed down, giving the cards a consistent light
/// source rather than an even glow.
pub const SHADOW_OFFSET_Y: i32 = 5;

/// Signed distance to a rounded box, shared by both shaders. Negative inside,
/// positive outside, in *logical* pixels — `size` and `v_coords` are both in
/// the element's own logical space — so it doubles as antialiasing coverage
/// once run through a `smoothstep` a pixel wide.
///
/// `size`, `alpha` and `v_coords` are supplied by smithay itself for every
/// pixel shader and must not be registered as additional uniforms. `#version`
/// must not appear either; it is prepended.
const SD_ROUNDED_BOX: &str = "
precision mediump float;
uniform vec2 size;
uniform float alpha;
uniform float radius;
varying vec2 v_coords;

float sd_rounded_box(vec2 p, vec2 half_size, float r) {
    vec2 q = abs(p) - half_size + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;
}
";

/// Soft drop shadow behind a card.
///
/// The falloff is `smoothstep` over the spread rather than a true Gaussian
/// blur: one pass, no sampling, and at this size the difference is not
/// visible.
const SHADOW_SHADER: &str = "
uniform float spread;

void main() {
    vec2 p = v_coords * size;
    // The shadow's own rect is the card grown by `spread` on every side, so
    // the card's edge sits `spread` inside it and `radius` is the card's own.
    float d = sd_rounded_box(p - size * 0.5, size * 0.5 - spread, radius);
    float shade = 1.0 - smoothstep(0.0, spread, max(d, 0.0));
    // Squared falloff reads closer to a real blur than a linear ramp.
    gl_FragColor = vec4(0.0, 0.0, 0.0, shade * shade * alpha);
}
";

/// The compiled shadow program, cached on the renderer so it is built once per
/// session rather than once per frame.
pub struct OverviewShaders {
    shadow: GlesPixelProgram,
}

impl OverviewShaders {
    fn compile(renderer: &mut GlesRenderer) -> Option<Self> {
        let shadow = renderer
            .compile_custom_pixel_shader(
                format!("{SD_ROUNDED_BOX}{SHADOW_SHADER}"),
                &[
                    UniformName::new("radius", UniformType::_1f),
                    UniformName::new("spread", UniformType::_1f),
                ],
            )
            .map_err(|error| tracing::warn!("overview shadow shader failed to compile: {error}"))
            .ok()?;
        Some(Self { shadow })
    }
}

/// Fetch the compiled shaders for `renderer`, compiling them on first use.
///
/// They live in the renderer's EGL context user data, which is where smithay
/// expects per-renderer GL objects to be kept: it ties their lifetime to the
/// context that owns them, so a renderer torn down and rebuilt (a GPU reset,
/// a session switch) gets fresh programs rather than dangling handles.
///
/// Returns `None` when compilation failed, in which case the caller draws the
/// grid without chrome rather than not at all.
fn shaders<R>(renderer: &mut R) -> Option<&OverviewShaders>
where
    R: BorrowMut<GlesRenderer>,
{
    let renderer: &mut GlesRenderer = renderer.borrow_mut();
    if renderer
        .egl_context()
        .user_data()
        .get::<Option<OverviewShaders>>()
        .is_none()
    {
        let compiled = OverviewShaders::compile(renderer);
        renderer
            .egl_context()
            .user_data()
            .insert_if_missing(|| compiled);
    }
    renderer
        .egl_context()
        .user_data()
        .get::<Option<OverviewShaders>>()?
        .as_ref()
}

/// The drop shadow cast by `cell`.
pub fn shadow_element<R>(
    renderer: &mut R,
    cell: Rectangle<i32, Logical>,
) -> Option<PixelShaderElement>
where
    R: BorrowMut<GlesRenderer>,
{
    let shader = shaders(renderer)?.shadow.clone();
    let mut area = grow(cell, SHADOW_SPREAD);
    area.loc.y += SHADOW_OFFSET_Y;
    Some(PixelShaderElement::new(
        shader,
        area,
        None,
        SHADOW_ALPHA,
        vec![
            Uniform::new("radius", RADIUS),
            Uniform::new("spread", SHADOW_SPREAD as f32),
        ],
        Kind::Unspecified,
    ))
}

/// Expand a rectangle by `by` on every side.
fn grow(rect: Rectangle<i32, Logical>, by: i32) -> Rectangle<i32, Logical> {
    Rectangle::new(
        (rect.loc.x - by, rect.loc.y - by).into(),
        (rect.size.w + by * 2, rect.size.h + by * 2).into(),
    )
}
