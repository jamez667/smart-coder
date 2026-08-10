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
            Message::EditorEvent(pane, ev) => {
                // Forward to THAT pane's active buffer — not the focused one — and adopt the
                // widget's own modified flag, so the dirty dot can't drift from the buffer.
                if let Some(tab) = self.panes.get_mut(pane).and_then(|p| p.active_tab_mut()) {
                    if let Some(editor) = tab.editor_mut() {
                        let task = editor.update(&ev);
                        tab.dirty = editor.is_modified();
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
            Message::ChooseMode(craft) => {
                use sc_win::config::Mode;
                // Unreachable in a craft-only build — `mode_chosen()` is true there, so the
                // first-run question never opens. Guarded anyway: this is the write that would
                // leave a `mode` in config.json for a later ordinary build to honour.
                if !self.cfg.mode_switchable() {
                    return Task::none();
                }
                self.cfg.mode = Some(if craft { Mode::Craft } else { Mode::Assistant });
                // The default layout differs per mode, and `App::default` picked one before the
                // question was answered — so re-derive it now that we know.
                self.layout = self.layouts.get(craft);
                self.sync_panes_to_layout();
                self.cfg.save_config();
                // Finish the boot the prompt was holding up. `run()` skips the welcome and the
                // conversation while the question is open, because both are Assistant-shaped and
                // would have to be undone the moment someone answered "Just code".
                if self.picked_workspace.is_some() {
                    self.show_welcome();
                    if !craft {
                        self.open_conversation();
                    }
                }
            }
            Message::RunCompile => return self.start_compile(),
            Message::CompileDone(report) => {
                self.compiling = false;
                self.compile_cancel = None;
                self.compile_report = Some(*report);
            }
            Message::CancelCompile => self.cancel_compile(),
            Message::OpenDiagnostic(i) => return self.open_diagnostic(i),
            Message::UnityPathChanged(s) => self.unity_path_input = s,
            Message::JumpToPendingLine => {
                if let Some(line) = self.panes.focused_mut().pending_scroll_line.take() {
                    return self.scroll_code_to_line(self.panes.focused_id(), line);
                }
            }
            Message::EscapePressed => {
                // Only meaningful while the first-run question is open. Deliberately narrow:
                // Escape closing arbitrary UI is a separate decision, not one to make here.
                if !self.cfg.mode_chosen() {
                    return Task::done(Message::DeclineToChoose);
                }
            }
            Message::DeclineToChoose => {
                // Closing the question is not an answer. Nothing is written, so the prompt
                // returns on the next launch. No dirty-buffer check: the question blocks boot,
                // so there is nothing open yet to lose.
                return Self::quit();
            }
            Message::ToggleCraftMode(on) => {
                use sc_win::config::Mode;
                // The toggle isn't rendered in a craft-only build, so this can only arrive from a
                // stale queued `Task`. Refusing here keeps the "never writes a mode" claim a
                // property of the build rather than of the view.
                if !self.cfg.mode_switchable() {
                    return Task::none();
                }
                self.cfg.mode = Some(if on { Mode::Craft } else { Mode::Assistant });
                if on {
                    // A run must never outlive the switch into a mode that claims no model is
                    // contacted — cancel first, then drop the surfaces. Both sessions: an agent
                    // run and a chat turn are separate workers.
                    if let Some(s) = &self.session {
                        s.cancel();
                    }
                    if let Some(s) = &self.chat_session {
                        s.cancel();
                    }
                    // The health badge showed a *model* endpoint's state; in Craft mode there is
                    // no such thing, so clear it rather than leaving a stale verdict on screen.
                    self.backend_health = None;
                    self.health_rx = None;
                    // Land on the only tab that still means anything.
                    self.settings_tab = SettingsTab::General;
                }
                // Swap to the arrangement saved for the mode being entered. Per-mode layouts are
                // what make toggling back and forth non-destructive: each is found as it was
                // left, rather than one mode silently rearranging the other.
                self.layout = self.layouts.get(self.cfg.craft());
                self.sync_panes_to_layout();
                // Persist immediately. Mode is an answer to a question we promised to ask once;
                // losing it to a crash before the next save-on-close would ask again and read as
                // the app ignoring them.
                self.cfg.save_config();
            }
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
            Message::OpenComplyDialog => {
                self.open_menu = None;
                self.comply_open = true;
                // Drop the previous run's outcome: showing last time's totals
                // beside a fresh dialog invites reading them as current.
                self.comply_result = None;
            }
            Message::CloseComplyDialog => self.comply_open = false,
            Message::ComplyModelChanged(m) => self.comply_model = m,
            Message::RunComply => {
                if self.comply_running {
                    return Task::none(); // never stack two audits
                }
                // Fold the settings inputs into cfg first, so a key the user
                // just typed into Connections is actually used by this run.
                self.commit_settings();
                self.comply_running = true;
                self.comply_result = None;

                let workspace = self.workspace_root();
                let out_dir = sc_win::comply::output_dir(&workspace);
                // Craft mode forces the deterministic audit. Hiding the picker is not enough on
                // its own — a value chosen before the switch would still be sitting in
                // `comply_model` — so the decision is made HERE, where the run is spawned.
                let choice = if self.cfg.craft() {
                    sc_win::comply::ComplyModel::None
                } else {
                    self.comply_model
                };
                let cfg = self.cfg.clone();
                return Task::perform(
                    async move {
                        // Blocking: a ten-framework audit walks the workspace
                        // once per pack, and with a model chosen also makes
                        // network calls. Never on the UI thread.
                        tokio::task::spawn_blocking(move || {
                            sc_win::comply::run(&workspace, &out_dir, choice, &cfg)
                                .map_err(|e| e.to_string())
                        })
                        .await
                        .unwrap_or_else(|e| Err(format!("the audit thread panicked: {e}")))
                    },
                    Message::ComplyDone,
                );
            }
            Message::ComplyDone(result) => {
                self.comply_running = false;
                self.comply_result = Some(result);
            }
            Message::OpenComplyReport => {
                if let Some(Ok(r)) = &self.comply_result {
                    // Best-effort: failing to launch a viewer is no reason to
                    // disturb the report that was successfully written.
                    let _ = sc_win::proc::open_path(&r.index);
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
            Message::ClaudeInputChanged(s) => self.claude_input = s,
            Message::ToggleClaudeMenu => {
                self.claude_menu = !self.claude_menu;
                // A stale filter would hide most of the menu the next time it opens, which
                // reads as options having disappeared.
                self.claude_filter.clear();
            }
            Message::ClaudeFilterChanged(s) => self.claude_filter = s,
            Message::CycleClaudeModel => {
                let all = sc_win::claudecode::Model::ALL;
                let i = all
                    .iter()
                    .position(|m| *m == self.cfg.claude.model)
                    .unwrap_or(0);
                self.cfg.claude.model = all[(i + 1) % all.len()];
                self.cfg.save_config();
            }
            Message::CycleClaudePermission => {
                let all = sc_win::claudecode::Permission::ALL;
                let i = all
                    .iter()
                    .position(|p| *p == self.cfg.claude.permission)
                    .unwrap_or(0);
                self.cfg.claude.permission = all[(i + 1) % all.len()];
                self.cfg.save_config();
            }
            Message::ToggleClaudeContinue => {
                self.cfg.claude.continue_session = !self.cfg.claude.continue_session;
                self.cfg.save_config();
            }
            Message::AttachActiveFile => {
                // Append the open file's path to the prompt rather than reading its contents:
                // Claude Code has its own Read tool and its own context management, so handing
                // it a path lets it fetch what it needs. Pasting the file would duplicate work
                // and blow the prompt on a large file.
                if let Some(f) = self.panes.focused().selected_file.clone() {
                    if !self.claude_input.is_empty() && !self.claude_input.ends_with(' ') {
                        self.claude_input.push(' ');
                    }
                    self.claude_input.push('@');
                    self.claude_input.push_str(&f);
                    self.claude_menu = false;
                }
            }
            Message::AddClaudeDir => {
                self.claude_menu = false;
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    let d = dir.to_string_lossy().to_string();
                    if !self.cfg.claude.add_dirs.contains(&d) {
                        self.cfg.claude.add_dirs.push(d);
                        self.cfg.save_config();
                    }
                }
            }
            Message::RemoveClaudeDir(i) => {
                if i < self.cfg.claude.add_dirs.len() {
                    self.cfg.claude.add_dirs.remove(i);
                    self.cfg.save_config();
                }
            }
            Message::ClaudeAllowedChanged(s) => {
                self.cfg.claude.allowed_tools = sc_win::claudecode::split_tools(&s);
                self.cfg.save_config();
            }
            Message::ClaudeDisallowedChanged(s) => {
                self.cfg.claude.disallowed_tools = sc_win::claudecode::split_tools(&s);
                self.cfg.save_config();
            }
            Message::ClearClaudeRun => {
                self.claude_feed.clear();
                self.claude_input.clear();
                self.claude_menu = false;
            }
            Message::RunClaudeCode => {
                self.open_menu = None;
                // Guarded even though the panel only exists when available: a queued Task or a
                // stale message can arrive after a mode switch or a failed detection.
                if !self.claude_code_available() || self.claude_input.trim().is_empty() {
                    return Task::none();
                }
                // The panel's OWN input drives the run, not the chat composer's — but `start()`
                // reads `self.intent`, which every run kind shares. Swap it in for the spawn and
                // put the composer's text back, so pressing Run here cannot silently rewrite
                // what the user had typed over there.
                let composer = std::mem::replace(&mut self.intent, self.claude_input.clone());
                // The feed ACCUMULATES across runs — it reads as a conversation, which is what
                // a panel with an input box looks like it should be. Clearing on every Run made
                // it a single-shot viewer that threw away the exchange you were reading, and
                // there is already a "Clear this conversation" menu item for when you want that.
                //
                // Echo the prompt first, so the feed says what was ASKED and not only what the
                // agent did. Without it a scrolled-back feed is a list of tool calls with no
                // question attached to them.
                let asked = self.claude_input.clone();
                if !self.claude_feed.is_empty() {
                    self.claude_feed.push(Row::ok(" ", String::new()));
                }
                self.claude_feed.push(Row::ok("❯", asked));
                self.start(RunKind::ClaudeCode);
                self.intent = composer;
                // The prompt has been sent and is now in the feed; leaving it in the box means
                // the next Run silently re-sends it.
                self.claude_input.clear();
            }
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
                    self.panes.focused_mut().code = Some(code);
                    self.panes.focused_mut().changed_lines = added;
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
                if let Some(next) = next.and_then(|l| l.sanitize(self.cfg.craft())) {
                    self.layout = next.clone();
                    self.layouts.set(self.cfg.craft(), next);
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
                    // Switching tabs pins the view and re-selects the file. `select_file` is
                    // idempotent for an already-open tab (it reloads + re-selects), and the
                    // active tab === `selected_file`, so the body just follows.
                    self.follow_agent = false;
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
                                .and_then(|l| l.sanitize(self.cfg.craft()))
                            {
                                self.layout = next.clone();
                                self.layouts.set(self.cfg.craft(), next);
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
                            if let Some(next) = moved.and_then(|l| l.sanitize(self.cfg.craft())) {
                                self.layout = next.clone();
                                self.layouts.set(self.cfg.craft(), next);
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
                self.layout = sc_win::layout::Layout::default_for(self.cfg.craft());
                self.layouts.set(self.cfg.craft(), self.layout.clone());
                self.layouts.save();
                self.open_menu = None;
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
                self.panes.focused_mut().drag = Some((n, n));
                self.panes.focused_mut().comment_range = None;
            }
            Message::LineDragTo(n) => {
                // Extend the drag to line n (only while a drag is active).
                if let Some((anchor, _)) = self.panes.focused().drag {
                    self.panes.focused_mut().drag = Some((anchor, n));
                }
            }
            Message::LineDragEnd => {
                // Commit the drag into a comment range (normalized so start ≤ end) and open
                // the comment box. A no-drag (just a click) yields a single-line range.
                if let Some((a, b)) = self.panes.focused_mut().drag.take() {
                    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                    self.panes.focused_mut().comment_range = Some((lo, hi));
                    self.panes.focused_mut().comment_draft.clear();
                }
            }
            Message::CommentDraftChanged(s) => self.panes.focused_mut().comment_draft = s,
            Message::CommentSubmit => self.submit_line_comment(),
            Message::CommentCancel => {
                self.panes.focused_mut().comment_range = None;
                self.panes.focused_mut().drag = None;
                self.panes.focused_mut().comment_draft.clear();
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
                // Feedback comes from CODE-REVIEW line comments: the user reads a phase's `.md`
                // in the code view, drops comments where they want changes, and clicks Send back.
                //
                // Comments are harvested across EVERY phase artifact, not just the gating one —
                // the file a comment sits on says which phase it's about, so noticing while
                // reading the layout that the ARCHITECTURE is wrong is expressible: comment on
                // architecture.md and the send-back targets Architecture (dropping layout, which
                // regenerates from the correction). `resolve_sendback` owns that rule; without
                // comments we fall back to the free-text box and bounce the gating phase to itself.
                let Some(gating) = self.gating_phase() else {
                    return Task::none();
                };
                let files: Vec<(sc_workflow::Phase, Option<String>)> = sc_workflow::Phase::ALL
                    .iter()
                    .map(|&p| (p, self.plan.path_for(p)))
                    .collect();
                let rows: Vec<sc_win::comments::PhaseComments<'_>> = files
                    .iter()
                    .filter_map(|(phase, file)| {
                        let file = file.as_deref()?;
                        Some(sc_win::comments::PhaseComments {
                            phase: *phase,
                            file,
                            notes: self.comments.on_file(file).map(|(_, c)| c).collect(),
                        })
                    })
                    .collect();

                let resolved = sc_win::comments::resolve_sendback(&rows);
                let commented: Vec<String> = rows
                    .iter()
                    .filter(|r| !r.notes.is_empty())
                    .map(|r| r.file.to_string())
                    .collect();
                let (target, notes) = match resolved {
                    Some((target, notes)) => (target, notes),
                    None => (gating, non_empty(&self.sendback_notes)),
                };
                self.answer_gate(Decision::SendBack { target, notes });
                // Every harvested comment has been DELIVERED as part of the notes — drop them
                // all (and persist) so none re-delivers at a later gate, anchored to text that
                // has since been regenerated.
                if !commented.is_empty() {
                    self.comments.items.retain(|c| !commented.contains(&c.file));
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
                self.panes.focused_mut().selected_file = None;
                self.panes.focused_mut().code = None;
                // Clear the CODE-panel tabs too — they belonged to the closed project.
                // EVERY pane: the old project's tabs are meaningless, and so is an arrangement
                // of panes holding them.
                self.panes.clear();
                self.confirm_close = None;
                self.save_conflict = None;
                self.refresh_project_kind();
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
        #[cfg(debug_assertions)]
        self.assert_panes_consistent();
        Task::none()
    }
}
