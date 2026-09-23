//! App update(): the message-dispatch reducer.

use super::*;

pub(crate) fn __perf_log(line: &str) {
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("sc-win-perf.log"))
    {
        let _ = writeln!(f, "{line}");
    }
}

impl App {
    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        let __label: String = format!("{message:?}").chars().take(60).collect();
        let __t = std::time::Instant::now();
        let __r = self.__update_inner(message);
        let __ms = __t.elapsed().as_millis();
        if __ms >= 3 {
            __perf_log(&format!("update {__ms:>6}ms  {__label}"));
        }
        __r
    }

    pub(crate) fn __update_inner(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::EditorEvent(pane, ev) => {
                // Forward to THAT pane's active buffer — not the focused one — and adopt the
                // widget's own modified flag, so the dirty dot can't drift from the buffer.
                if let Some(tab) = self.panes.get_mut(pane).and_then(|p| p.active_tab_mut()) {
                    // Read before the editor borrow: `editor_mut()` holds `tab` mutably.
                    let was_dirty = tab.dirty;
                    if let Some(editor) = tab.editor_mut() {
                        let task = editor.update(&ev);
                        let now_dirty = editor.is_modified();
                        tab.dirty = now_dirty;
                        // Bumped on any event that could have changed the text. Erring
                        // toward over-counting is deliberate: a version that moves when
                        // the text did not costs a plugin one rejected edit and a
                        // re-read, while one that fails to move lets a stale edit land.
                        if now_dirty || was_dirty {
                            tab.version = tab.version.wrapping_add(1);
                        }
                        // Re-wrap with the SAME id: the editor's own follow-up tasks (a
                        // scroll after paste, say) must come back to the pane that sent them.
                        return task.map(move |ev| Message::EditorEvent(pane, ev));
                    }
                }
            }
            Message::ToggleTabView => self.toggle_tab_view(),
            Message::SaveFile => self.save_active_tab(false),
            Message::DismissSaveConflict => self.save_conflict = None,
            Message::OverwriteOnConflict => {
                // The user's explicit answer to the refusal — the only path that overwrites.
                self.save_active_tab(true);
            }
            Message::DiscardAndClose(path) => {
                if let Some(tab) = self
                    .panes
                    .focused_mut()
                    .tabs
                    .iter_mut()
                    .find(|t| t.path == path)
                {
                    tab.dirty = false; // the answer WAS "lose these edits"
                }
                self.force_close_tab(&path);
            }
            Message::SaveAndClose(path) => {
                self.save_tab(&path, false);
                // Only close if the save actually landed; a refused save must not take the
                // buffer down with it.
                if !self.is_dirty(&path) {
                    self.force_close_tab(&path);
                }
            }
            Message::CancelClose => self.confirm_close = None,
            Message::CloseRequested => {
                // The one place unsaved work can leave the building without being asked about.
                // Clean ⇒ quit straight away, so the guard costs nothing when there is nothing
                // to guard.
                if self.any_dirty() {
                    self.confirm_quit = true;
                } else {
                    return Self::quit();
                }
            }
            Message::SaveAllAndQuit => {
                // Save every dirty buffer across EVERY pane, then leave — but only if they all
                // landed. A save refused because the file changed on disk (`save_conflict`) must
                // not be steamrolled by the quit it was blocking; the prompt stays up so the
                // conflict can be answered.
                for path in self.dirty_paths() {
                    self.save_tab(&path, false);
                }
                if self.any_dirty() {
                    self.confirm_quit = false; // let the conflict notice be seen and answered
                } else {
                    return Self::quit();
                }
            }
            Message::DiscardAndQuit => return Self::quit(),
            Message::CancelQuit => self.confirm_quit = false,
            Message::RunCompile => return self.start_compile(),
            Message::CompileDone(done) => {
                let (report, diagnostics) = *done;
                self.compiling = false;
                self.compile_cancel = None;
                // Wholesale replacement: a fresh run supersedes the previous one entirely,
                // including for files it no longer mentions. Other sources are untouched.
                self.diagnostics
                    .replace_all(sc_win::diagnostics::DiagnosticSource::Compile, diagnostics);
                self.compile_report = Some(report);
            }
            Message::CancelCompile => self.cancel_compile(),
            Message::OpenDiagnostic(i) => return self.open_diagnostic(i),
            Message::UnityPathChanged(s) => self.unity_path_input = s,
            Message::JumpToPendingLine => {
                if let Some(line) = self.panes.focused_mut().pending_scroll_line.take() {
                    return self.scroll_code_to_line(self.panes.focused_id(), line);
                }
            }
            Message::ToggleSettings => {
                self.open_menu = None;
                // Closing the modal COMMITS + persists the edits (save-on-close), so a user can
                // set up connections/routing and just close the panel without starting a run.
                let was_open = self.settings_open;
                self.settings_open = !self.settings_open;
                if was_open {
                    self.commit_settings();
                }
            }
            // --- Plugins (spec 25) ---
            //
            // Every arm here is fire-and-forget: the plugin is told what happened and
            // answers in its own time by pushing new content. Nothing blocks, and a dead
            // plugin costs a no-op write rather than a stall.
            Message::PluginCommand(panel, command, args) => {
                self.send_to_plugin_of(panel, |_| sc_plugin_proto::HostMessage::CommandInvoked {
                    command: command.clone(),
                    args: args.clone(),
                });
            }
            Message::PluginFieldChanged(panel, field, value) => {
                // Held host-side until submit. A round trip per keystroke is the same
                // mistake as a per-keystroke `buffer.changed` carrying text.
                self.plugin_fields.insert((panel, field), value);
            }
            Message::PluginFormSubmit(panel) => {
                // Only this panel's fields, in a stable order — `BTreeMap` gives
                // deterministic output so a plugin parsing the payload sees the same
                // shape every time.
                let values: Vec<(String, String)> = self
                    .plugin_fields
                    .iter()
                    .filter(|((p, _), _)| *p == panel)
                    .map(|((_, f), v)| (f.clone(), v.clone()))
                    .collect();
                let encoded = sc_craft_ui::plugin::view::encode_form(&values);
                self.send_to_plugin_of(panel, |panel_id| {
                    sc_plugin_proto::HostMessage::PanelEvent {
                        panel: panel_id,
                        value: encoded.clone(),
                    }
                });
            }
            Message::TogglePluginsModal => {
                self.plugins_modal = !self.plugins_modal;
                // Cleared on OPEN, not on close: an error from a previous visit is stale,
                // but one raised while the modal is open must stay until it is read.
                if self.plugins_modal {
                    self.plugin_toggle_error = None;
                }
            }
            Message::FinishFirstRunPlugins => self.finish_first_run_plugins(),
            Message::SetPluginEnabled(dir_name, enabled) => {
                let dir = sc_craft_ui::plugin::plugins_dir().join(&dir_name);
                match sc_craft_ui::plugin::discover::set_enabled(&dir, enabled) {
                    Ok(()) => {
                        self.plugin_toggle_error = None;
                        // The switch is recorded, not applied — plugins load at startup.
                        self.plugins_need_restart = true;
                    }
                    Err(why) => self.plugin_toggle_error = Some(why),
                }
            }
            Message::Tick => {
                self.pump_terminal();
                self.pump_plugins();
                // Drive the live code-view refresh OFF the UI thread (returns Task::none unless a
                // reload is due). This is the fix for the Execute-plan freeze.
                // Also keep the chat pinned to the bottom as content streams in (unless the user
                // scrolled up) — batched so both run this tick.
                return Task::batch([self.plugin_autoscroll_task(), self.diff_task()]);
            }
            Message::FileDiffReady(rel, diff) => {
                if self.diff_pending.as_deref() == Some(rel.as_str()) {
                    self.diff_pending = None;
                }
                self.diff_cache.insert(rel.clone(), (*diff).clone());
                // Apply ONLY if that file is still the one on screen. A fast
                // click-through must not repaint the pane with a diff belonging to a
                // file the user has already moved past.
                if self.panes.focused().selected_file.as_deref() == Some(rel.as_str()) {
                    let pane = self.panes.focused_mut();
                    pane.changed_lines = diff.added.clone();
                    pane.file_diff = *diff;
                }
            }
            Message::SyncWorkspace => {
                // Re-walk the tree + git state OFF the UI thread — the walk and the git
                // subprocesses are the slow part, so compute a snapshot on a background thread and
                // apply it when it's ready (`WorkspaceSynced`). Skip if a sync is already pending.
                if self.picked_workspace.is_some() && !self.sync_pending {
                    self.sync_pending = true;
                    let root = self.workspace_root();
                    return Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || compute_snapshot(root))
                                .await
                                .ok()
                        },
                        Message::WorkspaceSynced,
                    );
                }
            }
            Message::WorkspaceSynced(snap) => {
                self.sync_pending = false;
                if let Some(snap) = snap {
                    self.apply_snapshot(snap);
                }
            }
            Message::SelectFile(rel) => {
                self.select_file(rel);
            }
            Message::CloseTab(path) => self.close_tab(&path),
            Message::ModifiersChanged(m) => {
                // Cache the held modifiers so the next git-row click can tell single- from
                // ctrl-toggle from shift-range selection (button presses carry no modifiers).
                self.modifiers = m;
            }
            Message::SelectGitFile(rel) => {
                // Branch on the tracked modifiers (iced buttons don't report the modifiers held
                // at click time, so we read the live `self.modifiers` cached from key events):
                //   Ctrl → toggle this row into/out of the multi-selection (keep the rest).
                //   Shift → re-select the contiguous range from the anchor to this row.
                //   neither → plain single-select (clear the set, select just this row).
                // In every case the last-clicked row becomes the previewed file (`selected_file`),
                // since the CODE panel is single-file.
                if self.modifiers.control() && !self.modifiers.shift() {
                    // Ctrl-toggle: additive, doesn't clear the rest. Move the anchor here.
                    if !self.git_selection.remove(&rel) {
                        self.git_selection.insert(rel.clone());
                    }
                    self.git_select_anchor = Some(rel.clone());
                } else if self.modifiers.shift() {
                    // Shift-range: select the inclusive span between the anchor (or this row, if
                    // no anchor yet) and this row in the CURRENT DISPLAYED ORDER, replacing the
                    // set. The anchor is kept, so successive shift-clicks re-anchor from it.
                    let order = self.git_display_order();
                    let anchor = self
                        .git_select_anchor
                        .clone()
                        .unwrap_or_else(|| rel.clone());
                    self.git_selection = git_range(&order, &anchor, &rel);
                    if self.git_select_anchor.is_none() {
                        self.git_select_anchor = Some(anchor);
                    }
                } else {
                    // Plain click: single-select. The set always holds the selected file too, so a
                    // click leaves a 1-element selection consistent with the previewed file.
                    self.git_selection.clear();
                    self.git_selection.insert(rel.clone());
                    self.git_select_anchor = Some(rel.clone());
                }
                // From the GIT panel → the review view: the intent is to see what changed, and
                // only that surface has the diff wash and the jump-to-change affordance.
                self.select_file_for_review(rel);
                if self.panes.focused().changed_lines.iter().next().is_some() {
                    // Re-emit as a follow-up message: it's processed after this update's view()
                    // rebuilds with the new file, so scroll_to acts on the correct layout.
                    return Task::done(Message::JumpToFirstChange);
                }
            }
            Message::JumpToFirstChange => {
                if let Some(&first) = self.panes.focused().changed_lines.iter().next() {
                    return self.scroll_code_to_line(self.panes.focused_id(), first);
                }
            }
            Message::ToggleDir(rel) => {
                if !self.collapsed_dirs.remove(&rel) {
                    self.collapsed_dirs.insert(rel);
                }
            }
            Message::FileFilterChanged(q) => {
                self.file_filter = q;
            }
            Message::ToggleMenu(m) => {
                self.open_menu = if self.open_menu == Some(m) {
                    None
                } else {
                    Some(m)
                };
            }
            Message::MinimapJump(line) => {
                return self.scroll_code_to_line(self.panes.focused_id(), line);
            }
            Message::CodeScrolled(vp) => {
                // Record the visible slice as fractions of the whole content so the minimap can
                // box "you are here". top = how far down we've scrolled; height = how much of the
                // file fits on screen.
                let top = vp.relative_offset().y;
                let content_h = vp.content_bounds().height.max(1.0);
                let view_h = vp.bounds().height;
                self.panes.focused_mut().code_view_h = view_h;
                self.panes.focused_mut().code_view_w = vp.bounds().width;
                self.panes.focused_mut().code_scroll_y = vp.absolute_offset().y;
                let height = (view_h / content_h).clamp(0.0, 1.0);
                self.panes.focused_mut().code_viewport = Some((top * (1.0 - height), height));
            }
            Message::SelectBottomTab(t) => self.bottom_tab = t,
            Message::TermInput(s) => self.terminal.input = s,
            Message::TermSubmit => {
                if !self.terminal.running {
                    // Typed by the user, in their own project: the host, like the Run
                    // button. This terminal is theirs. The agent's own commands still go
                    // through `term_exec_mode` and stay contained.
                    let cmdline = self.terminal.input.clone();
                    let mode = self.user_exec_mode();
                    self.term_rx = self.terminal.run(&cmdline, &mode);
                }
            }
            Message::TermKill => self.terminal.kill(),
            Message::TermClear => self.terminal.clear(),
            Message::TermHistoryPrev => self.terminal.history_prev(),
            Message::TermHistoryNext => self.terminal.history_next(),
            Message::GitCursorMoved(p) => {
                self.cursor_pos = p;
                // Arm a pending tab drag once the cursor has travelled far enough to mean it.
                // Latching rather than re-testing each move means a drag that wanders back over
                // its origin stays a drag. Below the threshold the press is still a click, and
                // nothing about the UI has changed yet.
                if let Some(DragSubject::Tab { armed, origin, .. }) = &mut self.drag {
                    if !*armed && origin.distance(p) >= DragSubject::THRESHOLD {
                        *armed = true;
                    }
                }
                // Move the held divider by the cursor DELTA from the grab point, scaled by the
                // split's own extent. Delta-mapping needs only the extent — never the region's
                // origin — which is what let the guessed `0.20 * window_w` and the
                // chrome-constant arithmetic in `explorer_region_h` be deleted outright.
                if let Some(d) = &self.drag_split {
                    if d.extent > 1.0 {
                        let moved = match d.axis {
                            sc_win::layout::Axis::Horizontal => p.x - d.origin,
                            sc_win::layout::Axis::Vertical => p.y - d.origin,
                        };
                        let frac = (d.frac0 + moved / d.extent).clamp(0.1, 0.9);
                        let id = d.id.clone();
                        self.splits.set(&id, frac);
                    }
                }
            }
            Message::SplitGrab { id, axis, extent } => {
                // Anchor at the current cursor and fraction, so the divider never jumps on grab.
                let frac0 = self.splits.get(&id, 0.5);
                let origin = match axis {
                    sc_win::layout::Axis::Horizontal => self.cursor_pos.x,
                    sc_win::layout::Axis::Vertical => self.cursor_pos.y,
                };
                self.drag_split = Some(Drag {
                    id,
                    axis,
                    extent,
                    origin,
                    frac0,
                });
            }
            Message::SplitDragEnd => {
                // Persist on release, not per mouse-move. The fraction is already in the store —
                // the tree keys dividers by id, so there is nothing to copy across.
                if self.drag_split.take().is_some() {
                    self.splits.save();
                }
                // A panel released outside every drop target: cancel rather than strand it. This
                // fires for ANY release, so a drag can't survive letting go over the menu bar or
                // off the window.
                if self.drag.is_some() {
                    // A window-edge band counts as a target too, so a release over the frame
                    // completes the dock rather than cancelling it.
                    if self.drop_target.is_some() || self.dock_side.is_some() {
                        return Task::done(Message::PanelDrop);
                    }
                    self.drag = None;
                    self.dock_side = None;
                    self.drop_target = None;
                }
            }
            Message::WindowSize(w, h) => {
                self.window_w = w;
                self.window_h = h;
            }
            Message::TogglePanel(kind) => {
                let next = if self.layout.contains(kind) {
                    // Remember where it sat, so ticking it back on returns it there instead of
                    // dropping it beside whatever leaf comes first.
                    if let Some(slot) = self.layout.slot_of(kind) {
                        self.panel_slots.insert(kind, slot);
                    }
                    // Never hide the last editor — an IDE with nothing to edit is not a layout
                    // choice, it's a broken window.
                    self.layout.without(kind)
                } else {
                    // Put it back where it was. The fallback covers a first-ever show, or the
                    // case where the panel it used to sit beside is itself now hidden.
                    self.panel_slots
                        .get(&kind)
                        .and_then(|slot| self.layout.restore(kind, slot))
                        .or_else(|| {
                            Some(self.layout.with(
                                kind,
                                &format!("user:{}", kind.slug()),
                                sc_win::layout::Axis::Horizontal,
                            ))
                        })
                };
                if let Some(next) = next.and_then(|l| l.sanitize()) {
                    self.layout = next.clone();
                    self.layouts.set(next);
                    self.layouts.save();
                }
                self.open_menu = None;
            }
            Message::PanelGrab(kind) => {
                self.drag = Some(DragSubject::Panel(kind));
                self.dock_side = None;
                self.drop_target = None;
            }
            Message::TabPress(pane, path) => {
                // A *possible* drag. Nothing is selected and no drop target is painted until the
                // cursor actually moves — see `TabRelease` for the click case. The origin is the
                // live window-space cursor, which `CursorMoved` also arms against.
                self.drag = Some(DragSubject::Tab {
                    from: pane,
                    path,
                    armed: false,
                    origin: self.cursor_pos,
                });
                self.dock_side = None;
                self.drop_target = None;
            }
            Message::TabRelease(path) => {
                // Released on the tab it was pressed on. If the drag never armed, the gesture was
                // a click, so select. If it armed, the drop is resolved by `PanelDrop` and this
                // must not also select — that would fight the drop.
                let was_click = matches!(&self.drag, Some(DragSubject::Tab { armed: false, .. }));
                if was_click {
                    self.drag = None;
                    self.select_file(path);
                }
            }
            Message::TabDropOnPane(into) => {
                self.drop_tab_on_pane(into);
            }
            Message::PanelHover(target, x, y, w, h, tw, th) => {
                // Resolve the cursor to an EDGE of the hovered panel. Zones are fractions of the
                // panel, so a narrow panel is still droppable on all four sides.
                // A panel can't be dropped on itself; a TAB can be dropped on the edge of any
                // pane including its own (that's "split this pane and put me in the new half").
                let live = match &self.drag {
                    Some(DragSubject::Panel(k)) => *k != target,
                    Some(tab @ DragSubject::Tab { .. }) => tab.is_active(),
                    None => false,
                };
                if live {
                    let side = sc_win::layout::Side::nearest(x, y, w, h);
                    // Judged against the TREE, never the window: the window has a menu bar above
                    // the tree and sometimes a gate bar below, so neither edge lines up — which
                    // is exactly why bottom-edge docking never fired.
                    let outer = side.is_outer(x, y, w, h, tw, th);
                    self.drop_target = Some((target, side, outer));
                }
            }
            Message::DockHover(side) => {
                // Only meaningful mid-drag. Entering a band supersedes any per-panel target, so
                // the highlight can't show two competing outcomes at once.
                if self.drag.as_ref().is_some_and(|d| d.is_active()) {
                    self.dock_side = side;
                    if side.is_some() {
                        self.drop_target = None;
                    }
                }
            }
            Message::PanelDrop => {
                match self.drag.clone() {
                    // A tab resolves to a PANE, never to a tree edit of its own: it either lands
                    // in an existing pane's strip, or a new pane is opened for it at the edge
                    // that was targeted. See `drop_tab_at`.
                    Some(DragSubject::Tab { .. }) => {
                        let at = self
                            .dock_side
                            .map(|side| (None, side))
                            .or_else(|| self.drop_target.map(|(t, side, _)| (Some(t), side)));
                        if let Some((target, side)) = at {
                            self.drop_tab_at(target, side);
                        }
                    }
                    Some(DragSubject::Panel(kind)) => {
                        if let Some(side) = self.dock_side {
                            // A WINDOW-edge dock: a full-span column or row across the whole
                            // layout, whatever happens to sit under the cursor.
                            if let Some(next) = self
                                .layout
                                .move_to_edge(kind, side)
                                .and_then(|l| l.sanitize())
                            {
                                self.layout = next.clone();
                                self.layouts.set(next);
                                self.layouts.save();
                            }
                        } else if let Some((target, side, outer)) = self.drop_target {
                            // An outer-edge drop docks down the side of EVERYTHING (a new
                            // full-span column/row); an interior one splits just the panel under
                            // the cursor.
                            let moved = if outer {
                                self.layout.move_to_edge(kind, side)
                            } else {
                                self.layout.move_panel(kind, target, side)
                            };
                            if let Some(next) = moved.and_then(|l| l.sanitize()) {
                                self.layout = next.clone();
                                self.layouts.set(next);
                                self.layouts.save();
                            }
                        }
                    }
                    None => {}
                }
                self.drag = None;
                self.dock_side = None;
                self.drop_target = None;
            }
            Message::FocusPane(id) => self.panes.focus(id),
            Message::SplitEditor => self.split_editor(),
            Message::ResetLayout => {
                self.layout = sc_win::layout::Layout::default_for_product();
                self.layouts.set(self.layout.clone());
                self.layouts.save();
                self.open_menu = None;
            }
            // ---- the profiler (spec 24) ----
            Message::OpenProfile => return self.open_profile(),
            Message::ProfileLoaded(src, r) => self.profile_loaded(src, r),
            Message::RecordProfile => return self.record_profile(),
            Message::CancelProfile => {
                // Cooperative: the worker notices the flag between polls and kills the child.
                if let Some(c) = &self.flame_cancel {
                    c.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Message::ProfileRecorded(r) => return self.profile_recorded(r),
            Message::FlameZoom(path) => self.flame_zoom_to(path),
            Message::FlameHover(h) => self.flame_hover = h.map(|b| *b),
            Message::FlameSearch(q) => self.flame_search = q,
            Message::FlameTarget(t) => self.flame_target = t,
            Message::FlameArgs(a) => self.flame_args = a,
            Message::RecheckProfiler => self.recheck_flame_tool(),
            Message::CopyInstallHint => {
                // Whichever tool we'd rather they installed; samply first, matching `detect`.
                return iced::clipboard::write(
                    sc_win::flame::tool::Profiler::Samply
                        .install_hint()
                        .to_string(),
                );
            }

            Message::GitRowMenu(path, status) => {
                self.git_menu_at = self.cursor_pos;
                self.git_menu = Some((path, status));
            }
            Message::CloseGitMenu => self.git_menu = None,
            Message::GitStage(path) => {
                self.git_menu = None;
                // Batch: if this file is part of a multi-selection, stage every selected file in
                // one call (the user picked a set with Ctrl/Shift and expects the ＋/menu to act on
                // all of it). A lone or unselected file stages just itself.
                let targets = self.git_action_targets(&path);
                let mut args = vec!["add", "--"];
                args.extend(targets.iter().map(String::as_str));
                self.run_git(&args);
                self.refresh_git_view();
            }
            Message::GitUnstage(path) => {
                self.git_menu = None;
                let targets = self.git_action_targets(&path);
                let mut args = vec!["restore", "--staged", "--"];
                args.extend(targets.iter().map(String::as_str));
                self.run_git(&args);
                self.refresh_git_view();
            }
            Message::GitDiscard(path) => {
                self.git_menu = None;
                // Batch: discard every file in the selection when this row is part of one. Split by
                // tracked-ness — untracked files need `clean -f` (a `checkout --` is a no-op on
                // them), tracked files need `checkout --` to restore the committed content.
                let targets = self.git_action_targets(&path);
                let (untracked, tracked): (Vec<&String>, Vec<&String>) =
                    targets.iter().partition(|p| {
                        self.file_status.get(*p) == Some(&sc_win::gitdiff::FileStatus::Added)
                    });
                if !untracked.is_empty() {
                    let mut args = vec!["clean", "-f", "--"];
                    args.extend(untracked.iter().map(|p| p.as_str()));
                    self.run_git(&args);
                }
                if !tracked.is_empty() {
                    let mut args = vec!["checkout", "--"];
                    args.extend(tracked.iter().map(|p| p.as_str()));
                    self.run_git(&args);
                }
                self.refresh_git_view();
                // Close tabs for files the discard REMOVED from disk (deleting an untracked file
                // with `clean -f`) — a tab on a file that no longer exists is dead weight. Files
                // that were merely reverted still exist, so their tabs stay (reloaded below).
                let root = self.workspace_root();
                let gone: Vec<String> = targets
                    .iter()
                    .filter(|p| !root.join(p).exists())
                    .cloned()
                    .collect();
                for p in &gone {
                    self.close_tab(p);
                }
                // If the file still on screen was reverted (not deleted), reload it to show the
                // reverted content.
                if self
                    .panes
                    .focused()
                    .selected_file
                    .as_ref()
                    .is_some_and(|s| targets.contains(s))
                {
                    self.reload_selected();
                }
            }
            Message::CommitMsgChanged(s) => self.commit_msg = s,
            Message::GitStageAll => {
                self.run_git(&["add", "-A"]);
                self.refresh_git_view();
            }
            Message::GitUnstageAll => {
                self.run_git(&["reset"]); // unstage everything, keep working-tree changes
                self.refresh_git_view();
            }
            Message::GitCommit => {
                let msg = self.commit_msg.trim().to_string();
                // Nothing staged, or an empty message → don't attempt a commit.
                let has_staged = self.stage_states.values().any(|s| s.staged);
                if msg.is_empty() || !has_staged {
                    return Task::none();
                }
                if self.run_git(&["commit", "-m", &msg]) {
                    self.commit_msg.clear();
                }
                self.refresh_git_view();
                self.reload_selected();
            }
            // The three network ops return a Task and refresh in `GitNetDone` — they do NOT
            // refresh here, because at this point the op has not run yet.
            Message::GitPush => {
                // No upstream yet → set it on push so a fresh branch publishes cleanly.
                return if self.upstream.upstream.is_none() {
                    match self.branch.clone() {
                        Some(b) => self.start_git_net("push", &["push", "-u", "origin", &b]),
                        None => Task::none(),
                    }
                } else {
                    self.start_git_net("push", &["push"])
                };
            }
            Message::GitPull => return self.start_git_net("pull", &["pull", "--ff-only"]),
            Message::GitFetch => return self.start_git_net("fetch", &["fetch"]),
            Message::CancelGitNet => self.cancel_git_net(),
            Message::GitNetDone(label, ok, gist) => {
                let task = self.finish_git_net(&label, ok, &gist);
                // A pull can rewrite the file under the cursor, so re-read it — but only for the
                // ops that touch the working tree. A fetch never does.
                if label != "fetch" {
                    self.reload_selected();
                }
                return task;
            }
            Message::PickWorkspace => {
                self.open_menu = None;
                // Native folder dialog (blocking — fine for a button click). When a
                // folder is chosen, runs go there so follow-up prompts iterate on it.
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title("Choose a project folder to work in")
                    .pick_folder()
                {
                    self.open_workspace(dir);
                }
            }
            Message::OpenRecent(dir) => {
                self.open_menu = None;
                if dir.is_dir() {
                    self.open_workspace(dir);
                }
            }
            Message::NoOp => {}
            Message::ClearWorkspace => {
                self.open_menu = None;
                self.picked_workspace = None;
                self.panes.focused_mut().selected_file = None;
                self.panes.focused_mut().code = None;
                // Clear the CODE-panel tabs too — they belonged to the closed project.
                // EVERY pane: the old project's tabs are meaningless, and so is an arrangement
                // of panes holding them.
                self.panes.clear();
                self.confirm_close = None;
                self.save_conflict = None;
                self.refresh_project_kind();
                // Closed is a workspace change too — `None` is the real state, not an
                // error, and a plugin still running against the old root must hear it.
                self.notify_workspace_changed();
                // Forget the *current* project so a restart doesn't re-open it, but keep the
                // recents list (the user may want to re-pick one).
                let mut state = sc_win::persist::load();
                state.last_project = None;
                sc_win::persist::save(&state);
            }
        }
        #[cfg(debug_assertions)]
        self.assert_panes_consistent();
        Task::none()
    }
}
