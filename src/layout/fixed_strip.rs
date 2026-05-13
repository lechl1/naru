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
//! right). For the right side the anchor offset is rebuilt after every
//! content change so columns always pack against the workspace's right edge.

use std::rc::Rc;

use smithay::utils::{Logical, Point, Rectangle, Size};

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
/// caches the workspace's outer working area so it can rebuild the inner
/// ScrollingSpace's `parent_area` whenever the strip's content width changes
/// — which is how the right-side strip stays anchored to the workspace's
/// right edge while still using the carousel's column-width math.
#[derive(Debug)]
pub struct FixedStrip<W: LayoutElement> {
    /// Which workspace edge this strip is anchored to.
    side: FixedSide,

    /// Underlying scrolling layout, used as a column container only.
    /// `view_offset` is held at static zero — callers route column adds /
    /// removes through this wrapper so the strip never falls into the
    /// carousel's scroll/center code paths.
    inner: ScrollingSpace<W>,

    /// Cached workspace view size.
    view_size: Size<f64, Logical>,

    /// Cached outer working area (the workspace's `working_area`, already
    /// adjusted for layer-shell exclusive zones). Used as the base from
    /// which the side-specific `parent_area` is derived.
    outer_working_area: Rectangle<f64, Logical>,

    /// Cached output scale.
    scale: f64,

    /// Cached layout options.
    options: Rc<Options>,
}

impl<W: LayoutElement> FixedStrip<W> {
    pub fn new(
        side: FixedSide,
        view_size: Size<f64, Logical>,
        outer_working_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        let inner_parent_area =
            Self::compute_inner_parent_area(side, outer_working_area, 0.0);
        let inner =
            ScrollingSpace::new(view_size, inner_parent_area, scale, clock, options.clone());
        Self {
            side,
            inner,
            view_size,
            outer_working_area,
            scale,
            options,
        }
    }

    /// Derives the inner ScrollingSpace's `parent_area` from the outer
    /// workspace working area and this strip's current content width.
    ///
    /// For [`FixedSide::Left`] the parent area is unchanged from the outer
    /// working area — columns naturally lay out starting at the workspace's
    /// left edge.
    ///
    /// For [`FixedSide::Right`] the parent area is shifted right by
    /// `outer.width - content_width` so columns lay out flush against the
    /// workspace's right edge while still being sized against the full outer
    /// width (matching the carousel's column-width semantics).
    fn compute_inner_parent_area(
        side: FixedSide,
        outer: Rectangle<f64, Logical>,
        content_width: f64,
    ) -> Rectangle<f64, Logical> {
        match side {
            FixedSide::Left => outer,
            FixedSide::Right => {
                let offset = (outer.size.w - content_width).max(0.0);
                Rectangle::new(
                    Point::from((outer.loc.x + offset, outer.loc.y)),
                    outer.size,
                )
            }
        }
    }

    /// Rebuilds the inner ScrollingSpace's `parent_area` so a right-side
    /// strip stays anchored to the workspace's right edge after any change
    /// to content width. No-op for left-side strips. Resets the inner
    /// `view_offset` to zero afterwards so the carousel's
    /// `animate_view_offset_to_column` side effect inside `update_config`
    /// never bleeds into the strip.
    fn refresh_anchor(&mut self) {
        if self.side == FixedSide::Left {
            return;
        }
        let new_parent_area = Self::compute_inner_parent_area(
            self.side,
            self.outer_working_area,
            self.inner.content_width(),
        );
        self.inner.update_config(
            self.view_size,
            new_parent_area,
            self.scale,
            self.options.clone(),
        );
        self.inner.force_view_offset_zero();
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        outer_working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        self.view_size = view_size;
        self.outer_working_area = outer_working_area;
        self.scale = scale;
        self.options = options.clone();
        let inner_parent_area = Self::compute_inner_parent_area(
            self.side,
            outer_working_area,
            self.inner.content_width(),
        );
        self.inner
            .update_config(view_size, inner_parent_area, scale, options);
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

    /// Move focus one column to the left inside the strip, returning true on
    /// success. Skips ScrollingSpace's `activate_column` (which would trigger
    /// carousel scroll animation) by setting the active index statically.
    pub fn focus_left(&mut self) -> bool {
        let idx = self.inner.active_column_index();
        if idx == 0 {
            return false;
        }
        self.inner.set_active_column_idx_static(idx - 1);
        true
    }

    /// Mirror of [`focus_left`](Self::focus_left): move focus one column to
    /// the right inside the strip.
    pub fn focus_right(&mut self) -> bool {
        let idx = self.inner.active_column_index();
        if idx + 1 >= self.inner.column_count() {
            return false;
        }
        self.inner.set_active_column_idx_static(idx + 1);
        true
    }

    /// Focus the strip's innermost (carousel-facing) column. No-op if the
    /// strip is empty. Used when keyboard focus traverses INTO the strip from
    /// the carousel.
    pub fn focus_innermost(&mut self) -> bool {
        let n = self.inner.column_count();
        if n == 0 {
            return false;
        }
        let idx = match self.side {
            FixedSide::Left => n - 1,
            FixedSide::Right => 0,
        };
        self.inner.set_active_column_idx_static(idx);
        true
    }

    /// Returns the strip's active window, if any. Used by Workspace's
    /// active-window lookup once `active_fixed_side` indicates this strip is
    /// the focused layer.
    pub fn active_window(&self) -> Option<&W> {
        self.inner.active_window()
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        self.inner.active_window_mut()
    }

    /// Activate the column containing `window` (if any), making it the
    /// strip's focused column. Returns true on success. Keeps the inner
    /// `view_offset` clamped to zero so carousel-style scrolling never
    /// kicks in.
    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        let success = self.inner.activate_window(window);
        if success {
            self.inner.force_view_offset_zero();
        }
        success
    }

    /// Within-strip equivalent of
    /// [`ScrollingSpace::move_active_window_to_neighbor_column_as_new_row`].
    /// Mirrors the carousel's default cross-column stack-move semantic:
    /// extract the active tile and insert it as a new row in the neighbour
    /// column. Returns false at the strip's outer edge (for `to_left` on
    /// `fixed_left` or `!to_left` on `fixed_right`) so the caller can decide
    /// whether to keep the keypress as a no-op or cross to the next
    /// workspace.
    pub fn move_active_neighbor_as_new_row(&mut self, to_left: bool) -> bool {
        let result = self
            .inner
            .move_active_window_to_neighbor_column_as_new_row(to_left);
        self.inner.force_view_offset_zero();
        self.refresh_anchor();
        result
    }

    /// Whether the currently focused column inside this strip is the one
    /// closest to the carousel ("inner edge"). When true, a stack-move toward
    /// the carousel should hand the column back to it instead of moving
    /// within the strip.
    pub fn focused_column_is_at_inner_edge(&self) -> bool {
        let n = self.inner.column_count();
        if n == 0 {
            return false;
        }
        let active = self.inner.active_column_index();
        match self.side {
            FixedSide::Left => active == n - 1,
            FixedSide::Right => active == 0,
        }
    }

    /// Insert a column extracted from the carousel at this strip's inner
    /// (carousel-facing) edge. The new column becomes the focused column
    /// inside the strip.
    pub fn add_column_at_inner_edge(&mut self, column: Column<W>) {
        let insert_idx = match self.side {
            FixedSide::Left => self.inner.column_count(),
            FixedSide::Right => 0,
        };
        self.inner.add_column(Some(insert_idx), column, false, None);
        self.inner.set_active_column_idx_static(insert_idx);
        self.inner.force_view_offset_zero();
        self.refresh_anchor();
    }

    /// Remove the column at this strip's inner (carousel-facing) edge, ready
    /// to be inserted back into the carousel. Returns `None` if the strip is
    /// empty.
    pub fn remove_innermost_column(&mut self) -> Option<Column<W>> {
        let n = self.inner.column_count();
        if n == 0 {
            return None;
        }
        let idx = match self.side {
            FixedSide::Left => n - 1,
            FixedSide::Right => 0,
        };
        let column = self.inner.remove_column_by_idx(idx, None);
        self.inner.force_view_offset_zero();
        self.refresh_anchor();
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
        // Position is encoded in the inner ScrollingSpace's `parent_area`,
        // which `refresh_anchor` keeps up to date on the right side.
        self.inner.render(ctx, xray_pos, focus_ring, push);
    }
}
