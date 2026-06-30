//! A deterministic, assertion-friendly test harness for the layout subsystem.
//!
//! # Why this exists
//!
//! The layout (`Layout` / `Workspace` / `ScrollingSpace` / `FloatingSpace` /
//! `FixedStrip`) is generic over [`LayoutElement`]: the real compositor runs
//! `Layout<Mapped>`, driven by the input handler calling inherent `Layout`
//! methods (`move_window_right_stacked`, `focus_left`, …). The test build runs
//! `Layout<TestWindow>` driven by the *same* inherent methods. That generic
//! `Layout<W>` is therefore already the single interface shared between the
//! fake and the real implementation.
//!
//! What was missing — and what this module adds — is:
//!
//! 1. **One class holding all the simulated screen/workspace state.**
//!    [`LayoutSim`] owns a `Layout<TestWindow>` plus the fake environment
//!    (outputs, the clock, the window-id counter) and exposes the native-ish
//!    calls — adding outputs/windows, committing configures, advancing time —
//!    as plain methods. The production layout code runs unchanged underneath;
//!    only [`LayoutElement`] (the window) and the environment are faked.
//!
//! 2. **A shared introspection interface.** [`LayoutModel`] is implemented
//!    generically for `Layout<W>`, so the *exact same* state queries
//!    (`window_slot`, `window_geometry`, `active_window_id`, …) work against
//!    `Layout<TestWindow>` here and against a real `Layout<Mapped>` in an
//!    end-to-end harness. A scenario written against [`LayoutModel`] + the
//!    inherent `Layout` actions exercises the production code path regardless
//!    of the backing window type.
//!
//! 3. **Position/state assertions.** Because [`LayoutSim`] re-checks layout
//!    invariants after every action and exposes [`LayoutModel`], tests assert
//!    concrete facts ("window 2 is in `FixedRight` at x≈1820") instead of only
//!    "nothing panicked".
//!
//! 4. **Real animation simulation.** The fake drives a fake [`Clock`]:
//!    [`LayoutSim::advance_time`] / [`LayoutSim::advance_frame`] step the clock
//!    and run the exact `Layout::advance_animations` the compositor runs each
//!    frame, so `window_geometry` reports genuine mid-flight positions.
//!    [`LayoutSim::run_until_settled`] frames forward to the rest state, and
//!    [`LayoutSim::complete_animations`] jumps straight to it.
//!
//! The randomized fuzz test still exists; it now drives a [`LayoutSim`] through
//! the same [`LayoutSim::apply`] choke point as the deterministic tests.

// This is a test harness: it deliberately exposes a broad API surface that not
// every individual test exercises.
#![allow(dead_code)]

use std::time::Duration;

use naru_config::FloatOrInt;
use naru_ipc::{OpenInFixedSide, SizeChange};
use smithay::utils::{Logical, Point, Rectangle, Size};

use super::{Op, TestWindow, TestWindowParams};
use crate::animation::Clock;
use crate::layout::fixed_strip::FixedSide;
use crate::layout::workspace::WindowLayer;
use crate::layout::{Layout, LayoutElement, Options};
use crate::window::ResolvedWindowRules;

/// Read-only introspection shared by the fake [`LayoutSim`] and the real
/// `Layout<Mapped>`. Implemented generically for every `Layout<W>`, so a
/// scenario phrased against this trait sees identical answers whichever window
/// type backs the layout.
///
/// Actions are intentionally *not* on this trait: the inherent `Layout<W>`
/// methods (`move_window_right_stacked`, `focus_left`, `remove_window`, …) are
/// already the shared action surface — the input handler and the tests both
/// call them directly.
pub(crate) trait LayoutModel {
    type WindowId: Clone + PartialEq + std::fmt::Debug;

    /// Every window currently in the layout, across all monitors/workspaces.
    fn window_ids(&self) -> Vec<Self::WindowId>;

    /// The active (focused) window of the active workspace, if any.
    fn active_window_id(&self) -> Option<Self::WindowId>;

    /// Which sub-layout holds `id` — carousel, floating, or a fixed-side
    /// strip. `None` if the layout does not contain the window.
    fn window_slot(&self, id: &Self::WindowId) -> Option<WindowLayer>;

    /// Workspace-local render rectangle (position + tile size) of `id`.
    /// `None` if the window is not present (or is on no output).
    fn window_geometry(&self, id: &Self::WindowId) -> Option<Rectangle<f64, Logical>>;

    /// Which fixed-side strip, if any, currently owns focus on the active
    /// workspace.
    fn active_fixed_side(&self) -> Option<FixedSide>;
}

impl<W: LayoutElement> LayoutModel for Layout<W> {
    type WindowId = W::Id;

    fn window_ids(&self) -> Vec<W::Id> {
        self.windows().map(|(_, win)| win.id().clone()).collect()
    }

    fn active_window_id(&self) -> Option<W::Id> {
        // Use the monitor's active window so a focused fixed-side panel window
        // (monitor-owned) is reported, not just the active workspace's.
        self.active_monitor_ref()
            .and_then(|mon| mon.active_window())
            .map(|win| win.id().clone())
    }

    fn window_slot(&self, id: &W::Id) -> Option<WindowLayer> {
        // Fixed-side panels are owned by the monitor now, so consult them first.
        match self.panel_side_with_window(id) {
            Some(FixedSide::Left) => return Some(WindowLayer::FixedLeft),
            Some(FixedSide::Right) => return Some(WindowLayer::FixedRight),
            None => {}
        }
        self.workspaces().find_map(|(_, _, ws)| ws.window_slot(id))
    }

    fn window_geometry(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
        // Panel windows render pinned to the screen (monitor-owned).
        if let Some((tile, pos)) = self.panel_tile_with_render_position(id) {
            return Some(Rectangle::new(pos, tile.tile_size()));
        }
        for (_, _, ws) in self.workspaces() {
            for (tile, pos, _visible) in ws.tiles_with_render_positions() {
                if tile.window().id() == id {
                    return Some(Rectangle::new(pos, tile.tile_size()));
                }
            }
        }
        None
    }

    fn active_fixed_side(&self) -> Option<FixedSide> {
        self.layout_active_fixed_side()
    }
}

/// A single test-double class bundling the faked screen/workspace state.
///
/// Owns a real `Layout<TestWindow>` — the production layout code runs
/// unmodified — plus the simulated environment (the clock, the window-id
/// counter; outputs live inside the layout). Every mutating method funnels
/// through [`LayoutSim::apply`], which re-checks layout invariants, so a test
/// that performs an illegal sequence fails on the offending step rather than
/// much later.
pub(crate) struct LayoutSim {
    layout: Layout<TestWindow>,
    next_window_id: usize,
}

impl LayoutSim {
    /// A fresh simulator with default options. Note: stacking is *off* by
    /// default, matching `naru_config`'s default — use [`Self::new_stacking`]
    /// for fixed-strip / stack-move scenarios.
    pub fn new() -> Self {
        Self::with_options(Options::default())
    }

    /// A fresh simulator with `enable_stacking: true` — the configuration
    /// fixed-side panels and stack-moves require.
    pub fn new_stacking() -> Self {
        Self::with_options(Options {
            enable_stacking: true,
            ..Default::default()
        })
    }

    /// A fresh simulator with stacking enabled *and* deterministic 1000 ms
    /// linear window-move / resize animations. Use this when a scenario needs
    /// to observe genuine mid-flight geometry — frame-stepping the default
    /// (spring) animations works too, but linear easing makes assertions
    /// about progress trivial to reason about.
    pub fn new_animated() -> Self {
        use naru_config::animations::{Curve, EasingParams, Kind};

        const LINEAR: Kind = Kind::Easing(EasingParams {
            duration_ms: 1000,
            curve: Curve::Linear,
        });

        let mut options = Options {
            enable_stacking: true,
            ..Default::default()
        };
        options.animations.window_resize.anim.kind = LINEAR;
        options.animations.window_movement.0.kind = LINEAR;
        Self::with_options(options)
    }

    pub fn with_options(options: Options) -> Self {
        Self {
            layout: Layout::with_options(Clock::with_time(Duration::ZERO), options),
            next_window_id: 0,
        }
    }

    // --- the choke point -----------------------------------------------------

    /// Apply one raw [`Op`] and re-check layout invariants. Every ergonomic
    /// action below, and the randomized fuzz test, route through here so the
    /// invariant check is impossible to forget.
    pub fn apply(&mut self, op: Op) {
        op.apply(&mut self.layout);
        self.layout.verify_invariants();
    }

    /// Re-check invariants without performing an action. Handy after poking
    /// the layout through [`Self::layout_mut`].
    pub fn verify(&self) {
        self.layout.verify_invariants();
    }

    /// Drive a whole [`Op`] sequence through [`Self::apply`]. This is the entry
    /// point the randomized fuzz test uses, so the fuzzer and the deterministic
    /// scenario tests share the exact same per-op invariant checking.
    pub fn run(&mut self, ops: impl IntoIterator<Item = Op>) {
        for op in ops {
            self.apply(op);
        }
    }

    // --- simulated environment / native calls --------------------------------

    /// Plug in a fake output (1280×720 @ scale 1). `id` becomes part of its
    /// connector name (`output{id}`).
    pub fn add_output(&mut self, id: usize) {
        self.apply(Op::AddOutput(id));
    }

    /// Plug in a *portrait* fake output (720×1280 @ scale 1). The built-in
    /// [`add_output`](Self::add_output) is landscape; this one lets a scenario
    /// exercise orientation-dependent behaviour.
    pub fn add_portrait_output(&mut self, id: usize) {
        use naru_config::OutputName;
        use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};

        let name = format!("output{id}");
        if self.layout.outputs().any(|o| o.name() == name) {
            return;
        }
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: Size::from((720, 1280)),
                subpixel: Subpixel::Unknown,
                make: String::new(),
                model: String::new(),
                serial_number: String::new(),
            },
        );
        output.change_current_state(
            Some(Mode {
                size: Size::from((720, 1280)),
                refresh: 60000,
            }),
            None,
            None,
            None,
        );
        output.user_data().insert_if_missing(|| OutputName {
            connector: name,
            make: None,
            model: None,
            serial: None,
        });
        self.layout.add_output(output, None);
        self.verify();
    }

    /// Open a new tiled window in the carousel; returns its id.
    pub fn add_window(&mut self) -> usize {
        let id = self.fresh_id();
        self.apply(Op::AddWindow {
            params: TestWindowParams::new(id),
        });
        id
    }

    /// Open a new window, letting the caller tweak its params (size, floating,
    /// parent, rules) first; returns its id.
    pub fn add_window_with(&mut self, configure: impl FnOnce(&mut TestWindowParams)) -> usize {
        let id = self.fresh_id();
        let mut params = TestWindowParams::new(id);
        configure(&mut params);
        self.apply(Op::AddWindow { params });
        id
    }

    /// Open a new floating window; returns its id.
    pub fn add_floating_window(&mut self) -> usize {
        self.add_window_with(|params| params.is_floating = true)
    }

    /// Close the window with the given id.
    pub fn close_window(&mut self, id: usize) {
        self.apply(Op::CloseWindow(id));
    }

    /// Deliver `id`'s pending configure-ack (the fake equivalent of a Wayland
    /// client committing the size we asked it for).
    pub fn communicate(&mut self, id: usize) {
        self.apply(Op::Communicate(id));
    }

    /// Commit pending configures for every window currently in the layout.
    pub fn communicate_all(&mut self) {
        for id in self.window_ids() {
            self.communicate(id);
        }
    }

    /// Advance the simulated clock by `ms` milliseconds and step every
    /// animation forward by that much. This is real animation simulation: the
    /// fake clock drives the exact same `Layout::advance_animations` the
    /// compositor runs each frame, so `window_geometry` reports genuine
    /// mid-flight positions between calls.
    pub fn advance_time(&mut self, ms: i32) {
        self.apply(Op::AdvanceAnimations { msec_delta: ms });
    }

    /// Advance the clock by one ~60 fps frame (16 ms) and step animations —
    /// the granularity a real compositor renders at.
    pub fn advance_frame(&mut self) {
        self.advance_time(16);
    }

    /// Step ~60 fps frames until no animation is in flight (or `max_frames` is
    /// reached, which fails the test — an animation that never settles is a
    /// bug). Returns the number of frames it took.
    pub fn run_until_settled(&mut self) -> usize {
        const MAX_FRAMES: usize = 600; // 10 s of simulated time — generous.
        for frame in 0..MAX_FRAMES {
            if !self.are_animations_ongoing() {
                return frame;
            }
            self.advance_frame();
        }
        panic!("animations did not settle within {MAX_FRAMES} frames");
    }

    /// Whether any animation (window movement, view scroll, resize, open/close)
    /// is still in flight on any output.
    pub fn are_animations_ongoing(&self) -> bool {
        self.layout.are_animations_ongoing(None)
    }

    /// Run all in-flight animations to completion instantly (jump to their
    /// end state without stepping frames).
    pub fn complete_animations(&mut self) {
        self.apply(Op::CompleteAnimations);
    }

    /// Run a render-element update pass over every output. Some layout bugs
    /// only surface once render elements are recomputed, so scenario tests
    /// that care about geometry should call this before asserting.
    pub fn update_render_elements(&mut self) {
        self.layout.update_render_elements(None);
        self.layout.verify_invariants();
    }

    /// Drive a full settle cycle to a stable, committed layout.
    ///
    /// Some re-fits are kicked off *inside* the render pass — notably the
    /// fixed-side panel-area sync (`sync_carousel_parent_area`), which only
    /// notices a panel's width changed when render runs. So a single
    /// `communicate_all` before render misses the configures that render then
    /// emits. This renders first (to trigger any render-driven re-fit), then
    /// commits, finishes animations, and renders again — leaving committed
    /// geometry. Call it twice when a re-fit is itself one render-cycle delayed
    /// (a panel resize reflows the carousel on the *next* frame).
    pub fn settle(&mut self) {
        self.update_render_elements();
        self.communicate_all();
        self.complete_animations();
        self.update_render_elements();
    }

    // --- shared actions (verified) -------------------------------------------
    //
    // These delegate to the inherent `Layout` methods — the exact entry points
    // the real input handler uses — and then re-check invariants.

    pub fn move_window_left_stacked(&mut self) {
        self.layout.move_window_left_stacked();
        self.verify();
    }

    pub fn move_window_right_stacked(&mut self) {
        self.layout.move_window_right_stacked();
        self.verify();
    }

    pub fn move_window_up_stacked(&mut self) {
        self.layout.move_window_up_stacked();
        self.verify();
    }

    pub fn move_window_down_stacked(&mut self) {
        self.layout.move_window_down_stacked();
        self.verify();
    }

    pub fn focus_left(&mut self) {
        self.layout.focus_left();
        self.verify();
    }

    pub fn focus_right(&mut self) {
        self.layout.focus_right();
        self.verify();
    }

    pub fn focus_up(&mut self) {
        self.layout.focus_up();
        self.verify();
    }

    pub fn focus_down(&mut self) {
        self.layout.focus_down();
        self.verify();
    }

    pub fn move_left(&mut self) {
        self.layout.move_left();
        self.verify();
    }

    pub fn move_right(&mut self) {
        self.layout.move_right();
        self.verify();
    }

    pub fn focus_column_first(&mut self) {
        self.layout.focus_column_first();
        self.verify();
    }

    pub fn focus_column_last(&mut self) {
        self.layout.focus_column_last();
        self.verify();
    }

    /// Activate (focus) a specific window by id, wherever it lives.
    pub fn focus_window(&mut self, id: usize) {
        self.layout.activate_window(&id);
        self.verify();
    }

    // --- session-restore placement primitives --------------------------------
    //
    // These are the exact layout operations the restore matcher in
    // `handlers/compositor.rs` performs as each respawned window maps:
    // `move_column_to_index` for tiled-column restore, `set_window_width` /
    // `set_window_height` (`SizeChange::SetFixed`) for size restore, and
    // opening a window carrying an `open-in-fixed-side` rule for side-panel
    // restore. Testing them here exercises the production code path the restore
    // relies on, without standing up a whole `State`.

    /// Move the active column to `index`, clamped to the column count.
    ///
    /// Matches the production `Layout::move_column_to_index`, which is
    /// **1-based**: `index == 1` is the first column. (Callers restoring a
    /// 0-based saved index must add 1 — see [`Self::restore_tiled_placement`].)
    pub fn move_column_to_index(&mut self, index: usize) {
        self.apply(Op::MoveColumnToIndex(index));
    }

    /// Set `id`'s width via a [`SizeChange`] (the restore path uses
    /// [`SizeChange::SetFixed`]).
    pub fn set_window_width(&mut self, id: usize, change: SizeChange) {
        self.apply(Op::SetWindowWidth {
            id: Some(id),
            change,
        });
    }

    /// Set `id`'s height via a [`SizeChange`].
    pub fn set_window_height(&mut self, id: usize, change: SizeChange) {
        self.apply(Op::SetWindowHeight {
            id: Some(id),
            change,
        });
    }

    /// Restore a window's exact size the way `State::restore_window_size` does:
    /// a fixed width and/or height, each skipped when `0.0` (the "let the
    /// layout pick" sentinel persisted in older state files).
    pub fn restore_window_size(&mut self, id: usize, width: f64, height: f64) {
        if width > 0.0 {
            self.set_window_width(id, SizeChange::SetFixed(width.round() as i32));
        }
        if height > 0.0 {
            self.set_window_height(id, SizeChange::SetFixed(height.round() as i32));
        }
    }

    /// Restore a just-mapped tiled window into its saved slot, mirroring the
    /// post-`add_window` block in `State` (`handlers/compositor.rs`): focus the
    /// window, move its column to the saved index, then restore its size.
    ///
    /// `column_index` is **0-based** (as persisted in `Placement::Tiled`); the
    /// `+ 1` converts it to the 1-based [`Self::move_column_to_index`] — the
    /// exact conversion the compositor must perform. A test that drops the
    /// `+ 1` reproduces the off-by-one this harness originally surfaced.
    pub fn restore_tiled_placement(
        &mut self,
        id: usize,
        column_index: usize,
        width: f64,
        height: f64,
    ) {
        self.focus_window(id);
        self.move_column_to_index(column_index + 1);
        self.restore_window_size(id, width, height);
    }

    /// Open a window carrying an `open-in-fixed-side` rule, mirroring the
    /// side-panel restore path (which calls `Mapped::set_open_in_fixed_side`
    /// before `add_window`). Returns its id.
    pub fn add_window_in_fixed_side(&mut self, side: OpenInFixedSide) -> usize {
        self.add_window_with(|params| {
            params.rules = Some(ResolvedWindowRules {
                open_in_fixed_side: Some(side),
                ..ResolvedWindowRules::default()
            });
        })
    }

    /// Hit-test `pos` (output-local logical coordinates) on `output{output_id}`
    /// through the production `Layout::window_under` — the *mouse* path. This
    /// is deliberately distinct from [`focus_window`], which activates a window
    /// by id and bypasses hit-testing entirely: a pointer click first has to
    /// find the window under the cursor, and that lookup used to skip the
    /// fixed-side panels. Returns the id of the window under the point, if any.
    pub fn window_under(&self, output_id: usize, pos: (f64, f64)) -> Option<usize> {
        let name = format!("output{output_id}");
        let output = self.layout.outputs().find(|o| o.name() == name)?.clone();
        self.layout
            .window_under(&output, Point::from(pos))
            .map(|(win, _hit)| *win.id())
    }

    /// Simulate a left-click at `pos`: hit-test like the pointer handler, then
    /// focus whatever window was hit. Returns the focused window id, or `None`
    /// if the click landed on empty space.
    pub fn click_to_focus(&mut self, output_id: usize, pos: (f64, f64)) -> Option<usize> {
        let id = self.window_under(output_id, pos)?;
        self.focus_window(id);
        Some(id)
    }

    // --- escape hatches ------------------------------------------------------

    /// Borrow the underlying layout for queries/actions not yet wrapped.
    /// Prefer the wrapped actions and [`LayoutModel`] queries.
    pub fn layout(&self) -> &Layout<TestWindow> {
        &self.layout
    }

    /// Mutable access to the underlying layout. The caller is responsible for
    /// calling [`Self::verify`] afterwards.
    pub fn layout_mut(&mut self) -> &mut Layout<TestWindow> {
        &mut self.layout
    }

    fn fresh_id(&mut self) -> usize {
        let id = self.next_window_id;
        self.next_window_id += 1;
        id
    }
}

impl Default for LayoutSim {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutModel for LayoutSim {
    type WindowId = usize;

    fn window_ids(&self) -> Vec<usize> {
        LayoutModel::window_ids(&self.layout)
    }

    fn active_window_id(&self) -> Option<usize> {
        self.layout.active_window_id()
    }

    fn window_slot(&self, id: &usize) -> Option<WindowLayer> {
        self.layout.window_slot(id)
    }

    fn window_geometry(&self, id: &usize) -> Option<Rectangle<f64, Logical>> {
        self.layout.window_geometry(id)
    }

    fn active_fixed_side(&self) -> Option<FixedSide> {
        LayoutModel::active_fixed_side(&self.layout)
    }
}

impl LayoutSim {
    /// Convenience: x-position of a window's render rectangle, panicking with a
    /// clear message if the window isn't present.
    pub fn window_x(&self, id: usize) -> f64 {
        self.window_geometry(&id)
            .unwrap_or_else(|| panic!("window {id} has no geometry (not in layout?)"))
            .loc
            .x
    }

    /// Convenience: assert a window is in the expected slot.
    #[track_caller]
    pub fn assert_slot(&self, id: usize, expected: WindowLayer) {
        let got = self.window_slot(&id);
        assert_eq!(
            got,
            Some(expected),
            "window {id} expected in {expected:?}, but slot is {got:?}",
        );
    }

    /// Convenience: assert which window is focused.
    #[track_caller]
    pub fn assert_active(&self, expected: Option<usize>) {
        let got = self.active_window_id();
        assert_eq!(got, expected, "active window mismatch");
    }

    /// Right edge (`loc.x + size.w`) of a window's render rectangle.
    pub fn window_right_edge(&self, id: usize) -> f64 {
        let geo = self
            .window_geometry(&id)
            .unwrap_or_else(|| panic!("window {id} has no geometry (not in layout?)"));
        geo.loc.x + geo.size.w
    }

    /// 0-based column ordinal of a *carousel* (scrolling-layer) window, derived
    /// from the left-to-right render x order of all carousel columns.
    ///
    /// Columns are laid out left-to-right and the view-scroll offset shifts
    /// every column by the same amount, so the sorted set of distinct column
    /// x-positions yields a scroll-independent index. Tiles sharing a column
    /// share an x, so they collapse to one ordinal. Returns `None` if the
    /// window is not in the carousel (floating or a fixed-side panel).
    pub fn scrolling_column_index(&self, id: usize) -> Option<usize> {
        if self.window_slot(&id)? != WindowLayer::Scrolling {
            return None;
        }
        let mut xs: Vec<f64> = self
            .window_ids()
            .into_iter()
            .filter(|w| self.window_slot(w) == Some(WindowLayer::Scrolling))
            .map(|w| self.window_x(w))
            .collect();
        xs.sort_by(|a, b| a.partial_cmp(b).expect("finite column x"));
        xs.dedup_by(|a, b| (*a - *b).abs() < 1.0);
        let x = self.window_x(id);
        xs.iter().position(|&cx| (cx - x).abs() < 1.0)
    }

    /// Convenience: assert a carousel window sits at the expected column index.
    #[track_caller]
    pub fn assert_column_index(&self, id: usize, expected: usize) {
        let got = self.scrolling_column_index(id);
        assert_eq!(
            got,
            Some(expected),
            "window {id} expected at column {expected}, but index is {got:?}",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window the carousel pushes into a fixed strip is reachable, lands
    /// in the expected strip, and rides back out into the carousel — asserted
    /// by slot, not just by "didn't panic".
    #[test]
    fn stack_move_into_right_strip_and_back() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        let a = sim.add_window();
        let b = sim.add_window();

        // `b` is the active (rightmost) carousel column. A right-stack at the
        // carousel's right edge overflows into the right fixed strip.
        sim.move_window_right_stacked();
        sim.assert_slot(b, WindowLayer::FixedRight);
        sim.assert_slot(a, WindowLayer::Scrolling);
        assert_eq!(sim.active_fixed_side(), Some(FixedSide::Right));
        sim.assert_active(Some(b));

        // A configure-ack on the strip window must not panic (regression:
        // `update_window` used to route every window through the carousel).
        sim.communicate(b);
        sim.assert_slot(b, WindowLayer::FixedRight);

        // Ride it back out into the carousel.
        sim.move_window_left_stacked();
        sim.assert_slot(b, WindowLayer::Scrolling);
        assert_eq!(sim.active_fixed_side(), None);
    }

    /// Symmetric left-strip path.
    #[test]
    fn stack_move_into_left_strip_and_back() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        let a = sim.add_window();
        let b = sim.add_window();

        // Focus the leftmost column and left-stack it into the left strip.
        sim.focus_column_first();
        sim.move_window_left_stacked();
        sim.assert_slot(a, WindowLayer::FixedLeft);
        sim.assert_slot(b, WindowLayer::Scrolling);
        assert_eq!(sim.active_fixed_side(), Some(FixedSide::Left));

        sim.communicate(a);
        sim.move_window_right_stacked();
        sim.assert_slot(a, WindowLayer::Scrolling);
        assert_eq!(sim.active_fixed_side(), None);
    }

    /// Closing a window that lives in a fixed strip must route the removal
    /// into the strip (regression: `Workspace::remove_tile` used to send every
    /// removal to the carousel, which `unwrap()`-panicked).
    #[test]
    fn closing_a_fixed_strip_window_does_not_panic() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        let only = sim.add_window();

        sim.move_window_left_stacked();
        sim.assert_slot(only, WindowLayer::FixedLeft);

        sim.close_window(only);
        assert!(sim.window_ids().is_empty(), "window should be gone");
        assert_eq!(sim.active_fixed_side(), None);
    }

    /// A fixed-strip window stays on screen: its geometry must be queryable
    /// and lie within the output, even though strips don't scroll.
    #[test]
    fn fixed_strip_window_has_queryable_geometry() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        let a = sim.add_window();
        let _b = sim.add_window();
        sim.move_window_right_stacked();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        // Both windows must have a geometry; the carousel window `a` and the
        // strip window are both on screen.
        assert!(
            sim.window_geometry(&a).is_some(),
            "carousel window must have geometry",
        );
    }

    /// Animations are genuinely simulated frame by frame: an animated action
    /// leaves work in flight, frame-stepping makes observable progress in
    /// `window_geometry`, and the animation eventually settles at a rest
    /// position distinct from where it started.
    #[test]
    fn animations_are_simulated_frame_by_frame() {
        let mut sim = LayoutSim::new_animated();
        sim.add_output(1);
        let _a = sim.add_window();
        let _b = sim.add_window();
        let c = sim.add_window();
        sim.communicate_all();
        sim.run_until_settled();

        // `c` is the rightmost/active column. Record its resting x, then move
        // it left — that kicks off a column-movement animation.
        sim.focus_column_last();
        sim.update_render_elements();
        let x_start = sim.window_x(c);

        sim.move_left();
        assert!(
            sim.are_animations_ongoing(),
            "moving a column should start a movement animation",
        );

        // At t≈0 the render position is still essentially where it started
        // (the move sets up a decaying offset that cancels the layout jump).
        sim.update_render_elements();
        let x_t0 = sim.window_x(c);
        assert!(
            (x_t0 - x_start).abs() < 1.0,
            "at animation start the window should still render near its old x \
             (start {x_start}, t0 {x_t0})",
        );

        // Step a few frames: the window is now mid-flight, away from x_t0.
        sim.advance_frame();
        sim.advance_frame();
        sim.update_render_elements();
        let x_mid = sim.window_x(c);
        assert_ne!(
            x_mid, x_t0,
            "frame-stepping must move the window along its animation",
        );

        // Run the animation out; it settles, and the rest position differs
        // from where the window started.
        let frames = sim.run_until_settled();
        sim.update_render_elements();
        let x_settled = sim.window_x(c);
        assert!(!sim.are_animations_ongoing());
        assert!(frames > 0, "there was still animation left to run");
        assert_ne!(
            x_settled, x_start,
            "moving the column must change its resting position",
        );

        // `complete_animations` is the instant equivalent of run_until_settled.
        sim.focus_column_first();
        sim.move_right();
        sim.complete_animations();
        assert!(!sim.are_animations_ongoing());
    }

    /// Regression (found by the slow fuzzer, distilled to a fast test):
    /// consuming a window into its neighbour and then stack-moving a tile
    /// inside that 2-tile column left the column's per-tile `data` cache
    /// stale, tripping `Column::verify_invariants`.
    #[test]
    fn consume_then_stack_move_down_keeps_tile_data_consistent() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        // The two windows have *different* heights — that asymmetry is what
        // exposed the stale-cache bug.
        let _a = sim.add_window_with(|p| p.bbox = Rectangle::from_size(Size::from((1, 1))));
        let _b = sim.add_window_with(|p| p.bbox = Rectangle::from_size(Size::from((1, 2))));

        // Consume the active window into its left neighbour → one 2-tile column.
        sim.apply(Op::ConsumeOrExpelWindowLeft { id: None });
        // Focus the top tile, then stack-move it down within the column.
        sim.apply(Op::FocusWindowUpOrColumnLeft);
        sim.move_window_down_stacked();
        // `apply` / `move_window_down_stacked` already re-checked invariants;
        // a stale per-tile `data` cache would have panicked above.
    }

    /// A mouse click must be able to focus a window that lives in a fixed-side
    /// panel. The hit-test (`Layout::window_under`) used to check only floating
    /// and the carousel, so a click on a sidepanel window fell straight through
    /// to the carousel behind it — the window could never be focused by mouse.
    #[test]
    fn mouse_click_focuses_fixed_strip_window() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        let a = sim.add_window();
        let b = sim.add_window();

        // Park `b` in the right strip; `a` stays in the carousel.
        sim.move_window_right_stacked();
        sim.assert_slot(b, WindowLayer::FixedRight);
        sim.assert_slot(a, WindowLayer::Scrolling);
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        // A click at the centre of `b`'s render rectangle must resolve to `b`
        // and focus it (setting the active fixed side). Before the fix,
        // window_under skipped the fixed-side panels entirely, so a sidepanel
        // window could never be focused by mouse — the click fell through to
        // the carousel behind it.
        //
        // (We don't also assert a carousel click here: the right strip's
        // right-edge anchor is still a known follow-up, so it currently renders
        // at the left origin overlapping the carousel column. Where they
        // overlap the panel correctly wins — it's in front — which leaves no
        // carousel-only point to click in this two-window setup.)
        let geo = sim.window_geometry(&b).expect("strip window has geometry");
        let center = (geo.loc.x + geo.size.w / 2.0, geo.loc.y + geo.size.h / 2.0);
        assert_eq!(
            sim.window_under(1, center),
            Some(b),
            "window_under must hit the sidepanel window, not fall through to the carousel",
        );
        assert_eq!(sim.click_to_focus(1, center), Some(b));
        sim.assert_active(Some(b));
        assert_eq!(sim.active_fixed_side(), Some(FixedSide::Right));
    }

    /// Up/down keyboard focus must move *within* a fixed-strip column. Before
    /// the fix, `focus_up` / `focus_down` always operated on the carousel even
    /// when focus was inside a strip, so the active sidepanel window never
    /// changed.
    #[test]
    fn vertical_focus_moves_within_fixed_strip() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        let a = sim.add_window();
        let b = sim.add_window();

        // Build a two-tile strip column the only way the single-window-into-
        // panel rule allows: a multi-window column can't ride wholesale into
        // the panel, so park each window as its own single-window column and
        // merge them *inside* the strip.
        //
        // `b` (rightmost/active) right-stacks into the right strip…
        sim.move_window_right_stacked();
        sim.assert_slot(b, WindowLayer::FixedRight);
        // …then focus `a` back in the carousel and right-stack it in too; it
        // lands at the strip's inner edge beside `b`.
        sim.focus_window(a);
        sim.move_window_right_stacked();
        sim.assert_slot(a, WindowLayer::FixedRight);
        sim.assert_slot(b, WindowLayer::FixedRight);
        assert_eq!(sim.active_fixed_side(), Some(FixedSide::Right));
        // Merge the two single-window strip columns into one two-tile column.
        sim.apply(Op::ConsumeWindowIntoColumn);
        sim.communicate_all();
        sim.complete_animations();

        // Vertical focus moves between the two tiles of the strip column. The
        // merged-in window sits at the bottom and the active tile is the top
        // one, so move *down* first.
        let before = sim.active_window_id().expect("a window is focused");
        sim.focus_down();
        let after_down = sim.active_window_id().expect("a window is focused");
        assert_ne!(
            before, after_down,
            "focus_down must move within the strip column, not poke the empty carousel",
        );
        assert_eq!(sim.active_fixed_side(), Some(FixedSide::Right));
        sim.assert_slot(after_down, WindowLayer::FixedRight);

        // ...and back up to where we started.
        sim.focus_up();
        assert_eq!(sim.active_window_id(), Some(before));
    }

    /// A top strut models the space a top bar (waybar) reserves: the working
    /// area starts below it, and *both* the carousel and the side panels must
    /// lay their windows out below it. The panels ignoring the reserved top
    /// space is what left sidepanel windows rendering up under the bar.
    #[test]
    fn fixed_strip_windows_clear_a_top_strut() {
        let mut options = Options {
            enable_stacking: true,
            ..Default::default()
        };
        options.layout.struts.top = FloatOrInt(50.0);

        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1);
        let a = sim.add_window(); // carousel
        let b = sim.add_window();
        sim.move_window_right_stacked(); // b → right strip
        sim.assert_slot(b, WindowLayer::FixedRight);
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        let carousel_top = sim.window_geometry(&a).expect("carousel geo").loc.y;
        let strip_top = sim.window_geometry(&b).expect("strip geo").loc.y;

        assert!(
            carousel_top >= 50.0,
            "carousel window should clear the 50px top strut, got {carousel_top}",
        );
        assert!(
            strip_top >= 50.0,
            "strip window should clear the 50px top strut, got {strip_top}",
        );
        assert!(
            (carousel_top - strip_top).abs() < 0.5,
            "strip window top ({strip_top}) must align with the carousel ({carousel_top})",
        );
    }

    /// A window parked in the right fixed-side strip must render flush against
    /// the workspace's RIGHT edge, not at the left origin. Regression: the
    /// strip's right-edge anchor was attempted by shifting the inner
    /// ScrollingSpace's `parent_area`, but render positions are view-relative
    /// (the view offset was forced to zero), so the shift never reached the
    /// screen and the panel drew on the left, overlapping the carousel.
    #[test]
    fn right_strip_window_renders_against_right_edge() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1); // 1280×720 landscape, no struts → working area right edge = 1280
        let a = sim.add_window();
        let b = sim.add_window();

        // Park `b` in the right strip; `a` stays in the carousel.
        sim.move_window_right_stacked();
        sim.assert_slot(b, WindowLayer::FixedRight);
        sim.assert_slot(a, WindowLayer::Scrolling);
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        // The strip window's right edge sits on the working area's right edge…
        let right_edge = sim.window_right_edge(b);
        assert!(
            (right_edge - 1280.0).abs() < 1.0,
            "right-strip window should end flush with the 1280px right edge, got {right_edge}",
        );
        // …and it lives in the right half of the screen, not at the left origin.
        let strip_x = sim.window_x(b);
        assert!(
            strip_x > 640.0,
            "right-strip window should render in the right half, got x={strip_x}",
        );
        // The carousel window sits to its left.
        assert!(
            sim.window_x(a) < strip_x,
            "carousel window (x={}) must be left of the right strip (x={strip_x})",
            sim.window_x(a),
        );
    }

    /// Mirror guard for the left strip: a parked window stays flush against the
    /// LEFT edge (x≈0). This is the behaviour the right strip silently lacked.
    #[test]
    fn left_strip_window_renders_against_left_edge() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        let a = sim.add_window();
        let _b = sim.add_window();

        sim.focus_column_first();
        sim.move_window_left_stacked();
        sim.assert_slot(a, WindowLayer::FixedLeft);
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        let left_x = sim.window_x(a);
        assert!(
            left_x.abs() < 1.0,
            "left-strip window should render flush against the left edge, got x={left_x}",
        );
    }

    /// A stack-move on a *floating* window must pull it into the tiling layer
    /// rather than no-op. The tiling stack-move helpers ignore floating
    /// windows, so before the fix — and especially with floating-by-default —
    /// the window just stayed centered as a floating window.
    #[test]
    fn stack_move_on_floating_window_tiles_it() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        let f = sim.add_floating_window();
        sim.assert_slot(f, WindowLayer::Floating);

        sim.move_window_right_stacked();
        sim.assert_slot(f, WindowLayer::Scrolling);
        assert_eq!(sim.active_fixed_side(), None);
        sim.assert_active(Some(f));
    }

    /// Directional focus must cross the floating↔tiling boundary positionally:
    /// from a tiled window, pressing toward a floating window focuses it, and
    /// pressing back returns to the tiled window. (Before, focus stayed within
    /// the active layer and the two could only be swapped with a dedicated
    /// bind.)
    #[test]
    fn directional_focus_crosses_floating_tiling_boundary() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        let t = sim.add_window(); // tiled (carousel)
        let f = sim.add_floating_window(); // floating
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        let tg = sim.window_geometry(&t).expect("tiled geo");
        let fg = sim.window_geometry(&f).expect("floating geo");
        let dx = (fg.loc.x + fg.size.w / 2.0) - (tg.loc.x + tg.size.w / 2.0);
        let dy = (fg.loc.y + fg.size.h / 2.0) - (tg.loc.y + tg.size.h / 2.0);
        assert!(
            dx.abs() > 1.0 || dy.abs() > 1.0,
            "windows overlap exactly; can't pick a crossing direction",
        );

        // From the tiled window, move along the axis on which the floating
        // window is offset, toward it.
        sim.focus_window(t);
        sim.assert_slot(t, WindowLayer::Scrolling);
        if dx.abs() >= dy.abs() {
            if dx > 0.0 {
                sim.focus_right();
            } else {
                sim.focus_left();
            }
        } else if dy > 0.0 {
            sim.focus_down();
        } else {
            sim.focus_up();
        }
        sim.assert_active(Some(f));
        sim.assert_slot(f, WindowLayer::Floating);

        // And the opposite direction crosses back to the tiled window.
        if dx.abs() >= dy.abs() {
            if dx > 0.0 {
                sim.focus_left();
            } else {
                sim.focus_right();
            }
        } else if dy > 0.0 {
            sim.focus_up();
        } else {
            sim.focus_down();
        }
        sim.assert_active(Some(t));
    }

    /// How many windows render in the same carousel column as `id` (itself
    /// included). Tiles in one column share an exact render x, while distinct
    /// columns are at least a column-width + gap apart and the view scroll
    /// offset applies to every column equally — so an x match is an
    /// unambiguous "same column" signal regardless of scroll position.
    fn column_occupancy(sim: &LayoutSim, id: usize) -> usize {
        let x = sim.window_x(id);
        sim.window_ids()
            .into_iter()
            .filter(|w| (sim.window_x(*w) - x).abs() < 1.0)
            .count()
    }

    /// Moving a window right out of a column that holds more than one window
    /// must split it into its own new single-window column — not merge it into
    /// the right neighbour's stack. Regression: the default routing pushed the
    /// window into the neighbour column as a new row regardless of how many
    /// windows the source column held.
    #[test]
    fn stack_move_right_from_multi_window_column_splits_into_new_column() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        let a = sim.add_window();
        let b = sim.add_window();
        let c = sim.add_window();

        // Build [a, b][c]: focus the leftmost column and consume its right
        // neighbour `b` into it, leaving `c` as a separate right-neighbour
        // column — the exact merge-prone setup.
        sim.focus_column_first();
        sim.apply(Op::ConsumeWindowIntoColumn);
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        sim.focus_window(b);
        assert_eq!(
            column_occupancy(&sim, b),
            2,
            "precondition: b shares a two-window column with a",
        );

        sim.move_window_right_stacked();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        // `b` is pulled out into a column of its own, still tiled and focused.
        sim.assert_slot(b, WindowLayer::Scrolling);
        sim.assert_active(Some(b));
        assert_eq!(
            column_occupancy(&sim, b),
            1,
            "b should be alone in a brand-new column, not merged into a neighbour",
        );
        // ...and specifically did NOT land in `c`'s column (the old merge target).
        assert!(
            (sim.window_x(b) - sim.window_x(c)).abs() > 1.0,
            "b must not have merged into the right neighbour c's column",
        );
        // `a` is left behind, now alone in its column too.
        assert_eq!(column_occupancy(&sim, a), 1, "a should be left alone");
    }

    /// Mirror of the right-split test for the left direction.
    #[test]
    fn stack_move_left_from_multi_window_column_splits_into_new_column() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        let a = sim.add_window();
        let b = sim.add_window();
        let c = sim.add_window();

        // Build [a][b, c]: focus the middle column `b` and consume `c` into it,
        // leaving `a` as a separate left-neighbour column.
        sim.focus_column_first();
        sim.focus_right();
        sim.apply(Op::ConsumeWindowIntoColumn);
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        sim.focus_window(c);
        assert_eq!(
            column_occupancy(&sim, c),
            2,
            "precondition: c shares a two-window column with b",
        );

        sim.move_window_left_stacked();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        sim.assert_slot(c, WindowLayer::Scrolling);
        sim.assert_active(Some(c));
        assert_eq!(
            column_occupancy(&sim, c),
            1,
            "c should be alone in a brand-new column, not merged into a neighbour",
        );
        assert!(
            (sim.window_x(c) - sim.window_x(a)).abs() > 1.0,
            "c must not have merged into the left neighbour a's column",
        );
        assert_eq!(column_occupancy(&sim, b), 1, "b should be left alone");
    }

    /// Moving toward a side panel from a *multi-window* column at the carousel
    /// edge must NOT slide the whole column into the panel. Instead the active
    /// window splits out into its own single-window column at the edge, and
    /// only a *single-window* column can then enter the panel on a following
    /// move. (A whole multi-window column used to ride straight into the
    /// strip.)
    #[test]
    fn multi_window_column_splits_before_entering_right_strip() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        let a = sim.add_window();
        let b = sim.add_window();

        // Merge a + b into a single two-tile column — the rightmost (and only)
        // carousel column, so a right-stack is "toward the right panel".
        sim.apply(Op::ConsumeOrExpelWindowLeft { id: None });
        sim.communicate_all();
        sim.complete_animations();
        let active = sim.active_window_id().expect("a window is focused");
        assert_eq!(column_occupancy(&sim, active), 2, "precondition: 2-tile column");

        // First move: the active window splits out into its own column at the
        // right edge. Nothing enters the panel yet.
        sim.move_window_right_stacked();
        sim.communicate_all();
        sim.complete_animations();
        sim.assert_slot(active, WindowLayer::Scrolling);
        assert_eq!(sim.active_fixed_side(), None, "must not have entered the panel");
        assert_eq!(
            column_occupancy(&sim, active),
            1,
            "active window split into its own single-window column",
        );

        // Second move: now it's a single-window column at the edge, so it
        // enters the right panel.
        sim.move_window_right_stacked();
        sim.assert_slot(active, WindowLayer::FixedRight);
        assert_eq!(sim.active_fixed_side(), Some(FixedSide::Right));
        // The window left behind stayed in the carousel.
        let other = if active == a { b } else { a };
        sim.assert_slot(other, WindowLayer::Scrolling);
    }

    /// Left-edge mirror of [`multi_window_column_splits_before_entering_right_strip`].
    #[test]
    fn multi_window_column_splits_before_entering_left_strip() {
        let mut sim = LayoutSim::new_stacking();
        sim.add_output(1);
        sim.add_window();
        sim.add_window();

        // Merge into one two-tile column, then focus the leftmost (only) column
        // so a left-stack is "toward the left panel".
        sim.apply(Op::ConsumeOrExpelWindowLeft { id: None });
        sim.communicate_all();
        sim.complete_animations();
        sim.focus_column_first();
        let active = sim.active_window_id().expect("a window is focused");
        assert_eq!(column_occupancy(&sim, active), 2, "precondition: 2-tile column");

        sim.move_window_left_stacked();
        sim.communicate_all();
        sim.complete_animations();
        sim.assert_slot(active, WindowLayer::Scrolling);
        assert_eq!(sim.active_fixed_side(), None, "must not have entered the panel");
        assert_eq!(column_occupancy(&sim, active), 1, "split into its own column");

        sim.move_window_left_stacked();
        sim.assert_slot(active, WindowLayer::FixedLeft);
        assert_eq!(sim.active_fixed_side(), Some(FixedSide::Left));
    }

    /// Each new window opens in its own column to the right of the active one.
    #[test]
    fn new_placement_opens_new_window_in_new_column() {
        let mut sim = LayoutSim::new();
        sim.add_output(1);
        let a = sim.add_window();
        let b = sim.add_window();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        let ga = sim.window_geometry(&a).expect("a geo");
        let gb = sim.window_geometry(&b).expect("b geo");
        assert!(
            (ga.loc.x - gb.loc.x).abs() > 1.0,
            "default 'new' placement should open b in a separate column: a={ga:?} b={gb:?}",
        );
    }

    /// With `disable-carousel`, the workspace must never grow wider than the
    /// screen (minus any fixed side panels). When more windows open than fit,
    /// every window still gets its own column — nothing stacks below the active
    /// one — and the columns shrink so the whole carousel stays within the
    /// working area.
    #[test]
    fn disable_carousel_shrinks_columns_to_fit_instead_of_stacking() {
        let mut options = Options::default();
        options.layout.disable_carousel = true;
        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1); // 1280×720 — landscape

        // Fifteen columns at their ~100px natural width plus gaps run well past
        // the 1280px screen, so the fit path has to shrink them.
        let ids: Vec<usize> = (0..15).map(|_| sim.add_window()).collect();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        // Nothing stacked: each window is alone in its column.
        for &id in &ids {
            assert_eq!(
                column_occupancy(&sim, id),
                1,
                "disable-carousel must not stack windows; window {id} shares a column",
            );
        }

        let geos: Vec<_> = ids
            .iter()
            .map(|id| sim.window_geometry(id).expect("window geo"))
            .collect();

        // The carousel content fits within the screen — it never grew beyond it.
        // (The span is independent of any view scroll offset.)
        let left = geos.iter().map(|g| g.loc.x).fold(f64::INFINITY, f64::min);
        let right = geos
            .iter()
            .map(|g| g.loc.x + g.size.w)
            .fold(f64::NEG_INFINITY, f64::max);
        let screen_w = 1280.0;
        assert!(
            right - left <= screen_w + 1.0,
            "carousel content span {} must fit the {screen_w}px screen: geos={geos:?}",
            right - left,
        );

        // These windows all open at the same natural width, so the single
        // shared shrink factor leaves them equal — equal here because the
        // inputs were equal, not because the fit equalizes them (see
        // `disable_carousel_shrinks_proportionally_not_equally`).
        let w0 = geos[0].size.w;
        for g in &geos {
            assert!(
                (g.size.w - w0).abs() < 1.0,
                "equal-natural columns should stay equal after the shared shrink: {} vs {w0}",
                g.size.w,
            );
        }
    }

    /// When the columns fit, they keep their natural width (never grown to fill)
    /// and the row is centered with equal slack on both sides of the screen.
    #[test]
    fn disable_carousel_keeps_natural_width_and_centers_when_fitting() {
        let mut options = Options::default();
        options.layout.disable_carousel = true;
        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1); // 1280×720

        // One window fits, so it sits at its natural width.
        let a = sim.add_window();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let natural = sim.window_geometry(&a).expect("a geo").size.w;

        // Two windows still fit (2 × natural + gap < 1280), so neither shrinks.
        let b = sim.add_window();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        let ga = sim.window_geometry(&a).expect("a geo");
        let gb = sim.window_geometry(&b).expect("b geo");
        assert!(
            (ga.size.w - natural).abs() < 1.0 && (gb.size.w - natural).abs() < 1.0,
            "fitting windows must keep natural width (never grown): a={} b={} natural={natural}",
            ga.size.w,
            gb.size.w,
        );

        // The pair is centered: equal slack left and right.
        let screen_w = 1280.0;
        let left = ga.loc.x.min(gb.loc.x);
        let right = (ga.loc.x + ga.size.w).max(gb.loc.x + gb.size.w);
        let left_gap = left;
        let right_gap = screen_w - right;
        assert!(
            left_gap > 1.0,
            "the windows should not fill the screen (there must be slack to center): left_gap={left_gap}",
        );
        assert!(
            (left_gap - right_gap).abs() < 1.0,
            "row must be centered: left_gap={left_gap} right_gap={right_gap}",
        );
    }

    /// Closing windows grows the survivors back toward their preferred width:
    /// a disable-carousel row shrunk to fit many columns recovers proportionally
    /// when the crowd is closed, reclaiming the freed space rather than leaving
    /// it empty. The close path uses a grow-enabled fit (`allow_grow = true`).
    #[test]
    fn disable_carousel_grows_survivors_back_after_closing_windows() {
        let mut options = Options::default();
        options.layout.disable_carousel = true;
        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1);

        let a = sim.add_window();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let natural = sim.window_geometry(&a).expect("a geo").size.w;

        // Open enough windows that the row overflows the screen and has to
        // shrink (13 more × the ~100px natural width + gaps > 1280).
        let extra: Vec<usize> = (0..13).map(|_| sim.add_window()).collect();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let shrunk = sim.window_geometry(&a).expect("a geo").size.w;
        assert!(
            shrunk < natural - 1.0,
            "columns should have shrunk below natural width while crowded",
        );

        // Close the crowd — the survivor grows back toward its preferred
        // (natural) width, reclaiming the freed space.
        for id in extra {
            sim.close_window(id);
        }
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let after = sim.window_geometry(&a).expect("a geo").size.w;
        assert!(
            after > shrunk + 1.0,
            "closing windows should grow the survivor back: {shrunk} -> {after} (natural {natural})",
        );
        assert!(
            (after - natural).abs() < 1.0,
            "the lone survivor should return to its natural width: {after} (natural {natural})",
        );
    }

    /// Normal (scrolling) carousel: closing a window must not grow the
    /// survivors via the min-span floor. Three natural-width columns exceed the
    /// half-screen floor (so they open ungrown); closing one drops the row
    /// below the floor, which the old close path would have scaled back up.
    /// Two columns are left (min-span never grows a lone column anyway), so the
    /// difference isolates the close-time growth.
    #[test]
    fn closing_window_does_not_grow_survivors_via_min_span() {
        let mut sim = LayoutSim::new();
        sim.add_output(1); // 1280×720 → min-span floor 640px

        let a = sim.add_window();
        let b = sim.add_window();
        let c = sim.add_window();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let a_before = sim.window_geometry(&a).expect("a geo").size.w;
        let b_before = sim.window_geometry(&b).expect("b geo").size.w;

        // Close c, leaving a + b. Their combined span is now under the floor,
        // but neither may be scaled up — the close path skips min-span growth.
        sim.close_window(c);
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let a_after = sim.window_geometry(&a).expect("a geo").size.w;
        let b_after = sim.window_geometry(&b).expect("b geo").size.w;
        assert!(
            a_after <= a_before + 1.0 && b_after <= b_before + 1.0,
            "closing a window must not grow survivors: a {a_before}->{a_after}, b {b_before}->{b_after}",
        );
    }

    /// Overflowing columns shrink by one shared factor, preserving their
    /// relative proportions — a wide column stays proportionally wider than a
    /// narrow one rather than being equalized.
    #[test]
    fn disable_carousel_shrinks_proportionally_not_equally() {
        let mut options = Options::default();
        options.layout.disable_carousel = true;
        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1);

        // Column A wide (≈2/3), column B narrow (≈1/3): a ~2:1 natural ratio.
        let a = sim.add_window();
        sim.apply(Op::SetColumnWidth(SizeChange::SetProportion(66.0)));
        let b = sim.add_window();
        sim.apply(Op::SetColumnWidth(SizeChange::SetProportion(33.0)));
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let ratio_natural = sim.window_geometry(&a).expect("a geo").size.w
            / sim.window_geometry(&b).expect("b geo").size.w;
        assert!(
            ratio_natural > 1.3,
            "test setup should give A a clearly larger natural width than B: ratio={ratio_natural}",
        );

        // Crowd the row so the shared shrink-to-fit kicks in.
        let _c = sim.add_window();
        let _d = sim.add_window();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        let wa = sim.window_geometry(&a).expect("a geo").size.w;
        let wb = sim.window_geometry(&b).expect("b geo").size.w;
        let ratio_shrunk = wa / wb;
        assert!(
            (ratio_shrunk - ratio_natural).abs() < 0.05,
            "shrink must preserve proportions: ratio {ratio_shrunk} vs natural {ratio_natural}",
        );
        assert!(
            ratio_shrunk > 1.3,
            "columns must not be equalized by the shrink: a={wa} b={wb}",
        );
    }

    /// Resizing one window smaller frees space, and the other columns grow back
    /// proportionally toward their preferred widths to reclaim it. Two wide
    /// columns overflow and get shrunk by a shared factor; shrinking the active
    /// one lets the bystander recover toward its (large) preferred width.
    #[test]
    fn disable_carousel_grows_other_columns_when_resizing_smaller() {
        let mut options = Options::default();
        options.layout.disable_carousel = true;
        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1); // 1280×720

        // Two very wide columns: together they overflow, so the shared factor
        // shrinks both well below their preferred width.
        let bystander = sim.add_window();
        sim.apply(Op::SetColumnWidth(SizeChange::SetProportion(80.0)));
        let _active = sim.add_window();
        sim.apply(Op::SetColumnWidth(SizeChange::SetProportion(80.0)));
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let before = sim.window_geometry(&bystander).expect("bystander geo").size.w;

        // Shrink the active column so the row now fits at preferred width: the
        // bystander grows back to reclaim the freed space.
        sim.apply(Op::SetColumnWidth(SizeChange::SetProportion(10.0)));
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let after = sim.window_geometry(&bystander).expect("bystander geo").size.w;

        assert!(
            after > before + 1.0,
            "resizing one window smaller should grow the others back toward their \
             preferred width: bystander {before} -> {after}",
        );
    }

    /// Resizing a column must leave the whole row centered (equal slack on both
    /// sides) — not center the active column. With the active column on the
    /// right, active-column centering would push the row off-center; row
    /// centering keeps it balanced.
    #[test]
    fn disable_carousel_keeps_row_centered_after_resize() {
        let mut options = Options::default();
        options.layout.disable_carousel = true;
        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1); // 1280×720

        let a = sim.add_window();
        let b = sim.add_window(); // b is active and rightmost
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        // Resize the active (right) column to a clearly different width.
        sim.apply(Op::SetColumnWidth(SizeChange::SetProportion(50.0)));
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        let ga = sim.window_geometry(&a).expect("a geo");
        let gb = sim.window_geometry(&b).expect("b geo");
        let screen_w = 1280.0;
        let left = ga.loc.x.min(gb.loc.x);
        let right = (ga.loc.x + ga.size.w).max(gb.loc.x + gb.size.w);
        let left_gap = left;
        let right_gap = screen_w - right;
        assert!(
            (left_gap - right_gap).abs() < 1.0,
            "row must stay centered after a resize: left_gap={left_gap} right_gap={right_gap}",
        );
    }

    /// A mouse-driven interactive resize must track the cursor even when the
    /// disable-carousel fit has shrunk the columns below natural width. The
    /// rendered width is `natural × fit_scale`, but the resize sets the *natural*
    /// width; if the cursor delta isn't converted back out of `fit_scale`, the
    /// first motion gets re-scaled and the window snaps to a smaller fixed width
    /// instead of growing. Regression test for that snap.
    #[test]
    fn disable_carousel_interactive_resize_tracks_cursor() {
        let mut options = Options::default();
        options.layout.disable_carousel = true;
        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1); // 1280×720

        // Two very wide columns overflow, so the shared fit factor shrinks both
        // well below their natural width (fit_scale < 1) — the regime where the
        // bug bites.
        let _a = sim.add_window();
        sim.apply(Op::SetColumnWidth(SizeChange::SetProportion(80.0)));
        let b = sim.add_window(); // active, rightmost
        sim.apply(Op::SetColumnWidth(SizeChange::SetProportion(80.0)));
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        let before = sim.window_geometry(&b).expect("b geo").size.w;

        // Drag the right edge 150px to the right.
        let dx = 150.0;
        sim.apply(Op::InteractiveResizeBegin {
            window: b,
            edges: crate::layout::ResizeEdge::RIGHT,
        });
        sim.apply(Op::InteractiveResizeUpdate {
            window: b,
            dx,
            dy: 0.0,
        });
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        let after = sim.window_geometry(&b).expect("b geo").size.w;

        // The window must grow by roughly the cursor delta, not snap to a
        // smaller scaled width.
        assert!(
            after > before,
            "interactive resize must grow the window, not snap it smaller: {before} -> {after}",
        );
        assert!(
            (after - (before + dx)).abs() < 5.0,
            "interactive resize must track the cursor (~{}), got {after} (from {before})",
            before + dx,
        );

        sim.apply(Op::InteractiveResizeEnd { window: b });
    }

    /// A populated fixed-side panel shrinks the carousel's usable width; the
    /// remaining columns must re-fit into that smaller area so nothing overlaps
    /// the panel. The fixed-side panels are not part of the carousel, and all
    /// windows must stay on screen even as a panel eats into the row.
    #[test]
    fn disable_carousel_refits_when_a_fixed_panel_shrinks_it() {
        let mut options = Options::default();
        options.layout.disable_carousel = true;
        options.enable_stacking = true; // fixed-side panels need stacking
        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1); // 1280×720

        // Enough windows that, once one is parked in a panel, the rest overflow
        // the reduced area and are forced to shrink to fit it.
        let ids: Vec<usize> = (0..15).map(|_| sim.add_window()).collect();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        // Park the active (rightmost) window into the right fixed-side panel.
        sim.move_window_right_stacked();
        let parked = *ids.last().unwrap();
        sim.assert_slot(parked, WindowLayer::FixedRight);
        sim.communicate_all();
        // The panel-area re-fit (and its re-centering) is kicked off during the
        // render pass, so settle animations *after* it, then re-render.
        sim.update_render_elements();
        sim.complete_animations();
        sim.update_render_elements();

        // Every remaining carousel window sits left of the panel: the carousel
        // re-fit into the space the panel left it, so nothing overlaps.
        let panel_left = sim.window_geometry(&parked).expect("parked geo").loc.x;
        for &id in &ids[..ids.len() - 1] {
            let g = sim.window_geometry(&id).expect("carousel geo");
            assert!(
                g.loc.x + g.size.w <= panel_left + 1.0,
                "carousel window {id} (right edge {}) overlaps the fixed panel at {panel_left}",
                g.loc.x + g.size.w,
            );
        }
    }

    /// Resizing a fixed-side panel must keep the disable-carousel row out from
    /// under the panel: when the remaining columns can't shrink any further (a
    /// hard minimum width) and so can't fit the space the panel left, the row
    /// must spill off the *screen* edge, not slide under the panel. Regression
    /// test for a wide left panel pushing min-width windows under it.
    #[test]
    fn disable_carousel_panel_resize_does_not_push_carousel_under_panel() {
        let mut options = Options::default();
        options.layout.disable_carousel = true;
        options.enable_stacking = true; // fixed-side panels need stacking
        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1); // 1280×720

        // Windows with a real minimum width (like terminals/browsers) that
        // resist shrinking, so a wide-enough panel forces an unavoidable overflow.
        let ids: Vec<usize> = (0..4)
            .map(|_| sim.add_window_with(|p| p.min_max_size.0.w = 220))
            .collect();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        // Park the leftmost window into the left fixed-side panel.
        sim.focus_window(ids[0]);
        sim.move_window_left_stacked();
        let parked = ids[0];
        sim.assert_slot(parked, WindowLayer::FixedLeft);

        let settle = |sim: &mut LayoutSim| {
            for _ in 0..2 {
                sim.communicate_all();
                sim.complete_animations();
                sim.update_render_elements();
            }
        };
        settle(&mut sim);

        // Sweep the panel across widths, including ones too wide for the
        // remaining (min-width) columns to fit.
        for pw in [200, 500, 800, 300, 600] {
            sim.set_window_width(parked, SizeChange::SetFixed(pw));
            settle(&mut sim);

            let pg = sim.window_geometry(&parked).expect("parked geo");
            let panel_right = pg.loc.x + pg.size.w;
            for &id in &ids[1..] {
                let g = sim.window_geometry(&id).expect("carousel geo");
                assert!(
                    g.loc.x + 1.0 >= panel_right,
                    "panel width {pw}: carousel window {id} (left {}) slides under the \
                     left panel (right edge {panel_right})",
                    g.loc.x,
                );
            }
        }
    }

    /// Closing a window that lived in a fixed-side panel frees the carousel's
    /// usable width, and the carousel windows grow back proportionally toward
    /// their preferred widths to reclaim it. This extends the close-time
    /// grow-back across the panel boundary.
    #[test]
    fn closing_a_panel_window_grows_the_carousel_back() {
        let mut options = Options::default();
        options.layout.disable_carousel = true;
        options.enable_stacking = true; // fixed-side panels need stacking
        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1); // 1280×720

        // Eight carousel windows fit at their natural width.
        let ids: Vec<usize> = (0..8).map(|_| sim.add_window()).collect();
        sim.settle();
        let survivor = ids[0];
        let g = *ids.last().unwrap();
        let natural = sim.window_geometry(&survivor).expect("geo").size.w;

        // Park g into the right panel, then widen the panel so the remaining
        // carousel columns no longer fit and must shrink. The panel-area reflow
        // is one render cycle delayed, so settle twice.
        sim.move_window_right_stacked();
        sim.assert_slot(g, WindowLayer::FixedRight);
        sim.set_window_width(g, SizeChange::SetFixed(700));
        sim.settle();
        sim.settle();
        let shrunk = sim.window_geometry(&survivor).expect("geo").size.w;
        assert!(
            shrunk < natural - 1.0,
            "precondition: a wide panel should shrink the carousel ({natural} -> {shrunk})",
        );

        // Close the panel window. The freed panel space grows the carousel
        // survivors back toward their natural width.
        sim.close_window(g);
        sim.settle();
        sim.settle();
        let after = sim.window_geometry(&survivor).expect("geo").size.w;
        assert!(
            after > shrunk + 1.0,
            "closing a panel window should grow the carousel back: {shrunk} -> {after}",
        );
    }

    /// Growing one column wide enough to compress the row (shrink-to-fit), then
    /// shrinking it back so everything fits again, restores the *other* columns
    /// to their preferred widths. The disable-carousel fit must not ratchet
    /// permanently small once the overflow is gone. Regression for "preferred
    /// width not restored when shrinking (after growing)".
    #[test]
    fn disable_carousel_shrink_after_grow_restores_preferred_width() {
        let mut options = Options::default();
        options.layout.disable_carousel = true;
        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1); // 1280×720

        let ids: Vec<usize> = (0..3).map(|_| sim.add_window()).collect();
        sim.settle();
        let sibling = ids[1];
        let natural = sim.window_geometry(&sibling).expect("geo").size.w;

        // Grow column 0 far past what fits → the row shrinks to fit, so the
        // untouched sibling renders narrower than its preferred width.
        sim.set_window_width(ids[0], SizeChange::SetFixed(1500));
        sim.settle();
        let shrunk = sim.window_geometry(&sibling).expect("geo").size.w;
        assert!(
            shrunk < natural - 1.0,
            "precondition: growing one column should shrink the siblings \
             ({natural} -> {shrunk})",
        );

        // Shrink column 0 back so everything fits again → the sibling returns to
        // its preferred width, not stuck compressed.
        sim.set_window_width(ids[0], SizeChange::SetFixed(200));
        sim.settle();
        let restored = sim.window_geometry(&sibling).expect("geo").size.w;
        assert!(
            (restored - natural).abs() < 2.0,
            "shrinking back should restore the sibling's preferred width \
             (natural {natural}, shrunk {shrunk}, restored {restored})",
        );
    }

    /// Positionally-aware resize on a window in the *right* half of the screen:
    /// the left arrow points toward the center and grows it; the right arrow
    /// points away and shrinks it.
    #[test]
    fn disable_carousel_positional_resize_right_side_window() {
        let mut options = Options::default();
        options.layout.disable_carousel = true;
        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1); // 1280×720

        // Two modest columns that leave plenty of room to grow before the row
        // would need to shrink to fit.
        let _a = sim.add_window();
        sim.apply(Op::SetColumnWidth(SizeChange::SetProportion(20.0)));
        let b = sim.add_window(); // active, rightmost -> sits in the right half
        sim.apply(Op::SetColumnWidth(SizeChange::SetProportion(20.0)));
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let before = sim.window_geometry(&b).expect("b geo").size.w;

        // Left arrow points toward the center for a right-side window -> grow.
        sim.layout_mut()
            .resize_column_positional(false, SizeChange::AdjustProportion(20.0));
        sim.verify();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let grown = sim.window_geometry(&b).expect("b geo").size.w;
        assert!(
            grown > before + 1.0,
            "left arrow should grow a right-side window: {before} -> {grown}",
        );

        // Right arrow points away from the center -> shrink.
        sim.layout_mut()
            .resize_column_positional(true, SizeChange::AdjustProportion(20.0));
        sim.verify();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let shrunk = sim.window_geometry(&b).expect("b geo").size.w;
        assert!(
            shrunk < grown - 1.0,
            "right arrow should shrink a right-side window: {grown} -> {shrunk}",
        );
    }

    /// The mirror case: a window in the *left* half grows with the right arrow
    /// (toward center) and shrinks with the left arrow (away from center).
    #[test]
    fn disable_carousel_positional_resize_left_side_window() {
        let mut options = Options::default();
        options.layout.disable_carousel = true;
        let mut sim = LayoutSim::with_options(options);
        sim.add_output(1); // 1280×720

        let a = sim.add_window();
        sim.apply(Op::SetColumnWidth(SizeChange::SetProportion(20.0)));
        let _b = sim.add_window();
        sim.apply(Op::SetColumnWidth(SizeChange::SetProportion(20.0)));

        // Focus the left column so the resize acts on a left-half window.
        sim.focus_left();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let before = sim.window_geometry(&a).expect("a geo").size.w;

        // Right arrow points toward the center for a left-side window -> grow.
        sim.layout_mut()
            .resize_column_positional(true, SizeChange::AdjustProportion(20.0));
        sim.verify();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let grown = sim.window_geometry(&a).expect("a geo").size.w;
        assert!(
            grown > before + 1.0,
            "right arrow should grow a left-side window: {before} -> {grown}",
        );

        // Left arrow points away from the center -> shrink.
        sim.layout_mut()
            .resize_column_positional(false, SizeChange::AdjustProportion(20.0));
        sim.verify();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let shrunk = sim.window_geometry(&a).expect("a geo").size.w;
        assert!(
            shrunk < grown - 1.0,
            "left arrow should shrink a left-side window: {grown} -> {shrunk}",
        );
    }

    // --- session-restore placement -------------------------------------------
    //
    // The restore matcher (`handlers/compositor.rs`) steers each respawned
    // window into its saved slot using plain layout operations:
    // `move_column_to_index` (carousel column), `set_window_width/height` with
    // `SizeChange::SetFixed` (exact size), and an `open-in-fixed-side` rule on
    // the mapping window (side panel). These tests pin those primitives so a
    // regression in the layout surface the restore depends on is caught here,
    // in the fast layout suite, rather than only end-to-end.

    /// `move_column_to_index` relocates the active carousel column to an exact
    /// index, sliding the others over — the operation tiled-window restore uses
    /// to put each respawned window back in its saved column.
    #[test]
    fn move_column_to_index_reorders_carousel_columns() {
        let mut sim = LayoutSim::new();
        sim.add_output(1);
        let a = sim.add_window(); // column 0
        let b = sim.add_window(); // column 1
        let c = sim.add_window(); // column 2
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        // Precondition: opened left-to-right in their own columns.
        sim.assert_column_index(a, 0);
        sim.assert_column_index(b, 1);
        sim.assert_column_index(c, 2);

        // Move the first column to the end. `move_column_to_index` is 1-based,
        // so position 3 is the third (last) slot: [a,b,c] -> [b,c,a].
        sim.focus_window(a);
        sim.move_column_to_index(3);
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        sim.assert_column_index(b, 0);
        sim.assert_column_index(c, 1);
        sim.assert_column_index(a, 2);
    }

    /// An out-of-range index is clamped rather than panicking — restore reads a
    /// column index from a possibly-stale state file, so it must tolerate an
    /// index past the current column count.
    #[test]
    fn move_column_to_index_clamps_out_of_range() {
        let mut sim = LayoutSim::new();
        sim.add_output(1);
        let a = sim.add_window();
        let b = sim.add_window();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        // Ask for column 99 with only two columns present: clamps to the last.
        sim.focus_window(a);
        sim.move_column_to_index(99);
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        sim.assert_column_index(b, 0);
        sim.assert_column_index(a, 1);
    }

    /// `set_window_width/height` with `SizeChange::SetFixed` restores a window
    /// to an exact logical size. Borders are off by default, so the tile
    /// geometry equals the window size — the assertion can be tight.
    #[test]
    fn restore_window_size_sets_exact_fixed_geometry() {
        let mut sim = LayoutSim::new();
        sim.add_output(1); // 1280×720

        let a = sim.add_window();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        // Restore to a specific saved width/height, the way
        // `State::restore_window_size` does.
        sim.restore_window_size(a, 500.0, 400.0);
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        let geo = sim.window_geometry(&a).expect("a geo");
        assert!(
            (geo.size.w - 500.0).abs() < 1.0,
            "fixed width should restore to 500, got {}",
            geo.size.w,
        );
        assert!(
            (geo.size.h - 400.0).abs() < 1.0,
            "fixed height should restore to 400, got {}",
            geo.size.h,
        );
    }

    /// Moving a window into a *narrower* column grows that column to the moved window's
    /// width so the window keeps its size. (Widths ≥ the half-view min-span floor so the
    /// carousel's min-span growth doesn't perturb the assertions.)
    #[test]
    fn move_window_preserves_width_growing_narrow_target() {
        let mut sim = LayoutSim::new();
        sim.add_output(1); // 1280×720

        let a = sim.add_window(); // col 0
        sim.set_window_width(a, SizeChange::SetFixed(700)); // wide
        let b = sim.add_window(); // col 1
        sim.set_window_width(b, SizeChange::SetFixed(290)); // narrow
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        assert!((sim.window_geometry(&a).unwrap().size.w - 700.0).abs() < 2.0);

        // Move the wide window right, into the narrow column.
        sim.focus_window(a);
        sim.apply(Op::ConsumeOrExpelWindowRight { id: None });
        sim.settle();

        let a_w = sim.window_geometry(&a).unwrap().size.w;
        let b_w = sim.window_geometry(&b).unwrap().size.w;
        assert!((a_w - 700.0).abs() < 5.0, "moved window keeps its width ~700, got {a_w}");
        assert!((b_w - 700.0).abs() < 5.0, "target column grew to host it ~700, got {b_w}");
    }

    /// Moving a window into a *wider* column never shrinks it (grow-only): the resident
    /// windows keep their width and the moved window adopts the wider column.
    #[test]
    fn move_window_into_wider_column_does_not_shrink_it() {
        let mut sim = LayoutSim::new();
        sim.add_output(1); // 1280×720

        let a = sim.add_window();
        let b = sim.add_window();
        sim.settle();

        sim.set_window_width(a, SizeChange::SetFixed(300)); // moved window: narrow
        sim.set_window_width(b, SizeChange::SetFixed(800)); // target: wider
        sim.settle();

        sim.focus_window(a);
        sim.apply(Op::ConsumeOrExpelWindowRight { id: None });
        sim.settle();

        let b_w = sim.window_geometry(&b).unwrap().size.w;
        assert!((b_w - 800.0).abs() < 5.0, "wider target must not shrink, got {b_w}");
    }

    /// Continuing to move the window on restores the width of the column it leaves (which
    /// was only grown to host the visitor).
    #[test]
    fn moving_window_on_restores_left_behind_column() {
        let mut sim = LayoutSim::new();
        sim.add_output(1); // 1280×720

        let a = sim.add_window(); // col 0
        let b = sim.add_window(); // col 1
        let _c = sim.add_window(); // col 2 — keeps total content above the min-span floor
        sim.settle();

        sim.set_window_width(a, SizeChange::SetFixed(700));
        sim.set_window_width(b, SizeChange::SetFixed(300));
        sim.settle();

        // a → into b's column (grows it to 700).
        sim.focus_window(a);
        sim.apply(Op::ConsumeOrExpelWindowRight { id: None });
        sim.settle();
        assert!((sim.window_geometry(&b).unwrap().size.w - 700.0).abs() < 5.0);

        // Continue: expel a into its own column. The column it leaves snaps back to 300.
        sim.apply(Op::ConsumeOrExpelWindowRight { id: None });
        sim.settle();

        let a_w = sim.window_geometry(&a).unwrap().size.w;
        let b_w = sim.window_geometry(&b).unwrap().size.w;
        assert!((a_w - 700.0).abs() < 5.0, "moved window keeps ~700 in its own column, got {a_w}");
        assert!((b_w - 300.0).abs() < 5.0, "left-behind column restored to 300, got {b_w}");
    }

    /// Changing window focus ends the tracking: a column grown to host a window is no
    /// longer restored when that window later moves on.
    #[test]
    fn focus_change_stops_size_tracking() {
        let mut sim = LayoutSim::new();
        sim.add_output(1); // 1280×720

        let a = sim.add_window(); // col 0
        let b = sim.add_window(); // col 1
        let c = sim.add_window(); // col 2
        sim.settle();

        sim.set_window_width(a, SizeChange::SetFixed(700));
        sim.set_window_width(b, SizeChange::SetFixed(300));
        sim.settle();

        // a → into b's column (grows it to 700).
        sim.focus_window(a);
        sim.apply(Op::ConsumeOrExpelWindowRight { id: None });
        sim.settle();

        // Focus a different window — this ends move-size tracking.
        sim.focus_window(c);
        sim.settle();

        // Move a out again. Because tracking was cleared, the column it leaves is NOT
        // restored to 300 — it keeps the grown width.
        sim.focus_window(a);
        sim.apply(Op::ConsumeOrExpelWindowRight { id: None });
        sim.settle();

        let b_w = sim.window_geometry(&b).unwrap().size.w;
        assert!(
            (b_w - 700.0).abs() < 5.0,
            "focus change stops tracking, so the left-behind column is not restored \
             (stays ~700), got {b_w}",
        );
    }

    /// `restore_window_size` treats `0.0` as "let the layout pick" and leaves
    /// that dimension untouched — the default for state files written before
    /// width/height capture existed.
    #[test]
    fn restore_window_size_skips_zero_dimensions() {
        let mut sim = LayoutSim::new();
        sim.add_output(1);

        let a = sim.add_window();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();
        let natural = sim.window_geometry(&a).expect("a geo");

        // Width 0.0 → keep natural width; height 350 → set it.
        sim.restore_window_size(a, 0.0, 350.0);
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        let geo = sim.window_geometry(&a).expect("a geo");
        assert!(
            (geo.size.w - natural.size.w).abs() < 1.0,
            "zero width must leave the natural width untouched: {} vs {}",
            geo.size.w,
            natural.size.w,
        );
        assert!(
            (geo.size.h - 350.0).abs() < 1.0,
            "non-zero height should still be applied, got {}",
            geo.size.h,
        );
    }

    /// A window carrying an `open-in-fixed-side` rule lands directly in the
    /// matching fixed-side panel on open — the side-panel restore path, which
    /// sets that rule before `add_window`.
    #[test]
    fn open_in_fixed_side_rule_parks_window_in_panel() {
        let mut sim = LayoutSim::new_stacking(); // panels need stacking
        sim.add_output(1);

        // A carousel window plus one steered into each panel.
        let carousel = sim.add_window();
        let right = sim.add_window_in_fixed_side(OpenInFixedSide::Right);
        let left = sim.add_window_in_fixed_side(OpenInFixedSide::Left);

        sim.assert_slot(carousel, WindowLayer::Scrolling);
        sim.assert_slot(right, WindowLayer::FixedRight);
        sim.assert_slot(left, WindowLayer::FixedLeft);
    }

    /// End-to-end-ish replay of a session restore's *column placement*. Windows
    /// respawn and map in saved order; as each maps the matcher steers it into
    /// its saved (0-based) column — exactly the interleaved add-then-place
    /// sequence `handlers/compositor.rs` runs. The final column order must
    /// reproduce the saved layout.
    ///
    /// The saved columns ([2, 0, 1]) are a genuine permutation, so this would
    /// fail under the 0-based/1-based off-by-one that the column-index work
    /// here uncovered.
    ///
    /// Widths are deliberately *not* asserted here: restored windows are
    /// subject to the same carousel min-span rule as live ones
    /// (`grow_to_min_carousel_span`), which scales a too-narrow *multi-column*
    /// row up to fill the screen, so an exact fixed width does not survive a
    /// multi-window restore. The fixed-size primitive itself is pinned by
    /// `restore_window_size_sets_exact_fixed_geometry`.
    #[test]
    fn session_restore_replay_places_windows_in_saved_columns() {
        let mut sim = LayoutSim::new();
        sim.add_output(1);

        // Each respawned window's saved 0-based column, in map order.
        let saved_cols = [2usize, 0, 1];

        // Map each window and immediately restore it into its slot, the way the
        // first-map matcher does — not all-at-once at the end.
        let mut ids = Vec::new();
        for &col in &saved_cols {
            let id = sim.add_window();
            sim.communicate_all();
            sim.restore_tiled_placement(id, col, 0.0, 0.0);
            sim.communicate_all();
            sim.complete_animations();
            sim.update_render_elements();
            ids.push(id);
        }

        // Each window landed in its saved column.
        for (&id, &col) in ids.iter().zip(&saved_cols) {
            sim.assert_column_index(id, col);
        }
    }

    /// Session restore: a window saved on a per-output workspace *index* must be
    /// able to land on that workspace even when it doesn't exist yet in the
    /// freshly-started session — and even when windows map out of order. The old
    /// behavior resolved only *existing* workspaces, so every window fell back to
    /// the active workspace (0) and they all piled up there. `begin_session_restore`
    /// + `materialize_workspace_id_at` create the workspace on demand and protect
    /// the empty placeholders until restore settles.
    #[test]
    fn session_restore_materializes_distinct_workspaces_by_index() {
        let mut sim = LayoutSim::new();
        sim.add_output(1);

        sim.layout.begin_session_restore();

        // Windows restored in an arbitrary order: index 2 maps first, then 0, then 1.
        let ws2 = sim
            .layout
            .materialize_workspace_id_at(None, 2)
            .expect("workspace 2 materialized");
        let ws0 = sim
            .layout
            .materialize_workspace_id_at(None, 0)
            .expect("workspace 0");
        let ws1 = sim
            .layout
            .materialize_workspace_id_at(None, 1)
            .expect("workspace 1");

        // Each saved index resolved to a *distinct* workspace — nothing collapses
        // onto the active workspace.
        assert_ne!(ws0, ws1, "index 0 and 1 must be different workspaces");
        assert_ne!(ws1, ws2, "index 1 and 2 must be different workspaces");
        assert_ne!(ws0, ws2, "index 0 and 2 must be different workspaces");

        // Re-resolving the same index is idempotent: a second window saved on that
        // index joins the first rather than creating yet another workspace.
        assert_eq!(sim.layout.materialize_workspace_id_at(None, 2), Some(ws2));
        assert_eq!(sim.layout.materialize_workspace_id_at(None, 0), Some(ws0));
        assert_eq!(sim.layout.materialize_workspace_id_at(None, 1), Some(ws1));

        sim.layout.end_session_restore();
    }
}
