//! App logic: lifecycle and workspace.
//!
//! The chat send, the run start and the triage/replace pumps left for `sc-plugin-agent`
//! (spec 25). What stays is what the window itself needs: its title, its theme, its
//! subscription, and opening a project.

use super::*;

impl App {
    pub(crate) fn title(&self) -> String {
        // A leading ● when any tab has unsaved edits — the convention every editor uses, and
        // the one signal visible when the window isn't focused. It warns *before* a close is
        // attempted; the close itself is intercepted (`Message::CloseRequested`), which this
        // comment used to claim was impossible.
        // The PRODUCT's name, not the crate's. `Product::display_name` is the one place
        // it is spelled, so the title cannot drift from the state directory and the About
        // box the way "smart-coder — vibe coding" had already drifted from the binary.
        let name = sc_craft_ui::config::product().display_name();
        if self.any_dirty() {
            format!("● {name}")
        } else {
            name.to_string()
        }
    }

    pub(crate) fn theme(&self) -> Theme {
        Theme::TokyoNight
    }

    pub(crate) fn subscription(&self) -> Subscription<Message> {
        // Tick while there is anything live to pump: a terminal command, or a plugin
        // streaming into one of its panels. iced delivers the tick on the UI thread, so
        // draining the std::mpsc Receivers here is safe.
        //
        // The agent's own gates -- a running session, an open confirm, the topology glow
        // -- left with it (spec 25). What has NOT been replaced is a signal that a plugin
        // is busy: a plugin cannot yet tell the host "keep watching", so a plugin run is
        // only as live as its next content push.
        let tick = if self.term_rx.is_some()
            // A file was clicked and its diff has not been computed yet. Without this
            // the tick is OFF while idle -- which is exactly when a click happens -- and
            // the armed request would sit there until something else woke the app.
            || self.diff_wanted.is_some()
            || !self.plugin_scroll_to_bottom.is_empty()
        {
            iced::time::every(Duration::from_millis(50)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        };
        // A heartbeat that re-walks the tree + git state while a project is open, so files
        // created/removed OUTSIDE the app (or by a running agent) show up without a manual
        // refresh. The walk is cheap and off the render path, so 500ms feels live without cost.
        // Off when no project is open.
        let sync = if self.picked_workspace.is_some() {
            // 2s, always.
            //
            // It used to drop to 500ms while a run was live, because an agent writing
            // files is exactly when the tree needs to look fresh. The host no longer
            // knows when that is: a plugin has no way to say "I am busy" (spec 25 leaves
            // this open), so the choice was 500ms always or 2s always.
            //
            // 2s wins on measurement. The snapshot spawns several git processes, and a
            // git spawn costs ~26ms here before git does any work; at 500ms that is a
            // steady drip of subprocess churn competing with the render loop, felt as
            // typing lag. A tree that is two seconds stale while a plugin writes is the
            // cheaper mistake.
            let period = Duration::from_millis(2000);
            iced::time::every(period).map(|_| Message::SyncWorkspace)
        } else {
            Subscription::none()
        };
        // Track the window-absolute cursor position so a right-click in the git tab can pop its
        // context menu exactly at the pointer. `mouse_area::on_move` reports widget-relative
        // coordinates (useless for placing a window overlay); this window event is absolute.
        let cursor = iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                Some(Message::GitCursorMoved(position))
            }
            // A button-release anywhere ends a divider drag (even if the cursor left the handle).
            // `SplitDragEnd` ends BOTH the chat|code and git|files drags — they're mutually
            // exclusive in practice, so one release message clears whichever is active.
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) => {
                Some(Message::SplitDragEnd)
            }
            // Track the window size so a divider drag can map cursor X→width fraction and
            // cursor Y→height fraction. `Opened` seeds it at startup; `Resized` keeps it current.
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Message::WindowSize(size.width, size.height))
            }
            iced::Event::Window(iced::window::Event::Opened { size, .. }) => {
                Some(Message::WindowSize(size.width, size.height))
            }
            // Keep a live view of the held modifiers, so a git-row button click (which doesn't
            // report modifiers in its press message) can branch on Ctrl/Shift for multi-select.
            iced::Event::Keyboard(iced::keyboard::Event::ModifiersChanged(m)) => {
                Some(Message::ModifiersChanged(m))
            }
            // Ctrl+S saves the active tab. Handled at the WINDOW level, not as an editor key
            // binding, so it works with focus anywhere — including a find box or the tab strip.
            // `save_active_tab` is a no-op on a clean or unopened buffer, so this is safe to
            // fire unconditionally.
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Character(ref c),
                modifiers,
                ..
            }) if modifiers.command() && c.as_str().eq_ignore_ascii_case("s") => {
                Some(Message::SaveFile)
            }
            // The ✕, Alt+F4, or a taskbar close. Paired with `exit_on_close_request(false)` in
            // `run()`: iced hands us the request instead of obeying it, so unsaved buffers get
            // a prompt rather than being discarded. Without BOTH halves the window just closes.
            iced::Event::Window(iced::window::Event::CloseRequested) => {
                Some(Message::CloseRequested)
            }
            _ => None,
        });
        Subscription::batch([tick, sync, cursor])
    }

    /// Seconds since the app started, for anything that animates.
    ///
    /// This used to count from the current RUN's start, because the only animation was the
    /// amber pulse over lines the agent was working on. That left with the agent (spec 25),
    /// so the clock is now simply the process's own.
    pub(crate) fn now(&self) -> f32 {
        self.started.elapsed().as_secs_f32()
    }

    /// The workspace root the explorer/code panels read from.
    ///
    /// The picked project folder, or the current directory. The run-output fallback went
    /// with the agent (spec 25): there is no run here to have an output dir, and the
    /// config base it fell back to was the agent's scratch workspace.
    pub(crate) fn workspace_root(&self) -> std::path::PathBuf {
        self.picked_workspace
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }

    /// On opening a project, greet the user in the terminal: the project name, its
    /// README's TODO/roadmap excerpt (highlighted), and an invitation to say what to work
    /// on. No-op for a folder with no README (still greets, just no excerpt).
    pub(crate) fn show_welcome(&mut self) {
        let Some(root) = self.picked_workspace.clone() else {
            return;
        };
        let folder = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project")
            .to_string();
        let readme = find_readme(&root)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let todo_md = find_todo_file(&root)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let w = sc_win::welcome::build(&readme, &todo_md, &folder);

        // Into the TERMINAL scrollback. This greeting used to go to the activity stream,
        // which left with the agent (spec 25) — but "here is what this project's README
        // says is next" is a thing the editor should still say when you open a folder, so
        // it needed somewhere to land rather than being deleted with its old home.
        self.terminal.note(format!("opened  {}", w.title));
        if !w.lines.is_empty() {
            self.terminal.note(
                if w.no_todo {
                    "— from the README —"
                } else {
                    "— what's on the TODO —"
                }
                .to_string(),
            );
            for l in &w.lines {
                // A highlighted TODO/roadmap item gets a star; context lines a faint dot.
                let icon = if l.highlight { "★" } else { "·" };
                self.terminal.note(format!("{icon} {}", l.text));
            }
        }
        let prompt_icon = if w.no_todo { "⚠" } else { "▸" };
        self.terminal.note(format!("{prompt_icon} {}", w.prompt));
    }

    /// Adopt `dir` as the working project: reset per-project view state, walk its tree,
    /// remember it in recents, tell any remote mirror, and open its planning conversation.
    /// Shared by the desktop "open folder" button and a remote `/open` command.
    pub(crate) fn open_workspace(&mut self, dir: std::path::PathBuf) {
        self.picked_workspace = Some(dir.clone());
        // A fresh project → drop any stale selection from the last one, and open the tree
        // compacted: every top-level folder starts collapsed.
        self.panes.focused_mut().selected_file = None;
        self.panes.focused_mut().code = None;
        // Tabs held the old project's files — clear them so they don't linger into the new one.
        // EVERY pane: the old project's tabs are meaningless, and so is an arrangement
        // of panes holding them.
        self.panes.clear();
        self.confirm_close = None;
        self.save_conflict = None;
        // Which toolchain the Compile button offers depends on what's open (spec 21).
        self.refresh_project_kind();
        // Drop the git multi-selection + its shift anchor too — they keyed off the old project's
        // paths and would highlight/act on stale files in the new one.
        self.git_selection.clear();
        self.git_select_anchor = None;
        self.collapsed_dirs = sc_win::filetree::top_level_dirs(&dir);
        self.file_filter.clear();
        self.tree_cache = sc_win::filetree::full_rows(&dir);
        // Remember it (promotes to the front of recents) for next launch + the remote picker.
        let mut state = sc_win::persist::load();
        state.record_project(&dir);
        sc_win::persist::save(&state);
        // Greet: show the README/roadmap in Activity, and open the planning conversation.
        self.show_welcome();
    }
}
