use std::cmp::max;
use std::rc::Rc;
use std::time::Duration;

use naru_config::utils::MergeWith as _;
use naru_config::{
    CenterFocusedColumn, CornerRadius, OutputName, PresetSize, Workspace as WorkspaceConfig,
};
use naru_ipc::{ColumnDisplay, PositionChange, SizeChange, WindowLayout};
use smithay::backend::renderer::element::utils::CropRenderElement;
use smithay::backend::renderer::element::{Element, Kind};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::{layer_map_for_output, Window};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Serial, Size, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::SurfaceCachedState;

use super::fixed_strip::FixedSide;
use super::floating::{FloatingSpace, FloatingSpaceRenderElement};
use super::scrolling::{
    Column, ColumnWidth, ScrollDirection, ScrollingSpace, ScrollingSpaceRenderElement,
};
use super::shadow::Shadow;
use super::tile::{Tile, TileRenderSnapshot};
use super::{
    ActivateWindow, HitType, InsertPosition, InteractiveResizeData, LayoutElement, Options,
    RemovedTile, SizeFrac,
};
use crate::animation::Clock;
use crate::naru_render_elements;
use crate::render_helpers::edge_fade::{EdgeFadeOffscreenRenderElement, EdgeFadeShader};
use crate::render_helpers::offscreen::OffscreenBuffer;
use crate::render_helpers::renderer::NaruRenderer;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::xray::{Xray, XrayPos};
use crate::render_helpers::RenderCtx;
use crate::utils::id::IdCounter;
use crate::utils::transaction::{Transaction, TransactionBlocker};
use crate::utils::{
    ensure_min_max_size, ensure_min_max_size_maybe_zero, output_size, send_scale_transform,
    ResizeEdge,
};
use crate::window::ResolvedWindowRules;

/// Width in logical pixels of the per-pixel opacity fade applied to the
/// carousel's edge as it approaches a populated fixed-side panel. The carousel
/// content fades to fully transparent right at the panel's inner edge, so it
/// dissolves into the wallpaper just before sliding behind the panel rather
/// than being darkened by a shadow.
const CAROUSEL_EDGE_FADE_WIDTH: f64 = 12.0;

/// True when the view's aspect ratio is ≥ 21:9 (covers 21:9 ≈ 2.333 and 32:10 = 3.2).
fn is_ultrawide_view(view_size: Size<f64, Logical>) -> bool {
    let h = view_size.h.max(1.0);
    view_size.w / h >= 21.0 / 9.0
}

/// Flip a relative size change into its opposite direction (used by
/// positionally-aware resize to turn a grow step into a shrink step). Absolute
/// targets have no meaningful opposite, so they're returned unchanged.
fn negate_size_change(change: SizeChange) -> SizeChange {
    match change {
        SizeChange::AdjustFixed(n) => SizeChange::AdjustFixed(-n),
        SizeChange::AdjustProportion(n) => SizeChange::AdjustProportion(-n),
        other @ (SizeChange::SetFixed(_) | SizeChange::SetProportion(_)) => other,
    }
}

/// When the user hasn't set `default-column-width` in their config, return a sensible default
/// for ultrawide screens (≥ 21:9, which also covers 32:10): the configurable
/// `ultrawide_terminal_column_width` (default 1/5) when the window is a terminal (per
/// `layout.terminal_app_ids`), otherwise the configurable `ultrawide_default_column_width`
/// (default 2/5). Returns `None` on non-ultrawide so the caller falls back to "windows decide
/// their own width".
fn ultrawide_default_column_width(
    view_size: Size<f64, Logical>,
    app_id: Option<&str>,
    terminal_app_ids: &[String],
    default_for_others: PresetSize,
    default_for_terminals: PresetSize,
) -> Option<PresetSize> {
    if !is_ultrawide_view(view_size) {
        return None;
    }
    let is_terminal = app_id
        .map(|id| terminal_app_ids.iter().any(|t| t == id))
        .unwrap_or(false);
    Some(if is_terminal {
        default_for_terminals
    } else {
        default_for_others
    })
}

/// Default column width for a media-player window (app_id in `media_player_app_ids`): the
/// `ultrawide` width on ultrawide views (≥ 21:9), otherwise the `normal` width. Unlike the
/// terminal ultrawide default, this applies on every aspect ratio and takes precedence over the
/// global `default-column-width`. Returns `None` when the window isn't a media player, so the
/// caller falls through to the usual width defaults.
fn media_player_default_column_width(
    view_size: Size<f64, Logical>,
    app_id: Option<&str>,
    media_player_app_ids: &[String],
    normal: PresetSize,
    ultrawide: PresetSize,
) -> Option<PresetSize> {
    let is_media = app_id
        .map(|id| media_player_app_ids.iter().any(|t| t == id))
        .unwrap_or(false);
    if !is_media {
        return None;
    }
    Some(if is_ultrawide_view(view_size) {
        ultrawide
    } else {
        normal
    })
}

#[derive(Debug)]
pub struct Workspace<W: LayoutElement> {
    /// The scrollable-tiling layout.
    scrolling: ScrollingSpace<W>,

    /// The floating layout.
    floating: FloatingSpace<W>,

    /// Whether the floating layout is active instead of the scrolling layout.
    floating_is_active: FloatingActive,

    /// Width (logical px) of the monitor's fixed-side panels (left, right),
    /// pushed down from the owning [`Monitor`](super::monitor::Monitor) via
    /// [`Self::set_fixed_insets`]. The carousel's resting area is inset by these
    /// so it never overlaps a panel. `(0., 0.)` while both panels are empty.
    fixed_insets: (f64, f64),

    /// The original output of this workspace.
    ///
    /// Most of the time this will be the workspace's current output, however, after an output
    /// disconnection, it may remain pointing to the disconnected output.
    pub(super) original_output: OutputId,

    /// Current output of this workspace.
    output: Option<Output>,

    /// Latest known output scale for this workspace.
    ///
    /// This should be set from the current workspace output, or, if all outputs have been
    /// disconnected, preserved until a new output is connected.
    scale: smithay::output::Scale,

    /// Latest known output transform for this workspace.
    ///
    /// This should be set from the current workspace output, or, if all outputs have been
    /// disconnected, preserved until a new output is connected.
    transform: Transform,

    /// Latest known view size for this workspace.
    ///
    /// This should be computed from the current workspace output size, or, if all outputs have
    /// been disconnected, preserved until a new output is connected.
    view_size: Size<f64, Logical>,

    /// Latest known working area for this workspace.
    ///
    /// Not rounded to physical pixels.
    ///
    /// This is similar to view size, but takes into account things like layer shell exclusive
    /// zones.
    working_area: Rectangle<f64, Logical>,

    /// This workspace's shadow in the overview.
    shadow: Shadow,

    /// This workspace's background.
    background_buffer: SolidColorBuffer,

    /// Offscreen buffers used to render the thin carousel edge-fade band next to
    /// each populated fixed-side panel (left / right). Only touched on frames
    /// where carousel content actually reaches the corresponding panel edge.
    left_fade_offscreen: OffscreenBuffer,
    right_fade_offscreen: OffscreenBuffer,

    /// Clock for driving animations.
    pub(super) clock: Clock,

    /// Configurable properties of the layout as received from the parent monitor.
    pub(super) base_options: Rc<Options>,

    /// Configurable properties of the layout with logical sizes adjusted for the current `scale`.
    pub(super) options: Rc<Options>,

    /// Optional name of this workspace.
    pub(super) name: Option<String>,

    /// Layout config overrides for this workspace.
    layout_config: Option<naru_config::LayoutPart>,

    /// Unique ID of this workspace.
    id: WorkspaceId,
}

#[derive(Debug, Clone)]
pub struct OutputId(String);

impl OutputId {
    pub fn matches(&self, output: &Output) -> bool {
        let output_name = output.user_data().get::<OutputName>().unwrap();
        output_name.matches(&self.0)
    }
}

static WORKSPACE_ID_COUNTER: IdCounter = IdCounter::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceId(u64);

impl WorkspaceId {
    fn next() -> WorkspaceId {
        WorkspaceId(WORKSPACE_ID_COUNTER.next())
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn specific(id: u64) -> Self {
        Self(id)
    }
}

naru_render_elements! {
    WorkspaceRenderElement<R> => {
        Scrolling = ScrollingSpaceRenderElement<R>,
        CroppedScrolling = CropRenderElement<ScrollingSpaceRenderElement<R>>,
        EdgeFade = EdgeFadeOffscreenRenderElement,
        Floating = FloatingSpaceRenderElement<R>,
        SolidColor = SolidColorRenderElement,
    }
}

#[derive(Debug)]
pub(super) struct InteractiveResize<W: LayoutElement> {
    pub window: W::Id,
    pub original_window_size: Size<f64, Logical>,
    pub data: InteractiveResizeData,
}

/// Resolved width or height in logical pixels.
#[derive(Debug, Clone, Copy)]
pub enum ResolvedSize {
    /// Size of the tile including borders.
    Tile(f64),
    /// Size of the window excluding borders.
    Window(f64),
}

/// Whether the floating space is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatingActive {
    /// The scrolling space is active.
    No,
    /// The scrolling space is active, but the floating space should render on top, even if the
    /// active scrolling window is fullscreen.
    ///
    /// This is necessary for focus-follows-mouse that activates but doesn't raise the window to
    /// avoid being annoying.
    NoButRaised,
    /// The floating space is active.
    Yes,
}

/// Which sub-layout of a workspace owns a particular window.
///
/// Per-window operations (`set_window_width`, `set_fullscreen`, interactive
/// resize, …) must be dispatched to the layer that actually holds the window
/// — the carousel's by-id methods `unwrap()` on a window they don't own.
///
/// Also surfaced through [`Workspace::window_slot`] so the test harness (and
/// any future introspection) can assert which layer a window landed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowLayer {
    Floating,
    Scrolling,
    // Panels are owned by the monitor now; these variants are produced by the
    // monitor/test introspection (`Monitor`/`FixedPanels::side_with_window`),
    // not by `Workspace`, so they look dead in non-test builds.
    #[cfg_attr(not(test), allow(dead_code))]
    FixedLeft,
    #[cfg_attr(not(test), allow(dead_code))]
    FixedRight,
}

/// Where to put a newly added window.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAddWindowTarget<'a, W: LayoutElement> {
    /// No particular preference.
    #[default]
    Auto,
    /// As a new column at this index.
    NewColumnAt(usize),
    /// Next to this existing window.
    NextTo(&'a W::Id),
}

impl OutputId {
    pub fn new(output: &Output) -> Self {
        let output_name = output.user_data().get::<OutputName>().unwrap();
        Self(output_name.format_make_model_serial_or_connector())
    }
}

impl FloatingActive {
    fn get(self) -> bool {
        self == Self::Yes
    }
}

impl<W: LayoutElement> Workspace<W> {
    pub fn new(output: Output, clock: Clock, options: Rc<Options>) -> Self {
        Self::new_with_config(output, None, clock, options)
    }

    pub fn new_with_config(
        output: Output,
        mut config: Option<WorkspaceConfig>,
        clock: Clock,
        base_options: Rc<Options>,
    ) -> Self {
        let original_output = config
            .as_ref()
            .and_then(|c| c.open_on_output.clone())
            .map(OutputId)
            .unwrap_or(OutputId::new(&output));

        let layout_config = config.as_mut().and_then(|c| c.layout.take().map(|x| x.0));

        let scale = output.current_scale();
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(layout_config.as_ref())
                .adjusted_for_scale(scale.fractional_scale()),
        );

        let view_size = output_size(&output);
        let working_area = compute_working_area(&output);

        let scrolling = ScrollingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let floating = FloatingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);

        Self {
            scrolling,
            floating,
            floating_is_active: FloatingActive::No,
            fixed_insets: (0., 0.),
            original_output,
            scale,
            transform: output.current_transform(),
            view_size,
            working_area,
            shadow: Shadow::new(shadow_config),
            background_buffer: SolidColorBuffer::new(view_size, options.layout.background_color),
            left_fade_offscreen: OffscreenBuffer::default(),
            right_fade_offscreen: OffscreenBuffer::default(),
            output: Some(output),
            clock,
            base_options,
            options,
            name: config.map(|c| c.name.0),
            layout_config,
            id: WorkspaceId::next(),
        }
    }

    pub fn new_with_config_no_outputs(
        mut config: Option<WorkspaceConfig>,
        clock: Clock,
        base_options: Rc<Options>,
    ) -> Self {
        let original_output = OutputId(
            config
                .as_ref()
                .and_then(|c| c.open_on_output.clone())
                .unwrap_or_default(),
        );

        let layout_config = config.as_mut().and_then(|c| c.layout.take().map(|x| x.0));

        let scale = smithay::output::Scale::Integer(1);
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(layout_config.as_ref())
                .adjusted_for_scale(scale.fractional_scale()),
        );

        let view_size = Size::from((1280., 720.));
        let working_area = Rectangle::from_size(Size::from((1280., 720.)));

        let scrolling = ScrollingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let floating = FloatingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);

        Self {
            scrolling,
            floating,
            floating_is_active: FloatingActive::No,
            fixed_insets: (0., 0.),
            output: None,
            scale,
            transform: Transform::Normal,
            original_output,
            view_size,
            working_area,
            shadow: Shadow::new(shadow_config),
            background_buffer: SolidColorBuffer::new(view_size, options.layout.background_color),
            left_fade_offscreen: OffscreenBuffer::default(),
            right_fade_offscreen: OffscreenBuffer::default(),
            clock,
            base_options,
            options,
            name: config.map(|c| c.name.0),
            layout_config,
            id: WorkspaceId::next(),
        }
    }

    pub fn new_no_outputs(clock: Clock, options: Rc<Options>) -> Self {
        Self::new_with_config_no_outputs(None, clock, options)
    }

    pub fn id(&self) -> WorkspaceId {
        self.id
    }

    pub fn name(&self) -> Option<&String> {
        self.name.as_ref()
    }

    pub fn unname(&mut self) {
        self.name = None;
    }

    pub fn has_windows_or_name(&self) -> bool {
        self.has_windows() || self.name.is_some()
    }

    pub fn scale(&self) -> smithay::output::Scale {
        self.scale
    }

    pub fn advance_animations(&mut self) {
        self.scrolling.advance_animations();
        self.floating.advance_animations();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.scrolling.are_animations_ongoing() || self.floating.are_animations_ongoing()
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.scrolling.are_transitions_ongoing() || self.floating.are_transitions_ongoing()
    }

    /// The carousel's parent area: the workspace working area inset on each
    /// side by the width of a non-empty fixed-side panel *plus* one inter-window
    /// gap, so the carousel is separated from a panel by the same gap that
    /// separates two windows. With both panels empty this equals
    /// `self.working_area`, so the carousel spans the full width exactly as
    /// before (no gap is added on a side with no panel); a populated panel
    /// shrinks the carousel to the space remaining between the panels, minus the
    /// gap. The carousel still *renders* past these edges while scrolling (its
    /// edge tiles fade out behind the panels — see the render path), but its
    /// resting layout is confined here.
    fn carousel_parent_area(&self) -> Rectangle<f64, Logical> {
        let (left, right) = self.fixed_insets;
        let gaps = self.options.layout.gaps;
        // Only a side that actually has a panel gets the separating gap.
        let left_inset = if left > 0. { left + gaps } else { 0. };
        let right_inset = if right > 0. { right + gaps } else { 0. };
        let mut area = self.working_area;
        area.loc.x += left_inset;
        area.size.w = (area.size.w - left_inset - right_inset).max(0.);
        area
    }

    /// Push the owning monitor's fixed-side panel widths into this workspace.
    /// No-op when unchanged; otherwise re-inset the carousel so it never
    /// overlaps a panel (and re-fit toward preferred widths, letting freed
    /// space fill in).
    pub(super) fn set_fixed_insets(&mut self, left: f64, right: f64) {
        if self.fixed_insets == (left, right) {
            return;
        }
        self.fixed_insets = (left, right);
        self.sync_carousel_parent_area();
    }

    /// Re-fit the carousel after a layout change (add / close / resize / move).
    ///
    /// Runs both mode-specific resizers; each is a no-op outside its mode, so
    /// callers can invoke this unconditionally and get the right behavior:
    /// - `disable-carousel`: proportionally scale every column so the row fits
    ///   between the fixed-side panels, growing the columns back toward their
    ///   preferred (natural) widths whenever space frees up and shrinking when
    ///   the row would overflow, then re-center it (see
    ///   [`ScrollingSpace::fit_columns_to_parent`]).
    /// - scrolling carousel: grow the columns up to the minimum visible span
    ///   (see [`Self::grow_to_min_carousel_span`]).
    /// Re-fit the disable-carousel row. `animate` controls whether the width change
    /// tweens: paths that add a window pass `false` so the columns take their fitted
    /// widths *upfront*, before the new window's open animation — otherwise the window
    /// would open at its full natural width (partly off-screen) and only then shrink in.
    /// Resize/close paths pass `true` for a smooth grow/shrink.
    fn refit_carousel(&mut self, animate: bool) {
        self.scrolling.fit_columns_to_parent(animate);
        self.grow_to_min_carousel_span();
    }

    /// Re-fit the carousel when space is *freed* (a window/column closed, or a
    /// fixed-side panel shrank), letting the disable-carousel row grow back
    /// toward each column's preferred (natural) width. The shared shrink factor
    /// recomputes toward `1.0`, so survivors grow *proportionally* — the
    /// natural-width ratios set by the user are preserved (a column twice as
    /// wide stays twice as wide).
    ///
    /// Deliberately does *not* call [`Self::grow_to_min_carousel_span`]: the
    /// scrolling-carousel half-screen floor is a separate behavior we leave
    /// untouched on close. Outside disable-carousel mode
    /// [`ScrollingSpace::fit_columns_to_parent`] is a no-op, so this is inert
    /// there and the normal carousel close path is unchanged.
    fn refit_carousel_grow_to_preferred(&mut self, animate: bool) {
        self.scrolling.fit_columns_to_parent(animate);
    }

    /// Floor on how much of the workspace the carousel must visually occupy.
    /// After a column add / close / resize, if the total carousel content extent
    /// drops below `min_fraction × view_extent`, all columns are scaled up
    /// proportionally to hit the floor — preserving relative widths.
    ///
    /// `min_fraction` is 1/3 on ultrawide outputs (≥21:9 either way), 1/2
    /// otherwise. The "view extent" is `view_size.w` on landscape and
    /// `view_size.h` on portrait, so the floor tracks the orientation the user
    /// physically sees. No-op when:
    ///   - `disable-carousel` is on (it already pins widths to fit), or
    ///   - the carousel is empty, or
    ///   - the carousel already meets the floor, or
    ///   - the carousel can't grow further without overflowing the inset area
    ///     (i.e. fixed-side panels eat the working area down to the floor).
    fn grow_to_min_carousel_span(&mut self) {
        if self.options.layout.disable_carousel {
            return;
        }
        // A lone column is the user's explicit width choice — never override it
        // with the floor (matching the documented "min-span never grows a lone
        // column" behavior). The floor exists to keep a *multi-column* row from
        // collapsing into a sliver; a single column resized below it (e.g. via
        // `set-column-width`) must be honored, not snapped back up.
        if self.scrolling.column_count() <= 1 {
            return;
        }

        let landscape = self.view_size.w >= self.view_size.h;
        let view_extent = if landscape {
            self.view_size.w
        } else {
            self.view_size.h
        };
        if view_extent <= 0.0 {
            return;
        }

        // Treat the output as "ultrawide" when either axis is ≥21:9 of the
        // other — so a rotated portrait monitor counts too.
        let (long, short) = if self.view_size.w >= self.view_size.h {
            (self.view_size.w, self.view_size.h.max(1.0))
        } else {
            (self.view_size.h, self.view_size.w.max(1.0))
        };
        let is_uw = long / short >= 21.0 / 9.0;
        let min_fraction = if is_uw { 1.0 / 3.0 } else { 1.0 / 2.0 };
        let min_span = view_extent * min_fraction;

        // Measure the columns' target layout width, not their cached rendered
        // width: a window that committed smaller than its column would otherwise
        // make the row look narrower than it really is and get inflated to the
        // floor. See [`ScrollingSpace::target_content_width`].
        let current = self.scrolling.target_content_width();
        if current >= min_span || current <= 0.0 {
            return;
        }

        // Don't push the carousel past what its inset parent area can hold:
        // if the fixed-side strips eat enough horizontal space that the floor
        // is bigger than the carousel viewport, cap at the viewport so widths
        // remain physically realizable.
        let cap = self.carousel_parent_area().size.w;
        let target = min_span.min(cap);
        if target <= current {
            return;
        }

        let factor = target / current;
        self.scrolling.scale_all_columns_widths(factor);
    }

    /// Re-inset the carousel when a fixed-side panel's width changes (windows
    /// added/removed/resized in a panel). Cheap no-op when the area is
    /// unchanged, so it's safe to call every frame.
    fn sync_carousel_parent_area(&mut self) {
        let desired = self.carousel_parent_area();
        if desired != self.scrolling.parent_area() {
            self.scrolling.update_config(
                self.view_size,
                desired,
                self.scale.fractional_scale(),
                self.options.clone(),
            );
            // A fixed-side panel just grew or shrank, changing the carousel's
            // usable width. Re-fit so every column still fits between the panels
            // (the panels are not part of the carousel) — shrinking when a panel
            // grows into the carousel, and growing the columns back proportionally
            // toward their preferred widths when a panel frees space. Then
            // re-center.
            self.refit_carousel_grow_to_preferred(true);
            self.scrolling.auto_fit_or_center_view_offset();
        }
    }

    /// `panel_focus` is the monitor-global fixed-side panel that owns focus, if
    /// any (the panels live on the [`Monitor`](super::monitor::Monitor) now). It
    /// suppresses the carousel's focus ring while a panel is focused so the user
    /// can tell where keyboard input is actually going.
    pub fn update_render_elements(&mut self, is_active: bool, panel_focus: Option<FixedSide>) {
        self.sync_carousel_parent_area();

        // Last stop before the row is laid out for display: make sure it fits the
        // screen. Every path that changes a column's width re-fits explicitly, but
        // "the row never exceeds the screen" is the whole point of disable-carousel
        // mode, so it's enforced here as well rather than left to the discipline of
        // a dozen call sites — including ones added later. Cheap no-op unless a
        // width actually needs to change, and inert outside disable-carousel mode.
        self.scrolling.enforce_fit_to_parent();

        let tiling_active = is_active && !self.floating_is_active.get();
        let scrolling_focused = tiling_active && panel_focus.is_none();

        self.scrolling.update_render_elements(scrolling_focused);

        let view_rect = Rectangle::from_size(self.view_size);
        self.floating
            .update_render_elements(is_active && self.floating_is_active.get(), view_rect);

        self.shadow.update_render_elements(
            self.view_size,
            true,
            CornerRadius::default(),
            self.scale.fractional_scale(),
            1.,
        );
    }

    pub fn update_config(&mut self, base_options: Rc<Options>) {
        let scale = self.scale.fractional_scale();
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(self.layout_config.as_ref())
                .adjusted_for_scale(scale),
        );

        self.scrolling.update_config(
            self.view_size,
            self.carousel_parent_area(),
            self.scale.fractional_scale(),
            options.clone(),
        );

        self.floating.update_config(
            self.view_size,
            self.working_area,
            self.scale.fractional_scale(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, self.view_size);
        self.shadow.update_config(shadow_config);

        self.background_buffer
            .set_color(options.layout.background_color);

        self.base_options = base_options;
        self.options = options;
    }

    pub fn update_layout_config(&mut self, layout_config: Option<naru_config::LayoutPart>) {
        if self.layout_config == layout_config {
            return;
        }

        self.layout_config = layout_config;
        self.update_config(self.base_options.clone());
    }

    pub fn update_shaders(&mut self) {
        self.scrolling.update_shaders();
        self.floating.update_shaders();
        self.shadow.update_shaders();
    }

    /// True if any window in this workspace (any layer) reports the given
    /// xdg `app_id`. Used by the auto-open-floating default so a second copy
    /// of an app in the same workspace pops out as floating instead of
    /// displacing the primary tile.
    pub fn has_window_with_app_id(&self, app_id: &str) -> bool {
        self.windows()
            .any(|w| w.app_id().as_deref() == Some(app_id))
    }

    pub fn windows(&self) -> impl Iterator<Item = &W> + '_ {
        self.tiles().map(Tile::window)
    }

    pub fn windows_mut(&mut self) -> impl Iterator<Item = &mut W> + '_ {
        self.tiles_mut().map(Tile::window_mut)
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        let scrolling = self.scrolling.tiles();
        let floating = self.floating.tiles();
        scrolling.chain(floating)
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        let scrolling = self.scrolling.tiles_mut();
        let floating = self.floating.tiles_mut();
        scrolling.chain(floating)
    }

    pub fn is_floating(&self, id: &W::Id) -> bool {
        self.floating.has_window(id)
    }

    /// Which sub-layout currently holds `id`, or `None` if this workspace does
    /// not contain it. Unlike [`Self::layer_for`] this never guesses — it is a
    /// pure membership query, used by the test harness to assert routing.
    #[cfg(test)]
    pub(crate) fn window_slot(&self, id: &W::Id) -> Option<WindowLayer> {
        if self.floating.has_window(id) {
            Some(WindowLayer::Floating)
        } else if self.scrolling.columns().any(|col| col.contains(id)) {
            Some(WindowLayer::Scrolling)
        } else {
            None
        }
    }

    /// Which sub-layout a per-window operation should target.
    ///
    /// `Some(id)` is resolved by membership; `None` (meaning "the active
    /// window") follows the active-layer signals. The carousel is the
    /// fallback so an unknown id behaves exactly as before this routing
    /// existed.
    fn layer_for(&self, window: Option<&W::Id>) -> WindowLayer {
        match window {
            Some(id) => {
                if self.floating.has_window(id) {
                    WindowLayer::Floating
                } else {
                    WindowLayer::Scrolling
                }
            }
            None => {
                if self.floating_is_active.get() {
                    WindowLayer::Floating
                } else {
                    WindowLayer::Scrolling
                }
            }
        }
    }

    pub fn current_output(&self) -> Option<&Output> {
        self.output.as_ref()
    }

    pub fn active_window(&self) -> Option<&W> {
        if self.floating_is_active.get() {
            return self.floating.active_window();
        }
        self.scrolling.active_window()
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        if self.floating_is_active.get() {
            return self.floating.active_window_mut();
        }
        self.scrolling.active_window_mut()
    }

    pub fn is_active_pending_fullscreen(&self) -> bool {
        self.scrolling.is_active_pending_fullscreen()
    }

    pub fn set_output(&mut self, output: Option<Output>) {
        if self.output == output {
            return;
        }

        if let Some(output) = self.output.take() {
            for win in self.windows() {
                win.output_leave(&output);
            }
        }

        self.output = output;

        if let Some(output) = &self.output {
            // Normalize original output: possibly replace connector with make/model/serial.
            if self.original_output.matches(output) {
                self.original_output = OutputId::new(output);
            }

            self.update_output_size();

            for win in self.windows() {
                self.enter_output_for_window(win);
            }
        }
    }

    fn enter_output_for_window(&self, window: &W) {
        if let Some(output) = &self.output {
            window.set_preferred_scale_transform(self.scale, self.transform);
            window.output_enter(output);
        }
    }

    pub fn update_output_size(&mut self) {
        let output = self.output.as_ref().unwrap();
        let scale = output.current_scale();
        let transform = output.current_transform();
        let view_size = output_size(output);
        let working_area = compute_working_area(output);
        self.set_view_size(scale, transform, view_size, working_area);
    }

    fn set_view_size(
        &mut self,
        scale: smithay::output::Scale,
        transform: Transform,
        size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
    ) {
        let scale_transform_changed = self.transform != transform
            || self.scale.integer_scale() != scale.integer_scale()
            || self.scale.fractional_scale() != scale.fractional_scale();
        if !scale_transform_changed && self.view_size == size && self.working_area == working_area {
            return;
        }

        let fractional_scale_changed = self.scale.fractional_scale() != scale.fractional_scale();

        self.scale = scale;
        self.transform = transform;
        self.view_size = size;
        self.working_area = working_area;

        if fractional_scale_changed {
            // Options need to be recomputed for the new scale.
            self.update_config(self.base_options.clone());
        } else {
            // Pass our existing options as is.
            self.scrolling.update_config(
                size,
                self.carousel_parent_area(),
                scale.fractional_scale(),
                self.options.clone(),
            );
            self.floating.update_config(
                size,
                working_area,
                scale.fractional_scale(),
                self.options.clone(),
            );

            let shadow_config =
                compute_workspace_shadow_config(self.options.overview.workspace_shadow, size);
            self.shadow.update_config(shadow_config);
        }

        self.background_buffer.resize(size);

        if scale_transform_changed {
            for window in self.windows() {
                window.set_preferred_scale_transform(self.scale, self.transform);
            }
        }
    }

    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn make_tile(&self, window: W) -> Tile<W> {
        Tile::new(
            window,
            self.view_size,
            self.scale.fractional_scale(),
            self.clock.clone(),
            self.options.clone(),
        )
    }

    /// The visual rectangle a newly-floating popup should be centered over: its declared
    /// parent window if it has one, otherwise the active window when that shares the
    /// popup's `app_id` — a fixed-size dialog (color picker, open-file, …) that didn't
    /// declare a parent but plainly belongs to the app the user was just using. Centering
    /// on this drops the popup on top of its owner instead of the middle of the screen.
    /// `None` when no owner is found, in which case placement falls back to screen-center.
    fn floating_owner_rect(&self, popup: &W) -> Option<Rectangle<f64, Logical>> {
        let owner_id = self
            .windows()
            .find(|w| popup.is_child_of(w))
            .map(|w| w.id().clone())
            .or_else(|| {
                let app_id = popup.app_id()?;
                self.active_window()
                    .filter(|w| {
                        w.id() != popup.id() && w.app_id().as_deref() == Some(app_id.as_str())
                    })
                    .map(|w| w.id().clone())
            })?;
        self.popup_target_rect(&owner_id)
    }

    pub fn add_tile(
        &mut self,
        mut tile: Tile<W>,
        target: WorkspaceAddWindowTarget<W>,
        activate: ActivateWindow,
        width: ColumnWidth,
        is_full_width: bool,
        is_floating: bool,
    ) {
        self.enter_output_for_window(tile.window());
        tile.restore_to_floating = is_floating;

        match target {
            WorkspaceAddWindowTarget::Auto => {
                // Don't steal focus from an active fullscreen window.
                let activate = activate.map_smart(|| !self.is_active_pending_fullscreen());

                // If the tile is pending maximized or fullscreen, open it in the scrolling layout
                // where it can do that.
                if is_floating && tile.window().pending_sizing_mode().is_normal() {
                    let center_on = self.floating_owner_rect(tile.window());
                    self.floating.add_tile(tile, activate, center_on);

                    if activate || self.scrolling.is_empty() {
                        self.floating_is_active = FloatingActive::Yes;
                    }
                } else {
                    // Every new window opens in a fresh column to the right of
                    // the active one, at its natural width, independent of the
                    // active window's size.
                    //
                    // With `disable-carousel` the workspace can't scroll, so
                    // `refit_carousel` then shrinks every column by one shared
                    // proportional factor if they'd overflow the area between the
                    // fixed-side panels — never growing any column past its
                    // natural width — and re-centers the row.
                    self.scrolling
                        .add_tile(None, tile, activate, width, is_full_width, None);
                    // Adding a column re-fits the row, shrinking it to make room.
                    self.refit_carousel(false);

                    if activate {
                        self.floating_is_active = FloatingActive::No;
                    }
                }
            }
            WorkspaceAddWindowTarget::NewColumnAt(col_idx) => {
                let activate = activate.map_smart(|| false);
                self.scrolling
                    .add_tile(Some(col_idx), tile, activate, width, is_full_width, None);

                if activate {
                    self.floating_is_active = FloatingActive::No;
                }
            }
            WorkspaceAddWindowTarget::NextTo(next_to) => {
                // `active_window()` can be `None` (e.g. a workspace holding only a
                // non-active floating window), so default the smart-activate to
                // `false` rather than unwrapping.
                let activate =
                    activate.map_smart(|| self.active_window().is_some_and(|w| w.id() == next_to));

                let floating_has_window = self.floating.has_window(next_to);

                if is_floating && tile.window().pending_sizing_mode().is_normal() {
                    if floating_has_window {
                        self.floating.add_tile_above(next_to, tile, activate);
                    } else {
                        // FIXME: use static pos
                        // `next_to` may be a carousel *or* fixed-strip window,
                        // so search the workspace-wide iterator rather than
                        // just the carousel's (which would `unwrap()`-panic).
                        let (next_to_tile, render_pos, _visible) = self
                            .tiles_with_render_positions()
                            .find(|(tile, _, _)| tile.window().id() == next_to)
                            .unwrap();

                        // Position the new tile in the center above the next_to tile. Think a
                        // dialog opening on top of a window.
                        let tile_size = tile.tile_size();
                        let pos = render_pos
                            + (next_to_tile.tile_size().to_point() - tile_size.to_point())
                                .downscale(2.);
                        let pos = self.floating.clamp_within_working_area(pos, tile_size);
                        let pos = self.floating.logical_to_size_frac(pos);
                        tile.floating_pos = Some(pos);

                        self.floating.add_tile(tile, activate, None);
                    }

                    if activate || self.scrolling.is_empty() {
                        self.floating_is_active = FloatingActive::Yes;
                    }
                } else if floating_has_window {
                    self.scrolling
                        .add_tile(None, tile, activate, width, is_full_width, None);

                    if activate {
                        self.floating_is_active = FloatingActive::No;
                    }
                } else {
                    self.scrolling
                        .add_tile_right_of(next_to, tile, activate, width, is_full_width);

                    if activate {
                        self.floating_is_active = FloatingActive::No;
                    }
                }
            }
        }
    }

    pub fn add_tile_to_column(
        &mut self,
        col_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
    ) {
        self.enter_output_for_window(tile.window());
        self.scrolling
            .add_tile_to_column(col_idx, tile_idx, tile, activate);

        if activate {
            self.floating_is_active = FloatingActive::No;
        }
    }

    pub fn add_column(&mut self, column: Column<W>, activate: bool) {
        for (tile, _) in column.tiles() {
            self.enter_output_for_window(tile.window());
        }

        self.scrolling.add_column(None, column, activate, None);

        if activate {
            self.floating_is_active = FloatingActive::No;
        }
    }

    fn update_focus_floating_tiling_after_removing(&mut self, removed_from_floating: bool) {
        if removed_from_floating {
            if self.floating.is_empty() {
                self.floating_is_active = FloatingActive::No;
            }
        } else {
            // Scrolling should remain focused if both are empty.
            if self.scrolling.is_empty() && !self.floating.is_empty() {
                self.floating_is_active = FloatingActive::Yes;
            }
        }
    }

    /// Removes a carousel window's tile. The floating layer, output-leave, and
    /// focus updates are layered on by callers. (Fixed-side panels are owned by
    /// the monitor and removed there.)
    fn remove_tiled_tile(&mut self, id: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        self.scrolling.remove_tile(id, transaction)
    }

    pub fn remove_tile(&mut self, id: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        let mut from_floating = false;
        let removed = if self.floating.has_window(id) {
            from_floating = true;
            self.floating.remove_tile(id)
        } else {
            self.remove_tiled_tile(id, transaction)
        };

        if let Some(output) = &self.output {
            removed.tile.window().output_leave(output);
        }

        self.update_focus_floating_tiling_after_removing(from_floating);

        // Closing a window frees space: in disable-carousel mode grow the
        // surviving columns back proportionally toward their preferred widths.
        self.refit_carousel_grow_to_preferred(true);

        removed
    }

    pub fn remove_active_tile(&mut self, transaction: Transaction) -> Option<RemovedTile<W>> {
        let from_floating = self.floating_is_active.get();
        let removed = if from_floating {
            self.floating.remove_active_tile()?
        } else {
            self.scrolling.remove_active_tile(transaction)?
        };

        if let Some(output) = &self.output {
            removed.tile.window().output_leave(output);
        }

        self.update_focus_floating_tiling_after_removing(from_floating);
        self.refit_carousel_grow_to_preferred(true);

        Some(removed)
    }

    pub fn remove_active_column(&mut self) -> Option<Column<W>> {
        let from_floating = self.floating_is_active.get();
        if from_floating {
            return None;
        }

        let column = self.scrolling.remove_active_column()?;

        if let Some(output) = &self.output {
            for (tile, _) in column.tiles() {
                tile.window().output_leave(output);
            }
        }

        self.update_focus_floating_tiling_after_removing(from_floating);
        self.refit_carousel_grow_to_preferred(true);

        Some(column)
    }

    pub fn resolve_default_width(
        &self,
        default_width: Option<Option<PresetSize>>,
        is_floating: bool,
        app_id: Option<&str>,
    ) -> Option<PresetSize> {
        match default_width {
            Some(Some(width)) => Some(width),
            Some(None) => None,
            None if is_floating => None,
            // Media players get a narrower default (1/5 ultrawide, 1/3 otherwise) that overrides
            // the global `default-column-width`; other windows fall back to the global default,
            // then to the ultrawide-only terminal/non-terminal defaults.
            None => media_player_default_column_width(
                self.view_size,
                app_id,
                &self.options.layout.media_player_app_ids,
                self.options.layout.media_player_column_width,
                self.options.layout.media_player_ultrawide_column_width,
            )
            .or(self.options.layout.default_column_width)
            .or_else(|| {
                ultrawide_default_column_width(
                    self.view_size,
                    app_id,
                    &self.options.layout.terminal_app_ids,
                    self.options.layout.ultrawide_default_column_width,
                    self.options.layout.ultrawide_terminal_column_width,
                )
            }),
        }
    }

    pub fn resolve_default_height(
        &self,
        default_height: Option<Option<PresetSize>>,
        is_floating: bool,
    ) -> Option<PresetSize> {
        match default_height {
            Some(Some(height)) => Some(height),
            Some(None) => None,
            None if is_floating => None,
            // We don't have a global default at the moment.
            None => None,
        }
    }

    pub fn new_window_size(
        &self,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        is_floating: bool,
        rules: &ResolvedWindowRules,
        (min_size, max_size): (Size<i32, Logical>, Size<i32, Logical>),
    ) -> Size<i32, Logical> {
        let mut size = if is_floating {
            self.floating.new_window_size(width, height, rules)
        } else {
            self.scrolling.new_window_size(width, height, rules)
        };

        // If the window has a fixed size, or we're picking some fixed size, apply min and max
        // size. This is to ensure that a fixed-size window rule works on open, while still
        // allowing the window freedom to pick its default size otherwise.
        let (min_size, max_size) = rules.apply_min_max_size(min_size, max_size);
        size.w = ensure_min_max_size_maybe_zero(size.w, min_size.w, max_size.w);
        // For scrolling (where height is > 0) only ensure fixed height, since at runtime scrolling
        // will only honor fixed height currently.
        if min_size.h == max_size.h {
            size.h = ensure_min_max_size(size.h, min_size.h, max_size.h);
        } else if size.h > 0 {
            // Also always honor min height, scrolling always does.
            size.h = max(size.h, min_size.h);
        }

        size
    }

    pub fn configure_new_window(
        &self,
        window: &Window,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        is_floating: bool,
        rules: &ResolvedWindowRules,
    ) {
        window.with_surfaces(|surface, data| {
            send_scale_transform(surface, data, self.scale, self.transform);
        });

        let toplevel = window.toplevel().expect("no x11 support");
        let (min_size, max_size) = with_states(toplevel.wl_surface(), |state| {
            let mut guard = state.cached_state.get::<SurfaceCachedState>();
            let current = guard.current();
            (current.min_size, current.max_size)
        });
        toplevel.with_pending_state(|state| {
            if state.states.contains(xdg_toplevel::State::Fullscreen) {
                state.size = Some(self.view_size.to_i32_round());
            } else if state.states.contains(xdg_toplevel::State::Maximized) {
                state.size = Some(self.working_area.size.to_i32_round());
            } else {
                let size =
                    self.new_window_size(width, height, is_floating, rules, (min_size, max_size));
                state.size = Some(size);
            }

            if is_floating {
                state.bounds = Some(self.floating.new_window_toplevel_bounds(rules));
            } else {
                state.bounds = Some(self.scrolling.new_window_toplevel_bounds(rules));
            }
        });
    }

    pub(super) fn resolve_scrolling_width(
        &self,
        window: &W,
        width: Option<PresetSize>,
    ) -> ColumnWidth {
        let width = width.unwrap_or_else(|| PresetSize::Fixed(window.size().w));
        match width {
            PresetSize::Fixed(fixed) => {
                let mut fixed = f64::from(fixed);

                // Add border width since ColumnWidth includes borders.
                let rules = window.rules();
                let border = self.options.layout.border.merged_with(&rules.border);
                if !border.off {
                    fixed += border.width * 2.;
                }

                ColumnWidth::Fixed(fixed)
            }
            PresetSize::Proportion(prop) => ColumnWidth::Proportion(prop),
        }
    }

    pub fn focus_left(&mut self) -> bool {
        self.focus_left_in_layer() || self.focus_in_direction_cross_layer(FocusDir::Left)
    }

    pub fn focus_right(&mut self) -> bool {
        self.focus_right_in_layer() || self.focus_in_direction_cross_layer(FocusDir::Right)
    }

    pub fn focus_up(&mut self) -> bool {
        self.focus_up_in_layer() || self.focus_in_direction_cross_layer(FocusDir::Up)
    }

    pub fn focus_down(&mut self) -> bool {
        self.focus_down_in_layer() || self.focus_in_direction_cross_layer(FocusDir::Down)
    }

    /// Positional focus across the floating↔tiling boundary. When in-layer
    /// directional focus can't move any further in `dir` (the `*_in_layer`
    /// call returned false), jump to the nearest window in the *other* layer
    /// in that direction — the candidate closest along `dir`, with a penalty
    /// for perpendicular misalignment, so focus crosses between floating and
    /// tiled windows the way the eye expects. Returns false if there's no such
    /// window.
    fn focus_in_direction_cross_layer(&mut self, dir: FocusDir) -> bool {
        let Some(active_id) = self.active_window().map(|w| w.id().clone()) else {
            return false;
        };
        let from_floating = self.floating_is_active();

        // Collect render rects (workspace-local) in one immutable pass so the
        // borrow is released before we activate the chosen window.
        let mut active_rect = None;
        let mut candidates: Vec<(W::Id, Rectangle<f64, Logical>)> = Vec::new();
        for (tile, pos, visible) in self.tiles_with_render_positions() {
            let id = tile.window().id().clone();
            let rect = Rectangle::new(pos, tile.tile_size());
            if id == active_id {
                active_rect = Some(rect);
                continue;
            }
            if visible {
                candidates.push((id, rect));
            }
        }
        let Some(from) = active_rect else {
            return false;
        };

        let best = candidates
            .into_iter()
            // Cross-layer only: keep windows in the opposite layer.
            .filter(|(id, _)| self.floating.has_window(id) != from_floating)
            .filter_map(|(id, to)| directional_score(from, to, dir).map(|s| (id, s)))
            .min_by(|(_, a), (_, b)| a.total_cmp(b));

        if let Some((id, _)) = best {
            self.activate_window(&id);
            true
        } else {
            false
        }
    }

    /// Carousel/floating-only horizontal in-layer focus. The carousel↔panel
    /// hand-off is composed on top of this by the
    /// [`Monitor`](super::monitor::Monitor), which owns the panels.
    pub(super) fn focus_left_in_layer(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_left()
        } else {
            self.scrolling.focus_left()
        }
    }

    pub(super) fn focus_right_in_layer(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_right()
        } else {
            self.scrolling.focus_right()
        }
    }

    /// `true` when the carousel holds no columns. Used by the Monitor to decide
    /// whether a panel→carousel focus hop has a landing column.
    pub(super) fn scrolling_is_empty(&self) -> bool {
        self.scrolling.is_empty()
    }

    /// Focus the carousel's first/last column directly (no floating fallback) —
    /// used by the Monitor when focus hops out of a panel into the carousel.
    pub(super) fn carousel_focus_column_first(&mut self) {
        self.scrolling.focus_column_first();
    }

    pub(super) fn carousel_focus_column_last(&mut self) {
        self.scrolling.focus_column_last();
    }

    pub(super) fn focus_cross_layer_left(&mut self) -> bool {
        self.focus_in_direction_cross_layer(FocusDir::Left)
    }

    pub(super) fn focus_cross_layer_right(&mut self) -> bool {
        self.focus_in_direction_cross_layer(FocusDir::Right)
    }

    pub fn focus_column_first(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_leftmost();
        } else {
            self.scrolling.focus_column_first();
        }
    }

    pub fn focus_column_last(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_rightmost();
        } else {
            self.scrolling.focus_column_last();
        }
    }

    pub fn focus_column_right_or_first(&mut self) {
        if !self.focus_right() {
            self.focus_column_first();
        }
    }

    pub fn focus_column_left_or_last(&mut self) {
        if !self.focus_left() {
            self.focus_column_last();
        }
    }

    pub fn focus_column(&mut self, index: usize) {
        if self.floating_is_active.get() {
            self.focus_tiling();
        }
        self.scrolling.focus_column(index);
    }

    /// Visual center-X of the active scrolling column. Returns None if the
    /// scrolling layout is empty or floating is active. Used by Layout-level
    /// focus/move-up/down to carry positional info across workspaces.
    pub fn active_column_visual_center_x(&self) -> Option<f64> {
        if self.floating_is_active.get() {
            return None;
        }
        self.scrolling.active_column_visual_center_x()
    }

    /// Find the scrolling column whose visual center-X is closest to `target_x`.
    /// Returns None if the scrolling layout is empty.
    pub fn closest_column_to_visual_center_x(&self, target_x: f64) -> Option<usize> {
        self.scrolling.closest_column_to_visual_center_x(target_x)
    }

    pub fn focus_window_in_column(&mut self, index: u8) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.focus_window_in_column(index);
    }

    pub(super) fn focus_down_in_layer(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_down()
        } else {
            self.scrolling.focus_down()
        }
    }

    pub(super) fn focus_up_in_layer(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_up()
        } else {
            self.scrolling.focus_up()
        }
    }

    pub fn focus_down_or_left(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_down();
        } else {
            self.scrolling.focus_down_or_left();
        }
    }

    pub fn focus_down_or_right(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_down();
        } else {
            self.scrolling.focus_down_or_right();
        }
    }

    pub fn focus_up_or_left(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_up();
        } else {
            self.scrolling.focus_up_or_left();
        }
    }

    pub fn focus_up_or_right(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_up();
        } else {
            self.scrolling.focus_up_or_right();
        }
    }

    pub fn focus_window_top(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_topmost();
        } else {
            self.scrolling.focus_top();
        }
    }

    pub fn focus_window_bottom(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_bottommost();
        } else {
            self.scrolling.focus_bottom();
        }
    }

    pub fn focus_window_down_or_top(&mut self) {
        if !self.focus_down() {
            self.focus_window_top();
        }
    }

    pub fn focus_window_up_or_bottom(&mut self) {
        if !self.focus_up() {
            self.focus_window_bottom();
        }
    }

    pub fn move_left(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.move_left();
            true
        } else {
            self.scrolling.move_left()
        }
    }

    pub fn move_right(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.move_right();
            true
        } else {
            self.scrolling.move_right()
        }
    }

    pub fn move_column_to_first(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.move_column_to_first();
    }

    pub fn move_column_to_last(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.move_column_to_last();
    }

    pub fn move_column_to_index(&mut self, index: usize) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.move_column_to_index(index);
    }

    /// Reorder the scrolling single-window columns holding `ordered_ids` into that
    /// relative order (session-restore title reconcile). Returns whether anything moved.
    pub fn reorder_single_window_columns(&mut self, ordered_ids: &[W::Id]) -> bool {
        self.scrolling.reorder_single_window_columns(ordered_ids)
    }

    pub fn move_down(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.move_down();
            true
        } else {
            self.scrolling.move_down()
        }
    }

    pub fn move_up(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.move_up();
            true
        } else {
            self.scrolling.move_up()
        }
    }

    // ---- Stacking move primitives (Phase 4) -----------------------------------------------
    // Each delegates to the underlying ScrollingSpace method. They are no-ops in floating
    // layer for now. Returns true on success, false if at the workspace edge with no
    // applicable target (callers may then move to a neighboring workspace).

    pub fn move_active_window_to_new_column_left(&mut self) -> bool {
        if self.floating_is_active.get() {
            return false;
        }
        self.scrolling.move_active_window_to_new_column_left()
    }

    // --- Carousel column primitives for Monitor's cross-boundary movers ------
    //
    // The carousel↔panel column moves live on the Monitor now (it owns the
    // panels). These expose the carousel half so the Monitor can compose them
    // without reaching into `scrolling` directly.

    pub(super) fn carousel_is_floating_active(&self) -> bool {
        self.floating_is_active.get()
    }

    /// Active carousel column index, or `None` when the carousel is empty.
    pub(super) fn carousel_active_column_index(&self) -> Option<usize> {
        if self.scrolling.is_empty() {
            None
        } else {
            Some(self.scrolling.active_column_index())
        }
    }

    pub(super) fn carousel_column_count(&self) -> usize {
        self.scrolling.column_count()
    }

    pub(super) fn carousel_remove_active_column(&mut self) -> Option<Column<W>> {
        self.scrolling.remove_active_column()
    }

    /// Insert `column` into the carousel at `idx`, activating it (mirrors the
    /// "focus follows the moved window" behaviour of the strip→carousel OUT).
    pub(super) fn carousel_add_column_at(&mut self, idx: usize, column: Column<W>) {
        self.scrolling.add_column(Some(idx), column, true, None);
    }

    pub fn move_active_window_to_new_column_right(&mut self) -> bool {
        if self.floating_is_active.get() {
            return false;
        }
        self.scrolling.move_active_window_to_new_column_right()
    }

    pub fn move_active_window_to_left_neighbor_overlap(&mut self) -> bool {
        if self.floating_is_active.get() {
            return false;
        }
        self.scrolling.move_active_window_to_left_neighbor_overlap()
    }

    pub fn move_active_window_to_right_neighbor_overlap(&mut self) -> bool {
        if self.floating_is_active.get() {
            return false;
        }
        self.scrolling.move_active_window_to_right_neighbor_overlap()
    }

    pub fn move_active_window_to_new_row_above(&mut self) -> bool {
        if self.floating_is_active.get() {
            return false;
        }
        self.scrolling.move_active_window_to_new_row_above()
    }

    pub fn move_active_window_to_new_row_below(&mut self) -> bool {
        if self.floating_is_active.get() {
            return false;
        }
        self.scrolling.move_active_window_to_new_row_below()
    }

    pub fn move_active_window_to_above_neighbor_overlap(&mut self) -> bool {
        if self.floating_is_active.get() {
            return false;
        }
        self.scrolling.move_active_window_to_above_neighbor_overlap()
    }

    pub fn move_active_window_to_below_neighbor_overlap(&mut self) -> bool {
        if self.floating_is_active.get() {
            return false;
        }
        self.scrolling.move_active_window_to_below_neighbor_overlap()
    }

    pub fn move_active_window_to_neighbor_column_as_new_row(&mut self, to_left: bool) -> bool {
        if self.floating_is_active.get() {
            return false;
        }
        self.scrolling
            .move_active_window_to_neighbor_column_as_new_row(to_left)
    }

    /// Extract the active window into a brand-new single-window column on the
    /// given side (`to_left` ⇒ inserted immediately to the left of the source
    /// column, otherwise to the right). This is the path for a source column
    /// that holds more than one window: instead of merging the moved window
    /// into a neighbour's stack, it becomes a column of its own.
    ///
    /// Returns `false` — without side effects — when the floating layer is
    /// active, the carousel is empty, or the active column sits at the carousel
    /// edge on that side (no neighbour to split out next to). The edge case
    /// falls through to the caller's strip handling, matching the single-window
    /// routing.
    pub fn move_active_window_to_new_neighbor_column(&mut self, to_left: bool) -> bool {
        if self.floating_is_active.get() {
            return false;
        }
        let count = self.scrolling.column_count();
        if count == 0 {
            return false;
        }
        let idx = self.scrolling.active_column_index();
        let has_neighbor = if to_left { idx > 0 } else { idx + 1 < count };
        if !has_neighbor {
            return false;
        }
        if to_left {
            self.scrolling.move_active_window_to_new_column_left()
        } else {
            self.scrolling.move_active_window_to_new_column_right()
        }
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        // Floating windows have no columns to consume into / expel from.
        // (Fixed-side panels are routed by the monitor before this is reached.)
        if !self.floating_is_active.get() && self.layer_for(window) == WindowLayer::Scrolling {
            self.scrolling.consume_or_expel_window_left(window);
        }
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        if !self.floating_is_active.get() && self.layer_for(window) == WindowLayer::Scrolling {
            self.scrolling.consume_or_expel_window_right(window);
        }
    }

    pub fn consume_into_column(&mut self) {
        if !self.floating_is_active.get() {
            self.scrolling.consume_into_column();
        }
    }

    pub fn expel_from_column(&mut self) {
        if !self.floating_is_active.get() {
            self.scrolling.expel_from_column();
        }
    }

    pub fn swap_window_in_direction(&mut self, direction: ScrollDirection) {
        if !self.floating_is_active.get() {
            self.scrolling.swap_window_in_direction(direction);
        }
    }

    pub fn toggle_column_tabbed_display(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.toggle_column_tabbed_display();
    }

    pub fn set_column_display(&mut self, display: ColumnDisplay) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.set_column_display(display);
    }

    pub fn center_column(&mut self) {
        if self.floating_is_active.get() {
            self.floating.center_window(None);
        } else {
            self.scrolling.center_column();
        }
    }

    pub fn center_window(&mut self, id: Option<&W::Id>) {
        match self.layer_for(id) {
            WindowLayer::Floating => self.floating.center_window(id),
            _ => self.scrolling.center_window(id),
        }
    }

    pub fn center_visible_columns(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.center_visible_columns();
    }

    pub fn toggle_width(&mut self, forwards: bool) {
        if self.floating_is_active.get() {
            self.floating.toggle_window_width(None, forwards);
            return;
        }
        self.scrolling.toggle_width(forwards);
        self.refit_carousel(true);
        self.scrolling.auto_fit_or_center_view_offset();
    }

    pub fn toggle_full_width(&mut self) {
        if self.floating_is_active.get() {
            // Leave this unimplemented for now. For good UX, this probably needs moving the tile
            // to be against the left edge of the working area while it is full-width.
            return;
        }
        self.scrolling.toggle_full_width();
        self.refit_carousel(true);
        self.scrolling.auto_fit_or_center_view_offset();
    }

    pub fn set_column_width(&mut self, change: SizeChange) {
        if self.floating_is_active.get() {
            self.floating.set_window_width(None, change, true);
            return;
        }
        self.scrolling.set_window_width(None, change);
        self.refit_carousel(true);
        self.scrolling.auto_fit_or_center_view_offset();
    }

    /// Positionally-aware resize of the active column. `toward_right` is true
    /// when the right arrow was pressed, false for the left arrow. The arrow
    /// grows the active window when it points toward the screen center and
    /// shrinks it when it points away — i.e. a window in the left half grows
    /// with the right arrow, a window in the right half grows with the left
    /// arrow. `step` is the (positive) magnitude of the resize.
    pub fn resize_column_positional(&mut self, toward_right: bool, step: SizeChange) {
        // Which half of the working area does the active window sit in? Fixed
        // side-panel windows are unambiguously on their respective side; for the
        // carousel we ask the scrolling layer about the active column's visual
        // position.
        let left_of_center = if self.floating_is_active.get() {
            // Floating windows have no carousel position; fall back to a
            // plain grow so the bind still does something sensible.
            true
        } else {
            match self.scrolling.active_column_is_left_of_center() {
                Some(left) => left,
                // Empty workspace — nothing to resize.
                None => return,
            }
        };

        // The arrow points toward the center (grow) when its direction differs
        // from the side the window is on; otherwise it points away (shrink).
        let grow = left_of_center == toward_right;
        let change = if grow { step } else { negate_size_change(step) };

        self.set_column_width(change);
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        match self.layer_for(window) {
            WindowLayer::Floating => self.floating.set_window_width(window, change, true),
            _ => {
                self.scrolling.set_window_width(window, change);
                self.refit_carousel(true);
                self.scrolling.auto_fit_or_center_view_offset();
            }
        }
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        match self.layer_for(window) {
            WindowLayer::Floating => self.floating.set_window_height(window, change, true),
            _ => {
                self.scrolling.set_window_height(window, change);
                self.refit_carousel(true);
                self.scrolling.auto_fit_or_center_view_offset();
            }
        }
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        match self.layer_for(window) {
            WindowLayer::Floating => {}
            _ => {
                self.scrolling.reset_window_height(window);
                self.scrolling.auto_fit_or_center_view_offset();
            }
        }
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        match self.layer_for(window) {
            WindowLayer::Floating => self.floating.toggle_window_width(window, forwards),
            _ => {
                self.scrolling.toggle_window_width(window, forwards);
                self.refit_carousel(true);
                self.scrolling.auto_fit_or_center_view_offset();
            }
        }
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        match self.layer_for(window) {
            WindowLayer::Floating => self.floating.toggle_window_height(window, forwards),
            _ => {
                self.scrolling.toggle_window_height(window, forwards);
                self.scrolling.auto_fit_or_center_view_offset();
            }
        }
    }

    pub fn expand_column_to_available_width(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.expand_column_to_available_width();
        self.refit_carousel(true);
        self.scrolling.auto_fit_or_center_view_offset();
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) {
        let mut restore_to_floating = false;
        if self.floating.has_window(window) {
            if is_fullscreen {
                restore_to_floating = true;
                self.toggle_window_floating(Some(window));
            } else {
                // Floating windows are never fullscreen, so this is an unfullscreen request for an
                // already unfullscreen window.
                return;
            }
        } else if !is_fullscreen {
            // The window is in the scrolling layout and we're requesting an unfullscreen. If it is
            // indeed fullscreen (i.e. this isn't a duplicate unfullscreen request), then we may
            // need to unfullscreen into floating.
            let col = self
                .scrolling
                .columns()
                .find(|col| col.contains(window))
                .unwrap();

            // When going from fullscreen to maximized, don't consider restore_to_floating yet.
            if col.is_pending_fullscreen() && !col.is_pending_maximized() {
                let (tile, _) = col
                    .tiles()
                    .find(|(tile, _)| tile.window().id() == window)
                    .unwrap();
                if tile.restore_to_floating {
                    // Unfullscreen and float in one call so it has a chance to notice and request a
                    // (0, 0) size, rather than the scrolling column size.
                    self.toggle_window_floating(Some(window));
                    return;
                }
            }
        }

        let tile = self
            .scrolling
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        let was_normal = tile.window().pending_sizing_mode().is_normal();

        self.scrolling.set_fullscreen(window, is_fullscreen);

        // When going from normal to fullscreen, remember if we should unfullscreen to floating.
        let tile = self
            .scrolling
            .tiles_mut()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        if was_normal && !tile.window().pending_sizing_mode().is_normal() {
            tile.restore_to_floating = restore_to_floating;
        }
    }

    pub fn toggle_fullscreen(&mut self, window: &W::Id) {
        let tile = self
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        let current = tile.window().pending_sizing_mode().is_fullscreen();
        self.set_fullscreen(window, !current);
    }

    pub fn set_maximized(&mut self, window: &W::Id, maximize: bool) {
        let mut restore_to_floating = false;
        if self.floating.has_window(window) {
            if maximize {
                restore_to_floating = true;
                self.toggle_window_floating(Some(window));
            } else {
                // Floating windows are never maximized, so this is an unmaximize request for an
                // already unmaximized window.
                return;
            }
        } else if !maximize {
            // The window is in the scrolling layout and we're requesting to unmaximize. If it is
            // indeed maximized (i.e. this isn't a duplicate unmaximize request), then we may
            // need to unmaximize into floating.
            let tile = self
                .scrolling
                .tiles()
                .find(|tile| tile.window().id() == window)
                .unwrap();
            // The tile cannot unmaximize into fullscreen (pending_sizing_mode() will be fullscreen
            // in that case and not maximized), so this check works.
            if tile.window().pending_sizing_mode().is_maximized() && tile.restore_to_floating {
                // Unmaximize and float in one call so it has a chance to notice and request a
                // (0, 0) size, rather than the scrolling column size.
                self.toggle_window_floating(Some(window));
                return;
            }
        }

        let tile = self
            .scrolling
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        let was_normal = tile.window().pending_sizing_mode().is_normal();

        self.scrolling.set_maximized(window, maximize);

        // When going from normal to maximized, remember if we should unmaximize to floating.
        let tile = self
            .scrolling
            .tiles_mut()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        if was_normal && !tile.window().pending_sizing_mode().is_normal() {
            tile.restore_to_floating = restore_to_floating;
        }
    }

    pub fn toggle_maximized(&mut self, window: &W::Id) {
        let mut current = false;

        // We have to check the column property in case the window is in the scrolling layout and
        // both maximized and fullscreen. In this case, only the column knows whether it's
        // maximized.
        //
        // In the floating layout, windows cannot be maximized.
        let col = self
            .scrolling
            .columns()
            .find(|col| col.contains(window));
        if let Some(col) = col {
            current = col.is_pending_maximized();
        }

        self.set_maximized(window, !current);
    }

    pub fn toggle_window_floating(&mut self, id: Option<&W::Id>) {
        let active_id = self.active_window().map(|win| win.id().clone());
        let target_is_active = id.is_none_or(|id| Some(id) == active_id.as_ref());
        let Some(id) = id.cloned().or(active_id) else {
            return;
        };

        let (_, render_pos, _) = self
            .tiles_with_render_positions()
            .find(|(tile, _, _)| *tile.window().id() == id)
            .unwrap();

        if self.floating.has_window(&id) {
            let removed = self.floating.remove_tile(&id);
            // FIXME: compute closest pos?
            self.scrolling.add_tile(
                None,
                removed.tile,
                target_is_active,
                removed.width,
                removed.is_full_width,
                None,
            );
            if target_is_active {
                self.floating_is_active = FloatingActive::No;
            }
        } else {
            let mut removed = self.remove_tiled_tile(&id, Transaction::new());
            removed.tile.stop_move_animations();

            // Come up with a default floating position close to the tile position.
            let stored_or_default = self.floating.stored_or_default_tile_pos(&removed.tile);
            if stored_or_default.is_none() {
                let offset =
                    if self.options.layout.center_focused_column == CenterFocusedColumn::Always {
                        Point::from((0., 0.))
                    } else {
                        Point::from((50., 50.))
                    };
                let pos = render_pos + offset;
                let size = removed.tile.tile_size();
                let pos = self.floating.clamp_within_working_area(pos, size);
                let pos = self.floating.logical_to_size_frac(pos);
                removed.tile.floating_pos = Some(pos);
            }

            self.floating.add_tile(removed.tile, target_is_active, None);
            if target_is_active {
                self.floating_is_active = FloatingActive::Yes;
            }
        }

        let (tile, new_render_pos) = self
            .tiles_with_render_positions_mut(false)
            .find(|(tile, _)| *tile.window().id() == id)
            .unwrap();

        tile.animate_move_from(render_pos - new_render_pos);
    }

    pub fn set_window_floating(&mut self, id: Option<&W::Id>, floating: bool) {
        if id.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(id)
        }) == floating
        {
            return;
        }

        self.toggle_window_floating(id);
    }

    pub fn focus_floating(&mut self) {
        if !self.floating_is_active.get() {
            self.switch_focus_floating_tiling();
        }
    }

    pub fn focus_tiling(&mut self) {
        if self.floating_is_active.get() {
            self.switch_focus_floating_tiling();
        }
    }

    pub fn switch_focus_floating_tiling(&mut self) {
        if self.floating.is_empty() {
            // If floating is empty, keep focus on scrolling.
            return;
        } else if self.scrolling.is_empty() {
            // If floating isn't empty but scrolling is, keep focus on floating.
            return;
        }

        self.floating_is_active = if self.floating_is_active.get() {
            FloatingActive::No
        } else {
            FloatingActive::Yes
        };
    }

    pub fn move_floating_window(
        &mut self,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        let layer = self.layer_for(id);
        if layer == WindowLayer::Floating {
            self.floating.move_window(id, x, y, animate);
        } else {
            // The target tile is in the carousel or a fixed strip — set its
            // stored floating position so a later float remembers it. Search
            // the owning layer; the carousel's `tiles_mut().find().unwrap()`
            // would panic on a strip window.
            let tile = match id {
                Some(id) => self
                    .scrolling
                    .tiles_mut()
                    .find(|tile| tile.window().id() == id),
                None => self.scrolling.active_tile_mut(),
            };
            let Some(tile) = tile else {
                return;
            };

            let pos = self.floating.stored_or_default_tile_pos(tile);

            // If there's no stored floating position, we can only set both components at once, not
            // adjust.
            let pos = pos.or_else(|| {
                (matches!(
                    x,
                    PositionChange::SetFixed(_) | PositionChange::SetProportion(_)
                ) && matches!(
                    y,
                    PositionChange::SetFixed(_) | PositionChange::SetProportion(_)
                ))
                .then_some(Point::default())
            });

            let Some(mut pos) = pos else {
                return;
            };

            let working_area = self.floating.working_area();
            let available_width = working_area.size.w;
            let available_height = working_area.size.h;
            let working_area_loc = working_area.loc;

            const MAX_F: f64 = 10000.;

            match x {
                PositionChange::SetFixed(x) => pos.x = x + working_area_loc.x,
                PositionChange::SetProportion(prop) => {
                    let prop = (prop / 100.).clamp(0., MAX_F);
                    pos.x = available_width * prop + working_area_loc.x;
                }
                PositionChange::AdjustFixed(x) => pos.x += x,
                PositionChange::AdjustProportion(prop) => {
                    let current_prop = (pos.x - working_area_loc.x) / available_width.max(1.);
                    let prop = (current_prop + prop / 100.).clamp(0., MAX_F);
                    pos.x = available_width * prop + working_area_loc.x;
                }
            }
            match y {
                PositionChange::SetFixed(y) => pos.y = y + working_area_loc.y,
                PositionChange::SetProportion(prop) => {
                    let prop = (prop / 100.).clamp(0., MAX_F);
                    pos.y = available_height * prop + working_area_loc.y;
                }
                PositionChange::AdjustFixed(y) => pos.y += y,
                PositionChange::AdjustProportion(prop) => {
                    let current_prop = (pos.y - working_area_loc.y) / available_height.max(1.);
                    let prop = (current_prop + prop / 100.).clamp(0., MAX_F);
                    pos.y = available_height * prop + working_area_loc.y;
                }
            }

            let pos = self.floating.logical_to_size_frac(pos);
            tile.floating_pos = Some(pos);
        }
    }

    pub fn has_windows(&self) -> bool {
        self.windows().next().is_some()
    }

    pub fn has_window(&self, window: &W::Id) -> bool {
        self.windows().any(|win| win.id() == window)
    }

    pub fn find_wl_surface(&self, wl_surface: &WlSurface) -> Option<&W> {
        self.windows().find(|win| win.is_wl_surface(wl_surface))
    }

    pub fn find_wl_surface_mut(&mut self, wl_surface: &WlSurface) -> Option<&mut W> {
        self.windows_mut().find(|win| win.is_wl_surface(wl_surface))
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> {
        let scrolling = self.scrolling.tiles_with_render_positions();

        let floating = self.floating.tiles_with_render_positions();
        let visible = self.is_floating_visible();
        let floating = floating.map(move |(tile, pos)| (tile, pos, visible));

        floating.chain(scrolling)
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let scrolling = self.scrolling.tiles_with_render_positions_mut(round);
        let floating = self.floating.tiles_with_render_positions_mut(round);
        floating.chain(scrolling)
    }

    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> {
        let scrolling = self.scrolling.tiles_with_ipc_layouts();
        let floating = self.floating.tiles_with_ipc_layouts();
        floating.chain(scrolling)
    }

    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        if self.floating_is_active.get() {
            self.floating.active_window_visual_rectangle()
        } else {
            self.scrolling.active_window_visual_rectangle()
        }
    }

    pub fn popup_target_rect(&self, window: &W::Id) -> Option<Rectangle<f64, Logical>> {
        if self.floating.has_window(window) {
            self.floating.popup_target_rect(window)
        } else {
            self.scrolling.popup_target_rect(window)
        }
    }

    /// Renders the carousel beneath the fixed-side panels.
    ///
    /// When a populated panel's inner edge actually has carousel content next to
    /// it, that content is faded to transparent over [`CAROUSEL_EDGE_FADE_WIDTH`]
    /// logical pixels so it dissolves into the wallpaper just before sliding
    /// behind the panel, rather than ending in a hard edge or being darkened by
    /// a shadow. The fade is a real per-pixel alpha gradient: the carousel
    /// content within the thin band is rendered to an offscreen texture and
    /// drawn through the `edge_fade` shader, while the rest of the carousel is
    /// drawn directly (cropped to exclude the band) to keep the common, perf-
    /// sensitive scrolling path cheap. With no populated panel, or with no
    /// content reaching a panel edge this frame, this is a plain direct render.
    pub fn render_scrolling<R: NaruRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        let focus_ring = focus_ring && !self.floating_is_active();

        let left_panel = self.fixed_insets.0 > 0.;
        let right_panel = self.fixed_insets.1 > 0.;

        // No populated panel → no edge to fade against; render directly. Same
        // for `disable-carousel`: the workspace doesn't scroll, so no content
        // ever slides behind a panel and there's nothing to dissolve.
        if (!left_panel && !right_panel) || self.options.layout.disable_carousel {
            self.scrolling
                .render(ctx, xray_pos, focus_ring, &mut |elem| push(elem.into()));
            return;
        }

        let scale = Scale::from(self.scale.fractional_scale());

        // The fade needs the edge_fade shader; if it failed to compile, fall
        // back to a plain direct render.
        let program = {
            let ctx = ctx.as_gles();
            EdgeFadeOffscreenRenderElement::shader(ctx.renderer)
        };
        let Some(program) = program else {
            self.scrolling
                .render(ctx, xray_pos, focus_ring, &mut |elem| push(elem.into()));
            return;
        };

        let wa = self.working_area;
        let fade_w = CAROUSEL_EDGE_FADE_WIDTH;
        // Panel inner edges in logical screen-space x — the carousel slides
        // behind the panels past these.
        let left_edge = wa.loc.x + self.fixed_insets.0;
        let right_edge = wa.loc.x + wa.size.w - self.fixed_insets.1;

        // Thin fade bands just inside each panel edge. They span the full output
        // height so this horizontal-only crop never clips content vertically.
        let band = |x: f64| -> Rectangle<i32, Physical> {
            Rectangle::new(Point::from((x, 0.)), Size::from((fade_w, self.view_size.h)))
                .to_physical_precise_round(scale)
        };
        let left_band = band(left_edge);
        let right_band = band(right_edge - fade_w);

        // Collect the carousel elements once so we can both test band overlap
        // and reuse them for the cropped middle.
        let mut elems: Vec<ScrollingSpaceRenderElement<R>> = Vec::new();
        self.scrolling
            .render(ctx.r(), xray_pos, focus_ring, &mut |elem| elems.push(elem));

        let left_active = left_panel
            && elems.iter().any(|e| {
                e.geometry(scale)
                    .intersection(left_band)
                    .is_some_and(|i| !i.is_empty())
            });
        let right_active = right_panel
            && elems.iter().any(|e| {
                e.geometry(scale)
                    .intersection(right_band)
                    .is_some_and(|i| !i.is_empty())
            });

        // Nothing actually reaches a panel edge this frame → direct render.
        if !left_active && !right_active {
            for elem in elems {
                push(elem.into());
            }
            return;
        }

        // Draw the carousel between the fade bands at full opacity. Excluding the
        // band region(s) is what lets the faded copy show through to the
        // wallpaper instead of stacking on top of opaque content.
        let mid_left = if left_active { left_edge + fade_w } else { wa.loc.x };
        let mid_right = if right_active {
            right_edge - fade_w
        } else {
            wa.loc.x + wa.size.w
        };
        let mid_rect = Rectangle::new(
            Point::from((mid_left, 0.)),
            Size::from(((mid_right - mid_left).max(0.), self.view_size.h)),
        )
        .to_physical_precise_round(scale);
        for elem in elems {
            if let Some(cropped) = CropRenderElement::from_element(elem, scale, mid_rect) {
                push(WorkspaceRenderElement::CroppedScrolling(cropped));
            }
        }

        // Render each engaged edge's band through the fade shader. `x_alpha0` is
        // the panel edge (fully transparent), `x_alpha1` the carousel-ward end
        // (fully opaque).
        if left_active {
            self.render_fade_band(
                &mut ctx,
                &self.left_fade_offscreen,
                xray_pos,
                focus_ring,
                scale,
                left_band,
                left_edge,
                left_edge + fade_w,
                &program,
                push,
            );
        }
        if right_active {
            self.render_fade_band(
                &mut ctx,
                &self.right_fade_offscreen,
                xray_pos,
                focus_ring,
                scale,
                right_band,
                right_edge,
                right_edge - fade_w,
                &program,
                push,
            );
        }
    }

    /// Renders just the carousel content inside `band` into `offscreen`, then
    /// pushes it through the `edge_fade` shader so it fades from transparent at
    /// `x_alpha0` (the panel edge) to opaque at `x_alpha1` (carousel-ward).
    #[allow(clippy::too_many_arguments)]
    fn render_fade_band<R: NaruRenderer>(
        &self,
        ctx: &mut RenderCtx<R>,
        offscreen: &OffscreenBuffer,
        xray_pos: XrayPos,
        focus_ring: bool,
        scale: Scale<f64>,
        band: Rectangle<i32, Physical>,
        x_alpha0: f64,
        x_alpha1: f64,
        program: &EdgeFadeShader,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        let mut ctx = ctx.as_gles();

        let mut elems: Vec<ScrollingSpaceRenderElement<GlesRenderer>> = Vec::new();
        self.scrolling
            .render(ctx.r(), xray_pos, focus_ring, &mut |elem| elems.push(elem));

        let cropped: Vec<CropRenderElement<ScrollingSpaceRenderElement<GlesRenderer>>> = elems
            .into_iter()
            .filter_map(|elem| CropRenderElement::from_element(elem, scale, band))
            .collect();
        if cropped.is_empty() {
            return;
        }

        match offscreen.render(ctx.renderer, scale, &cropped) {
            Ok((elem, _sync, _data)) => {
                let elem =
                    EdgeFadeOffscreenRenderElement::new(elem, program.clone(), x_alpha0, x_alpha1);
                push(WorkspaceRenderElement::EdgeFade(elem));
            }
            Err(err) => {
                warn!("error rendering carousel edge-fade band to offscreen: {err:?}");
            }
        }
    }

    pub fn render_floating<R: NaruRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        if !self.is_floating_visible() {
            return;
        }

        let view_rect = Rectangle::from_size(self.view_size);
        let floating_focus_ring = focus_ring && self.floating_is_active();
        self.floating
            .render(ctx, xray_pos, view_rect, floating_focus_ring, &mut |elem| {
                push(elem.into())
            });
    }

    pub fn render_shadow<R: NaruRenderer>(
        &self,
        renderer: &mut R,
        push: &mut dyn FnMut(ShadowRenderElement),
    ) {
        self.shadow.render(renderer, Point::from((0., 0.)), push);
    }

    pub fn render_background(&self) -> SolidColorRenderElement {
        SolidColorRenderElement::from_buffer(
            &self.background_buffer,
            Point::new(0., 0.),
            1.,
            Kind::Unspecified,
        )
    }

    pub fn render_above_top_layer(&self) -> bool {
        self.scrolling.render_above_top_layer()
    }

    pub fn is_floating_visible(&self) -> bool {
        // If the focus is on a fullscreen scrolling window, hide the floating windows.
        matches!(
            self.floating_is_active,
            FloatingActive::Yes | FloatingActive::NoButRaised
        ) || !self.render_above_top_layer()
    }

    pub fn store_unmap_snapshot_if_empty(
        &mut self,
        renderer: &mut GlesRenderer,
        xray: Option<&mut Xray>,
        xray_has_blocked_out_layers: bool,
        xray_pos: XrayPos,
        window: &W::Id,
    ) {
        let view_size = self.view_size();
        for (tile, tile_pos) in self.tiles_with_render_positions_mut(false) {
            if tile.window().id() == window {
                let view_pos = Point::from((-tile_pos.x, -tile_pos.y));
                let view_rect = Rectangle::new(view_pos, view_size);
                tile.update_render_elements(false, view_rect);
                let xray_pos = xray_pos.offset(tile_pos);
                tile.store_unmap_snapshot_if_empty(
                    renderer,
                    xray,
                    xray_has_blocked_out_layers,
                    xray_pos,
                );
                return;
            }
        }
    }

    pub fn clear_unmap_snapshot(&mut self, window: &W::Id) {
        for tile in self.tiles_mut() {
            if tile.window().id() == window {
                let _ = tile.take_unmap_snapshot();
                return;
            }
        }
    }

    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        if self.floating.has_window(window) {
            self.floating
                .start_close_animation_for_window(renderer, window, blocker);
        } else {
            self.scrolling
                .start_close_animation_for_window(renderer, window, blocker);
        }
    }

    pub fn start_close_animation_for_tile(
        &mut self,
        renderer: &mut GlesRenderer,
        snapshot: TileRenderSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
    ) {
        self.floating
            .start_close_animation_for_tile(renderer, snapshot, tile_size, tile_pos, blocker);
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        self.scrolling.start_open_animation(id) || self.floating.start_open_animation(id)
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        // Mirror the render z-order (front → back): floating on top, then the
        // carousel. The fixed-side panels are hit-tested by the monitor (which
        // owns them) before delegating here.
        if self.is_floating_visible() {
            if let Some(rv) = self
                .floating
                .tiles_with_render_positions()
                .find_map(|(tile, tile_pos)| HitType::hit_tile(tile, tile_pos, pos))
            {
                return Some(rv);
            }
        }

        self.scrolling.window_under(pos)
    }

    pub fn resize_edges_under(&self, pos: Point<f64, Logical>) -> Option<ResizeEdge> {
        self.tiles_with_render_positions()
            .find_map(|(tile, tile_pos, visible)| {
                // This logic should be consistent with window_under() in when it returns Some vs.
                // None.
                if !visible {
                    return None;
                }

                let pos_within_tile = pos - tile_pos;

                if tile.hit(pos_within_tile).is_some() {
                    let size = tile.tile_size().to_f64();

                    let mut edges = ResizeEdge::empty();
                    if pos_within_tile.x < size.w / 3. {
                        edges |= ResizeEdge::LEFT;
                    } else if 2. * size.w / 3. < pos_within_tile.x {
                        edges |= ResizeEdge::RIGHT;
                    }
                    if pos_within_tile.y < size.h / 3. {
                        edges |= ResizeEdge::TOP;
                    } else if 2. * size.h / 3. < pos_within_tile.y {
                        edges |= ResizeEdge::BOTTOM;
                    }
                    return Some(edges);
                }

                None
            })
    }

    pub fn descendants_added(&mut self, id: &W::Id) -> bool {
        self.floating.descendants_added(id)
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        // Route the configure-ack to whichever layer owns the window. The
        // carousel's `update_window` `unwrap()`-panics on a window it doesn't
        // hold, so the floating layer is tried before falling through. (Fixed
        // panels are owned by the monitor and updated there.)
        if self.floating.update_window(window, serial) {
            return;
        }
        self.scrolling.update_window(window, serial);

        // The window just committed its assigned size, so its natural column
        // width is now final. In disable-carousel mode this is the "wait for the
        // window to be assigned a size" path: a freshly opened (or self-resized)
        // window may not have had its real width when it was first placed, so
        // re-fit the row to it now — proportionally scaling every column toward
        // its preferred width to fit the available space — and re-center.
        //
        // During a mouse-driven interactive resize this has to re-fit *without*
        // cancelling the drag — the plain re-fit cancels it. Skipping the re-fit
        // entirely (what this used to do) leaves the row fitted to the width the
        // window had one commit ago: the client acks the drag's new size only now,
        // so the row is re-fitted and re-centered against a stale width and the
        // growing window walks off the screen edge.
        if self.options.layout.disable_carousel {
            if self.scrolling.is_interactive_resize_ongoing() {
                self.scrolling.fit_columns_to_parent_keep_resize(false);
            } else {
                self.refit_carousel(false);
            }
            self.scrolling.auto_fit_or_center_view_offset();
        }
    }

    /// `panel_focus` is the monitor-global fixed-side panel owning focus, if
    /// any (so the carousel's `Activated` flag is suppressed while a panel is
    /// focused). The monitor refreshes the panels themselves.
    pub fn refresh(&mut self, is_active: bool, is_focused: bool, panel_focus: Option<FixedSide>) {
        let tiling_active = is_active && !self.floating_is_active.get();
        let scrolling_focused = tiling_active && panel_focus.is_none();

        self.scrolling.refresh(scrolling_focused, is_focused);
        self.floating
            .refresh(is_active && self.floating_is_active.get(), is_focused);
    }

    pub fn scroll_amount_to_activate(&self, window: &W::Id) -> f64 {
        // Floating windows are always on screen — no carousel scrolling is
        // needed to reveal them. (Panels are handled by the monitor.)
        if self.floating.has_window(window) {
            return 0.;
        }

        self.scrolling.scroll_amount_to_activate(window)
    }

    pub fn is_urgent(&self) -> bool {
        self.windows().any(|win| win.is_urgent())
    }

    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        if self.floating.activate_window(window) {
            self.floating_is_active = FloatingActive::Yes;
            true
        } else if self.scrolling.activate_window(window) {
            self.floating_is_active = FloatingActive::No;
            true
        } else {
            false
        }
    }

    pub fn activate_window_without_raising(&mut self, window: &W::Id) -> bool {
        if self.floating.activate_window_without_raising(window) {
            self.floating_is_active = FloatingActive::Yes;
            true
        } else if self.scrolling.activate_window(window) {
            self.floating_is_active = match self.floating_is_active {
                FloatingActive::No => FloatingActive::No,
                FloatingActive::NoButRaised => FloatingActive::NoButRaised,
                FloatingActive::Yes => FloatingActive::NoButRaised,
            };
            true
        } else {
            false
        }
    }

    pub(super) fn scrolling_insert_position(&self, pos: Point<f64, Logical>) -> InsertPosition {
        self.scrolling.insert_position(pos)
    }

    pub(super) fn insert_hint_area(
        &self,
        position: InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        self.scrolling.insert_hint_area(position)
    }

    pub fn view_offset_gesture_begin(&mut self, is_touchpad: bool) {
        // With `disable-carousel`, the workspace can't pan at all — every
        // column already fits inside the inset working area. Swallowing the
        // gesture here (rather than at every input site) means trackpad
        // swipes / mouse-wheel deltas can't leak a stray view-offset into the
        // layout that would never settle back to zero.
        if self.options.layout.disable_carousel {
            return;
        }
        self.scrolling.view_offset_gesture_begin(is_touchpad);
    }

    pub fn view_offset_gesture_update(
        &mut self,
        delta_x: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        if self.options.layout.disable_carousel {
            return None;
        }
        self.scrolling
            .view_offset_gesture_update(delta_x, timestamp, is_touchpad)
    }

    pub fn view_offset_gesture_end(&mut self, is_touchpad: Option<bool>) -> bool {
        if self.options.layout.disable_carousel {
            return false;
        }
        self.scrolling.view_offset_gesture_end(is_touchpad)
    }

    pub fn dnd_scroll_gesture_begin(&mut self) {
        if self.options.layout.disable_carousel {
            return;
        }
        self.scrolling.dnd_scroll_gesture_begin();
    }

    pub fn dnd_scroll_gesture_scroll(&mut self, pos: Point<f64, Logical>, speed: f64) -> bool {
        if self.options.layout.disable_carousel {
            return false;
        }
        let config = &self.options.gestures.dnd_edge_view_scroll;
        let trigger_width = config.trigger_width;

        // This working area intentionally does not include extra struts from Options.
        let x = pos.x - self.working_area.loc.x;
        let width = self.working_area.size.w;

        let x = x.clamp(0., width);
        let trigger_width = trigger_width.clamp(0., width / 2.);

        let delta = if x < trigger_width {
            -(trigger_width - x)
        } else if width - x < trigger_width {
            trigger_width - (width - x)
        } else {
            0.
        };

        let delta = if trigger_width < 0.01 {
            // Sanity check for trigger-width 0 or small window sizes.
            0.
        } else {
            // Normalize to [0, 1].
            delta / trigger_width
        };
        let delta = delta * speed;

        self.scrolling.dnd_scroll_gesture_scroll(delta)
    }

    pub fn dnd_scroll_gesture_end(&mut self) {
        if self.options.layout.disable_carousel {
            return;
        }
        self.scrolling.dnd_scroll_gesture_end();
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        match self.layer_for(Some(&window)) {
            WindowLayer::Floating => self.floating.interactive_resize_begin(window, edges),
            _ => self.scrolling.interactive_resize_begin(window, edges),
        }
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        match self.layer_for(Some(window)) {
            WindowLayer::Floating => self.floating.interactive_resize_update(window, delta),
            _ => self.scrolling.interactive_resize_update(window, delta),
        }
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        if let Some(window) = window {
            match self.layer_for(Some(window)) {
                WindowLayer::Floating => self.floating.interactive_resize_end(Some(window)),
                _ => self.scrolling.interactive_resize_end(Some(window)),
            }
        } else {
            self.floating.interactive_resize_end(None);
            self.scrolling.interactive_resize_end(None);
        }
    }

    pub fn floating_is_active(&self) -> bool {
        self.floating_is_active.get()
    }

    pub fn floating_logical_to_size_frac(
        &self,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, SizeFrac> {
        self.floating.logical_to_size_frac(logical_pos)
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub fn layout_config(&self) -> Option<&naru_config::LayoutPart> {
        self.layout_config.as_ref()
    }

    #[cfg(test)]
    pub fn scrolling(&self) -> &ScrollingSpace<W> {
        &self.scrolling
    }

    /// Mutable access to the currently focused scrolling-layer tile, if any. Floating tiles are
    /// not yet considered for stacking ops.
    pub fn active_scrolling_tile_mut(&mut self) -> Option<&mut Tile<W>> {
        self.scrolling.active_tile_mut()
    }

    /// Number of tiles in the active scrolling-layer column, or None if there's no active
    /// column. Used by stacking-move routing to distinguish "tile inside a multi-tile column"
    /// from "tile that IS its whole column".
    pub fn active_scrolling_column_tile_count(&self) -> Option<usize> {
        self.scrolling.active_column_tile_count()
    }

    #[cfg(test)]
    pub fn floating(&self) -> &FloatingSpace<W> {
        &self.floating
    }

    #[cfg(test)]
    pub fn verify_invariants(&self, move_win_id: Option<&W::Id>) {
        use approx::assert_abs_diff_eq;

        let scale = self.scale.fractional_scale();
        assert!(scale > 0.);
        assert!(scale.is_finite());

        let options = Options::clone(&self.base_options)
            .with_merged_layout(self.layout_config.as_ref())
            .adjusted_for_scale(scale);
        assert_eq!(
            &*self.options, &options,
            "options must be base options adjusted for scale"
        );

        assert!(self.view_size.w > 0.);
        assert!(self.view_size.h > 0.);

        assert_eq!(self.background_buffer.size(), self.view_size);
        assert_eq!(
            self.background_buffer.color().components(),
            options.layout.background_color.to_array_unpremul(),
        );

        assert_eq!(self.view_size, self.scrolling.view_size());
        // `scrolling.parent_area()` is no longer a static mirror of
        // `working_area`: `sync_carousel_parent_area` (run in
        // `update_render_elements`) insets it by any populated fixed-side
        // panel and re-syncs each frame, so its value reflects whichever
        // panel-width snapshot was last applied — not necessarily the
        // current `working_area` or `carousel_parent_area()`. Asserting
        // any specific shape here would race with strip mutations.
        assert_eq!(&self.clock, self.scrolling.clock());
        assert!(Rc::ptr_eq(&self.options, self.scrolling.options()));
        self.scrolling.verify_invariants();

        assert_eq!(self.view_size, self.floating.view_size());
        assert_eq!(self.working_area, self.floating.working_area());
        assert_eq!(&self.clock, self.floating.clock());
        assert!(Rc::ptr_eq(&self.options, self.floating.options()));
        self.floating.verify_invariants();

        if self.floating.is_empty() {
            assert!(
                !self.floating_is_active.get(),
                "when floating is empty it must never be active"
            );
        }
        // Note: the old "scrolling empty ⇒ floating active" invariant no longer
        // holds at the workspace level — focus can legitimately rest in a
        // monitor-global fixed-side panel (which lives outside the workspace
        // now), leaving the carousel empty without floating taking over.

        for (tile, tile_pos, visible) in self.tiles_with_render_positions() {
            if Some(tile.window().id()) != move_win_id {
                assert_eq!(tile.interactive_move_offset, Point::from((0., 0.)));
            }

            let rounded_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

            // Tile positions must be rounded to physical pixels.
            assert_abs_diff_eq!(tile_pos.x, rounded_pos.x, epsilon = 1e-5);
            assert_abs_diff_eq!(tile_pos.y, rounded_pos.y, epsilon = 1e-5);

            if let Some(alpha) = &tile.alpha_animation {
                let anim = &alpha.anim;
                if visible {
                    assert_eq!(anim.to(), 1., "visible tiles can animate alpha only to 1");
                }

                assert!(
                    !alpha.hold_after_done,
                    "tiles in the layout cannot have held alpha animation"
                );
            }
        }
    }
}

/// Direction for cross-layer positional focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// Scores `to` as a focus target reached from `from` by moving in `dir` (lower
/// is better). Returns `None` when `to` is not in that direction — i.e. its
/// center isn't past `from`'s center along the axis. The score is the
/// center-to-center distance along `dir` plus a weighted perpendicular offset,
/// so a window roughly in line is preferred over one far off-axis.
fn directional_score(
    from: Rectangle<f64, Logical>,
    to: Rectangle<f64, Logical>,
    dir: FocusDir,
) -> Option<f64> {
    let fcx = from.loc.x + from.size.w / 2.0;
    let fcy = from.loc.y + from.size.h / 2.0;
    let tcx = to.loc.x + to.size.w / 2.0;
    let tcy = to.loc.y + to.size.h / 2.0;
    let (primary, perp) = match dir {
        FocusDir::Left => (fcx - tcx, (tcy - fcy).abs()),
        FocusDir::Right => (tcx - fcx, (tcy - fcy).abs()),
        FocusDir::Up => (fcy - tcy, (tcx - fcx).abs()),
        FocusDir::Down => (tcy - fcy, (tcx - fcx).abs()),
    };
    if primary <= 0.0 {
        return None;
    }
    Some(primary + perp * 1.5)
}

pub(super) fn compute_working_area(output: &Output) -> Rectangle<f64, Logical> {
    layer_map_for_output(output).non_exclusive_zone().to_f64()
}

fn compute_workspace_shadow_config(
    config: naru_config::WorkspaceShadow,
    view_size: Size<f64, Logical>,
) -> naru_config::Shadow {
    // Gaps between workspaces are a multiple of the view height, so shadow settings should also be
    // normalized to the view height to prevent them from overlapping on lower resolutions.
    let norm = view_size.h / 1080.;

    let mut config = naru_config::Shadow::from(config);
    config.softness *= norm;
    config.spread *= norm;
    config.offset.x.0 *= norm;
    config.offset.y.0 *= norm;

    config
}

#[cfg(test)]
mod media_player_width_tests {
    use super::*;

    const NORMAL: PresetSize = PresetSize::Proportion(1. / 3.);
    const ULTRA: PresetSize = PresetSize::Proportion(1. / 5.);

    fn ids() -> Vec<String> {
        vec!["mpv".to_owned(), "org.videolan.VLC".to_owned()]
    }

    #[test]
    fn non_media_player_falls_through() {
        // A window not in the list yields None regardless of aspect ratio, so the
        // caller falls back to the usual default-width resolution.
        let wide = Size::from((3440., 1440.));
        assert_eq!(
            media_player_default_column_width(wide, Some("firefox"), &ids(), NORMAL, ULTRA),
            None
        );
        let hd = Size::from((1920., 1080.));
        assert_eq!(
            media_player_default_column_width(hd, Some("firefox"), &ids(), NORMAL, ULTRA),
            None
        );
        assert_eq!(
            media_player_default_column_width(hd, None, &ids(), NORMAL, ULTRA),
            None
        );
    }

    #[test]
    fn media_player_narrow_on_normal_wide_on_ultrawide() {
        // 16:9 → 1/3; 21:9 and 32:10 → 1/5.
        let hd = Size::from((1920., 1080.));
        assert_eq!(
            media_player_default_column_width(hd, Some("mpv"), &ids(), NORMAL, ULTRA),
            Some(NORMAL)
        );
        let uw = Size::from((3440., 1440.)); // ~21.5:9
        assert_eq!(
            media_player_default_column_width(uw, Some("mpv"), &ids(), NORMAL, ULTRA),
            Some(ULTRA)
        );
        let superuw = Size::from((5120., 1440.)); // 32:9
        assert_eq!(
            media_player_default_column_width(
                superuw,
                Some("org.videolan.VLC"),
                &ids(),
                NORMAL,
                ULTRA
            ),
            Some(ULTRA)
        );
    }
}
