//! App update(): the message-dispatch reducer.

use super::*;

impl App {
    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::IntentChanged(s) => self.intent = s,
            Message::ModelChanged(s) => self.model_input = s,
            Message::OrchModelChanged(s) => self.orch_model_input = s,
            Message::AdvisorChanged(s) => self.advisor_input = s,
            Message::LocalUrlChanged(s) => self.local_url_input = s,
            Message::LocalKeyChanged(s) => self.local_key_input = s,
            Message::GeminiUrlChanged(s) => self.gemini_url_input = s,
            Message::GeminiKeyChanged(s) => self.gemini_key_input = s,
            Message::CoderProviderChanged(p) => self.cfg.coder_provider = p,
            Message::PlannerProviderChanged(p) => self.cfg.planner_provider = p,
            Message::AdvisorProviderChanged(p) => self.cfg.advisor_provider = p,
            Message::SettingsTabChanged(t) => self.settings_tab = t,
            Message::VerifyChanged(s) => self.verify_input = s,
            Message::SuffixChanged(s) => self.suffix_input = s,
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
            Message::ToggleYolo(v) => self.cfg.yolo = v,
            Message::ToggleDryRun(v) => self.cfg.dry_run = v,
            Message::RunTdd => self.start(RunKind::Tdd),
            // The composer's main run now goes through the DISCIPLINED path: staged plan →
            // architecture → decompose → compiler-driven build (tiny compiler-verified steps),
            // instead of the bare single-agent iterate loop. The line-comment small-fix path
            // (`start_iterate_with`) still uses iterate — it's a tiny scoped edit, not a feature.
            Message::RunIterate => self.start(RunKind::StagedBuild),
            Message::Tick => {
                self.pump();
                // Drive the live code-view refresh OFF the UI thread (returns Task::none unless a
                // reload is due). This is the fix for the Execute-plan freeze.
                // Also keep the chat pinned to the bottom as content streams in (unless the user
                // scrolled up) — batched so both run this tick.
                return Task::batch([self.live_reload_task(), self.chat_autoscroll_task()]);
            }
            Message::HealthTick => self.tick_health_probe(),
            Message::LiveViewReloaded(result) => {
                if let Some((code, added)) = result {
                    self.code = Some(code);
                    self.changed_lines = added;
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
                // Click-to-pin: show this file and stop auto-following the agent until
                // the next run re-arms follow.
                self.follow_agent = false;
                self.select_file(rel);
            }
            Message::SelectTab(path) => {
                // Switching tabs pins the view and re-selects the file. `select_file` is
                // idempotent for an already-open tab (it reloads + re-selects), and the active
                // tab === `selected_file`, so the body just follows.
                self.follow_agent = false;
                self.select_file(path);
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
                // Open the file, then jump the code view to its first changed line (git-tab
                // click → land you on the change, VS-Code-diff style). The scroll is DEFERRED a
                // beat so it runs against the newly-laid-out content, not the previous file's
                // tree — a same-frame scroll_to misses on large files (the new lines don't exist
                // in the layout yet).
                self.follow_agent = false;
                self.select_file(rel);
                if self.changed_lines.iter().next().is_some() {
                    // Re-emit as a follow-up message: it's processed after this update's view()
                    // rebuilds with the new file, so scroll_to acts on the correct layout.
                    return Task::done(Message::JumpToFirstChange);
                }
            }
            Message::JumpToFirstChange => {
                if let Some(&first) = self.changed_lines.iter().next() {
                    return self.scroll_code_to_line(first);
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
            Message::ChatSend => self.send_chat(),
            Message::CopyTurn(t) => return iced::clipboard::write(t),
            Message::RunProposedCommand => {
                if let Some(cmd) = self.proposed_command.take() {
                    // Show the Terminal tab and run there, through the same sandbox path as a
                    // typed command (strict containment applies — refused if it can't sandbox).
                    self.bottom_tab = BottomTab::Terminal;
                    if !self.terminal.running {
                        match self.term_exec_mode() {
                            Ok(mode) => self.term_rx = self.terminal.run(&cmd, &mode),
                            Err(reason) => self.terminal.blocked(&cmd, &reason),
                        }
                    }
                }
            }
            Message::DismissProposedCommand => self.proposed_command = None,
            Message::ChatEditorAction(i, action) => {
                // Read-only: apply selection/cursor/scroll actions so drag-select + Ctrl+C
                // work, but never edits — the message text is immutable.
                if !action.is_edit() {
                    if let Some(content) = self.chat_editors.get_mut(i) {
                        content.perform(action);
                    }
                }
            }
            Message::ApplyFile(i) => self.apply_proposed_file(i),
            Message::ExecutePlan(i) => self.execute_plan(i),
            Message::BreakdownPlan(i) => self.breakdown_plan(i),
            Message::ExecuteOpenPlan => self.execute_open_plan(),
            Message::BuildOpenPlan => self.build_open_plan(),
            Message::BuildLastPlan => self.build_last_plan(),
            Message::CommitPlan => self.commit_plan(),
            Message::ToggleThink(v) => self.think = v,
            Message::ToggleDebug(v) => self.debug = v,
            Message::UndoLastChange => self.undo_last_change(),
            Message::DismissComment(i) => {
                self.comments.remove(i);
                sc_win::comments::save(&self.workspace_root(), &self.comments);
            }
            Message::RevertComment(i) => self.revert_comment(i),
            Message::RevertBlock(cur_start) => self.revert_block(cur_start),
            Message::MinimapJump(line) => {
                return self.scroll_code_to_line(line);
            }
            Message::CodeScrolled(vp) => {
                // Record the visible slice as fractions of the whole content so the minimap can
                // box "you are here". top = how far down we've scrolled; height = how much of the
                // file fits on screen.
                let top = vp.relative_offset().y;
                let content_h = vp.content_bounds().height.max(1.0);
                let view_h = vp.bounds().height;
                self.code_view_h = view_h;
                self.code_view_w = vp.bounds().width;
                self.code_scroll_y = vp.absolute_offset().y;
                let height = (view_h / content_h).clamp(0.0, 1.0);
                self.code_viewport = Some((top * (1.0 - height), height));
            }
            Message::ChatScrolled(vp) => {
                // Arm auto-scroll only when the user is at (or within a line of) the bottom; scrolling
                // UP disarms it so a streaming reply doesn't yank them back down while they read. The
                // last few px of tolerance keeps it "stuck" through the tiny jitter as content grows.
                let content_h = vp.content_bounds().height;
                let view_h = vp.bounds().height;
                let bottom = (content_h - view_h).max(0.0);
                let at_bottom = bottom - vp.absolute_offset().y <= 8.0;
                self.chat_stuck_to_bottom = at_bottom;
            }
            Message::CancelRun => {
                if let Some(s) = &self.session {
                    s.cancel();
                    self.chat_turns.push(sc_win::chat::Turn {
                        role: sc_win::chat::Speaker::Agent,
                        text: "⏹ cancelling — stopping at the next step…".to_string(),
                    });
                }
            }
            Message::CancelChat => {
                if let Some(s) = &self.chat_session {
                    s.cancel();
                }
            }
            Message::SelectBottomTab(t) => self.bottom_tab = t,
            Message::TermInput(s) => self.terminal.input = s,
            Message::TermSubmit => {
                if !self.terminal.running {
                    let cmdline = self.terminal.input.clone();
                    match self.term_exec_mode() {
                        Ok(mode) => {
                            self.term_rx = self.terminal.run(&cmdline, &mode);
                        }
                        // Strict containment: sandbox was intended but unavailable. Echo the
                        // command as blocked and DO NOT run it on the host.
                        Err(reason) => {
                            if !cmdline.trim().is_empty() {
                                self.terminal.blocked(cmdline.trim(), &reason);
                            }
                        }
                    }
                }
            }
            Message::TermKill => self.terminal.kill(),
            Message::TermClear => self.terminal.clear(),
            Message::TermHistoryPrev => self.terminal.history_prev(),
            Message::TermHistoryNext => self.terminal.history_next(),
            Message::GitCursorMoved(p) => {
                self.cursor_pos = p;
                // While the chat|code divider is held, map the absolute cursor X to chat's share
                // of the chat+code region. The explorer occupies a fixed 20% on the left, so that
                // region runs from 0.20·W to W.
                if self.dragging_split && self.window_w > 0.0 {
                    let region_left = 0.20 * self.window_w;
                    let region_w = self.window_w - region_left;
                    if region_w > 1.0 {
                        let frac = (p.x - region_left) / region_w;
                        self.chat_frac = frac.clamp(0.15, 0.85);
                    }
                }
                // While the git|files divider is held, move it by the cursor Y DELTA from the grab
                // point — not an absolute mapping (which would need the explorer's exact top offset
                // and snap on grab). `explorer_frac` is Git's share of the EXPLORER COLUMN's height,
                // so a cursor delta of `d` px must be scaled by that column's height, NOT the whole
                // window — dividing by `window_h` made the divider lag the cursor (the column is
                // shorter than the window). `explorer_region_h()` is the column's true height, so
                // the divider now tracks the cursor 1:1.
                if let Some((y0, frac0)) = self.explorer_drag {
                    let region_h = self.explorer_region_h();
                    if region_h > 1.0 {
                        let dfrac = (p.y - y0) / region_h;
                        self.explorer_frac = (frac0 + dfrac).clamp(0.1, 0.8);
                    }
                }
            }
            Message::SplitDragStart => self.dragging_split = true,
            Message::SplitDragEnd => {
                self.dragging_split = false;
                self.explorer_drag = None;
                // A drag settled — persist both dividers' current positions by id (one write, on
                // release, not per mouse-move).
                self.splits
                    .set(sc_win::splits::id::CHAT_CODE, self.chat_frac);
                self.splits
                    .set(sc_win::splits::id::EXPLORER_GIT_FILES, self.explorer_frac);
                self.splits.save();
            }
            Message::WindowSize(w, h) => {
                self.window_w = w;
                self.window_h = h;
            }
            // Anchor the drag at the current cursor Y and current fraction — moves are deltas
            // from here, so the divider never jumps on grab.
            Message::ExplorerDragStart => {
                self.explorer_drag = Some((self.cursor_pos.y, self.explorer_frac));
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
            Message::GitPush => {
                // No upstream yet → set it on push so a fresh branch publishes cleanly.
                if self.upstream.upstream.is_none() {
                    if let Some(b) = self.branch.clone() {
                        self.run_git_net("push", &["push", "-u", "origin", &b]);
                    }
                } else {
                    self.run_git_net("push", &["push"]);
                }
                self.refresh_git_view();
                self.reload_selected();
            }
            Message::GitPull => {
                self.run_git_net("pull", &["pull", "--ff-only"]);
                self.refresh_git_view();
                self.reload_selected();
            }
            Message::GitFetch => {
                self.run_git_net("fetch", &["fetch"]);
                self.refresh_git_view();
            }
            Message::LineDragStart(n) => {
                // Begin a drag-selection anchored at line n. Clear any open comment box.
                self.drag = Some((n, n));
                self.comment_range = None;
            }
            Message::LineDragTo(n) => {
                // Extend the drag to line n (only while a drag is active).
                if let Some((anchor, _)) = self.drag {
                    self.drag = Some((anchor, n));
                }
            }
            Message::LineDragEnd => {
                // Commit the drag into a comment range (normalized so start ≤ end) and open
                // the comment box. A no-drag (just a click) yields a single-line range.
                if let Some((a, b)) = self.drag.take() {
                    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                    self.comment_range = Some((lo, hi));
                    self.comment_draft.clear();
                }
            }
            Message::CommentDraftChanged(s) => self.comment_draft = s,
            Message::CommentSubmit => self.submit_line_comment(),
            Message::CommentCancel => {
                self.comment_range = None;
                self.drag = None;
                self.comment_draft.clear();
            }
            Message::ConfirmAllow => self.answer_confirm(Confirmation::AllowOnce),
            Message::ConfirmDeny => {
                self.answer_confirm(Confirmation::Deny("denied by user".to_string()))
            }
            Message::ConfirmRemember => {
                // Remember the command's first token as the approved prefix.
                let prefix = match self.gatebar.first() {
                    Some(Gatebar::Confirm { command, .. }) => remember_prefix(command),
                    _ => String::new(),
                };
                self.answer_confirm(Confirmation::AllowRemember { prefix });
            }
            Message::NotesChanged(s) => self.sendback_notes = s,
            Message::GateApprove => self.answer_gate(Decision::Approve),
            // (Revise dropped from the UI — send-back-with-comments supersedes it. `Decision::Revise`
            //  stays in the workflow enum for the CLI; the GUI no longer surfaces a button for it.)
            Message::GateSendBack => {
                // Change B: feedback comes from CODE-REVIEW line comments. Harvest every
                // comment on the gating phase's artifact file and turn it into the send-back
                // notes (one bullet per comment). This is the primary feedback path: the user
                // reads the phase's `.md` in the code view, drops line comments where they want
                // changes, and clicks Send back. Fall back to the free-text notes box when there
                // are no line comments (so a general note still works).
                let Some(target) = self.gating_phase() else {
                    return Task::none();
                };
                let file = self.plan.path_for(target);
                let harvested = file.as_deref().and_then(|f| {
                    let on_file: Vec<&sc_win::comments::Comment> =
                        self.comments.on_file(f).map(|(_, c)| c).collect();
                    sc_win::comments::format_sendback_notes(&on_file)
                });
                let notes = harvested.or_else(|| non_empty(&self.sendback_notes));
                self.answer_gate(Decision::SendBack { target, notes });
                // The harvested comments have been DELIVERED as the send-back notes — drop them
                // (and persist) so they don't re-deliver on the re-planned phase's next gate.
                if let Some(f) = file {
                    self.comments.items.retain(|c| c.file != f);
                    sc_win::comments::save(&self.workspace_root(), &self.comments);
                }
            }
            Message::GateAbort => self.answer_gate(Decision::Abort),
            Message::SelectCoder(id) => self.selected_coder = Some(id),
            Message::ClearSelection => self.selected_coder = None,
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
                self.selected_file = None;
                self.code = None;
                // Clear the CODE-panel tabs too — they belonged to the closed project.
                self.open_tabs.clear();
                // Forget the *current* project so a restart doesn't re-open it, but keep the
                // recents list (the user may want to re-pick one).
                let mut state = sc_win::persist::load();
                state.last_project = None;
                sc_win::persist::save(&state);
                self.publish_workspace_to_remote();
            }
            Message::OpenOutputFolder => {
                if let Some(dir) = self.result.as_ref().and_then(|r| r.dir.clone()) {
                    // Open in the system file manager (Explorer on Windows).
                    #[cfg(target_os = "windows")]
                    let _ = std::process::Command::new("explorer").arg(&dir).spawn();
                    #[cfg(target_os = "macos")]
                    let _ = std::process::Command::new("open").arg(&dir).spawn();
                    #[cfg(all(unix, not(target_os = "macos")))]
                    let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
                }
            }
        }
        // Keep the per-turn selectable editors in step with the chat thread (no-op unless the
        // thread changed). Runs after every message so streamed/appended turns are covered.
        self.sync_chat_editors();
        Task::none()
    }
}
