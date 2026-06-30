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

use naru_ipc::{SizeChange, WindowLayout};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
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

    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> + '_ {
        self.inner.tiles_with_ipc_layouts()
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

/// Both fixed-side panels (left + right) for one output, plus the monitor-global
/// active-side signal and the cached geometry the strips need.
///
/// Panels are owned by the [`Monitor`](super::monitor::Monitor) (not the
/// workspace), so they render once per output, stay pinned to the screen during
/// a workspace switch, and persist across switches. `Monitor` composes the
/// cross-boundary carousel↔strip moves and focus traversal (which need the
/// active workspace's carousel) on top of the half-ops exposed here.
#[derive(Debug)]
pub struct FixedPanels<W: LayoutElement> {
    /// Fixed panel pinned to the left edge of the working area.
    fixed_left: FixedStrip<W>,

    /// Fixed panel pinned to the right edge of the working area.
    fixed_right: FixedStrip<W>,

    /// Which fixed-side panel currently owns keyboard focus, if any. Persists
    /// across workspace switches because the panels themselves do.
    active_fixed_side: Option<FixedSide>,

    /// Cached view size, working area, and scale for the strips. Updated by
    /// [`Self::update_config`] / [`Self::update_output_size`].
    view_size: Size<f64, Logical>,
    working_area: Rectangle<f64, Logical>,
    scale: f64,
}

impl<W: LayoutElement> FixedPanels<W> {
    pub fn new(
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        let fixed_left = FixedStrip::new(
            FixedSide::Left,
            view_size,
            working_area,
            scale,
            clock.clone(),
            options.clone(),
        );
        let fixed_right = FixedStrip::new(
            FixedSide::Right,
            view_size,
            working_area,
            scale,
            clock,
            options,
        );
        Self {
            fixed_left,
            fixed_right,
            active_fixed_side: None,
            view_size,
            working_area,
            scale,
        }
    }

    pub fn advance_animations(&mut self) {
        self.fixed_left.advance_animations();
        self.fixed_right.advance_animations();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.fixed_left.are_animations_ongoing() || self.fixed_right.are_animations_ongoing()
    }

    /// Update both strips' render elements. `focus` says which strip (if any)
    /// currently owns focus, so only that strip draws its active tile with the
    /// focused styling.
    pub fn update_render_elements(&mut self, focus: Option<FixedSide>) {
        self.fixed_left
            .update_render_elements(focus == Some(FixedSide::Left));
        self.fixed_right
            .update_render_elements(focus == Some(FixedSide::Right));
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        self.view_size = view_size;
        self.working_area = working_area;
        self.scale = scale;
        self.fixed_left
            .update_config(view_size, working_area, scale, options.clone());
        self.fixed_right
            .update_config(view_size, working_area, scale, options);
    }

    /// Mirror of [`Self::update_config`] for an output-size change: the strips
    /// track the working area too (e.g. when a layer-shell exclusive zone maps
    /// after the panels were created).
    pub fn update_output_size(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        self.update_config(view_size, working_area, scale, options);
    }

    pub fn left_width(&self) -> f64 {
        self.fixed_left.width()
    }

    pub fn right_width(&self) -> f64 {
        self.fixed_right.width()
    }

    pub fn has_window(&self, id: &W::Id) -> bool {
        self.fixed_left.has_window(id) || self.fixed_right.has_window(id)
    }

    /// The panel window owning `wl_surface` (or any of its descendant surfaces),
    /// if it lives in either fixed-side panel. Mirrors
    /// [`Workspace::find_wl_surface`](super::workspace::Workspace::find_wl_surface)
    /// so the commit path can resolve a panel window's output and queue a redraw
    /// for it, just like a carousel window.
    pub fn find_wl_surface(&self, wl_surface: &WlSurface) -> Option<&W> {
        self.tiles()
            .map(Tile::window)
            .find(|win| win.is_wl_surface(wl_surface))
    }

    pub fn find_wl_surface_mut(&mut self, wl_surface: &WlSurface) -> Option<&mut W> {
        self.tiles_mut()
            .map(Tile::window_mut)
            .find(|win| win.is_wl_surface(wl_surface))
    }

    pub fn side_with_window(&self, id: &W::Id) -> Option<FixedSide> {
        if self.fixed_left.has_window(id) {
            Some(FixedSide::Left)
        } else if self.fixed_right.has_window(id) {
            Some(FixedSide::Right)
        } else {
            None
        }
    }

    pub fn is_empty(&self, side: FixedSide) -> bool {
        match side {
            FixedSide::Left => self.fixed_left.is_empty(),
            FixedSide::Right => self.fixed_right.is_empty(),
        }
    }

    pub fn active_fixed_side(&self) -> Option<FixedSide> {
        self.active_fixed_side
    }

    pub fn set_active_fixed_side(&mut self, side: Option<FixedSide>) {
        self.active_fixed_side = side;
    }

    fn strip(&self, side: FixedSide) -> &FixedStrip<W> {
        match side {
            FixedSide::Left => &self.fixed_left,
            FixedSide::Right => &self.fixed_right,
        }
    }

    fn strip_mut(&mut self, side: FixedSide) -> &mut FixedStrip<W> {
        match side {
            FixedSide::Left => &mut self.fixed_left,
            FixedSide::Right => &mut self.fixed_right,
        }
    }

    fn strip_with_window_mut(&mut self, id: &W::Id) -> Option<&mut FixedStrip<W>> {
        self.side_with_window(id).map(move |side| self.strip_mut(side))
    }

    fn active_strip_mut(&mut self) -> Option<&mut FixedStrip<W>> {
        self.active_fixed_side.map(move |side| self.strip_mut(side))
    }

    pub fn columns(&self) -> impl Iterator<Item = &Column<W>> + '_ {
        self.fixed_left.columns().chain(self.fixed_right.columns())
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.fixed_left.tiles().chain(self.fixed_right.tiles())
    }

    /// Panel tiles paired with their IPC layout, for foreign-toplevel / the IPC
    /// `windows` listing. Panel windows are output-owned, so the caller pairs
    /// them with the monitor's active workspace id.
    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> + '_ {
        self.fixed_left
            .tiles_with_ipc_layouts()
            .chain(self.fixed_right.tiles_with_ipc_layouts())
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        self.fixed_left
            .tiles_mut()
            .chain(self.fixed_right.tiles_mut())
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> {
        self.fixed_left
            .tiles_with_render_positions()
            .chain(self.fixed_right.tiles_with_render_positions())
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        self.fixed_left
            .tiles_with_render_positions_mut(round)
            .chain(self.fixed_right.tiles_with_render_positions_mut(round))
    }

    /// The active window of the focused panel (per `active_fixed_side`). Unlike
    /// the workspace's old accessor there is no carousel fallback — that belongs
    /// to the Monitor now.
    pub fn active_window(&self) -> Option<&W> {
        match self.active_fixed_side {
            Some(side) => self.strip(side).active_window(),
            None => None,
        }
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        match self.active_fixed_side {
            Some(side) => self.strip_mut(side).active_window_mut(),
            None => None,
        }
    }

    pub fn active_tile_mut(&mut self) -> Option<&mut Tile<W>> {
        match self.active_fixed_side {
            Some(side) => self.strip_mut(side).active_tile_mut(),
            None => None,
        }
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        if let Some(rv) = self.fixed_left.window_under(pos) {
            return Some(rv);
        }
        self.fixed_right.window_under(pos)
    }

    pub fn popup_target_rect(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
        if self.fixed_left.has_window(id) {
            self.fixed_left.popup_target_rect(id)
        } else if self.fixed_right.has_window(id) {
            self.fixed_right.popup_target_rect(id)
        } else {
            None
        }
    }

    /// Apply a committed configure-ack to a panel window. Returns `true` if a
    /// strip owned the window.
    pub fn update_window(&mut self, id: &W::Id, serial: Option<Serial>) -> bool {
        self.fixed_left.update_window(id, serial) || self.fixed_right.update_window(id, serial)
    }

    pub fn refresh(&mut self, focus: Option<FixedSide>, is_focused: bool) {
        self.fixed_left
            .refresh(focus == Some(FixedSide::Left), is_focused);
        self.fixed_right
            .refresh(focus == Some(FixedSide::Right), is_focused);
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        self.fixed_left.start_open_animation(id) || self.fixed_right.start_open_animation(id)
    }

    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        id: &W::Id,
        blocker: TransactionBlocker,
    ) {
        if self.fixed_left.has_window(id) {
            self.fixed_left
                .start_close_animation_for_window(renderer, id, blocker);
        } else if self.fixed_right.has_window(id) {
            self.fixed_right
                .start_close_animation_for_window(renderer, id, blocker);
        }
    }

    /// Activate the panel window `id`, setting `active_fixed_side` to its side.
    /// Returns `true` if a strip owned the window. The caller clears
    /// `active_fixed_side` (and updates floating state) when no strip owns it.
    pub fn activate_window(&mut self, id: &W::Id) -> bool {
        if self.fixed_left.activate_window(id) {
            self.active_fixed_side = Some(FixedSide::Left);
            true
        } else if self.fixed_right.activate_window(id) {
            self.active_fixed_side = Some(FixedSide::Right);
            true
        } else {
            false
        }
    }

    /// Remove a panel window's tile, routing to the owning strip and clearing
    /// `active_fixed_side` when that strip empties. Returns `None` if neither
    /// strip owns `id`.
    pub fn remove_tile(&mut self, id: &W::Id, transaction: Transaction) -> Option<RemovedTile<W>> {
        if self.fixed_left.has_window(id) {
            let removed = self.fixed_left.remove_tile(id, transaction);
            if self.fixed_left.is_empty() && self.active_fixed_side == Some(FixedSide::Left) {
                self.active_fixed_side = None;
            }
            Some(removed)
        } else if self.fixed_right.has_window(id) {
            let removed = self.fixed_right.remove_tile(id, transaction);
            if self.fixed_right.is_empty() && self.active_fixed_side == Some(FixedSide::Right) {
                self.active_fixed_side = None;
            }
            Some(removed)
        } else {
            None
        }
    }

    pub fn remove_active_tile(&mut self, transaction: Transaction) -> Option<RemovedTile<W>> {
        match self.active_fixed_side {
            Some(FixedSide::Left) => {
                let removed = self.fixed_left.remove_active_tile(transaction)?;
                if self.fixed_left.is_empty() {
                    self.active_fixed_side = None;
                }
                Some(removed)
            }
            Some(FixedSide::Right) => {
                let removed = self.fixed_right.remove_active_tile(transaction)?;
                if self.fixed_right.is_empty() {
                    self.active_fixed_side = None;
                }
                Some(removed)
            }
            None => None,
        }
    }

    pub fn remove_active_column(&mut self) -> Option<Column<W>> {
        match self.active_fixed_side {
            Some(FixedSide::Left) => {
                let column = self.fixed_left.remove_active_column()?;
                if self.fixed_left.is_empty() {
                    self.active_fixed_side = None;
                }
                Some(column)
            }
            Some(FixedSide::Right) => {
                let column = self.fixed_right.remove_active_column()?;
                if self.fixed_right.is_empty() {
                    self.active_fixed_side = None;
                }
                Some(column)
            }
            None => None,
        }
    }

    /// Tiles parked in the fixed-side panels, paired with their side and their
    /// 0-based (column, tile) position within that strip. Used by session
    /// restore to persist side-panel placement.
    pub fn fixed_side_tiles(&self) -> Vec<(FixedSide, usize, usize, &Tile<W>)> {
        let mut out = Vec::new();
        for (side, strip) in [
            (FixedSide::Left, &self.fixed_left),
            (FixedSide::Right, &self.fixed_right),
        ] {
            for (col_idx, col) in strip.columns().enumerate() {
                for (tile_idx, (tile, _pos)) in col.tiles().enumerate() {
                    out.push((side, col_idx, tile_idx, tile));
                }
            }
        }
        out
    }

    // --- Per-window operations (by id, else active side) ---------------------

    /// `true` if a strip owns `id` and applied the change.
    pub fn set_fullscreen(&mut self, id: &W::Id, is_fullscreen: bool) -> bool {
        if let Some(strip) = self.strip_with_window_mut(id) {
            strip.set_fullscreen(id, is_fullscreen);
            true
        } else {
            false
        }
    }

    pub fn set_maximized(&mut self, id: &W::Id, maximize: bool) -> bool {
        if let Some(strip) = self.strip_with_window_mut(id) {
            strip.set_maximized(id, maximize);
            true
        } else {
            false
        }
    }

    /// Resolve the strip a per-window op should target: the owning strip for a
    /// `Some(id)`, else the active strip for `None`.
    fn op_strip_mut(&mut self, window: Option<&W::Id>) -> Option<&mut FixedStrip<W>> {
        match window {
            Some(id) => self.strip_with_window_mut(id),
            None => self.active_strip_mut(),
        }
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        if let Some(strip) = self.op_strip_mut(window) {
            strip.set_window_width(window, change);
        }
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        if let Some(strip) = self.op_strip_mut(window) {
            strip.set_window_height(window, change);
        }
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        if let Some(strip) = self.op_strip_mut(window) {
            strip.reset_window_height(window);
        }
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        if let Some(strip) = self.op_strip_mut(window) {
            strip.toggle_window_width(window, forwards);
        }
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        if let Some(strip) = self.op_strip_mut(window) {
            strip.toggle_window_height(window, forwards);
        }
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        if let Some(strip) = self.op_strip_mut(window) {
            strip.consume_or_expel_window_left(window);
        }
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        if let Some(strip) = self.op_strip_mut(window) {
            strip.consume_or_expel_window_right(window);
        }
    }

    pub fn consume_into_column(&mut self) {
        if let Some(strip) = self.active_strip_mut() {
            strip.consume_into_column();
        }
    }

    pub fn expel_from_column(&mut self) {
        if let Some(strip) = self.active_strip_mut() {
            strip.expel_from_column();
        }
    }

    pub fn swap_window_in_direction(&mut self, direction: ScrollDirection) {
        if let Some(strip) = self.active_strip_mut() {
            strip.swap_window_in_direction(direction);
        }
    }

    /// `true` if a strip owns `window` and began the resize.
    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        if let Some(strip) = self.strip_with_window_mut(&window) {
            strip.interactive_resize_begin(window, edges)
        } else {
            false
        }
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        if let Some(strip) = self.strip_with_window_mut(window) {
            strip.interactive_resize_update(window, delta)
        } else {
            false
        }
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        match window {
            Some(id) => {
                if let Some(strip) = self.strip_with_window_mut(id) {
                    strip.interactive_resize_end(Some(id));
                }
            }
            None => {
                self.fixed_left.interactive_resize_end(None);
                self.fixed_right.interactive_resize_end(None);
            }
        }
    }

    // --- Focus within the active panel ---------------------------------------

    pub fn focus_left_in_active(&mut self) -> bool {
        self.active_strip_mut().is_some_and(|s| s.focus_left())
    }

    pub fn focus_right_in_active(&mut self) -> bool {
        self.active_strip_mut().is_some_and(|s| s.focus_right())
    }

    pub fn focus_up_in_active(&mut self) -> bool {
        self.active_strip_mut().is_some_and(|s| s.focus_up())
    }

    pub fn focus_down_in_active(&mut self) -> bool {
        self.active_strip_mut().is_some_and(|s| s.focus_down())
    }

    /// Focus the innermost (carousel-facing) column of `side`. Returns `true`
    /// on success (the strip is non-empty); does NOT set `active_fixed_side`
    /// — the Monitor sets it when the cross-boundary hop succeeds.
    pub fn focus_innermost(&mut self, side: FixedSide) -> bool {
        self.strip_mut(side).focus_innermost()
    }

    // --- Cross-boundary half-ops (composed by Monitor) -----------------------

    pub fn add_column_at_inner_edge(&mut self, side: FixedSide, column: Column<W>) {
        self.strip_mut(side).add_column_at_inner_edge(column);
    }

    pub fn remove_innermost_column(&mut self, side: FixedSide) -> Option<Column<W>> {
        self.strip_mut(side).remove_innermost_column()
    }

    pub fn focused_column_is_at_inner_edge(&self, side: FixedSide) -> bool {
        self.strip(side).focused_column_is_at_inner_edge()
    }

    pub fn add_tile_at_inner_edge(
        &mut self,
        side: FixedSide,
        tile: Tile<W>,
        width: ColumnWidth,
        is_full_width: bool,
    ) {
        self.strip_mut(side)
            .add_tile_at_inner_edge(tile, width, is_full_width);
    }

    pub fn add_tile_right_of(
        &mut self,
        side: FixedSide,
        right_of: &W::Id,
        tile: Tile<W>,
        activate: bool,
        width: ColumnWidth,
        is_full_width: bool,
    ) {
        self.strip_mut(side)
            .add_tile_right_of(right_of, tile, activate, width, is_full_width);
    }

    pub fn move_active_neighbor_as_new_row(&mut self, side: FixedSide, to_left: bool) -> bool {
        self.strip_mut(side).move_active_neighbor_as_new_row(to_left)
    }

    // --- Render --------------------------------------------------------------

    pub fn render_left<R: NaruRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(ScrollingSpaceRenderElement<R>),
    ) {
        self.fixed_left.render(ctx, xray_pos, focus_ring, push);
    }

    pub fn render_right<R: NaruRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(ScrollingSpaceRenderElement<R>),
    ) {
        self.fixed_right.render(ctx, xray_pos, focus_ring, push);
    }

    #[cfg(test)]
    pub fn verify_invariants(&self) {
        self.fixed_left.verify_invariants();
        self.fixed_right.verify_invariants();

        match self.active_fixed_side {
            Some(FixedSide::Left) => assert!(
                !self.fixed_left.is_empty(),
                "active_fixed_side is Left but fixed_left is empty"
            ),
            Some(FixedSide::Right) => assert!(
                !self.fixed_right.is_empty(),
                "active_fixed_side is Right but fixed_right is empty"
            ),
            None => {}
        }
    }
}
