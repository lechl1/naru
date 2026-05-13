//! Fixed panels — vertical strips pinned to the left/right edges of a workspace.
//!
//! A `FixedStrip` is a thin wrapper around `ScrollingSpace` operating in
//! "no-scroll" mode: `view_offset` stays at zero, scroll/center/edge-wrap
//! methods are never called. By construction the strip shares the carousel's
//! column/tile/gap math, so windows in a panel size and lay out exactly as
//! they would in the carousel.
//!
//! Windows enter a strip only via stack-move overflow at the carousel's
//! left/right edges. Cross-strip moves route through the carousel multi-step.
//! The strip's screen position is anchored to one workspace edge (left or
//! right); panel width is `sum(column.width) + gaps` and is zero while empty.

use std::rc::Rc;

use smithay::utils::{Logical, Rectangle, Size};

use super::scrolling::{Column, ScrollingSpace, ScrollingSpaceRenderElement};
use super::tile::Tile;
use super::{LayoutElement, Options};
use crate::animation::Clock;
use crate::render_helpers::renderer::NaruRenderer;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;

/// Which edge of the workspace working area this strip is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedSide {
    Left,
    Right,
}

/// A vertical strip of columns pinned to one edge of the workspace.
///
/// Wraps a `ScrollingSpace<W>` running with the carousel disabled. The wrapper
/// exposes only the operations that make sense for a panel — there is no
/// public surface for `view_offset` manipulation, scroll animation, or other
/// carousel-specific behaviour.
#[derive(Debug)]
pub struct FixedStrip<W: LayoutElement> {
    /// Which workspace edge this strip is anchored to.
    side: FixedSide,

    /// Underlying scrolling layout, used as a column container only.
    /// `view_offset` is held at static zero — callers route column adds /
    /// removes through this wrapper so the strip never falls into the
    /// carousel's scroll/center code paths.
    inner: ScrollingSpace<W>,
}

impl<W: LayoutElement> FixedStrip<W> {
    pub fn new(
        side: FixedSide,
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        Self {
            side,
            inner: ScrollingSpace::new(view_size, parent_area, scale, clock, options),
        }
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        self.inner
            .update_config(view_size, parent_area, scale, options);
        self.inner.force_view_offset_zero();
    }

    pub fn side(&self) -> FixedSide {
        self.side
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Total logical width this strip occupies inside the workspace working
    /// area: sum of column widths plus inter-column gaps. Zero while empty.
    pub fn width(&self) -> f64 {
        self.inner.content_width()
    }

    /// Whether the currently focused column inside this strip is the one
    /// closest to the carousel ("inner edge"). When true, a stack-move toward
    /// the carousel should hand the column back to it instead of moving
    /// within the strip.
    pub fn focused_column_is_at_inner_edge(&self) -> bool {
        let n = self.inner_column_count();
        if n == 0 {
            return false;
        }
        match self.side {
            FixedSide::Left => self.inner_active_column_idx() == Some(n - 1),
            FixedSide::Right => self.inner_active_column_idx() == Some(0),
        }
    }

    fn inner_column_count(&self) -> usize {
        self.inner.column_count()
    }

    fn inner_active_column_idx(&self) -> Option<usize> {
        if self.is_empty() {
            None
        } else {
            Some(self.inner.active_column_index())
        }
    }

    /// Insert a column extracted from the carousel at this strip's inner
    /// (carousel-facing) edge. The new column becomes the focused column
    /// inside the strip.
    pub fn add_column_at_inner_edge(&mut self, column: Column<W>) {
        let insert_idx = match self.side {
            FixedSide::Left => self.inner_column_count(),
            FixedSide::Right => 0,
        };
        self.inner.add_column(Some(insert_idx), column, false, None);
        self.inner.set_active_column_idx_static(insert_idx);
        self.inner.force_view_offset_zero();
    }

    /// Remove the column at this strip's inner (carousel-facing) edge, ready
    /// to be inserted back into the carousel. Returns `None` if the strip is
    /// empty.
    pub fn remove_innermost_column(&mut self) -> Option<Column<W>> {
        let n = self.inner_column_count();
        if n == 0 {
            return None;
        }
        let idx = match self.side {
            FixedSide::Left => n - 1,
            FixedSide::Right => 0,
        };
        let column = self.inner.remove_column_by_idx(idx, None);
        self.inner.force_view_offset_zero();
        Some(column)
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.inner.tiles()
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        self.inner.tiles_mut()
    }

    pub fn advance_animations(&mut self) {
        self.inner.advance_animations();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.inner.are_animations_ongoing()
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        self.inner.update_render_elements(is_active);
    }

    pub fn render<R: NaruRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(ScrollingSpaceRenderElement<R>),
    ) {
        if self.is_empty() {
            return;
        }
        // Fixed-left panels render with the carousel's natural column-layout
        // origin at the workspace's left edge — same place the carousel's
        // leftmost column would be at view_offset 0. Right-side anchoring
        // (translating render output by working_area.width - content_width)
        // is a follow-up.
        self.inner.render(ctx, xray_pos, focus_ring, push);
    }
}
