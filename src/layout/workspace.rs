use std::cmp::max;
use std::rc::Rc;
use std::time::Duration;

use naru_config::utils::MergeWith as _;
use naru_config::{
    CenterFocusedColumn, CornerRadius, OutputName, PresetSize, Workspace as WorkspaceConfig,
};
use naru_ipc::{ColumnDisplay, PositionChange, SizeChange, WindowLayout};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::{layer_map_for_output, Window};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Serial, Size, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::SurfaceCachedState;

use super::fixed_strip::{FixedSide, FixedStrip};
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

/// Width in logical pixels of a single band in the fixed-panel edge gradient.
/// The render path stacks several bands side-by-side, each with a decreasing
/// alpha, to approximate a smooth gradient without a custom shader.
const FIXED_PANEL_SHADOW_BAND_WIDTH: f64 = 2.0;

/// Per-band alpha multipliers applied to the (otherwise opaque black) gradient
/// buffer. Bands are ordered strip-ward → carousel-ward, so the first (densest)
/// band sits adjacent to the panel's inner edge and successive bands fade out
/// into the carousel.
///
/// Because the panels now render *in front of* the carousel, this gradient
/// reads as the carousel fading out as it approaches the panel — the "fade
/// before disappearing behind the side panel" — rather than as a thin drop
/// shadow. It's deliberately wide (`len() × WIDTH` ≈ 24 px) and smoothly
/// tapered so the transition is gentle.
const FIXED_PANEL_SHADOW_BAND_ALPHAS: &[f32] = &[
    0.50, 0.43, 0.36, 0.30, 0.24, 0.19, 0.15, 0.11, 0.08, 0.05, 0.03, 0.01,
];

/// Color of the fixed-panel drop shadow before alpha multiplication.
const FIXED_PANEL_SHADOW_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// True when the view's aspect ratio is ≥ 21:9 (covers 21:9 ≈ 2.333 and 32:10 = 3.2).
fn is_ultrawide_view(view_size: Size<f64, Logical>) -> bool {
    let h = view_size.h.max(1.0);
    view_size.w / h >= 21.0 / 9.0
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

#[derive(Debug)]
pub struct Workspace<W: LayoutElement> {
    /// The scrollable-tiling layout.
    scrolling: ScrollingSpace<W>,

    /// The floating layout.
    floating: FloatingSpace<W>,

    /// Fixed panel pinned to the left edge of the working area.
    ///
    /// Windows only enter via stack-move overflow at the carousel's left edge.
    /// While empty, occupies zero width and renders nothing.
    fixed_left: FixedStrip<W>,

    /// Fixed panel pinned to the right edge of the working area.
    ///
    /// Symmetric counterpart to [`fixed_left`](Self::fixed_left).
    fixed_right: FixedStrip<W>,

    /// Whether the floating layout is active instead of the scrolling layout.
    floating_is_active: FloatingActive,

    /// Lightweight active-layer signal for fixed-side panels. Set to
    /// `Some(side)` immediately after a stack-move IN succeeds on the matching
    /// side and cleared on the matching stack-move OUT. Lets the layout's
    /// stack-move handlers route a subsequent move on the opposite-direction
    /// hotkey back to the carousel without depending on the full
    /// active-layer refactor (which would replace `FloatingActive`).
    active_fixed_side: Option<FixedSide>,

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

    /// Shared black buffer used to draw the fixed-panel edge gradient's bands.
    /// Sized one band wide and as tall as the working area
    /// (`FIXED_PANEL_SHADOW_BAND_WIDTH × working_area.size.h`) so it spans the
    /// panel's vertical extent below the bar rather than the whole output, and
    /// re-used at render time by stacking it with the alphas in
    /// [`FIXED_PANEL_SHADOW_BAND_ALPHAS`].
    fixed_panel_shadow_buffer: SolidColorBuffer,

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
    FixedLeft,
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

        let fixed_left = FixedStrip::new(
            FixedSide::Left,
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let fixed_right = FixedStrip::new(
            FixedSide::Right,
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
            fixed_left,
            fixed_right,
            floating_is_active: FloatingActive::No,
            active_fixed_side: None,
            original_output,
            scale,
            transform: output.current_transform(),
            view_size,
            working_area,
            shadow: Shadow::new(shadow_config),
            background_buffer: SolidColorBuffer::new(view_size, options.layout.background_color),
            fixed_panel_shadow_buffer: SolidColorBuffer::new(
                Size::from((FIXED_PANEL_SHADOW_BAND_WIDTH, working_area.size.h)),
                FIXED_PANEL_SHADOW_COLOR,
            ),
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

        let fixed_left = FixedStrip::new(
            FixedSide::Left,
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let fixed_right = FixedStrip::new(
            FixedSide::Right,
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
            fixed_left,
            fixed_right,
            floating_is_active: FloatingActive::No,
            active_fixed_side: None,
            output: None,
            scale,
            transform: Transform::Normal,
            original_output,
            view_size,
            working_area,
            shadow: Shadow::new(shadow_config),
            background_buffer: SolidColorBuffer::new(view_size, options.layout.background_color),
            fixed_panel_shadow_buffer: SolidColorBuffer::new(
                Size::from((FIXED_PANEL_SHADOW_BAND_WIDTH, working_area.size.h)),
                FIXED_PANEL_SHADOW_COLOR,
            ),
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
        self.fixed_left.advance_animations();
        self.fixed_right.advance_animations();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.scrolling.are_animations_ongoing()
            || self.floating.are_animations_ongoing()
            || self.fixed_left.are_animations_ongoing()
            || self.fixed_right.are_animations_ongoing()
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.scrolling.are_transitions_ongoing() || self.floating.are_transitions_ongoing()
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        self.scrolling
            .update_render_elements(is_active && !self.floating_is_active.get());

        let view_rect = Rectangle::from_size(self.view_size);
        self.floating
            .update_render_elements(is_active && self.floating_is_active.get(), view_rect);

        // Fixed-side panels render passively whenever they contain windows
        // (focus is independent of which layer hosts those windows). Phase 4
        // of the fixed-panels work refines the active/inactive state once a
        // proper "panel is focused" notion exists in Workspace.
        self.fixed_left
            .update_render_elements(is_active && !self.floating_is_active.get());
        self.fixed_right
            .update_render_elements(is_active && !self.floating_is_active.get());

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
            self.working_area,
            self.scale.fractional_scale(),
            options.clone(),
        );

        self.floating.update_config(
            self.view_size,
            self.working_area,
            self.scale.fractional_scale(),
            options.clone(),
        );

        self.fixed_left.update_config(
            self.view_size,
            self.working_area,
            self.scale.fractional_scale(),
            options.clone(),
        );

        self.fixed_right.update_config(
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

    pub fn windows(&self) -> impl Iterator<Item = &W> + '_ {
        self.tiles().map(Tile::window)
    }

    pub fn windows_mut(&mut self) -> impl Iterator<Item = &mut W> + '_ {
        self.tiles_mut().map(Tile::window_mut)
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        let scrolling = self.scrolling.tiles();
        let floating = self.floating.tiles();
        let fixed_left = self.fixed_left.tiles();
        let fixed_right = self.fixed_right.tiles();
        scrolling.chain(floating).chain(fixed_left).chain(fixed_right)
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        let scrolling = self.scrolling.tiles_mut();
        let floating = self.floating.tiles_mut();
        let fixed_left = self.fixed_left.tiles_mut();
        let fixed_right = self.fixed_right.tiles_mut();
        scrolling.chain(floating).chain(fixed_left).chain(fixed_right)
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
        } else if self.fixed_left.has_window(id) {
            Some(WindowLayer::FixedLeft)
        } else if self.fixed_right.has_window(id) {
            Some(WindowLayer::FixedRight)
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
                } else if self.fixed_left.has_window(id) {
                    WindowLayer::FixedLeft
                } else if self.fixed_right.has_window(id) {
                    WindowLayer::FixedRight
                } else {
                    WindowLayer::Scrolling
                }
            }
            None => {
                if self.floating_is_active.get() {
                    WindowLayer::Floating
                } else {
                    match self.active_fixed_side {
                        Some(FixedSide::Left) => WindowLayer::FixedLeft,
                        Some(FixedSide::Right) => WindowLayer::FixedRight,
                        None => WindowLayer::Scrolling,
                    }
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
        match self.active_fixed_side {
            Some(FixedSide::Left) => self
                .fixed_left
                .active_window()
                .or_else(|| self.scrolling.active_window()),
            Some(FixedSide::Right) => self
                .fixed_right
                .active_window()
                .or_else(|| self.scrolling.active_window()),
            None => self.scrolling.active_window(),
        }
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        if self.floating_is_active.get() {
            return self.floating.active_window_mut();
        }
        match self.active_fixed_side {
            Some(FixedSide::Left) => {
                if self.fixed_left.is_empty() {
                    self.scrolling.active_window_mut()
                } else {
                    self.fixed_left.active_window_mut()
                }
            }
            Some(FixedSide::Right) => {
                if self.fixed_right.is_empty() {
                    self.scrolling.active_window_mut()
                } else {
                    self.fixed_right.active_window_mut()
                }
            }
            None => self.scrolling.active_window_mut(),
        }
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
                working_area,
                scale.fractional_scale(),
                self.options.clone(),
            );
            self.floating.update_config(
                size,
                working_area,
                scale.fractional_scale(),
                self.options.clone(),
            );
            // The fixed-side panels track the working area too. Skipping them
            // here is what left the side panels overlapping the waybar: when a
            // layer-shell surface (the bar) maps its exclusive zone *after* the
            // workspace was created, `working_area.loc.y` grows, but only the
            // carousel and floating layers were re-pinned — the strips kept
            // their stale full-height area and rendered under the bar.
            self.fixed_left.update_config(
                size,
                working_area,
                scale.fractional_scale(),
                self.options.clone(),
            );
            self.fixed_right.update_config(
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
        self.fixed_panel_shadow_buffer
            .resize(Size::from((
                FIXED_PANEL_SHADOW_BAND_WIDTH,
                working_area.size.h,
            )));

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
                    self.floating.add_tile(tile, activate);

                    if activate || self.scrolling.is_empty() {
                        self.floating_is_active = FloatingActive::Yes;
                    }
                } else {
                    self.scrolling
                        .add_tile(None, tile, activate, width, is_full_width, None);

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
                let activate = activate.map_smart(|| self.active_window().unwrap().id() == next_to);

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

                        self.floating.add_tile(tile, activate);
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
                } else if self.fixed_left.has_window(next_to) {
                    // `next_to` lives in a fixed strip — insert there so the
                    // carousel's `add_tile_right_of` doesn't `unwrap()`-panic
                    // on a window it doesn't own.
                    self.fixed_left
                        .add_tile_right_of(next_to, tile, activate, width, is_full_width);
                    if activate {
                        self.floating_is_active = FloatingActive::No;
                        self.active_fixed_side = Some(FixedSide::Left);
                    }
                } else if self.fixed_right.has_window(next_to) {
                    self.fixed_right
                        .add_tile_right_of(next_to, tile, activate, width, is_full_width);
                    if activate {
                        self.floating_is_active = FloatingActive::No;
                        self.active_fixed_side = Some(FixedSide::Right);
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

    /// Removes a tiled (carousel or fixed-strip) window's tile, routing the
    /// removal to whichever layer actually owns it. The carousel's
    /// `remove_tile` `unwrap()`-panics on a window it doesn't hold, so the
    /// fixed strips must be checked first. Drops the strip-active signal when
    /// a strip empties. Does NOT touch the floating layer, run output-leave,
    /// or update focus — callers layer that on as needed.
    fn remove_tiled_tile(&mut self, id: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        if self.fixed_left.has_window(id) {
            let removed = self.fixed_left.remove_tile(id, transaction);
            if self.fixed_left.is_empty() && self.active_fixed_side == Some(FixedSide::Left) {
                self.active_fixed_side = None;
            }
            removed
        } else if self.fixed_right.has_window(id) {
            let removed = self.fixed_right.remove_tile(id, transaction);
            if self.fixed_right.is_empty() && self.active_fixed_side == Some(FixedSide::Right) {
                self.active_fixed_side = None;
            }
            removed
        } else {
            self.scrolling.remove_tile(id, transaction)
        }
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

        removed
    }

    pub fn remove_active_tile(&mut self, transaction: Transaction) -> Option<RemovedTile<W>> {
        let from_floating = self.floating_is_active.get();
        let removed = match self.active_fixed_side {
            // Focus is inside a fixed strip — the "active tile" lives there,
            // not in the carousel or floating layer. Drop the strip-active
            // signal once the strip empties.
            Some(FixedSide::Left) => {
                let removed = self.fixed_left.remove_active_tile(transaction)?;
                if self.fixed_left.is_empty() {
                    self.active_fixed_side = None;
                }
                removed
            }
            Some(FixedSide::Right) => {
                let removed = self.fixed_right.remove_active_tile(transaction)?;
                if self.fixed_right.is_empty() {
                    self.active_fixed_side = None;
                }
                removed
            }
            None if from_floating => self.floating.remove_active_tile()?,
            None => self.scrolling.remove_active_tile(transaction)?,
        };

        if let Some(output) = &self.output {
            removed.tile.window().output_leave(output);
        }

        self.update_focus_floating_tiling_after_removing(from_floating);

        Some(removed)
    }

    pub fn remove_active_column(&mut self) -> Option<Column<W>> {
        let from_floating = self.floating_is_active.get();
        if from_floating {
            return None;
        }

        let column = match self.active_fixed_side {
            Some(FixedSide::Left) => {
                let column = self.fixed_left.remove_active_column()?;
                if self.fixed_left.is_empty() {
                    self.active_fixed_side = None;
                }
                column
            }
            Some(FixedSide::Right) => {
                let column = self.fixed_right.remove_active_column()?;
                if self.fixed_right.is_empty() {
                    self.active_fixed_side = None;
                }
                column
            }
            None => self.scrolling.remove_active_column()?,
        };

        if let Some(output) = &self.output {
            for (tile, _) in column.tiles() {
                tile.window().output_leave(output);
            }
        }

        self.update_focus_floating_tiling_after_removing(from_floating);

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
            None => self.options.layout.default_column_width.or_else(|| {
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

    fn focus_left_in_layer(&mut self) -> bool {
        if self.floating_is_active.get() {
            return self.floating.focus_left();
        }
        match self.active_fixed_side {
            Some(FixedSide::Left) => {
                // Move further left inside the left strip. At the outer edge
                // (TODO Phase 6 workspace cross) stop.
                self.fixed_left.focus_left()
            }
            Some(FixedSide::Right) => {
                // Move left inside the right strip. At its inner edge (the
                // carousel-facing leftmost column) fall back into the
                // carousel's rightmost column — but only if the carousel
                // actually has a column. With an empty carousel, hopping out
                // would clear `active_fixed_side` and leave focus nowhere.
                if self.fixed_right.focus_left() {
                    true
                } else if !self.scrolling.is_empty() {
                    self.scrolling.focus_column_last();
                    self.active_fixed_side = None;
                    true
                } else {
                    false
                }
            }
            None => {
                // Focus is in the carousel. Try to move left there; if we're
                // already at the leftmost carousel column, hop into the left
                // strip's innermost (carousel-facing) column.
                if self.scrolling.focus_left() {
                    true
                } else if self.fixed_left.focus_innermost() {
                    self.active_fixed_side = Some(FixedSide::Left);
                    true
                } else {
                    false
                }
            }
        }
    }

    fn focus_right_in_layer(&mut self) -> bool {
        if self.floating_is_active.get() {
            return self.floating.focus_right();
        }
        match self.active_fixed_side {
            Some(FixedSide::Left) => {
                // Move right inside the left strip; at its inner edge
                // (rightmost column) fall back into the carousel's leftmost —
                // but only if the carousel has a column. With an empty
                // carousel, hopping out would clear `active_fixed_side` and
                // leave focus nowhere.
                if self.fixed_left.focus_right() {
                    true
                } else if !self.scrolling.is_empty() {
                    self.scrolling.focus_column_first();
                    self.active_fixed_side = None;
                    true
                } else {
                    false
                }
            }
            Some(FixedSide::Right) => {
                // Move right inside the right strip. At the outer edge stop.
                self.fixed_right.focus_right()
            }
            None => {
                if self.scrolling.focus_right() {
                    true
                } else if self.fixed_right.focus_innermost() {
                    self.active_fixed_side = Some(FixedSide::Right);
                    true
                } else {
                    false
                }
            }
        }
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

    fn focus_down_in_layer(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_down()
        } else {
            match self.active_fixed_side {
                Some(FixedSide::Left) => self.fixed_left.focus_down(),
                Some(FixedSide::Right) => self.fixed_right.focus_down(),
                None => self.scrolling.focus_down(),
            }
        }
    }

    fn focus_up_in_layer(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_up()
        } else {
            match self.active_fixed_side {
                Some(FixedSide::Left) => self.fixed_left.focus_up(),
                Some(FixedSide::Right) => self.fixed_right.focus_up(),
                None => self.scrolling.focus_up(),
            }
        }
    }

    pub fn focus_down_or_left(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_down();
        } else {
            // Within a strip, up/down stays inside the strip's active column;
            // the carousel-only horizontal fallback does not apply.
            match self.active_fixed_side {
                Some(FixedSide::Left) => {
                    self.fixed_left.focus_down();
                }
                Some(FixedSide::Right) => {
                    self.fixed_right.focus_down();
                }
                None => self.scrolling.focus_down_or_left(),
            }
        }
    }

    pub fn focus_down_or_right(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_down();
        } else {
            match self.active_fixed_side {
                Some(FixedSide::Left) => {
                    self.fixed_left.focus_down();
                }
                Some(FixedSide::Right) => {
                    self.fixed_right.focus_down();
                }
                None => self.scrolling.focus_down_or_right(),
            }
        }
    }

    pub fn focus_up_or_left(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_up();
        } else {
            match self.active_fixed_side {
                Some(FixedSide::Left) => {
                    self.fixed_left.focus_up();
                }
                Some(FixedSide::Right) => {
                    self.fixed_right.focus_up();
                }
                None => self.scrolling.focus_up_or_left(),
            }
        }
    }

    pub fn focus_up_or_right(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_up();
        } else {
            match self.active_fixed_side {
                Some(FixedSide::Left) => {
                    self.fixed_left.focus_up();
                }
                Some(FixedSide::Right) => {
                    self.fixed_right.focus_up();
                }
                None => self.scrolling.focus_up_or_right(),
            }
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

    /// Extracts the active carousel column and inserts it into `fixed_left`
    /// at the strip's inner (carousel-facing) edge. Returns false if the
    /// floating layer is active, the carousel is empty, or the active column
    /// is not the leftmost — the caller should fall back to its existing
    /// edge handling in those cases.
    ///
    /// Used to implement stack-move overflow from the carousel's left edge
    /// into the left fixed panel. Sets `active_fixed_side = Some(Left)` so
    /// the next stack-move on the opposite hotkey routes back out.
    pub fn move_active_carousel_column_into_left_strip(&mut self) -> bool {
        if self.floating_is_active.get() {
            return false;
        }
        if self.scrolling.is_empty() || self.scrolling.active_column_index() != 0 {
            return false;
        }
        let Some(column) = self.scrolling.remove_active_column() else {
            return false;
        };
        self.fixed_left.add_column_at_inner_edge(column);
        self.active_fixed_side = Some(FixedSide::Left);
        true
    }

    /// Reverses [`move_active_carousel_column_into_left_strip`]: extracts the
    /// strip's innermost (carousel-facing) column and inserts it back into
    /// the carousel at the new leftmost slot. Only fires when the focused
    /// column inside the strip IS the innermost — otherwise the OUT semantic
    /// is wrong (the caller wants a within-strip move).
    pub fn move_active_strip_column_back_to_carousel_left(&mut self) -> bool {
        if self.active_fixed_side != Some(FixedSide::Left) {
            return false;
        }
        if !self.fixed_left.focused_column_is_at_inner_edge() {
            return false;
        }
        let Some(column) = self.fixed_left.remove_innermost_column() else {
            self.active_fixed_side = None;
            return false;
        };
        // Insert at carousel index 0 so the returned column becomes the new
        // leftmost. `activate=true` makes the carousel focus the inserted
        // column, mirroring naru's existing "focus follows the moved window"
        // behaviour.
        self.scrolling.add_column(Some(0), column, true, None);
        self.active_fixed_side = None;
        true
    }

    /// Mirror of [`move_active_carousel_column_into_left_strip`] for the
    /// right edge: extracts the active carousel column and inserts it into
    /// `fixed_right` at the strip's inner (carousel-facing) edge. Returns
    /// false unless the active carousel column is the rightmost.
    pub fn move_active_carousel_column_into_right_strip(&mut self) -> bool {
        if self.floating_is_active.get() {
            return false;
        }
        let last_idx = match self.scrolling.column_count().checked_sub(1) {
            Some(idx) => idx,
            None => return false,
        };
        if self.scrolling.active_column_index() != last_idx {
            return false;
        }
        let Some(column) = self.scrolling.remove_active_column() else {
            return false;
        };
        self.fixed_right.add_column_at_inner_edge(column);
        self.active_fixed_side = Some(FixedSide::Right);
        true
    }

    /// Reverse of [`move_active_carousel_column_into_right_strip`]: extracts
    /// `fixed_right`'s innermost column and re-inserts it as the carousel's
    /// new rightmost. Only fires when the focused column inside the strip IS
    /// the innermost — otherwise the caller wants a within-strip move
    /// instead.
    pub fn move_active_strip_column_back_to_carousel_right(&mut self) -> bool {
        if self.active_fixed_side != Some(FixedSide::Right) {
            return false;
        }
        if !self.fixed_right.focused_column_is_at_inner_edge() {
            return false;
        }
        let Some(column) = self.fixed_right.remove_innermost_column() else {
            self.active_fixed_side = None;
            return false;
        };
        // Append after the current carousel columns (i.e. new rightmost).
        let idx = self.scrolling.column_count();
        self.scrolling.add_column(Some(idx), column, true, None);
        self.active_fixed_side = None;
        true
    }

    /// Read-only accessor for the strip-active signal. Used by Layout's
    /// stack-move handlers to dispatch a within-strip move instead of
    /// falling through to carousel handling when focus is inside a strip.
    pub fn active_fixed_side(&self) -> Option<FixedSide> {
        self.active_fixed_side
    }

    /// Within-strip stack-move on the left fixed panel. `to_left=true` moves
    /// the active column one slot toward the strip's outer edge; `false`
    /// moves it toward the inner (carousel-facing) edge. Returns `false` if
    /// the strip's active column is at the matching edge — the caller can
    /// then decide whether to OUT to the carousel or treat as a no-op.
    /// Returns `false` (without side effects) when `fixed_left` is not the
    /// active layer at all.
    pub fn move_active_window_within_left_strip(&mut self, to_left: bool) -> bool {
        if self.active_fixed_side != Some(FixedSide::Left) {
            return false;
        }
        self.fixed_left.move_active_neighbor_as_new_row(to_left)
    }

    /// Mirror of [`move_active_window_within_left_strip`] for the right
    /// panel.
    pub fn move_active_window_within_right_strip(&mut self, to_left: bool) -> bool {
        if self.active_fixed_side != Some(FixedSide::Right) {
            return false;
        }
        self.fixed_right.move_active_neighbor_as_new_row(to_left)
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

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        match self.layer_for(window) {
            // Floating windows have no columns to consume into / expel from.
            WindowLayer::Floating => {}
            WindowLayer::FixedLeft => self.fixed_left.consume_or_expel_window_left(window),
            WindowLayer::FixedRight => self.fixed_right.consume_or_expel_window_left(window),
            WindowLayer::Scrolling => self.scrolling.consume_or_expel_window_left(window),
        }
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        match self.layer_for(window) {
            WindowLayer::Floating => {}
            WindowLayer::FixedLeft => self.fixed_left.consume_or_expel_window_right(window),
            WindowLayer::FixedRight => self.fixed_right.consume_or_expel_window_right(window),
            WindowLayer::Scrolling => self.scrolling.consume_or_expel_window_right(window),
        }
    }

    pub fn consume_into_column(&mut self) {
        match self.layer_for(None) {
            WindowLayer::Floating => {}
            WindowLayer::FixedLeft => self.fixed_left.consume_into_column(),
            WindowLayer::FixedRight => self.fixed_right.consume_into_column(),
            WindowLayer::Scrolling => self.scrolling.consume_into_column(),
        }
    }

    pub fn expel_from_column(&mut self) {
        match self.layer_for(None) {
            WindowLayer::Floating => {}
            WindowLayer::FixedLeft => self.fixed_left.expel_from_column(),
            WindowLayer::FixedRight => self.fixed_right.expel_from_column(),
            WindowLayer::Scrolling => self.scrolling.expel_from_column(),
        }
    }

    pub fn swap_window_in_direction(&mut self, direction: ScrollDirection) {
        match self.layer_for(None) {
            WindowLayer::Floating => {}
            WindowLayer::FixedLeft => self.fixed_left.swap_window_in_direction(direction),
            WindowLayer::FixedRight => self.fixed_right.swap_window_in_direction(direction),
            WindowLayer::Scrolling => self.scrolling.swap_window_in_direction(direction),
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
            WindowLayer::Scrolling => self.scrolling.center_window(id),
            // Fixed-side strips are pinned (no view scrolling), so there is
            // nothing to center — leave the strip untouched.
            WindowLayer::FixedLeft | WindowLayer::FixedRight => {}
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
        } else {
            self.scrolling.toggle_width(forwards);
            self.scrolling.auto_fit_or_center_view_offset();
        }
    }

    pub fn toggle_full_width(&mut self) {
        if self.floating_is_active.get() {
            // Leave this unimplemented for now. For good UX, this probably needs moving the tile
            // to be against the left edge of the working area while it is full-width.
            return;
        }
        self.scrolling.toggle_full_width();
        self.scrolling.auto_fit_or_center_view_offset();
    }

    pub fn set_column_width(&mut self, change: SizeChange) {
        if self.floating_is_active.get() {
            self.floating.set_window_width(None, change, true);
        } else {
            self.scrolling.set_window_width(None, change);
            self.scrolling.auto_fit_or_center_view_offset();
        }
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        match self.layer_for(window) {
            WindowLayer::Floating => self.floating.set_window_width(window, change, true),
            WindowLayer::FixedLeft => self.fixed_left.set_window_width(window, change),
            WindowLayer::FixedRight => self.fixed_right.set_window_width(window, change),
            WindowLayer::Scrolling => {
                self.scrolling.set_window_width(window, change);
                self.scrolling.auto_fit_or_center_view_offset();
            }
        }
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        match self.layer_for(window) {
            WindowLayer::Floating => self.floating.set_window_height(window, change, true),
            WindowLayer::FixedLeft => self.fixed_left.set_window_height(window, change),
            WindowLayer::FixedRight => self.fixed_right.set_window_height(window, change),
            WindowLayer::Scrolling => {
                self.scrolling.set_window_height(window, change);
                self.scrolling.auto_fit_or_center_view_offset();
            }
        }
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        match self.layer_for(window) {
            WindowLayer::Floating => {}
            WindowLayer::FixedLeft => self.fixed_left.reset_window_height(window),
            WindowLayer::FixedRight => self.fixed_right.reset_window_height(window),
            WindowLayer::Scrolling => {
                self.scrolling.reset_window_height(window);
                self.scrolling.auto_fit_or_center_view_offset();
            }
        }
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        match self.layer_for(window) {
            WindowLayer::Floating => self.floating.toggle_window_width(window, forwards),
            WindowLayer::FixedLeft => self.fixed_left.toggle_window_width(window, forwards),
            WindowLayer::FixedRight => self.fixed_right.toggle_window_width(window, forwards),
            WindowLayer::Scrolling => {
                self.scrolling.toggle_window_width(window, forwards);
                self.scrolling.auto_fit_or_center_view_offset();
            }
        }
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        match self.layer_for(window) {
            WindowLayer::Floating => self.floating.toggle_window_height(window, forwards),
            WindowLayer::FixedLeft => self.fixed_left.toggle_window_height(window, forwards),
            WindowLayer::FixedRight => self.fixed_right.toggle_window_height(window, forwards),
            WindowLayer::Scrolling => {
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
        self.scrolling.auto_fit_or_center_view_offset();
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) {
        // Fixed-strip windows are handled up front: the carousel-specific
        // `restore_to_floating` bookkeeping below doesn't apply to them, and
        // routing keeps the carousel's by-id lookups from `unwrap()`-panicking
        // on a window they don't own.
        if self.fixed_left.has_window(window) {
            self.fixed_left.set_fullscreen(window, is_fullscreen);
            return;
        }
        if self.fixed_right.has_window(window) {
            self.fixed_right.set_fullscreen(window, is_fullscreen);
            return;
        }

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
        // See `set_fullscreen`: strip windows are routed before the
        // carousel-specific path.
        if self.fixed_left.has_window(window) {
            self.fixed_left.set_maximized(window, maximize);
            return;
        }
        if self.fixed_right.has_window(window) {
            self.fixed_right.set_maximized(window, maximize);
            return;
        }

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
            .chain(self.fixed_left.columns())
            .chain(self.fixed_right.columns())
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

            self.floating.add_tile(removed.tile, target_is_active);
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
            let tile = match layer {
                WindowLayer::Scrolling => match id {
                    Some(id) => self
                        .scrolling
                        .tiles_mut()
                        .find(|tile| tile.window().id() == id),
                    None => self.scrolling.active_tile_mut(),
                },
                WindowLayer::FixedLeft => match id {
                    Some(id) => self
                        .fixed_left
                        .tiles_mut()
                        .find(|tile| tile.window().id() == id),
                    None => self.fixed_left.active_tile_mut(),
                },
                WindowLayer::FixedRight => match id {
                    Some(id) => self
                        .fixed_right
                        .tiles_mut()
                        .find(|tile| tile.window().id() == id),
                    None => self.fixed_right.active_tile_mut(),
                },
                WindowLayer::Floating => unreachable!(),
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

        let fixed_left = self.fixed_left.tiles_with_render_positions();
        let fixed_right = self.fixed_right.tiles_with_render_positions();

        floating
            .chain(scrolling)
            .chain(fixed_left)
            .chain(fixed_right)
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let scrolling = self.scrolling.tiles_with_render_positions_mut(round);
        let floating = self.floating.tiles_with_render_positions_mut(round);
        let fixed_left = self.fixed_left.tiles_with_render_positions_mut(round);
        let fixed_right = self.fixed_right.tiles_with_render_positions_mut(round);
        floating
            .chain(scrolling)
            .chain(fixed_left)
            .chain(fixed_right)
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

    pub fn render_scrolling<R: NaruRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        let scrolling_focus_ring = focus_ring && !self.floating_is_active();
        self.scrolling
            .render(ctx, xray_pos, scrolling_focus_ring, &mut |elem| {
                push(elem.into())
            });
    }

    /// Emits a faux-gradient black edge fade on the carousel-facing inner edge
    /// of each populated fixed-side panel. The gradient is built by stacking
    /// [`FIXED_PANEL_SHADOW_BAND_ALPHAS.len()`] one-band-wide rectangles
    /// side-by-side, each with a decreasing alpha, so the carousel reads as
    /// fading out as it approaches the panel before sliding behind it. Pushed
    /// *after* the per-strip render methods and *before*
    /// [`render_scrolling`](Self::render_scrolling) (push order is
    /// front-to-back), so it sits on top of the carousel but beneath the
    /// panel's own windows. Empty strips contribute nothing.
    pub fn render_fixed_strip_shadows<R: NaruRenderer>(
        &self,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        let working_area = self.working_area;
        let band_w = FIXED_PANEL_SHADOW_BAND_WIDTH;

        if !self.fixed_left.is_empty() {
            // Bands stack rightward from the strip's inner edge into the
            // carousel; band 0 (densest alpha) is adjacent to the strip.
            let base_x = working_area.loc.x + self.fixed_left.width();
            for (i, &alpha) in FIXED_PANEL_SHADOW_BAND_ALPHAS.iter().enumerate() {
                let x = base_x + (i as f64) * band_w;
                let elem = SolidColorRenderElement::from_buffer(
                    &self.fixed_panel_shadow_buffer,
                    Point::from((x, working_area.loc.y)),
                    alpha,
                    Kind::Unspecified,
                );
                push(elem.into());
            }
        }

        if !self.fixed_right.is_empty() {
            // Bands stack leftward from the strip's inner edge into the
            // carousel; band 0 (densest alpha) is adjacent to the strip.
            let strip_inner_edge =
                working_area.loc.x + working_area.size.w - self.fixed_right.width();
            for (i, &alpha) in FIXED_PANEL_SHADOW_BAND_ALPHAS.iter().enumerate() {
                let x = strip_inner_edge - ((i as f64) + 1.0) * band_w;
                let elem = SolidColorRenderElement::from_buffer(
                    &self.fixed_panel_shadow_buffer,
                    Point::from((x, working_area.loc.y)),
                    alpha,
                    Kind::Unspecified,
                );
                push(elem.into());
            }
        }
    }

    /// Renders the left fixed-side panel on top of the carousel. Empty strip
    /// produces no elements.
    pub fn render_fixed_left<R: NaruRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        let strip_focus_ring = focus_ring && !self.floating_is_active();
        self.fixed_left
            .render(ctx, xray_pos, strip_focus_ring, &mut |elem| {
                push(elem.into())
            });
    }

    /// Renders the right fixed-side panel on top of the carousel. Empty
    /// strip produces no elements. The right-side anchor (translating
    /// output to the workspace's right edge) is a follow-up — for now this
    /// renders at the left-anchored origin same as the left strip, so it is
    /// only visually correct once that anchor work lands.
    pub fn render_fixed_right<R: NaruRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        let strip_focus_ring = focus_ring && !self.floating_is_active();
        self.fixed_right
            .render(ctx, xray_pos, strip_focus_ring, &mut |elem| {
                push(elem.into())
            });
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
        } else if self.fixed_left.has_window(window) {
            self.fixed_left
                .start_close_animation_for_window(renderer, window, blocker);
        } else if self.fixed_right.has_window(window) {
            self.fixed_right
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
        self.scrolling.start_open_animation(id)
            || self.floating.start_open_animation(id)
            || self.fixed_left.start_open_animation(id)
            || self.fixed_right.start_open_animation(id)
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        // Mirror the render z-order (front → back): floating on top, then the
        // fixed-side panels, then the carousel behind them. Hit-testing the
        // strips before the carousel is what lets a mouse click focus a
        // sidepanel window (and matches the carousel sliding *behind* the
        // panel after the z-order change).
        if self.is_floating_visible() {
            if let Some(rv) = self
                .floating
                .tiles_with_render_positions()
                .find_map(|(tile, tile_pos)| HitType::hit_tile(tile, tile_pos, pos))
            {
                return Some(rv);
            }
        }

        if let Some(rv) = self.fixed_left.window_under(pos) {
            return Some(rv);
        }
        if let Some(rv) = self.fixed_right.window_under(pos) {
            return Some(rv);
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
        // hold, so the fixed strips must be tried before falling through.
        if self.floating.update_window(window, serial)
            || self.fixed_left.update_window(window, serial)
            || self.fixed_right.update_window(window, serial)
        {
            return;
        }
        self.scrolling.update_window(window, serial);
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        self.scrolling
            .refresh(is_active && !self.floating_is_active.get(), is_focused);
        self.floating
            .refresh(is_active && self.floating_is_active.get(), is_focused);
    }

    pub fn scroll_amount_to_activate(&self, window: &W::Id) -> f64 {
        // Floating windows and fixed-strip windows are always on screen — no
        // carousel scrolling is needed to reveal them.
        if self.floating.has_window(window)
            || self.fixed_left.has_window(window)
            || self.fixed_right.has_window(window)
        {
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
            self.active_fixed_side = None;
            true
        } else if self.fixed_left.activate_window(window) {
            self.floating_is_active = FloatingActive::No;
            self.active_fixed_side = Some(FixedSide::Left);
            true
        } else if self.fixed_right.activate_window(window) {
            self.floating_is_active = FloatingActive::No;
            self.active_fixed_side = Some(FixedSide::Right);
            true
        } else if self.scrolling.activate_window(window) {
            self.floating_is_active = FloatingActive::No;
            self.active_fixed_side = None;
            true
        } else {
            false
        }
    }

    pub fn activate_window_without_raising(&mut self, window: &W::Id) -> bool {
        if self.floating.activate_window_without_raising(window) {
            self.floating_is_active = FloatingActive::Yes;
            self.active_fixed_side = None;
            true
        } else if self.fixed_left.activate_window(window) {
            self.floating_is_active = FloatingActive::No;
            self.active_fixed_side = Some(FixedSide::Left);
            true
        } else if self.fixed_right.activate_window(window) {
            self.floating_is_active = FloatingActive::No;
            self.active_fixed_side = Some(FixedSide::Right);
            true
        } else if self.scrolling.activate_window(window) {
            self.floating_is_active = match self.floating_is_active {
                FloatingActive::No => FloatingActive::No,
                FloatingActive::NoButRaised => FloatingActive::NoButRaised,
                FloatingActive::Yes => FloatingActive::NoButRaised,
            };
            self.active_fixed_side = None;
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
        self.scrolling.view_offset_gesture_begin(is_touchpad);
    }

    pub fn view_offset_gesture_update(
        &mut self,
        delta_x: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        self.scrolling
            .view_offset_gesture_update(delta_x, timestamp, is_touchpad)
    }

    pub fn view_offset_gesture_end(&mut self, is_touchpad: Option<bool>) -> bool {
        self.scrolling.view_offset_gesture_end(is_touchpad)
    }

    pub fn dnd_scroll_gesture_begin(&mut self) {
        self.scrolling.dnd_scroll_gesture_begin();
    }

    pub fn dnd_scroll_gesture_scroll(&mut self, pos: Point<f64, Logical>, speed: f64) -> bool {
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
        self.scrolling.dnd_scroll_gesture_end();
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        match self.layer_for(Some(&window)) {
            WindowLayer::Floating => self.floating.interactive_resize_begin(window, edges),
            WindowLayer::FixedLeft => self.fixed_left.interactive_resize_begin(window, edges),
            WindowLayer::FixedRight => self.fixed_right.interactive_resize_begin(window, edges),
            WindowLayer::Scrolling => self.scrolling.interactive_resize_begin(window, edges),
        }
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        match self.layer_for(Some(window)) {
            WindowLayer::Floating => self.floating.interactive_resize_update(window, delta),
            WindowLayer::FixedLeft => self.fixed_left.interactive_resize_update(window, delta),
            WindowLayer::FixedRight => self.fixed_right.interactive_resize_update(window, delta),
            WindowLayer::Scrolling => self.scrolling.interactive_resize_update(window, delta),
        }
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        if let Some(window) = window {
            match self.layer_for(Some(window)) {
                WindowLayer::Floating => self.floating.interactive_resize_end(Some(window)),
                WindowLayer::FixedLeft => self.fixed_left.interactive_resize_end(Some(window)),
                WindowLayer::FixedRight => self.fixed_right.interactive_resize_end(Some(window)),
                WindowLayer::Scrolling => self.scrolling.interactive_resize_end(Some(window)),
            }
        } else {
            self.floating.interactive_resize_end(None);
            self.scrolling.interactive_resize_end(None);
            self.fixed_left.interactive_resize_end(None);
            self.fixed_right.interactive_resize_end(None);
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
        assert_eq!(self.working_area, self.scrolling.parent_area());
        assert_eq!(&self.clock, self.scrolling.clock());
        assert!(Rc::ptr_eq(&self.options, self.scrolling.options()));
        self.scrolling.verify_invariants();

        assert_eq!(self.view_size, self.floating.view_size());
        assert_eq!(self.working_area, self.floating.working_area());
        assert_eq!(&self.clock, self.floating.clock());
        assert!(Rc::ptr_eq(&self.options, self.floating.options()));
        self.floating.verify_invariants();

        self.fixed_left.verify_invariants();
        self.fixed_right.verify_invariants();

        // `active_fixed_side` must always point at a strip that actually has
        // a window — otherwise stack-move / focus dispatch routes keypresses
        // to an empty (or stale) strip.
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

        if self.floating.is_empty() {
            assert!(
                !self.floating_is_active.get(),
                "when floating is empty it must never be active"
            );
        } else if self.scrolling.is_empty() && self.active_fixed_side.is_none() {
            // With an empty carousel and a non-empty floating layer, focus
            // must sit in floating — unless it has moved into a fixed-side
            // strip, which is a valid resting place that the carousel/floating
            // active-layer logic predates.
            assert!(
                self.floating_is_active.get(),
                "when scrolling is empty but floating isn't, floating should be active"
            );
        }

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
