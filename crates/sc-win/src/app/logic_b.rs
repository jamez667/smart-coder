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

    /// Start a NETWORK git op (push / pull / fetch) OFF the UI thread, reporting the outcome when
    /// it lands (`GitNetDone` → [`Self::finish_git_net`]). These fail more often than local git
    /// (auth, conflicts, no remote), so the message matters.
    ///
    /// Off-thread because these are the one git path that is not merely *slow* but unbounded: a
    /// network round trip, a TLS handshake, possibly a credential prompt. Run on the UI thread the
    /// window simply stops repainting for the duration — and since iced cannot paint mid-`update`,
    /// there is no way to show a spinner from inside the blocking call. `git_net` is the guard, so
    /// a second click while one is in flight is dropped rather than stacking a second pull.
    pub(crate) fn start_git_net(&mut self, label: &str, args: &[&str]) -> Task<Message> {
        if self.git_net.is_some() {
            return Task::none(); // one network op at a time
        }
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.git_net_cancel = Some(cancel.clone());
        self.git_net = Some(label.to_string());
        let root = self.workspace_root();
        let label = label.to_string();
        // Owned before the move — `args` is borrowed from the caller's frame.
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        Task::perform(
            async move {
                let done =
                    tokio::task::spawn_blocking(move || run_git_net_op(&root, &args, &cancel))
                        .await;
                // A panicked worker must still clear the in-flight flag, or the buttons stay dead
                // for the rest of the session.
                done.unwrap_or_else(|e| (false, format!("the git thread panicked: {e}")))
            },
            move |(ok, gist)| Message::GitNetDone(label.clone(), ok, gist),
        )
    }

    /// Ask the in-flight network git op to stop.
    ///
    /// Only sets the flag; the worker does the killing and still reports through `GitNetDone`, so
    /// there is exactly one path that clears `git_net` and refreshes the view.
    pub(crate) fn cancel_git_net(&mut self) {
        if let Some(c) = &self.git_net_cancel {
            c.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Report a finished network git op and refresh the git view off-thread.
    ///
    /// The refresh goes through the async snapshot path rather than [`Self::refresh_git_view`]: a
    /// pull that just rewrote the tree is exactly when the walk is most expensive, and blocking
    /// here would undo the point of having run the pull off-thread.
    pub(crate) fn finish_git_net(&mut self, label: &str, ok: bool, gist: &str) -> Task<Message> {
        self.git_net = None;
        self.git_net_cancel = None;
        // Reported into the TERMINAL. The chat thread was where this went, and it left
        // with the agent (spec 25) — but a push that silently fails is the one git
        // outcome a user must not miss, so it needs somewhere to land.
        self.terminal.note(if ok {
            format!(
                "git {label} — {}",
                if gist.is_empty() { "done" } else { gist }
            )
        } else if gist == "cancelled" {
            // The user pressed ✕. Reporting their own choice as a failure would be a lie.
            format!("git {label} cancelled")
        } else {
            format!("git {label} failed — {gist}")
        });
        if self.sync_pending {
            return Task::none(); // a walk is already coming; it will pick this up
        }
        self.sync_pending = true;
        let root = self.workspace_root();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || compute_snapshot(root))
                    .await
                    .ok()
            },
            Message::WorkspaceSynced,
        )
    }
}

/// Run one network `git` invocation to completion, killable partway through.
///
/// Mirrors `run_record` in [`super::logic_flame`]: helper threads drain the pipes so a chatty child
/// cannot deadlock on a full buffer, and exit is polled rather than waited on, so cancelling is
/// responsive instead of taking effect whenever git happens to finish.
///
/// **stderr, not stdout**, is the interesting stream: git writes progress and its failures there,
/// and stdout is near-empty for these ops.
fn run_git_net_op(
    root: &std::path::Path,
    args: &[String],
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> (bool, String) {
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::atomic::Ordering;

    let mut child = match sc_win::proc::git()
        .arg("-C")
        .arg(root)
        .args(args)
        // An inherited stdin is how a credential prompt wedges the op forever with nothing on
        // screen: git waits on a terminal this GUI does not have. Closing it makes git fail fast
        // with "could not read Username" instead, which is a message we can actually show.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (false, e.to_string()),
    };

    // Drain both pipes on helper threads. git writes progress to stderr continuously during a
    // fetch; leaving it unread risks filling the pipe and stalling the child.
    let mut err_pipe = child.stderr.take();
    let mut out_pipe = child.stdout.take();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_string(&mut s);
        }
        s
    });
    let out_reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_string(&mut s);
        }
        s
    });

    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            // The TREE, not the child. Measured: `git pull` over ssh is three processes deep —
            // `git` spawns a second `git.exe`, which spawns `ssh`. Killing only the process we
            // hold leaves `ssh` orphaned and still holding the connection, which is the hang we
            // are trying to escape.
            sc_win::proc::kill_tree(child.id());
            let _ = child.kill();
            // Return WITHOUT joining the reader threads. The outcome is already known — the user
            // asked to stop — and those threads block until every handle on the pipes closes,
            // which includes the grandchildren `taskkill` is still unwinding. Joining here cost 21
            // seconds of a button that says "Pulling…" after the cancel had already happened. The
            // threads are detached and exit on their own once the pipes close; nothing leaks that
            // outlives the process.
            let _ = child.wait();
            return (false, "cancelled".to_string());
        }
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => return (false, e.to_string()),
        }
    };

    let stderr = err_reader.join().unwrap_or_default();
    let stdout = out_reader.join().unwrap_or_default();
    // git writes progress/results to stderr; prefer it, fall back to stdout.
    let msg = if stderr.trim().is_empty() {
        stdout.trim().to_string()
    } else {
        stderr.trim().to_string()
    };
    // Keep the report short — the last non-empty line carries the gist.
    let gist = msg
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_string();
    (status.map(|s| s.success()).unwrap_or(false), gist)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Cancelling a hung network op actually returns, and says it was cancelled.
    ///
    /// The case this guards is the one the ✕ exists for: a `git pull` that will never finish on its
    /// own. `10.255.255.1` is routable-but-dead, so the ssh connect blocks until TCP gives up
    /// (minutes) — if the cancel path did not kill the child, this test would hang with it rather
    /// than fail, which is precisely the bug's signature.
    #[test]
    fn cancel_kills_a_hung_network_op() {
        // A FIXED path, not one per pid: see the cleanup note at the end of this test. Windows may
        // leave the emptied directory behind, and a per-pid name would litter TEMP with one dead
        // shell per run. `remove_dir_all` at the top makes the reuse safe.
        let dir = std::env::temp_dir().join("sc-git-cancel-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        // A repo whose only remote is a black hole.
        let ok = sc_win::proc::git()
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return; // no usable git on PATH — nothing to assert about
        }
        // Guards against this test passing VACUOUSLY: if the repo were not really created, the
        // pull below would fail instantly for the wrong reason and the cancel path would never be
        // exercised, yet the test would still report success.
        assert!(
            dir.join(".git").is_dir(),
            "the fixture repo was not created"
        );
        let _ = sc_win::proc::git()
            .arg("-C")
            .arg(&dir)
            .args(["remote", "add", "origin", "ssh://git@10.255.255.1/nope.git"])
            .status();

        let cancel = Arc::new(AtomicBool::new(false));
        let flip = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(600));
            flip.store(true, Ordering::Relaxed);
        });

        let started = std::time::Instant::now();
        let args = vec!["pull".to_string(), "--ff-only".to_string()];
        let (ok, gist) = super::run_git_net_op(&dir, &args, &cancel);

        assert!(!ok, "a cancelled pull is not a success");
        assert_eq!(gist, "cancelled", "the outcome must name itself cancelled");
        // Tight on purpose. A 30s bound here passed happily while cancel actually took 21s,
        // because the reader threads were joined after the kill and blocked on pipes the orphaned
        // `ssh` grandchild still held. The flag fires at 600ms, so anything past a few seconds
        // means that regression is back.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "cancel took {:?}; it should return as soon as the tree is killed, without waiting \
             on the reader threads",
            started.elapsed()
        );
        // Best-effort, and retried: `kill_tree` is non-blocking by design, so for a second or two
        // the doomed `git`/`ssh` grandchildren still hold handles inside `.git`. The retry gets the
        // contents deleted; Windows can still refuse the final rmdir of the emptied directory
        // while its handle finishes closing, which is why the fixture path is FIXED rather than
        // per-pid — a leftover empty shell is then reused by the next run instead of accumulating
        // one stray directory in TEMP forever.
        for _ in 0..20 {
            if std::fs::remove_dir_all(&dir).is_ok() || !dir.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
}
