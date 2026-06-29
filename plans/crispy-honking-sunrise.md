# Plan: three layout/session fixes for naru

## Context

Three independent issues in the naru Wayland compositor, all in core layout / session-restore code:

1. **Session restore piles windows into workspace 0.** Saved windows record a per-output
   workspace *index*, but on restore that index is resolved with
   `existing_workspace_id_at`, which returns `None` when the workspace doesn't exist yet
   (a freshly-started session has only workspace 0). The window then falls back to `Auto`
   (the active workspace), so almost everything lands on workspace 0. Even when workspaces
   are created, naru compacts away empty middle workspaces (`clean_up_workspaces`, fired
   when a workspace-switch animation completes during restore), so indices shift while
   windows are still mapping — a race. We need restore to **materialize** the workspace at
   the saved index and keep those placeholders stable until restore settles.

2. **Side panels are workspace-scoped, but should be output-scoped.** The left/right
   "fixed side" strips live on each `Workspace`, so switching workspaces hides them. They
   should belong to the screen (output) and stay visible across all its workspaces.
   (User chose the full architectural move.)

3. **Closed columns leave dead space instead of growing survivors back.** In
   `disable-carousel` (fit-all) mode, opening many columns shrinks them all by a shared
   `fit_scale` to fit; each column's natural ("preferred") width is preserved in
   `col.width`. Today closing a column calls the *no-grow* refit, leaving the freed space
   empty. The user now wants survivors to grow back **proportionally toward their
   preferred width** when columns/windows close.

All three implemented together; validate with `naru validate` and a nested userland
session per CLAUDE.md. Commit/push when done (standing authorization).

---

## Task 1 — Stable workspace index on session restore

**Materialize instead of "find existing".**
- Add `Layout::materialize_workspace_id_at(&mut self, output_name: Option<&str>, index: usize) -> Option<WorkspaceId>` in `src/layout/mod.rs` (next to the existing `existing_workspace_id_at`, ~1726). Resolve the target monitor exactly as that fn does (saved connector → matching monitor, else active monitor). If `index < mon.workspaces.len()`, return that ws id; otherwise append empty workspaces via `Monitor::add_workspace_bottom()` (monitor.rs:429) until `workspaces.len() > index`, then return `mon.workspaces[index].id()`. Handle the `MonitorSet::NoOutputs` branch analogously.
- In `src/handlers/compositor.rs` (~301-304), change the `WorkspaceRef::Index { index }` arm from `existing_workspace_id_at(...)` to `materialize_workspace_id_at(...)`. The `WorkspaceRef::Name` arm is unchanged.

**Keep placeholders alive until restore settles (fix the compaction race).**
- Add a `restoring: bool` field to `Monitor` (`src/layout/monitor.rs`), default `false`.
- Gate the two empty-workspace compaction sites on `!self.restoring`:
  - `Monitor::clean_up_workspaces` (monitor.rs ~625-642) — skip removing empty unnamed non-active workspaces while restoring.
  - The remove-path compaction in `Layout` (mod.rs ~1224-1235 Normal branch and ~1253-1265 NoOutputs branch) — same guard via the monitor's flag.
- Lifecycle (`src/layout/mod.rs` + `src/naru.rs` + `src/session/manager.rs`):
  - `Layout::begin_session_restore()` / `end_session_restore()` set the flag on every monitor; `end_session_restore()` also runs a `clean_up_workspaces` pass per monitor (when no switch is ongoing) to drop any never-filled placeholders.
  - Call `begin_session_restore()` from `Naru::restore_session_apps` (naru.rs:2284) only when there are pending entries.
  - End it two ways (whichever comes first): (a) in the compositor map handler, after `take_pending_for` drains `pending_restore` to empty, call `end_session_restore()`; (b) a bounded one-shot "settle" timer (~30s) scheduled at startup via the existing calloop pattern in `SessionManager` (mirror `schedule_periodic_saves`, manager.rs:132) that calls `end_session_restore()` unconditionally, so a never-matched entry can't suppress compaction forever.

**Why this is stable:** windows map in any order; the first window for index N creates workspaces 0..=N, lands on N, and the empty middle placeholders survive (restore guard) until their own windows arrive. Uniform append keeps the trailing-empty-workspace invariant (`add_tile` re-adds a trailing empty when a window lands on the last workspace).

## Task 2 — Move fixed side panels from `Workspace` to the output (`Monitor`)

Extract a new `FixedPanels<W>` struct (in `src/layout/fixed_strip.rs`) owning `fixed_left`,
`fixed_right`, `active_fixed_side`, and the cached `view_size/working_area/scale/options`.
Embed one in `Monitor` and one in the `MonitorSet::NoOutputs` stash. Panels then render
once per output, pinned to the screen, and persist across workspace switches.

Ordered, file-by-file (the per-site detail is large; the pattern is "lift pure-strip logic
into `FixedPanels`, make cross-boundary carousel↔strip ops `Monitor` methods, push panel
inset widths down to every workspace"):

1. **`src/layout/fixed_strip.rs`** — add `FixedPanels<W>`; move the pure-strip methods off
   `Workspace` into it: construction, `advance_animations`/`are_animations_ongoing`,
   `update_render_elements(panel_focus_side)`, `update_config`, output-size update,
   `left_width()`/`right_width()`, tile/column enumeration, `has_window`/`layer_for`,
   active-window accessors, `window_under`, focus-movement set, per-window ops
   (width/height/fullscreen/maximize/interactive-resize/consume-expel/swap),
   `remove_*`, `fixed_side_tiles()`, `render_left`/`render_right`, `verify_invariants`.

2. **`src/layout/workspace.rs`** — remove `fixed_left`/`fixed_right`/`active_fixed_side`
   and every strip branch (the `open_in_fixed_side` short-circuit in `add_tile`, the
   `WindowLayer::FixedLeft/Right` arms across per-window ops, `render_fixed_*`, strip lines
   in `advance_animations`/`update_render_elements`/`update_config`/output-size/`tiles*`/
   `columns`/`window_under`/`refresh`/`activate_window`/`has_window`/`verify_invariants`).
   Add `fixed_insets: (f64, f64)` + `set_fixed_insets(left, right)` (no-op if unchanged,
   else `sync_carousel_parent_area()` + no-grow refit). `carousel_parent_area`
   (workspace.rs ~534-541) and the edge-fade band (~2779-2810) read `self.fixed_insets`
   instead of strip widths.

3. **`src/layout/monitor.rs`** — add `panels: FixedPanels<W>`; construct in `Monitor::new`.
   Add private `sync_panel_insets_to_workspaces()` (pushes `panels.left/right_width()` to
   every workspace via `set_fixed_insets`) and call it after any panel mutation,
   `update_config`, `update_output_size`, and workspace add/move. Drive panels once per
   output in `advance_animations`/`are_animations_ongoing`/`update_render_elements`/render
   (emit panels pinned, outside the per-workspace `geo` loop). Move the cross-boundary
   carousel↔strip movers and the per-window-op dispatch here (`if panels.has_window(id) {
   panels… } else { active_workspace_mut()… }`), plus `add_window`'s `open_in_fixed_side`
   routing, `window_under`, and `Monitor::tiles()/windows()/has_window` that chain
   workspaces + panels once. Focus rule: `active_fixed_side` is monitor-global and persists
   across switches; the active workspace's `scrolling_focused` is gated off when a panel
   owns focus.

4. **`src/layout/mod.rs`** — `MonitorSet::NoOutputs` gains `panels`; update `Default` and
   every destructure (most via `..`). Move `panels` between `Monitor` and the stash on the
   last-output-disconnect / first-output-reconnect transitions (so panel windows survive).
   Route the stacking-move dispatch and `active_fixed_side()` through the active **monitor**;
   include panel windows once per output in `Layout::windows()`/`has_window`.

5. **`src/session/snapshot.rs`** — enumerate `fixed_side_tiles()` **once per monitor** (and
   once for the `NoOutputs` stash) instead of per workspace, to avoid duplicate/empty panel
   rows. Restore is unchanged in spirit (windows tagged `open_in_fixed_side` route through
   `Monitor::add_window` into the monitor strip). Keep `output` = monitor name on panel rows.

**Top risks to watch:** window-ownership leaks (every `ws.windows()`/`tiles()`/`has_window`
consumer crate-wide must now also see monitor panels — grep IPC/close/focus/DnD/popup
paths); focus coherence across switches; `NoOutputs` panel hand-off (easiest correctness
bug); render z-order/origin (panels pinned, not sliding during a switch); snapshot
duplication. `verify_invariants` asserts move with the strips.

## Task 3 — Grow survivors back to preferred width on close (disable-carousel)

`col.width` already holds the natural/preferred width; `fit_scale` does the shrink, and
`ScrollingSpace::fit_columns_to_parent(allow_grow=true)` already recomputes the shared
factor freely toward `1.0`, growing all columns proportionally (uniform factor preserves
preferred-width ratios). So no new "preferred width" state is needed — only the close path
must stop forcing no-grow.

- Add `Workspace::refit_carousel_grow()` in `src/layout/workspace.rs` (beside
  `refit_carousel_no_grow`, ~578): body `self.scrolling.fit_columns_to_parent(true);`
  only — deliberately **not** calling `grow_to_min_carousel_span`, so carousel-mode close
  behavior is unchanged (and `fit_columns_to_parent` is itself a no-op outside
  disable-carousel).
- Replace `refit_carousel_no_grow()` with `refit_carousel_grow()` at the **close** sites
  only: workspace.rs ~1247 (`remove_tile`), ~1281, ~1317. Apply the same to the
  panel-freed-space site (~662) for consistency ("space freed → fill proportionally").
- **Do not touch** the resize sites that pass `allow_grow=false` (e.g. ~2145, 2163, 2183,
  2233, 2246, 2271, 2301): resizing one window must still not auto-grow the others.

---

## Verification

- `cargo build` (use `CARGO_PROFILE_DEV_DEBUG=0` per the disk-constraint memory), then
  `naru validate` on the freshly built binary.
- `cargo test` — including the layout sim tests (`src/layout/tests/sim.rs`) and session
  manager tests; add/extend sim coverage for: (1) restore materializing workspace N with
  out-of-order maps and no compaction mid-restore; (3) disable-carousel close growing
  survivors proportionally back toward natural width.
- Nested userland session (per memory): launch a fresh nested naru, then
  - Task 1: save a session with windows spread across several workspaces, restart, confirm
    each window returns to its workspace (not all on 0), including when apps map out of order.
  - Task 2: open left/right side-panel windows, switch workspaces — panels stay visible on
    every workspace of that screen; carousel never overlaps them; panel focus/resize work.
  - Task 3: in disable-carousel mode, open many columns (they shrink), close some — the
    remaining columns grow back proportionally and fill the freed space.
- The unrelated in-tree WIP in `scrolling.rs`/`sim.rs` (`MoveSizeMemory`) stays untouched.
