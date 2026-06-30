use std::cmp::{max, min};
use std::iter::{self, zip};
use std::rc::Rc;
use std::time::Duration;

use naru_config::utils::MergeWith as _;
use naru_config::{CenterFocusedColumn, PresetSize, Struts};
use naru_ipc::{ColumnDisplay, SizeChange, WindowLayout};
use ordered_float::NotNan;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Scale, Serial, Size};

use super::closing_window::{ClosingWindow, ClosingWindowRenderElement};
use super::monitor::InsertPosition;
use super::tab_indicator::{TabIndicator, TabIndicatorRenderElement, TabInfo};
use super::tile::{Tile, TileRenderElement, TileRenderSnapshot};
use super::workspace::{InteractiveResize, ResolvedSize};
use super::{ConfigureIntent, HitType, InteractiveResizeData, LayoutElement, Options, RemovedTile};
use crate::animation::{Animation, Clock};
use crate::input::swipe_tracker::SwipeTracker;
use crate::layout::SizingMode;
use crate::naru_render_elements;
use crate::render_helpers::renderer::NaruRenderer;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;
use crate::utils::transaction::{Transaction, TransactionBlocker};
use crate::utils::ResizeEdge;
use crate::window::ResolvedWindowRules;

/// Amount of touchpad movement to scroll the view for the width of one working area.
const VIEW_GESTURE_WORKING_AREA_MOVEMENT: f64 = 1200.;

/// A snapshot of a column's width-defining state, used to restore a column to its
/// previous width after it was temporarily grown to host a window being moved through it.
#[derive(Debug, Clone, Copy)]
struct WidthSnapshot {
    width: ColumnWidth,
    is_full_width: bool,
    preset_width_idx: Option<usize>,
}

/// Tracks a window's width as it is moved across columns, so the window keeps its size
/// and the columns it passes through are grown to fit and then restored.
///
/// Created when a window first starts moving (and re-created when a *different* window is
/// moved); cleared when window focus changes (see the `focus_*` methods). The moves
/// themselves carry focus with the window, so they don't clear it.
#[derive(Debug)]
struct MoveSizeMemory<W: LayoutElement> {
    /// The window being carried.
    window: W::Id,
    /// Its rendered window-width (px) at the start of the move sequence — the size to
    /// preserve as it travels.
    carried_width: f64,
    /// The pre-arrival width of the column the carried window most recently grew into, to
    /// restore when the window leaves it. `None` when the current host wasn't grown by us
    /// (it was already wide enough), so there's nothing to restore.
    host_prior: Option<WidthSnapshot>,
}

/// A scrollable-tiling space for windows.
#[derive(Debug)]
pub struct ScrollingSpace<W: LayoutElement> {
    /// Columns of windows on this space.
    columns: Vec<Column<W>>,

    /// Extra per-column data.
    data: Vec<ColumnData>,

    /// Index of the currently active column, if any.
    active_column_idx: usize,

    /// Ongoing interactive resize.
    interactive_resize: Option<InteractiveResize<W>>,

    /// Tracks the size of a window being moved across columns, so it keeps its width and
    /// the columns it passes through grow to fit then restore. Persists across a sequence
    /// of moves of the same window; cleared on focus change. See [`MoveSizeMemory`].
    move_size: Option<MoveSizeMemory<W>>,

    /// Set while a cross-column move (consume/expel) is mid-flight. The move
    /// re-activates columns internally — transiently landing focus on a tile
    /// that isn't the carried window — which would otherwise trip the
    /// focus-change clear in [`Self::activate_column_with_anim_config`] and drop
    /// [`Self::move_size`] before the move finishes. While this is set, that
    /// clear is suppressed so tracking survives the move's own activations.
    in_window_move: bool,

    /// Offset of the view computed from the active column.
    ///
    /// Any gaps, including left padding from work area left exclusive zone, is handled
    /// with this view offset (rather than added as a constant elsewhere in the code). This allows
    /// for natural handling of fullscreen windows, which must ignore work area padding.
    view_offset: ViewOffset,

    /// Whether to activate the previous, rather than the next, column upon column removal.
    ///
    /// When a new column is created and removed with no focus changes in-between, it is more
    /// natural to activate the previously-focused column. This variable tracks that.
    ///
    /// Since we only create-and-activate columns immediately to the right of the active column (in
    /// contrast to tabs in Firefox, for example), we can track this as a bool, rather than an
    /// index of the previous column to activate.
    ///
    /// The value is the view offset that the previous column had before, to restore it.
    activate_prev_column_on_removal: Option<f64>,

    /// View offset to restore after unfullscreening or unmaximizing.
    view_offset_to_restore: Option<f64>,

    /// Windows in the closing animation.
    closing_windows: Vec<ClosingWindow>,

    /// View size for this space.
    view_size: Size<f64, Logical>,

    /// Working area for this space.
    ///
    /// Takes into account layer-shell exclusive zones and naru struts.
    working_area: Rectangle<f64, Logical>,

    /// Working area for this space excluding struts.
    ///
    /// Used for popup unconstraining. Popups can go over struts, but they shouldn't go over
    /// the layer-shell top layer (which renders on top of popups).
    parent_area: Rectangle<f64, Logical>,

    /// Scale of the output the space is on (and rounds its sizes to).
    scale: f64,

    /// Clock for driving animations.
    clock: Clock,

    /// Configurable properties of the layout.
    options: Rc<Options>,
}

naru_render_elements! {
    ScrollingSpaceRenderElement<R> => {
        Tile = TileRenderElement<R>,
        ClosingWindow = ClosingWindowRenderElement,
        TabIndicator = TabIndicatorRenderElement,
    }
}

/// Extra per-column data.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ColumnData {
    /// Cached actual column width.
    width: f64,
}

#[derive(Debug)]
pub(super) enum ViewOffset {
    /// The view offset is static.
    Static(f64),
    /// The view offset is animating.
    Animation(Animation),
    /// The view offset is controlled by the ongoing gesture.
    Gesture(ViewGesture),
}

#[derive(Debug)]
pub(super) struct ViewGesture {
    current_view_offset: f64,
    /// Animation for the extra offset to the current position.
    ///
    /// For example, when we need to activate a specific window during a DnD scroll.
    animation: Option<Animation>,
    tracker: SwipeTracker,
    delta_from_tracker: f64,
    // The view offset we'll use if needed for activate_prev_column_on_removal.
    stationary_view_offset: f64,
    /// Whether the gesture is controlled by the touchpad.
    is_touchpad: bool,

    // If this gesture is for drag-and-drop scrolling, this is the last event's unadjusted
    // timestamp.
    dnd_last_event_time: Option<Duration>,
    // Time when the drag-and-drop scroll delta became non-zero, used for debouncing.
    //
    // If `None` then the scroll delta is currently zero.
    dnd_nonzero_start_time: Option<Duration>,
}

#[derive(Debug)]
pub struct Column<W: LayoutElement> {
    /// Tiles in this column.
    ///
    /// Must be non-empty.
    tiles: Vec<Tile<W>>,

    /// Extra per-tile data.
    ///
    /// Must have the same number of elements as `tiles`.
    data: Vec<TileData>,

    /// Index of the currently active tile.
    active_tile_idx: usize,

    /// Desired width of this column.
    ///
    /// If the column is full-width or full-screened, this is the width that should be restored
    /// upon unfullscreening and untoggling full-width.
    width: ColumnWidth,

    /// Currently selected preset width index.
    preset_width_idx: Option<usize>,

    /// `disable-carousel` only: a shared shrink factor in `(0, 1]` applied to
    /// the resolved column width so the whole non-scrolling row fits in the
    /// area between the fixed-side panels. `1.0` means "natural size". It is set
    /// uniformly across every column by
    /// [`ScrollingSpace::fit_columns_to_parent`] and never grows a column past
    /// its natural width. Kept separate from [`Self::width`] so the natural
    /// width is preserved and restored when space frees up.
    fit_scale: f64,

    /// Whether this column is full-width.
    is_full_width: bool,

    /// Whether this column is going to be fullscreen.
    ///
    /// This is the compositor-side fullscreen state, so it changes immediately upon
    /// set_fullscreen(). The actual tiles will take some time to respond to the fullscreen request
    /// and become fullscreen.
    ///
    /// Similarly, unsetting fullscreen will change this value to false immediately, and tiles will
    /// take some time to catch up and actually unfullscreen.
    is_pending_fullscreen: bool,

    /// Whether this column is going to be maximized.
    ///
    /// Can be `true` together with `is_pending_fullscreen`, which means that the column is
    /// effectively pending fullscreen, but unfullscreening should go back to maximized state,
    /// rather than normal.
    is_pending_maximized: bool,

    /// How this column displays and arranges windows.
    display_mode: ColumnDisplay,

    /// Tab indicator for the tabbed display mode.
    tab_indicator: TabIndicator,

    /// Animation of the render offset during window swapping.
    move_animation: Option<MoveAnimation>,

    /// Latest known view size for this column's workspace.
    view_size: Size<f64, Logical>,

    /// Latest known working area for this column's workspace.
    working_area: Rectangle<f64, Logical>,

    /// Working area for this column's workspace excluding struts.
    ///
    /// Used for maximize-to-edges.
    parent_area: Rectangle<f64, Logical>,

    /// Scale of the output the column is on (and rounds its sizes to).
    scale: f64,

    /// Clock for driving animations.
    clock: Clock,

    /// Configurable properties of the layout.
    options: Rc<Options>,
}

/// Extra per-tile data.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TileData {
    /// Requested height of the window.
    ///
    /// This is window height, not tile height, so it excludes tile decorations.
    height: WindowHeight,

    /// Cached actual size of the tile.
    size: Size<f64, Logical>,

    /// Cached whether the tile is being interactively resized by its left edge.
    interactively_resizing_by_left_edge: bool,
}

/// Width of a column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColumnWidth {
    /// Proportion of the current view width.
    Proportion(f64),
    /// Fixed width in logical pixels.
    Fixed(f64),
}

/// Height of a window in a column.
///
/// Every window but one in a column must be `Auto`-sized so that the total height can add up to
/// the workspace height. Resizing a window converts all other windows to `Auto`, weighted to
/// preserve their visual heights at the moment of the conversion.
///
/// In contrast to column widths, proportional height changes are converted to, and stored as,
/// fixed height right away. With column widths you frequently want e.g. two columns side-by-side
/// with 50% width each, and you want them to remain this way when moving to a differently sized
/// monitor. Windows in a column, however, already auto-size to fill the available height, giving
/// you this behavior. The main reason to set a different window height, then, is when you want
/// something in the window to fit exactly, e.g. to fit 30 lines in a terminal, which corresponds
/// to the `Fixed` variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowHeight {
    /// Automatically computed *tile* height, distributed across the column according to weights.
    ///
    /// This controls the tile height rather than the window height because it's easier in the auto
    /// height distribution algorithm.
    Auto { weight: f64 },
    /// Fixed *window* height in logical pixels.
    Fixed(f64),
    /// One of the preset heights (tile or window).
    Preset(usize),
}

/// Horizontal direction for an operation.
///
/// As operations often have a symmetrical counterpart, e.g. focus-right/focus-left, methods
/// on `Scrolling` can sometimes be factored using the direction of the operation as a parameter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollDirection {
    Left,
    Right,
}

#[derive(Debug)]
struct MoveAnimation {
    anim: Animation,
    from: f64,
}

impl<W: LayoutElement> ScrollingSpace<W> {
    pub fn new(
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        let working_area = compute_working_area(parent_area, scale, options.layout.struts);

        Self {
            columns: Vec::new(),
            data: Vec::new(),
            active_column_idx: 0,
            interactive_resize: None,
            move_size: None,
            in_window_move: false,
            view_offset: ViewOffset::Static(0.),
            activate_prev_column_on_removal: None,
            view_offset_to_restore: None,
            closing_windows: Vec::new(),
            view_size,
            working_area,
            parent_area,
            scale,
            clock,
            options,
        }
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let working_area = compute_working_area(parent_area, scale, options.layout.struts);

        for (column, data) in zip(&mut self.columns, &mut self.data) {
            column.update_config(view_size, working_area, parent_area, scale, options.clone());
            data.update(column);
        }

        self.view_size = view_size;
        self.working_area = working_area;
        self.parent_area = parent_area;
        self.scale = scale;
        self.options = options;

        // Apply always-center and such right away.
        if !self.columns.is_empty() && !self.view_offset.is_gesture() {
            self.animate_view_offset_to_column(None, self.active_column_idx, None);
            // Working area / column widths just changed — re-run the cluster centering so
            // multi-column layouts stay centered after a resize. animate_view_offset_to_column
            // above only re-centers the active column; without this call the auto-fit-or-center
            // target stays at the old (pre-resize) position until another layout change kicks it.
            self.auto_fit_or_center_view_offset();
        }
    }

    pub fn update_shaders(&mut self) {
        for col in &mut self.columns {
            col.update_shaders();
        }
    }

    pub fn advance_animations(&mut self) {
        if let ViewOffset::Animation(anim) = &self.view_offset {
            if anim.is_done() {
                self.view_offset = ViewOffset::Static(anim.to());
            }
        }

        if let ViewOffset::Gesture(gesture) = &mut self.view_offset {
            // Make sure the last event time doesn't go too much out of date (for
            // workspaces not under cursor), causing sudden jumps.
            //
            // This happens after any dnd_scroll_gesture_scroll() calls (in
            // Layout::advance_animations()), so it doesn't mess up the time delta there.
            if let Some(last_time) = &mut gesture.dnd_last_event_time {
                let now = self.clock.now_unadjusted();
                if *last_time != now {
                    *last_time = now;

                    // If last_time was already == now, then dnd_scroll_gesture_scroll() must've
                    // updated the gesture already. Therefore, when this code runs, the pointer
                    // must be outside the DnD scrolling zone.
                    gesture.dnd_nonzero_start_time = None;
                }
            }

            if let Some(anim) = &mut gesture.animation {
                if anim.is_done() {
                    gesture.animation = None;
                }
            }
        }

        for col in &mut self.columns {
            col.advance_animations();
        }

        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.view_offset.is_animation_ongoing()
            || self.columns.iter().any(Column::are_animations_ongoing)
            || !self.closing_windows.is_empty()
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        !self.view_offset.is_static()
            || self.columns.iter().any(Column::are_transitions_ongoing)
            || !self.closing_windows.is_empty()
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        let view_pos = Point::from((self.view_pos(), 0.));
        let view_size = self.view_size;
        let active_idx = self.active_column_idx;
        for (col_idx, (col, col_x)) in self.columns_mut().enumerate() {
            let is_active = is_active && col_idx == active_idx;
            let col_off = Point::from((col_x, 0.));
            let col_pos = view_pos - col_off - col.render_offset();
            let view_rect = Rectangle::new(col_pos, view_size);
            col.update_render_elements(is_active, view_rect);
        }
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.columns.iter().flat_map(|col| col.tiles.iter())
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        self.columns.iter_mut().flat_map(|col| col.tiles.iter_mut())
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    pub fn active_column_index(&self) -> usize {
        self.active_column_idx
    }

    /// Total logical width of all columns plus inter-column gaps.
    /// Returns zero when empty. Used by [`super::fixed_strip::FixedStrip`] to
    /// report how much horizontal space a panel occupies inside the workspace
    /// working area.
    pub fn content_width(&self) -> f64 {
        if self.columns.is_empty() {
            return 0.0;
        }
        let gaps = self.options.layout.gaps;
        let total_col_width: f64 = self.data.iter().map(|d| d.width).sum();
        total_col_width + (self.columns.len().saturating_sub(1)) as f64 * gaps
    }

    /// Like [`Self::content_width`], but using each column's *target* width
    /// (`Column::natural_width`) rather than its cached rendered width.
    ///
    /// The cached `data[i].width` follows the window's last *committed* size, so
    /// a window that committed smaller than its column (e.g. a client that
    /// hasn't grown into its tile yet) makes `content_width` report less than
    /// the space the column actually occupies in the row. The carousel's
    /// min-span floor cares about the columns' layout extent, not the
    /// momentarily-undersized content, so it uses this instead — otherwise an
    /// under-committed multi-column row gets spuriously inflated up to the floor.
    pub fn target_content_width(&self) -> f64 {
        if self.columns.is_empty() {
            return 0.0;
        }
        let gaps = self.options.layout.gaps;
        let total: f64 = self.columns.iter().map(|c| c.natural_width()).sum();
        total + (self.columns.len().saturating_sub(1)) as f64 * gaps
    }

    /// Resets the view offset to a static zero. Used by
    /// [`super::fixed_strip::FixedStrip`] after column insertion to neutralise
    /// any carousel-style scroll target that [`add_column`](Self::add_column)
    /// may have computed — strips never scroll, regardless of how many
    /// columns they hold.
    pub fn force_view_offset_zero(&mut self) {
        self.view_offset = ViewOffset::Static(0.0);
    }

    /// Pins the (non-scrolling) view so the whole column block sits flush
    /// against one edge of the working area. Used by [`super::fixed_strip::
    /// FixedStrip`], which never scrolls: instead of centring the active
    /// column the way the carousel does, a strip parks its entire content
    /// block at the workspace edge it is anchored to.
    ///
    /// With `anchor_right`, the rightmost column's right edge lands on the
    /// working area's right edge; otherwise the leftmost column's left edge
    /// lands on its left edge. The result is independent of which column is
    /// active, so moving focus between columns inside the strip doesn't shift
    /// the block. Tiles render at `column_x(i) - view_pos`, so pinning the
    /// leftmost column (`column_x == 0`) at `target_left` means
    /// `view_offset = -column_x(active) - target_left`.
    pub fn pin_columns_to_edge(&mut self, anchor_right: bool) {
        if self.columns.is_empty() {
            self.view_offset = ViewOffset::Static(0.0);
            return;
        }
        let active_col_x = self.column_x(self.active_column_idx);
        let target_left = if anchor_right {
            self.working_area.loc.x + self.working_area.size.w - self.content_width()
        } else {
            self.working_area.loc.x
        };
        self.view_offset = ViewOffset::Static(-active_col_x - target_left);
    }

    /// Sets the active column index without scroll animation. Used by
    /// [`super::fixed_strip::FixedStrip`] to keep "focused column inside the
    /// strip" bookkeeping consistent after column insertion / removal while
    /// avoiding the carousel's centre-on-column behaviour.
    pub fn set_active_column_idx_static(&mut self, idx: usize) {
        if idx < self.columns.len() {
            self.active_column_idx = idx;
            self.view_offset = ViewOffset::Static(0.0);
        }
    }

    pub fn active_window(&self) -> Option<&W> {
        if self.columns.is_empty() {
            return None;
        }

        let col = &self.columns[self.active_column_idx];
        Some(col.tiles[col.active_tile_idx].window())
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        if self.columns.is_empty() {
            return None;
        }

        let col = &mut self.columns[self.active_column_idx];
        Some(col.tiles[col.active_tile_idx].window_mut())
    }

    /// Number of tiles in the active column, or None if there's no active column.
    pub fn active_column_tile_count(&self) -> Option<usize> {
        let col = self.columns.get(self.active_column_idx)?;
        Some(col.tiles.len())
    }

    pub fn active_tile_mut(&mut self) -> Option<&mut Tile<W>> {
        if self.columns.is_empty() {
            return None;
        }

        let col = &mut self.columns[self.active_column_idx];
        Some(&mut col.tiles[col.active_tile_idx])
    }

    pub fn is_active_pending_fullscreen(&self) -> bool {
        if self.columns.is_empty() {
            return false;
        }

        let col = &self.columns[self.active_column_idx];
        col.pending_sizing_mode().is_fullscreen()
    }

    pub fn new_window_toplevel_bounds(&self, rules: &ResolvedWindowRules) -> Size<i32, Logical> {
        let border_config = self.options.layout.border.merged_with(&rules.border);

        let display_mode = rules
            .default_column_display
            .unwrap_or(self.options.layout.default_column_display);
        let will_tab = display_mode == ColumnDisplay::Tabbed;
        let extra_size = if will_tab {
            TabIndicator::new(self.options.layout.tab_indicator).extra_size(1, self.scale)
        } else {
            Size::from((0., 0.))
        };

        compute_toplevel_bounds(
            border_config,
            self.working_area.size,
            extra_size,
            self.options.layout.gaps,
        )
    }

    pub fn new_window_size(
        &self,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        let border = self.options.layout.border.merged_with(&rules.border);

        let display_mode = rules
            .default_column_display
            .unwrap_or(self.options.layout.default_column_display);
        let will_tab = display_mode == ColumnDisplay::Tabbed;
        let extra = if will_tab {
            TabIndicator::new(self.options.layout.tab_indicator).extra_size(1, self.scale)
        } else {
            Size::from((0., 0.))
        };

        let working_size = self.working_area.size;

        let width = if let Some(size) = width {
            let size = match resolve_preset_size(size, &self.options, working_size.w, extra.w) {
                ResolvedSize::Tile(mut size) => {
                    if !border.off {
                        size -= border.width * 2.;
                    }
                    size
                }
                ResolvedSize::Window(size) => size,
            };

            max(1, size.floor() as i32)
        } else {
            0
        };

        let mut full_height = self.working_area.size.h - self.options.layout.gaps * 2.;
        if !border.off {
            full_height -= border.width * 2.;
        }

        let height = if let Some(height) = height {
            let height = match resolve_preset_size(height, &self.options, working_size.h, extra.h) {
                ResolvedSize::Tile(mut size) => {
                    if !border.off {
                        size -= border.width * 2.;
                    }
                    size
                }
                ResolvedSize::Window(size) => size,
            };
            f64::min(height, full_height)
        } else {
            full_height
        };

        Size::from((width, max(height.floor() as i32, 1)))
    }

    pub fn is_centering_focused_column(&self) -> bool {
        self.options.layout.center_focused_column == CenterFocusedColumn::Always
            || (self.options.layout.always_center_single_column && self.columns.len() <= 1)
    }

    fn compute_new_view_offset_fit(
        &self,
        target_x: Option<f64>,
        col_x: f64,
        width: f64,
        mode: SizingMode,
    ) -> f64 {
        if mode.is_fullscreen() {
            return 0.;
        }

        let (area, padding) = if mode.is_maximized() {
            (self.parent_area, 0.)
        } else {
            (self.working_area, self.options.layout.gaps)
        };

        let target_x = target_x.unwrap_or_else(|| self.target_view_pos());

        let new_offset =
            compute_new_view_offset(target_x + area.loc.x, area.size.w, col_x, width, padding);

        // Non-fullscreen windows are always offset at least by the working area position.
        new_offset - area.loc.x
    }

    fn compute_new_view_offset_centered(
        &self,
        target_x: Option<f64>,
        col_x: f64,
        width: f64,
        mode: SizingMode,
    ) -> f64 {
        if mode.is_fullscreen() {
            return self.compute_new_view_offset_fit(target_x, col_x, width, mode);
        }

        let area = if mode.is_maximized() {
            self.parent_area
        } else {
            self.working_area
        };

        // Columns wider than the view are left-aligned (the fit code can deal with that).
        if area.size.w <= width {
            return self.compute_new_view_offset_fit(target_x, col_x, width, mode);
        }

        -(area.size.w - width) / 2. - area.loc.x
    }

    fn compute_new_view_offset_for_column_fit(&self, target_x: Option<f64>, idx: usize) -> f64 {
        let col = &self.columns[idx];
        self.compute_new_view_offset_fit(
            target_x,
            self.column_x(idx),
            col.width(),
            col.sizing_mode(),
        )
    }

    fn compute_new_view_offset_for_column_centered(
        &self,
        target_x: Option<f64>,
        idx: usize,
    ) -> f64 {
        let col = &self.columns[idx];
        self.compute_new_view_offset_centered(
            target_x,
            self.column_x(idx),
            col.width(),
            col.sizing_mode(),
        )
    }

    fn compute_new_view_offset_for_column(
        &self,
        target_x: Option<f64>,
        idx: usize,
        prev_idx: Option<usize>,
    ) -> f64 {
        // In disable-carousel mode the row never scrolls: every "scroll to a
        // column" op resolves to the same centered resting position, so
        // resizing or activating a column can't fight the row centering (which
        // would otherwise leave the row a little off-center after a resize).
        if self.options.layout.disable_carousel {
            if let Some(view_pos) = self.compute_centered_view_pos() {
                return view_pos - self.column_x(idx);
            }
        }

        if self.is_centering_focused_column() {
            return self.compute_new_view_offset_for_column_centered(target_x, idx);
        }

        match self.options.layout.center_focused_column {
            CenterFocusedColumn::Always => {
                self.compute_new_view_offset_for_column_centered(target_x, idx)
            }
            CenterFocusedColumn::OnOverflow => {
                let Some(prev_idx) = prev_idx else {
                    return self.compute_new_view_offset_for_column_fit(target_x, idx);
                };

                // Activating the same column.
                if prev_idx == idx {
                    return self.compute_new_view_offset_for_column_fit(target_x, idx);
                }

                // Always take the left or right neighbor of the target as the source.
                let source_idx = if prev_idx > idx {
                    min(idx + 1, self.columns.len() - 1)
                } else {
                    idx.saturating_sub(1)
                };

                let source_col_x = self.column_x(source_idx);
                let source_col_width = self.columns[source_idx].width();

                let target_col_x = self.column_x(idx);
                let target_col_width = self.columns[idx].width();

                // NOTE: This logic won't work entirely correctly with small fixed-size maximized
                // windows (they have a different area and padding).
                let total_width = if source_col_x < target_col_x {
                    // Source is left from target.
                    target_col_x - source_col_x + target_col_width
                } else {
                    // Source is right from target.
                    source_col_x - target_col_x + source_col_width
                } + self.options.layout.gaps * 2.;

                // If it fits together, do a normal animation, otherwise center the new column.
                if total_width <= self.working_area.size.w {
                    self.compute_new_view_offset_for_column_fit(target_x, idx)
                } else {
                    self.compute_new_view_offset_for_column_centered(target_x, idx)
                }
            }
            CenterFocusedColumn::Never => {
                self.compute_new_view_offset_for_column_fit(target_x, idx)
            }
        }
    }

    fn animate_view_offset(&mut self, idx: usize, new_view_offset: f64) {
        self.animate_view_offset_with_config(
            idx,
            new_view_offset,
            self.options.animations.horizontal_view_movement.0,
        );
    }

    fn animate_view_offset_with_config(
        &mut self,
        idx: usize,
        new_view_offset: f64,
        config: naru_config::Animation,
    ) {
        let new_col_x = self.column_x(idx);
        let old_col_x = self.column_x(self.active_column_idx);
        let offset_delta = old_col_x - new_col_x;
        self.view_offset.offset(offset_delta);

        let pixel = 1. / self.scale;

        // If our view offset is already this or animating towards this, we don't need to do
        // anything.
        let to_diff = new_view_offset - self.view_offset.target();
        if to_diff.abs() < pixel {
            // Correct for any inaccuracy.
            self.view_offset.offset(to_diff);
            return;
        }

        match &mut self.view_offset {
            ViewOffset::Gesture(gesture) if gesture.dnd_last_event_time.is_some() => {
                gesture.stationary_view_offset = new_view_offset;

                let current_pos = gesture.current_view_offset - gesture.delta_from_tracker;
                gesture.delta_from_tracker = new_view_offset - current_pos;
                let offset_delta = new_view_offset - gesture.current_view_offset;
                gesture.current_view_offset = new_view_offset;

                gesture.animate_from(-offset_delta, self.clock.clone(), config);
            }
            _ => {
                // FIXME: also compute and use current velocity.
                self.view_offset = ViewOffset::Animation(Animation::new(
                    self.clock.clone(),
                    self.view_offset.current(),
                    new_view_offset,
                    0.,
                    config,
                ));
            }
        }
    }

    fn animate_view_offset_to_column_centered(
        &mut self,
        target_x: Option<f64>,
        idx: usize,
        config: naru_config::Animation,
    ) {
        let new_view_offset = self.compute_new_view_offset_for_column_centered(target_x, idx);
        self.animate_view_offset_with_config(idx, new_view_offset, config);
    }

    fn animate_view_offset_to_column_with_config(
        &mut self,
        target_x: Option<f64>,
        idx: usize,
        prev_idx: Option<usize>,
        config: naru_config::Animation,
    ) {
        let new_view_offset = self.compute_new_view_offset_for_column(target_x, idx, prev_idx);
        self.animate_view_offset_with_config(idx, new_view_offset, config);
    }

    fn animate_view_offset_to_column(
        &mut self,
        target_x: Option<f64>,
        idx: usize,
        prev_idx: Option<usize>,
    ) {
        self.animate_view_offset_to_column_with_config(
            target_x,
            idx,
            prev_idx,
            self.options.animations.horizontal_view_movement.0,
        )
    }

    fn activate_column(&mut self, idx: usize) {
        self.activate_column_with_anim_config(
            idx,
            self.options.animations.horizontal_view_movement.0,
        );
    }

    fn activate_column_with_anim_config(&mut self, idx: usize, config: naru_config::Animation) {
        // End move-size tracking when focus moves to a *different* window. A window move
        // re-activates the moved window itself (so the id still matches and tracking
        // survives the move), but any other focus change — focus-left/right, clicking a
        // different window — lands on a different active window and clears it. Checked
        // before the early-return so same-column re-activations are covered too.
        if !self.in_window_move {
            if let Some(ms) = &self.move_size {
                let now_focused = self
                    .columns
                    .get(idx)
                    .and_then(|c| c.tiles.get(c.active_tile_idx))
                    .map(|t| t.window().id());
                if now_focused != Some(&ms.window) {
                    self.move_size = None;
                }
            }
        }

        if self.active_column_idx == idx
            // During a DnD scroll, animate even when activating the same window, for DnD hold.
            && (self.columns.is_empty() || !self.view_offset.is_dnd_scroll())
        {
            return;
        }

        self.animate_view_offset_to_column_with_config(
            None,
            idx,
            Some(self.active_column_idx),
            config,
        );

        if self.active_column_idx != idx {
            self.active_column_idx = idx;

            // A different column was activated; reset the flag.
            self.activate_prev_column_on_removal = None;
            self.view_offset_to_restore = None;
            self.interactive_resize = None;
        }
    }

    pub(super) fn insert_position(&self, pos: Point<f64, Logical>) -> InsertPosition {
        if self.columns.is_empty() {
            return InsertPosition::NewColumn(0);
        }

        let x = pos.x + self.view_pos();

        // Aim for the center of the gap.
        let x = x + self.options.layout.gaps / 2.;
        let y = pos.y + self.options.layout.gaps / 2.;

        // Insert position is before the first column.
        if x < 0. {
            return InsertPosition::NewColumn(0);
        }

        // Find the closest gap between columns.
        let (closest_col_idx, col_x) = self
            .column_xs(self.data.iter().copied())
            .enumerate()
            .min_by_key(|(_, col_x)| NotNan::new((col_x - x).abs()).unwrap())
            .unwrap();

        // Find the column containing the position.
        let (col_idx, _) = self
            .column_xs(self.data.iter().copied())
            .enumerate()
            .take_while(|(_, col_x)| *col_x <= x)
            .last()
            .unwrap_or((0, 0.));

        // Insert position is past the last column.
        if col_idx == self.columns.len() {
            return InsertPosition::NewColumn(closest_col_idx);
        }

        // Find the closest gap between tiles.
        let col = &self.columns[col_idx];

        let (closest_tile_idx, tile_y) = if col.display_mode == ColumnDisplay::Tabbed {
            // In tabbed mode, there's only one tile visible, and we want to check its top and
            // bottom.
            let top = col.tile_offsets().nth(col.active_tile_idx).unwrap().y;
            let bottom = top + col.data[col.active_tile_idx].size.h;
            if (top - y).abs() <= (bottom - y).abs() {
                (col.active_tile_idx, top)
            } else {
                (col.active_tile_idx + 1, bottom)
            }
        } else {
            col.tile_offsets()
                .map(|tile_off| tile_off.y)
                .enumerate()
                .min_by_key(|(_, tile_y)| NotNan::new((tile_y - y).abs()).unwrap())
                .unwrap()
        };

        // Return the closest among the vertical and the horizontal gap.
        let vert_dist = (col_x - x).abs();
        let hor_dist = (tile_y - y).abs();
        if vert_dist <= hor_dist {
            InsertPosition::NewColumn(closest_col_idx)
        } else {
            InsertPosition::InColumn(col_idx, closest_tile_idx)
        }
    }

    pub fn add_tile(
        &mut self,
        col_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
        width: ColumnWidth,
        is_full_width: bool,
        anim_config: Option<naru_config::Animation>,
    ) {
        let column = Column::new_with_tile(
            tile,
            self.view_size,
            self.working_area,
            self.parent_area,
            self.scale,
            width,
            is_full_width,
        );

        self.add_column(col_idx, column, activate, anim_config);
    }

    pub fn add_tile_to_column(
        &mut self,
        col_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
    ) {
        let prev_next_x = self.column_x(col_idx + 1);

        let target_column = &mut self.columns[col_idx];
        let tile_idx = tile_idx.unwrap_or(target_column.tiles.len());
        let mut prev_active_tile_idx = target_column.active_tile_idx;

        target_column.add_tile_at(tile_idx, tile);
        self.data[col_idx].update(target_column);

        if tile_idx <= prev_active_tile_idx {
            target_column.active_tile_idx += 1;
            prev_active_tile_idx += 1;
        }

        if activate {
            target_column.activate_idx(tile_idx);
            if self.active_column_idx != col_idx {
                self.activate_column(col_idx);
            }
        }

        let target_column = &mut self.columns[col_idx];
        if target_column.display_mode == ColumnDisplay::Tabbed {
            if target_column.active_tile_idx == tile_idx {
                // Fade out the previously active tile.
                let tile = &mut target_column.tiles[prev_active_tile_idx];
                tile.animate_alpha(1., 0., self.options.animations.window_movement.0);
            } else {
                // Fade out when adding into a tabbed column into the background.
                let tile = &mut target_column.tiles[tile_idx];
                tile.animate_alpha(1., 0., self.options.animations.window_movement.0);
            }
        }

        // Adding a wider window into a column increases its width now (even if the window will
        // shrink later). Move the columns to account for this.
        let offset = self.column_x(col_idx + 1) - prev_next_x;
        if self.active_column_idx <= col_idx {
            for col in &mut self.columns[col_idx + 1..] {
                col.animate_move_from(-offset);
            }
        } else {
            for col in &mut self.columns[..=col_idx] {
                col.animate_move_from(offset);
            }
        }
    }

    pub fn add_tile_right_of(
        &mut self,
        right_of: &W::Id,
        tile: Tile<W>,
        activate: bool,
        width: ColumnWidth,
        is_full_width: bool,
    ) {
        let right_of_idx = self
            .columns
            .iter()
            .position(|col| col.contains(right_of))
            .unwrap();
        let col_idx = right_of_idx + 1;

        self.add_tile(Some(col_idx), tile, activate, width, is_full_width, None);
    }

    pub fn add_column(
        &mut self,
        idx: Option<usize>,
        mut column: Column<W>,
        activate: bool,
        anim_config: Option<naru_config::Animation>,
    ) {
        let was_empty = self.columns.is_empty();

        let idx = idx.unwrap_or_else(|| {
            if was_empty {
                0
            } else {
                self.active_column_idx + 1
            }
        });

        column.update_config(
            self.view_size,
            self.working_area,
            self.parent_area,
            self.scale,
            self.options.clone(),
        );
        self.data.insert(idx, ColumnData::new(&column));
        self.columns.insert(idx, column);

        if !was_empty && idx <= self.active_column_idx {
            self.active_column_idx += 1;
        }

        // Animate movement of other columns.
        let offset = self.column_x(idx + 1) - self.column_x(idx);
        let config = anim_config.unwrap_or(self.options.animations.window_movement.0);
        if self.active_column_idx <= idx {
            for col in &mut self.columns[idx + 1..] {
                col.animate_move_from_with_config(-offset, config);
            }
        } else {
            for col in &mut self.columns[..idx] {
                col.animate_move_from_with_config(offset, config);
            }
        }

        if activate {
            // If this is the first window on an empty workspace, remove the effect of whatever
            // view_offset was left over and skip the animation. If auto_fit_or_center is
            // enabled, jump straight to the centered target — otherwise the subsequent
            // auto_fit_or_center_view_offset call would animate the view from this fit
            // position to the centered position, making the column appear to slide in from
            // the left.
            if was_empty {
                self.view_offset = ViewOffset::Static(0.);
                let target = self
                    .compute_auto_fit_or_center_view_offset()
                    .unwrap_or_else(|| {
                        self.compute_new_view_offset_for_column(None, idx, None)
                    });
                self.view_offset = ViewOffset::Static(target);
            }

            let prev_offset = (!was_empty && idx == self.active_column_idx + 1)
                .then(|| self.view_offset.stationary());

            let anim_config =
                anim_config.unwrap_or(self.options.animations.horizontal_view_movement.0);
            self.activate_column_with_anim_config(idx, anim_config);
            self.activate_prev_column_on_removal = prev_offset;
        }

        self.auto_fit_or_center_view_offset();
    }

    pub fn remove_active_tile(&mut self, transaction: Transaction) -> Option<RemovedTile<W>> {
        if self.columns.is_empty() {
            return None;
        }

        let column = &self.columns[self.active_column_idx];
        Some(self.remove_tile_by_idx(
            self.active_column_idx,
            column.active_tile_idx,
            transaction,
            None,
        ))
    }

    pub fn remove_tile(&mut self, window: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        let column_idx = self
            .columns
            .iter()
            .position(|col| col.contains(window))
            .unwrap();
        let column = &self.columns[column_idx];

        let tile_idx = column.position(window).unwrap();
        self.remove_tile_by_idx(column_idx, tile_idx, transaction, None)
    }

    pub fn remove_tile_by_idx(
        &mut self,
        column_idx: usize,
        tile_idx: usize,
        transaction: Transaction,
        anim_config: Option<naru_config::Animation>,
    ) -> RemovedTile<W> {
        // If this is the only tile in the column, remove the whole column.
        if self.columns[column_idx].tiles.len() == 1 {
            let mut column = self.remove_column_by_idx(column_idx, anim_config);
            return RemovedTile {
                tile: column.tiles.remove(tile_idx),
                width: column.width,
                is_full_width: column.is_full_width,
                is_floating: false,
            };
        }

        let column = &mut self.columns[column_idx];
        let prev_width = self.data[column_idx].width;

        let movement_config = anim_config.unwrap_or(self.options.animations.window_movement.0);

        // Animate movement of other tiles.
        // FIXME: tiles can move by X too, in a centered or resizing layout with one window smaller
        // than the others.
        let offset_y = column.tile_offset(tile_idx + 1).y - column.tile_offset(tile_idx).y;
        for tile in &mut column.tiles[tile_idx + 1..] {
            tile.animate_move_y_from(offset_y);
        }

        if column.display_mode == ColumnDisplay::Tabbed && tile_idx != column.active_tile_idx {
            // Fade in when removing background tab from a tabbed column.
            let tile = &mut column.tiles[tile_idx];
            tile.animate_alpha(0., 1., movement_config);
        }

        let was_normal = column.sizing_mode().is_normal();

        let tile = column.tiles.remove(tile_idx);
        column.data.remove(tile_idx);

        // If an active column became non-fullscreen after removing the tile, clear the stored
        // unfullscreen offset.
        if column_idx == self.active_column_idx && !was_normal && column.sizing_mode().is_normal() {
            self.view_offset_to_restore = None;
        }

        // If one window is left, reset its weight to 1.
        if column.data.len() == 1 {
            if let WindowHeight::Auto { weight } = &mut column.data[0].height {
                *weight = 1.;
            }
        }

        // Stop interactive resize.
        if let Some(resize) = &self.interactive_resize {
            if tile.window().id() == &resize.window {
                self.interactive_resize = None;
            }
        }

        let tile = RemovedTile {
            tile,
            width: column.width,
            is_full_width: column.is_full_width,
            is_floating: false,
        };

        #[allow(clippy::comparison_chain)] // What do you even want here?
        if tile_idx < column.active_tile_idx {
            // A tile above was removed; preserve the current position.
            column.active_tile_idx -= 1;
        } else if tile_idx == column.active_tile_idx {
            // The active tile was removed, so the active tile index shifted to the next tile.
            if tile_idx == column.tiles.len() {
                // The bottom tile was removed and it was active, update active idx to remain valid.
                column.activate_idx(tile_idx - 1);
            } else {
                // Ensure the newly active tile animates to opaque.
                column.tiles[tile_idx].ensure_alpha_animates_to_1();
            }
        }

        column.update_tile_sizes_with_transaction(true, transaction);
        self.data[column_idx].update(column);
        let offset = prev_width - column.width();

        // Animate movement of the other columns.
        if self.active_column_idx <= column_idx {
            for col in &mut self.columns[column_idx + 1..] {
                col.animate_move_from_with_config(offset, movement_config);
            }
        } else {
            for col in &mut self.columns[..=column_idx] {
                col.animate_move_from_with_config(-offset, movement_config);
            }
        }

        tile
    }

    pub fn remove_active_column(&mut self) -> Option<Column<W>> {
        if self.columns.is_empty() {
            return None;
        }

        Some(self.remove_column_by_idx(self.active_column_idx, None))
    }

    pub fn remove_column_by_idx(
        &mut self,
        column_idx: usize,
        anim_config: Option<naru_config::Animation>,
    ) -> Column<W> {
        // Animate movement of the other columns.
        let movement_config = anim_config.unwrap_or(self.options.animations.window_movement.0);
        let offset = self.column_x(column_idx + 1) - self.column_x(column_idx);
        if self.active_column_idx <= column_idx {
            for col in &mut self.columns[column_idx + 1..] {
                col.animate_move_from_with_config(offset, movement_config);
            }
        } else {
            for col in &mut self.columns[..column_idx] {
                col.animate_move_from_with_config(-offset, movement_config);
            }
        }

        let column = self.columns.remove(column_idx);
        self.data.remove(column_idx);

        // Stop interactive resize.
        if let Some(resize) = &self.interactive_resize {
            if column
                .tiles
                .iter()
                .any(|tile| tile.window().id() == &resize.window)
            {
                self.interactive_resize = None;
            }
        }

        if column_idx + 1 == self.active_column_idx {
            // The previous column, that we were going to activate upon removal of the active
            // column, has just been itself removed.
            self.activate_prev_column_on_removal = None;
        }

        if column_idx == self.active_column_idx {
            self.view_offset_to_restore = None;
        }

        if self.columns.is_empty() {
            return column;
        }

        let view_config = anim_config.unwrap_or(self.options.animations.horizontal_view_movement.0);

        if column_idx < self.active_column_idx {
            // A column to the left was removed; preserve the current position.
            // FIXME: preserve activate_prev_column_on_removal.
            self.active_column_idx -= 1;
            self.activate_prev_column_on_removal = None;
        } else if column_idx == self.active_column_idx
            && self.activate_prev_column_on_removal.is_some()
        {
            // The active column was removed, and we needed to activate the previous column.
            if 0 < column_idx {
                let prev_offset = self.activate_prev_column_on_removal.unwrap();

                self.activate_column_with_anim_config(self.active_column_idx - 1, view_config);

                // Restore the view offset but make sure to scroll the view in case the
                // previous window had resized.
                self.animate_view_offset_with_config(
                    self.active_column_idx,
                    prev_offset,
                    view_config,
                );
                self.animate_view_offset_to_column_with_config(
                    None,
                    self.active_column_idx,
                    None,
                    view_config,
                );
            }
        } else {
            self.activate_column_with_anim_config(
                min(self.active_column_idx, self.columns.len() - 1),
                view_config,
            );
        }

        self.auto_fit_or_center_view_offset();

        column
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        let (col_idx, column) = self
            .columns
            .iter_mut()
            .enumerate()
            .find(|(_, col)| col.contains(window))
            .unwrap();
        let was_normal = column.sizing_mode().is_normal();
        let prev_origin = column.tiles_origin();

        let (tile_idx, tile) = column
            .tiles
            .iter_mut()
            .enumerate()
            .find(|(_, tile)| tile.window().id() == window)
            .unwrap();

        let resize = tile.window_mut().interactive_resize_data();

        // Do this before calling update_window() so it can get up-to-date info.
        if let Some(serial) = serial {
            tile.window_mut().on_commit(serial);
        }

        let prev_width = self.data[col_idx].width;

        column.update_window(window);
        self.data[col_idx].update(column);
        column.update_tile_sizes(false);

        let offset = prev_width - self.data[col_idx].width;

        // Move other columns in tandem with resizing.
        let ongoing_resize_anim = column.tiles[tile_idx].resize_animation().is_some();
        if offset != 0. {
            if self.active_column_idx <= col_idx {
                for col in &mut self.columns[col_idx + 1..] {
                    // If there's a resize animation on the tile (that may have just started in
                    // column.update_window()), then the apparent size change is smooth with no
                    // sudden jumps. This corresponds to adding an X animation to adjacent columns.
                    //
                    // There could also be no resize animation with nonzero offset. This could
                    // happen for example:
                    // - if the window resized on its own, which we don't animate
                    // - if the window resized by less than 10 px (the resize threshold)
                    //
                    // The latter case could also cancel an ongoing resize animation.
                    //
                    // Now, stationary columns shouldn't react to this offset change in any way,
                    // i.e. their apparent X position should jump together with the resize.
                    // However, adjacent columns that are already animating an X movement should
                    // offset their animations to avoid the jump.
                    //
                    // Notably, this is necessary to fix the animation jump when resizing width back
                    // and forth in quick succession (in a way that cancels the resize animation).
                    if ongoing_resize_anim {
                        col.animate_move_from_with_config(
                            offset,
                            self.options.animations.window_resize.anim,
                        );
                    } else {
                        col.offset_move_anim_current(offset);
                    }
                }
            } else {
                for col in &mut self.columns[..=col_idx] {
                    if ongoing_resize_anim {
                        col.animate_move_from_with_config(
                            -offset,
                            self.options.animations.window_resize.anim,
                        );
                    } else {
                        col.offset_move_anim_current(-offset);
                    }
                }
            }
        }

        // When a column goes between fullscreen and non-fullscreen, the tiles origin can change.
        // The change comes from things like ignoring struts and hiding the tab indicator in
        // fullscreen, so both in X and Y directions.
        let column = &mut self.columns[col_idx];
        let new_origin = column.tiles_origin();
        let origin_delta = prev_origin - new_origin;
        if origin_delta != Point::new(0., 0.) {
            for (tile, _pos) in column.tiles_mut() {
                tile.animate_move_from(origin_delta);
            }
        }

        if col_idx == self.active_column_idx {
            // If offset == 0, then don't mess with the view or the gesture. Some clients (Firefox,
            // Chromium, Electron) currently don't commit after the ack of a configure that drops
            // the Resizing state, which can trigger this code path for a while.
            let resize = if offset != 0. { resize } else { None };
            if let Some(resize) = resize {
                // Don't bother with the gesture.
                self.view_offset.cancel_gesture();

                // If this is an interactive resize commit of an active window, then we need to
                // either preserve the view offset or adjust it accordingly.
                let centered = self.is_centering_focused_column();

                let width = self.data[col_idx].width;
                let offset = if let Some(view_pos) = self
                    .options
                    .layout
                    .disable_carousel
                    .then(|| self.compute_centered_view_pos())
                    .flatten()
                {
                    // disable-carousel keeps the whole row centered during an
                    // interactive resize, not just the active column.
                    (view_pos - self.column_x(col_idx)) - self.view_offset.target()
                } else if centered {
                    // FIXME: when view_offset becomes fractional, this can be made additive too.
                    let new_offset =
                        -(self.working_area.size.w - width) / 2. - self.working_area.loc.x;
                    new_offset - self.view_offset.target()
                } else if resize.edges.contains(ResizeEdge::LEFT) {
                    -offset
                } else {
                    0.
                };

                self.view_offset.offset(offset);
            }

            // When the active column goes fullscreen, store the view offset to restore later.
            let is_normal = self.columns[col_idx].sizing_mode().is_normal();
            if was_normal && !is_normal {
                self.view_offset_to_restore = Some(self.view_offset.stationary());
            }

            // Upon unfullscreening, restore the view offset.
            //
            // In tabbed display mode, there can be multiple tiles in a fullscreen column. They
            // will unfullscreen one by one, and the column width will shrink only when the
            // last tile unfullscreens. This is when we want to restore the view offset,
            // otherwise it will immediately reset back by the animate_view_offset below.
            let unfullscreen_offset = if !was_normal && is_normal {
                // Take the value unconditionally, even if the view is currently frozen by
                // a view gesture. It shouldn't linger around because it's only valid for this
                // particular unfullscreen.
                self.view_offset_to_restore.take()
            } else {
                None
            };

            // We might need to move the view to ensure the resized window is still visible. But
            // only do it when the view isn't frozen by an interactive resize or a view gesture.
            if self.interactive_resize.is_none() && !self.view_offset.is_gesture() {
                // Synchronize the horizontal view movement with the resize so that it looks nice.
                // This is especially important for always-centered view.
                let config = if ongoing_resize_anim {
                    self.options.animations.window_resize.anim
                } else {
                    self.options.animations.horizontal_view_movement.0
                };

                // Restore the view offset upon unfullscreening if needed.
                if let Some(prev_offset) = unfullscreen_offset {
                    self.animate_view_offset_with_config(col_idx, prev_offset, config);
                }

                // FIXME: we will want to skip the animation in some cases here to make continuously
                // resizing windows not look janky.
                self.animate_view_offset_to_column_with_config(None, col_idx, None, config);
            }
        }
    }

    pub fn scroll_amount_to_activate(&self, window: &W::Id) -> f64 {
        let column_idx = self
            .columns
            .iter()
            .position(|col| col.contains(window))
            .unwrap();

        if self.active_column_idx == column_idx {
            return 0.;
        }

        // Consider the end of an ongoing animation because that's what compute to fit does too.
        let target_x = self.target_view_pos();
        let new_view_offset = self.compute_new_view_offset_for_column(
            Some(target_x),
            column_idx,
            Some(self.active_column_idx),
        );

        let new_col_x = self.column_x(column_idx);
        let from_view_offset = target_x - new_col_x;

        (from_view_offset - new_view_offset).abs() / self.working_area.size.w
    }

    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        let column_idx = self.columns.iter().position(|col| col.contains(window));
        let Some(column_idx) = column_idx else {
            return false;
        };
        let column = &mut self.columns[column_idx];

        column.activate_window(window);
        self.activate_column(column_idx);

        true
    }

    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        let (tile, mut tile_pos) = self
            .tiles_with_render_positions_mut(false)
            .find(|(tile, _)| tile.window().id() == window)
            .unwrap();

        let Some(snapshot) = tile.take_unmap_snapshot() else {
            return;
        };

        let tile_size = tile.tile_size();

        let (col_idx, tile_idx) = self
            .columns
            .iter()
            .enumerate()
            .find_map(|(col_idx, col)| {
                col.tiles
                    .iter()
                    .position(|tile| tile.window().id() == window)
                    .map(move |tile_idx| (col_idx, tile_idx))
            })
            .unwrap();

        let col = &self.columns[col_idx];
        let removing_last = col.tiles.len() == 1;

        // Skip closing animation for invisible tiles in a tabbed column.
        if col.display_mode == ColumnDisplay::Tabbed && tile_idx != col.active_tile_idx {
            return;
        }

        tile_pos.x += self.view_pos();

        if col_idx < self.active_column_idx {
            let offset = if removing_last {
                self.column_x(col_idx + 1) - self.column_x(col_idx)
            } else {
                self.data[col_idx].width
                    - col
                        .data
                        .iter()
                        .enumerate()
                        .filter_map(|(idx, data)| {
                            (idx != tile_idx).then_some(NotNan::new(data.size.w).unwrap())
                        })
                        .max()
                        .map(NotNan::into_inner)
                        .unwrap()
            };
            tile_pos.x -= offset;
        }

        self.start_close_animation_for_tile(renderer, snapshot, tile_size, tile_pos, blocker);
    }

    fn start_close_animation_for_tile(
        &mut self,
        renderer: &mut GlesRenderer,
        snapshot: TileRenderSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
    ) {
        let anim = Animation::new(
            self.clock.clone(),
            0.,
            1.,
            0.,
            self.options.animations.window_close.anim,
        );

        let blocker = if self.options.disable_transactions {
            TransactionBlocker::completed()
        } else {
            blocker
        };

        let scale = Scale::from(self.scale);
        let res = ClosingWindow::new(
            renderer, snapshot, scale, tile_size, tile_pos, blocker, anim,
        );
        match res {
            Ok(closing) => {
                self.closing_windows.push(closing);
            }
            Err(err) => {
                warn!("error creating a closing window animation: {err:?}");
            }
        }
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        self.columns
            .iter_mut()
            .any(|col| col.start_open_animation(id))
    }

    pub fn focus_left(&mut self) -> bool {
        if self.active_column_idx == 0 {
            return false;
        }
        self.activate_column(self.active_column_idx - 1);
        true
    }

    pub fn focus_right(&mut self) -> bool {
        if self.active_column_idx + 1 >= self.columns.len() {
            return false;
        }

        self.activate_column(self.active_column_idx + 1);
        true
    }

    pub fn focus_column_first(&mut self) {
        self.activate_column(0);
    }

    pub fn focus_column_last(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        self.activate_column(self.columns.len() - 1);
    }

    pub fn focus_column(&mut self, index: usize) {
        if self.columns.is_empty() {
            return;
        }

        self.activate_column(index.saturating_sub(1).min(self.columns.len() - 1));
    }

    pub fn focus_window_in_column(&mut self, index: u8) {
        if self.columns.is_empty() {
            return;
        }

        self.columns[self.active_column_idx].focus_index(index);
    }

    pub fn focus_down(&mut self) -> bool {
        if self.columns.is_empty() {
            return false;
        }

        self.columns[self.active_column_idx].focus_down()
    }

    pub fn focus_up(&mut self) -> bool {
        if self.columns.is_empty() {
            return false;
        }

        self.columns[self.active_column_idx].focus_up()
    }

    pub fn focus_down_or_left(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let column = &mut self.columns[self.active_column_idx];
        if !column.focus_down() {
            self.focus_left();
        }
    }

    pub fn focus_down_or_right(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let column = &mut self.columns[self.active_column_idx];
        if !column.focus_down() {
            self.focus_right();
        }
    }

    pub fn focus_up_or_left(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let column = &mut self.columns[self.active_column_idx];
        if !column.focus_up() {
            self.focus_left();
        }
    }

    pub fn focus_up_or_right(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let column = &mut self.columns[self.active_column_idx];
        if !column.focus_up() {
            self.focus_right();
        }
    }

    pub fn focus_top(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        self.columns[self.active_column_idx].focus_top()
    }

    pub fn focus_bottom(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        self.columns[self.active_column_idx].focus_bottom()
    }

    pub fn move_column_to_index(&mut self, index: usize) {
        if self.columns.is_empty() {
            return;
        }

        self.move_column_to(index.saturating_sub(1).min(self.columns.len() - 1));
    }

    fn move_column_to(&mut self, new_idx: usize) {
        if self.active_column_idx == new_idx {
            return;
        }

        let current_col_x = self.column_x(self.active_column_idx);
        let next_col_x = self.column_x(self.active_column_idx + 1);

        let mut column = self.columns.remove(self.active_column_idx);
        let data = self.data.remove(self.active_column_idx);
        cancel_resize_for_column(&mut self.interactive_resize, &mut column);
        self.columns.insert(new_idx, column);
        self.data.insert(new_idx, data);

        // Preserve the camera position when moving to the left.
        let view_offset_delta = -self.column_x(self.active_column_idx) + current_col_x;
        self.view_offset.offset(view_offset_delta);

        // The column we just moved is offset by the difference between its new and old position.
        let new_col_x = self.column_x(new_idx);
        self.columns[new_idx].animate_move_from(current_col_x - new_col_x);

        // All columns in between moved by the width of the column that we just moved.
        let others_x_offset = next_col_x - current_col_x;
        if self.active_column_idx < new_idx {
            for col in &mut self.columns[self.active_column_idx..new_idx] {
                col.animate_move_from(others_x_offset);
            }
        } else {
            for col in &mut self.columns[new_idx + 1..=self.active_column_idx] {
                col.animate_move_from(-others_x_offset);
            }
        }

        self.activate_column_with_anim_config(new_idx, self.options.animations.window_movement.0);
    }

    pub fn move_left(&mut self) -> bool {
        if self.active_column_idx == 0 {
            return false;
        }

        self.move_column_to(self.active_column_idx - 1);
        true
    }

    pub fn move_right(&mut self) -> bool {
        let new_idx = self.active_column_idx + 1;
        if new_idx >= self.columns.len() {
            return false;
        }

        self.move_column_to(new_idx);
        true
    }

    pub fn move_column_to_first(&mut self) {
        self.move_column_to(0);
    }

    pub fn move_column_to_last(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let new_idx = self.columns.len() - 1;
        self.move_column_to(new_idx);
    }

    pub fn move_down(&mut self) -> bool {
        if self.columns.is_empty() {
            return false;
        }

        self.columns[self.active_column_idx].move_down()
    }

    pub fn move_up(&mut self) -> bool {
        if self.columns.is_empty() {
            return false;
        }

        self.columns[self.active_column_idx].move_up()
    }

    // ---- Stacking move primitives (Phase 4) -------------------------------------------------
    //
    // Each of the four `move_active_window_*_stacked_*` methods moves a SINGLE window — the
    // active window of the active tile — per call. The two "new column/row" variants pull the
    // window into a freshly-created column/tile placed adjacent to the source. The two
    // "overlap-neighbor" variants push the window onto the active tile of the neighbor's window
    // stack and remove the source tile (and column, if it was the only tile) when the source's
    // window stack would otherwise become empty.
    //
    // Returns true on success, false on no-op (e.g. at the workspace edge with no neighbor in
    // the requested direction). The Layout-level orchestrator interprets `false` from an
    // overlap-variant as "edge → next workspace" handling per E5.

    pub fn move_active_window_to_new_column_left(&mut self) -> bool {
        if self.columns.is_empty() {
            return false;
        }
        let source_col_idx = self.active_column_idx;
        let source_tile_idx = self.columns[source_col_idx].active_tile_idx;
        // Snapshot attributes from the source tile before mutating. Carry the source
        // column's `is_full_width` so a full-width source preserves that override
        // instead of falling back to the underlying proportion (e.g. the 50% default).
        let (view_size, scale, clock, options, width, is_full_width) = {
            let source_tile = &self.columns[source_col_idx].tiles[source_tile_idx];
            (
                source_tile.view_size(),
                source_tile.scale(),
                source_tile.clock.clone(),
                source_tile.options.clone(),
                self.columns[source_col_idx].width,
                self.columns[source_col_idx].is_full_width,
            )
        };
        // Capture the source tile's pre-pop geometry and animation snapshot so we
        // can drive a smooth resize on the new tile from the old (in-stack) size
        // to its destination size in the new column.
        let resize_from = {
            let st = &mut self.columns[source_col_idx].tiles[source_tile_idx];
            let snapshot = st.window_mut().take_animation_snapshot();
            snapshot.map(|s| (st.window_size(), st.tile_size(), s))
        };
        // If the source tile holds only the active window, popping would leave the tile
        // with an empty WindowStack (and panic on the next Deref). Use remove_tile_by_idx
        // to cleanly remove the tile — and the column too, if it was the only tile.
        let source_is_single_window =
            self.columns[source_col_idx].tiles[source_tile_idx].stack_len() == 1;
        let source_was_only_tile = self.columns[source_col_idx].tiles.len() == 1;
        let source_column_removed = source_is_single_window && source_was_only_tile;
        let popped = if source_is_single_window {
            let mut removed = self.remove_tile_by_idx(
                source_col_idx,
                source_tile_idx,
                Transaction::new(),
                None,
            );
            removed.tile.pop_active_window()
        } else {
            self.columns[source_col_idx].pop_window_from_tile(source_tile_idx)
        };
        let new_tile = Tile::new(popped, view_size, scale, clock, options);
        // Insert at source_col_idx so the new column appears immediately to the LEFT of the
        // (possibly removed) source column. add_column with activate=true moves focus to the
        // new column.
        self.add_tile(Some(source_col_idx), new_tile, true, width, is_full_width, None);
        if let Some((window_size, tile_size, snapshot)) = resize_from {
            self.columns[source_col_idx].tiles[0]
                .animate_resize_from(window_size, tile_size, snapshot);
        }
        // If the source column survived, it shifted to source_col_idx + 1. Its tile stack
        // just shrunk (or one of its tiles was removed), so refresh its cached column width
        // before any downstream consumer (e.g. column_x or auto_fit_or_center_view_offset)
        // reads it.
        if !source_column_removed {
            let old_source_idx = source_col_idx + 1;
            if old_source_idx < self.columns.len() {
                self.data[old_source_idx].update(&self.columns[old_source_idx]);
            }
        }
        true
    }

    pub fn move_active_window_to_new_column_right(&mut self) -> bool {
        if self.columns.is_empty() {
            return false;
        }
        let source_col_idx = self.active_column_idx;
        let source_tile_idx = self.columns[source_col_idx].active_tile_idx;
        // Carry `is_full_width` so a full-width source preserves the override
        // instead of revealing the underlying proportion in the new column.
        let (view_size, scale, clock, options, width, is_full_width) = {
            let source_tile = &self.columns[source_col_idx].tiles[source_tile_idx];
            (
                source_tile.view_size(),
                source_tile.scale(),
                source_tile.clock.clone(),
                source_tile.options.clone(),
                self.columns[source_col_idx].width,
                self.columns[source_col_idx].is_full_width,
            )
        };
        let resize_from = {
            let st = &mut self.columns[source_col_idx].tiles[source_tile_idx];
            let snapshot = st.window_mut().take_animation_snapshot();
            snapshot.map(|s| (st.window_size(), st.tile_size(), s))
        };
        // If the source tile holds only the active window, popping would leave the tile
        // with an empty WindowStack (and panic on the next Deref). Use remove_tile_by_idx
        // to cleanly remove the tile — and the column too, if it was the only tile.
        let source_is_single_window =
            self.columns[source_col_idx].tiles[source_tile_idx].stack_len() == 1;
        let source_was_only_tile = self.columns[source_col_idx].tiles.len() == 1;
        let source_column_removed = source_is_single_window && source_was_only_tile;
        let popped = if source_is_single_window {
            let mut removed = self.remove_tile_by_idx(
                source_col_idx,
                source_tile_idx,
                Transaction::new(),
                None,
            );
            removed.tile.pop_active_window()
        } else {
            self.columns[source_col_idx].pop_window_from_tile(source_tile_idx)
        };
        let new_tile = Tile::new(popped, view_size, scale, clock, options);
        // Insert at source_col_idx + 1 to place the new column immediately to the RIGHT.
        // If the source column was removed, columns shifted left by one, so its prior right
        // neighbor now sits at source_col_idx — insert at source_col_idx instead to keep the
        // new column immediately to the right of where source was.
        let insert_idx = if source_column_removed {
            source_col_idx
        } else {
            source_col_idx + 1
        };
        self.add_tile(Some(insert_idx), new_tile, true, width, is_full_width, None);
        if let Some((window_size, tile_size, snapshot)) = resize_from {
            self.columns[insert_idx].tiles[0]
                .animate_resize_from(window_size, tile_size, snapshot);
        }
        // If the source column survived, it stays at source_col_idx (insertion was after it).
        // Its tile stack just shrunk (or one of its tiles was removed), so refresh its cached
        // column width.
        if !source_column_removed && source_col_idx < self.columns.len() {
            self.data[source_col_idx].update(&self.columns[source_col_idx]);
        }
        true
    }

    pub fn move_active_window_to_left_neighbor_overlap(&mut self) -> bool {
        let source_col_idx = self.active_column_idx;
        if source_col_idx == 0 {
            return false;
        }
        let source_tile_idx = self.columns[source_col_idx].active_tile_idx;
        let target_col_idx = source_col_idx - 1;

        let source_is_single_window =
            self.columns[source_col_idx].tiles[source_tile_idx].stack_len() == 1;

        let window = if source_is_single_window {
            // Remove the entire source tile (and column if it was the only tile). Target is to
            // the LEFT of source so its index doesn't shift regardless of column removal.
            let mut removed = self.remove_tile_by_idx(
                source_col_idx,
                source_tile_idx,
                Transaction::new(),
                None,
            );
            removed.tile.pop_active_window()
        } else {
            // Multi-window source: just pop one window, source tile stays.
            self.columns[source_col_idx].pop_window_from_tile(source_tile_idx)
        };

        // Push onto target column's active tile.
        let target_col = &mut self.columns[target_col_idx];
        let target_active_tile_idx = target_col.active_tile_idx;
        target_col.tiles[target_active_tile_idx].push_window(window);
        // `push_window` swapped the tile's active window — resync its cached
        // per-tile data so `Column::verify_invariants` stays satisfied.
        target_col.refresh_tile_data(target_active_tile_idx);

        // Focus the target column.
        self.active_column_idx = target_col_idx;

        // Pushed window changed the target tile's content. Re-run per-tile sizing
        // immediately so heights are correct on the next paint — without this the
        // layout stays stale until the next input event triggers a redraw — then
        // refresh the cached column width before auto_fit_or_center_view_offset
        // reads it.
        self.columns[target_col_idx].update_tile_sizes(false);
        self.data[target_col_idx].update(&self.columns[target_col_idx]);
        self.auto_fit_or_center_view_offset();
        true
    }

    pub fn move_active_window_to_right_neighbor_overlap(&mut self) -> bool {
        let source_col_idx = self.active_column_idx;
        if source_col_idx + 1 >= self.columns.len() {
            return false;
        }
        let source_tile_idx = self.columns[source_col_idx].active_tile_idx;
        let original_target = source_col_idx + 1;

        let source_is_single_window =
            self.columns[source_col_idx].tiles[source_tile_idx].stack_len() == 1;

        let (window, target_col_idx) = if source_is_single_window {
            let mut removed = self.remove_tile_by_idx(
                source_col_idx,
                source_tile_idx,
                Transaction::new(),
                None,
            );
            // If source was the only tile in its column, the column was removed; the right
            // neighbor's index shifts left by one.
            let column_was_removed = !self
                .columns
                .get(source_col_idx)
                .map(|_| true)
                .unwrap_or(false)
                || self.columns.len() < original_target + 1;
            // Simpler check: remove always shifts indices to the right of removed by -1 IF the
            // column was removed. We can detect that by comparing tile counts before/after, but
            // since we don't have "before" handy, just check whether `original_target` is still
            // valid. If columns.len() <= original_target, the target shifted to original_target-1.
            let _ = column_was_removed;
            let target = if original_target < self.columns.len() {
                // Could either still be the same neighbor, or could be the shifted-left neighbor.
                // remove_tile_by_idx removes the column iff it was the only tile.
                // Compare: if source column was single-tile, len shrunk by 1 -> target shifts.
                // We know source_is_single_window is true here; that's per-tile, not per-column.
                // The column may have had multiple tiles. Need to detect column removal.
                // Use heuristic: the column at source_col_idx exists iff column wasn't removed.
                if source_col_idx < self.columns.len() {
                    original_target
                } else {
                    original_target - 1
                }
            } else {
                original_target - 1
            };
            (removed.tile.pop_active_window(), target)
        } else {
            (
                self.columns[source_col_idx].pop_window_from_tile(source_tile_idx),
                original_target,
            )
        };

        let target_col = &mut self.columns[target_col_idx];
        let target_active_tile_idx = target_col.active_tile_idx;
        target_col.tiles[target_active_tile_idx].push_window(window);
        // `push_window` swapped the tile's active window — resync its cached
        // per-tile data so `Column::verify_invariants` stays satisfied.
        target_col.refresh_tile_data(target_active_tile_idx);
        self.active_column_idx = target_col_idx;

        // Pushed window changed the target tile's content. Re-run per-tile sizing
        // immediately so heights are correct on the next paint — without this the
        // layout stays stale until the next input event triggers a redraw — then
        // refresh the cached column width before auto_fit_or_center_view_offset
        // reads it.
        self.columns[target_col_idx].update_tile_sizes(false);
        self.data[target_col_idx].update(&self.columns[target_col_idx]);
        self.auto_fit_or_center_view_offset();
        true
    }

    // Vertical stacking moves: act within the active column. Up moves the active window to the
    // tile above; Down moves to the tile below.

    pub fn move_active_window_to_new_row_above(&mut self) -> bool {
        if self.columns.is_empty() {
            return false;
        }
        let col_idx = self.active_column_idx;
        let source_tile_idx = self.columns[col_idx].active_tile_idx;
        if self.columns[col_idx].tiles[source_tile_idx].stack_len() < 2 {
            return false;
        }
        let (view_size, scale, clock, options) = {
            let source_tile = &self.columns[col_idx].tiles[source_tile_idx];
            (
                source_tile.view_size(),
                source_tile.scale(),
                source_tile.clock.clone(),
                source_tile.options.clone(),
            )
        };
        let popped = self.columns[col_idx].pop_window_from_tile(source_tile_idx);
        let new_tile = Tile::new(popped, view_size, scale, clock, options);
        // Insert above the current tile.
        self.add_tile_to_column(col_idx, Some(source_tile_idx), new_tile, true);
        true
    }

    pub fn move_active_window_to_new_row_below(&mut self) -> bool {
        if self.columns.is_empty() {
            return false;
        }
        let col_idx = self.active_column_idx;
        let source_tile_idx = self.columns[col_idx].active_tile_idx;
        if self.columns[col_idx].tiles[source_tile_idx].stack_len() < 2 {
            return false;
        }
        let (view_size, scale, clock, options) = {
            let source_tile = &self.columns[col_idx].tiles[source_tile_idx];
            (
                source_tile.view_size(),
                source_tile.scale(),
                source_tile.clock.clone(),
                source_tile.options.clone(),
            )
        };
        let popped = self.columns[col_idx].pop_window_from_tile(source_tile_idx);
        let new_tile = Tile::new(popped, view_size, scale, clock, options);
        self.add_tile_to_column(col_idx, Some(source_tile_idx + 1), new_tile, true);
        true
    }

    pub fn move_active_window_to_above_neighbor_overlap(&mut self) -> bool {
        let col_idx = self.active_column_idx;
        if self.columns.is_empty() {
            return false;
        }
        let source_tile_idx = self.columns[col_idx].active_tile_idx;
        if source_tile_idx == 0 {
            return false;
        }
        let target_tile_idx = source_tile_idx - 1;
        let source_is_single_window =
            self.columns[col_idx].tiles[source_tile_idx].stack_len() == 1;

        // Capture the target tile's pre-mutation geometry + snapshot so we can drive
        // a smooth resize animation if the column rebalances after a tile removal.
        let target_resize_from = {
            let tt = &mut self.columns[col_idx].tiles[target_tile_idx];
            let snapshot = tt.window_mut().take_animation_snapshot();
            snapshot.map(|s| (tt.window_size(), tt.tile_size(), s))
        };

        let window = if source_is_single_window {
            let mut removed =
                self.remove_tile_by_idx(col_idx, source_tile_idx, Transaction::new(), None);
            removed.tile.pop_active_window()
        } else {
            self.columns[col_idx].pop_window_from_tile(source_tile_idx)
        };

        // After removal of source tile (if applicable), target_tile_idx is unchanged (it was
        // ABOVE the source). If the column itself was removed, col_idx may be invalid — but a
        // single-tile column being removed means there's no above neighbor, so we'd have
        // returned false earlier. Multi-tile column means column survives.
        let column = &mut self.columns[col_idx];
        column.tiles[target_tile_idx].push_window(window);
        // `push_window` swapped the tile's active window — resync its cached
        // per-tile data so `Column::verify_invariants` stays satisfied.
        column.refresh_tile_data(target_tile_idx);
        column.active_tile_idx = target_tile_idx;
        if let Some((window_size, tile_size, snapshot)) = target_resize_from {
            column.tiles[target_tile_idx]
                .animate_resize_from(window_size, tile_size, snapshot);
        }
        // Pushed window changed the target tile's content (e.g. stack indicator now claims
        // a strip of vertical space). Re-run per-tile sizing immediately so the inner
        // window size is correct on the next paint — without this the layout stays stale
        // until the next input event triggers a redraw — then refresh the cached column
        // width.
        self.columns[col_idx].update_tile_sizes(false);
        self.data[col_idx].update(&self.columns[col_idx]);
        true
    }

    pub fn move_active_window_to_below_neighbor_overlap(&mut self) -> bool {
        let col_idx = self.active_column_idx;
        if self.columns.is_empty() {
            return false;
        }
        let source_tile_idx = self.columns[col_idx].active_tile_idx;
        if source_tile_idx + 1 >= self.columns[col_idx].tiles.len() {
            return false;
        }
        let original_target = source_tile_idx + 1;
        let source_is_single_window =
            self.columns[col_idx].tiles[source_tile_idx].stack_len() == 1;

        // Capture the target tile's pre-mutation geometry + snapshot so we can drive
        // a smooth resize animation if the column rebalances after a tile removal.
        let target_resize_from = {
            let tt = &mut self.columns[col_idx].tiles[original_target];
            let snapshot = tt.window_mut().take_animation_snapshot();
            snapshot.map(|s| (tt.window_size(), tt.tile_size(), s))
        };

        let (window, target_tile_idx) = if source_is_single_window {
            let mut removed =
                self.remove_tile_by_idx(col_idx, source_tile_idx, Transaction::new(), None);
            // Removing the source tile shifts the BELOW neighbor's index left by 1.
            (removed.tile.pop_active_window(), original_target - 1)
        } else {
            (
                self.columns[col_idx].pop_window_from_tile(source_tile_idx),
                original_target,
            )
        };

        let column = &mut self.columns[col_idx];
        column.tiles[target_tile_idx].push_window(window);
        // `push_window` swapped the tile's active window — resync its cached
        // per-tile data so `Column::verify_invariants` stays satisfied.
        column.refresh_tile_data(target_tile_idx);
        column.active_tile_idx = target_tile_idx;
        if let Some((window_size, tile_size, snapshot)) = target_resize_from {
            column.tiles[target_tile_idx]
                .animate_resize_from(window_size, tile_size, snapshot);
        }
        // Pushed window changed the target tile's content (e.g. stack indicator now claims
        // a strip of vertical space). Re-run per-tile sizing immediately so the inner
        // window size is correct on the next paint — without this the layout stays stale
        // until the next input event triggers a redraw — then refresh the cached column
        // width.
        self.columns[col_idx].update_tile_sizes(false);
        self.data[col_idx].update(&self.columns[col_idx]);
        true
    }

    /// Cross-column stack-move primitive: lift the active window out of its
    /// column and insert it as a NEW row (tile) in the neighbor column on the
    /// requested side. The new row's index is chosen to match the source
    /// tile's vertical center against the target column's existing tile
    /// centers — so a window that was high in its column lands high in the
    /// neighbor, and one near the bottom lands near the bottom.
    ///
    /// Unlike `move_active_window_to_left_neighbor_overlap`, the moved window
    /// never joins an existing stacked tab; overlapping is reserved for
    /// within-column stack-move alternation. Returns `false` when there is no
    /// neighbor on the requested side.
    pub fn move_active_window_to_neighbor_column_as_new_row(&mut self, to_left: bool) -> bool {
        let source_col_idx = self.active_column_idx;
        let raw_target_idx = if to_left {
            if source_col_idx == 0 {
                return false;
            }
            source_col_idx - 1
        } else {
            if source_col_idx + 1 >= self.columns.len() {
                return false;
            }
            source_col_idx + 1
        };
        let source_tile_idx = self.columns[source_col_idx].active_tile_idx;

        // Positional awareness: walk the target column tiles top-to-bottom and stop at
        // the first whose vertical center sits BELOW the source tile's center. That
        // index becomes the insertion slot. If the source is below every target row,
        // append at the end. Both columns share the same working-area y origin, so the
        // column-local offsets compare directly.
        let source_center_y = {
            let col = &self.columns[source_col_idx];
            col.tile_offset(source_tile_idx).y
                + col.tiles[source_tile_idx].tile_size().h / 2.
        };
        let target_insert_idx = {
            let col = &self.columns[raw_target_idx];
            col.tiles
                .iter()
                .enumerate()
                .find_map(|(i, tile)| {
                    let center = col.tile_offset(i).y + tile.tile_size().h / 2.;
                    (source_center_y < center).then_some(i)
                })
                .unwrap_or(col.tiles.len())
        };

        let source_is_single_window =
            self.columns[source_col_idx].tiles[source_tile_idx].stack_len() == 1;
        let source_was_only_tile = self.columns[source_col_idx].tiles.len() == 1;
        let source_column_removed = source_is_single_window && source_was_only_tile;

        // Snapshot the source tile's pre-move geometry so the new tile can fly from
        // the source's old screen position instead of popping into existence at the
        // target.
        let source_prev_off = self.columns[source_col_idx].tile_offset(source_tile_idx);
        let source_render_x = self.columns[source_col_idx].render_offset().x;
        let source_col_x_before = self.column_x(source_col_idx);

        let tile = if source_is_single_window {
            let removed = self.remove_tile_by_idx(
                source_col_idx,
                source_tile_idx,
                Transaction::new(),
                Some(self.options.animations.window_movement.0),
            );
            removed.tile
        } else {
            // Multi-window tile: pop one window, host it in a fresh tile clone.
            let (view_size, scale, clock, options) = {
                let st = &self.columns[source_col_idx].tiles[source_tile_idx];
                (
                    st.view_size(),
                    st.scale(),
                    st.clock.clone(),
                    st.options.clone(),
                )
            };
            let popped =
                self.columns[source_col_idx].pop_window_from_tile(source_tile_idx);
            Tile::new(popped, view_size, scale, clock, options)
        };

        // When a single-tile source column is removed, columns to its right shift left
        // by one — so the original right neighbor at source_col_idx + 1 now sits at
        // source_col_idx. Left-neighbor targets are unaffected by that shift.
        let target_col_idx = if !to_left && source_column_removed {
            source_col_idx
        } else {
            raw_target_idx
        };

        self.add_tile_to_column(target_col_idx, Some(target_insert_idx), tile, true);

        // Slide the new tile from the source tile's old screen position to its
        // destination slot. animate_move_from(from) makes the tile render at
        // `tile_pos + from` at t=0 and decay to `tile_pos` at t=1.
        let target_col_x_after = self.column_x(target_col_idx);
        let new_tile_off = self.columns[target_col_idx].tile_offset(target_insert_idx);
        let target_render_x = self.columns[target_col_idx].render_offset().x;

        // Source's old screen position - target's new screen position.
        let source_screen_x = source_col_x_before + source_render_x + source_prev_off.x;
        let target_screen_x = target_col_x_after + target_render_x + new_tile_off.x;
        let from_x = source_screen_x - target_screen_x;
        let from_y = source_prev_off.y - new_tile_off.y;
        self.columns[target_col_idx].tiles[target_insert_idx]
            .animate_move_from(Point::from((from_x, from_y)));

        // Multi-window source: the source tile's window stack lost one entry. Refresh
        // its sizing/cache so the next column_x pass sees consistent state.
        if !source_is_single_window {
            self.columns[source_col_idx].update_tile_sizes(false);
            self.data[source_col_idx].update(&self.columns[source_col_idx]);
        }

        true
    }

    /// Begin (or continue) tracking the width of a window being moved across columns.
    ///
    /// Captures the window's current width the first time it moves; a continuation move of
    /// the same window keeps the originally-captured width so the window's size is
    /// preserved across the whole sequence. Moving a *different* window starts fresh.
    fn note_window_move(&mut self, source_col_idx: usize, source_tile_idx: usize) {
        let tile = &self.columns[source_col_idx].tiles[source_tile_idx];
        let window = tile.window().id().clone();
        let carried_width = tile.window_size().w;
        // Continuation of the same window keeps its originally-captured width; a different
        // window (or no tracking yet) starts fresh.
        let is_continuation = matches!(&self.move_size, Some(ms) if ms.window == window);
        // The move's own column re-activations must not trip the focus-change
        // clear; `end_window_move` lifts this once the move finishes.
        self.in_window_move = true;
        if !is_continuation {
            self.move_size = Some(MoveSizeMemory {
                window,
                carried_width,
                host_prior: None,
            });
        }
    }

    /// Grow `col_idx` to the carried window width if it's currently narrower (never
    /// shrink), snapshotting its prior width so it can be restored when the window leaves.
    /// The carried window is the last tile in the column (it was just added there).
    fn grow_host_to_carried(&mut self, col_idx: usize) {
        let Some(carried) = self.move_size.as_ref().map(|ms| ms.carried_width) else {
            return;
        };
        let tile_idx = self.columns[col_idx].tiles.len() - 1;
        // Compare in column-width terms using the column's resolved (synchronous) width.
        // The just-added tile's `window_size()` may not reflect the new column yet, and
        // relying on it could shrink a wider column — this must only ever grow.
        let needed = self.columns[col_idx].tiles[tile_idx].tile_width_for_window_width(carried);
        let current = self.columns[col_idx].natural_width();
        if needed <= current + 1.0 {
            return;
        }
        let snapshot = self.columns[col_idx].width_snapshot();
        if let Some(ms) = self.move_size.as_mut() {
            ms.host_prior = Some(snapshot);
        }
        self.columns[col_idx].set_column_width(
            SizeChange::SetFixed(carried.round() as i32),
            Some(tile_idx),
            true,
        );
        self.data[col_idx].update(&self.columns[col_idx]);
    }

    /// Restore `col_idx`'s width from the host snapshot, if the carried window had grown
    /// it. Called when the window leaves the column it was hosted in.
    fn restore_host_width(&mut self, col_idx: usize) {
        let snapshot = self.move_size.as_mut().and_then(|ms| ms.host_prior.take());
        if let Some(snapshot) = snapshot {
            self.columns[col_idx].restore_width_snapshot(&snapshot);
            self.data[col_idx].update(&self.columns[col_idx]);
        }
    }

    /// Set `col_idx` (the carried window's own freshly-created column) to exactly the
    /// carried width, so an expelled window is restored to its remembered size.
    fn set_carried_column_width(&mut self, col_idx: usize) {
        let Some(carried) = self.move_size.as_ref().map(|ms| ms.carried_width) else {
            return;
        };
        self.columns[col_idx].set_column_width(
            SizeChange::SetFixed(carried.round() as i32),
            Some(0),
            true,
        );
        self.data[col_idx].update(&self.columns[col_idx]);
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        if self.columns.is_empty() {
            return;
        }

        let (source_col_idx, source_tile_idx) = if let Some(window) = window {
            self.columns
                .iter_mut()
                .enumerate()
                .find_map(|(col_idx, col)| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (col_idx, tile_idx))
                })
                .unwrap()
        } else {
            let source_col_idx = self.active_column_idx;
            let source_tile_idx = self.columns[self.active_column_idx].active_tile_idx;
            (source_col_idx, source_tile_idx)
        };

        let source_column = &self.columns[source_col_idx];
        let prev_off = source_column.tile_offset(source_tile_idx);

        let source_tile_was_active = self.active_column_idx == source_col_idx
            && source_column.active_tile_idx == source_tile_idx;

        if source_column.tiles.len() == 1 {
            if source_col_idx == 0 {
                return;
            }

            // Move into adjacent column.
            let target_column_idx = source_col_idx - 1;
            self.note_window_move(source_col_idx, source_tile_idx);

            let offset = if self.active_column_idx <= source_col_idx {
                // Tiles to the right animate from the following column.
                self.column_x(source_col_idx) - self.column_x(target_column_idx)
            } else {
                // Tiles to the left animate to preserve their right edge position.
                f64::max(
                    0.,
                    self.data[target_column_idx].width - self.data[source_col_idx].width,
                )
            };
            let mut offset = Point::from((offset, 0.));

            if source_tile_was_active {
                // Make sure the previous (target) column is activated so the animation looks right.
                //
                // However, if it was already going to be activated, leave the offset as is. This
                // improves the workflow that has become common with tabbed columns: open a new
                // window, then immediately consume it left as a new tab.
                self.activate_prev_column_on_removal
                    .get_or_insert(self.view_offset.stationary() + offset.x);
            }

            offset.x += self.columns[source_col_idx].render_offset().x;
            let RemovedTile { tile, .. } = self.remove_tile_by_idx(
                source_col_idx,
                0,
                Transaction::new(),
                Some(self.options.animations.window_movement.0),
            );
            self.add_tile_to_column(target_column_idx, None, tile, source_tile_was_active);

            let target_column = &mut self.columns[target_column_idx];
            offset.x -= target_column.render_offset().x;
            offset += prev_off - target_column.tile_offset(target_column.tiles.len() - 1);

            let new_tile = target_column.tiles.last_mut().unwrap();
            new_tile.animate_move_from(offset);

            // Preserve the moved window's size: grow the column it joined to fit it.
            self.grow_host_to_carried(target_column_idx);
        } else {
            // Move out of column.
            let mut offset = Point::from((source_column.render_offset().x, 0.));
            self.note_window_move(source_col_idx, source_tile_idx);

            let removed =
                self.remove_tile_by_idx(source_col_idx, source_tile_idx, Transaction::new(), None);

            // The window is leaving this column; restore it if we'd grown it to host the
            // window (the column still sits at source_col_idx after the removal).
            self.restore_host_width(source_col_idx);

            // We're inserting into the source column position.
            let target_column_idx = source_col_idx;

            self.add_tile(
                Some(target_column_idx),
                removed.tile,
                source_tile_was_active,
                removed.width,
                removed.is_full_width,
                Some(self.options.animations.window_movement.0),
            );

            if source_tile_was_active {
                // We added to the left, don't activate even further left on removal.
                self.activate_prev_column_on_removal = None;
            }

            if target_column_idx <= self.active_column_idx {
                // Tiles to the left animate from the following column.
                offset.x += self.column_x(target_column_idx + 1) - self.column_x(target_column_idx);
            }

            let new_col = &mut self.columns[target_column_idx];
            offset += prev_off - new_col.tile_offset(0);
            new_col.tiles[0].animate_move_from(offset);

            // The expelled window keeps its remembered size in its own new column.
            self.set_carried_column_width(target_column_idx);
        }
        // The move (and its internal column re-activations) has finished, so
        // re-enable the focus-change clear for `move_size`.
        self.in_window_move = false;
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        if self.columns.is_empty() {
            return;
        }

        let (source_col_idx, source_tile_idx) = if let Some(window) = window {
            self.columns
                .iter_mut()
                .enumerate()
                .find_map(|(col_idx, col)| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (col_idx, tile_idx))
                })
                .unwrap()
        } else {
            let source_col_idx = self.active_column_idx;
            let source_tile_idx = self.columns[self.active_column_idx].active_tile_idx;
            (source_col_idx, source_tile_idx)
        };

        let cur_x = self.column_x(source_col_idx);

        let source_column = &self.columns[source_col_idx];
        let mut offset = Point::from((source_column.render_offset().x, 0.));
        let prev_off = source_column.tile_offset(source_tile_idx);

        let source_tile_was_active = self.active_column_idx == source_col_idx
            && source_column.active_tile_idx == source_tile_idx;

        if source_column.tiles.len() == 1 {
            if source_col_idx + 1 == self.columns.len() {
                return;
            }

            // Move into adjacent column.
            let target_column_idx = source_col_idx;
            self.note_window_move(source_col_idx, source_tile_idx);

            offset.x += cur_x - self.column_x(source_col_idx + 1);
            offset.x -= self.columns[source_col_idx + 1].render_offset().x;

            if source_tile_was_active {
                // Make sure the target column gets activated.
                self.activate_prev_column_on_removal = None;
            }

            let RemovedTile { tile, .. } = self.remove_tile_by_idx(
                source_col_idx,
                0,
                Transaction::new(),
                Some(self.options.animations.window_movement.0),
            );
            self.add_tile_to_column(target_column_idx, None, tile, source_tile_was_active);

            let target_column = &mut self.columns[target_column_idx];
            offset += prev_off - target_column.tile_offset(target_column.tiles.len() - 1);

            let new_tile = target_column.tiles.last_mut().unwrap();
            new_tile.animate_move_from(offset);

            // Preserve the moved window's size: grow the column it joined to fit it.
            self.grow_host_to_carried(target_column_idx);
        } else {
            // Move out of column.
            self.note_window_move(source_col_idx, source_tile_idx);

            let prev_width = self.data[source_col_idx].width;

            let removed =
                self.remove_tile_by_idx(source_col_idx, source_tile_idx, Transaction::new(), None);

            // The window is leaving this column; restore it if we'd grown it to host the
            // window (the column still sits at source_col_idx after the removal).
            self.restore_host_width(source_col_idx);

            let target_column_idx = source_col_idx + 1;

            self.add_tile(
                Some(target_column_idx),
                removed.tile,
                source_tile_was_active,
                removed.width,
                removed.is_full_width,
                Some(self.options.animations.window_movement.0),
            );

            offset.x += if self.active_column_idx <= target_column_idx {
                // Tiles to the right animate to the following column.
                cur_x - self.column_x(target_column_idx)
            } else {
                // Tiles to the left animate for a change in width.
                -f64::max(0., prev_width - self.data[target_column_idx].width)
            };

            let new_col = &mut self.columns[target_column_idx];
            offset += prev_off - new_col.tile_offset(0);
            new_col.tiles[0].animate_move_from(offset);

            // The expelled window keeps its remembered size in its own new column.
            self.set_carried_column_width(target_column_idx);
        }
        // The move (and its internal column re-activations) has finished, so
        // re-enable the focus-change clear for `move_size`.
        self.in_window_move = false;
    }

    pub fn consume_into_column(&mut self) {
        if self.columns.len() < 2 {
            return;
        }

        if self.active_column_idx == self.columns.len() - 1 {
            return;
        }

        let target_column_idx = self.active_column_idx;
        let source_column_idx = self.active_column_idx + 1;

        let offset = self.column_x(source_column_idx)
            + self.columns[source_column_idx].render_offset().x
            - self.column_x(target_column_idx);
        let mut offset = Point::from((offset, 0.));
        let prev_off = self.columns[source_column_idx].tile_offset(0);

        let removed = self.remove_tile_by_idx(source_column_idx, 0, Transaction::new(), None);
        self.add_tile_to_column(target_column_idx, None, removed.tile, false);

        let target_column = &mut self.columns[target_column_idx];
        offset += prev_off - target_column.tile_offset(target_column.tiles.len() - 1);
        offset.x -= target_column.render_offset().x;

        let new_tile = target_column.tiles.last_mut().unwrap();
        new_tile.animate_move_from(offset);
    }

    pub fn expel_from_column(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let source_col_idx = self.active_column_idx;
        let target_col_idx = self.active_column_idx + 1;
        let cur_x = self.column_x(source_col_idx);

        let source_column = &self.columns[self.active_column_idx];
        if source_column.tiles.len() == 1 {
            return;
        }

        let source_tile_idx = source_column.tiles.len() - 1;

        let mut offset = Point::from((source_column.render_offset().x, 0.));
        let prev_off = source_column.tile_offset(source_tile_idx);

        let removed =
            self.remove_tile_by_idx(source_col_idx, source_tile_idx, Transaction::new(), None);

        self.add_tile(
            Some(target_col_idx),
            removed.tile,
            false,
            removed.width,
            removed.is_full_width,
            Some(self.options.animations.window_movement.0),
        );

        offset.x += cur_x - self.column_x(target_col_idx);

        let new_col = &mut self.columns[target_col_idx];
        offset += prev_off - new_col.tile_offset(0);
        new_col.tiles[0].animate_move_from(offset);
    }

    pub fn swap_window_in_direction(&mut self, direction: ScrollDirection) {
        if self.columns.is_empty() {
            return;
        }

        // if this is the first (resp. last column), then this operation is equivalent
        // to an `consume_or_expel_window_left` (resp. `consume_or_expel_window_right`)
        match direction {
            ScrollDirection::Left => {
                if self.active_column_idx == 0 {
                    return;
                }
            }
            ScrollDirection::Right => {
                if self.active_column_idx == self.columns.len() - 1 {
                    return;
                }
            }
        }

        let source_column_idx = self.active_column_idx;
        let target_column_idx = self.active_column_idx.wrapping_add_signed(match direction {
            ScrollDirection::Left => -1,
            ScrollDirection::Right => 1,
        });

        // if both source and target columns contain a single tile, then the operation is equivalent
        // to a simple column move
        if self.columns[source_column_idx].tiles.len() == 1
            && self.columns[target_column_idx].tiles.len() == 1
        {
            return self.move_column_to(target_column_idx);
        }

        let source_tile_idx = self.columns[source_column_idx].active_tile_idx;
        let target_tile_idx = self.columns[target_column_idx].active_tile_idx;
        let source_column_drained = self.columns[source_column_idx].tiles.len() == 1;

        // capture the original positions of the tiles
        let (mut source_pt, mut target_pt) = (
            self.columns[source_column_idx].render_offset()
                + self.columns[source_column_idx].tile_offset(source_tile_idx),
            self.columns[target_column_idx].render_offset()
                + self.columns[target_column_idx].tile_offset(target_tile_idx),
        );
        source_pt.x += self.column_x(source_column_idx);
        target_pt.x += self.column_x(target_column_idx);

        let transaction = Transaction::new();

        // If the source column contains a single tile, this will also remove the column.
        // When this happens `source_column_drained` will be set and the column will need to be
        // recreated with `add_tile`
        let source_removed = self.remove_tile_by_idx(
            source_column_idx,
            source_tile_idx,
            transaction.clone(),
            None,
        );

        {
            // special case when the source column disappears after removing its last tile
            let adjusted_target_column_idx =
                if direction == ScrollDirection::Right && source_column_drained {
                    target_column_idx - 1
                } else {
                    target_column_idx
                };

            self.add_tile_to_column(
                adjusted_target_column_idx,
                Some(target_tile_idx),
                source_removed.tile,
                false,
            );

            let RemovedTile {
                tile: target_tile, ..
            } = self.remove_tile_by_idx(
                adjusted_target_column_idx,
                target_tile_idx + 1,
                transaction.clone(),
                None,
            );

            if source_column_drained {
                // recreate the drained column with only the target tile
                self.add_tile(
                    Some(source_column_idx),
                    target_tile,
                    true,
                    source_removed.width,
                    source_removed.is_full_width,
                    None,
                )
            } else {
                // simply add the removed target tile to the source column
                self.add_tile_to_column(
                    source_column_idx,
                    Some(source_tile_idx),
                    target_tile,
                    false,
                );
            }
        }

        // update the active tile in the modified columns
        self.columns[source_column_idx].active_tile_idx = source_tile_idx;
        self.columns[target_column_idx].active_tile_idx = target_tile_idx;

        // Animations
        self.columns[target_column_idx].tiles[target_tile_idx]
            .animate_move_from(source_pt - target_pt);
        self.columns[target_column_idx].tiles[target_tile_idx].ensure_alpha_animates_to_1();

        // FIXME: this stop_move_animations() causes the target tile animation to "reset" when
        // swapping. It's here as a workaround to stop the unwanted animation of moving the source
        // tile down when adding the target tile above it. This code needs to be written in some
        // other way not to trigger that animation, or to cancel it properly, so that swap doesn't
        // cancel all ongoing target tile animations.
        self.columns[source_column_idx].tiles[source_tile_idx].stop_move_animations();
        self.columns[source_column_idx].tiles[source_tile_idx]
            .animate_move_from(target_pt - source_pt);
        self.columns[source_column_idx].tiles[source_tile_idx].ensure_alpha_animates_to_1();

        self.activate_column(target_column_idx);
    }

    pub fn toggle_column_tabbed_display(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let col = &mut self.columns[self.active_column_idx];
        let display = match col.display_mode {
            ColumnDisplay::Normal => ColumnDisplay::Tabbed,
            ColumnDisplay::Tabbed => ColumnDisplay::Normal,
        };

        self.set_column_display(display);
    }

    pub fn set_column_display(&mut self, display: ColumnDisplay) {
        if self.columns.is_empty() {
            return;
        }

        let col = &mut self.columns[self.active_column_idx];
        if col.display_mode == display {
            return;
        }

        cancel_resize_for_column(&mut self.interactive_resize, col);
        col.set_column_display(display);

        // With place_within_column, the tab indicator changes the column size immediately.
        self.data[self.active_column_idx].update(col);
        col.update_tile_sizes(true);

        // Disable fullscreen if needed.
        if col.display_mode != ColumnDisplay::Tabbed && col.tiles.len() > 1 {
            let window = col.tiles[col.active_tile_idx].window().id().clone();
            self.set_fullscreen(&window, false);
            self.set_maximized(&window, false);
        }
    }

    pub fn center_column(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        self.animate_view_offset_to_column_centered(
            None,
            self.active_column_idx,
            self.options.animations.horizontal_view_movement.0,
        );

        let col = &mut self.columns[self.active_column_idx];
        cancel_resize_for_column(&mut self.interactive_resize, col);
    }

    pub fn center_window(&mut self, window: Option<&W::Id>) {
        if self.columns.is_empty() {
            return;
        }

        let col_idx = if let Some(window) = window {
            self.columns
                .iter()
                .position(|col| col.contains(window))
                .unwrap()
        } else {
            self.active_column_idx
        };

        // We can reasonably center only the active column.
        if col_idx != self.active_column_idx {
            return;
        }

        self.center_column();
    }

    pub fn center_visible_columns(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        if self.is_centering_focused_column() {
            return;
        }

        // Consider the end of an ongoing animation because that's what compute to fit does too.
        let view_x = self.target_view_pos();
        let working_x = self.working_area.loc.x;
        let working_w = self.working_area.size.w;

        // Count all columns that are fully visible inside the working area.
        let mut width_taken = 0.;
        let mut leftmost_col_x = None;
        let mut active_col_x = None;

        let gap = self.options.layout.gaps;
        let col_xs = self.column_xs(self.data.iter().copied());
        for (idx, col_x) in col_xs.take(self.columns.len()).enumerate() {
            if col_x < view_x + working_x + gap {
                // Column goes off-screen to the left.
                continue;
            }

            leftmost_col_x.get_or_insert(col_x);

            let width = self.data[idx].width;
            if view_x + working_x + working_w < col_x + width + gap {
                // Column goes off-screen to the right. We can stop here.
                break;
            }

            if idx == self.active_column_idx {
                active_col_x = Some(col_x);
            }

            width_taken += width + gap;
        }

        if active_col_x.is_none() {
            // The active column wasn't fully on screen, so we can't meaningfully do anything.
            return;
        }

        let col = &mut self.columns[self.active_column_idx];
        cancel_resize_for_column(&mut self.interactive_resize, col);

        let free_space = working_w - width_taken + gap;
        let new_view_x = leftmost_col_x.unwrap() - free_space / 2. - working_x;

        self.animate_view_offset(self.active_column_idx, new_view_x - active_col_x.unwrap());
        // Just in case.
        self.animate_view_offset_to_column(None, self.active_column_idx, None);
    }

    pub fn view_pos(&self) -> f64 {
        self.column_x(self.active_column_idx) + self.view_offset.current()
    }

    pub fn target_view_pos(&self) -> f64 {
        self.column_x(self.active_column_idx) + self.view_offset.target()
    }

    /// Visual center-X of the active column inside the working area, accounting
    /// for view_offset and working-area inset. Used as the "carry" coordinate
    /// when crossing workspaces with focus-up/down so the new workspace's focus
    /// lands on a positionally similar column.
    pub fn active_column_visual_center_x(&self) -> Option<f64> {
        if self.columns.is_empty() {
            return None;
        }
        let idx = self.active_column_idx;
        let col_x = self.column_x(idx);
        let width = self.data[idx].width;
        let view_x = self.target_view_pos();
        Some(col_x + width / 2. - view_x + self.working_area.loc.x)
    }

    /// Whether the active column's visual center sits in the left half of the
    /// working area. Used by positionally-aware resize to decide whether a given
    /// arrow grows (points toward the center) or shrinks (points away) the
    /// column. Returns None when the workspace is empty.
    pub fn active_column_is_left_of_center(&self) -> Option<bool> {
        let center_x = self.active_column_visual_center_x()?;
        let area_center = self.working_area.loc.x + self.working_area.size.w / 2.;
        Some(center_x < area_center)
    }

    /// Find the column whose visual center-X is closest to `target_x` (in the
    /// same coordinate space as `active_column_visual_center_x`). Returns None
    /// if the workspace is empty.
    pub fn closest_column_to_visual_center_x(&self, target_x: f64) -> Option<usize> {
        if self.columns.is_empty() {
            return None;
        }
        let view_x = self.target_view_pos();
        let working_x = self.working_area.loc.x;
        let mut best_idx = 0usize;
        let mut best_dist = f64::INFINITY;
        for (i, x) in self
            .column_xs(self.data.iter().copied())
            .take(self.columns.len())
            .enumerate()
        {
            let center = x + self.data[i].width / 2. - view_x + working_x;
            let dist = (center - target_x).abs();
            if dist < best_dist {
                best_dist = dist;
                best_idx = i;
            }
        }
        Some(best_idx)
    }

    /// Absolute resting view position (`view_pos`) that centers the whole column
    /// row inside the working area, or clamps it to the edges when it overflows.
    /// Returns None when neither `auto-fit-or-center` nor `disable-carousel` is
    /// on, or the workspace is empty.
    ///
    /// `disable-carousel` always centers the (non-scrolling) row: with no
    /// scrolling there's no other sensible resting position, and the columns are
    /// sized to fit, so the group sits centered with equal slack on both sides
    /// whenever it doesn't fill the area between the panels.
    fn compute_centered_view_pos(&self) -> Option<f64> {
        if !self.options.layout.auto_fit_or_center && !self.options.layout.disable_carousel {
            return None;
        }
        if self.columns.is_empty() {
            return None;
        }

        let working_x = self.working_area.loc.x;
        let working_w = self.working_area.size.w;
        let gap = self.options.layout.gaps;

        // column_xs() yields one extra "past-the-end" position with an extra
        // trailing gap; total content extent is that position minus the gap.
        let total_content = self.column_x(self.columns.len()) - gap;

        let view_pos = if total_content <= working_w {
            // Fits — center the group inside the working area.
            -working_x - (working_w - total_content) / 2.
        } else {
            // Overflows — clamp so neither extremity has wasted gap.
            // `min_view_x` keeps the content's left edge at the working area's
            // left edge (content spills off the right); `max_view_x` keeps its
            // right edge at the working area's right edge (spills off the left).
            let min_view_x = -working_x;
            let max_view_x = total_content - working_w - working_x;

            // In disable-carousel mode there is no horizontal scrolling, so when
            // a fixed-side panel insets one edge the overflowing row must spill
            // off the *screen* edge, not slide under the panel. Anchor it to the
            // panel's inner edge: left panel → left-anchor (overflow right),
            // right panel → right-anchor (overflow left). With panels on both
            // sides or neither — and in scrolling mode, where columns are meant
            // to slide under the panel behind the edge fade — follow the active
            // column instead.
            let left_inset = working_x.max(0.);
            let right_inset = (self.view_size.w - working_x - working_w).max(0.);
            if self.options.layout.disable_carousel && left_inset > right_inset + 1. {
                min_view_x
            } else if self.options.layout.disable_carousel && right_inset > left_inset + 1. {
                max_view_x
            } else {
                self.target_view_pos().clamp(min_view_x, max_view_x)
            }
        };
        Some(view_pos)
    }

    /// Compute the view_offset that `auto_fit_or_center_view_offset` would target
    /// for the current column set / active column / working area. Returns None if
    /// the feature is disabled or the workspace is empty (caller should fall back
    /// to its own default).
    fn compute_auto_fit_or_center_view_offset(&self) -> Option<f64> {
        // view_offset is expressed relative to the active column.
        let view_pos = self.compute_centered_view_pos()?;
        Some(view_pos - self.column_x(self.active_column_idx))
    }

    /// If `auto-fit-or-center` is enabled in config, recompute the view offset
    /// so that all columns are centered as a group when they fit, or so that
    /// neither edge has wasted empty space when they overflow.
    ///
    /// Should be called after any layout-changing operation (window add/remove,
    /// stacking move, neighbor-overlap move). No-op if the flag is off, the
    /// workspace is empty, or there's no active column.
    pub fn auto_fit_or_center_view_offset(&mut self) {
        let Some(new_offset) = self.compute_auto_fit_or_center_view_offset() else {
            return;
        };
        self.animate_view_offset(self.active_column_idx, new_offset);
    }

    // HACK: pass a self.data iterator in manually as a workaround for the lack of method partial
    // borrowing. Note that this method's return value does not borrow the entire &Self!
    fn column_xs(&self, data: impl Iterator<Item = ColumnData>) -> impl Iterator<Item = f64> {
        let gaps = self.options.layout.gaps;
        let mut x = 0.;

        // Chain with a dummy value to be able to get one past all columns' X.
        let dummy = ColumnData { width: 0. };
        let data = data.chain(iter::once(dummy));

        data.map(move |data| {
            let rv = x;
            x += data.width + gaps;
            rv
        })
    }

    fn column_x(&self, column_idx: usize) -> f64 {
        self.column_xs(self.data.iter().copied())
            .nth(column_idx)
            .unwrap()
    }

    fn column_xs_in_render_order(
        &self,
        data: impl Iterator<Item = ColumnData>,
    ) -> impl Iterator<Item = f64> {
        let active_idx = self.active_column_idx;
        let active_pos = self.column_x(active_idx);
        let offsets = self
            .column_xs(data)
            .enumerate()
            .filter_map(move |(idx, pos)| (idx != active_idx).then_some(pos));
        iter::once(active_pos).chain(offsets)
    }

    pub fn columns(&self) -> impl Iterator<Item = &Column<W>> {
        self.columns.iter()
    }

    fn columns_mut(&mut self) -> impl Iterator<Item = (&mut Column<W>, f64)> + '_ {
        let offsets = self.column_xs(self.data.iter().copied());
        zip(&mut self.columns, offsets)
    }

    fn columns_in_render_order(&self) -> impl Iterator<Item = (&Column<W>, f64)> + '_ {
        let offsets = self.column_xs_in_render_order(self.data.iter().copied());

        let (first, active, rest) = if self.columns.is_empty() {
            (&[][..], &[][..], &[][..])
        } else {
            let (first, rest) = self.columns.split_at(self.active_column_idx);
            let (active, rest) = rest.split_at(1);
            (first, active, rest)
        };

        let columns = active.iter().chain(first).chain(rest);
        zip(columns, offsets)
    }

    fn columns_in_render_order_mut(&mut self) -> impl Iterator<Item = (&mut Column<W>, f64)> + '_ {
        let offsets = self.column_xs_in_render_order(self.data.iter().copied());

        let (first, active, rest) = if self.columns.is_empty() {
            (&mut [][..], &mut [][..], &mut [][..])
        } else {
            let (first, rest) = self.columns.split_at_mut(self.active_column_idx);
            let (active, rest) = rest.split_at_mut(1);
            (first, active, rest)
        };

        let columns = active.iter_mut().chain(first).chain(rest);
        zip(columns, offsets)
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> {
        let scale = self.scale;
        let view_off = Point::from((-self.view_pos(), 0.));
        self.columns_in_render_order()
            .flat_map(move |(col, col_x)| {
                let col_off = Point::from((col_x, 0.));
                let col_render_off = col.render_offset();
                col.tiles_in_render_order()
                    .map(move |(tile, tile_off, visible)| {
                        let pos =
                            view_off + col_off + col_render_off + tile_off + tile.render_offset();
                        // Round to physical pixels.
                        let pos = pos.to_physical_precise_round(scale).to_logical(scale);
                        (tile, pos, visible)
                    })
            })
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let scale = self.scale;
        let view_off = Point::from((-self.view_pos(), 0.));
        self.columns_in_render_order_mut()
            .flat_map(move |(col, col_x)| {
                let col_off = Point::from((col_x, 0.));
                let col_render_off = col.render_offset();
                col.tiles_in_render_order_mut()
                    .map(move |(tile, tile_off)| {
                        let mut pos =
                            view_off + col_off + col_render_off + tile_off + tile.render_offset();
                        // Round to physical pixels.
                        if round {
                            pos = pos.to_physical_precise_round(scale).to_logical(scale);
                        }
                        (tile, pos)
                    })
            })
    }

    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> {
        self.columns
            .iter()
            .enumerate()
            .flat_map(move |(col_idx, col)| {
                col.tiles().enumerate().map(move |(tile_idx, (tile, _))| {
                    let layout = WindowLayout {
                        // Our indices are 1-based, consistent with the actions.
                        pos_in_scrolling_layout: Some((col_idx + 1, tile_idx + 1)),
                        ..tile.ipc_layout_template()
                    };
                    (tile, layout)
                })
            })
    }

    pub(super) fn insert_hint_area(
        &self,
        position: InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        let mut hint_area = match position {
            InsertPosition::NewColumn(column_index) => {
                if column_index == 0 || column_index == self.columns.len() {
                    let size = Size::from((
                        300.,
                        self.working_area.size.h - self.options.layout.gaps * 2.,
                    ));
                    let mut loc = Point::from((
                        self.column_x(column_index),
                        self.working_area.loc.y + self.options.layout.gaps,
                    ));
                    if column_index == 0 && !self.columns.is_empty() {
                        loc.x -= size.w + self.options.layout.gaps;
                    }
                    Rectangle::new(loc, size)
                } else if column_index > self.columns.len() {
                    error!("insert hint column index is out of range");
                    return None;
                } else {
                    let size = Size::from((
                        300.,
                        self.working_area.size.h - self.options.layout.gaps * 2.,
                    ));
                    let loc = Point::from((
                        self.column_x(column_index) - size.w / 2. - self.options.layout.gaps / 2.,
                        self.working_area.loc.y + self.options.layout.gaps,
                    ));
                    Rectangle::new(loc, size)
                }
            }
            InsertPosition::InColumn(column_index, tile_index) => {
                if column_index > self.columns.len() {
                    error!("insert hint column index is out of range");
                    return None;
                }

                let col = &self.columns[column_index];
                if tile_index > col.tiles.len() {
                    error!("insert hint tile index is out of range");
                    return None;
                }

                let is_tabbed = col.display_mode == ColumnDisplay::Tabbed;

                let (height, y) = if is_tabbed {
                    // In tabbed mode, there's only one tile visible, and we want to draw the hint
                    // at its top or bottom.
                    let top = col.tile_offset(col.active_tile_idx).y;
                    let bottom = top + col.data[col.active_tile_idx].size.h;

                    if tile_index <= col.active_tile_idx {
                        (150., top)
                    } else {
                        (150., bottom - 150.)
                    }
                } else {
                    let top = col.tile_offset(tile_index).y;

                    if tile_index == 0 {
                        (150., top)
                    } else if tile_index == col.tiles.len() {
                        (150., top - self.options.layout.gaps - 150.)
                    } else {
                        (300., top - self.options.layout.gaps / 2. - 150.)
                    }
                };

                // Adjust for place-within-column tab indicator.
                let origin_x = col.tiles_origin().x;
                let extra_w = if is_tabbed && col.sizing_mode().is_normal() {
                    col.tab_indicator.extra_size(col.tiles.len(), col.scale).w
                } else {
                    0.
                };

                let size = Size::from((self.data[column_index].width - extra_w, height));
                let loc = Point::from((self.column_x(column_index) + origin_x, y));
                Rectangle::new(loc, size)
            }
            InsertPosition::Floating => return None,
        };

        // First window on an empty workspace will cancel out any view offset. Replicate this
        // effect here.
        if self.columns.is_empty() {
            let view_offset = if self.is_centering_focused_column() {
                self.compute_new_view_offset_centered(
                    Some(0.),
                    0.,
                    hint_area.size.w,
                    SizingMode::Normal,
                )
            } else {
                self.compute_new_view_offset_fit(Some(0.), 0., hint_area.size.w, SizingMode::Normal)
            };
            hint_area.loc.x -= view_offset;
        } else {
            hint_area.loc.x -= self.view_pos();
        }

        Some(hint_area)
    }

    /// Returns the geometry of the active window relative to and clamped to the view.
    ///
    /// During animations, assumes the final view position.
    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        let col = self.columns.get(self.active_column_idx)?;

        let final_view_offset = self.view_offset.target();
        let view_off = Point::from((-final_view_offset, 0.));

        let (tile, tile_off) = col.tiles().nth(col.active_tile_idx).unwrap();

        let window_pos = view_off + tile_off + tile.window_loc();
        let window_size = tile.window_size();
        let window_rect = Rectangle::new(window_pos, window_size);

        let view = Rectangle::from_size(self.view_size);
        view.intersection(window_rect)
    }

    pub fn popup_target_rect(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
        for col in &self.columns {
            for (tile, pos) in col.tiles() {
                if tile.window().id() == id {
                    // In the scrolling layout, we try to position popups horizontally within the
                    // window geometry (so they remain visible even if the window scrolls flush with
                    // the left/right edge of the screen), and vertically within the whole parent
                    // working area.
                    let width = tile.window_size().w;
                    let height = self.parent_area.size.h;

                    let mut target = Rectangle::from_size(Size::from((width, height)));
                    target.loc.y += self.parent_area.loc.y;
                    target.loc.y -= pos.y;
                    target.loc.y -= tile.window_loc().y;

                    return Some(target);
                }
            }
        }
        None
    }

    pub fn toggle_width(&mut self, forwards: bool) {
        if self.columns.is_empty() {
            return;
        }

        let col_idx = self.active_column_idx;
        let col = &mut self.columns[col_idx];
        col.toggle_width(None, forwards);

        cancel_resize_for_column(&mut self.interactive_resize, col);
        // Refresh the cached column width so `column_x` and the
        // `auto_fit_or_center_view_offset` call that follows see the new size.
        self.data[col_idx].update(&self.columns[col_idx]);
    }

    pub fn toggle_full_width(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let col_idx = self.active_column_idx;
        let col = &mut self.columns[col_idx];
        col.toggle_full_width();

        cancel_resize_for_column(&mut self.interactive_resize, col);
        self.data[col_idx].update(&self.columns[col_idx]);
    }

    /// Apply `preset` as the width of every column in the carousel. Used by
    /// the `disable-carousel` layout option to keep all columns at the same
    /// uniform width so the workspace stays fully visible between the
    /// fixed-side panels. No-op on an empty carousel.
    /// In `disable-carousel` mode, size every column at its natural width and,
    /// when the row would overflow the area between the fixed-side panels, scale
    /// all columns down by one shared factor so the whole row just fits.
    ///
    /// Never scales above `1.0` — a column is never grown past its natural width
    /// — and the factor is uniform, so the columns' relative proportions are
    /// preserved (windows resize proportionally, never equally).
    ///
    /// The total row width is computed *including the inter-column spacing*: the
    /// columns are shrunk to fit whatever space is left after the gaps, so the
    /// whole row — columns plus gaps — always fits the area between the
    /// fixed-side panels (which are not part of the carousel).
    ///
    /// The factor is recomputed from scratch every call, never clamped to a
    /// previous value, so whenever space frees up — a window closed, a column
    /// resized smaller, a fixed-side panel shrunk — the columns grow back
    /// proportionally toward their preferred (natural) widths instead of staying
    /// stuck at an old, smaller scale.
    ///
    /// Re-centers the row afterwards. No-op when the carousel is enabled or
    /// empty.
    pub fn fit_columns_to_parent(&mut self) {
        if !self.options.layout.disable_carousel || self.columns.is_empty() {
            return;
        }
        let available = self.working_area.size.w;
        if available <= 0.0 {
            return;
        }

        let gaps = self.options.layout.gaps;
        let n = self.columns.len();
        // Gaps sit between columns and are not scaled, so the shrink factor
        // applies only to the column widths: the columns must fit in whatever
        // space is left after the inter-column gaps. (Total row width =
        // sum(columns) + gaps; this scales the columns so that total fits
        // `available`.)
        let gaps_total = gaps * n.saturating_sub(1) as f64;
        let total_columns: f64 = self
            .columns
            .iter()
            .map(|col| col.natural_width().max(1.0))
            .sum();
        let usable = available - gaps_total;

        // Shrink the row to fit when it would overflow; otherwise show every
        // column at its preferred (natural) width. The factor is shared, so the
        // user's relative widths are preserved, and it never exceeds `1.0` (no
        // column grows past its preferred width).
        //
        // Recomputed from scratch every time — never clamped to a previous
        // value — so that whenever space frees up (a column resized smaller, a
        // window closed, a fixed-side panel shrunk) the columns grow back
        // *proportionally* toward their preferred widths rather than staying
        // stuck at an old, smaller scale. The row is centered below.
        let factor = if total_columns > usable && usable > 0.0 {
            usable / total_columns
        } else {
            1.0
        };

        for (col, data) in zip(&mut self.columns, &mut self.data) {
            col.set_fit_scale(factor, true);
            cancel_resize_for_column(&mut self.interactive_resize, col);
            data.update(col);
        }

        self.auto_fit_or_center_view_offset();
    }

    /// Whether a mouse-driven interactive resize is currently in progress. Used
    /// by the commit path to avoid re-fitting (which would cancel the resize)
    /// while the user is dragging a window edge.
    pub fn is_interactive_resize_ongoing(&self) -> bool {
        self.interactive_resize.is_some()
    }

    /// Multiply each column's width by `factor`, in place, preserving the
    /// relative proportions between columns. Used by the workspace-level
    /// minimum-span enforcement so that close/resize/open events grow the
    /// carousel back up to its floor without flattening user-set ratios.
    /// No-op when `factor` is non-finite, ≤ 0, or the carousel is empty.
    pub fn scale_all_columns_widths(&mut self, factor: f64) {
        if !factor.is_finite() || factor <= 0.0 || self.columns.is_empty() {
            return;
        }
        for (col, data) in zip(&mut self.columns, &mut self.data) {
            let new_w = (data.width * factor).max(1.0);
            col.set_column_width(SizeChange::SetFixed(new_w as i32), None, true);
            cancel_resize_for_column(&mut self.interactive_resize, col);
            data.update(col);
        }
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        if self.columns.is_empty() {
            return;
        }

        let (col_idx, tile_idx) = if let Some(window) = window {
            self.columns
                .iter()
                .enumerate()
                .find_map(|(i, col)| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (i, Some(tile_idx)))
                })
                .unwrap()
        } else {
            (self.active_column_idx, None)
        };

        let col = &mut self.columns[col_idx];
        col.set_column_width(change, tile_idx, true);
        cancel_resize_for_column(&mut self.interactive_resize, col);

        // Refresh the cached column width so `column_x` (and downstream consumers like
        // `auto_fit_or_center_view_offset`, which workspace.rs calls right after this)
        // see the post-resize width.
        self.data[col_idx].update(&self.columns[col_idx]);
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        if self.columns.is_empty() {
            return;
        }

        let (col, tile_idx) = if let Some(window) = window {
            self.columns
                .iter_mut()
                .find_map(|col| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (col, Some(tile_idx)))
                })
                .unwrap()
        } else {
            (&mut self.columns[self.active_column_idx], None)
        };

        col.set_window_height(change, tile_idx, true);

        cancel_resize_for_column(&mut self.interactive_resize, col);
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        if self.columns.is_empty() {
            return;
        }

        let (col, tile_idx) = if let Some(window) = window {
            self.columns
                .iter_mut()
                .find_map(|col| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (col, Some(tile_idx)))
                })
                .unwrap()
        } else {
            (&mut self.columns[self.active_column_idx], None)
        };

        col.reset_window_height(tile_idx);

        cancel_resize_for_column(&mut self.interactive_resize, col);
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        if self.columns.is_empty() {
            return;
        }

        let (col_idx, tile_idx) = if let Some(window) = window {
            self.columns
                .iter()
                .enumerate()
                .find_map(|(i, col)| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (i, Some(tile_idx)))
                })
                .unwrap()
        } else {
            (self.active_column_idx, None)
        };

        let col = &mut self.columns[col_idx];
        col.toggle_width(tile_idx, forwards);

        cancel_resize_for_column(&mut self.interactive_resize, col);
        self.data[col_idx].update(&self.columns[col_idx]);
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        if self.columns.is_empty() {
            return;
        }

        let (col, tile_idx) = if let Some(window) = window {
            self.columns
                .iter_mut()
                .find_map(|col| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (col, Some(tile_idx)))
                })
                .unwrap()
        } else {
            (&mut self.columns[self.active_column_idx], None)
        };

        col.toggle_window_height(tile_idx, forwards);

        cancel_resize_for_column(&mut self.interactive_resize, col);
    }

    pub fn expand_column_to_available_width(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let col = &mut self.columns[self.active_column_idx];
        if !col.pending_sizing_mode().is_normal() || col.is_full_width {
            return;
        }

        if self.is_centering_focused_column() {
            // Always-centered mode is different since the active window position cannot be
            // controlled (it's always at the center). I guess you could come up with different
            // logic here that computes the width in such a way so as to leave nearby columns fully
            // on screen while taking into account that the active column will remain centered
            // after resizing. But I'm not sure it's that useful? So let's do the simple thing.
            let col = &mut self.columns[self.active_column_idx];
            col.toggle_full_width();
            cancel_resize_for_column(&mut self.interactive_resize, col);
            // Refresh the cached column width — `col.toggle_full_width()` mutates
            // the column directly and would otherwise leave `self.data` stale.
            self.data[self.active_column_idx].update(&self.columns[self.active_column_idx]);
            return;
        }

        // NOTE: This logic won't work entirely correctly with small fixed-size maximized windows
        // (they have a different area and padding).

        // Consider the end of an ongoing animation because that's what compute to fit does too.
        let view_x = self.target_view_pos();
        let working_x = self.working_area.loc.x;
        let working_w = self.working_area.size.w;

        // Count all columns that are fully visible inside the working area.
        let mut width_taken = 0.;
        let mut leftmost_col_x = None;
        let mut active_col_x = None;
        let mut counted_non_active_column = false;

        let gap = self.options.layout.gaps;
        let col_xs = self.column_xs(self.data.iter().copied());
        for (idx, col_x) in col_xs.take(self.columns.len()).enumerate() {
            if col_x < view_x + working_x + gap {
                // Column goes off-screen to the left.
                continue;
            }

            leftmost_col_x.get_or_insert(col_x);

            let width = self.data[idx].width;
            if view_x + working_x + working_w < col_x + width + gap {
                // Column goes off-screen to the right. We can stop here.
                break;
            }

            if idx == self.active_column_idx {
                active_col_x = Some(col_x);
            } else {
                counted_non_active_column = true;
            }

            width_taken += width + gap;
        }

        if active_col_x.is_none() {
            // The active column wasn't fully on screen, so we can't meaningfully do anything.
            return;
        }

        let col = &mut self.columns[self.active_column_idx];

        let available_width = working_w - gap - width_taken - col.extra_size().w;
        if available_width <= 0. {
            // Nowhere to expand.
            return;
        }

        cancel_resize_for_column(&mut self.interactive_resize, col);

        if !counted_non_active_column {
            // Only the active column was fully on-screen (maybe it's the only column), so we're
            // about to set its width to 100% of the working area. Let's do it via
            // toggle_full_width() as it lets you back out of it more intuitively.
            col.toggle_full_width();
            // Refresh the cached column width — `col.toggle_full_width()` mutates
            // the column directly and would otherwise leave `self.data` stale.
            self.data[self.active_column_idx].update(&self.columns[self.active_column_idx]);
            return;
        }

        let active_width = self.data[self.active_column_idx].width;
        col.width = ColumnWidth::Fixed(active_width + available_width);
        col.preset_width_idx = None;
        col.is_full_width = false;
        col.update_tile_sizes(true);
        // Same as above: keep `self.data` in sync with the post-resize column width.
        self.data[self.active_column_idx].update(&self.columns[self.active_column_idx]);

        // Put the leftmost window into the view.
        let new_view_x = leftmost_col_x.unwrap() - gap - working_x;
        self.animate_view_offset(self.active_column_idx, new_view_x - active_col_x.unwrap());
        // Just in case.
        self.animate_view_offset_to_column(None, self.active_column_idx, None);
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) -> bool {
        let mut col_idx = self
            .columns
            .iter()
            .position(|col| col.contains(window))
            .unwrap();

        if is_fullscreen == self.columns[col_idx].is_pending_fullscreen {
            return false;
        }

        let mut col = &mut self.columns[col_idx];
        let is_tabbed = col.display_mode == ColumnDisplay::Tabbed;

        cancel_resize_for_column(&mut self.interactive_resize, col);

        if is_fullscreen && (col.tiles.len() > 1 && !is_tabbed) {
            // This wasn't the only window in its column; extract it into a separate column.
            self.consume_or_expel_window_right(Some(window));
            col_idx += 1;
            col = &mut self.columns[col_idx];
        }

        col.set_fullscreen(is_fullscreen);

        // With place_within_column, the tab indicator changes the column size immediately.
        self.data[col_idx].update(col);

        true
    }

    pub fn set_maximized(&mut self, window: &W::Id, maximize: bool) -> bool {
        let mut col_idx = self
            .columns
            .iter()
            .position(|col| col.contains(window))
            .unwrap();

        if maximize == self.columns[col_idx].is_pending_maximized {
            return false;
        }

        let mut col = &mut self.columns[col_idx];
        let is_tabbed = col.display_mode == ColumnDisplay::Tabbed;

        cancel_resize_for_column(&mut self.interactive_resize, col);

        if maximize && (col.tiles.len() > 1 && !is_tabbed) {
            // This wasn't the only window in its column; extract it into a separate column.
            self.consume_or_expel_window_right(Some(window));
            col_idx += 1;
            col = &mut self.columns[col_idx];
        }

        col.set_maximized(maximize);

        // With place_within_column, the tab indicator changes the column size immediately.
        self.data[col_idx].update(col);

        true
    }

    pub fn render_above_top_layer(&self) -> bool {
        // Render above the top layer if we're on a fullscreen window and the view is stationary.
        if self.columns.is_empty() {
            return false;
        }

        if !self.view_offset.is_static() {
            return false;
        }

        self.columns[self.active_column_idx]
            .sizing_mode()
            .is_fullscreen()
    }

    pub fn render<R: NaruRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(ScrollingSpaceRenderElement<R>),
    ) {
        let scale = Scale::from(self.scale);

        // Draw the closing windows on top of the other windows.
        let view_rect = Rectangle::new(Point::from((self.view_pos(), 0.)), self.view_size);
        for closing in self.closing_windows.iter().rev() {
            let elem = closing.render(ctx.as_gles(), view_rect, scale);
            push(elem.into());
        }

        if self.columns.is_empty() {
            return;
        }

        let mut first = true;

        // This matches self.tiles_in_render_order().
        let view_off = Point::from((-self.view_pos(), 0.));
        for (col, col_x) in self.columns_in_render_order() {
            let col_off = Point::from((col_x, 0.));
            let col_render_off = col.render_offset();

            // Draw the tab indicator on top.
            {
                let pos = view_off + col_off + col_render_off;
                let pos = pos.to_physical_precise_round(scale).to_logical(scale);
                col.tab_indicator
                    .render(ctx.renderer, pos, &mut |elem| push(elem.into()));
            }

            for (tile, tile_off, visible) in col.tiles_in_render_order() {
                let tile_pos =
                    view_off + col_off + col_render_off + tile_off + tile.render_offset();
                // Round to physical pixels.
                let tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

                // And now the drawing logic.

                // For the active tile (which comes first), draw the focus ring.
                let focus_ring = focus_ring && first;
                first = false;

                // In the scrolling layout, we currently use visible only for hidden tabs in the
                // tabbed mode. We want to animate their opacity when going in and out of tabbed
                // mode, so we don't want to apply "visible" immediately. However, "visible" is
                // also used for input handling, and there we *do* want to apply it immediately.
                // So, let's just selectively ignore "visible" here when animating alpha.
                let visible = visible || tile.alpha_animation.is_some();
                if !visible {
                    continue;
                }

                let xray_pos = xray_pos.offset(tile_pos);
                tile.render(ctx.r(), tile_pos, xray_pos, focus_ring, &mut |elem| {
                    push(elem.into())
                });
            }
        }
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        // This matches self.tiles_with_render_positions().
        let scale = self.scale;
        let view_off = Point::from((-self.view_pos(), 0.));
        for (col, col_x) in self.columns_in_render_order() {
            let col_off = Point::from((col_x, 0.));
            let col_render_off = col.render_offset();

            // Hit the tab indicator.
            if col.display_mode == ColumnDisplay::Tabbed && col.sizing_mode().is_normal() {
                let col_pos = view_off + col_off + col_render_off;
                let col_pos = col_pos.to_physical_precise_round(scale).to_logical(scale);

                if let Some(idx) = col.tab_indicator.hit(
                    col.tab_indicator_area(),
                    col.tiles.len(),
                    scale,
                    pos - col_pos,
                ) {
                    let hit = HitType::Activate {
                        is_tab_indicator: true,
                    };
                    return Some((col.tiles[idx].window(), hit));
                }
            }

            for (tile, tile_off, visible) in col.tiles_in_render_order() {
                if !visible {
                    continue;
                }

                let tile_pos =
                    view_off + col_off + col_render_off + tile_off + tile.render_offset();
                // Round to physical pixels.
                let tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

                if let Some(rv) = HitType::hit_tile(tile, tile_pos, pos) {
                    return Some(rv);
                }
            }
        }

        None
    }

    pub fn view_offset_gesture_begin(&mut self, is_touchpad: bool) {
        if self.columns.is_empty() {
            return;
        }

        if self.interactive_resize.is_some() {
            return;
        }

        let gesture = ViewGesture {
            current_view_offset: self.view_offset.current(),
            animation: None,
            tracker: SwipeTracker::new(),
            delta_from_tracker: self.view_offset.current(),
            stationary_view_offset: self.view_offset.stationary(),
            is_touchpad,
            dnd_last_event_time: None,
            dnd_nonzero_start_time: None,
        };
        self.view_offset = ViewOffset::Gesture(gesture);
    }

    pub fn dnd_scroll_gesture_begin(&mut self) {
        if let ViewOffset::Gesture(ViewGesture {
            dnd_last_event_time: Some(_),
            ..
        }) = &self.view_offset
        {
            // Already active.
            return;
        }

        let gesture = ViewGesture {
            current_view_offset: self.view_offset.current(),
            animation: None,
            tracker: SwipeTracker::new(),
            delta_from_tracker: self.view_offset.current(),
            stationary_view_offset: self.view_offset.stationary(),
            is_touchpad: false,
            dnd_last_event_time: Some(self.clock.now_unadjusted()),
            dnd_nonzero_start_time: None,
        };
        self.view_offset = ViewOffset::Gesture(gesture);

        self.interactive_resize = None;
    }

    pub fn view_offset_gesture_update(
        &mut self,
        delta_x: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
            return None;
        };

        if gesture.is_touchpad != is_touchpad || gesture.dnd_last_event_time.is_some() {
            return None;
        }

        gesture.tracker.push(delta_x, timestamp);

        let norm_factor = if gesture.is_touchpad {
            self.working_area.size.w / VIEW_GESTURE_WORKING_AREA_MOVEMENT
        } else {
            1.
        };
        let pos = gesture.tracker.pos() * norm_factor;
        let view_offset = pos + gesture.delta_from_tracker;
        gesture.current_view_offset = view_offset;

        Some(true)
    }

    pub fn dnd_scroll_gesture_scroll(&mut self, delta: f64) -> bool {
        let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
            return false;
        };

        let Some(last_time) = gesture.dnd_last_event_time else {
            // Not a DnD scroll.
            return false;
        };

        let config = &self.options.gestures.dnd_edge_view_scroll;

        let now = self.clock.now_unadjusted();
        gesture.dnd_last_event_time = Some(now);

        if delta == 0. {
            // We're outside the scrolling zone.
            gesture.dnd_nonzero_start_time = None;
            return false;
        }

        let nonzero_start = *gesture.dnd_nonzero_start_time.get_or_insert(now);

        // Delay starting the gesture a bit to avoid unwanted movement when dragging across
        // monitors.
        let delay = Duration::from_millis(u64::from(config.delay_ms));
        if now.saturating_sub(nonzero_start) < delay {
            return true;
        }

        let time_delta = now.saturating_sub(last_time).as_secs_f64();

        let delta = delta * time_delta * config.max_speed;

        gesture.tracker.push(delta, now);

        let view_offset = gesture.tracker.pos() + gesture.delta_from_tracker;

        // Clamp it so that it doesn't go too much out of bounds.
        let (leftmost, rightmost) = if self.columns.is_empty() {
            (0., 0.)
        } else {
            let gaps = self.options.layout.gaps;

            let mut leftmost = -self.working_area.size.w;

            let last_col_idx = self.columns.len() - 1;
            let last_col_x = self
                .columns
                .iter()
                .take(last_col_idx)
                .fold(0., |col_x, col| col_x + col.width() + gaps);
            let last_col_width = self.data[last_col_idx].width;
            let mut rightmost = last_col_x + last_col_width - self.working_area.loc.x;

            let active_col_x = self
                .columns
                .iter()
                .take(self.active_column_idx)
                .fold(0., |col_x, col| col_x + col.width() + gaps);
            leftmost -= active_col_x;
            rightmost -= active_col_x;

            (leftmost, rightmost)
        };
        let min_offset = f64::min(leftmost, rightmost);
        let max_offset = f64::max(leftmost, rightmost);
        let clamped_offset = view_offset.clamp(min_offset, max_offset);

        gesture.delta_from_tracker += clamped_offset - view_offset;
        gesture.current_view_offset = clamped_offset;
        true
    }

    pub fn view_offset_gesture_end(&mut self, is_touchpad: Option<bool>) -> bool {
        let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
            return false;
        };

        if is_touchpad.is_some_and(|x| gesture.is_touchpad != x) {
            return false;
        }

        // We do not handle cancelling, just like GNOME Shell doesn't. For this gesture, proper
        // cancelling would require keeping track of the original active column, and then updating
        // it in all the right places (adding columns, removing columns, etc.) -- quite a bit of
        // effort and bug potential.

        // Take into account any idle time between the last event and now.
        let now = self.clock.now_unadjusted();
        gesture.tracker.push(0., now);

        let norm_factor = if gesture.is_touchpad {
            self.working_area.size.w / VIEW_GESTURE_WORKING_AREA_MOVEMENT
        } else {
            1.
        };
        let velocity = gesture.tracker.velocity() * norm_factor;
        let pos = gesture.tracker.pos() * norm_factor;
        let current_view_offset = pos + gesture.delta_from_tracker;

        if self.columns.is_empty() {
            self.view_offset = ViewOffset::Static(current_view_offset);
            return true;
        }

        // Figure out where the gesture would stop after deceleration.
        let end_pos = gesture.tracker.projected_end_pos() * norm_factor;
        let target_view_offset = end_pos + gesture.delta_from_tracker;

        // Compute the snapping points. These are where the view aligns with column boundaries on
        // either side.
        struct Snap {
            // View position relative to x = 0 (the first column).
            view_pos: f64,
            // Column to activate for this snapping point.
            col_idx: usize,
        }

        let mut snapping_points = Vec::new();

        if self.is_centering_focused_column() {
            let mut col_x = 0.;
            for (col_idx, col) in self.columns.iter().enumerate() {
                let col_w = col.width();
                let mode = col.sizing_mode();

                let area = if mode.is_maximized() {
                    self.parent_area
                } else {
                    self.working_area
                };

                let left_strut = area.loc.x;

                let view_pos = if mode.is_fullscreen() {
                    col_x
                } else if area.size.w <= col_w {
                    col_x - left_strut
                } else {
                    col_x - (area.size.w - col_w) / 2. - left_strut
                };
                snapping_points.push(Snap { view_pos, col_idx });

                col_x += col_w + self.options.layout.gaps;
            }
        } else {
            let center_on_overflow = matches!(
                self.options.layout.center_focused_column,
                CenterFocusedColumn::OnOverflow
            );

            let view_width = self.view_size.w;
            let gaps = self.options.layout.gaps;

            let snap_points =
                |col_x, col: &Column<W>, prev_col_w: Option<f64>, next_col_w: Option<f64>| {
                    let col_w = col.width();
                    let mode = col.sizing_mode();

                    let area = if mode.is_maximized() {
                        self.parent_area
                    } else {
                        self.working_area
                    };

                    let left_strut = area.loc.x;
                    let right_strut = self.view_size.w - area.size.w - area.loc.x;

                    // Normal columns align with the working area, but fullscreen columns align with
                    // the view size.
                    if mode.is_fullscreen() {
                        let left = col_x;
                        let right = left + col_w;
                        (left, right)
                    } else {
                        // Logic from compute_new_view_offset.
                        let padding = if mode.is_maximized() {
                            0.
                        } else {
                            ((area.size.w - col_w) / 2.).clamp(0., gaps)
                        };

                        let center = if area.size.w <= col_w {
                            col_x - left_strut
                        } else {
                            col_x - (area.size.w - col_w) / 2. - left_strut
                        };
                        let is_overflowing = |adj_col_w: Option<f64>| {
                            center_on_overflow
                                && adj_col_w
                                    .filter(|adj_col_w| {
                                        // NOTE: This logic won't work entirely correctly with small
                                        // fixed-size maximized windows (they have a different area
                                        // and padding).
                                        center_on_overflow
                                            && adj_col_w + 3.0 * gaps + col_w > area.size.w
                                    })
                                    .is_some()
                        };

                        let left = if is_overflowing(next_col_w) {
                            center
                        } else {
                            col_x - padding - left_strut
                        };
                        let right = if is_overflowing(prev_col_w) {
                            center + view_width
                        } else {
                            col_x + col_w + padding + right_strut
                        };
                        (left, right)
                    }
                };

            // Prevent the gesture from snapping further than the first/last column, as this is
            // generally undesired.
            //
            // It's ok if leftmost_snap is > rightmost_snap (this happens if the columns on a
            // workspace total up to less than the workspace width).

            // The first column's left snap isn't actually guaranteed to be the *leftmost* snap.
            // With weird enough left strut and perhaps a maximized small fixed-size window, you
            // can make the second window's left snap be further to the left than the first
            // window's. The same goes for the rightmost snap.
            //
            // This isn't actually a big problem because it's very much an obscure edge case. Just
            // need to make sure the code doesn't panic when that happens.
            let leftmost_snap = snap_points(
                0.,
                &self.columns[0],
                None,
                self.columns.get(1).map(|c| c.width()),
            )
            .0;
            let last_col_idx = self.columns.len() - 1;
            let last_col_x = self
                .columns
                .iter()
                .take(last_col_idx)
                .fold(0., |col_x, col| col_x + col.width() + gaps);
            let rightmost_snap = snap_points(
                last_col_x,
                &self.columns[last_col_idx],
                last_col_idx
                    .checked_sub(1)
                    .and_then(|idx| self.columns.get(idx).map(|c| c.width())),
                None,
            )
            .1 - view_width;

            snapping_points.push(Snap {
                view_pos: leftmost_snap,
                col_idx: 0,
            });
            snapping_points.push(Snap {
                view_pos: rightmost_snap,
                col_idx: last_col_idx,
            });

            let mut push = |col_idx, left, right| {
                if leftmost_snap < left && left < rightmost_snap {
                    snapping_points.push(Snap {
                        view_pos: left,
                        col_idx,
                    });
                }

                let right = right - view_width;
                if leftmost_snap < right && right < rightmost_snap {
                    snapping_points.push(Snap {
                        view_pos: right,
                        col_idx,
                    });
                }
            };

            let mut col_x = 0.;
            for (col_idx, col) in self.columns.iter().enumerate() {
                let (left, right) = snap_points(
                    col_x,
                    col,
                    col_idx
                        .checked_sub(1)
                        .and_then(|idx| self.columns.get(idx).map(|c| c.width())),
                    self.columns.get(col_idx + 1).map(|c| c.width()),
                );
                push(col_idx, left, right);

                col_x += col.width() + gaps;
            }
        }

        // Find the closest snapping point.
        snapping_points.sort_by_key(|snap| NotNan::new(snap.view_pos).unwrap());

        let active_col_x = self.column_x(self.active_column_idx);
        let target_view_pos = active_col_x + target_view_offset;
        let target_snap = snapping_points
            .iter()
            .min_by_key(|snap| NotNan::new((snap.view_pos - target_view_pos).abs()).unwrap())
            .unwrap();

        let mut new_col_idx = target_snap.col_idx;

        if !self.is_centering_focused_column() {
            // Focus the furthest window towards the direction of the gesture.
            if target_view_offset >= current_view_offset {
                for col_idx in (new_col_idx + 1)..self.columns.len() {
                    let col = &self.columns[col_idx];
                    let col_x = self.column_x(col_idx);
                    let col_w = col.width();
                    let mode = col.sizing_mode();

                    let area = if mode.is_maximized() {
                        self.parent_area
                    } else {
                        self.working_area
                    };

                    let left_strut = area.loc.x;

                    if mode.is_fullscreen() {
                        if target_snap.view_pos + self.view_size.w < col_x + col_w {
                            break;
                        }
                    } else {
                        let padding = if mode.is_maximized() {
                            0.
                        } else {
                            ((area.size.w - col_w) / 2.).clamp(0., self.options.layout.gaps)
                        };

                        if target_snap.view_pos + left_strut + area.size.w < col_x + col_w + padding
                        {
                            break;
                        }
                    }

                    new_col_idx = col_idx;
                }
            } else {
                for col_idx in (0..new_col_idx).rev() {
                    let col = &self.columns[col_idx];
                    let col_x = self.column_x(col_idx);
                    let col_w = col.width();
                    let mode = col.sizing_mode();

                    let area = if mode.is_maximized() {
                        self.parent_area
                    } else {
                        self.working_area
                    };

                    let left_strut = area.loc.x;

                    if mode.is_fullscreen() {
                        if col_x < target_snap.view_pos {
                            break;
                        }
                    } else {
                        let padding = if mode.is_maximized() {
                            0.
                        } else {
                            ((area.size.w - col_w) / 2.).clamp(0., self.options.layout.gaps)
                        };

                        if col_x - padding < target_snap.view_pos + left_strut {
                            break;
                        }
                    }

                    new_col_idx = col_idx;
                }
            }
        }

        let new_col_x = self.column_x(new_col_idx);
        let delta = active_col_x - new_col_x;

        if self.active_column_idx != new_col_idx {
            self.view_offset_to_restore = None;
        }

        self.active_column_idx = new_col_idx;

        let target_view_offset = target_snap.view_pos - new_col_x;

        self.view_offset = ViewOffset::Animation(Animation::new(
            self.clock.clone(),
            current_view_offset + delta,
            target_view_offset,
            velocity,
            self.options.animations.horizontal_view_movement.0,
        ));

        // HACK: deal with things like snapping to the right edge of a larger-than-view window.
        self.animate_view_offset_to_column(None, new_col_idx, None);

        true
    }

    pub fn dnd_scroll_gesture_end(&mut self) {
        let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
            return;
        };

        if gesture.dnd_last_event_time.is_some() && gesture.tracker.pos() == 0. {
            // DnD didn't scroll anything, so preserve the current view position (rather than
            // snapping the window).

            // If there's an ongoing animation within the gesture (e.g. from a window being removed
            // during DnD), preserve it.
            if let Some(mut anim) = gesture.animation.take() {
                anim.offset(gesture.current_view_offset);
                self.view_offset = ViewOffset::Animation(anim);
            } else {
                self.view_offset = ViewOffset::Static(gesture.delta_from_tracker);
            }

            if !self.columns.is_empty() {
                // Just in case, make sure the active window remains on screen.
                self.animate_view_offset_to_column(None, self.active_column_idx, None);
            }
            return;
        }

        self.view_offset_gesture_end(None);
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        if self.interactive_resize.is_some() {
            return false;
        }

        let col = self
            .columns
            .iter_mut()
            .find(|col| col.contains(&window))
            .unwrap();

        if !col.pending_sizing_mode().is_normal() {
            return false;
        }

        // In disable-carousel mode the rendered tile width is the *natural*
        // column width times `fit_scale`, while the update path sets the natural
        // width via `set_column_width(SetFixed(..))`. Capture the natural window
        // width as the resize base so feeding it straight back is a no-op at zero
        // delta. Capturing the scaled `window_size()` instead would be re-scaled
        // on the first motion and snap the window to a smaller fixed width.
        let natural_col_width = col.natural_width();

        let tile = col
            .tiles
            .iter_mut()
            .find(|tile| tile.window().id() == &window)
            .unwrap();

        let mut original_window_size = tile.window_size();
        original_window_size.w = tile.window_width_for_tile_width(natural_col_width);

        let resize = InteractiveResize {
            window,
            original_window_size,
            data: InteractiveResizeData { edges },
        };
        self.interactive_resize = Some(resize);

        self.view_offset.stop_anim_and_gesture();

        true
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        let Some(resize) = &self.interactive_resize else {
            return false;
        };

        if window != &resize.window {
            return false;
        }

        let is_centering = self.is_centering_focused_column();

        let col = self
            .columns
            .iter_mut()
            .find(|col| col.contains(window))
            .unwrap();

        let tile_idx = col
            .tiles
            .iter()
            .position(|tile| tile.window().id() == window)
            .unwrap();

        if resize.data.edges.intersects(ResizeEdge::LEFT_RIGHT) {
            let mut dx = delta.x;
            if resize.data.edges.contains(ResizeEdge::LEFT) {
                dx = -dx;
            };

            if is_centering {
                dx *= 2.;
            }

            // The cursor delta is in rendered (post-`fit_scale`) space; convert it
            // back to natural width so the SetFixed below — which sets the natural
            // column width — tracks the cursor instead of being re-scaled by the
            // disable-carousel fit. `fit_scale` is 1.0 in every other mode.
            let fit_scale = col.fit_scale;
            if fit_scale > 0.0 {
                dx /= fit_scale;
            }

            let window_width = (resize.original_window_size.w + dx).round() as i32;
            col.set_column_width(SizeChange::SetFixed(window_width), Some(tile_idx), false);
        }

        if resize.data.edges.intersects(ResizeEdge::TOP_BOTTOM) {
            // Prevent the simplest case of weird resizing (top edge when this is the topmost
            // window).
            if !(resize.data.edges.contains(ResizeEdge::TOP) && tile_idx == 0) {
                let mut dy = delta.y;
                if resize.data.edges.contains(ResizeEdge::TOP) {
                    dy = -dy;
                };

                // FIXME: some smarter height distribution would be nice here so that vertical
                // resizes work as expected in more cases.

                let window_height = (resize.original_window_size.h + dy).round() as i32;
                col.set_window_height(SizeChange::SetFixed(window_height), Some(tile_idx), false);
            }
        }

        true
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        let Some(resize) = &self.interactive_resize else {
            return;
        };

        let resized_window = resize.window.clone();

        if let Some(window) = window {
            if window != &resize.window {
                return;
            }

            // Animate the active window into view right away.
            if self.columns[self.active_column_idx].contains(window) {
                self.animate_view_offset_to_column(None, self.active_column_idx, None);
            }
        }

        self.interactive_resize = None;

        // Interactive resize mutates column width via `col.set_column_width(..)` directly
        // (in interactive_resize_update), bypassing the wrapper that refreshes
        // `self.data[col_idx].width`. Refresh it now so subsequent layout math sees the
        // post-resize width, then re-run auto-fit/center so the view recentres if the
        // total content shrunk to fit the workspace.
        if let Some(col_idx) = self
            .columns
            .iter()
            .position(|col| col.contains(&resized_window))
        {
            self.data[col_idx].update(&self.columns[col_idx]);
        }
        self.auto_fit_or_center_view_offset();
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        for (col_idx, col) in self.columns.iter_mut().enumerate() {
            let mut col_resize_data = None;
            if let Some(resize) = &self.interactive_resize {
                if col.contains(&resize.window) {
                    col_resize_data = Some(resize.data);
                }
            }

            let is_tabbed = col.display_mode == ColumnDisplay::Tabbed;
            let extra_size = col.extra_size();

            // If transactions are disabled, also disable combined throttling, for more intuitive
            // behavior. In tabbed display mode, only one window is visible, so individual
            // throttling makes more sense.
            let individual_throttling = self.options.disable_transactions || is_tabbed;

            let intent = if self.options.disable_resize_throttling {
                ConfigureIntent::CanSend
            } else if individual_throttling {
                // In this case, we don't use combined throttling, but rather compute throttling
                // individually below.
                ConfigureIntent::CanSend
            } else {
                col.tiles
                    .iter()
                    .fold(ConfigureIntent::NotNeeded, |intent, tile| {
                        match (intent, tile.window().configure_intent()) {
                            (_, ConfigureIntent::ShouldSend) => ConfigureIntent::ShouldSend,
                            (ConfigureIntent::NotNeeded, tile_intent) => tile_intent,
                            (ConfigureIntent::CanSend, ConfigureIntent::Throttled) => {
                                ConfigureIntent::Throttled
                            }
                            (intent, _) => intent,
                        }
                    })
            };

            for (tile_idx, tile) in col.tiles.iter_mut().enumerate() {
                let win = tile.window_mut();

                let active_in_column = col.active_tile_idx == tile_idx;
                win.set_active_in_column(active_in_column);
                win.set_floating(false);

                let mut active = is_active && self.active_column_idx == col_idx;
                if self.options.deactivate_unfocused_windows {
                    active &= active_in_column && is_focused;
                } else {
                    // In tabbed mode, all tabs have activated state to reduce unnecessary
                    // animations when switching tabs.
                    active &= active_in_column || is_tabbed;
                }
                win.set_activated(active);

                win.set_interactive_resize(col_resize_data);

                let border_config = self.options.layout.border.merged_with(&win.rules().border);
                let bounds = compute_toplevel_bounds(
                    border_config,
                    self.working_area.size,
                    extra_size,
                    self.options.layout.gaps,
                );
                win.set_bounds(bounds);

                let intent = if individual_throttling {
                    win.configure_intent()
                } else {
                    intent
                };

                if matches!(
                    intent,
                    ConfigureIntent::CanSend | ConfigureIntent::ShouldSend
                ) {
                    win.send_pending_configure();
                }

                win.refresh();
            }
        }
    }

    #[cfg(test)]
    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn parent_area(&self) -> Rectangle<f64, Logical> {
        self.parent_area
    }

    #[cfg(test)]
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    #[cfg(test)]
    pub fn options(&self) -> &Rc<Options> {
        &self.options
    }

    #[cfg(test)]
    pub fn active_column_idx(&self) -> usize {
        self.active_column_idx
    }

    #[cfg(test)]
    pub(super) fn view_offset(&self) -> &ViewOffset {
        &self.view_offset
    }

    #[cfg(test)]
    pub fn verify_invariants(&self) {
        assert!(self.view_size.w > 0.);
        assert!(self.view_size.h > 0.);
        assert!(self.scale > 0.);
        assert!(self.scale.is_finite());
        assert_eq!(self.columns.len(), self.data.len());
        assert_eq!(
            self.working_area,
            compute_working_area(self.parent_area, self.scale, self.options.layout.struts)
        );

        if !self.columns.is_empty() {
            assert!(self.active_column_idx < self.columns.len());

            for (column, data) in zip(&self.columns, &self.data) {
                assert!(Rc::ptr_eq(&self.options, &column.options));
                assert_eq!(self.clock, column.clock);
                assert_eq!(self.scale, column.scale);
                column.verify_invariants();

                let mut data2 = *data;
                data2.update(column);
                assert_eq!(data, &data2, "column data must be up to date");
            }

            let col = &self.columns[self.active_column_idx];

            if self.view_offset_to_restore.is_some() {
                assert!(
                    !col.sizing_mode().is_normal(),
                    "when view_offset_to_restore is set, \
                     the active column must be fullscreen or maximized"
                );
            }
        }

        if let Some(resize) = &self.interactive_resize {
            assert!(
                self.columns
                    .iter()
                    .flat_map(|col| &col.tiles)
                    .any(|tile| tile.window().id() == &resize.window),
                "interactive resize window must be present in the layout"
            );
        }
    }
}

impl ViewOffset {
    /// Returns the current view offset.
    pub fn current(&self) -> f64 {
        match self {
            ViewOffset::Static(offset) => *offset,
            ViewOffset::Animation(anim) => anim.value(),
            ViewOffset::Gesture(gesture) => {
                gesture.current_view_offset
                    + gesture.animation.as_ref().map_or(0., |anim| anim.value())
            }
        }
    }

    /// Returns the target view offset suitable for computing the new view offset.
    pub fn target(&self) -> f64 {
        match self {
            ViewOffset::Static(offset) => *offset,
            ViewOffset::Animation(anim) => anim.to(),
            // This can be used for example if a gesture is interrupted.
            ViewOffset::Gesture(gesture) => gesture.current_view_offset,
        }
    }

    /// Returns a view offset value suitable for saving and later restoration.
    ///
    /// This means that it shouldn't return an in-progress animation or gesture value.
    fn stationary(&self) -> f64 {
        match self {
            ViewOffset::Static(offset) => *offset,
            // For animations we can return the final value.
            ViewOffset::Animation(anim) => anim.to(),
            ViewOffset::Gesture(gesture) => gesture.stationary_view_offset,
        }
    }

    pub fn is_static(&self) -> bool {
        matches!(self, Self::Static(_))
    }

    pub fn is_gesture(&self) -> bool {
        matches!(self, Self::Gesture(_))
    }

    pub fn is_dnd_scroll(&self) -> bool {
        matches!(&self, ViewOffset::Gesture(gesture) if gesture.dnd_last_event_time.is_some())
    }

    pub fn is_animation_ongoing(&self) -> bool {
        match self {
            ViewOffset::Static(_) => false,
            ViewOffset::Animation(_) => true,
            ViewOffset::Gesture(gesture) => gesture.animation.is_some(),
        }
    }

    pub fn offset(&mut self, delta: f64) {
        match self {
            ViewOffset::Static(offset) => *offset += delta,
            ViewOffset::Animation(anim) => anim.offset(delta),
            ViewOffset::Gesture(gesture) => {
                gesture.stationary_view_offset += delta;
                gesture.delta_from_tracker += delta;
                gesture.current_view_offset += delta;
            }
        }
    }

    pub fn cancel_gesture(&mut self) {
        if let ViewOffset::Gesture(gesture) = self {
            *self = ViewOffset::Static(gesture.current_view_offset);
        }
    }

    pub fn stop_anim_and_gesture(&mut self) {
        *self = ViewOffset::Static(self.current());
    }
}

impl ViewGesture {
    fn animate_from(&mut self, from: f64, clock: Clock, config: naru_config::Animation) {
        let current = self.animation.as_ref().map_or(0., Animation::value);
        self.animation = Some(Animation::new(clock, from + current, 0., 0., config));
    }
}

impl ColumnData {
    pub fn new<W: LayoutElement>(column: &Column<W>) -> Self {
        let mut rv = Self { width: 0. };
        rv.update(column);
        rv
    }

    pub fn update<W: LayoutElement>(&mut self, column: &Column<W>) {
        self.width = column.width();
    }
}

impl TileData {
    pub fn new<W: LayoutElement>(tile: &Tile<W>, height: WindowHeight) -> Self {
        let mut rv = Self {
            height,
            size: Size::default(),
            interactively_resizing_by_left_edge: false,
        };
        rv.update(tile);
        rv
    }

    pub fn update<W: LayoutElement>(&mut self, tile: &Tile<W>) {
        self.size = tile.tile_size();
        self.interactively_resizing_by_left_edge = tile
            .window()
            .interactive_resize_data()
            .is_some_and(|data| data.edges.contains(ResizeEdge::LEFT));
    }
}

impl From<PresetSize> for ColumnWidth {
    fn from(value: PresetSize) -> Self {
        match value {
            PresetSize::Proportion(p) => Self::Proportion(p.clamp(0., 10000.)),
            PresetSize::Fixed(f) => Self::Fixed(f64::from(f.clamp(1, 100000))),
        }
    }
}

impl WindowHeight {
    const fn auto_1() -> Self {
        Self::Auto { weight: 1. }
    }
}

impl<W: LayoutElement> Column<W> {
    #[allow(clippy::too_many_arguments)]
    fn new_with_tile(
        tile: Tile<W>,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        width: ColumnWidth,
        is_full_width: bool,
    ) -> Self {
        let options = tile.options.clone();

        let display_mode = tile
            .window()
            .rules()
            .default_column_display
            .unwrap_or(options.layout.default_column_display);

        // Try to match width to a preset width. Consider the following case: a terminal (foot)
        // sizes itself to the terminal grid. We open it with default-column-width 0.5. It shrinks
        // by a few pixels to evenly match the terminal grid. Then we press
        // switch-preset-column-width intending to go to proportion 0.667, but the preset width
        // matching code picks the proportion 0.5 preset because it's the next smallest width after
        // the current foot's window width. Effectively, this makes the first
        // switch-preset-column-width press ignored.
        //
        // However, here, we do know that width = proportion 0.5 (regardless of what the window
        // opened with), and we can match it to a preset right away, if one exists.
        let preset_width_idx = options
            .layout
            .preset_column_widths
            .iter()
            .position(|preset| width == ColumnWidth::from(*preset));

        let mut rv = Self {
            tiles: vec![],
            data: vec![],
            active_tile_idx: 0,
            width,
            preset_width_idx,
            fit_scale: 1.0,
            is_full_width,
            is_pending_maximized: false,
            is_pending_fullscreen: false,
            display_mode,
            tab_indicator: TabIndicator::new(options.layout.tab_indicator),
            move_animation: None,
            view_size,
            working_area,
            parent_area,
            scale,
            clock: tile.clock.clone(),
            options,
        };

        let pending_sizing_mode = tile.window().pending_sizing_mode();

        rv.add_tile_at(0, tile);

        match pending_sizing_mode {
            SizingMode::Normal => (),
            SizingMode::Maximized => rv.set_maximized(true),
            SizingMode::Fullscreen => rv.set_fullscreen(true),
        }

        // Animate the tab indicator for new columns.
        if display_mode == ColumnDisplay::Tabbed
            && !rv.options.layout.tab_indicator.hide_when_single_tab
            && rv.sizing_mode().is_normal()
        {
            // Usually new columns are created together with window movement actions. For new
            // windows, we handle that in start_open_animation().
            rv.tab_indicator
                .start_open_animation(rv.clock.clone(), rv.options.animations.window_movement.0);
        }

        rv
    }

    fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let mut update_sizes = false;

        if self.view_size != view_size
            || self.working_area != working_area
            || self.parent_area != parent_area
        {
            update_sizes = true;
        }

        // If preset widths changed, clear our stored preset index.
        if self.options.layout.preset_column_widths != options.layout.preset_column_widths {
            self.preset_width_idx = None;
        }

        // If preset heights changed, make our heights non-preset.
        if self.options.layout.preset_window_heights != options.layout.preset_window_heights {
            self.convert_heights_to_auto();
            update_sizes = true;
        }

        if self.options.layout.gaps != options.layout.gaps {
            update_sizes = true;
        }

        if self.options.layout.border.off != options.layout.border.off
            || self.options.layout.border.width != options.layout.border.width
        {
            update_sizes = true;
        }

        if self.options.layout.tab_indicator != options.layout.tab_indicator {
            update_sizes = true;
        }

        for (tile, data) in zip(&mut self.tiles, &mut self.data) {
            tile.update_config(view_size, scale, options.clone());
            data.update(tile);
        }

        self.tab_indicator
            .update_config(options.layout.tab_indicator);
        self.view_size = view_size;
        self.working_area = working_area;
        self.parent_area = parent_area;
        self.scale = scale;
        self.options = options;

        if update_sizes {
            self.update_tile_sizes(false);
        }
    }

    pub fn update_shaders(&mut self) {
        for tile in &mut self.tiles {
            tile.update_shaders();
        }

        self.tab_indicator.update_shaders();
    }

    pub fn advance_animations(&mut self) {
        if let Some(move_) = &mut self.move_animation {
            if move_.anim.is_done() {
                self.move_animation = None;
            }
        }

        for tile in &mut self.tiles {
            tile.advance_animations();
        }

        self.tab_indicator.advance_animations();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.move_animation.is_some()
            || self.tab_indicator.are_animations_ongoing()
            || self.tiles.iter().any(Tile::are_animations_ongoing)
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.move_animation.is_some()
            || self.tab_indicator.are_animations_ongoing()
            || self.tiles.iter().any(Tile::are_transitions_ongoing)
    }

    pub fn update_render_elements(&mut self, is_active: bool, view_rect: Rectangle<f64, Logical>) {
        let active_idx = self.active_tile_idx;
        for (tile_idx, (tile, tile_off)) in self.tiles_mut().enumerate() {
            let is_active = is_active && tile_idx == active_idx;

            let mut tile_view_rect = view_rect;
            tile_view_rect.loc -= tile_off + tile.render_offset();
            tile.update_render_elements(is_active, tile_view_rect);
        }

        let config = self.tab_indicator.config();
        let offsets = self.tile_offsets_iter(self.data.iter().copied());
        let tabs = zip(&self.tiles, offsets)
            .enumerate()
            .map(|(tile_idx, (tile, tile_off))| {
                let is_active = tile_idx == active_idx;
                let is_urgent = tile.window().is_urgent();
                let tile_pos = tile_off + tile.render_offset();
                TabInfo::from_tile(tile, tile_pos, is_active, is_urgent, &config)
            });

        // Hide the tab indicator in fullscreen. If you have it configured to overlap the window,
        // you don't want that to happen in fullscreen. Also, laying things out correctly when the
        // tab indicator is within the column and the column goes fullscreen, would require too
        // many changes to the code for too little benefit (it's mostly invisible anyway).
        let enabled = self.display_mode == ColumnDisplay::Tabbed && self.sizing_mode().is_normal();

        self.tab_indicator.update_render_elements(
            enabled,
            self.tab_indicator_area(),
            view_rect,
            self.tiles.len(),
            tabs,
            is_active,
            self.scale,
        );
    }

    pub fn is_pending_fullscreen(&self) -> bool {
        self.is_pending_fullscreen
    }

    pub fn is_pending_maximized(&self) -> bool {
        self.is_pending_maximized
    }

    pub fn pending_sizing_mode(&self) -> SizingMode {
        if self.is_pending_fullscreen {
            SizingMode::Fullscreen
        } else if self.is_pending_maximized {
            SizingMode::Maximized
        } else {
            SizingMode::Normal
        }
    }

    pub fn render_offset(&self) -> Point<f64, Logical> {
        let mut offset = Point::from((0., 0.));

        if let Some(move_) = &self.move_animation {
            offset.x += move_.from * move_.anim.value();
        }

        offset
    }

    pub fn animate_move_from(&mut self, from_x_offset: f64) {
        self.animate_move_from_with_config(
            from_x_offset,
            self.options.animations.window_movement.0,
        );
    }

    pub fn animate_move_from_with_config(
        &mut self,
        from_x_offset: f64,
        config: naru_config::Animation,
    ) {
        let current_offset = self
            .move_animation
            .as_ref()
            .map_or(0., |move_| move_.from * move_.anim.value());

        let anim = Animation::new(self.clock.clone(), 1., 0., 0., config);
        self.move_animation = Some(MoveAnimation {
            anim,
            from: from_x_offset + current_offset,
        });
    }

    pub fn offset_move_anim_current(&mut self, offset: f64) {
        if let Some(move_) = self.move_animation.as_mut() {
            // If the anim is almost done, there's little point trying to offset it; we can let
            // things jump. If it turns out like a bad idea, we could restart the anim instead.
            let value = move_.anim.value();
            if value > 0.001 {
                move_.from += offset / value;
            }
        }
    }

    /// Returns whether this column is currently fullscreen.
    ///
    /// As in, if it contains one currently-fullscreen tile, or in tabbed mode, if it contains at
    /// least one currently-fullscreen tile.
    ///
    /// This will lag behind is_pending_fullscreen, depending on when the tiles actually respond to
    /// the un/fullscreen request. But, it's possible for is_fullscreen() to flip instantly, for
    /// example when consuming a fullscreen tile into a non-pending-fullscreen column.
    ///
    /// This controls things like:
    ///
    /// - whether the column draws at the top of the screen or at the start of the working area
    /// - whether the column draws above the top layer-shell layer
    /// - whether the tab indicator is shown
    /// - restoring view_offset_before_fullscreen
    ///
    /// Edge cases to watch out for:
    ///
    /// - Consuming a fullscreen tile into a non-tabbed column will keep that tile fullscreen until
    ///   it responds to the unfullscreen request. This tile may be anywhere in the column,
    ///   including at the active position.
    ///
    /// - Changing a fullscreen tabbed column into normal mode is an easy way to get randomly
    ///   delayed unfullscreening tiles in a normal column.
    ///
    /// - is_fullscreen() can suddenly change when consuming/expelling a fullscreen tile into/from a
    ///   non-fullscreen column. This can influence the code that saves/restores the unfullscreen
    ///   view offset.
    fn sizing_mode(&self) -> SizingMode {
        // Behaviors that we want:
        //
        // 1. The common case: single tile in a column. Assume no animations. Fullscreening the tile
        //    should make it jump to the top-left of the screen only when the tile finishes
        //    fullscreening. Similarly, unfullscreening should keep it at the top-left until the
        //    tile had unfullscreened.
        //
        // 2. Unfullscreening a tabbed column with multiple tiles should restore the view offset
        //    correctly. This means waiting for *all* tiles to unfullscreen, because otherwise the
        //    restored view offset will immediately get overwritten by the still screen-wide column
        //    (it uses the largest tile's width).
        //
        // 3. Changing a fullscreen tabbed column to normal should probably also restore the view
        //    offset correctly. Same problem as above, but now for normal columns (since display
        //    mode change applies instantly).
        //
        // The logic that satisfies these behaviors is to check if *any* tile is fullscreen.
        let mut any_fullscreen = false;
        let mut any_maximized = false;
        for tile in &self.tiles {
            match tile.sizing_mode() {
                SizingMode::Normal => (),
                SizingMode::Maximized => any_maximized = true,
                SizingMode::Fullscreen => any_fullscreen = true,
            }
        }

        if any_fullscreen {
            SizingMode::Fullscreen
        } else if any_maximized {
            SizingMode::Maximized
        } else {
            SizingMode::Normal
        }
    }

    pub fn contains(&self, window: &W::Id) -> bool {
        // Scan the entire WindowStack of each tile, not just the active window —
        // otherwise a stacked-but-currently-hidden window appears "not present" and
        // activation silently fails (e.g. picking a stacked window from the MRU).
        self.tiles
            .iter()
            .any(|tile| tile.stacked_windows().iter().any(|w| w.id() == window))
    }

    pub fn position(&self, window: &W::Id) -> Option<usize> {
        // See `contains`: search the full stack so stacked-but-hidden windows are
        // located correctly.
        self.tiles.iter().position(|tile| {
            tile.stacked_windows().iter().any(|w| w.id() == window)
        })
    }

    fn activate_idx(&mut self, idx: usize) -> bool {
        if self.active_tile_idx == idx {
            return false;
        }

        self.active_tile_idx = idx;

        self.tiles[idx].ensure_alpha_animates_to_1();

        true
    }

    fn activate_window(&mut self, window: &W::Id) {
        let idx = self.position(window).unwrap();
        // If the target lives inside a multi-window stack but isn't its currently-active
        // window, switch the stack to it first — otherwise activating the tile index
        // would leave the wrong window visible/focused (e.g. MRU pick of a hidden
        // stacked window).
        self.tiles[idx].activate_stacked_window(window);
        self.activate_idx(idx);
    }

    fn add_tile_at(&mut self, idx: usize, mut tile: Tile<W>) {
        tile.update_config(self.view_size, self.scale, self.options.clone());

        // Inserting a tile pushes down all tiles below it, but also in always-centering mode it
        // will affect the X position of all tiles in the column.
        let mut prev_offsets = Vec::with_capacity(self.tiles.len() + 1);
        prev_offsets.extend(self.tile_offsets().take(self.tiles.len()));

        if self.display_mode != ColumnDisplay::Tabbed {
            self.is_pending_fullscreen = false;
            self.is_pending_maximized = false;
        }

        self.data
            .insert(idx, TileData::new(&tile, WindowHeight::auto_1()));
        self.tiles.insert(idx, tile);
        self.update_tile_sizes(true);

        // Animate tiles according to the offset changes.
        prev_offsets.insert(idx, Point::default());
        for (i, ((tile, offset), prev)) in zip(self.tiles_mut(), prev_offsets).enumerate() {
            if i == idx {
                continue;
            }

            tile.animate_move_from(prev - offset);
        }
    }

    fn update_window(&mut self, window: &W::Id) {
        let (tile_idx, tile) = self
            .tiles
            .iter_mut()
            .enumerate()
            .find(|(_, tile)| tile.window().id() == window)
            .unwrap();

        let prev_height = self.data[tile_idx].size.h;

        tile.update_window();
        self.data[tile_idx].update(tile);

        let offset = prev_height - self.data[tile_idx].size.h;

        let is_tabbed = self.display_mode == ColumnDisplay::Tabbed;

        // Move windows below in tandem with resizing.
        //
        // FIXME: in always-centering mode, window resizing will affect the offsets of all other
        // windows in the column, so they should all be animated. How should this interact with
        // animated vs. non-animated resizes? For example, an animated +20 resize followed by two
        // non-animated -10 resizes.
        if !is_tabbed && offset != 0. {
            if tile.resize_animation().is_some() {
                // If there's a resize animation (that may have just started in
                // tile.update_window()), then the apparent size change is smooth with no sudden
                // jumps. This corresponds to adding an Y animation to tiles below.
                for tile in &mut self.tiles[tile_idx + 1..] {
                    tile.animate_move_y_from_with_config(
                        offset,
                        self.options.animations.window_resize.anim,
                    );
                }
            } else {
                // There's no resize animation, but the offset is nonzero. This could happen for
                // example:
                // - if the window resized on its own, which we don't animate
                // - if the window resized by less than 10 px (the resize threshold)
                //
                // The latter case could also cancel an ongoing resize animation.
                //
                // Now, stationary tiles below shouldn't react to this offset change in any way,
                // i.e. their apparent Y position should jump together with the resize. However,
                // tiles below that are already animating an Y movement should offset their
                // animations to avoid the jump.
                //
                // Notably, this is necessary to fix the animation jump when resizing height back
                // and forth in quick succession (in a way that cancels the resize animation).
                for tile in &mut self.tiles[tile_idx + 1..] {
                    tile.offset_move_y_anim_current(offset);
                }
            }
        }
    }

    /// Resync the cached per-tile [`TileData`] at `tile_idx` with the tile's
    /// current state.
    ///
    /// `TileData` normally tracks the committed tile size and is refreshed
    /// through the configure-ack path ([`Column::update_window`]). But
    /// `Tile::push_window` / `Tile::pop_active_window` swap the tile's active
    /// window synchronously, which changes its reported `tile_size()`
    /// immediately — without this resync the stack-move helpers would leave
    /// `data` stale and trip `Column::verify_invariants`.
    fn refresh_tile_data(&mut self, tile_idx: usize) {
        self.data[tile_idx].update(&self.tiles[tile_idx]);
    }

    /// Pop the active window out of the tile at `tile_idx` (which keeps living
    /// in this column), keeping that tile's cached [`TileData`] in sync — see
    /// [`Column::refresh_tile_data`]. Use this rather than poking
    /// `tiles[idx].pop_active_window()` directly whenever the tile stays in the
    /// column afterwards.
    fn pop_window_from_tile(&mut self, tile_idx: usize) -> W {
        let window = self.tiles[tile_idx].pop_active_window();
        self.refresh_tile_data(tile_idx);
        window
    }

    /// Extra size taken up by elements in the column such as the tab indicator.
    fn extra_size(&self) -> Size<f64, Logical> {
        if self.display_mode == ColumnDisplay::Tabbed {
            self.tab_indicator.extra_size(self.tiles.len(), self.scale)
        } else {
            Size::from((0., 0.))
        }
    }

    fn resolve_preset_width(&self, preset: PresetSize) -> ResolvedSize {
        let extra = self.extra_size();
        resolve_preset_size(preset, &self.options, self.working_area.size.w, extra.w)
    }

    fn resolve_preset_height(&self, preset: PresetSize) -> ResolvedSize {
        let extra = self.extra_size();
        resolve_preset_size(preset, &self.options, self.working_area.size.h, extra.h)
    }

    fn resolve_column_width(&self, width: ColumnWidth) -> f64 {
        let working_size = self.working_area.size;
        let gaps = self.options.layout.gaps;
        let extra = self.extra_size();

        match width {
            ColumnWidth::Proportion(proportion) => {
                (working_size.w - gaps) * proportion - gaps - extra.w
            }
            ColumnWidth::Fixed(width) => width,
        }
    }

    /// The natural (unscaled) resolved width this column wants, before any
    /// disable-carousel shrink-to-fit factor. Mirrors the width selection in
    /// [`Self::update_tile_sizes`] but without `fit_scale` or min/max clamping.
    fn natural_width(&self) -> f64 {
        let width = if self.is_full_width {
            ColumnWidth::Proportion(1.)
        } else {
            self.width
        };
        self.resolve_column_width(width)
    }

    /// Snapshot the column's width-defining state so it can be restored later (used by
    /// move-size tracking when a column is temporarily grown to host a moving window).
    fn width_snapshot(&self) -> WidthSnapshot {
        WidthSnapshot {
            width: self.width,
            is_full_width: self.is_full_width,
            preset_width_idx: self.preset_width_idx,
        }
    }

    /// Restore a width previously captured by [`Self::width_snapshot`] and resize tiles.
    fn restore_width_snapshot(&mut self, snap: &WidthSnapshot) {
        self.width = snap.width;
        self.is_full_width = snap.is_full_width;
        self.preset_width_idx = snap.preset_width_idx;
        self.update_tile_sizes(true);
    }

    /// Set the disable-carousel shrink factor and resize tiles to match. No-op
    /// when the factor is unchanged, so it's cheap to call after every layout
    /// change.
    fn set_fit_scale(&mut self, scale: f64, animate: bool) {
        if self.fit_scale == scale {
            return;
        }
        self.fit_scale = scale;
        self.update_tile_sizes(animate);
    }

    fn update_tile_sizes(&mut self, animate: bool) {
        self.update_tile_sizes_with_transaction(animate, Transaction::new());
    }

    fn update_tile_sizes_with_transaction(&mut self, animate: bool, transaction: Transaction) {
        let sizing_mode = self.pending_sizing_mode();
        if matches!(sizing_mode, SizingMode::Fullscreen | SizingMode::Maximized) {
            for (tile_idx, tile) in self.tiles.iter_mut().enumerate() {
                // In tabbed mode, only the visible window participates in the transaction.
                let is_active = tile_idx == self.active_tile_idx;
                let transaction = if self.display_mode == ColumnDisplay::Tabbed && !is_active {
                    None
                } else {
                    Some(transaction.clone())
                };

                if matches!(sizing_mode, SizingMode::Fullscreen) {
                    tile.request_fullscreen(animate, transaction);
                } else {
                    tile.request_maximized(self.parent_area.size, animate, transaction);
                }
            }
            return;
        }

        let is_tabbed = self.display_mode == ColumnDisplay::Tabbed;

        let min_size: Vec<_> = self
            .tiles
            .iter()
            .map(Tile::min_size_nonfullscreen)
            .map(|mut size| {
                size.w = size.w.max(1.);
                size.h = size.h.max(1.);
                size
            })
            .collect();
        let max_size: Vec<_> = self
            .tiles
            .iter()
            .map(Tile::max_size_nonfullscreen)
            .collect();

        // Compute the column width.
        let min_width = min_size
            .iter()
            .map(|size| NotNan::new(size.w).unwrap())
            .max()
            .map(NotNan::into_inner)
            .unwrap();
        let max_width = max_size
            .iter()
            .filter_map(|size| {
                let w = size.w;
                if w == 0. {
                    None
                } else {
                    Some(NotNan::new(w).unwrap())
                }
            })
            .min()
            .map(NotNan::into_inner)
            .unwrap_or(f64::from(i32::MAX));
        let max_width = f64::max(max_width, min_width);

        let width = if self.is_full_width {
            ColumnWidth::Proportion(1.)
        } else {
            self.width
        };

        let working_size = self.working_area.size;
        let extra_size = self.extra_size();

        // `fit_scale` is the disable-carousel shrink-to-fit factor (1.0 in every
        // other mode). Applying it here — the single point that turns the
        // desired `ColumnWidth` into pixels — means the shrink flows through to
        // both the tile sizes computed below and the cached column width, while
        // `self.width` stays the natural width.
        let width = self.resolve_column_width(width) * self.fit_scale;
        let width = f64::max(f64::min(width, max_width), min_width);
        let max_tile_height = working_size.h - self.options.layout.gaps * 2. - extra_size.h;

        // If there are multiple windows in a column, clamp the non-auto window's height according
        // to other windows' min sizes.
        let mut max_non_auto_window_height = None;
        if self.tiles.len() > 1 && !is_tabbed {
            if let Some(non_auto_idx) = self
                .data
                .iter()
                .position(|data| !matches!(data.height, WindowHeight::Auto { .. }))
            {
                let min_height_taken = min_size
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| *idx != non_auto_idx)
                    .map(|(_, min_size)| min_size.h + self.options.layout.gaps)
                    .sum::<f64>();

                let tile = &self.tiles[non_auto_idx];
                let height_left = max_tile_height - min_height_taken;
                max_non_auto_window_height = Some(f64::max(
                    1.,
                    tile.window_height_for_tile_height(height_left).round(),
                ));
            }
        }

        // Compute the tile heights. Start by converting window heights to tile heights.
        let mut heights = zip(&self.tiles, &self.data)
            .map(|(tile, data)| match data.height {
                auto @ WindowHeight::Auto { .. } => auto,
                WindowHeight::Fixed(height) => {
                    let mut window_height = height.round().max(1.);
                    if let Some(max) = max_non_auto_window_height {
                        window_height = f64::min(window_height, max);
                    } else {
                        // In any case, clamp to the working area height.
                        let max = tile.window_height_for_tile_height(max_tile_height).round();
                        window_height = f64::min(window_height, max);
                    }

                    WindowHeight::Fixed(tile.tile_height_for_window_height(window_height))
                }
                WindowHeight::Preset(idx) => {
                    let preset = self.options.layout.preset_window_heights[idx];
                    let window_height = match self.resolve_preset_height(preset) {
                        ResolvedSize::Tile(h) => tile.window_height_for_tile_height(h),
                        ResolvedSize::Window(h) => h,
                    };

                    let mut window_height = window_height.round().clamp(1., 100000.);
                    if let Some(max) = max_non_auto_window_height {
                        window_height = f64::min(window_height, max);
                    }

                    let tile_height = tile.tile_height_for_window_height(window_height);
                    WindowHeight::Fixed(tile_height)
                }
            })
            .collect::<Vec<_>>();

        // In tabbed display mode, fill fixed heights right away.
        if is_tabbed {
            // All tiles have the same height, equal to the height of the only fixed tile (if any).
            let tabbed_height = heights
                .iter()
                .find_map(|h| {
                    if let WindowHeight::Fixed(h) = h {
                        Some(*h)
                    } else {
                        None
                    }
                })
                .unwrap_or(max_tile_height);

            // We also take min height of all tabs into account.
            let min_height = min_size
                .iter()
                .map(|size| NotNan::new(size.h).unwrap())
                .max()
                .map(NotNan::into_inner)
                .unwrap();
            // But, if there's a larger-than-workspace tab, we don't want to force all tabs to that
            // size.
            let min_height = f64::min(max_tile_height, min_height);
            let tabbed_height = f64::max(tabbed_height, min_height);

            heights.fill(WindowHeight::Fixed(tabbed_height));

            // The following logic will apply individual min/max height, etc.
        }

        let gaps_left = self.options.layout.gaps * (self.tiles.len() + 1) as f64;
        let mut height_left = working_size.h - gaps_left;
        let mut auto_tiles_left = self.tiles.len();

        // Subtract all fixed-height tiles.
        for (h, (min_size, max_size)) in zip(&mut heights, zip(&min_size, &max_size)) {
            // Check if the tile has an exact height constraint.
            if min_size.h == max_size.h {
                *h = WindowHeight::Fixed(min_size.h);
            }

            if let WindowHeight::Fixed(h) = h {
                if max_size.h > 0. {
                    *h = f64::min(*h, max_size.h);
                }
                *h = f64::max(*h, min_size.h);

                height_left -= *h;
                auto_tiles_left -= 1;
            }
        }

        let mut total_weight: f64 = heights
            .iter()
            .filter_map(|h| {
                if let WindowHeight::Auto { weight } = *h {
                    Some(weight)
                } else {
                    None
                }
            })
            .sum();

        // Iteratively try to distribute the remaining height, checking against tile min heights.
        // Pick an auto height according to the current sizes, then check if it satisfies all
        // remaining min heights. If not, allocate fixed height to those tiles and repeat the
        // loop. On each iteration the auto height will get smaller.
        //
        // NOTE: we do not respect max height here. Doing so would complicate things: if the current
        // auto height is above some tile's max height, then the auto height can become larger.
        // Combining this with the min height loop is where the complexity appears.
        //
        // However, most max height uses are for fixed-size dialogs, where min height == max_height.
        // This case is separately handled above.
        'outer: while auto_tiles_left > 0 {
            // Wayland requires us to round the requested size for a window to integer logical
            // pixels, therefore we compute the remaining auto height dynamically.
            let mut height_left_2 = height_left;
            let mut total_weight_2 = total_weight;
            for ((h, tile), min_size) in zip(zip(&mut heights, &self.tiles), &min_size) {
                let weight = match *h {
                    WindowHeight::Auto { weight } => weight,
                    WindowHeight::Fixed(_) => continue,
                    WindowHeight::Preset(_) => unreachable!(),
                };
                let factor = weight / total_weight_2;

                // Compute the current auto height.
                let mut auto = height_left_2 * factor;

                // Check if the auto height satisfies the min height.
                if min_size.h > auto {
                    auto = min_size.h;
                    *h = WindowHeight::Fixed(auto);
                    height_left -= auto;
                    total_weight -= weight;
                    auto_tiles_left -= 1;

                    // If a min height was unsatisfied, then we allocate the tile more than the
                    // auto height, which means that the remaining auto tiles now have less height
                    // to work with, and the loop must run again.
                    //
                    // If we keep going in this loop and break out later, we may allocate less
                    // height to the subsequent tiles than would be available next iteration and
                    // potentially trip their min height check earlier than necessary, leading to
                    // visible snapping.
                    continue 'outer;
                }

                auto = tile.tile_height_for_window_height(
                    tile.window_height_for_tile_height(auto).round().max(1.),
                );

                height_left_2 -= auto;
                total_weight_2 -= weight;
            }

            // All min heights were satisfied, fill them in.
            for (h, tile) in zip(&mut heights, &self.tiles) {
                let weight = match *h {
                    WindowHeight::Auto { weight } => weight,
                    WindowHeight::Fixed(_) => continue,
                    WindowHeight::Preset(_) => unreachable!(),
                };
                let factor = weight / total_weight;

                // Compute the current auto height.
                let auto = height_left * factor;
                let auto = tile.tile_height_for_window_height(
                    tile.window_height_for_tile_height(auto).round().max(1.),
                );

                *h = WindowHeight::Fixed(auto);
                height_left -= auto;
                total_weight -= weight;
                auto_tiles_left -= 1;
            }

            assert_eq!(auto_tiles_left, 0);
        }

        for (tile_idx, (tile, h)) in zip(&mut self.tiles, heights).enumerate() {
            let WindowHeight::Fixed(height) = h else {
                unreachable!()
            };

            let size = Size::from((width, height));

            // In tabbed mode, only the visible window participates in the transaction.
            let is_active = tile_idx == self.active_tile_idx;
            let transaction = if self.display_mode == ColumnDisplay::Tabbed && !is_active {
                None
            } else {
                Some(transaction.clone())
            };

            tile.request_tile_size(size, animate, transaction);
        }
    }

    fn width(&self) -> f64 {
        let mut tiles_width = self
            .data
            .iter()
            .map(|data| NotNan::new(data.size.w).unwrap())
            .max()
            .map(NotNan::into_inner)
            .unwrap();

        if self.display_mode == ColumnDisplay::Tabbed && self.sizing_mode().is_normal() {
            let extra_size = self.tab_indicator.extra_size(self.tiles.len(), self.scale);
            tiles_width += extra_size.w;
        }

        tiles_width
    }

    fn focus_index(&mut self, index: u8) {
        let idx = min(usize::from(index.saturating_sub(1)), self.tiles.len() - 1);
        self.activate_idx(idx);
    }

    fn focus_up(&mut self) -> bool {
        self.activate_idx(self.active_tile_idx.saturating_sub(1))
    }

    fn focus_down(&mut self) -> bool {
        self.activate_idx(min(self.active_tile_idx + 1, self.tiles.len() - 1))
    }

    fn focus_top(&mut self) {
        self.activate_idx(0);
    }

    fn focus_bottom(&mut self) {
        self.activate_idx(self.tiles.len() - 1);
    }

    fn move_up(&mut self) -> bool {
        let new_idx = self.active_tile_idx.saturating_sub(1);
        if self.active_tile_idx == new_idx {
            return false;
        }

        let mut ys = self.tile_offsets().skip(self.active_tile_idx);
        let active_y = ys.next().unwrap().y;
        let next_y = ys.next().unwrap().y;
        drop(ys);

        self.tiles.swap(self.active_tile_idx, new_idx);
        self.data.swap(self.active_tile_idx, new_idx);
        self.active_tile_idx = new_idx;

        // Animate the movement.
        let new_active_y = self.tile_offset(new_idx).y;
        self.tiles[new_idx].animate_move_y_from(active_y - new_active_y);
        self.tiles[new_idx + 1].animate_move_y_from(active_y - next_y);

        true
    }

    fn move_down(&mut self) -> bool {
        let new_idx = min(self.active_tile_idx + 1, self.tiles.len() - 1);
        if self.active_tile_idx == new_idx {
            return false;
        }

        let mut ys = self.tile_offsets().skip(self.active_tile_idx);
        let active_y = ys.next().unwrap().y;
        let next_y = ys.next().unwrap().y;
        drop(ys);

        self.tiles.swap(self.active_tile_idx, new_idx);
        self.data.swap(self.active_tile_idx, new_idx);
        self.active_tile_idx = new_idx;

        // Animate the movement.
        let new_active_y = self.tile_offset(new_idx).y;
        self.tiles[new_idx].animate_move_y_from(active_y - new_active_y);
        self.tiles[new_idx - 1].animate_move_y_from(next_y - active_y);

        true
    }

    fn toggle_width(&mut self, tile_idx: Option<usize>, forwards: bool) {
        let tile_idx = tile_idx.unwrap_or(self.active_tile_idx);

        let preset_idx = if self.is_full_width || self.is_pending_maximized {
            None
        } else {
            self.preset_width_idx
        };

        let len = self.options.layout.preset_column_widths.len();
        let preset_idx = if let Some(idx) = preset_idx {
            (idx + if forwards { 1 } else { len - 1 }) % len
        } else {
            let tile = &self.tiles[tile_idx];
            let current_window = tile.window_expected_or_current_size().w;
            let current_tile = tile.tile_expected_or_current_size().w;

            let mut it = self
                .options
                .layout
                .preset_column_widths
                .iter()
                .map(|preset| self.resolve_preset_width(*preset));

            if forwards {
                it.position(|resolved| {
                    match resolved {
                        // Some allowance for fractional scaling purposes.
                        ResolvedSize::Tile(resolved) => current_tile + 1. < resolved,
                        ResolvedSize::Window(resolved) => current_window + 1. < resolved,
                    }
                })
                .unwrap_or(0)
            } else {
                it.rposition(|resolved| {
                    match resolved {
                        // Some allowance for fractional scaling purposes.
                        ResolvedSize::Tile(resolved) => resolved + 1. < current_tile,
                        ResolvedSize::Window(resolved) => resolved + 1. < current_window,
                    }
                })
                .unwrap_or(len - 1)
            }
        };

        let preset = self.options.layout.preset_column_widths[preset_idx];
        self.set_column_width(SizeChange::from(preset), Some(tile_idx), true);

        self.preset_width_idx = Some(preset_idx);
    }

    fn toggle_full_width(&mut self) {
        if self.is_pending_maximized {
            // Treat it as unmaximize.
            self.is_pending_maximized = false;
            self.is_full_width = false;
        } else {
            self.is_full_width = !self.is_full_width;
        }

        self.update_tile_sizes(true);
    }

    fn set_column_width(&mut self, change: SizeChange, tile_idx: Option<usize>, animate: bool) {
        let current = if self.is_full_width || self.is_pending_maximized {
            ColumnWidth::Proportion(1.)
        } else {
            self.width
        };

        let current_px = self.resolve_column_width(current);

        // FIXME: fix overflows then remove limits.
        const MAX_PX: f64 = 100000.;
        const MAX_F: f64 = 10000.;

        let width = match (current, change) {
            (_, SizeChange::SetFixed(fixed)) => {
                // As a special case, setting a fixed column width will compute it in such a way
                // that the specified (usually active) window gets that width. This is the
                // intention behind the ability to set a fixed size.
                let tile_idx = tile_idx.unwrap_or(self.active_tile_idx);
                let tile = &self.tiles[tile_idx];
                ColumnWidth::Fixed(
                    tile.tile_width_for_window_width(f64::from(fixed))
                        .clamp(1., MAX_PX),
                )
            }
            (_, SizeChange::SetProportion(proportion)) => {
                ColumnWidth::Proportion((proportion / 100.).clamp(0., MAX_F))
            }
            (_, SizeChange::AdjustFixed(delta)) => {
                let width = (current_px + f64::from(delta)).clamp(1., MAX_PX);
                ColumnWidth::Fixed(width)
            }
            (ColumnWidth::Proportion(current), SizeChange::AdjustProportion(delta)) => {
                let proportion = (current + delta / 100.).clamp(0., MAX_F);
                ColumnWidth::Proportion(proportion)
            }
            (ColumnWidth::Fixed(_), SizeChange::AdjustProportion(delta)) => {
                let full = self.working_area.size.w - self.options.layout.gaps;
                let current = if full == 0. {
                    1.
                } else {
                    (current_px + self.options.layout.gaps + self.extra_size().w) / full
                };
                let proportion = (current + delta / 100.).clamp(0., MAX_F);
                ColumnWidth::Proportion(proportion)
            }
        };

        self.width = width;
        self.preset_width_idx = None;
        self.is_full_width = false;
        self.is_pending_maximized = false;
        self.update_tile_sizes(animate);
    }

    fn set_window_height(&mut self, change: SizeChange, tile_idx: Option<usize>, animate: bool) {
        let tile_idx = tile_idx.unwrap_or(self.active_tile_idx);

        // Start by converting all heights to automatic, since only one window in the column can be
        // non-auto-height. If the current tile is already non-auto, however, we can skip that
        // step. Which is not only for optimization, but also preserves automatic weights in case
        // one window is resized in such a way that other windows hit their min size, and then
        // back.
        if matches!(self.data[tile_idx].height, WindowHeight::Auto { .. }) {
            self.convert_heights_to_auto();
        }

        let current = self.data[tile_idx].height;
        let tile = &self.tiles[tile_idx];
        let current_window_px = match current {
            WindowHeight::Auto { .. } | WindowHeight::Preset(_) => tile.window_size().h,
            WindowHeight::Fixed(height) => height,
        };
        let current_tile_px = tile.tile_height_for_window_height(current_window_px);

        let working_size = self.working_area.size.h;
        let gaps = self.options.layout.gaps;
        let extra_size = self.extra_size().h;
        let full = working_size - gaps;
        let current_prop = if full == 0. {
            1.
        } else {
            (current_tile_px + gaps) / full
        };

        // FIXME: fix overflows then remove limits.
        const MAX_PX: f64 = 100000.;

        let mut window_height = match change {
            SizeChange::SetFixed(fixed) => f64::from(fixed),
            SizeChange::SetProportion(proportion) => {
                let tile_height = (working_size - gaps) * (proportion / 100.) - gaps - extra_size;
                tile.window_height_for_tile_height(tile_height)
            }
            SizeChange::AdjustFixed(delta) => current_window_px + f64::from(delta),
            SizeChange::AdjustProportion(delta) => {
                let proportion = current_prop + delta / 100.;
                let tile_height = (working_size - gaps) * proportion - gaps - extra_size;
                tile.window_height_for_tile_height(tile_height)
            }
        };

        // Clamp the height according to other windows' min sizes, or simply to working area height.
        let min_height_taken = if self.display_mode == ColumnDisplay::Tabbed {
            0.
        } else {
            self.tiles
                .iter()
                .enumerate()
                .filter(|(idx, _)| *idx != tile_idx)
                .map(|(_, tile)| f64::max(1., tile.min_size_nonfullscreen().h) + gaps)
                .sum::<f64>()
        };
        let height_left = working_size - extra_size - gaps - min_height_taken - gaps;
        let height_left = f64::max(1., tile.window_height_for_tile_height(height_left));
        window_height = f64::min(height_left, window_height);

        // Clamp it against the window height constraints.
        let win = &self.tiles[tile_idx].window();
        let min_h = win.min_size().h;
        let max_h = win.max_size().h;

        if max_h > 0 {
            window_height = f64::min(window_height, f64::from(max_h));
        }
        if min_h > 0 {
            window_height = f64::max(window_height, f64::from(min_h));
        }

        self.data[tile_idx].height = WindowHeight::Fixed(window_height.clamp(1., MAX_PX));
        self.is_pending_maximized = false;
        self.update_tile_sizes(animate);
    }

    fn reset_window_height(&mut self, tile_idx: Option<usize>) {
        if self.display_mode == ColumnDisplay::Tabbed {
            // When tabbed, reset window height should work on any window, not just the fixed-size
            // one.
            for data in &mut self.data {
                data.height = WindowHeight::auto_1();
            }
        } else {
            let tile_idx = tile_idx.unwrap_or(self.active_tile_idx);
            self.data[tile_idx].height = WindowHeight::auto_1();
        }

        self.update_tile_sizes(true);
    }

    fn toggle_window_height(&mut self, tile_idx: Option<usize>, forwards: bool) {
        let tile_idx = tile_idx.unwrap_or(self.active_tile_idx);

        // Start by converting all heights to automatic, since only one window in the column can be
        // non-auto-height. If the current tile is already non-auto, however, we can skip that
        // step. Which is not only for optimization, but also preserves automatic weights in case
        // one window is resized in such a way that other windows hit their min size, and then
        // back.
        if matches!(self.data[tile_idx].height, WindowHeight::Auto { .. }) {
            self.convert_heights_to_auto();
        }

        let len = self.options.layout.preset_window_heights.len();
        let preset_idx = match self.data[tile_idx].height {
            WindowHeight::Preset(idx) if !self.is_pending_maximized => {
                (idx + if forwards { 1 } else { len - 1 }) % len
            }
            _ => {
                let current = self.data[tile_idx].size.h;
                let tile = &self.tiles[tile_idx];

                let mut it = self
                    .options
                    .layout
                    .preset_window_heights
                    .iter()
                    .copied()
                    .map(|preset| {
                        let window_height = match self.resolve_preset_height(preset) {
                            ResolvedSize::Tile(h) => tile.window_height_for_tile_height(h),
                            ResolvedSize::Window(h) => h,
                        };
                        tile.tile_height_for_window_height(window_height.round().clamp(1., 100000.))
                    });

                if forwards {
                    it.position(|resolved| {
                        // Some allowance for fractional scaling purposes.
                        current + 1. < resolved
                    })
                    .unwrap_or(0)
                } else {
                    it.rposition(|resolved| {
                        // Some allowance for fractional scaling purposes.
                        resolved + 1. < current
                    })
                    .unwrap_or(len - 1)
                }
            }
        };
        self.data[tile_idx].height = WindowHeight::Preset(preset_idx);
        self.is_pending_maximized = false;
        self.update_tile_sizes(true);
    }

    /// Converts all heights in the column to automatic, preserving the apparent heights.
    ///
    /// All weights are recomputed to preserve the current tile heights while "centering" the
    /// weights at the median window height (it gets weight = 1).
    ///
    /// One case where apparent heights will not be preserved is when the column is taller than the
    /// working area.
    fn convert_heights_to_auto(&mut self) {
        let heights: Vec<_> = self.tiles.iter().map(|tile| tile.tile_size().h).collect();

        // Weights are invariant to multiplication: a column with weights 2, 2, 1 is equivalent to
        // a column with weights 4, 4, 2. So we find the median window height and use that as 1.
        let mut sorted = heights.clone();
        sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2];

        for (data, height) in zip(&mut self.data, heights) {
            let weight = height / median;
            data.height = WindowHeight::Auto { weight };
        }
    }

    fn set_fullscreen(&mut self, is_fullscreen: bool) {
        if self.is_pending_fullscreen == is_fullscreen {
            return;
        }

        if is_fullscreen {
            assert!(self.tiles.len() == 1 || self.display_mode == ColumnDisplay::Tabbed);
        }

        self.is_pending_fullscreen = is_fullscreen;
        self.update_tile_sizes(true);
    }

    fn set_maximized(&mut self, maximize: bool) {
        if self.is_pending_maximized == maximize {
            return;
        }

        if maximize {
            assert!(self.tiles.len() == 1 || self.display_mode == ColumnDisplay::Tabbed);
        }

        self.is_pending_maximized = maximize;
        self.update_tile_sizes(true);
    }

    fn set_column_display(&mut self, display: ColumnDisplay) {
        if self.display_mode == display {
            return;
        }

        // Animate the movement.
        //
        // We're doing some shortcuts here because we know that currently normal vs. tabbed can
        // only cause a vertical shift + a shift to the origin.
        //
        // Doing it this way to avoid storing all tile positions in a vector. If more display modes
        // are added it might be simpler to just collect everything into a smallvec.
        let prev_origin = self.tiles_origin();
        self.display_mode = display;
        let new_origin = self.tiles_origin();
        let origin_delta = prev_origin - new_origin;

        // When need to walk the tiles in the normal display mode to get the right offsets.
        self.display_mode = ColumnDisplay::Normal;
        for (tile, pos) in self.tiles_mut() {
            let mut y_delta = pos.y - prev_origin.y;

            // Invert the Y motion when transitioning *to* normal display mode.
            if display == ColumnDisplay::Normal {
                y_delta *= -1.;
            }

            let mut delta = origin_delta;
            delta.y += y_delta;
            tile.animate_move_from(delta);
        }

        // Animate the opacity.
        for (idx, tile) in self.tiles.iter_mut().enumerate() {
            let is_active = idx == self.active_tile_idx;
            if !is_active {
                let (from, to) = if display == ColumnDisplay::Tabbed {
                    (1., 0.)
                } else {
                    (0., 1.)
                };
                tile.animate_alpha(from, to, self.options.animations.window_movement.0);
            }
        }

        // Animate the appearance of the tab indicator.
        if display == ColumnDisplay::Tabbed {
            self.tab_indicator.start_open_animation(
                self.clock.clone(),
                self.options.animations.window_movement.0,
            );
        }

        // Now switch the display mode for real.
        self.display_mode = display;
        self.update_tile_sizes(true);
    }

    fn tiles_origin(&self) -> Point<f64, Logical> {
        let mut origin = Point::from((0., 0.));

        match self.sizing_mode() {
            SizingMode::Normal => (),
            SizingMode::Maximized => {
                origin.y += self.parent_area.loc.y;
                return origin;
            }
            SizingMode::Fullscreen => return origin,
        }

        origin.y += self.working_area.loc.y + self.options.layout.gaps;

        if self.display_mode == ColumnDisplay::Tabbed {
            origin += self
                .tab_indicator
                .content_offset(self.tiles.len(), self.scale);
        }

        origin
    }

    // HACK: pass a self.data iterator in manually as a workaround for the lack of method partial
    // borrowing. Note that this method's return value does not borrow the entire &Self!
    fn tile_offsets_iter(
        &self,
        data: impl Iterator<Item = TileData>,
    ) -> impl Iterator<Item = Point<f64, Logical>> {
        // FIXME: this should take into account always-center-single-column, which means that
        // Column should somehow know when it is being centered due to being the single column on
        // the workspace or some other reason.
        let center = self.options.layout.center_focused_column == CenterFocusedColumn::Always;
        let gaps = self.options.layout.gaps;
        let tabbed = self.display_mode == ColumnDisplay::Tabbed;

        // Does not include extra size from the tab indicator.
        let tiles_width = self
            .data
            .iter()
            .map(|data| NotNan::new(data.size.w).unwrap())
            .max()
            .map(NotNan::into_inner)
            .unwrap_or(0.);

        let mut origin = self.tiles_origin();

        // Chain with a dummy value to be able to get one past all tiles' Y.
        let dummy = TileData {
            height: WindowHeight::auto_1(),
            size: Size::default(),
            interactively_resizing_by_left_edge: false,
        };
        let data = data.chain(iter::once(dummy));

        data.map(move |data| {
            let mut pos = origin;

            if center {
                pos.x += (tiles_width - data.size.w) / 2.;
            } else if data.interactively_resizing_by_left_edge {
                pos.x += tiles_width - data.size.w;
            }

            if !tabbed {
                origin.y += data.size.h + gaps;
            }

            pos
        })
    }

    fn tile_offsets(&self) -> impl Iterator<Item = Point<f64, Logical>> + '_ {
        self.tile_offsets_iter(self.data.iter().copied())
    }

    fn tile_offset(&self, tile_idx: usize) -> Point<f64, Logical> {
        self.tile_offsets().nth(tile_idx).unwrap()
    }

    fn tile_offsets_in_render_order(
        &self,
        data: impl Iterator<Item = TileData>,
    ) -> impl Iterator<Item = Point<f64, Logical>> {
        let active_idx = self.active_tile_idx;
        let active_pos = self.tile_offset(active_idx);
        let offsets = self
            .tile_offsets_iter(data)
            .enumerate()
            .filter_map(move |(idx, pos)| (idx != active_idx).then_some(pos));
        iter::once(active_pos).chain(offsets)
    }

    pub fn tiles(&self) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> + '_ {
        let offsets = self.tile_offsets_iter(self.data.iter().copied());
        zip(&self.tiles, offsets)
    }

    fn tiles_mut(&mut self) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        let offsets = self.tile_offsets_iter(self.data.iter().copied());
        zip(&mut self.tiles, offsets)
    }

    fn tiles_in_render_order(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> + '_ {
        let offsets = self.tile_offsets_in_render_order(self.data.iter().copied());

        let (first, rest) = self.tiles.split_at(self.active_tile_idx);
        let (active, rest) = rest.split_at(1);

        let active = active.iter().map(|tile| (tile, true));

        let rest_visible = self.display_mode != ColumnDisplay::Tabbed;
        let rest = first.iter().chain(rest);
        let rest = rest.map(move |tile| (tile, rest_visible));

        let tiles = active.chain(rest);
        zip(tiles, offsets).map(|((tile, visible), pos)| (tile, pos, visible))
    }

    fn tiles_in_render_order_mut(
        &mut self,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        let offsets = self.tile_offsets_in_render_order(self.data.iter().copied());

        let (first, rest) = self.tiles.split_at_mut(self.active_tile_idx);
        let (active, rest) = rest.split_at_mut(1);

        let tiles = active.iter_mut().chain(first).chain(rest);
        zip(tiles, offsets)
    }

    fn tab_indicator_area(&self) -> Rectangle<f64, Logical> {
        // We'd like to use the active tile's animated size for the tab indicator, however we need
        // to be mindful of the case where the active tile is smaller than some other tile in the
        // column. The column assumes the size of the largest tile.
        //
        // We expect users to mainly resize tabbed columns by width, so matching the animated size
        // is more important here. Besides, we always try to resize all windows in a column to the
        // same width when possible, and also the animation for going into tabbed mode doesn't move
        // tiles horizontally as much.
        //
        // For height though, it's a different story. First, users probably aren't resizing a
        // tabbed column by height. Second, we don't match windows by height, so it's easy to have
        // a smaller active tile than the rest of the column, e.g. by adding a fixed-size dialog.
        // Then, switching to that dialog and back should ideally keep the tab indicator position
        // fixed. Third, the animation for making a column tabbed moves tiles vertically, and using
        // the active tile's animated size in this case only works for the topmost tile, and looks
        // broken otherwise.
        let mut max_height = 0.;
        for tile in &self.tiles {
            max_height = f64::max(max_height, tile.tile_size().h);
        }

        let tile = &self.tiles[self.active_tile_idx];
        let area_size = Size::from((tile.animated_tile_size().w, max_height));

        Rectangle::new(self.tiles_origin(), area_size)
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        for tile in &mut self.tiles {
            if tile.window().id() == id {
                tile.start_open_animation();

                // Animate the appearance of the tab indicator.
                if self.display_mode == ColumnDisplay::Tabbed
                    && self.sizing_mode().is_normal()
                    && self.tiles.len() == 1
                    && !self.tab_indicator.config().hide_when_single_tab
                {
                    self.tab_indicator.start_open_animation(
                        self.clock.clone(),
                        self.options.animations.window_open.anim,
                    );
                }

                return true;
            }
        }

        false
    }

    #[cfg(test)]
    fn verify_invariants(&self) {
        assert!(!self.tiles.is_empty(), "columns can't be empty");
        assert!(self.active_tile_idx < self.tiles.len());
        assert_eq!(self.tiles.len(), self.data.len());

        if !self.pending_sizing_mode().is_normal() {
            assert!(self.tiles.len() == 1 || self.display_mode == ColumnDisplay::Tabbed);
        }

        if let Some(idx) = self.preset_width_idx {
            assert!(idx < self.options.layout.preset_column_widths.len());
        }

        let is_tabbed = self.display_mode == ColumnDisplay::Tabbed;

        let tile_count = self.tiles.len();
        if tile_count == 1 {
            if let WindowHeight::Auto { weight } = self.data[0].height {
                assert_eq!(
                    weight, 1.,
                    "auto height weight must reset to 1 for a single window"
                );
            }
        }

        let working_size = self.working_area.size;
        let extra_size = self.extra_size();
        let gaps = self.options.layout.gaps;

        let mut found_fixed = false;
        let mut total_height = 0.;
        let mut total_min_height = 0.;
        for (tile, data) in zip(&self.tiles, &self.data) {
            assert!(Rc::ptr_eq(&self.options, &tile.options));
            assert_eq!(self.clock, tile.clock);
            assert_eq!(self.scale, tile.scale());
            assert_eq!(
                self.pending_sizing_mode(),
                tile.window().pending_sizing_mode()
            );
            assert_eq!(self.view_size, tile.view_size());
            tile.verify_invariants();

            let mut data2 = *data;
            data2.update(tile);
            assert_eq!(data, &data2, "tile data must be up to date");

            if matches!(data.height, WindowHeight::Fixed(_)) {
                assert!(
                    !found_fixed,
                    "there can only be one fixed-height window in a column"
                );
                found_fixed = true;
            }

            if let WindowHeight::Preset(idx) = data.height {
                assert!(self.options.layout.preset_window_heights.len() > idx);
            }

            let requested_size = tile.window().requested_size().unwrap();
            let requested_tile_height =
                tile.tile_height_for_window_height(f64::from(requested_size.h));
            let min_tile_height = f64::max(1., tile.min_size_nonfullscreen().h);

            if !is_tabbed
                && self.pending_sizing_mode().is_normal()
                && self.scale.round() == self.scale
                && working_size.h.round() == working_size.h
                && gaps.round() == gaps
            {
                let total_height = requested_tile_height + gaps * 2. + extra_size.h;
                let total_min_height = min_tile_height + gaps * 2. + extra_size.h;
                let max_height = f64::max(total_min_height, working_size.h);
                assert!(
                    total_height <= max_height,
                    "each tile in a column mustn't go beyond working area height \
                     (tile height {total_height} > max height {max_height})"
                );
            }

            total_height += requested_tile_height;
            total_min_height += min_tile_height;
        }

        if !is_tabbed
            && tile_count > 1
            && self.scale.round() == self.scale
            && working_size.h.round() == working_size.h
            && gaps.round() == gaps
        {
            total_height += gaps * (tile_count + 1) as f64 + extra_size.h;
            total_min_height += gaps * (tile_count + 1) as f64 + extra_size.h;
            let max_height = f64::max(total_min_height, working_size.h);
            assert!(
                total_height <= max_height,
                "multiple tiles in a column mustn't go beyond working area height \
                 (total height {total_height} > max height {max_height})"
            );
        }
    }
}

fn compute_new_view_offset(
    cur_x: f64,
    view_width: f64,
    new_col_x: f64,
    new_col_width: f64,
    gaps: f64,
) -> f64 {
    // If the column is wider than the view, always left-align it.
    if view_width <= new_col_width {
        return 0.;
    }

    // Compute the padding in case it needs to be smaller due to large tile width.
    let padding = ((view_width - new_col_width) / 2.).clamp(0., gaps);

    // Compute the desired new X with padding.
    let new_x = new_col_x - padding;
    let new_right_x = new_col_x + new_col_width + padding;

    // If the column is already fully visible, leave the view as is.
    if cur_x <= new_x && new_right_x <= cur_x + view_width {
        return -(new_col_x - cur_x);
    }

    // Otherwise, prefer the alignment that results in less motion from the current position.
    let dist_to_left = (cur_x - new_x).abs();
    let dist_to_right = ((cur_x + view_width) - new_right_x).abs();
    if dist_to_left <= dist_to_right {
        -padding
    } else {
        -(view_width - padding - new_col_width)
    }
}

fn compute_working_area(
    parent_area: Rectangle<f64, Logical>,
    scale: f64,
    struts: Struts,
) -> Rectangle<f64, Logical> {
    let mut working_area = parent_area;

    // Add struts.
    working_area.size.w = f64::max(0., working_area.size.w - struts.left.0 - struts.right.0);
    working_area.loc.x += struts.left.0;

    working_area.size.h = f64::max(0., working_area.size.h - struts.top.0 - struts.bottom.0);
    working_area.loc.y += struts.top.0;

    // Round location to start at a physical pixel.
    let loc = working_area
        .loc
        .to_physical_precise_ceil(scale)
        .to_logical(scale);

    let mut size_diff = (loc - working_area.loc).to_size();
    size_diff.w = f64::min(working_area.size.w, size_diff.w);
    size_diff.h = f64::min(working_area.size.h, size_diff.h);

    working_area.size -= size_diff;
    working_area.loc = loc;

    working_area
}

fn compute_toplevel_bounds(
    border_config: naru_config::Border,
    working_area_size: Size<f64, Logical>,
    extra_size: Size<f64, Logical>,
    gaps: f64,
) -> Size<i32, Logical> {
    let mut border = 0.;
    if !border_config.off {
        border = border_config.width * 2.;
    }

    Size::from((
        f64::max(working_area_size.w - gaps * 2. - extra_size.w - border, 1.),
        f64::max(working_area_size.h - gaps * 2. - extra_size.h - border, 1.),
    ))
    .to_i32_floor()
}

fn cancel_resize_for_column<W: LayoutElement>(
    interactive_resize: &mut Option<InteractiveResize<W>>,
    column: &mut Column<W>,
) {
    if let Some(resize) = interactive_resize {
        if column.contains(&resize.window) {
            *interactive_resize = None;
        }
    }

    for tile in &mut column.tiles {
        tile.window_mut().cancel_interactive_resize();
    }
}

fn resolve_preset_size(
    preset: PresetSize,
    options: &Options,
    view_size: f64,
    extra_size: f64,
) -> ResolvedSize {
    match preset {
        PresetSize::Proportion(proportion) => ResolvedSize::Tile(
            (view_size - options.layout.gaps) * proportion - options.layout.gaps - extra_size,
        ),
        PresetSize::Fixed(width) => ResolvedSize::Window(f64::from(width)),
    }
}

#[cfg(test)]
mod tests {
    use naru_config::FloatOrInt;

    use super::*;
    use crate::utils::round_logical_in_physical;

    #[test]
    fn working_area_starts_at_physical_pixel() {
        let struts = Struts {
            left: FloatOrInt(0.5),
            right: FloatOrInt(1.),
            top: FloatOrInt(0.75),
            bottom: FloatOrInt(1.),
        };

        let parent_area = Rectangle::from_size(Size::from((1280., 720.)));
        let area = compute_working_area(parent_area, 1., struts);

        assert_eq!(round_logical_in_physical(1., area.loc.x), area.loc.x);
        assert_eq!(round_logical_in_physical(1., area.loc.y), area.loc.y);
    }

    #[test]
    fn large_fractional_strut() {
        let struts = Struts {
            left: FloatOrInt(0.),
            right: FloatOrInt(0.),
            top: FloatOrInt(50000.5),
            bottom: FloatOrInt(0.),
        };

        let parent_area = Rectangle::from_size(Size::from((1280., 720.)));
        compute_working_area(parent_area, 1., struts);
    }
}
