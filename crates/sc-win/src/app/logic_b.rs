//! Tabs, panes and file selection — the editor half of what this file used to be.
//!
//! Everything above `select_file` was the agent's: applying a line replacement, reverting
//! a comment, starting a run or a plan, applying a proposed file. It left for
//! `sc-plugin-agent` (spec 25). The boundary was clean because the two halves never
//! shared a function — the agent's half drove runs, this half drives what is on screen.

use super::*;
use sc_win::layout::EditorId;

impl App {
    pub(crate) fn select_file(&mut self, rel: String) {
        self.select_file_from(rel, Origin::Tree);
    }

    /// Select `rel` and land in the REVIEW view — for the git panel, where
    /// the intent is to see what changed rather than to type.
    pub(crate) fn select_file_for_review(&mut self, rel: String) {
        self.select_file_from(rel, Origin::Review);
    }

    /// Select `rel`, opening a tab with `origin` deciding the initial view.
    ///
    /// If the file is already open in ANOTHER pane, focus goes there rather than opening a
    /// second copy — see [`Self::select_file_into`].
    pub(crate) fn select_file_from(&mut self, rel: String, origin: Origin) {
        let into = self
            .panes
            .pane_holding(&rel)
            .unwrap_or_else(|| self.panes.focused_id());
        self.select_file_into(rel, origin, into);
    }

    /// Select `rel` in `into`, opening a tab there if it isn't one already.
    ///
    /// **A path lives in exactly one pane.** `Tab` owns its buffer, so the same file open twice
    /// would be two independent buffers over one path — two dirty flags, two disk stamps, and a
    /// single path-keyed save-conflict slot between them. Saving in one would then raise a
    /// conflict in the other reporting a change *you* made seconds ago, whose only offered answer
    /// destroys the first pane's edits. Sharing one buffer across panes needs a document model
    /// the editor widget doesn't have, so until it does, asking for a file that's open elsewhere
    /// takes you there.
    pub(crate) fn select_file_into(&mut self, rel: String, origin: Origin, into: EditorId) {
        // If some pane already holds it, that pane wins over the requested one.
        let into = self.panes.pane_holding(&rel).unwrap_or(into);
        self.panes.focus(into);

        let root = self.workspace_root();
        let switching = self.panes.focused().selected_file.as_deref() != Some(rel.as_str());
        // Switching to a different file resets the scrollable to the top; keep our virtualization
        // offset in sync so the first frame renders the top window, not the old file's slice.
        if switching {
            self.panes.focused_mut().code_scroll_y = 0.0;
            self.panes.focused_mut().code_viewport = None;
        }
        // The REVIEW view is rebuilt from disk on every selection — it is a rendering of the
        // file, not a buffer, so it must show current bytes.
        self.panes.focused_mut().code = Some(sc_win::codeview::load(&root, &rel));
        // Re-selecting an ALREADY-OPEN tab must not re-open it: that would throw away its
        // buffer, and with it any unsaved edits. Just make it active.
        if !self.panes.focused().tabs.iter().any(|t| t.path == rel) {
            let abs = root.join(&rel);
            self.panes
                .focused_mut()
                .tabs
                .push(Tab::open(rel.clone(), &abs, origin));
        }
        self.panes.focused_mut().selected_file = Some(rel);
        self.refresh_changed_lines();
    }

    /// The active tab, if any.
    pub(crate) fn active_tab(&self) -> Option<&Tab> {
        let sel = self.panes.focused().selected_file.as_deref()?;
        self.panes.focused().tabs.iter().find(|t| t.path == sel)
    }

    /// The active tab, mutably.
    pub(crate) fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        let sel = self.panes.focused().selected_file.clone()?;
        self.panes
            .focused_mut()
            .tabs
            .iter_mut()
            .find(|t| t.path == sel)
    }

    /// Whether the tab for `rel` has unsaved edits. Used to guard every path that would
    /// overwrite or reload a file behind the editor's back.
    pub(crate) fn is_dirty(&self, rel: &str) -> bool {
        // A question about the FILE, so it asks every pane — a path lives in exactly one of
        // them, but which one is not this caller's business.
        self.panes.is_dirty(rel)
    }

    /// Close the CODE-panel tab for `path` (no-op if it isn't open). If it was the active tab,
    /// a neighbour becomes active (see `tab_after_close`); if none remain, the panel clears.
    /// Called by the ✕ button and when a file is deleted/discarded out from under it — a tab on
    /// a file that no longer exists is dead weight.
    ///
    /// A tab with unsaved edits is NOT closed — it raises the confirm prompt instead. Losing
    /// typing to a stray click on a ✕ is exactly the kind of quiet data loss the editor has to
    /// rule out.
    pub(crate) fn close_tab(&mut self, path: &str) {
        if self.is_dirty(path) {
            self.confirm_close = Some(path.to_string());
            return;
        }
        self.force_close_tab(path);
    }

    /// Close `path` without the dirty check — the "Discard" answer to the confirm prompt, and
    /// the path used when a file has been deleted out from under its tab.
    pub(crate) fn force_close_tab(&mut self, path: &str) {
        if let Some(i) = self
            .panes
            .focused()
            .tabs
            .iter()
            .position(|t| t.path == path)
        {
            let was_active = self.panes.focused().selected_file.as_deref() == Some(path);
            self.panes.focused_mut().tabs.remove(i);
            if self.confirm_close.as_deref() == Some(path) {
                self.confirm_close = None;
            }
            // Only the active tab closing changes what's shown; closing a background tab leaves
            // the active file alone.
            if was_active {
                match tab_after_close(i, self.panes.focused().tabs.len()) {
                    Some(idx) => {
                        let next = self.panes.focused().tabs[idx].path.clone();
                        self.select_file(next);
                    }
                    None => {
                        self.panes.focused_mut().selected_file = None;
                        self.panes.focused_mut().code = None;
                        // A pane with no tabs left closes itself — otherwise it is a dead
                        // rectangle with no tab to drag and no way to dismiss it.
                        self.prune_empty_panes();
                    }
                }
            }
        }
    }

    /// Re-read the currently selected file from disk (after the agent edited it), so the
    /// code panel reflects the latest bytes — and refresh which lines differ from HEAD.
    pub(crate) fn reload_selected(&mut self) {
        if let Some(rel) = self.panes.focused().selected_file.clone() {
            let root = self.workspace_root();
            self.panes.focused_mut().code = Some(sc_win::codeview::load(&root, &rel));
            // The review view above is derived from disk, so refreshing it is always safe. The
            // tab's BUFFER is not — `reload_tab_from_disk` refuses while it's dirty, so an
            // agent write can never erase what you were typing.
            self.reload_tab_from_disk(&rel);
        }
        self.refresh_changed_lines();
    }

    /// Turn an armed diff request into an off-thread `git diff`.
    ///
    /// `file_diff` costs ~50ms on this machine, ~26ms of which is bare process spawn
    /// (`git --version`, which does nothing, costs the same). Blocking the UI thread on
    /// that is the file-click freeze; this is where it stops.
    pub(crate) fn diff_task(&mut self) -> Task<Message> {
        let Some(rel) = self.take_diff_request() else {
            return Task::none();
        };
        let root = self.workspace_root();
        let for_msg = rel.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || sc_win::gitdiff::file_diff(&root, &rel))
                    .await
                    .unwrap_or_default()
            },
            move |diff| Message::FileDiffReady(for_msg.clone(), Box::new(diff)),
        )
    }

    /// Scroll `pane`'s code view so `line` sits near the middle.
    ///
    /// Takes the pane explicitly rather than assuming the focused one: the height it centres
    /// against and the scrollable it targets both belong to a specific pane, and a jump-to-line
    /// triggered by a compile diagnostic may well be aimed somewhere other than where focus is.
    pub(crate) fn scroll_code_to_line(&self, pane: EditorId, line: usize) -> Task<Message> {
        let center = line as f32 * CODE_LINE_PX;
        // `code_view_h` is a guess until that pane's first scroll event; fall back to a typical
        // editor height so a jump on a freshly-opened file still lands near centre rather than
        // glued to the very top.
        let view_h = match self.panes.get(pane) {
            Some(p) if p.code_view_h > 1.0 => p.code_view_h,
            _ => 400.0,
        };
        let y = (center - view_h / 2.0).max(0.0);
        iced::widget::operation::scroll_to(
            code_scroll_id(pane),
            iced::widget::scrollable::AbsoluteOffset { x: 0.0, y },
        )
    }

    /// Recompute the shown file's PR-style diff vs HEAD (git): added lines (green) + removed lines
    /// (red). Cheap `git diff -U0` on the one file (all-added for an untracked file); empty when
    /// nothing's selected. `changed_lines` is the green set, kept for the minimap + jump-to-change.
    /// Paint the focused pane's diff from cache, and return a task to compute it if it
    /// is not cached yet.
    ///
    /// This used to call `file_diff` inline, on the UI thread, on every file click.
    /// Measured on this machine that is ~50ms per click — and ~26ms of it is bare
    /// process spawn, since `git --version` (which does nothing) costs the same. There
    /// is no making the git call fast enough to block on; the only fix is not to block.
    ///
    /// So a cached diff paints immediately and a miss paints the file with no diff
    /// highlight, which arrives a frame or two later. The wrong-looking alternative —
    /// keeping the previous file's highlight until the new one lands — would draw red
    /// and green lines against the wrong source.
    pub(crate) fn refresh_changed_lines(&mut self) {
        let Some(rel) = self.panes.focused().selected_file.clone() else {
            let pane = self.panes.focused_mut();
            pane.changed_lines = Default::default();
            pane.file_diff = sc_win::gitdiff::FileDiff::default();
            return;
        };
        if let Some(diff) = self.diff_cache.get(&rel).cloned() {
            let pane = self.panes.focused_mut();
            pane.changed_lines = diff.added.clone();
            pane.file_diff = diff;
            return;
        }
        // A miss clears the stale highlight and ARMS the computation; the next tick
        // turns that into an off-thread task (`take_diff_request`). Arming rather than
        // returning a `Task` keeps the dozen callers of this function unchanged --
        // several are deep in save/reload paths that have no `Task` to return.
        {
            let pane = self.panes.focused_mut();
            pane.changed_lines = Default::default();
            pane.file_diff = sc_win::gitdiff::FileDiff::default();
        }
        self.diff_wanted = Some(rel);
    }

    /// The pending diff request, if one is due and nothing is already in flight.
    ///
    /// One computation at a time: clicking through ten files must not spawn ten git
    /// processes, and only the last file's diff is wanted anyway.
    pub(crate) fn take_diff_request(&mut self) -> Option<String> {
        if self.diff_pending.is_some() {
            return None;
        }
        let rel = self.diff_wanted.take()?;
        self.diff_pending = Some(rel.clone());
        Some(rel)
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
        // A new snapshot means the working tree moved, so every cached diff is suspect.
        // Clearing wholesale rather than per-file: the snapshot does not say WHICH files
        // changed, and a stale green line is a lie about the user's own code.
        self.diff_cache.clear();
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
        // Reported into the TERMINAL. The chat thread was where this went, and it left
        // with the agent (spec 25) — but a push that silently fails is the one git
        // outcome a user must not miss, so it needs somewhere to land.
        self.terminal.note(if ok {
            format!(
                "git {label} — {}",
                if gist.is_empty() { "done" } else { gist }
            )
        } else {
            format!("git {label} failed — {gist}")
        });
    }
}
