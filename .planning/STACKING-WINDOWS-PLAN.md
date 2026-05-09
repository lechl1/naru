# Stacking Windows Inside Tiles - Implementation Plan

**Workspace:** `/home/leochl/workspace/naru`  
**Target Feature:** Enable a Tile to hold multiple windows in a stack, with alternating move semantics for horizontal and vertical moves.

---

## DECISIONS (confirmed with user)

These resolve the Open Questions in section E and pin down implementation choices.

| ID | Decision |
|----|----------|
| E1 | **Alternation state lifetime: window-centric, ephemeral.** State exists only during a sequence of consecutive same-direction moves on a single focused window. Not persisted, not stored on the window itself — just transient state on the input/layout handler. |
| E2 | **Reset on any non-move action.** Anything other than another same-direction move on the same focused window resets the alternation counter. This includes: direction change, focus change, switching workspaces, opening/closing windows, any other input event. |
| E3 | **Add new actions, keep old.** `MoveColumnLeft/Right` and `MoveWindowUp/Down` keep their current swap-style semantics. We add four new actions for the alternating semantics. Working names: `MoveWindowLeftStacked`, `MoveWindowRightStacked`, `MoveWindowUpStacked`, `MoveWindowDownStacked` (final names TBD during implementation). |
| E4 | **Single-window-tile first move = OVERLAP into neighbor.** Multi-window-tile first move = NEW COLUMN/ROW. Then both alternate from there on subsequent same-direction moves (overlap → new → overlap → ..., or new → overlap → new → ...). This matches the original spec verbatim. |
| E5 | **No visual indication of stacks (yet).** Only the active window in a stack is rendered. Tab bar / count badge deferred to a follow-up. |
| E6 | **Always on, no feature flag.** New actions ship with the binary. **Default keybindings: `Mod+Alt+Left`, `Mod+Alt+Right`, `Mod+Alt+Up`, `Mod+Alt+Down`** bound to the four new `MoveWindow*Stacked` actions in `resources/default-config.kdl`. (`Mod` is the niri convention for the user's chosen modifier — typically Super; `Alt` adds the second modifier per the user's request.) |

### Behavioral state machine (derived from E1+E2+E4)

```
state := Option<{ window_id, last_dir: Dir, last_was_overlap: bool }>

on MoveWindow*Stacked(dir):
    if state.is_some() && state.window_id == focused.id && state.last_dir == dir:
        # continuation of a sequence
        action := if state.last_was_overlap { NewColumn } else { Overlap }
    else:
        # first move (or reset by intervening event)
        action := if focused.tile.windows.len() > 1 { NewColumn } else { Overlap }
    apply(action, dir)
    if at_workspace_edge_in(dir):
        # E5: edge always wins, regardless of alternation
        move_to_next_workspace_in(dir)
        clear state
    else:
        state := Some({ focused.id, dir, last_was_overlap = (action == Overlap) })

on any_other_event:
    clear state
```

### Build environment status (as of plan write)

`cargo check` on the rebranded base **fails** at the `libseat-sys` build script. Missing system dev packages on this host (probed via `pkg-config`):

```
MISSING: wayland-server wayland-client libinput libseat gbm pipewire-0.3
         egl glesv2 libdisplay-info xkbcommon libdrm pango pangocairo
         gtk4 libadwaita-1
OK:      libudev pixman-1 cairo
```

Until these are installed, no code change can be validated locally with `cargo check`/`build`/`test`. The Phase 7 snapshot tests would also be unrunnable. Install command (Debian/Ubuntu, requires sudo):

```
sudo apt install libwayland-dev libinput-dev libseat-dev libgbm-dev \
    libpipewire-0.3-dev libegl-dev libgles-dev libdisplay-info-dev \
    libxkbcommon-dev libdrm-dev libpango1.0-dev libgtk-4-dev libadwaita-1-dev
```

---

## A. Data Model Map

### Current Structure

**Workspace** (`/home/leochl/workspace/naru/src/layout/workspace.rs:46`)
```rust
pub struct Workspace<W: LayoutElement> {
    scrolling: ScrollingSpace<W>,    // Main tiling layout
    floating: FloatingSpace<W>,      // Floating windows layout
    floating_is_active: FloatingActive,  // Mode toggle
    original_output: OutputId,
    output: Option<Output>,
    // ... other fields
}
```

**ScrollingSpace** (`/home/leochl/workspace/naru/src/layout/scrolling.rs:43`)
- `columns: Vec<Column<W>>` — ordered list of columns left-to-right
- `active_column_idx: usize` — currently focused column index
- `data: Vec<ColumnData>` — per-column metadata

**Column** (`/home/leochl/workspace/naru/src/layout/scrolling.rs:146`)
```rust
pub struct Column<W: LayoutElement> {
    tiles: Vec<Tile<W>>,         // Stack of tiles (currently each holds 1 window)
    data: Vec<TileData>,         // Per-tile metadata (size, flags)
    active_tile_idx: usize,      // Currently focused tile in this column
    width: ColumnWidth,
    // ... animations, display mode (Normal or Tabbed)
}
```

**Tile** (`/home/leochl/workspace/naru/src/layout/tile.rs:40`)
```rust
pub struct Tile<W: LayoutElement> {
    window: W,                   // Currently holds a SINGLE window
    border: FocusRing,
    focus_ring: FocusRing,
    shadow: Shadow,
    sizing_mode: SizingMode,
    // ... animations, snapshots, floating state tracking
}
```

### Focus Tracking

- **Per-workspace:** `Workspace::scrolling.active_column_idx` (which column has focus)
- **Per-column:** `Column::active_tile_idx` (which tile in the column has focus)
- **Per-tile:** Currently **only one window**, so no "active window within tile" concept yet
- **Focus delegation:** `Workspace::focus_left()`, `focus_right()`, `focus_up()`, `focus_down()` delegate to `ScrollingSpace` which delegates to `Column`

### Rendering

- **Tile rendering:** `Tile` owns a single `window: W` and renders it directly
- **Z-order:** Only the top tile in a column is rendered (others are invisible)
- **Column display modes:** `ColumnDisplay::{Normal, Tabbed}` handled in `Column::update_layout()` (affects tile sizing/positioning)
- **Tab indicator:** `TabIndicator` struct tracks tabbed mode visuals but does NOT yet represent stacked windows within a tile

---

## B. Existing Move Actions

### Action Enum Location

**IPC/Config Actions:** `/home/leochl/workspace/naru/naru-ipc/src/lib.rs:194`
- `MoveColumnLeft`, `MoveColumnRight`
- `MoveWindowUp`, `MoveWindowDown`
- `MoveColumnLeftOrToMonitorLeft`, `MoveColumnRightOrToMonitorRight`
- `MoveWindowUpOrToWorkspaceUp`, `MoveWindowDownOrToWorkspaceDown`

### Input Handler Call Sites

**Input handler:** `/home/leochl/workspace/naru/src/input/mod.rs:918–1027`
- Line 918: `Action::MoveColumnLeft` → calls `self.naru.layout.move_left()`
- Line 929: `Action::MoveColumnRight` → calls `self.naru.layout.move_right()`
- Line 990: `Action::MoveWindowDown` → calls `self.naru.layout.move_down()`
- Line 1001: `Action::MoveWindowUp` → calls `self.naru.layout.move_up()`

### Layout Method Call Chain

**Entry points (Layout struct):** `/home/leochl/workspace/naru/src/layout/mod.rs`
- Line 1791: `pub fn move_left()` → delegates to `active_workspace_mut().move_left()`
- Line 1798: `pub fn move_right()` → delegates to `active_workspace_mut().move_right()`
- Line 1848: `pub fn move_down()` → delegates to `active_workspace_mut().move_down()`
- Line 1855: `pub fn move_up()` → delegates to `active_workspace_mut().move_up()`

**Workspace delegation:** `/home/leochl/workspace/naru/src/layout/workspace.rs`
- Line 1042: `pub fn move_left()` → delegates to `self.scrolling.move_left()`
- Line 1051: `pub fn move_right()` → delegates to `self.scrolling.move_right()`
- Line 1081: `pub fn move_down()` → delegates to `self.scrolling.move_down()`
- Line 1090: `pub fn move_up()` → delegates to `self.scrolling.move_up()`

**ScrollingSpace (horizontal):** `/home/leochl/workspace/naru/src/layout/scrolling.rs`
- Line 1717: `pub fn move_left()` — if `active_column_idx > 0`, calls `move_column_to(idx - 1)` (swaps columns)
- Line 1726: `pub fn move_right()` — if `active_column_idx + 1 < len`, calls `move_column_to(idx + 1)` (swaps columns)

**ScrollingSpace (vertical):** `/home/leochl/workspace/naru/src/layout/scrolling.rs`
- Line 1749: `pub fn move_down()` → delegates to `columns[active_column_idx].move_down()`
- Line 1757: `pub fn move_up()` → delegates to `columns[active_column_idx].move_up()`

**Column (vertical):** `/home/leochl/workspace/naru/src/layout/scrolling.rs`
- Line 4750: `fn move_up()` — swaps `tiles[active_tile_idx]` with `tiles[active_tile_idx - 1]`, updates `active_tile_idx`
- Line 4773: `fn move_down()` — swaps `tiles[active_tile_idx]` with `tiles[active_tile_idx + 1]`, updates `active_tile_idx`

### Current Behavior

**MoveColumnLeft/Right:** Moves the **entire active column** to the left/right position by reordering columns in the `ScrollingSpace.columns` vector. Does NOT move individual windows.

**MoveWindowUp/Down:** Swaps the **active tile's position** within the column with adjacent tiles. The window itself stays in the same tile object; only the tile's position in the list changes.

### Empty Column Removal

**Removal logic:** `/home/leochl/workspace/naru/src/layout/scrolling.rs:1058–1061`
- When a window is removed from a column with only 1 tile, the entire column is removed by `remove_column_by_idx()` (line 1162).
- Method: `columns.remove(column_idx)` and `data.remove(column_idx)` (line 1180–1181).
- **Active column index is updated** if needed (line 1176–1178).

### Edge Behavior (At Workspace Bounds)

- **MoveColumnLeft at left edge:** Returns `false`, no action (line 1718–1720)
- **MoveColumnRight at right edge:** Returns `false`, no action (line 1728–1730)
- **MoveWindowUp at top tile:** Returns `false`, no action (line 4751–4753)
- **MoveWindowDown at bottom tile:** Returns `false`, no action (line 4774–4776)

**Separate actions for workspace wrap:**
- `MoveColumnLeftOrToMonitorLeft` (input/mod.rs:952) — if at left edge, calls `layout.move_column_left_or_to_output(&output)` to move column to adjacent monitor
- `MoveWindowUpOrToWorkspaceUp` (input/mod.rs:1022) — if at top of workspace, calls `layout.move_up_or_to_workspace_up()` to move window to workspace above
- Similar for right/down variants

---

## C. Impact Analysis by Area

### 1. Data Model Changes
**File:** `/home/leochl/workspace/naru/src/layout/tile.rs:40`  
**Changes:**
- Change `window: W` to `windows: Vec<W>` (stack of windows)
- Add `active_window_idx: usize` (which window in the stack is on top)
- Update all accessors: `window()`, `window_mut()` to use `windows[active_window_idx]`

**Complexity:** M (15-20 changes across the Tile struct and its 50+ methods)  
**Risk:** High — Tile is fundamental; impacts every method that accesses the window

### 2. Focus Tracking & Cycling
**File:** `/home/leochl/workspace/naru/src/layout/scrolling.rs` (Column) + `/home/leochl/workspace/naru/src/layout/workspace.rs` (Workspace delegation)  
**Changes:**
- Add new action `Action::CycleWindowInTile` (or similar) in naru-ipc/src/lib.rs
- Implement `Column::cycle_window_focus_forward()` and `cycle_window_focus_backward()`
- Wire input handler (src/input/mod.rs) to dispatch these actions
- Each Column tracks "which window is active" per tile

**Complexity:** M (new input action, ~5-10 methods in Column)  
**Risk:** M — Must coordinate focus with keyboard input dispatch

### 3. Move Semantics (Horizontal & Vertical)
**Files:** 
- `/home/leochl/workspace/naru/src/layout/scrolling.rs:1717–1763` (move_left, move_right, move_down, move_up in ScrollingSpace and Column)
- Possibly new state tracking (see open questions)

**Changes:**
- **move_left():** 
  - If active tile has N > 1 windows: extract top window, create new column to the left, insert window into that column
  - If active tile has 1 window: instead of moving column, **move window into left column's active tile (stack merge)**
  - Handle alternation: track "last move direction" state to alternate behavior
- **move_right():** Mirror of move_left
- **move_down():** Similar alternation logic (insert new tile vs. stack merge)
- **move_up():** Mirror of move_down

**Complexity:** L (deep refactor of move logic; new state to track alternation; handle column creation/destruction)  
**Risk:** H — Core layout mechanics; easy to break existing behavior. Alternation state lifetime is ambiguous (see E).

### 4. Empty Column Cleanup
**File:** `/home/leochl/workspace/naru/src/layout/scrolling.rs:1058–1061, 1162–1182`  
**Changes:**
- After moves that extract windows from stacks, if a tile becomes empty (0 windows), remove the tile from its column
- If a column becomes empty (0 tiles), remove the column (already exists)
- Update `active_tile_idx`, `active_column_idx` as needed

**Complexity:** S (extend existing removal logic)  
**Risk:** M — Easy to miss edge cases when both tile and column become empty in one operation

### 5. Rendering & Z-Ordering
**Files:** `/home/leochl/workspace/naru/src/layout/tile.rs` (render methods), `/home/leochl/workspace/naru/src/layout/scrolling.rs` (Column rendering)  
**Changes:**
- Tile's render methods: only render `windows[active_window_idx]`; hide others
- If visual stack indicator is needed (e.g., tabs, count badge), update TabIndicator or add StackIndicator

**Complexity:** S (render already assumes 1 window; change to index into Vec)  
**Risk:** L — Rendering is delicate; wrong Z-order or clipping could show/hide wrong windows

### 6. IPC / Window Layout Reporting
**File:** `/home/leochl/workspace/naru/naru-ipc/src/lib.rs:1367` (WindowLayout struct)  
**Changes:**
- `WindowLayout` currently reports `pos_in_scrolling_layout: Option<(usize, usize)>` as (column index, tile index)
- **Ambiguity:** Should this now be (column, tile, window-in-tile)? Or just report the **visible** window?
- Update serialization if we add more detail

**Complexity:** S (small struct addition or clarification)  
**Risk:** M — Breaking change if clients depend on the current tuple structure

### 7. Tests & Snapshots
**File:** `/home/leochl/workspace/naru/src/layout/tests.rs` and `/home/leochl/workspace/naru/src/tests/snapshots/`  
**Changes:**
- Add property tests for stack-of-one (existing behavior must still work)
- Add tests for push/pop window into stack
- Add tests for alternating move semantics
- Update property-based test Op enum (already has Move* variants at line 4750)
- Generate new snapshots for stacking scenarios

**Complexity:** M (40–60 test cases; many snapshots)  
**Risk:** M — Easy to miss edge cases; snapshots must be manually reviewed

### 8. Config & Keybindings
**File:** Likely `naru-config/` (if it exists) and `default.ron` or equivalent config  
**Changes:**
- Add keybinding for `CycleWindowInTile` (or rename from placeholder)
- Document new behavior in config schema
- May need config option to toggle stacking on/off (backward compat)

**Complexity:** S  
**Risk:** L (config schema changes need to be versioned/compatible)

### 9. Transaction / Serialization
**File:** `/home/leochl/workspace/naru/src/utils/transaction.rs` and potentially others  
**Changes:**
- If layout state is serialized (e.g., for suspend/resume), update serialization to include per-tile window stacks

**Complexity:** M  
**Risk:** H — Old serialized state won't deserialize; must handle migration

---

## D. Concrete Implementation Phases

Each phase should compile, run existing tests, and not break single-window behavior.

### Phase 1: Data Model Foundation
**Objective:** Tile holds Vec<Window>, but always has exactly 1 window (stack-of-one).  
**Changes:**
1. Change `Tile::window: W` → `Tile::windows: Vec<W>` + `Tile::active_window_idx: usize`
2. Add constructor `Tile::new_with_window(w) -> Self` that wraps single window in Vec
3. Update all read accessors: `window()`, `window_mut()` to use `windows[active_window_idx]`
4. Update mutation accessors: ensure they maintain the invariant `windows.len() > 0`
5. Fix compilation errors throughout codebase (expect ~100-150 compiler errors initially)
6. **Test:** All existing tests pass; single-window behavior unchanged

**Files affected:** `tile.rs`, `scrolling.rs`, `workspace.rs`, all layout-related files  
**Estimated effort:** 4–6 hours (mostly mechanical)  
**Risk:** M (large refactor, easy to miss accessor calls)

### Phase 2: Focus Cycling Within Tile
**Objective:** Able to cycle focus between windows in the same tile.  
**Changes:**
1. Add `Action::CycleWindowInTile(direction: Direction)` to `naru-ipc/src/lib.rs` (or `FocusWindowInTile`)
2. Implement `Workspace::cycle_window_focus(direction)` → `ScrollingSpace::cycle_window_focus(col_idx, direction)` → `Column::cycle_active_window(tile_idx, direction)`
3. In `Column::cycle_active_window()`:
   - If tile has 1 window: no-op
   - If tile has N > 1: rotate `active_window_idx` forward/backward with wrapping
4. Wire input handler (`src/input/mod.rs`) to dispatch the action
5. **Test:** Focus cycles through windows; move/focus actions still work on single-window tiles

**Files affected:** `naru-ipc/src/lib.rs`, `src/layout/workspace.rs`, `src/layout/scrolling.rs`, `src/input/mod.rs`  
**Estimated effort:** 2–3 hours  
**Risk:** L (isolated feature, doesn't change move semantics yet)

### Phase 3: Window Stack Manipulation (Push/Pop)
**Objective:** Can manually push a window into a neighbor's stack, or pop out.  
**Changes:**
1. Add actions: `Action::PushWindowIntoLeftTile`, `PushWindowIntoRightTile`, `PushWindowIntoUpper`, `PushWindowIntoLower` (or combined with move)
2. Implement core stack ops in `ScrollingSpace`:
   - `push_window_into_adjacent_tile(source_col, source_tile, direction)` — move window from source tile into adjacent column/tile's stack
   - `pop_window_from_tile_at(col, tile) -> Option<W>` — extract window, return it (for insertion elsewhere)
3. Implement removal of empty tiles after push/pop
4. Wire input handlers
5. **Test:** Windows can move into stacks; empty tiles are cleaned up; focus follows moved window

**Files affected:** `scrolling.rs`, `workspace.rs`, `input/mod.rs`, `naru-ipc/src/lib.rs`  
**Estimated effort:** 3–4 hours  
**Risk:** M (state transitions; empty cleanup must be correct)

### Phase 4: Alternating Move Semantics
**Objective:** Implement the alternating new-column / overlap-neighbor behavior.  
**Changes:**
1. **State tracking:** Decide where alternation state lives (per workspace? per tile? per window?). ⚠ **See Open Question E1.**
2. Refactor `ScrollingSpace::move_left()` and `move_right()`:
   - If active tile has N > 1 windows:
     - If alternation state = "next move creates column": extract window, create new column, insert
     - If alternation state = "next move overlaps neighbor": move window into neighbor's stack
     - Toggle state
   - If active tile has 1 window: (same logic)
3. Similarly for `Column::move_up()` and `move_down()`
4. Update empty tile/column cleanup to run after each move
5. **Test:** Move left-left produces column-then-overlap; move left-right toggles back; edge at workspace bounds still handled

**Files affected:** `scrolling.rs`, `workspace.rs`  
**Estimated effort:** 4–5 hours  
**Risk:** H (alternation state lifetime is complex; easy to reset at wrong time)

### Phase 5: Workspace-Wrap Edge Handling
**Objective:** When alternating move would go past edge, move to next workspace.  
**Changes:**
1. Extend `MoveWindowLeftOrToMonitorLeft` actions (already exist for columns at line 952)
2. Adapt to new alternating semantics: if move-left would wrap edge AND alternation state is "create column", move to workspace left; otherwise overlap in current workspace
3. Test workspace wrapping doesn't break alternation state

**Files affected:** `input/mod.rs`, `workspace.rs`, `mod.rs` (layout wrapper)  
**Estimated effort:** 2–3 hours  
**Risk:** M (must coordinate with workspace switching)

### Phase 6: IPC & Serialization
**Objective:** Stacking state can be queried and persisted.  
**Changes:**
1. Update `WindowLayout` struct in `naru-ipc/src/lib.rs` to report stacking info (or leave as is if IPC only reports visible window)
2. Add serialization for per-tile window stacks (if using serde snapshots)
3. Update migration logic for old snapshots

**Files affected:** `naru-ipc/src/lib.rs`, potentially `src/utils/transaction.rs`, snapshot code  
**Estimated effort:** 2–3 hours  
**Risk:** M (deserialization compatibility)

### Phase 7: Tests, Snapshots, Docs
**Objective:** All move/focus scenarios covered; docs updated.  
**Changes:**
1. Write property-based tests in `src/layout/tests.rs` for:
   - Stack-of-one unchanged (Phase 1 validation)
   - Cycle focus (Phase 2)
   - Push/pop (Phase 3)
   - Alternating moves (Phase 4)
   - Workspace wrapping (Phase 5)
2. Generate and review snapshots
3. Update CHANGELOG, docs, config schema

**Files affected:** `src/layout/tests.rs`, docs/, snapshots/, Cargo.toml (if adding feature flag)  
**Estimated effort:** 4–6 hours (snapshot review is manual)  
**Risk:** L (testing is defensive)

---

## E. Open Questions for User

Before implementation, clarify:

### E1. Alternation State Lifetime
**Question:** When does the "alternate" counter reset? 
- **Per-workspace?** (shared across all tiles in the workspace)
- **Per-tile?** (each tile has its own move-sequence state)
- **Per-window?** (each window remembers its last move direction)
- **Per-move-direction?** (separate counter for left/right vs. up/down)

**Impact:** Determines where state is stored and updated. If per-workspace, a single field in `ScrollingSpace` suffices. If per-tile, we need a field in `TileData`. This affects synchronization and reset logic.

**Blocker?** Yes — must decide before Phase 4.

### E2. State Reset Triggers
**Question:** When does the alternation counter reset to "next move creates column"?
- When focus moves to a different tile?
- When focus moves to a different column?
- When the move direction changes (left → right)?
- Never (counter persists across all operations)?
- Only when user explicitly moves focus away from the current tile?

**Example scenarios:**
- User presses move-left twice (should: create column, then overlap)
- User presses move-left, then clicks on a different window, then move-left again
- User presses move-left, then move-right

**Impact:** Affects how intuitive the behavior feels; wrong reset logic breaks the alternating pattern.

### E3. MoveColumn vs. MoveWindow Distinction
**Question:** Should the existing `MoveColumnLeft`/`MoveColumnRight` actions keep their meaning (move entire column as a unit), or should they be **repurposed** to mean "move window left/right"?

**Current behavior:** `MoveColumnLeft` moves the entire column left in the column list (swaps positions).  
**New behavior option 1:** Keep `MoveColumnLeft` as-is; only `MoveWindowLeft` (if we add it) uses alternating semantics.  
**New behavior option 2:** `MoveColumnLeft` becomes an alias for "move active window left with alternation."

**Impact:** If option 2, we must deprecate the old column-move behavior or provide a different keybinding.

### E4. Single-Window Tile Behavior
**Question:** Should a single-window tile behave **differently** from a multi-window tile?
- **Option A:** Single-window tiles use OLD behavior (move window to neighbor column immediately), multi-window tiles use NEW alternating behavior
- **Option B:** All tiles use NEW alternating behavior uniformly
- **Option C:** Single-window tiles are "upgraded" to multi-window on first stack operation, then both use NEW behavior

**Impact:** Backward compatibility vs. consistency. Option A keeps single-window users unaffected but creates two code paths.

### E5. Visual Indication of Stacks
**Question:** How should a stacked tile be visually distinguished?
- **Tabs:** Show window titles as tabs above/below the tile (already supported via `ColumnDisplay::Tabbed`)
- **Count badge:** Show "3/5" (window 3 of 5) in corner
- **Nothing:** Only visible when cycling focus
- **Color:** Highlight border if tile has >1 window

**Impact:** Affects rendering (Phase 5) and user clarity.

### E6. Config / Feature Flag
**Question:** Should stacking be:
- Always enabled (breaking change for existing configs)
- Behind a feature flag (default on/off?)
- Configurable per-keybinding (bind alternating moves separately from column moves)

**Impact:** Config schema, backward compatibility, testing.

---

## Summary Table

| Phase | Focus | Complexity | Risk | Duration | Blocker? |
|-------|-------|------------|------|----------|----------|
| 1 | Vec<Window> foundation | M | M | 4–6h | No |
| 2 | Focus cycle | M | L | 2–3h | No |
| 3 | Push/pop stacks | M | M | 3–4h | No |
| 4 | Alternating moves | L | H | 4–5h | **E1, E2** |
| 5 | Workspace wrap | S | M | 2–3h | No |
| 6 | IPC/serialization | M | M | 2–3h | No |
| 7 | Tests/docs | M | L | 4–6h | No |
| **Total** | | | | **21–30h** | |

---

## Top 3 Risks

1. **Alternation state management (Phase 4):** The most complex design decision. Wrong choice for when/how to reset will make the feature feel unintuitive or buggy. Requires careful thought and user testing.

2. **Tile accessor refactor (Phase 1):** Tile is accessed in 50+ places. Mechanical refactoring has high error rate. Risk of subtle bugs where a method still assumes 1 window (e.g., a layout calculation using tile size without considering window count).

3. **Empty tile/column cleanup (Phases 3–4):** After each move, we must remove empty tiles and columns. Forgetting to clean up in one code path breaks layout invariants and causes crashes or display bugs. Needs thorough testing and careful audit.

---

## Top 3 Open Questions Blocking Implementation

1. **E1: Alternation state lifetime** — Must decide per-workspace vs. per-tile vs. per-window before coding Phase 4.

2. **E2: State reset triggers** — How the counter resets affects user experience and must be understood before coding behavior.

3. **E3: MoveColumn vs. MoveWindow distinction** — Will the existing `MoveColumn*` actions change meaning, or do we need separate `MoveWindow*` actions? This determines API stability.

