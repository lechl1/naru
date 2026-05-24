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

use naru_config::{FloatOrInt, NewWindowPlacement};
use smithay::utils::{Logical, Point, Rectangle, Size};

use super::{Op, TestWindow, TestWindowParams};
use crate::animation::Clock;
use crate::layout::fixed_strip::FixedSide;
use crate::layout::workspace::WindowLayer;
use crate::layout::{Layout, LayoutElement, Options};

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
        self.active_workspace()
            .and_then(|ws| ws.active_window())
            .map(|win| win.id().clone())
    }

    fn window_slot(&self, id: &W::Id) -> Option<WindowLayer> {
        self.workspaces().find_map(|(_, _, ws)| ws.window_slot(id))
    }

    fn window_geometry(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
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
        self.active_workspace().and_then(|ws| ws.active_fixed_side())
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

    /// A fresh simulator whose `new-window-placement` is `"stack"`: a new
    /// window opens under the active one on landscape outputs and to its right
    /// on portrait outputs. (The code/`Options` default is `"new"`, so the
    /// other constructors keep niri's new-column-per-window behaviour.)
    pub fn new_stack_placement() -> Self {
        let mut options = Options::default();
        options.layout.new_window_placement = NewWindowPlacement::Stack;
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
    /// exercise orientation-dependent behaviour (e.g. `new-window-placement
    /// "stack"`, which adds to the right rather than below on portrait).
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

        // Merge `a` and `b` into a single two-tile column, then ride that whole
        // column into the right strip (no right neighbour ⇒ column-into-strip).
        sim.apply(Op::ConsumeOrExpelWindowLeft { id: None });
        sim.move_window_right_stacked();
        sim.assert_slot(a, WindowLayer::FixedRight);
        sim.assert_slot(b, WindowLayer::FixedRight);
        assert_eq!(sim.active_fixed_side(), Some(FixedSide::Right));

        // Vertical focus moves between the two tiles of the strip column.
        let before = sim.active_window_id().expect("a window is focused");
        sim.focus_up();
        let after_up = sim.active_window_id().expect("a window is focused");
        assert_ne!(
            before, after_up,
            "focus_up must move within the strip column, not poke the empty carousel",
        );
        assert_eq!(sim.active_fixed_side(), Some(FixedSide::Right));
        sim.assert_slot(after_up, WindowLayer::FixedRight);

        // ...and back down to where we started.
        sim.focus_down();
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

    /// `new-window-placement "stack"` on a landscape output: a second window
    /// opens *under* the active one — same column, below it, sharing the column
    /// height equally — and takes focus.
    #[test]
    fn stack_placement_opens_new_window_under_active_in_landscape() {
        let mut sim = LayoutSim::new_stack_placement();
        sim.add_output(1); // 1280×720 — landscape
        let a = sim.add_window();
        let b = sim.add_window();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        // Both tiled; `b` opened under `a` and took focus.
        sim.assert_slot(a, WindowLayer::Scrolling);
        sim.assert_slot(b, WindowLayer::Scrolling);
        sim.assert_active(Some(b));

        let ga = sim.window_geometry(&a).expect("a geo");
        let gb = sim.window_geometry(&b).expect("b geo");
        // Same column ⇒ identical render x.
        assert!(
            (ga.loc.x - gb.loc.x).abs() < 1.0,
            "a and b should share one column (same x): a={ga:?} b={gb:?}",
        );
        // `b` is the lower tile.
        assert!(
            gb.loc.y > ga.loc.y,
            "b should open under a (greater y): a.y={} b.y={}",
            ga.loc.y,
            gb.loc.y,
        );
        // Equal split of the column height.
        assert!(
            (ga.size.h - gb.size.h).abs() < 2.0,
            "stacked tiles should be equal height: a.h={} b.h={}",
            ga.size.h,
            gb.size.h,
        );
    }

    /// `new-window-placement "stack"` on a *portrait* output adds to the right
    /// instead of below: the second window opens in a new column.
    #[test]
    fn stack_placement_opens_new_column_on_portrait() {
        let mut sim = LayoutSim::new_stack_placement();
        sim.add_portrait_output(1); // 720×1280 — portrait
        let a = sim.add_window();
        let b = sim.add_window();
        sim.communicate_all();
        sim.complete_animations();
        sim.update_render_elements();

        let ga = sim.window_geometry(&a).expect("a geo");
        let gb = sim.window_geometry(&b).expect("b geo");
        assert!(
            (ga.loc.x - gb.loc.x).abs() > 1.0,
            "on portrait, stack placement should open b in a new column (different x): \
             a={ga:?} b={gb:?}",
        );
        sim.assert_active(Some(b));
    }

    /// The conservative code default (`new-window-placement "new"`) keeps
    /// niri's behaviour: each new window opens in its own column to the right.
    #[test]
    fn new_placement_opens_new_window_in_new_column() {
        let mut sim = LayoutSim::new(); // Options::default() ⇒ "new"
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
}
