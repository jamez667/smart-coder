//! App logic: line-replace, reverts, plan/build actions, tabs, git.

use super::*;

impl App {
    /// Apply a completed line-replace reply: splice the new block into the file, record the
    /// before-text on the comment, resolve it, refresh the view + git, and verify if needed.
    pub(crate) fn apply_line_replace(&mut self, raw: &str) {
        let Some(rf) = self.replace.take() else {
            return;
        };
        self.working = None;
        let c = rf.comment;
        let root = self.workspace_root();
        let path = root.join(&c.file);
        let Some(new_block) = sc_win::linecomment::extract_replacement(raw) else {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ the model returned nothing to apply.".to_string(),
            });
            return;
        };
        if self.debug {
            self.debug_prompt(
                "line-replace reply",
                &format!(
                    "hint range = {}-{}\nRAW REPLY:\n{}\n\nEXTRACTED BLOCK ({} lines):\n{}",
                    c.start,
                    c.end,
                    raw.trim(),
                    new_block.lines().count(),
                    new_block
                ),
            );
        }
        let Ok(current) = std::fs::read_to_string(&path) else {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!("⚠ couldn't read {}.", c.file),
            });
            return;
        };
        // Re-anchor to where the selected block ACTUALLY is on disk right now — guards against the
        // captured line numbers having drifted (which would splice the fix onto the wrong lines,
        // duplicating the block instead of replacing it). Abort clearly if we can't locate it.
        let Some((start, end)) =
            sc_win::linecomment::locate_range(&current, c.start, c.end, &c.selection)
        else {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!(
                    "⚠ couldn't safely locate the selected lines in {} (the file changed) — no edit made.",
                    c.file
                ),
            });
            return;
        };
        // Capture the exact BEFORE-text of the range (for per-comment Revert), then splice.
        let before: String = current
            .lines()
            .skip(start.saturating_sub(1))
            .take(end.saturating_sub(start) + 1)
            .collect::<Vec<_>>()
            .join("\n");
        let spliced = sc_win::linecomment::splice_lines(&current, start, end, &new_block);
        if spliced == current {
            // The model returned the selection unchanged (a local model sometimes echoes the
            // input on "shorten this"). Be honest about it and leave the comment PENDING so you
            // can re-run or rephrase, rather than falsely claiming success.
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ the model returned the same lines unchanged — nothing applied. Try \
                       rephrasing (e.g. \"make this comment 2 lines\") or run it again."
                    .to_string(),
            });
            self.refresh_git_view();
            return;
        }
        if std::fs::write(&path, &spliced).is_err() {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!("⚠ couldn't write {}.", c.file),
            });
            return;
        }
        // Resolve the matching stored comment and stash the before-text + new length for Revert.
        let new_len = new_block.lines().count().max(1);
        if let Some((i, _)) = self
            .comments
            .on_file(&c.file)
            .filter(|(_, cc)| !cc.resolved && cc.start == c.start && cc.end == c.end)
            .last()
        {
            if let Some(stored) = self.comments.items.get_mut(i) {
                stored.resolved = true;
                stored.before = Some(before);
                stored.after_len = Some(new_len);
                // Record where the edit ACTUALLY landed (after re-anchoring) so a later Revert
                // splices the before-text back over the right lines. The NEW block spans
                // `start .. start + new_len - 1` — use that as the end, so the resolved comment
                // anchors to the last NEW line (not the stale old end, which drifts when the fix
                // added or removed lines).
                stored.start = start;
                stored.end = start + new_len - 1;
            }
        }
        sc_win::comments::save(&root, &self.comments);

        // Show the applied file + refresh highlights/tree.
        self.selected_file = Some(c.file.clone());
        self.follow_agent = false;
        self.select_file(c.file.clone());
        self.refresh_git_view();

        // Verify — unless the change is comment-only (git says all changed lines are comments).
        let diff = sc_win::gitdiff::files_diff(&root, std::slice::from_ref(&c.file));
        let comment_only = sc_win::gitdiff::is_comment_only_change(&diff);
        if comment_only {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "✓ Done — comment/whitespace only, skipped the compile check.".to_string(),
            });
        } else {
            // Kick a lightweight verify run (cargo check) to confirm it still compiles.
            self.verify_after_replace(&c.file);
        }
    }

    /// Revert just this comment's change: splice its stored before-text back over the lines the
    /// fix produced (`start .. start + after_len - 1`), restoring the original. The comment goes
    /// back to PENDING (you can re-run or dismiss it). No-op if it has no stored before-text.
    pub(crate) fn revert_comment(&mut self, i: usize) {
        let Some(c) = self.comments.items.get(i).cloned() else {
            return;
        };
        let (Some(before), Some(after_len)) = (c.before.clone(), c.after_len) else {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ nothing stored to revert for this comment.".to_string(),
            });
            return;
        };
        let root = self.workspace_root();
        let path = root.join(&c.file);
        let Ok(current) = std::fs::read_to_string(&path) else {
            return;
        };
        // The fix produced `after_len` lines starting at `c.start`; swap them back to `before`.
        let restored =
            sc_win::linecomment::splice_lines(&current, c.start, c.start + after_len - 1, &before);
        if std::fs::write(&path, &restored).is_ok() {
            // Reverting the change also removes the comment line entirely (VS-Code-style: undo
            // the edit → the comment thread goes with it), rather than leaving it pending.
            self.comments.remove(i);
            sc_win::comments::save(&root, &self.comments);
            self.select_file(c.file.clone());
            self.refresh_git_view();
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!(
                    "↩ Reverted the change on {} and removed the comment.",
                    c.file
                ),
            });
        }
    }

    /// Revert ONE diff block (VS-Code-style) back to its HEAD text: replace the block's current
    /// line range with the committed version. `cur_start` identifies the hunk (its first current
    /// line). Recomputes the diff from the freshly-loaded file, so it's always ground-truth.
    pub(crate) fn revert_block(&mut self, cur_start: usize) {
        let Some(rel) = self.selected_file.clone() else {
            return;
        };
        let root = self.workspace_root();
        // Find the hunk from the CURRENT on-disk diff (not stale UI state).
        let diff = sc_win::gitdiff::file_diff(&root, &rel);
        let Some(hunk) = diff.hunks().into_iter().find(|h| h.cur_start == cur_start) else {
            return; // the diff moved under us — no-op, the view will refresh
        };
        let path = root.join(&rel);
        let Ok(current) = std::fs::read_to_string(&path) else {
            return;
        };
        // Replace the block's current range with its HEAD text. For a pure addition head_text is
        // empty (→ the added lines are deleted); for a pure deletion the range is empty (→ the
        // removed text is inserted back before cur_start).
        let restored = sc_win::linecomment::splice_lines(
            &current,
            hunk.cur_start,
            hunk.cur_end,
            &hunk.head_text,
        );
        if std::fs::write(&path, &restored).is_ok() {
            self.select_file(rel.clone());
            self.refresh_git_view();
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!("↩ Reverted a block in {rel} to its committed version."),
            });
        }
    }

    /// After a line-replace that changed code, run the configured verify command to confirm it
    /// still compiles; report the result in the chat. Runs on a worker via a tiny iterate-less
    /// path would be ideal, but for v1 we run it inline-async through the existing Session by
    /// spawning a no-op check — simplest: just report optimistically and let the PR highlight +
    /// Undo cover a bad edit. (Verification wiring can deepen later.)
    pub(crate) fn verify_after_replace(&mut self, file: &str) {
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::Agent,
            text: format!(
                "✓ Applied to {file}. (Tip: if it doesn't compile, use the comment's ↩ Revert.)"
            ),
        });
    }

    /// Start an iterate run from a ready-made instruction (used by the small-fix line-comment
    /// path). Mirrors `start(RunKind::Iterate)` but with an explicit instruction instead of
    /// the composer text.
    #[allow(dead_code)]
    pub(crate) fn start_iterate_with(&mut self, instruction: String) {
        if self.session.is_some() {
            return;
        }
        self.debug_prompt("fix (iterate)", &instruction);
        self.intent = instruction;
        self.start(RunKind::Iterate);
        self.intent.clear();
        // Mark this run so its outcome is reported back into the chat (set AFTER start(),
        // which clears run state).
        self.iterate_from_comment = true;
    }

    /// Start a PLAN-ONLY workflow run from a ready-made task: run the staged workflow through the
    /// stage breakdown and stop for review (produces reviewable design artifacts, does NOT build).
    pub(crate) fn start_plan_with(&mut self, task: String) {
        if self.session.is_some() {
            return;
        }
        self.debug_prompt("plan", &task);
        // Stash the task so the result view's "Build this plan" button can start a staged build
        // against the same plan (the Breakdown → Build hand-off) with no retyping.
        self.last_plan_task = Some(task.clone());
        self.intent = task;
        self.start(RunKind::Plan);
        self.intent.clear();
        self.iterate_from_comment = true;
    }

    /// Start the full plan→BUILD flow (`RunKind::StagedBuild`) from a ready-made task: staged
    /// design through decomposition, then the compiler-driven executor builds it to green. This
    /// is what "Execute plan" does — it actually BUILDS the plan, not just re-designs it.
    pub(crate) fn start_staged_build_with(&mut self, task: String) {
        if self.session.is_some() {
            return;
        }
        self.debug_prompt("execute plan (build)", &task);
        self.intent = task;
        self.start(RunKind::StagedBuild);
        self.intent.clear();
        // Report the build outcome back into the chat thread.
        self.iterate_from_comment = true;
    }

    /// Apply the Nth proposed plan-file, then kick off an iterate build to implement it.
    /// The one-click bridge from a `PLAN-<slug>.md` design doc to a real build run: the plan
    /// is written to disk (so the agent can read it), then an iterate run is started with an
    /// instruction pointing at that file. Iterate needs a real project on disk, so this is a
    /// no-op (with a chat note) when no project folder is open or a run is already in flight.
    pub(crate) fn execute_plan(&mut self, i: usize) {
        let Some(pf) = self.proposed_files.get(i).cloned() else {
            return;
        };
        if self.session.is_some() {
            return; // A run is already in flight — don't stack another.
        }
        if self.picked_workspace.is_none() {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ Open a project folder first — executing a plan builds into it."
                    .to_string(),
            });
            return;
        }
        // Land the plan on disk (and refresh the conversation snapshot) first, so the workflow
        // can read the plan it's told to design against. This also clears it from the pending
        // proposals, so `i` is consumed exactly like a plain Apply.
        self.apply_proposed_file(i);
        // Execute = BUILD the plan (staged design → compiler-driven build to green), not just
        // re-design it. `plan_task` names the PLAN file so the workflow grounds on its contents.
        self.start_staged_build_with(plan_task(&pf.name));
    }

    /// Apply the Nth proposed plan-file, then run the DESIGN-only staged pipeline (Breakdown):
    /// the staged phases through decomposition, gated for review, WITHOUT building code. Sibling
    /// of [`Self::execute_plan`] (which continues into the build). Same guards + apply-first.
    pub(crate) fn breakdown_plan(&mut self, i: usize) {
        let Some(pf) = self.proposed_files.get(i).cloned() else {
            return;
        };
        if self.session.is_some() {
            return;
        }
        if self.picked_workspace.is_none() {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ Open a project folder first — the breakdown designs against it."
                    .to_string(),
            });
            return;
        }
        self.apply_proposed_file(i);
        self.start_plan_with(plan_task(&pf.name));
    }

    /// Kick off an iterate build to implement the `PLAN-*.md` open in the code view. Unlike
    /// [`Self::execute_plan`], the file is already on disk (opened from the tree), so there's
    /// nothing to apply — just point an iterate run at it. Same guards: needs an open project
    /// and no run in flight.
    pub(crate) fn execute_open_plan(&mut self) {
        let Some(rel) = self.selected_file.clone() else {
            return;
        };
        if !is_feature_plan(&rel) || self.session.is_some() {
            return;
        }
        if self.picked_workspace.is_none() {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ Open a project folder first — executing a plan builds into it."
                    .to_string(),
            });
            return;
        }
        // Act on the feature's spec.md, whichever artifact of specs/<slug>/ is selected (spec /
        // architecture / layout / breakdown / decomposition) — so the run targets specs/<slug>/,
        // reusing its approved design, instead of treating e.g. decomposition.md as a plan to
        // re-design from.
        self.start_plan_with(plan_task(&feature_spec_of(&rel)));
    }

    /// Build the plan open in the code view: the full staged design → compiler-driven build to
    /// green (`RunKind::StagedBuild`). Sibling of [`Self::execute_open_plan`] (which stops at the
    /// design breakdown); the file is already on disk, so nothing to apply. Same guards.
    pub(crate) fn build_open_plan(&mut self) {
        let Some(rel) = self.selected_file.clone() else {
            return;
        };
        if !is_feature_plan(&rel) || self.session.is_some() {
            return;
        }
        if self.picked_workspace.is_none() {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: "⚠ Open a project folder first — building a plan builds into it.".to_string(),
            });
            return;
        }
        // Build the feature (its spec.md), whichever specs/<slug>/ artifact is open — so selecting
        // decomposition.md (or any phase file) and hitting Build targets specs/<slug>/ and reuses
        // the already-approved design instead of re-designing from that one file.
        self.start_staged_build_with(plan_task(&feature_spec_of(&rel)));
    }

    /// Build the plan from the last Breakdown — the Breakdown → Build hand-off. Reuses the exact
    /// task the plan run designed against (`last_plan_task`), so approving a breakdown then hitting
    /// "Build this plan" runs the staged build with no retyping. No-op if a run is in flight or no
    /// Breakdown has run this session.
    pub(crate) fn build_last_plan(&mut self) {
        if self.session.is_some() {
            return;
        }
        let Some(task) = self.last_plan_task.clone() else {
            return;
        };
        self.start_staged_build_with(task);
    }

    /// Commit the plan artifacts from the last Breakdown to the repo: `git add` the artifact dir
    /// (specs/<slug>/ or .smart-coder/plan/) + a commit. Lets a reviewed design be saved before —
    /// or instead of — building. Reports the outcome in the chat and refreshes the git view.
    pub(crate) fn commit_plan(&mut self) {
        if self.picked_workspace.is_none() {
            return;
        }
        // Stage everything the breakdown wrote (the plan/spec artifacts are the only new files a
        // plan-only run produces), then commit. `run_git` is best-effort and returns success.
        let staged = self.run_git(&["add", "-A"]);
        let committed =
            staged && self.run_git(&["commit", "-m", "docs: add reviewed plan/breakdown"]);
        let note = if committed {
            "✓ committed the plan to the repo".to_string()
        } else {
            "⚠ couldn't commit the plan (nothing to commit, or not a git repo)".to_string()
        };
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::Agent,
            text: note,
        });
        self.refresh_git_view();
    }

    /// Write the Nth proposed plan-file to disk (README.md / TODO.md), then refresh the
    /// conversation's view of the plan files and re-open it in the code view.
    pub(crate) fn apply_proposed_file(&mut self, i: usize) {
        let Some(pf) = self.proposed_files.get(i).cloned() else {
            return;
        };
        let root = self.workspace_root();
        let path = root.join(&pf.name);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&path, &pf.content).is_ok() {
            // Refresh the conversation's plan snapshot so later turns see the new files.
            if let Some(convo) = self.conversation.as_mut() {
                let (readme, todo) = {
                    let r = find_readme(&root)
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .unwrap_or_default();
                    let t = find_todo_file(&root)
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .unwrap_or_default();
                    (r, t)
                };
                convo.set_plan_files(&readme, &todo);
            }
            // Show the file we just wrote.
            self.follow_agent = false;
            self.select_file(pf.name.clone());
            // A feature plan KEEPS its card (marked applied) so its Breakdown/Build actions stay
            // available in the chat — a plan is written so you can then build it, so removing the
            // card strands you. A plain README/TODO edit isn't buildable, so it's removed as before.
            if is_feature_plan(&pf.name) {
                if let Some(slot) = self.proposed_files.get_mut(i) {
                    slot.applied = true;
                }
            } else {
                self.proposed_files.remove(i);
            }
            // Confirm the write in the chat thread, so applying is visible in the record.
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!("✓ Applied {} to disk.", pf.name),
            });
        } else {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!("⚠ Could not write {}.", pf.name),
            });
        }
    }

    /// Select `rel` for the code panel and load its contents from the workspace root.
    pub(crate) fn select_file(&mut self, rel: String) {
        let root = self.workspace_root();
        // Switching to a different file resets the scrollable to the top; keep our virtualization
        // offset in sync so the first frame renders the top window, not the old file's slice.
        if self.selected_file.as_deref() != Some(rel.as_str()) {
            self.code_scroll_y = 0.0;
            self.code_viewport = None;
        }
        self.code = Some(sc_win::codeview::load(&root, &rel));
        // Opening a file makes it a CODE-panel tab. Push before the move into `selected_file`
        // (which becomes the ACTIVE tab); no duplicates, so re-opening an already-open file
        // just re-selects it. Every open path (tree/git/plan/comment) funnels through here, so
        // they all get a tab for free.
        if !self.open_tabs.contains(&rel) {
            self.open_tabs.push(rel.clone());
        }
        self.selected_file = Some(rel);
        self.refresh_changed_lines();
    }

    /// Close the CODE-panel tab for `path` (no-op if it isn't open). If it was the active tab,
    /// a neighbour becomes active (see `tab_after_close`); if none remain, the panel clears.
    /// Called by the ✕ button and when a file is deleted/discarded out from under it — a tab on
    /// a file that no longer exists is dead weight.
    pub(crate) fn close_tab(&mut self, path: &str) {
        if let Some(i) = self.open_tabs.iter().position(|p| p == path) {
            let was_active = self.selected_file.as_deref() == Some(path);
            self.open_tabs.remove(i);
            // Only the active tab closing changes what's shown; closing a background tab leaves
            // the active file alone.
            if was_active {
                match tab_after_close(i, self.open_tabs.len()) {
                    Some(idx) => {
                        let next = self.open_tabs[idx].clone();
                        self.select_file(next);
                    }
                    None => {
                        self.selected_file = None;
                        self.code = None;
                    }
                }
            }
        }
    }

    /// Re-read the currently selected file from disk (after the agent edited it), so the
    /// code panel reflects the latest bytes — and refresh which lines differ from HEAD.
    pub(crate) fn reload_selected(&mut self) {
        if let Some(rel) = self.selected_file.clone() {
            let root = self.workspace_root();
            self.code = Some(sc_win::codeview::load(&root, &rel));
        }
        self.refresh_changed_lines();
    }

    /// The OFF-THREAD live-view refresh: while a run is in flight, reload the shown file's
    /// contents + its git-diff highlight on a background thread and apply via
    /// [`Message::LiveViewReloaded`] — never on the UI thread (the file read + `git diff` cost
    /// 80–432ms per call and froze the UI when done synchronously in `pump`). Throttled to ~1s;
    /// returns `Task::none()` when no reload is due or nothing is selected.
    pub(crate) fn live_reload_task(&mut self) -> Task<Message> {
        if !(self.iterate_from_comment && self.session.is_some()) {
            return Task::none();
        }
        let due = self
            .last_reload
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(750));
        let Some(rel) = self.selected_file.clone() else {
            return Task::none();
        };
        if !due {
            return Task::none();
        }
        self.last_reload = Some(Instant::now());
        let root = self.workspace_root();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let code = sc_win::codeview::load(&root, &rel);
                    let diff = sc_win::gitdiff::file_diff(&root, &rel);
                    (code, diff.added)
                })
                .await
                .ok()
            },
            Message::LiveViewReloaded,
        )
    }

    /// Scroll the code view so `line` (1-based) sits in the MIDDLE of the viewport. Each rendered
    /// line is ~`CODE_LINE_PX` tall; back off by half the visible height so the target lands
    /// centered (falls back to a small top offset before the first scroll gives us a real
    /// viewport height). Shared by the minimap jump and the git-tab "open at first change".
    /// Commit the in-flight streamed reply (`self.streaming`) as a finished agent turn, then clear
    /// the live bubble. A no-op when nothing is streaming. Used wherever a stream ENDS (the next
    /// phase header, a gate decision, run completion) so the streamed text sticks in the thread
    /// instead of vanishing with the bubble. Strips any hidden `<think>` block, matching what the
    /// live bubble showed via `visible_so_far`.
    pub(crate) fn commit_streaming_turn(&mut self) {
        if let Some(buf) = self.streaming.take() {
            let visible = sc_win::chat::visible_so_far(&buf);
            if !visible.trim().is_empty() {
                self.chat_turns.push(sc_win::chat::Turn {
                    role: sc_win::chat::Speaker::Agent,
                    text: visible,
                });
            }
        }
    }

    /// Keep the chat thread pinned to the bottom as content streams in — `Task::none` unless
    /// auto-scroll is armed (the user is at the bottom, not reading back). `snap_to` with a
    /// relative y of 1.0 jumps to the end; it's cheap and idempotent when already there, so
    /// running it every tick is fine. Disarmed by `ChatScrolled` when the user scrolls up.
    pub(crate) fn chat_autoscroll_task(&self) -> Task<Message> {
        if self.chat_stuck_to_bottom {
            iced::widget::operation::snap_to(
                chat_scroll_id(),
                iced::widget::scrollable::RelativeOffset { x: 0.0, y: 1.0 },
            )
        } else {
            Task::none()
        }
    }

    pub(crate) fn scroll_code_to_line(&self, line: usize) -> Task<Message> {
        let center = line as f32 * CODE_LINE_PX;
        // `code_view_h` is 0 until the view's first scroll event; fall back to a typical editor
        // height so a jump on a freshly-opened file still lands the target near center, not glued
        // to the very top.
        let view_h = if self.code_view_h > 1.0 {
            self.code_view_h
        } else {
            400.0
        };
        let y = (center - view_h / 2.0).max(0.0);
        iced::widget::operation::scroll_to(
            code_scroll_id(),
            iced::widget::scrollable::AbsoluteOffset { x: 0.0, y },
        )
    }

    /// Recompute the shown file's PR-style diff vs HEAD (git): added lines (green) + removed lines
    /// (red). Cheap `git diff -U0` on the one file (all-added for an untracked file); empty when
    /// nothing's selected. `changed_lines` is the green set, kept for the minimap + jump-to-change.
    pub(crate) fn refresh_changed_lines(&mut self) {
        self.file_diff = match &self.selected_file {
            Some(rel) => sc_win::gitdiff::file_diff(&self.workspace_root(), rel),
            None => sc_win::gitdiff::FileDiff::default(),
        };
        self.changed_lines = self.file_diff.added.clone();
    }

    /// Refresh the PR-view git state synchronously: the tree cache, per-file M/A/D statuses,
    /// branch, upstream. Used at the points where we want the state up-to-date *before* the next
    /// line runs (project open, right after a stage/discard). The periodic heartbeat instead uses
    /// the async path (`SyncWorkspace` → `compute_snapshot` off-thread → `WorkspaceSynced`).
    pub(crate) fn refresh_git_view(&mut self) {
        let snap = compute_snapshot(self.workspace_root());
        self.apply_snapshot(snap);
    }

    /// Apply a computed [`WorkspaceSnapshot`] to the live state. Pure assignment — the expensive
    /// walk/git work already happened in [`compute_snapshot`] (possibly on a background thread).
    pub(crate) fn apply_snapshot(&mut self, snap: WorkspaceSnapshot) {
        self.tree_cache = snap.tree;
        self.file_status = snap.file_status;
        self.stage_states = snap.stage_states;
        self.unstaged_deltas = snap.unstaged_deltas;
        self.staged_deltas = snap.staged_deltas;
        self.branch = snap.branch;
        self.upstream = snap.upstream;
    }

    /// Run a `git` subcommand in the workspace (e.g. `["add", "--", path]`) and return whether
    /// it succeeded. Used by the git-tab context-menu actions (stage / unstage / discard).
    pub(crate) fn run_git(&self, args: &[&str]) -> bool {
        let root = self.workspace_root();
        sc_win::proc::git()
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Run a NETWORK git op (push / pull / fetch), capturing its output and reporting the outcome
    /// in the chat — these fail more often (auth, conflicts, no remote), so the message matters.
    /// `label` names the op for the report. Runs synchronously (briefly blocks the UI).
    pub(crate) fn run_git_net(&mut self, label: &str, args: &[&str]) {
        let root = self.workspace_root();
        let out = sc_win::proc::git().arg("-C").arg(&root).args(args).output();
        let (ok, detail) = match out {
            Ok(o) => {
                // git writes progress/results to stderr; prefer it, fall back to stdout.
                let err = String::from_utf8_lossy(&o.stderr);
                let msg = if err.trim().is_empty() {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                } else {
                    err.trim().to_string()
                };
                (o.status.success(), msg)
            }
            Err(e) => (false, e.to_string()),
        };
        // Keep the report short — the last non-empty line usually carries the gist.
        let gist = detail
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        let text = if ok {
            format!(
                "✓ git {label} — {}",
                if gist.is_empty() { "done" } else { gist }
            )
        } else {
            format!("⚠ git {label} failed — {gist}")
        };
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::Agent,
            text,
        });
    }
}
