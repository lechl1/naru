use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{
    GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::Texture as _;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

use super::offscreen::OffscreenRenderElement;
use super::renderer::AsGlesFrame as _;
use super::shaders::Shaders;
use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};

/// Draws an [`OffscreenRenderElement`] through the `edge_fade` shader, applying
/// a linear opacity fade along the x axis. Used to fade the carousel out as it
/// approaches a fixed-side panel instead of darkening it with a shadow band.
#[derive(Debug, Clone)]
pub struct EdgeFadeOffscreenRenderElement {
    inner: OffscreenRenderElement,
    program: EdgeFadeShader,
    /// `cutoff.0` = `v_coords.x` at which the content is fully transparent,
    /// `cutoff.1` = `v_coords.x` at which it is fully opaque.
    cutoff: (f32, f32),
}

#[derive(Debug, Clone)]
pub struct EdgeFadeShader(GlesTexProgram);

impl EdgeFadeOffscreenRenderElement {
    /// Wraps `inner` so that it fades from fully transparent at screen position
    /// `x_alpha0` to fully opaque at `x_alpha1` (both in logical screen-space x).
    /// The screen positions are converted to the texture-coordinate `cutoff` the
    /// shader expects, accounting for the offscreen's offset, logical size, and
    /// the fact that its content may occupy only part of a (re-used) texture.
    pub fn new(
        inner: OffscreenRenderElement,
        program: EdgeFadeShader,
        x_alpha0: f64,
        x_alpha1: f64,
    ) -> Self {
        let off = inner.offset().x;
        let logical_w = inner.logical_size().w;
        // `v_coords` is normalised over the whole texture; the content occupies
        // `[0, src_w / tex_w]` of it (its src starts at the texture origin).
        let src_w = inner.src().size.w;
        let tex_w = inner.texture().size().w as f64;
        let full = if tex_w > 0. { src_w / tex_w } else { 1. };

        let to_uv = |x: f64| -> f32 {
            if logical_w > 0. {
                (full * (x - off) / logical_w) as f32
            } else {
                0.
            }
        };

        let cutoff = (to_uv(x_alpha0), to_uv(x_alpha1));
        Self {
            inner,
            program,
            cutoff,
        }
    }

    pub fn shader(renderer: &mut GlesRenderer) -> Option<EdgeFadeShader> {
        let program = Shaders::get(renderer).edge_fade.clone();
        program.map(EdgeFadeShader)
    }
}

impl Element for EdgeFadeOffscreenRenderElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // The fade introduces partial transparency across the whole element, so
        // nothing can be treated as opaque.
        let _ = scale;
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for EdgeFadeOffscreenRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let uniforms = vec![Uniform::new("cutoff", self.cutoff)];
        frame.override_default_tex_program(self.program.0.clone(), uniforms);
        let res = RenderElement::<GlesRenderer>::draw(
            &self.inner,
            frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        );
        frame.clear_tex_program_override();
        res
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for EdgeFadeOffscreenRenderElement {
    fn draw(
        &self,
        frame: &mut TtyFrame<'render, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        let gles_frame = frame.as_gles_frame();
        RenderElement::<GlesRenderer>::draw(
            self,
            gles_frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )?;
        Ok(())
    }

    fn underlying_storage(
        &self,
        renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}
