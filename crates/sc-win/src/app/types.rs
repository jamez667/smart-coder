//! Core application types: App state, Message, small UI enums.

use super::*;

pub(crate) enum Gatebar {
    Confirm {
        /// Remote-correlation id, set when the mirror is active so a phone `/approve id`
        /// can find this entry (0 when there's no remote mirror). Local buttons ignore it.
        // Retained: constructing it drives `register_confirm`'s remote-side registration, and it
        // documents the entry's mirror identity; nothing reads it back on the local path yet.
        #[allow(dead_code)]
        id: u64,
        command: String,
        reason: String,
        reply: Sender<Confirmation>,
    },
    Gate {
        phase: Phase,
        /// The artifact text as produced. Retained from the `Pending::Gate` request but no longer
        /// displayed inline — the master list opens the on-disk file in CODE instead (Change A).
        #[allow(dead_code)]
        content: String,
        reply: Sender<Decision>,
    },
}

/// The outcome of a finished run, for the result banner. Computed when the run ends by
/// pairing the honest-stop status with a scan of the output folder.
pub(crate) struct RunResult {
    /// True only when the run finished cleanly AND actually produced source files.
    pub(crate) ok: bool,
    /// A one-line headline (e.g. "5 files built, tests green").
    pub(crate) headline: String,
    /// The specific reason on failure (e.g. "built 0 source files — decomposition
    /// produced no subtasks"). Empty on success.
    pub(crate) reason: String,
    /// Source files present in the output folder (what it built).
    pub(crate) files: Vec<String>,
    /// The output folder, for the "open folder" button.
    pub(crate) dir: Option<std::path::PathBuf>,
    /// True for a finished **Breakdown** (plan-only) run: the design is ready but nothing was
    /// built yet. The result view then offers the follow-on **Build** + **Commit plan** actions,
    /// so the next step is unmissable instead of a passive "plan ready" line.
    pub(crate) plan_ready: bool,
}

pub(crate) struct App {
    /// The remote-session mirror (Claude-Code-remote style). `Some` when `SC_REMOTE` is
    /// set at launch: the desktop tees its live agent/chat events out to a phone and
    /// drains inbound chat/approve/cancel from it. `None` = local-only (the default).
    pub(crate) remote: Option<sc_web::RemoteMirror>,
    pub(crate) cfg: UiConfig,
    /// Latest backend health from the periodic probe (startup + every ~10s), shown as a
    /// menu-bar badge and used to gate runs. `None` until the first probe returns.
    pub(crate) backend_health: Option<sc_model::BackendHealth>,
    /// Receiver for the in-flight background health probe, if one is running. Drained on tick.
    pub(crate) health_rx: Option<std::sync::mpsc::Receiver<sc_model::BackendHealth>>,
    /// Wall-clock of the last probe kick-off, to space probes ~10s apart. `None` = never
    /// probed (kick one immediately).
    pub(crate) last_health_probe: Option<std::time::Instant>,
    /// When the live code view was last reloaded during a run. The reload does a synchronous
    /// `git diff` on the UI thread, so at the 50ms tick rate it froze the UI on a big repo —
    /// throttle it to ~1s so the view stays live without starving rendering.
    pub(crate) last_reload: Option<std::time::Instant>,
    pub(crate) intent: String,
    /// Editable mirrors of the config for the settings panel. Seeded from
    /// `UiConfig::default()` so the boxes show the active values (and a run never
    /// reads a blank input over a good default — see [`App::default`]).
    ///
    /// Two groups now: the **connection** inputs (endpoint + key for Local and Gemini) live on
    /// the Connections tab; the **model** inputs (one per stage) live on the Routing tab alongside
    /// the per-stage provider toggle (which is edited straight on `cfg`, like yolo/dry-run).
    pub(crate) model_input: String, // coder model
    pub(crate) orch_model_input: String, // planner model
    pub(crate) advisor_input: String,    // advisor model (optional)
    // Connection endpoints + keys.
    pub(crate) local_url_input: String,
    pub(crate) local_key_input: String,
    pub(crate) gemini_url_input: String,
    pub(crate) gemini_key_input: String,
    pub(crate) verify_input: String,
    pub(crate) suffix_input: String,
    pub(crate) settings_open: bool,
    /// Which settings tab is showing (Connections vs Routing).
    pub(crate) settings_tab: SettingsTab,
    /// Activity rows accumulated from the event stream.
    pub(crate) rows: Vec<Row>,
    /// The latest single-run plan steps (right panel, agent mode).
    pub(crate) board: Vec<String>,
    /// The live per-subtask board (right panel, swarm mode).
    pub(crate) swarm_board: sc_win::SwarmBoard,
    /// The staged-workflow plan (TDD mode), shown in the always-visible plan panel.
    pub(crate) plan: sc_win::Plan,
    /// The live swarm topology (advisor/orchestrator/coders + glowing flows), drawn on
    /// the canvas during a swarm run.
    pub(crate) topology: sc_win::Topology,
    /// When the current run started, for the canvas's monotonic animation clock.
    pub(crate) run_started: Option<Instant>,
    /// The latest verification text (bottom panel), failure-first.
    pub(crate) verify_text: Option<String>,
    /// The closing honest-stop summary, set when the run ends.
    pub(crate) summary: Option<String>,
    /// The outcome banner of the last finished run (success/failure + files built).
    pub(crate) result: Option<RunResult>,
    /// The live run, if one is in flight.
    pub(crate) session: Option<Session>,
    /// Pending human decisions (FIFO; the oldest is shown).
    pub(crate) gatebar: Vec<Gatebar>,
    /// Send-back notes the human is typing for a workflow checkpoint.
    pub(crate) sendback_notes: String,
    /// The coder box selected on the topology canvas (shows its prompt + proposal in
    /// the detail panel). `None` = show the orchestrator's decomposition reply.
    pub(crate) selected_coder: Option<String>,
    /// The actual folder the current/last run writes to (a picked dir, or a fresh
    /// datetime folder).
    pub(crate) run_dir: Option<std::path::PathBuf>,
    /// A folder the user picked to work in. When set, runs go HERE (so a follow-up
    /// prompt iterates on the existing files) instead of a fresh datetime folder.
    pub(crate) picked_workspace: Option<std::path::PathBuf>,
    /// True while the current/last run is an ITERATE (in-place edit) run, so the outcome
    /// banner reports "N files changed" from what the agent actually edited — never a
    /// whole-repo "files built" scan (which would count thousands in an existing project).
    pub(crate) iterating: bool,
    /// True while the current/last run is PLAN-ONLY (Execute-plan design pass): it produces
    /// reviewable artifacts, not a build, so its outcome banner must NOT report "N files built"
    /// (a whole-repo scan counted every source file — the bogus "13730 files built").
    pub(crate) planning_only: bool,
    /// The task string of the last Breakdown (plan-only) run, stashed so the result view's
    /// **Build this plan** button can start a staged build against the SAME plan without the user
    /// re-typing anything. `None` until a Breakdown has run this session.
    pub(crate) last_plan_task: Option<String>,
    /// The files the agent actually *edited/wrote* this run (workspace-relative, de-duped),
    /// for the iterate outcome banner. Reset at each run start.
    pub(crate) edited_files: Vec<String>,

    // --- IDE shell state (explorer + code viewer) --------------------------------
    /// Collapsed directories in the explorer (workspace-relative paths). Everything
    /// expanded by default; clicking a dir toggles it here.
    pub(crate) collapsed_dirs: std::collections::HashSet<String>,
    /// The explorer's quick-filter query. When non-empty the file tree is narrowed to matching
    /// files/folders (searching the whole tree, ignoring collapse). Empty = normal tree.
    pub(crate) file_filter: String,
    /// The fully-walked file tree, cached so `view()` derives the collapsed/filtered display in
    /// memory instead of re-walking the filesystem every frame (which made filtering laggy).
    /// Refreshed by the snapshot path on workspace change and after edits/git actions.
    pub(crate) tree_cache: Vec<sc_win::filetree::TreeRow>,
    /// True while a background `compute_snapshot` is in flight, so the heartbeat doesn't stack up
    /// overlapping walks if one runs long.
    pub(crate) sync_pending: bool,
    /// The file shown in the code panel (workspace-relative). `None` before any file
    /// is chosen / touched.
    pub(crate) selected_file: Option<String>,
    /// The files open as tabs in the CODE panel, in the order they were opened (left→right).
    /// `selected_file` is the ACTIVE tab; this is the full open set. Opening a file adds it
    /// here if absent; closing a tab removes it.
    pub(crate) open_tabs: Vec<String>,
    /// When true, the code panel follows the agent (auto-jumps to the file it's
    /// editing). Clicking a file in the tree pins it (sets this false); it re-arms when
    /// a new run starts. This is the "watch it work" behaviour, escapable on demand.
    pub(crate) follow_agent: bool,
    /// The rendered contents of `selected_file`, recomputed when the selection changes
    /// or the file is edited. Cached so `view()` doesn't hit the disk every frame.
    pub(crate) code: Option<sc_win::CodeView>,
    /// Which top-bar menu is currently open (File / View), if any. `None` = all closed.
    pub(crate) open_menu: Option<Menu>,

    // --- Conversation (plan-first chat) ------------------------------------------
    /// The planning conversation with the model (mode-shaped, plans-as-files). `None`
    /// until a project folder is opened.
    pub(crate) conversation: Option<sc_win::chat::Conversation>,
    /// The chat thread shown in the middle column (you ⟷ agent), in order.
    pub(crate) chat_turns: Vec<sc_win::chat::Turn>,
    /// One read-only `text_editor` buffer per chat turn, so displayed messages are
    /// drag-selectable + Ctrl+C-copyable (iced's plain `text` isn't). Kept in lock-step with
    /// `chat_turns` by [`Self::sync_chat_editors`], which rebuilds them only when the thread
    /// actually changes (detected via [`Self::chat_sig`]) — cheap, and avoids touching the
    /// ~30 scattered `chat_turns.push` sites.
    pub(crate) chat_editors: Vec<iced::widget::text_editor::Content>,
    /// Change signature of `chat_turns` (count + last text length) at the last editor sync;
    /// a mismatch triggers a rebuild.
    pub(crate) chat_sig: (usize, usize),
    /// An in-flight chat turn (a `generate` call on a worker thread), if any.
    pub(crate) chat_session: Option<sc_win::chat_session::ChatSession>,
    /// Plan-file changes the assistant proposed in its latest reply, awaiting Apply.
    pub(crate) proposed_files: Vec<sc_win::chat::ProposedFile>,
    /// A shell command the chat proposed (from a ```command block, the `Command` intent),
    /// awaiting a one-click Run into the integrated terminal. `None` when the latest reply
    /// proposed no command.
    pub(crate) proposed_command: Option<String>,
    /// Whether the next chat turn should let the model *reason* (slower, deeper) vs. answer
    /// directly (`/no_think`, fast). Off by default — this 8B rambles when left to think, so
    /// fast conclusions are the default and Think is opt-in per the composer toggle.
    pub(crate) think: bool,
    /// Which bottom-panel tab is selected (Activity / Verification / Build).
    pub(crate) bottom_tab: BottomTab,
    /// The integrated command-runner terminal (bottom-strip "Terminal" tab). Host-testable
    /// state; the running command's output channel is held in `term_rx`.
    pub(crate) terminal: sc_win::terminal::Terminal,
    /// The receiver for the currently-running terminal command, drained each tick. `None`
    /// when no command is running.
    pub(crate) term_rx: Option<std::sync::mpsc::Receiver<sc_win::terminal::TermMsg>>,
    /// The workspace's persistent sandbox container, when sandboxing is on and a project is
    /// open. Started lazily on the first terminal command and torn down on project switch /
    /// close, so terminal commands `docker exec` into it rather than touching the host.
    /// `None` when sandboxing is off (host mode) or no project is open.
    pub(crate) term_container: Option<sc_verify::SessionContainer>,
    /// Whether [`Self::term_container`]'s `docker run` has been issued this session (so we
    /// start it exactly once, not per command).
    pub(crate) term_container_started: bool,

    // --- Line comments (PR-style) ------------------------------------------------
    /// The committed line range being commented on (1-based, inclusive `(start, end)`), if a
    /// comment box is open. A single-line comment is `(n, n)`.
    pub(crate) comment_range: Option<(usize, usize)>,
    /// The comment text being typed for `comment_range`.
    pub(crate) comment_draft: String,
    /// Drag-select state: `Some((anchor, current))` while the mouse is pressed and dragging
    /// across lines in the code view; `None` when not dragging. On release it becomes the
    /// committed `comment_range`.
    pub(crate) drag: Option<(usize, usize)>,
    /// A line-comment classification in flight (the small/big triage call), if any. Carries
    /// the comment so the result can be routed once the verdict arrives.
    pub(crate) triage: Option<TriageInFlight>,
    /// A fast line-replace fix in flight (Small verdict → one call → splice), if any.
    pub(crate) replace: Option<ReplaceInFlight>,
    /// True when the current iterate run was triggered by a small line-comment fix, so its
    /// outcome (files changed + verify result) is reported back into the chat thread.
    pub(crate) iterate_from_comment: bool,
    /// The in-flight assistant reply as it streams in token-by-token (the live "typing"
    /// bubble). `None` when nothing is streaming; replaced by a finished turn on completion.
    pub(crate) streaming: Option<String>,
    /// Whether the chat thread auto-scrolls to the bottom as new content arrives. True by default
    /// and re-armed when the user scrolls back to the bottom; cleared when they scroll UP to read,
    /// so streaming replies don't yank them away from what they're looking at.
    pub(crate) chat_stuck_to_bottom: bool,
    /// Debug mode: when on, every prompt sent to the model is echoed into the chat as a
    /// (dimmed, collapsible) debug turn, so you can see exactly what the agent receives.
    pub(crate) debug: bool,
    /// Lines of the currently-shown file that differ from HEAD right now (from `git diff`),
    /// highlighted GitHub-PR-style. Refreshed as a fix edits the file, so you SEE changes land.
    pub(crate) changed_lines: std::collections::BTreeSet<usize>,
    /// The full PR-style diff of the shown file vs HEAD: `.added` (green, == `changed_lines`) plus
    /// `.removed_before` (red deleted lines, anchored to the current line they sat before). Drives
    /// the GitHub-diff rendering in the code view. Refreshed alongside `changed_lines`.
    pub(crate) file_diff: sc_win::gitdiff::FileDiff,
    /// The visible slice of the code view as (top_fraction, height_fraction) of the whole file,
    /// updated on every scroll so the minimap draws a "you are here" box. `None` until first scroll.
    pub(crate) code_viewport: Option<(f32, f32)>,
    /// Pixel height of the code viewport (last seen on scroll), so a minimap jump can center the
    /// clicked line rather than parking it at the top. `0.0` until first scroll.
    pub(crate) code_view_h: f32,
    /// Absolute vertical scroll offset (px) of the code view, last seen on scroll. Drives line
    /// virtualization: only lines within `[code_scroll_y, code_scroll_y + code_view_h]` (plus
    /// overscan) are turned into widgets. `0.0` until first scroll.
    pub(crate) code_scroll_y: f32,
    /// Pixel WIDTH of the code viewport, last seen on scroll. Sizes the comment/revert bars to the
    /// viewport (not the horizontally-scrollable content width) so they end before the minimap and
    /// stay visible without scrolling right. `0.0` until first scroll (→ fall back to Fill).
    pub(crate) code_view_w: f32,
    /// The range the agent is actively working on (the lines you commented on), highlighted in
    /// a pulsing amber from submit until the change lands — so the "thinking" gap feels active.
    /// `(file, start, end)`; `None` when nothing is in flight.
    pub(crate) working: Option<(String, usize, usize)>,

    // --- PR-review state ---------------------------------------------------------
    /// Persisted inline code comments (`.dc/comments.json`), rendered under their lines and
    /// marked resolved when the agent finishes the change.
    pub(crate) comments: sc_win::comments::Comments,
    /// Working-tree file statuses (path → M/A/D) for the PR-style file tree, refreshed as
    /// fixes land.
    pub(crate) file_status: std::collections::BTreeMap<String, sc_win::gitdiff::FileStatus>,
    /// The current git branch (shown in the explorer header), if any.
    pub(crate) branch: Option<String>,
    /// Ahead/behind vs the upstream tracking branch (for the ↑↓ header + push/pull buttons).
    /// Refreshed alongside the branch; `behind` reflects the last fetch (Pull/Fetch updates it).
    pub(crate) upstream: sc_win::gitdiff::UpstreamStatus,
    /// Last cursor position seen over the git list, so a right-click can pop the context menu
    /// at the cursor. Updated on mouse-move within the git rows.
    pub(crate) cursor_pos: iced::Point,
    /// The open git-row context menu: the file it targets + its status. `None` when closed.
    pub(crate) git_menu: Option<(String, sc_win::gitdiff::FileStatus)>,
    /// Where to draw the open git context menu (the cursor position at right-click time).
    pub(crate) git_menu_at: iced::Point,
    /// Per-file staged/unstaged state (from `git status` XY codes), for the Staged section and
    /// the Stage/Unstage menu items. Refreshed alongside `file_status`.
    pub(crate) stage_states: std::collections::BTreeMap<String, sc_win::gitdiff::StageState>,
    /// Per-file unstaged +added/−removed line counts (`git diff --numstat`), shown on the right
    /// of each Changes row. Untracked files are counted directly (git won't diff them).
    pub(crate) unstaged_deltas: std::collections::BTreeMap<String, sc_win::gitdiff::LineDelta>,
    /// Per-file STAGED +added/−removed line counts (`git diff --cached --numstat`), for the
    /// right of each Staged Changes row.
    pub(crate) staged_deltas: std::collections::BTreeMap<String, sc_win::gitdiff::LineDelta>,
    /// The commit-message draft typed in the git tab's VS-Code-style commit box.
    pub(crate) commit_msg: String,
    /// Live view of which keyboard modifiers are held (Ctrl/Shift/…). iced button-press
    /// messages don't carry the modifiers active at click time, so we track them here (updated
    /// from `ModifiersChanged` events) and read this when a git row is clicked to decide
    /// single- vs. ctrl-toggle vs. shift-range selection.
    pub(crate) modifiers: iced::keyboard::Modifiers,
    /// The multi-selected git files (workspace-relative paths, same keys as `file_status`), for
    /// batch operations. Always contains the plainly-selected file too, so a single click leaves
    /// a 1-element set. `selected_file` still drives the single-file diff preview; this set is
    /// additive on top for Ctrl/Shift multi-select and only affects row highlighting for now.
    pub(crate) git_selection: std::collections::BTreeSet<String>,
    /// The anchor row for Shift-range selection (the last plainly/ctrl-clicked row). A Shift-click
    /// re-selects the contiguous range from here to the clicked row. `None` until first click.
    pub(crate) git_select_anchor: Option<String>,

    // --- Resizable chat|code divider --------------------------------------------
    /// Chat's share of the combined chat+code region (0.15..0.85). 0.5 = the old even
    /// split; dragging the divider between the chat and code panels moves this.
    pub(crate) chat_frac: f32,
    /// Last-seen window width (px), from resize events — needed to turn an absolute
    /// cursor X into a split fraction while dragging the chat|code divider.
    pub(crate) window_w: f32,
    /// True while the chat|code divider is being dragged (mouse down on the handle).
    pub(crate) dragging_split: bool,

    // --- Resizable git|files divider (explorer column) --------------------------
    /// Fraction of the explorer column's height given to the Git section (the rest goes to
    /// Files). Dragged via the horizontal divider between them. Clamped to a sane band.
    pub(crate) explorer_frac: f32,
    /// The window height, tracked so an explorer-divider drag maps cursor Y *delta* to a
    /// fraction delta (the region height is what matters, not the absolute origin).
    pub(crate) window_h: f32,
    /// The grab anchor for a git|files divider drag: `(cursor_y_at_grab, explorer_frac_at_grab)`.
    /// `None` when not dragging. We drag by DELTA from this anchor — the divider moves only by how
    /// far the cursor actually travels — so it never jumps on grab (an absolute-Y mapping needs
    /// the explorer's exact top offset, which we don't track; a delta needs none).
    pub(crate) explorer_drag: Option<(f32, f32)>,
    /// Persisted divider positions, keyed by id — the ONE place split positions are saved. Seeded
    /// from `chat_frac`/`explorer_frac` at startup and re-saved when a drag settles.
    pub(crate) splits: sc_win::splits::SplitStore,
}

/// A line-comment triage running on a worker thread: the classify call + the comment it's
/// deciding, so `pump` can route to a small fix or a planning turn when the verdict lands.
pub(crate) struct TriageInFlight {
    pub(crate) comment: sc_win::linecomment::LineComment,
    pub(crate) session: sc_win::chat_session::ChatSession,
}

/// A fast LINE-REPLACE fix in flight: one model call for the new block text, which the IDE
/// splices into the file by line number (no agent-loop `edit_file` whitespace thrashing).
pub(crate) struct ReplaceInFlight {
    pub(crate) comment: sc_win::linecomment::LineComment,
    pub(crate) session: sc_win::chat_session::ChatSession,
    /// Streaming buffer for the "watch it type" preview of the replacement.
    pub(crate) streamed: String,
}

/// The bottom panel's tabs — the verify output and the last run's build outcome. Tabbed
/// (not stacked) so they share the bottom space. (Activity was dropped: the chat column
/// now carries "what the agent is doing", so a separate activity log is redundant.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BottomTab {
    Verification,
    Build,
    /// The integrated command-runner terminal.
    Terminal,
}

/// The top menu-bar dropdowns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Menu {
    File,
    View,
}

impl Drop for App {
    /// Force-remove the sandbox container on app exit so it never outlives the process. A
    /// hard kill can skip this, but the container's `--rm` and the stale-cleanup on next
    /// start cover that case.
    fn drop(&mut self) {
        self.teardown_term_container();
    }
}

impl Default for App {
    fn default() -> Self {
        // Seed the editable input boxes from the *loaded* config, so the settings
        // panel shows the active values and `start()` never commits a blank input
        // over a sensible default (URL, /no_think suffix, …). `load()` layers the
        // machine-local endpoint/model (config.json / env) over the neutral default,
        // so the specific backend this box uses never lives in the repo.
        let cfg = UiConfig::load();
        // Restore saved divider positions (one id-keyed store), so each split comes back where the
        // user left it. Defaults match the historical hardcoded fractions.
        let splits = sc_win::splits::SplitStore::load();
        let chat_frac = splits.get(sc_win::splits::id::CHAT_CODE, 0.5);
        let explorer_frac = splits.get(sc_win::splits::id::EXPLORER_GIT_FILES, 0.25);
        // Re-open the last project the user worked in (if it still exists on disk), so the
        // app comes back to where they left off instead of the empty scratch base.
        let picked_workspace = sc_win::persist::load().last_project;
        // Re-opening a remembered project → open its tree compacted (top-level folders collapsed),
        // matching the fresh-pick behavior.
        let collapsed_dirs = picked_workspace
            .as_deref()
            .map(sc_win::filetree::top_level_dirs)
            .unwrap_or_default();
        // Walk the remembered project's tree once up front; the view derives from this cache.
        let tree_cache = picked_workspace
            .as_deref()
            .map(sc_win::filetree::full_rows)
            .unwrap_or_default();
        Self {
            // Populated by `run()` when SC_REMOTE is set; default is local-only.
            remote: None,
            model_input: cfg.model.clone(),
            orch_model_input: cfg.orchestrator_model.clone().unwrap_or_default(),
            advisor_input: cfg.advisor_model.clone().unwrap_or_default(),
            // Connection inputs, seeded from the two connections resolved by `UiConfig::load`.
            local_url_input: cfg.local_conn.base_url.clone(),
            local_key_input: cfg.local_conn.key.clone().unwrap_or_default(),
            gemini_url_input: cfg.gemini_conn.base_url.clone(),
            gemini_key_input: cfg.gemini_conn.key.clone().unwrap_or_default(),
            verify_input: cfg.verify_command.clone().unwrap_or_default(),
            suffix_input: cfg.system_suffix.clone().unwrap_or_default(),
            settings_tab: SettingsTab::default(),
            cfg,
            backend_health: None,
            health_rx: None,
            last_health_probe: None,
            last_reload: None,
            intent: String::new(),
            settings_open: false,
            rows: Vec::new(),
            board: Vec::new(),
            swarm_board: sc_win::SwarmBoard::default(),
            plan: sc_win::Plan::default(),
            topology: sc_win::Topology::default(),
            run_started: None,
            verify_text: None,
            summary: None,
            result: None,
            session: None,
            gatebar: Vec::new(),
            sendback_notes: String::new(),
            selected_coder: None,
            run_dir: None,
            picked_workspace,
            iterating: false,
            planning_only: false,
            last_plan_task: None,
            edited_files: Vec::new(),
            collapsed_dirs,
            file_filter: String::new(),
            tree_cache,
            sync_pending: false,
            selected_file: None,
            open_tabs: Vec::new(),
            follow_agent: true,
            code: None,
            open_menu: None,
            conversation: None,
            chat_turns: Vec::new(),
            chat_editors: Vec::new(),
            chat_sig: (0, 0),
            chat_session: None,
            proposed_files: Vec::new(),
            proposed_command: None,
            think: false,
            bottom_tab: BottomTab::Verification,
            terminal: sc_win::terminal::Terminal::default(),
            term_rx: None,
            term_container: None,
            term_container_started: false,
            comment_range: None,
            comment_draft: String::new(),
            drag: None,
            triage: None,
            replace: None,
            iterate_from_comment: false,
            streaming: None,
            chat_stuck_to_bottom: true,
            debug: false,
            changed_lines: std::collections::BTreeSet::new(),
            file_diff: sc_win::gitdiff::FileDiff::default(),
            code_viewport: None,
            code_view_h: 0.0,
            working: None,
            comments: sc_win::comments::Comments::default(),
            code_scroll_y: 0.0,
            code_view_w: 0.0,
            file_status: std::collections::BTreeMap::new(),
            branch: None,
            upstream: sc_win::gitdiff::UpstreamStatus::default(),
            cursor_pos: iced::Point::ORIGIN,
            git_menu: None,
            git_menu_at: iced::Point::ORIGIN,
            stage_states: std::collections::BTreeMap::new(),
            unstaged_deltas: std::collections::BTreeMap::new(),
            staged_deltas: std::collections::BTreeMap::new(),
            commit_msg: String::new(),
            modifiers: iced::keyboard::Modifiers::empty(),
            git_selection: std::collections::BTreeSet::new(),
            git_select_anchor: None,
            chat_frac,
            window_w: 1040.0,
            dragging_split: false,
            explorer_frac,
            window_h: 800.0,
            explorer_drag: None,
            splits,
        }
    }
}

/// Which tab of the settings modal is showing. **Connections** edits the two endpoints + keys;
/// **Routing** picks which connection + model each pipeline stage uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SettingsTab {
    #[default]
    Connections,
    Routing,
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
    IntentChanged(String),
    // --- Model inputs (Routing tab) ---
    ModelChanged(String),     // coder model
    OrchModelChanged(String), // planner model
    AdvisorChanged(String),   // advisor model
    // --- Connection inputs (Connections tab) ---
    LocalUrlChanged(String),
    LocalKeyChanged(String),
    GeminiUrlChanged(String),
    GeminiKeyChanged(String),
    // --- Per-stage routing: which connection a stage uses ---
    CoderProviderChanged(sc_win::config::Provider),
    PlannerProviderChanged(sc_win::config::Provider),
    AdvisorProviderChanged(sc_win::config::Provider),
    /// Switch the settings modal tab (Connections / Routing).
    SettingsTabChanged(SettingsTab),
    VerifyChanged(String),
    SuffixChanged(String),
    ToggleSettings,
    ToggleYolo(bool),
    ToggleDryRun(bool),
    RunTdd,
    /// Run the in-place iterate loop (the default when a project folder is picked).
    RunIterate,
    Tick,
    /// Slow always-on heartbeat that drives the backend health probe (independent of `Tick`,
    /// which only fires when something is actively running).
    HealthTick,
    /// The off-thread live-view reload finished: the shown file's fresh contents + its
    /// changed-line set (green git-diff highlight). `None` if the background load failed.
    LiveViewReloaded(Option<(sc_win::CodeView, std::collections::BTreeSet<usize>)>),
    /// Heartbeat while a project is open: kick off an OFF-THREAD re-walk of the tree + git state
    /// so externally-created/removed files appear without a manual refresh.
    SyncWorkspace,
    /// The background workspace snapshot finished — apply it (or drop it if the compute failed).
    WorkspaceSynced(Option<WorkspaceSnapshot>),
    // Explorer / code-viewer interaction.
    /// Select a file in the tree → show it in the code panel (and pin, stop following).
    SelectFile(String),
    /// Click a CODE-panel tab → make it the active file. Pins (stops following) and just
    /// re-selects (no jump-to-first-change; that's a git-row nicety, not a plain tab switch).
    SelectTab(String),
    /// Close a CODE-panel tab (its ✕). Removes it from `open_tabs`; if it was the ACTIVE tab,
    /// a neighbour is activated (see `tab_after_close`), else `selected_file` is unchanged.
    CloseTab(String),
    /// Toggle a directory's collapsed state in the explorer.
    ToggleDir(String),
    /// The explorer's quick-filter text changed → narrow the tree to matching files/folders.
    FileFilterChanged(String),
    // Top menu bar.
    /// Open (or toggle) a top-bar dropdown menu.
    ToggleMenu(Menu),
    // Conversation.
    /// Send the composer text as a chat message to the planning agent.
    ChatSend,
    /// Copy a chat turn's text to the system clipboard (the per-turn copy button).
    CopyTurn(String),
    /// Run the chat's proposed command (from a ```command block) in the integrated terminal.
    RunProposedCommand,
    /// Dismiss the chat's proposed command without running it.
    DismissProposedCommand,
    /// An action inside a chat turn's read-only editor (selection/scroll only — edits are
    /// dropped so the message text stays immutable). `usize` is the turn index.
    ChatEditorAction(usize, iced::widget::text_editor::Action),
    /// Apply the Nth proposed plan-file to disk (writes README.md / TODO.md).
    ApplyFile(usize),
    /// Apply the Nth proposed plan-file, then run the FULL plan→build flow (staged design +
    /// compiler-driven build to green) — the one-click "Build" on the proposal card.
    ExecutePlan(usize),
    /// Apply the Nth proposed plan-file, then run the DESIGN-only staged pipeline (Breakdown):
    /// staged phases through decomposition, gated for review, no code build.
    BreakdownPlan(usize),
    /// Run the DESIGN-only staged pipeline (Breakdown) on the plan open in the code view — the
    /// staged phases through decomposition, gated for review, no code build. Header button.
    ExecuteOpenPlan,
    /// Run the FULL plan→build flow on the plan open in the code view: staged design then the
    /// compiler-driven executor builds it to green. Header "⚒ Build" button.
    BuildOpenPlan,
    /// After a Breakdown finishes: build the plan just designed (the stashed `last_plan_task`),
    /// starting a staged build with no retyping. The result view's "⚒ Build this plan" button.
    BuildLastPlan,
    /// After a Breakdown finishes: `git add` the plan artifacts + commit them, so the reviewed
    /// design is saved to the repo before (or instead of) building. Result view's "✓ Commit plan".
    CommitPlan,
    /// Toggle "think" mode for the next chat turn (reason vs. answer directly).
    ToggleThink(bool),
    /// Toggle debug mode: echo every prompt sent to the model into the chat.
    ToggleDebug(bool),
    /// Undo the last fix: git-revert exactly the files it changed, back to committed state.
    UndoLastChange,
    /// Dismiss the stored inline comment at this index (remove it from the review list).
    DismissComment(usize),
    /// Revert just this comment's change: splice its stored before-text back into the file.
    RevertComment(usize),
    /// Revert a single diff block (VS-Code-style) back to its HEAD text. Carries the hunk's
    /// current start line, which identifies the block to restore.
    RevertBlock(usize),
    /// Jump the code view to a 1-based line (clicked in the minimap).
    MinimapJump(usize),
    /// The code view was scrolled — carries the viewport so the minimap can draw a "you are here"
    /// box tracking the visible slice of the file.
    CodeScrolled(scrollable::Viewport),
    /// The chat thread was scrolled — carries the viewport so we can tell whether the user is at
    /// the bottom (keep auto-scrolling) or has scrolled up to read (stop yanking them down).
    ChatScrolled(scrollable::Viewport),
    /// Cancel the in-flight run/fix (stops the agent at its next turn; reverts partial edits).
    CancelRun,
    /// Cancel the in-flight chat/plan turn (interrupts the streaming model call).
    CancelChat,
    /// Select a bottom-panel tab (Activity / Verification / Build).
    SelectBottomTab(BottomTab),
    // Integrated terminal (bottom-strip "Terminal" tab).
    /// The terminal input box text changed.
    TermInput(String),
    /// Submit the current input line as a command to run.
    TermSubmit,
    /// Kill the currently-running terminal command.
    TermKill,
    /// Clear the terminal scrollback.
    TermClear,
    /// Recall the previous command into the input box (Up).
    TermHistoryPrev,
    /// Recall the next command into the input box (Down).
    TermHistoryNext,
    /// Cursor moved over the git list — track it so a right-click can place the context menu.
    GitCursorMoved(iced::Point),
    /// Right-clicked a git-tab row: open its context menu (stage / unstage / discard …).
    GitRowMenu(String, sc_win::gitdiff::FileStatus),
    /// Close the open git context menu without acting.
    CloseGitMenu,
    /// Stage this file (`git add -- <path>`).
    GitStage(String),
    /// Unstage this file (`git restore --staged -- <path>`).
    GitUnstage(String),
    /// Discard this file's working-tree changes (`git checkout -- <path>`); restores a deleted
    /// file or reverts a modified one to its committed state.
    GitDiscard(String),
    /// The held keyboard modifiers changed. Tracked so a git-row click (whose button-press
    /// message carries no modifiers) can read whether Ctrl/Shift is down for multi-select.
    ModifiersChanged(iced::keyboard::Modifiers),
    /// Select a file from the git tab → open it AND jump to its first changed line.
    SelectGitFile(String),
    /// Deferred second step of `SelectGitFile`: scroll to the first changed line once the new
    /// file's content has actually been laid out (avoids scrolling against the old file's tree).
    JumpToFirstChange,
    /// The commit-message draft in the git tab changed.
    CommitMsgChanged(String),
    /// Commit the staged files with the draft message (`git commit -m …`).
    GitCommit,
    /// Stage every changed file (`git add -A`) — the "Stage All Changes" ＋ on the Changes header.
    GitStageAll,
    /// Unstage every staged file (`git reset`) — the "− All" on the Staged Changes header.
    GitUnstageAll,
    /// Push HEAD to its upstream (`git push`). If the branch has no upstream, sets it on first push.
    GitPush,
    /// Pull from upstream (`git pull --ff-only`) — fast-forward only, so it never auto-merges.
    GitPull,
    /// Fetch from the remote (`git fetch`) to refresh the behind-count without changing the tree.
    GitFetch,
    // Line comments (PR-style) — drag to select a range, then comment.
    /// Mouse pressed on line N: start a drag-selection anchored there.
    LineDragStart(usize),
    /// Mouse entered line N while dragging: extend the selection to N.
    LineDragTo(usize),
    /// Mouse released: commit the drag-selection and open the comment box for the range.
    LineDragEnd,
    /// The comment draft text changed.
    CommentDraftChanged(String),
    /// Submit the line comment — triage it (small → fix now, big → plan).
    CommentSubmit,
    /// Cancel the open comment box.
    CommentCancel,
    // Confirm-gate answers.
    ConfirmAllow,
    ConfirmDeny,
    ConfirmRemember,
    // Workflow-gate answers.
    NotesChanged(String),
    GateApprove,
    GateSendBack,
    GateAbort,
    // Topology canvas interaction.
    SelectCoder(String),
    ClearSelection,
    // Workspace folder.
    PickWorkspace,
    /// Open a specific recent project (from the File ▸ Recent list).
    OpenRecent(std::path::PathBuf),
    /// A no-op (used by non-interactive dropdown labels like the "Recent" header).
    NoOp,
    ClearWorkspace,
    /// Open the output folder of the last run in the system file manager.
    OpenOutputFolder,
    // Resizable chat|code divider.
    /// Mouse pressed on the chat|code divider → begin dragging it.
    SplitDragStart,
    /// Mouse released anywhere → stop dragging the divider (either the chat|code or git|files one).
    SplitDragEnd,
    /// The window was resized — remember its width AND height so the divider drags can map an
    /// absolute cursor X→chat fraction (width) and cursor Y→explorer fraction (height).
    WindowSize(f32, f32),
    // Resizable git|files divider (explorer column). Release is handled by `SplitDragEnd`,
    // which clears both drag flags, so there's no separate ExplorerDragEnd.
    /// Mouse pressed on the git|files divider → begin dragging it.
    ExplorerDragStart,
}
