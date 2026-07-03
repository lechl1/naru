use std::collections::hash_map::Entry;

use naru_ipc::PositionChange;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
use smithay::reexports::calloop::Interest;
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface, remove_pre_commit_hook,
    with_states, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes,
};
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::wayland::shell::xdg::ToplevelCachedState;
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{delegate_compositor, delegate_shm};

use super::xdg_shell::add_mapped_toplevel_pre_commit_hook;
use crate::handlers::XDG_ACTIVATION_TOKEN_TIMEOUT;
use crate::layout::{ActivateWindow, AddWindowTarget, LayoutElement as _};
use crate::naru::{CastTarget, ClientState, LockState, State};
use crate::utils::transaction::Transaction;
use crate::utils::{is_mapped, send_scale_transform, with_toplevel_role};
use crate::window::{InitialConfigureState, Mapped, ResolvedWindowRules, Unmapped};

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.naru.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn new_subsurface(&mut self, surface: &WlSurface, parent: &WlSurface) {
        let mut root = parent.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }

        if let Some(output) = self.naru.output_for_root(&root) {
            let scale = output.current_scale();
            let transform = output.current_transform();
            with_states(surface, |data| {
                send_scale_transform(surface, data, scale, transform);
            });
        }
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        self.add_default_dmabuf_pre_commit_hook(surface);
    }

    fn commit(&mut self, surface: &WlSurface) {
        let _span = tracy_client::span!("CompositorHandler::commit");
        let _span = trace_span!("commit", surface = %surface.id()).entered();
        trace!("commit");

        on_commit_buffer_handler::<Self>(surface);
        self.backend.early_import(surface);

        let mut root_surface = surface.clone();
        while let Some(parent) = get_parent(&root_surface) {
            root_surface = parent;
        }

        // Update the cached root surface.
        self.naru
            .root_surface
            .insert(surface.clone(), root_surface.clone());

        if is_sync_subsurface(surface) {
            return;
        }

        if surface == &root_surface {
            // This is a root surface commit. It might have mapped a previously-unmapped toplevel.
            if let Entry::Occupied(entry) = self.naru.unmapped_windows.entry(surface.clone()) {
                if is_mapped(surface) {
                    // The toplevel got mapped.
                    let Unmapped {
                        window,
                        state,
                        activation_token_data,
                    } = entry.remove();

                    window.on_commit();

                    let toplevel = window.toplevel().expect("no X11 support");

                    let (
                        rules,
                        width,
                        height,
                        is_full_width,
                        output,
                        workspace_id,
                        is_pending_maximized,
                    ) = if let InitialConfigureState::Configured {
                        rules,
                        width,
                        height,
                        floating_width: _,
                        floating_height: _,
                        is_full_width,
                        output,
                        workspace_name,
                        is_pending_maximized,
                    } = state
                    {
                        // Check that the output is still connected.
                        let output =
                            output.filter(|o| self.naru.layout.monitor_for_output(o).is_some());

                        // Check that the workspace still exists.
                        let workspace_id = workspace_name
                            .as_deref()
                            .and_then(|n| self.naru.layout.find_workspace_by_name(n))
                            .map(|(_, ws)| ws.id());

                        (
                            rules,
                            width,
                            height,
                            is_full_width,
                            output,
                            workspace_id,
                            is_pending_maximized,
                        )
                    } else {
                        // Can happen when a surface unmaps by attaching a null buffer while
                        // there are in-flight pending configures.
                        debug!("window mapped without proper initial configure");
                        (
                            ResolvedWindowRules::default(),
                            None,
                            None,
                            false,
                            None,
                            None,
                            false,
                        )
                    };

                    // The GTK about dialog sets min/max size after the initial configure but
                    // before mapping, so we need to compute open_floating at the last possible
                    // moment, that is here.
                    //
                    // Same-app auto-floating: look for an existing tile with the same xdg
                    // app_id in the workspace this window is about to land in (falling back
                    // to the active workspace if no explicit target was recorded). When such
                    // a tile exists, `compute_open_floating` floats this one by default —
                    // unless the rule pinned `open-floating` either way.
                    let same_app = with_toplevel_role(toplevel, |r| r.app_id.clone())
                        .and_then(|app| {
                            let ws = workspace_id
                                .and_then(|id| {
                                    self.naru
                                        .layout
                                        .workspaces()
                                        .find(|(_, _, ws)| ws.id() == id)
                                        .map(|(_, _, ws)| ws)
                                })
                                .or_else(|| self.naru.layout.active_workspace());
                            ws.map(|ws| ws.has_window_with_app_id(&app))
                        })
                        .unwrap_or(false);
                    let mut is_floating = rules.compute_open_floating(toplevel, same_app);
                    // PWAs (Chromium site-specific browsers) should open tiled as a
                    // new column, not floating — a PWA otherwise auto-floats as a
                    // fixed-size or same-app secondary window. Only override the
                    // *auto* decision (an explicit `open-floating` window rule still
                    // wins), and only pay for the `.desktop` scan when a window would
                    // actually float, which is the uncommon case.
                    if is_floating && rules.open_floating.is_none() {
                        if let Some(app_id) = with_toplevel_role(toplevel, |r| r.app_id.clone()) {
                            let index = crate::session::index_startup_wm_classes();
                            if crate::session::is_pwa(&app_id, &index) {
                                is_floating = false;
                            }
                        }
                    }

                    // Figure out if we should activate the window.
                    let activate = rules.open_focused.map(|focus| {
                        if focus {
                            ActivateWindow::Yes
                        } else {
                            ActivateWindow::No
                        }
                    });
                    let activate = activate.unwrap_or_else(|| {
                        // Check the token timestamp again in case the window took a while between
                        // requesting activation and mapping.
                        let token = activation_token_data.filter(|token| {
                            token.timestamp.elapsed() < XDG_ACTIVATION_TOKEN_TIMEOUT
                        });
                        if token.is_some() {
                            ActivateWindow::Yes
                        } else {
                            let config = self.naru.config.borrow();
                            if config.debug.strict_new_window_focus_policy {
                                ActivateWindow::No
                            } else {
                                ActivateWindow::Smart
                            }
                        }
                    });

                    let parent = toplevel
                        .parent()
                        .and_then(|parent| self.naru.layout.find_window_and_output(&parent))
                        // Only consider the parent if we configured the window for the same
                        // output.
                        //
                        // Normally when we're following the parent, the configured output will be
                        // None. If the configured output is set, that means it was set explicitly
                        // by a window rule or a fullscreen request.
                        .filter(|(_, parent_output)| {
                            parent_output.is_none()
                                || output.is_none()
                                || output.as_ref() == *parent_output
                        })
                        .map(|(mapped, _)| mapped.window.clone());

                    // The mapped pre-commit hook deals with dma-bufs on its own.
                    self.remove_default_dmabuf_pre_commit_hook(surface);
                    let hook = add_mapped_toplevel_pre_commit_hook(toplevel);
                    let mut mapped = {
                        let config = self.naru.config.borrow();
                        Mapped::new(window, rules, hook, &config)
                    };
                    let window = mapped.window.clone();

                    let target = if let Some(p) = &parent {
                        // Open dialogs next to their parent window.
                        AddWindowTarget::NextTo(p)
                    } else if let Some(id) = workspace_id {
                        AddWindowTarget::Workspace(id)
                    } else if let Some(output) = &output {
                        AddWindowTarget::Output(output)
                    } else {
                        AddWindowTarget::Auto
                    };
                    // Session-restore: try to match this newly-mapping window
                    // against a saved entry from the prior session. Match on cwd and
                    // title as well as app_id: the restored process was spawned in its
                    // saved directory (distinguishes terminals), and the title
                    // distinguishes same-app GUI windows that share a cwd. The title may
                    // still be settling this early, so a mismatch just falls back to the
                    // cwd/FIFO keys — the deferred reconcile fixes ordering once titles
                    // land.
                    let saved_entry = mapped.app_id().and_then(|id| {
                        let cwd = mapped.session_cwd();
                        let title = with_toplevel_role(mapped.toplevel(), |r| r.title.clone());
                        self.naru
                            .session_manager
                            .as_mut()
                            .and_then(|sm| sm.take_pending_for(&id, cwd, title.as_deref()))
                    });

                    // Capture the saved placement up front: `mapped` is consumed
                    // by `add_window` below, and we need the placement both before
                    // (to steer side-panel routing) and after (to restore size).
                    let saved_placement =
                        saved_entry.as_ref().map(|e| e.placement.clone());

                    // Side-panel restore: pin the respawned window to the panel it
                    // last occupied via the same `open-in-fixed-side` path that
                    // window-rules use. Placement reads this when the window maps.
                    if let Some(crate::session::Placement::SidePanel { side, .. }) =
                        &saved_placement
                    {
                        let side = match side {
                            crate::session::PanelSide::Left => naru_ipc::OpenInFixedSide::Left,
                            crate::session::PanelSide::Right => naru_ipc::OpenInFixedSide::Right,
                        };
                        mapped.set_open_in_fixed_side(Some(side));
                    }

                    // Tiled restore: tag the window with its saved (column, tile) slot so
                    // the post-add placement can position it *relative* to the other
                    // restored windows — deterministic regardless of the order clients
                    // happen to map in. Also force activation, because the placement uses
                    // `move_column_to_index`, which acts on the active column.
                    let activate = if let Some(crate::session::Placement::Tiled {
                        column_index,
                        tile_index,
                        ..
                    }) = &saved_placement
                    {
                        mapped.set_restore_pos(Some((*column_index, *tile_index)));
                        ActivateWindow::Yes
                    } else {
                        activate
                    };

                    // Route the restored window back to where it was.
                    //
                    // Fixed-side panels are monitor-owned, not workspace-scoped, and
                    // the panel routing (`open-in-fixed-side`, set above) only fires
                    // on an Auto/output target — a *Workspace* target instead drops
                    // the window into that workspace's carousel, which is why a saved
                    // side-panel PWA was coming back in the carousel. So a side-panel
                    // entry steers to its saved *output* (which resolves to Auto on
                    // that monitor) and lets the routing park it in the panel.
                    //
                    // Everything else routes to its saved workspace: named workspaces
                    // match by name; index-only entries are *materialized*
                    // (`materialize_workspace_id_at` creates the workspace at that
                    // per-output index, and any below it, if missing) so a window
                    // saved on workspace N lands there instead of piling onto the
                    // active workspace. Placeholders are protected from compaction
                    // until restore settles, so out-of-order maps still land right.
                    let is_side_panel = matches!(
                        &saved_placement,
                        Some(crate::session::Placement::SidePanel { .. })
                    );
                    // Cloned so the `&Output` doesn't borrow `self.naru` across the
                    // `add_window` call below.
                    let panel_output = if is_side_panel {
                        saved_entry
                            .as_ref()
                            .and_then(|e| e.output.as_deref())
                            .and_then(|name| self.naru.output_by_name_match(name))
                            .cloned()
                    } else {
                        None
                    };
                    let target = if is_side_panel {
                        panel_output
                            .as_ref()
                            .map(AddWindowTarget::Output)
                            .unwrap_or(AddWindowTarget::Auto)
                    } else {
                        match saved_entry.as_ref().and_then(|e| match &e.workspace {
                            crate::session::WorkspaceRef::Name { name } => self
                                .naru
                                .layout
                                .find_workspace_by_name(name)
                                .map(|(_, ws)| ws.id()),
                            crate::session::WorkspaceRef::Index { index } => self
                                .naru
                                .layout
                                .materialize_workspace_id_at(e.output.as_deref(), *index),
                        }) {
                            Some(ws_id) => AddWindowTarget::Workspace(ws_id),
                            None => target,
                        }
                    };

                    // If this was the last pending restore entry, leave restore
                    // mode so the placeholder-protection lifts and any never-filled
                    // workspaces are reclaimed. (A bounded settle timer is the
                    // backstop when some saved windows never reappear.)
                    let restore_drained = saved_entry.is_some()
                        && self
                            .naru
                            .session_manager
                            .as_ref()
                            .is_some_and(|sm| sm.pending_restore.is_empty());

                    // Floating-vs-tiled override.
                    let is_floating = match &saved_entry {
                        Some(e) => matches!(
                            e.placement,
                            crate::session::Placement::Floating { .. }
                        ),
                        None => is_floating,
                    };

                    let output = self.naru.layout.add_window(
                        mapped,
                        target,
                        width,
                        height,
                        is_full_width,
                        is_floating,
                        activate,
                    );
                    let output = output.cloned();

                    // Post-add placement restore: column index, then exact size.
                    match &saved_placement {
                        Some(crate::session::Placement::Tiled {
                            column_index,
                            tile_index,
                            width,
                            height,
                            ..
                        }) => {
                            // Deterministic relative placement: count the restored
                            // windows already on this window's workspace whose saved
                            // (column, tile) sorts before ours, and move our column to
                            // that 1-based slot. This is independent of the order clients
                            // map in — an *absolute* saved index races, because a window
                            // saved at column 5 can't land there until the earlier
                            // columns exist (it would clamp to the rightmost slot and the
                            // final order would reflect launch timing, not the layout).
                            //
                            // `activate` was forced to `Yes` above for tiled restores, so
                            // `move_column_to_index` (which acts on the active column)
                            // targets the just-added window. Our own window carries the
                            // same `restore_pos` but sorts equal, not before, so it is
                            // never counted.
                            let key = (*column_index, *tile_index);
                            let before = output
                                .as_ref()
                                .and_then(|out| self.naru.layout.monitor_for_output(out))
                                .map(|mon| {
                                    mon.active_workspace_ref()
                                        .windows()
                                        .filter(|w| w.restore_pos().is_some_and(|p| p < key))
                                        .count()
                                })
                                .unwrap_or(0);
                            self.naru.layout.move_column_to_index(before + 1);
                            self.restore_window_size(&window, *width, *height);
                        }
                        Some(crate::session::Placement::SidePanel { width, height, .. }) => {
                            // The window is already parked in its strip via the
                            // open-in-fixed-side override set before add_window.
                            self.restore_window_size(&window, *width, *height);
                        }
                        Some(crate::session::Placement::Floating { .. }) | None => {}
                    }

                    // Last restored window placed: lift placeholder-workspace
                    // protection and reclaim any workspaces that stayed empty,
                    // then rebuild any multi-window columns now that every
                    // restored window is present and positioned.
                    if restore_drained {
                        self.naru.layout.end_session_restore();
                        self.stack_restored_columns();
                        // Every restored window is now placed and activated in map
                        // order; return focus to the workspace the user left active.
                        self.naru.restore_active_workspace();
                    }

                    // Session-restore: window appearing changes what we'd persist.
                    self.naru.session_mark_dirty();

                    // The window state cannot contain Fullscreen and Maximized at once. Therefore,
                    // if the window ended up fullscreen, then we only know that it is also
                    // maximized from the is_pending_maximized variable. Tell the layout about it
                    // here so that unfullscreening the window makes it maximized.
                    if let Some((mapped, _)) = self.naru.layout.find_window_and_output(surface) {
                        if mapped.pending_sizing_mode().is_fullscreen() && is_pending_maximized {
                            self.naru.layout.set_maximized(&window, true);
                        }
                    } else {
                        error!("layout is missing the window that we just added");
                    }

                    if let Some(output) = output {
                        self.naru.layout.start_open_animation_for_window(&window);

                        let new_focus = self.naru.layout.focus().map(|m| &m.window);
                        if new_focus == Some(&window) {
                            // We activated the newly opened window.
                            self.maybe_warp_cursor_to_focus();
                            self.naru.layer_shell_on_demand_focus = None;
                        }

                        self.naru.queue_redraw(&output);
                    }
                    return;
                }

                // The toplevel remains unmapped.
                trace!("toplevel remains unmapped");
                let unmapped = entry.get();
                if unmapped.needs_initial_configure() {
                    let toplevel = unmapped.window.toplevel().expect("no x11 support").clone();
                    self.queue_initial_configure(toplevel);
                }
                return;
            }

            // This is a commit of a previously-mapped root or a non-toplevel root.
            if let Some((mapped, output)) = self.naru.layout.find_window_and_output(surface) {
                let window = mapped.window.clone();
                let output = output.cloned();

                let id = mapped.id();

                // This is a commit of a previously-mapped toplevel.
                let is_mapped = is_mapped(surface);

                // Must start the close animation before window.on_commit().
                let transaction = Transaction::new();
                if !is_mapped {
                    let blocker = transaction.blocker();
                    self.backend.with_primary_renderer(|renderer| {
                        self.naru
                            .layout
                            .start_close_animation_for_window(renderer, &window, blocker);
                    });
                }

                window.on_commit();

                if !is_mapped {
                    // The toplevel got unmapped.
                    //
                    // Test client: wleird-unmap.
                    trace!("toplevel got unmapped");

                    let active_window = self.naru.layout.focus().map(|m| &m.window);
                    let was_active = active_window == Some(&window);

                    self.naru
                        .stop_casts_for_target(CastTarget::Window { id: id.get() });

                    self.naru.window_mru_ui.remove_window(id);
                    self.naru.layout.remove_window(&window, transaction.clone());
                    // Session-restore: window vanishing changes what we'd persist.
                    self.naru.session_mark_dirty();
                    self.add_default_dmabuf_pre_commit_hook(surface);

                    // If this is the only instance, then this transaction will complete
                    // immediately, so no need to set the timer.
                    if !transaction.is_last() {
                        transaction.register_deadline_timer(&self.naru.event_loop);
                    }

                    if was_active {
                        self.maybe_warp_cursor_to_focus();
                    }

                    // Newly-unmapped toplevels must perform the initial commit-configure sequence
                    // afresh.
                    let unmapped = Unmapped::new(window);
                    self.naru.unmapped_windows.insert(surface.clone(), unmapped);

                    if let Some(output) = output {
                        self.naru.queue_redraw(&output);
                        self.naru.queue_redraw_mru_output();
                    }
                    return;
                }

                let (serial, buffer_delta) = with_states(surface, |states| {
                    let buffer_delta = states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .buffer_delta
                        .take();

                    let serial = states
                        .cached_state
                        .get::<ToplevelCachedState>()
                        .current()
                        .last_acked
                        .as_ref()
                        .map(|c| c.serial);
                    (serial, buffer_delta)
                });
                if serial.is_none() {
                    error!("commit on a mapped surface without a configured serial");
                }

                // The toplevel remains mapped.
                self.naru.window_mru_ui.update_window(&self.naru.layout, id);
                self.naru.layout.update_window(&window, serial);

                // Move the toplevel according to the attach offset.
                if let Some(delta) = buffer_delta {
                    if delta.x != 0 || delta.y != 0 {
                        let (x, y) = delta.to_f64().into();
                        self.naru.layout.move_floating_window(
                            Some(&window),
                            PositionChange::AdjustFixed(x),
                            PositionChange::AdjustFixed(y),
                            false,
                        );
                    }
                }

                // Popup placement depends on window size which might have changed.
                self.update_reactive_popups(&window);

                if let Some(output) = output {
                    self.naru.queue_redraw(&output);
                    self.naru.queue_redraw_mru_output();
                }
                return;
            }

            // This is a commit of a non-toplevel root.
        }

        // This is a commit of a non-root or a non-toplevel root.
        let root_window_output = self.naru.layout.find_window_and_output(&root_surface);
        if let Some((mapped, output)) = root_window_output {
            let window = mapped.window.clone();
            let output = output.cloned();
            window.on_commit();
            self.naru
                .window_mru_ui
                .update_window(&self.naru.layout, mapped.id());
            self.naru.layout.update_window(&window, None);
            if let Some(output) = output {
                self.naru.queue_redraw(&output);
                self.naru.queue_redraw_mru_output();
            }
            return;
        }

        // This might be a popup.
        self.popups_handle_commit(surface);
        if let Some(popup) = self.naru.popups.find_popup(surface) {
            if let Some(output) = self.output_for_popup(&popup) {
                self.naru.queue_redraw(&output.clone());
            }
            return;
        }

        // This might be a layer-shell surface.
        if self.layer_shell_handle_commit(surface) {
            return;
        }

        // This might be a cursor surface.
        if matches!(
            &self.naru.cursor_manager.cursor_image(),
            CursorImageStatus::Surface(s) if s == &root_surface
        ) {
            // In case the cursor surface has been committed handle the role specific
            // buffer offset by applying the offset on the cursor image hotspot
            if surface == &root_surface {
                with_states(surface, |states| {
                    let cursor_image_attributes = states.data_map.get::<CursorImageSurfaceData>();

                    if let Some(mut cursor_image_attributes) =
                        cursor_image_attributes.map(|attrs| attrs.lock().unwrap())
                    {
                        let buffer_delta = states
                            .cached_state
                            .get::<SurfaceAttributes>()
                            .current()
                            .buffer_delta
                            .take();
                        if let Some(buffer_delta) = buffer_delta {
                            cursor_image_attributes.hotspot -= buffer_delta;
                        }
                    }
                });
            }

            // FIXME: granular redraws for cursors.
            self.naru.queue_redraw_all();
            return;
        }

        // This might be a DnD icon surface.
        if matches!(&self.naru.dnd_icon, Some(icon) if icon.surface == root_surface) {
            let dnd_icon = self.naru.dnd_icon.as_mut().unwrap();

            // In case the dnd surface has been committed handle the role specific
            // buffer offset by applying the offset on the dnd icon offset
            if surface == &dnd_icon.surface {
                with_states(&dnd_icon.surface, |states| {
                    let buffer_delta = states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .buffer_delta
                        .take()
                        .unwrap_or_default();
                    dnd_icon.offset += buffer_delta;
                });
            }

            // FIXME: granular redraws for cursors.
            self.naru.queue_redraw_all();
            return;
        }

        // This might be a lock surface.
        for (output, state) in &self.naru.output_state {
            if let Some(lock_surface) = &state.lock_surface {
                if lock_surface.wl_surface() == &root_surface {
                    if matches!(self.naru.lock_state, LockState::WaitingForSurfaces { .. }) {
                        self.naru.maybe_continue_to_locking();
                    } else {
                        self.naru.queue_redraw(&output.clone());
                    }

                    return;
                }
            }
        }

        // This message can trigger for lock surfaces that had a commit right after we unlocked
        // the session, but that's ok, we don't need to handle them.
        trace!("commit on an unrecognized surface: {surface:?}, root: {root_surface:?}");
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        // Clients may destroy their subsurfaces before the main surface. Ensure we have a snapshot
        // when that happens, so that the closing animation includes all these subsurfaces.
        //
        // Test client: alacritty with CSD <= 0.13 (it was fixed in winit afterwards:
        // https://github.com/rust-windowing/winit/pull/3625).
        //
        // This is still not perfect, as this function is called already after the (first)
        // subsurface is destroyed; in the case of alacritty, this is the top CSD shadow. But, it
        // gets most of the job done.
        if let Some(root) = self.naru.root_surface.get(surface) {
            if let Some((mapped, output)) = self.naru.layout.find_window_and_output(root) {
                let window = mapped.window.clone();
                let output = output.cloned();
                self.store_unmap_snapshot(&window, output.as_ref());
            }
        }

        self.naru
            .root_surface
            .retain(|k, v| k != surface && v != surface);

        // The object destruction order is not guaranteed to follow the logical role order. So for
        // example when a client disconnects unexpectedly, WlSurface::destroyed() may be called
        // before XdgShellHandler::toplevel_destroyed(). In this case, the surface will *not* have
        // the default dmabuf pre-commit hook: it will still have the toplevel pre-commit hook.
        //
        // So, this may come out empty, and then the toplevel pre-commit hook will be removed in the
        // subsequent toplevel_destroyed() call.
        if let Some(hook) = self.naru.dmabuf_pre_commit_hook.remove(surface) {
            remove_pre_commit_hook(surface, &hook);
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.naru.shm_state
    }
}

delegate_compositor!(State);
delegate_shm!(State);

impl State {
    /// Restore a window's saved logical size (session-restore). A zero dimension
    /// means "wasn't captured / let the layout decide" and is skipped. Works for
    /// both carousel and fixed-side windows — `set_window_*` routes by the
    /// window's current layer.
    fn restore_window_size(
        &mut self,
        id: &smithay::desktop::Window,
        width: f64,
        height: f64,
    ) {
        if width > 0.0 {
            self.naru
                .layout
                .set_window_width(Some(id), naru_ipc::SizeChange::SetFixed(width.round() as i32));
        }
        if height > 0.0 {
            self.naru
                .layout
                .set_window_height(Some(id), naru_ipc::SizeChange::SetFixed(height.round() as i32));
        }
    }

    /// Rebuild the multi-window columns of a restored session. Each restored
    /// window was added as its own single-window column and positioned in saved
    /// `(column, tile)` order, so windows that shared a saved column sit in
    /// adjacent columns sorted by tile. Consuming every non-leader into its left
    /// neighbour — walking each workspace's restored windows in `(column, tile)`
    /// order — collapses each run back into one column, with the tiles in their
    /// saved order (consume appends to the bottom of the target column).
    ///
    /// Called once, after the final restored window has mapped and been placed.
    /// Window heights are intentionally not restored (see `snapshot.rs`), so the
    /// rebuilt columns split their height evenly.
    fn stack_restored_columns(&mut self) {
        // Collect the consume targets first (immutable borrow of the layout),
        // then apply them (mutable). Grouping is per-workspace so two windows
        // that shared a saved column index on *different* workspaces are never
        // merged together.
        let mut to_consume = Vec::new();
        for (_mon, _idx, ws) in self.naru.layout.workspaces() {
            let mut entries: Vec<_> = ws
                .windows()
                .filter_map(|w| w.restore_pos().map(|pos| (w.window.clone(), pos)))
                .collect();
            entries.sort_by_key(|(_, pos)| *pos);
            let mut prev_col: Option<usize> = None;
            for (id, (col, _tile)) in &entries {
                if Some(*col) == prev_col {
                    to_consume.push(id.clone());
                }
                prev_col = Some(*col);
            }
        }
        for id in to_consume {
            self.naru.layout.consume_or_expel_window_left(Some(&id));
        }
    }

    pub fn add_default_dmabuf_pre_commit_hook(&mut self, surface: &WlSurface) {
        if !surface.is_alive() {
            error!("tried to add dmabuf pre-commit hook for a dead surface");
            return;
        }

        let hook = add_pre_commit_hook::<Self, _>(surface, move |state, _dh, surface| {
            let maybe_dmabuf = with_states(surface, |surface_data| {
                surface_data
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                    .and_then(|assignment| match assignment {
                        BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                        _ => None,
                    })
            });
            if let Some(dmabuf) = maybe_dmabuf {
                if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) {
                    if let Some(client) = surface.client() {
                        let res =
                            state
                                .naru
                                .event_loop
                                .insert_source(source, move |_, _, state| {
                                    let display_handle = state.naru.display_handle.clone();
                                    state
                                        .client_compositor_state(&client)
                                        .blocker_cleared(state, &display_handle);
                                    Ok(())
                                });
                        if res.is_ok() {
                            add_blocker(surface, blocker);
                            trace!("added default dmabuf blocker");
                        }
                    }
                }
            }
        });

        let s = surface.clone();
        if let Some(prev) = self.naru.dmabuf_pre_commit_hook.insert(s, hook) {
            error!("tried to add dmabuf pre-commit hook when there was already one");
            remove_pre_commit_hook(surface, &prev);
        }
    }

    pub fn remove_default_dmabuf_pre_commit_hook(&mut self, surface: &WlSurface) {
        if let Some(hook) = self.naru.dmabuf_pre_commit_hook.remove(surface) {
            remove_pre_commit_hook(surface, &hook);
        } else {
            error!("tried to remove dmabuf pre-commit hook but there was none");
        }
    }
}
