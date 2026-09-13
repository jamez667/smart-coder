//! Core application types: App state, Message, small UI enums.

use super::*;

pub(crate) struct App {
    /// A folder the user picked to work in. When set, runs go HERE (so a follow-up
    /// prompt iterates on the existing files) instead of a fresh datetime folder.
    pub(crate) picked_workspace: Option<std::path::PathBuf>,

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
    /// Cached `git diff` per workspace-relative path, so clicking a file paints from
    /// memory instead of waiting on a subprocess.
    ///
    /// Measured on this machine: `file_diff` costs ~50ms, of which ~26ms is bare process
    /// spawn — `git --version`, which does nothing, costs the same 26ms. That is the
    /// floor for shelling out at all here (Defender scanning each spawn), so the fix
    /// cannot be "make the git call faster"; it has to be "do not block on it".
    /// Invalidated wholesale whenever the workspace snapshot changes.
    pub(crate) diff_cache: std::collections::HashMap<String, sc_win::gitdiff::FileDiff>,
    /// The file whose diff is currently being computed off-thread, so a burst of clicks
    /// does not queue one subprocess per click.
    pub(crate) diff_pending: Option<String>,
    /// A file whose diff is wanted but not yet started — armed by
    /// `refresh_changed_lines`, drained by the tick.
    pub(crate) diff_wanted: Option<String>,
    /// The editor panes: their tabs, active file and viewport state, and which is focused.
    ///
    /// One pane's worth of this used to live directly on `App`. It moved because a second pane
    /// would otherwise have shared the first's scroll, diff and comment draft — and painted one
    /// pane's diff over the other's lines. See [`super::pane`] for the rule on what is per-pane
    /// and what stays global.
    pub(crate) panes: Panes,
    // --- Panel layout (spec 21) ---
    /// The panel arrangement currently on screen, for the current mode.
    pub(crate) layout: sc_win::layout::Layout,
    /// The saved arrangement for each mode, so toggling modes doesn't rearrange the other one.
    pub(crate) layouts: sc_win::layout::LayoutStore,
    /// The divider drag in flight. ONE field for every split — it replaces the two bespoke
    /// `dragging_split: bool` / `explorer_drag` fields, which only worked because there were
    /// exactly two draggable dividers.
    pub(crate) drag_split: Option<Drag>,
    /// What is being dragged — a panel by its header, or a tab out of its strip (spec 21).
    ///
    /// One field for both, so the shared drop machinery can't be handed a state where two things
    /// are in flight at once. See [`DragSubject`].
    pub(crate) drag: Option<DragSubject>,
    /// The window-edge dock band under the cursor, if any.
    ///
    /// Takes priority over [`Self::drop_target`]: the frame sits above the panels, so pointing at
    /// it means "dock across the whole layout" no matter which panel happens to be underneath.
    pub(crate) dock_side: Option<sc_win::layout::Side>,
    /// The drop target under the cursor while dragging: which panel, which of its edges, and
    /// whether the drop would span the WHOLE layout (an outer-edge drop) rather than split that
    /// one panel. The two produce very different layouts, so the preview must distinguish them.
    pub(crate) drop_target: Option<(sc_win::layout::PanelKind, sc_win::layout::Side, bool)>,
    /// Where each hidden panel used to sit, so ticking it back on returns it there.
    ///
    /// Session-scoped rather than persisted: it only matters between a hide and the matching
    /// show, and the layout itself already survives a restart.
    pub(crate) panel_slots:
        std::collections::BTreeMap<sc_win::layout::PanelKind, sc_win::layout::PanelSlot>,

    // --- Compile & check (spec 21) ---
    /// The kind of project open, detected from the tree on workspace change. Decides whether a
    /// compile can be offered at all, and with what command.
    pub(crate) project_kind: sc_win::project::ProjectKind,
    /// The last compile's outcome. `None` before the first run.
    pub(crate) compile_report: Option<sc_win::diagnostics::CompileReport>,
    /// A compile is in flight — the button reads "Compiling…" and offers cancel.
    pub(crate) compiling: bool,

    // ---- the profiler (spec 24) ----
    /// The loaded profile, if any. `None` is the section's resting state, not a failure.
    pub(crate) flame_profile: Option<sc_win::flame::Profile>,
    /// Where the loaded profile came from, for the header line.
    pub(crate) flame_source: String,
    /// The subtree currently zoomed to, as a path from the root. Empty ⇒ the whole profile.
    ///
    /// A path rather than a borrowed `&Frame` because the profile can be replaced underneath
    /// it; `flame::at_path` returns `None` for a stale path and the view falls back to the root.
    pub(crate) flame_zoom: Vec<String>,
    /// The search box's contents; matching frames are highlighted.
    pub(crate) flame_search: String,
    /// The frame under the cursor, for the detail line.
    pub(crate) flame_hover: Option<sc_win::flame::Placed>,
    /// Which sampling profiler was found on PATH. Probed ONCE at startup — a `--version`
    /// spawn per frame would be absurd.
    pub(crate) flame_tool: Option<sc_win::flame::tool::Profiler>,
    /// What a recorded run should profile.
    pub(crate) flame_target: sc_win::flame::tool::Target,
    /// Extra arguments passed to the profiled program, after `--`.
    pub(crate) flame_args: String,
    /// A recording run is in flight.
    pub(crate) flame_running: bool,
    /// Cooperative cancel for the recording run, mirroring the compile flow.
    pub(crate) flame_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// The last run's or load's failure, if it failed.
    pub(crate) flame_error: Option<String>,
    /// The Unity editor path override (Settings ▸ General). Blank ⇒ search the Hub convention.
    pub(crate) unity_path_input: String,
    /// Set to cancel an in-flight compile. The worker checks it between reads and kills the
    /// child, so a cold Unity build (minutes) is never a hostage situation.
    pub(crate) compile_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,

    /// The tab a close was attempted on while it had unsaved edits — the confirm prompt's
    /// subject. `None` = no prompt showing.
    pub(crate) confirm_close: Option<String>,
    /// The window close the user asked for while unsaved work was open, held until they answer.
    ///
    /// Separate from [`Self::confirm_close`] because the two prompts have different subjects: one
    /// tab versus *everything* dirty across every pane. Quitting is also the more dangerous of
    /// the two — it discards all of it at once — so it is worth its own state rather than a flag
    /// squeezed into the per-tab path.
    pub(crate) confirm_quit: bool,
    /// A save that was REFUSED because the file changed on disk under a dirty buffer. Holds the
    /// path, so the conflict can be explained where the user is looking rather than failing
    /// silently or clobbering someone's work.
    pub(crate) save_conflict: Option<String>,
    /// Which top-bar menu is currently open (File / View), if any. `None` = all closed.
    pub(crate) open_menu: Option<Menu>,
    /// Which bottom-panel tab is selected (Activity / Verification / Build).
    /// Whether the Settings modal is open.
    /// When the process started, for anything that animates.
    ///
    /// Replaces `run_started`, which counted from the current agent run — the only
    /// animation was the amber pulse over lines the agent was editing, and that left with
    /// it (spec 25).
    pub(crate) started: Instant,
    pub(crate) settings_open: bool,
    /// The editor's own settings. A far smaller thing than the `UiConfig` that left with
    /// the agent: no endpoints, no keys, no providers — just what the editor configures.
    pub(crate) cfg: sc_craft_ui::config::CraftConfig,
    pub(crate) bottom_tab: BottomTab,
    /// The integrated command-runner terminal (bottom-strip "Terminal" tab). Host-testable
    /// state; the running command's output channel is held in `term_rx`.
    pub(crate) terminal: sc_win::terminal::Terminal,
    /// The receiver for the currently-running terminal command, drained each tick. `None`
    /// when no command is running.
    pub(crate) term_rx: Option<std::sync::mpsc::Receiver<sc_win::terminal::TermMsg>>,

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

    // --- Window geometry --------------------------------------------------------
    /// Last-seen window width (px), from resize events. Still tracked for overlay placement;
    /// divider drags no longer need it — `responsive` gives each split its true extent.
    pub(crate) window_w: f32,
    /// Last-seen window height (px).
    pub(crate) window_h: f32,
    /// Persisted divider positions, keyed by split id — the ONE place split positions are saved.
    /// The panel tree stores ids, not fractions, so this needed no change to serve it.
    pub(crate) splits: sc_win::splits::SplitStore,

    // --- Plugins (spec 25) ---
    /// The running plugins, their failures, and their logs.
    ///
    /// `None` in a test-constructed `App`: `Plugins::start` spawns processes, so
    /// `Default` does not do it and `run()` installs this after the handshakes.
    pub(crate) plugins: Option<sc_craft_ui::plugin::Plugins>,
    /// The last content each plugin panel pushed, already flattened.
    ///
    /// Flattened on ARRIVAL rather than on paint: the limits and the nesting walk are the
    /// expensive part, and doing them per frame would put a plugin's content size on the
    /// render path. Absent ⇒ the plugin has pushed nothing yet, which the panel reports
    /// differently from an empty push.
    pub(crate) plugin_panels: std::collections::BTreeMap<
        sc_craft_ui::plugin::PluginPanelId,
        Vec<sc_craft_ui::plugin::view::Row>,
    >,
    /// Whether the plugin manager is open.
    pub(crate) plugins_modal: bool,
    /// A toggle changed something that only a restart will apply.
    ///
    /// Set by a successful enable/disable, and never cleared: the restart is still
    /// pending however many times the modal is reopened, and a notice that vanished when
    /// you closed the window would be worse than none.
    pub(crate) plugins_need_restart: bool,
    /// Why the last enable/disable failed, if it did.
    ///
    /// Shown in the modal rather than swallowed — silently failing to disable a plugin
    /// leaves the user certain they turned it off.
    pub(crate) plugin_toggle_error: Option<String>,
    /// Panels whose plugin asked to be pinned to the bottom on the next paint.
    ///
    /// A set rather than a flag, because two plugins can stream at once. Drained when
    /// the scroll Task is issued — a request that stayed set would fight the user every
    /// time they scrolled up to read something.
    pub(crate) plugin_scroll_to_bottom:
        std::collections::BTreeSet<sc_craft_ui::plugin::PluginPanelId>,
    /// In-progress values for plugin form fields, keyed by `(panel, field)`.
    ///
    /// Held by the HOST rather than echoed to the plugin per keystroke: a round trip per
    /// character is the same mistake as a per-keystroke `buffer.changed` carrying text.
    /// The values cross the wire once, on submit.
    pub(crate) plugin_fields:
        std::collections::BTreeMap<(sc_craft_ui::plugin::PluginPanelId, String), String>,
}

/// The bottom panel's tabs — the verify output and the last run's build outcome. Tabbed
/// (not stacked) so they share the bottom space. (Activity was dropped: the chat column
/// now carries "what the agent is doing", so a separate activity log is redundant.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BottomTab {
    /// The integrated command-runner terminal.
    Terminal,
    /// Compile results: the Compile button and the parsed diagnostics (spec 21). Present in both
    /// modes — in Craft mode it is the ONLY way to find out whether the code builds, since there
    /// is no agent to ask.
    Problems,
}

/// The top menu-bar dropdowns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Menu {
    File,
    View,
}

impl Default for App {
    fn default() -> Self {
        // Seed the editable input boxes from the *loaded* config, so the settings panel
        // shows the active values. The endpoint and model boxes this used to describe left
        // with the agent (spec 25); what remains is the editor's own settings.
        let cfg = sc_craft_ui::config::CraftConfig::load();
        // Read before `cfg` is moved into the struct below. The Unity path is a persisted setting
        // now, so the input box has to come back filled or the user retypes it every launch.
        let unity_path_seed = cfg.unity_path.clone().unwrap_or_default();
        // Restore saved divider positions (one id-keyed store), so each split comes back where the
        // user left it. Defaults match the historical hardcoded fractions.
        let splits = sc_win::splits::SplitStore::load();
        // The panel arrangement. An unusable stored tree (no editor, too deep, corrupt) falls
        // back to the default rather than wedging the window.
        let layouts = sc_win::layout::LayoutStore::load();
        let layout = layouts.get();
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
            // Connection inputs, seeded from the two connections resolved by `UiConfig::load`.
            picked_workspace,
            collapsed_dirs,
            file_filter: String::new(),
            tree_cache,
            sync_pending: false,
            diff_cache: std::collections::HashMap::new(),
            diff_pending: None,
            diff_wanted: None,
            panes: Panes::default(),
            project_kind: sc_win::project::ProjectKind::Unknown,
            compile_report: None,
            compiling: false,
            flame_profile: None,
            flame_source: String::new(),
            flame_zoom: Vec::new(),
            flame_search: String::new(),
            flame_hover: None,
            // Probed at boot rather than here: constructing an App in a test must not spawn
            // processes.
            flame_tool: None,
            flame_target: sc_win::flame::tool::Target::Bin(None),
            flame_args: String::new(),
            flame_running: false,
            flame_cancel: None,
            flame_error: None,
            // Probed at boot rather than here: `App::default()` runs in tests, and spawning a
            // process per constructed App would make the suite slow and machine-dependent.
            compile_cancel: None,
            unity_path_input: unity_path_seed,
            confirm_close: None,
            confirm_quit: false,
            save_conflict: None,
            open_menu: None,
            started: Instant::now(),
            settings_open: false,
            cfg,
            bottom_tab: BottomTab::Problems,
            terminal: sc_win::terminal::Terminal::default(),
            term_rx: None,
            comments: sc_win::comments::Comments::default(),
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
            layout,
            layouts,
            drag_split: None,
            drag: None,
            dock_side: None,
            drop_target: None,
            panel_slots: std::collections::BTreeMap::new(),
            plugins: None,
            plugins_modal: false,
            plugins_need_restart: false,
            plugin_toggle_error: None,
            plugin_panels: std::collections::BTreeMap::new(),
            plugin_scroll_to_bottom: std::collections::BTreeSet::new(),
            plugin_fields: std::collections::BTreeMap::new(),
            window_w: 1040.0,
            window_h: 800.0,
            splits,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
    // coder model // planner model   // advisor model
    // --- Per-stage routing: which connection a stage uses ---
    // --- Compliance report ---
    // --- The editor (spec 21) ---
    /// An event from the CODE pane's edit view, forwarded to the active tab's buffer.
    /// An event from an editor pane's edit view, forwarded to **that pane's** active buffer.
    ///
    /// The id is carried, not inferred from focus. With two live editors, routing by focus means
    /// a keystroke delivered in the same batch as the click that moved focus lands in the wrong
    /// file — silent, and exactly the kind of bug that only shows up under real use. The view
    /// knows which pane emitted it, so it says so.
    EditorEvent(sc_win::layout::EditorId, sc_editor::Message),
    /// Flip the active tab between the read-only review view and the editor.
    ToggleTabView,
    /// Write the active tab to disk (Ctrl+S). Refuses on a save conflict.
    SaveFile,
    /// Dismiss the save-conflict notice, leaving the buffer untouched and unsaved.
    DismissSaveConflict,
    /// Discard the file on disk and write the buffer over it — the explicit answer to a save
    /// conflict, never automatic.
    OverwriteOnConflict,
    /// Drop the unsaved changes in the tab awaiting a close confirmation, and close it.
    DiscardAndClose(String),
    /// Save the tab awaiting a close confirmation, then close it.
    SaveAndClose(String),
    /// Cancel a pending close, keeping the tab and its edits.
    CancelClose,
    /// The window manager asked to close the window (the ✕, Alt+F4, taskbar close).
    ///
    /// Intercepted rather than obeyed: with unsaved buffers this opens the quit prompt instead.
    /// Clean ⇒ quits immediately, so the guard is invisible unless it is needed.
    CloseRequested,
    /// Save every dirty buffer in every pane, then quit.
    SaveAllAndQuit,
    /// Quit, discarding every unsaved buffer. The explicit answer, never automatic.
    DiscardAndQuit,
    /// Dismiss the quit prompt and stay open.
    CancelQuit,

    // --- Plugins (spec 25) ---
    /// A clickable row in a plugin panel was pressed: run its command.
    PluginCommand(sc_craft_ui::plugin::PluginPanelId, String, Vec<String>),
    /// A plugin form field was typed into.
    PluginFieldChanged(sc_craft_ui::plugin::PluginPanelId, String, String),
    /// A plugin form's submit button was pressed.
    PluginFormSubmit(sc_craft_ui::plugin::PluginPanelId),
    /// Open or close the plugin manager.
    TogglePluginsModal,
    /// Enable or disable a plugin by its directory name. Applies at the next launch.
    SetPluginEnabled(String, bool),
    // --- Compile & check (spec 21) ---
    /// Run the project's compile command and parse its diagnostics.
    RunCompile,
    /// The compile finished off-thread.
    CompileDone(Box<sc_win::diagnostics::CompileReport>),
    /// Stop an in-flight compile — a cold Unity build is minutes, not seconds.
    CancelCompile,
    /// Open a diagnostic's file at its line. This is what makes the panel a list rather than a
    /// log: the whole point is to land on the offending character.
    OpenDiagnostic(usize),
    /// The Unity editor path override in Settings.
    UnityPathChanged(String),
    /// Scroll the code view to [`App::pending_scroll_line`], once the file is laid out.
    JumpToPendingLine,
    ToggleSettings,
    Tick,
    /// A `git diff` finished off-thread: `(relative path, diff)`. Applied only if that
    /// file is still the selected one — a fast click-through must not repaint the pane
    /// with a diff for a file the user has already moved on from.
    FileDiffReady(String, Box<sc_win::gitdiff::FileDiff>),
    /// Heartbeat while a project is open: kick off an OFF-THREAD re-walk of the tree + git state
    /// so externally-created/removed files appear without a manual refresh.
    SyncWorkspace,
    /// The background workspace snapshot finished — apply it (or drop it if the compute failed).
    WorkspaceSynced(Option<WorkspaceSnapshot>),
    // Explorer / code-viewer interaction.
    /// Select a file in the tree → show it in the code panel (and pin, stop following).
    SelectFile(String),
    /// Close a CODE-panel tab (its ✕). Removes it from `open_tabs`; if it was the ACTIVE tab,
    /// a neighbour is activated (see `tab_after_close`), else `selected_file` is unchanged.
    CloseTab(String),
    /// Mouse pressed on a tab — a *possible* drag, not yet a real one.
    ///
    /// Nothing is selected here. A tab both selects and drags, so committing to either on press
    /// would break the other: selection happens on release-without-movement ([`Self::TabRelease`])
    /// and the drag arms only once the cursor passes `DragSubject::THRESHOLD`, measured in the
    /// existing global [`Self::CursorMoved`] handler.
    ///
    /// Carries no position: the origin is read from the app's tracked `cursor_pos`, which is
    /// WINDOW space. A position from the tab's own `mouse_area` would be content-space — the tab
    /// strip is inside a `scrollable`, and iced translates the cursor by the scroll offset before
    /// handing it down, so the two would be measured in different frames of reference.
    TabPress(sc_win::layout::EditorId, String),
    /// Mouse released on the tab it was pressed on, having never moved: that's a click, so make
    /// it the active file. Pins (stops following) and just re-selects — no jump-to-first-change,
    /// which is a git-row nicety rather than part of a plain tab switch.
    ///
    /// Replaces the old `SelectTab`: selection had to move from press to release so that the same
    /// gesture could also start a drag. A release that DID move is a drop and ends at
    /// [`Self::PanelDrop`] or [`Self::TabDropOnPane`] instead.
    TabRelease(String),
    /// Drop the dragged tab onto `pane`'s strip — a move between panes, with no layout change.
    TabDropOnPane(sc_win::layout::EditorId),
    /// Toggle a directory's collapsed state in the explorer.
    ToggleDir(String),
    /// The explorer's quick-filter text changed → narrow the tree to matching files/folders.
    FileFilterChanged(String),
    // Top menu bar.
    /// Open (or toggle) a top-bar dropdown menu.
    ToggleMenu(Menu),
    /// Revert a single diff block (VS-Code-style) back to its HEAD text. Carries the hunk's
    /// current start line, which identifies the block to restore.
    /// Jump the code view to a 1-based line (clicked in the minimap).
    MinimapJump(usize),
    /// The code view was scrolled — carries the viewport so the minimap can draw a "you are here"
    /// box tracking the visible slice of the file.
    CodeScrolled(scrollable::Viewport),
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
    // Workspace folder.
    PickWorkspace,
    /// Open a specific recent project (from the File ▸ Recent list).
    OpenRecent(std::path::PathBuf),
    /// A no-op (used by non-interactive dropdown labels like the "Recent" header).
    NoOp,
    ClearWorkspace,
    // --- Panel tree (spec 21) ---
    /// Mouse pressed on a divider → begin dragging THAT split.
    ///
    /// One message for every divider, carrying the node's id and the region's true extent from
    /// `responsive`. Replaces the two bespoke start messages, which only worked because there
    /// were exactly two draggable dividers in a fixed layout.
    SplitGrab {
        id: String,
        axis: sc_win::layout::Axis,
        extent: f32,
    },
    /// Mouse released anywhere → end the drag and persist the position.
    SplitDragEnd,
    /// The window was resized — remembered for overlay placement.
    WindowSize(f32, f32),
    /// Show or hide a panel (View ▸ Panels).
    TogglePanel(sc_win::layout::PanelKind),
    /// Mouse down on a panel's header → pick that panel up.
    PanelGrab(sc_win::layout::PanelKind),
    /// The cursor moved over a panel while dragging:
    /// `(target, cursor x, y within the panel, panel w, h, tree w, h)`.
    ///
    /// The tree's size rides along because edge-docking is judged against the LAYOUT, not the
    /// window — the tree sits below the menu bar. The side is derived in `update` rather than
    /// the view, so all the geometry lives in one place.
    PanelHover(sc_win::layout::PanelKind, f32, f32, f32, f32, f32, f32),
    /// The cursor entered or left a window-edge dock band.
    DockHover(Option<sc_win::layout::Side>),
    /// Mouse released → drop the dragged panel on the current target, if there is one.
    PanelDrop,
    /// Put the panels back the way they started.
    ResetLayout,

    // ---- the profiler (spec 24) ----
    /// Open a folded-stack file through the system picker.
    OpenProfile,
    /// A profile finished loading (or failed to): `(source label, result)`.
    ProfileLoaded(String, Result<Box<sc_win::flame::Profile>, String>),
    /// Record a new profile with the detected tool.
    RecordProfile,
    /// Stop a recording run.
    CancelProfile,
    /// A recording run finished: the folded text it produced, or why it didn't.
    ProfileRecorded(Result<String, String>),
    /// Zoom to a frame; the payload is its path from the root. Empty ⇒ zoom all the way out.
    FlameZoom(Vec<String>),
    /// The cursor moved onto a frame, or off every frame.
    FlameHover(Option<Box<sc_win::flame::Placed>>),
    /// The search box changed.
    FlameSearch(String),
    /// The record target changed.
    FlameTarget(sc_win::flame::tool::Target),
    /// The extra-arguments box changed.
    FlameArgs(String),
    /// Copy the install hint for the missing profiler to the clipboard.
    CopyInstallHint,
    /// Look for a sampling profiler again, after the user installed one.
    RecheckProfiler,
    /// Split the focused editor, moving its active tab into a new pane beside it.
    ///
    /// The tab MOVES rather than being copied — a path lives in exactly one pane (see
    /// `select_file_into`), and a copy would be two buffers over one file.
    SplitEditor,
    /// Make `id` the focused pane — where Ctrl+S saves, where the tree opens files.
    FocusPane(sc_win::layout::EditorId),
}
