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
//! right) by pinning the inner ScrollingSpace's `view_offset` so the whole
//! column block parks flush against that edge — see [`FixedStrip::repin`].
//! The block stays put regardless of which column is focused, and the
//! right-side strip ends flush with the working area's right edge.

use std::rc::Rc;

use naru_ipc::SizeChange;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Serial, Size};

use super::scrolling::{
    Column, ColumnWidth, ScrollDirection, ScrollingSpace, ScrollingSpaceRenderElement,
};
use super::tile::Tile;
use super::{HitType, LayoutElement, Options, RemovedTile};
use crate::animation::Clock;
use crate::render_helpers::renderer::NaruRenderer;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;
use crate::utils::transaction::{Transaction, TransactionBlocker};
use crate::utils::ResizeEdge;

/// Which edge of the workspace working area this strip is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedSide {
    Left,
    Right,
}

/// A vertical strip of columns pinned to one edge of the workspace.
///
/// Wraps a `ScrollingSpace<W>` running with the carousel disabled. After every
/// content or focus change the wrapper re-pins the inner ScrollingSpace's view
/// offset (see [`FixedStrip::repin`]) so the column block stays anchored to the
/// strip's workspace edge — a right-side strip ends flush with the working
/// area's right edge — while still using the carousel's column-width math.
#[derive(Debug)]
pub struct FixedStrip<W: LayoutElement> {
    /// Which workspace edge this strip is anchored to.
    side: FixedSide,

    /// Underlying scrolling layout, used as a column container only.
    /// `view_offset` is pinned to a static edge anchor (see [`FixedStrip::
    /// repin`]) — callers route column adds / removes through this wrapper so
    /// the strip never falls into the carousel's scroll/center code paths.
    inner: ScrollingSpace<W>,
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
        // The inner ScrollingSpace gets the outer working area verbatim (same
        // as the carousel) for both sides; the strip's edge anchoring is done
        // by pinning the view offset in `repin`, not by shifting the parent
        // area. So column-width math matches the carousel exactly.
        let inner = ScrollingSpace::new(view_size, outer_working_area, scale, clock, options);
        Self { side, inner }
    }

    /// Re-pins the inner ScrollingSpace's view offset so the strip's column
    /// block stays flush against its anchored workspace edge (the right edge
    /// for a right strip, the left edge for a left strip). Called after every
    /// content or focus change because the underlying ScrollingSpace mutators
    /// reset or re-centre the view offset as a carousel-style side effect.
    fn repin(&mut self) {
        self.inner
            .pin_columns_to_edge(self.side == FixedSide::Right);
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        outer_working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        self.inner
            .update_config(view_size, outer_working_area, scale, options);
        self.repin();
    }

    pub fn side(&self) -> FixedSide {
        self.side
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Index of the existing column at the strip's inner (carousel-facing)
    /// edge. For a left strip the inner edge is the *last* column in the
    /// inner ScrollingSpace; for a right strip it is the *first*. `None`
    /// when the strip holds no columns. Callers reading an existing column
    /// use this; callers about to insert a new column at the inner edge
    /// should use [`Self::inner_edge_insert_idx`] instead.
    fn inner_edge_idx(&self) -> Option<usize> {
        let n = self.inner.column_count();
        if n == 0 {
            return None;
        }
        Some(match self.side {
            FixedSide::Left => n - 1,
            FixedSide::Right => 0,
        })
    }

    /// Insertion index that places a new column at the strip's inner edge,
    /// making it the new inner-edge column. For a left strip that's one
    /// past the current last column; for a right strip it's 0 (push every
    /// existing column one slot outward). Mirror of
    /// [`Self::inner_edge_idx`] for the insert-vs-read distinction.
    fn inner_edge_insert_idx(&self) -> usize {
        match self.side {
            FixedSide::Left => self.inner.column_count(),
            FixedSide::Right => 0,
        }
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
        self.repin();
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
        self.repin();
        true
    }

    /// Move focus one row up within the active column of the strip. Strips can
    /// hold multi-tile columns, so vertical keyboard focus has to be routed
    /// here when `active_fixed_side` points at this strip — otherwise it would
    /// fall through to the carousel. Re-pins the view so the inner
    /// ScrollingSpace's row-focus side effect can't bleed a horizontal scroll
    /// into the strip. Returns true if focus moved.
    pub fn focus_up(&mut self) -> bool {
        let moved = self.inner.focus_up();
        self.repin();
        moved
    }

    /// Mirror of [`focus_up`](Self::focus_up): move focus one row down within
    /// the strip's active column.
    pub fn focus_down(&mut self) -> bool {
        let moved = self.inner.focus_down();
        self.repin();
        moved
    }

    /// Hit-test a workspace-local point against this strip's windows, mirroring
    /// the carousel's [`ScrollingSpace::window_under`]. Returns `None` when the
    /// strip is empty or the point misses every tile, so the workspace can fall
    /// through to the next layer. The inner ScrollingSpace positions its tiles
    /// via the pinned view offset, so the point is already in the right
    /// coordinate space — no extra translation needed.
    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        if self.is_empty() {
            return None;
        }
        self.inner.window_under(pos)
    }

    /// Focus the strip's innermost (carousel-facing) column. No-op if the
    /// strip is empty. Used when keyboard focus traverses INTO the strip from
    /// the carousel.
    pub fn focus_innermost(&mut self) -> bool {
        let Some(idx) = self.inner_edge_idx() else {
            return false;
        };
        self.inner.set_active_column_idx_static(idx);
        self.repin();
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

    pub fn active_tile_mut(&mut self) -> Option<&mut Tile<W>> {
        self.inner.active_tile_mut()
    }

    /// Activate the column containing `window` (if any), making it the
    /// strip's focused column. Returns true on success. Re-pins the strip to
    /// its edge anchor afterwards so carousel-style scrolling never kicks in.
    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        let success = self.inner.activate_window(window);
        if success {
            self.repin();
        }
        success
    }

    /// Whether `window` lives in this strip — scans the full window stack of
    /// every tile, so a stacked-but-hidden window still counts as present.
    pub fn has_window(&self, window: &W::Id) -> bool {
        self.inner.columns().any(|col| col.contains(window))
    }

    /// Rect (in workspace-local coordinates) that the inner ScrollingSpace
    /// would use to unconstrain a popup spawned by `window`. Forwards to the
    /// inner space: it was constructed with the outer working_area verbatim,
    /// so its `parent_area.size.h` is the correct vertical span for popup
    /// flipping/sliding. Without this, `Workspace::popup_target_rect` falls
    /// through to `scrolling.popup_target_rect`, which returns `None` for any
    /// strip-hosted window — and the caller's `unwrap()` aborts the
    /// compositor.
    pub fn popup_target_rect(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
        self.inner.popup_target_rect(id)
    }

    /// Remove `window`'s tile from the strip. The caller is responsible for
    /// clearing `active_fixed_side` when the strip ends up empty. Keeps the
    /// strip pinned (view offset zeroed, right-side anchor refreshed) after
    /// the underlying column/tile bookkeeping runs.
    pub fn remove_tile(&mut self, window: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        let removed = self.inner.remove_tile(window, transaction);
        self.repin();
        removed
    }

    /// Remove the strip's focused tile. `None` when the strip is empty. The
    /// caller clears `active_fixed_side` once the strip empties.
    pub fn remove_active_tile(&mut self, transaction: Transaction) -> Option<RemovedTile<W>> {
        let removed = self.inner.remove_active_tile(transaction)?;
        self.repin();
        Some(removed)
    }

    /// Remove the strip's focused column. `None` when the strip is empty. The
    /// caller clears `active_fixed_side` once the strip empties.
    pub fn remove_active_column(&mut self) -> Option<Column<W>> {
        let column = self.inner.remove_active_column()?;
        self.repin();
        Some(column)
    }

    /// Apply a committed window update (configure-ack) to a strip window.
    /// Returns `false` (without side effects) when `window` is not in this
    /// strip, so the caller can fall through to the next layer — mirroring
    /// [`FloatingSpace::update_window`]'s contract. A configure-ack can
    /// change the window's size, so the strip is re-pinned afterwards.
    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) -> bool {
        if !self.has_window(window) {
            return false;
        }
        self.inner.update_window(window, serial);
        self.repin();
        true
    }

    // --- Per-window operations routed from the workspace ---------------------
    //
    // The carousel's by-window-id methods `unwrap()` on a window they don't
    // own, so the workspace must dispatch these to whichever strip holds the
    // window. Each mutating call re-pins the strip afterwards (view offset
    // zeroed, right-side anchor rebuilt) since column widths may have changed.

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        self.inner.set_window_width(window, change);
        self.repin();
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        self.inner.set_window_height(window, change);
        self.repin();
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        self.inner.reset_window_height(window);
        self.repin();
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        self.inner.toggle_window_width(window, forwards);
        self.repin();
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        self.inner.toggle_window_height(window, forwards);
        self.repin();
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) -> bool {
        let rv = self.inner.set_fullscreen(window, is_fullscreen);
        self.repin();
        rv
    }

    pub fn set_maximized(&mut self, window: &W::Id, maximize: bool) -> bool {
        let rv = self.inner.set_maximized(window, maximize);
        self.repin();
        rv
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        self.inner.interactive_resize_begin(window, edges)
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        let rv = self.inner.interactive_resize_update(window, delta);
        if rv {
            self.repin();
        }
        rv
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        self.inner.interactive_resize_end(window);
        self.repin();
    }

    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        self.inner
            .start_close_animation_for_window(renderer, window, blocker);
        self.repin();
    }

    /// Starts `window`'s open animation if it lives in this strip. Returns
    /// `false` (no side effects) otherwise, so the caller can fall through.
    pub fn start_open_animation(&mut self, window: &W::Id) -> bool {
        self.inner.start_open_animation(window)
    }

    /// Inserts `tile` as a new column immediately to the right of the column
    /// holding `right_of` (which must be a window in this strip).
    pub fn add_tile_right_of(
        &mut self,
        right_of: &W::Id,
        tile: Tile<W>,
        activate: bool,
        width: ColumnWidth,
        is_full_width: bool,
    ) {
        self.inner
            .add_tile_right_of(right_of, tile, activate, width, is_full_width);
        self.repin();
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        self.inner.consume_or_expel_window_left(window);
        self.repin();
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        self.inner.consume_or_expel_window_right(window);
        self.repin();
    }

    pub fn consume_into_column(&mut self) {
        self.inner.consume_into_column();
        self.repin();
    }

    pub fn expel_from_column(&mut self) {
        self.inner.expel_from_column();
        self.repin();
    }

    pub fn swap_window_in_direction(&mut self, direction: ScrollDirection) {
        self.inner.swap_window_in_direction(direction);
        self.repin();
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
        self.repin();
        result
    }

    /// Whether the currently focused column inside this strip is the one
    /// closest to the carousel ("inner edge"). When true, a stack-move toward
    /// the carousel should hand the column back to it instead of moving
    /// within the strip.
    pub fn focused_column_is_at_inner_edge(&self) -> bool {
        self.inner_edge_idx() == Some(self.inner.active_column_index())
    }

    /// Insert a column extracted from the carousel at this strip's inner
    /// (carousel-facing) edge. The new column becomes the focused column
    /// inside the strip.
    pub fn add_column_at_inner_edge(&mut self, column: Column<W>) {
        let insert_idx = self.inner_edge_insert_idx();
        self.inner.add_column(Some(insert_idx), column, false, None);
        self.inner.set_active_column_idx_static(insert_idx);
        self.repin();
    }

    /// Wrap `tile` in a fresh column at the strip's inner (carousel-facing)
    /// edge. Used by `open-in-fixed-side` to route a newly-opened window
    /// directly into a fixed-side panel instead of the carousel. Mirrors
    /// `add_column_at_inner_edge`, but skips having to build a Column up-front
    /// — `ScrollingSpace::add_tile` does that with the strip's own column-width
    /// math.
    pub fn add_tile_at_inner_edge(
        &mut self,
        tile: Tile<W>,
        width: ColumnWidth,
        is_full_width: bool,
    ) {
        let insert_idx = self.inner_edge_insert_idx();
        self.inner
            .add_tile(Some(insert_idx), tile, false, width, is_full_width, None);
        self.inner.set_active_column_idx_static(insert_idx);
        self.repin();
    }

    /// Remove the column at this strip's inner (carousel-facing) edge, ready
    /// to be inserted back into the carousel. Returns `None` if the strip is
    /// empty.
    pub fn remove_innermost_column(&mut self) -> Option<Column<W>> {
        let idx = self.inner_edge_idx()?;
        let column = self.inner.remove_column_by_idx(idx, None);
        self.repin();
        Some(column)
    }

    pub fn columns(&self) -> impl Iterator<Item = &Column<W>> + '_ {
        self.inner.columns()
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.inner.tiles()
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        self.inner.tiles_mut()
    }

    /// Tiles paired with their workspace-local render positions. The position
    /// is encoded by the inner ScrollingSpace's pinned view; callers (focus
    /// lookup, float-position seeding, invariant checks) need the strip's
    /// windows enumerated here just like the carousel's.
    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> {
        self.inner.tiles_with_render_positions()
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        self.inner.tiles_with_render_positions_mut(round)
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

    /// Propagate per-tile state (xdg `Activated`, configure throttling, etc.)
    /// the way the carousel does. Without this the strip's "active" tile never
    /// gets `set_activated(true)` and apps inside the strip never know they're
    /// focused — only the carousel ever flipped that flag.
    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        self.inner.refresh(is_active, is_focused);
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
        // Position is encoded in the inner ScrollingSpace's pinned view
        // offset, which `repin` keeps anchored to the strip's workspace edge.
        self.inner.render(ctx, xray_pos, focus_ring, push);
    }

    #[cfg(test)]
    pub fn verify_invariants(&self) {
        self.inner.verify_invariants();

        // A strip is a no-scroll container: its view offset is pinned at a
        // static edge anchor (carousel scroll/center code is never invoked).
        assert!(
            matches!(
                self.inner.view_offset(),
                super::scrolling::ViewOffset::Static(_)
            ),
            "fixed strip view offset must stay static",
        );
    }
}
