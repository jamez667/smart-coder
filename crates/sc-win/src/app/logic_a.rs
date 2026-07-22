//! App logic: lifecycle, workspace, chat, triage/replace pumps.

use super::*;

impl App {
    pub(crate) fn title(&self) -> String {
        "smart-coder — vibe coding".to_string()
    }

    pub(crate) fn theme(&self) -> Theme {
        Theme::TokyoNight
    }

    pub(crate) fn subscription(&self) -> Subscription<Message> {
        // Tick while there's anything live to pump or animate: a running session, an
        // open gate, or topology flows still glowing (so the fade animates to rest
        // even after the run ends). iced delivers the tick on the UI thread, so
        // draining the std::mpsc Receiver here is safe.
        let glowing = !self.topology.active_flows(self.now()).is_empty();
        let tick = if self.session.is_some()
            || self.chat_session.is_some()
            || self.triage.is_some()
            || self.replace.is_some()
            || self.working.is_some()
            || !self.gatebar.is_empty()
            || self.term_rx.is_some()
            || glowing
            // Remote mirror active: keep ticking so inbound phone commands (chat/cancel)
            // are drained even when the desktop is otherwise idle.
            || self.remote.is_some()
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
            iced::time::every(Duration::from_millis(500)).map(|_| Message::SyncWorkspace)
        } else {
            Subscription::none()
        };
        // An always-on slow heartbeat that drives the backend health probe (startup + every
        // ~10s; the 3s tick just gives the 10s gate resolution and adopts a finished probe
        // promptly). Runs even when the app is otherwise idle — that's the whole point: know
        // the backend is down BEFORE you try to use it.
        let health = iced::time::every(Duration::from_secs(3)).map(|_| Message::HealthTick);
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
            _ => None,
        });
        Subscription::batch([tick, sync, cursor, health])
    }

    /// The run action for the primary button / intent submit, chosen by context: a
    /// picked project folder means "iterate in place"; otherwise the from-scratch build.
    pub(crate) fn run_message(&self) -> Message {
        if self.picked_workspace.is_some() {
            Message::RunIterate
        } else {
            Message::RunTdd
        }
    }

    /// The primary button label matching [`Self::run_message`].
    pub(crate) fn run_label(&self) -> &'static str {
        if self.picked_workspace.is_some() {
            "⚒  iterate"
        } else {
            "⚒  build"
        }
    }

    /// Fold every settings-panel input into `self.cfg` and persist the connection fields to
    /// config.json, so the current backend (coder + Gemini planner) is used AND survives a
    /// restart. Called before any run/chat/triage that talks to a model — the single place the
    /// input boxes become config, so the three entry points can't drift.
    pub(crate) fn commit_settings(&mut self) {
        // Models (per stage) + the endpoint-agnostic knobs.
        self.cfg.model = self.model_input.clone();
        self.cfg.orchestrator_model = non_empty(&self.orch_model_input);
        self.cfg.advisor_model = non_empty(&self.advisor_input);
        self.cfg.verify_command = non_empty(&self.verify_input);
        self.cfg.system_suffix = non_empty(&self.suffix_input);
        // Connections (endpoint + key each). The per-stage provider routing is already live on
        // `cfg` (edited by the toggle handlers). A blank local url keeps the current one rather
        // than wiping the endpoint.
        if !self.local_url_input.trim().is_empty() {
            self.cfg.local_conn.base_url = self.local_url_input.trim().to_string();
        }
        self.cfg.local_conn.key = non_empty(&self.local_key_input);
        if !self.gemini_url_input.trim().is_empty() {
            self.cfg.gemini_conn.base_url = self.gemini_url_input.trim().to_string();
        }
        self.cfg.gemini_conn.key = non_empty(&self.gemini_key_input);
        // Flatten connections + routing into the scalar fields the backend builders read, THEN
        // persist. Without this the run would use stale base_url/orchestrator_* from before the
        // edit. `resolve_stages` also clears a redundant orchestrator override when planner==coder.
        self.cfg.resolve_stages();
        // Persist (best-effort) so the connection/routing setup survives a restart.
        self.cfg.save_config();
    }

    pub(crate) fn start(&mut self, kind: RunKind) {
        if self.intent.trim().is_empty() || self.session.is_some() {
            return;
        }
        // Commit the settings inputs into the config (and persist them) before the run.
        self.commit_settings();

        // Preflight: don't launch a run against a known-bad backend — surface the reason in the
        // activity stream instead of failing several turns in.
        if let Some(reason) = self.backend_unready_reason() {
            self.rows.push(Row::ok(
                "⚠",
                format!("{reason} — check the backend badge (top bar)"),
            ));
            return;
        }

        self.rows.clear();
        self.board.clear();
        self.swarm_board = sc_win::SwarmBoard::default();
        self.plan = sc_win::Plan::default();
        self.topology = sc_win::Topology::default();
        self.selected_coder = None;
        self.run_started = Some(Instant::now());
        self.last_reload = None; // reload the live view immediately on the first tick
        self.verify_text = None;

        // Route the agent's command execution through the SAME persistent container the
        // terminal uses (starting it if needed), so its commands keep state and you can inspect
        // its work from the terminal. Falls back to per-run/host inside `agent_sandbox`.
        self.cfg.sandbox_override = Some(self.agent_sandbox());
        self.summary = None;
        self.result = None;
        self.gatebar.clear();
        // Re-arm follow so the code panel tracks the agent through this run.
        self.follow_agent = true;
        // Track this run's mode + which files it edits (for the honest iterate banner).
        self.iterating = matches!(kind, RunKind::Iterate | RunKind::StagedBuild);
        self.planning_only = matches!(kind, RunKind::Plan);
        self.edited_files.clear();
        // Jump to the Verification tab so the run's checks are visible as it works.
        self.bottom_tab = BottomTab::Verification;

        // Where to run:
        //  • a folder you picked → run there directly, so a follow-up prompt iterates
        //    on (and edits) the existing files in that project;
        //  • otherwise → a fresh datetime-stamped folder under the scratch base, so a
        //    new project never overwrites a previous one.
        let ws = match &self.picked_workspace {
            Some(dir) => {
                let _ = std::fs::create_dir_all(dir);
                dir.clone()
            }
            None => {
                let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
                self.cfg.run_workspace(&stamp)
            }
        };
        self.run_dir = Some(ws.clone());
        self.session = Some(Session::spawn(
            kind,
            self.cfg.clone(),
            self.intent.clone(),
            ws,
        ));
    }

    /// Monotonic seconds since the current run started — the canvas's animation clock.
    pub(crate) fn now(&self) -> f32 {
        self.run_started
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0)
    }

    /// The workspace root the explorer/code panels read from: the picked project folder
    /// if any, else the current run's output dir, else the config base. This is the tree
    /// the user is actually working in.
    pub(crate) fn workspace_root(&self) -> std::path::PathBuf {
        self.picked_workspace
            .clone()
            .or_else(|| self.run_dir.clone())
            .unwrap_or_else(|| self.cfg.workspace.clone())
    }

    /// On opening a project, greet the user in the Activity stream: the project name, its
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

        // Clear any previous run's activity so the welcome is the first thing shown.
        self.rows.clear();
        self.summary = None;
        self.result = None;
        self.rows.push(Row::ok("◆", format!("opened  {}", w.title)));
        if !w.lines.is_empty() {
            let header = if w.no_todo {
                "— from the README —"
            } else {
                "— what's on the TODO —"
            };
            self.rows.push(Row::ok(" ", header.to_string()));
            for l in &w.lines {
                // A highlighted TODO/roadmap item gets a star; context lines a faint dot.
                let icon = if l.highlight { "★" } else { "·" };
                self.rows.push(Row::ok(icon, l.text.clone()));
            }
        }
        // The closing prompt — for a project with no TODO, this nudges the user to make one.
        let prompt_icon = if w.no_todo { "⚠" } else { "▸" };
        self.rows.push(Row::ok(prompt_icon, w.prompt));
    }

    /// Adopt `dir` as the working project: reset per-project view state, walk its tree,
    /// remember it in recents, tell any remote mirror, and open its planning conversation.
    /// Shared by the desktop "open folder" button and a remote `/open` command.
    pub(crate) fn open_workspace(&mut self, dir: std::path::PathBuf) {
        // Retire the previous project's sandbox container before switching — it mounts the
        // old workspace and must not outlive it. A fresh one starts on the next command.
        self.teardown_term_container();
        self.picked_workspace = Some(dir.clone());
        // A fresh project → drop any stale selection from the last one, and open the tree
        // compacted: every top-level folder starts collapsed.
        self.selected_file = None;
        self.code = None;
        // Tabs held the old project's files — clear them so they don't linger into the new one.
        self.open_tabs.clear();
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
        self.publish_workspace_to_remote();
        // Greet: show the README/roadmap in Activity, and open the planning conversation.
        self.show_welcome();
        self.open_conversation();
    }

    /// Push the current workspace name + recents list to the remote mirror, so the phone's
    /// header shows the current repo and its picker lists recent projects. No-op without a mirror.
    pub(crate) fn publish_workspace_to_remote(&self) {
        if let Some(m) = &self.remote {
            let current = self
                .picked_workspace
                .as_ref()
                .map(|p| p.to_string_lossy().to_string());
            let recents: Vec<String> = sc_win::persist::load()
                .recents
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            m.set_projects(current, recents);
        }
    }

    /// Open a planning conversation for the current project: read the plan files, pick the
    /// mode (scratch vs existing), and seed the thread with the agent's opening line.
    /// The project's file paths (workspace-relative, `/`-separated) from the walked tree cache
    /// — directories dropped. Fed to a plan conversation so "Files to touch" names real files.
    pub(crate) fn project_file_paths(&self) -> Vec<String> {
        self.tree_cache
            .iter()
            .filter(|r| !r.is_dir)
            .map(|r| r.rel.clone())
            .collect()
    }

    pub(crate) fn open_conversation(&mut self) {
        let root = self.workspace_root();
        // Load persisted inline comments, ensure .dc/ is git-ignored, and pull the initial
        // git view (branch + file statuses) for the PR-style tree.
        self.comments = sc_win::comments::load(&root);
        sc_win::comments::ensure_gitignored(&root);
        self.refresh_git_view();
        let (readme, todo) = self.read_plan_files(&root);
        let mut convo = sc_win::chat::Conversation::open(&readme, &todo);
        convo.set_file_tree(self.project_file_paths());
        self.chat_turns.clear();
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::Agent,
            text: convo.opening_line(),
        });
        self.proposed_files.clear();
        // In an existing project, auto-open the TODO (or README) in the code view so you
        // land on the plan; the conversation continues from there.
        if let Some(f) = self.plan_file_to_show(&root) {
            self.follow_agent = false;
            self.select_file(f);
        }
        self.conversation = Some(convo);
    }

    /// The current README/TODO contents (empty string if absent) for the workspace `root`.
    pub(crate) fn read_plan_files(&self, root: &std::path::Path) -> (String, String) {
        let readme = find_readme(root)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let todo = find_todo_file(root)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        (readme, todo)
    }

    /// Which plan file to auto-open in the code view when a project opens: the TODO if it
    /// exists, else the README, else nothing.
    pub(crate) fn plan_file_to_show(&self, root: &std::path::Path) -> Option<String> {
        let rel = |p: std::path::PathBuf| {
            p.strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/")
        };
        find_todo_file(root)
            .map(rel)
            .or_else(|| find_readme(root).map(rel))
    }

    /// Send the composer text as a chat turn to the planning agent (worker thread). No-op
    /// when there's no conversation, no text, or a turn is already in flight.
    pub(crate) fn send_chat(&mut self) {
        let text = self.intent.trim().to_string();
        if text.is_empty() || self.chat_session.is_some() || self.conversation.is_none() {
            return;
        }
        // Preflight: refuse to send into a known-bad backend (the failure the health badge
        // warns about), so you get a clear message instead of a mid-stream "connection
        // refused". A not-yet-probed (`None`) or `Ready` backend passes.
        if let Some(reason) = self.backend_unready_reason() {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text: format!("⚠ {reason} — check the backend badge (top bar)."),
            });
            self.intent.clear();
            return;
        }
        // Commit connection settings (mirrors `start`), so a chat uses the current backend.
        self.commit_settings();
        let think = self.think;

        // Snapshot the file open in the code view so the model answers against what the user is
        // looking at ("what does this do?", "add handling here"). Read from disk (not the capped
        // CodeView render) so the chat sees the real file; chat.rs head-clips it for the window.
        let open_file = self.selected_file.as_ref().and_then(|rel| {
            let path = self.workspace_root().join(rel);
            std::fs::read_to_string(&path)
                .ok()
                .map(|body| (rel.clone(), body))
        });

        // Refresh the project file list (files may have been added/removed this session) so a
        // plan grounds on the current tree. Computed before the mutable convo borrow.
        let file_tree = self.project_file_paths();
        // Update the conversation, then spawn a planning turn (classify intent → generate). The
        // classification decides how the reply is shaped; the app no longer sniffs the text.
        let convo = {
            let convo = self.conversation.as_mut().expect("checked above");
            convo.set_open_file(open_file);
            convo.set_file_tree(file_tree);
            convo.user_turn(&text);
            convo.clone()
        };
        // Mirror the user's message to any remote client (so the phone shows what was
        // sent — whether it was typed on the desktop or came in from the phone itself).
        if let Some(m) = &self.remote {
            m.push(sc_core::AgentEvent::ChatMessage {
                role: "you".into(),
                text: text.clone(),
            });
        }
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::You,
            text,
        });
        self.proposed_files.clear();
        self.proposed_command = None;
        self.intent.clear();
        if self.debug {
            // Show the generate prompt for the most likely intent path (feature plan is the one
            // that was misbehaving); the real intent is classified on the worker.
            let req = convo.request(think, sc_win::chat::ChatIntent::Question);
            let joined = req
                .messages
                .iter()
                .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            self.debug_prompt("chat", &joined);
        }
        self.chat_session = Some(sc_win::chat_session::ChatSession::spawn_planning(
            self.cfg.clone(),
            convo,
            think,
        ));
    }

    /// Spawn a chat/generate call, first echoing its prompt into the chat when debug mode is
    /// on. Every model call that streams into the chat goes through here.
    pub(crate) fn spawn_chat(&mut self, label: &str, req: sc_model::GenerateRequest) {
        if self.debug {
            let joined = req
                .messages
                .iter()
                .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            self.debug_prompt(label, &joined);
        }
        self.chat_session = Some(sc_win::chat_session::ChatSession::spawn(
            self.cfg.clone(),
            req,
        ));
    }

    /// Submit the open line comment: capture the line + text, spawn the small/big triage
    /// classification (fast `/no_think`), and close the comment box. Routing happens in
    /// [`Self::pump_triage`] when the verdict arrives.
    pub(crate) fn submit_line_comment(&mut self) {
        let (Some((start, end)), Some(cv)) = (self.comment_range, self.code.as_ref()) else {
            return;
        };
        let comment = self.comment_draft.trim().to_string();
        if comment.is_empty() {
            return;
        }
        let rel = cv.rel.clone();
        // Scope from the CURRENT on-disk file, freshly numbered — NOT from `self.code`, which can
        // be a stale streamed *preview* (renumbered/spliced) left by an earlier fix. Reading disk
        // here guarantees `start`/`end`/`selection` all refer to the same real bytes we'll splice,
        // so the fix lands on the lines you actually selected.
        let disk = sc_win::codeview::load(&self.workspace_root(), &rel);
        let lines = if disk.note.is_none() {
            &disk.lines
        } else {
            &cv.lines // fall back to the view if the file can't be re-read
        };
        // The IDE does the scoping: hand the model the selected code + a small window, so it
        // doesn't slurp the whole file (what made a one-line fix slow + context-heavy).
        let (selection, context) = sc_win::linecomment::scope_context(lines, start, end);
        let lc = sc_win::linecomment::LineComment {
            file: rel,
            start,
            end,
            selection,
            context,
            comment,
        };
        // Persist the comment inline (pending) — it stays visible in the code view and gets
        // marked resolved when the agent finishes. A Question is removed again in pump_triage
        // (it's answered, not a change to track).
        self.comments.add(sc_win::comments::Comment::new(
            lc.file.clone(),
            start,
            end,
            lc.comment.clone(),
        ));
        sc_win::comments::save(&self.workspace_root(), &self.comments);
        // Commit connection settings so the triage/edit use the current backend.
        self.commit_settings();
        // Keep the commented lines highlighted (pulsing amber) while the agent works on them,
        // so the "thinking" gap between submit and the edit landing feels active. Cleared when
        // the run/answer finishes.
        self.working = Some((lc.file.clone(), start, end));
        self.run_started = Some(Instant::now()); // (re)start the animation clock
                                                 // Echo the comment into the chat (before `lc` moves into the triage state).
        let span = if start == end {
            format!("line {start}")
        } else {
            format!("lines {start}-{end}")
        };
        let echo = format!("💬 {} {span}: {}", lc.file, lc.comment);
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::You,
            text: echo,
        });
        let req = lc.classify_request();
        if self.debug {
            // Dump exactly what range/text was captured, so a mis-splice is diagnosable.
            let sel_first = lc.selection.lines().next().unwrap_or("").to_string();
            self.debug_prompt(
                "capture",
                &format!(
                    "range = lines {}-{}\nselection[0] = {:?}\nselection = {} line(s)",
                    lc.start,
                    lc.end,
                    sel_first,
                    lc.selection.lines().count()
                ),
            );
            let joined = req
                .messages
                .iter()
                .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            self.debug_prompt("triage", &joined);
        }
        let session = sc_win::chat_session::ChatSession::spawn(self.cfg.clone(), req);
        self.triage = Some(TriageInFlight {
            comment: lc,
            session,
        });
        self.comment_range = None;
        self.comment_draft.clear();
    }

    /// Drain the line-comment triage call. On a verdict: SMALL → run a scoped-but-coherent
    /// iterate fix now; BIG → seed a planning chat turn for approval.
    pub(crate) fn pump_triage(&mut self) {
        let Some(t) = &self.triage else {
            return;
        };
        let events = t.session.drain();
        for ev in events {
            let reply = match ev {
                // Triage is a one-word classify — ignore streamed tokens, act on the final.
                sc_win::chat_session::ChatEvent::Token(_) => continue,
                sc_win::chat_session::ChatEvent::Reply(r, _) => r,
                sc_win::chat_session::ChatEvent::Failed(_) => {
                    // On a failed triage, fall back to planning (the safe route).
                    "BIG".to_string()
                }
            };
            let verdict = sc_win::linecomment::parse_verdict(&reply);
            let comment = self.triage.take().expect("in flight").comment;
            match verdict {
                sc_win::linecomment::Verdict::Question => {
                    // A question about the code → ANSWER it (streamed into the chat), edit
                    // nothing. It's not a change to track, so drop the pending inline comment
                    // we stored on submit.
                    if let Some((i, _)) = self
                        .comments
                        .on_file(&comment.file)
                        .filter(|(_, c)| !c.resolved)
                        .last()
                    {
                        self.comments.remove(i);
                        sc_win::comments::save(&self.workspace_root(), &self.comments);
                    }
                    self.commit_settings();
                    let req = comment.question_request(self.think);
                    self.spawn_chat("question", req);
                }
                sc_win::linecomment::Verdict::Small => {
                    // FAST PATH: one model call for the new block text; the IDE splices it in by
                    // line number (no edit_file whitespace thrashing — the thing that made a
                    // reindent take 3 tries). The amber "working" highlight already shows the range.
                    self.chat_turns.push(sc_win::chat::Turn {
                        role: sc_win::chat::Speaker::Agent,
                        text: "→ quick fix — rewriting the selection…".to_string(),
                    });
                    self.commit_settings();
                    let req = comment.replace_request();
                    if self.debug {
                        let joined = req
                            .messages
                            .iter()
                            .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        self.debug_prompt("fix (line-replace)", &joined);
                    }
                    let session = sc_win::chat_session::ChatSession::spawn(self.cfg.clone(), req);
                    self.replace = Some(ReplaceInFlight {
                        comment,
                        session,
                        streamed: String::new(),
                    });
                }
                sc_win::linecomment::Verdict::Big => {
                    // Route into planning: seed a user turn and send it to the chat agent.
                    let seed = comment.planning_seed();
                    self.chat_turns.push(sc_win::chat::Turn {
                        role: sc_win::chat::Speaker::Agent,
                        text: "→ this needs a plan — let's talk it through first.".to_string(),
                    });
                    self.intent = seed;
                    self.send_chat();
                }
            }
            break;
        }
    }

    /// Drain the fast line-replace fix. Streams the replacement into the code-view preview;
    /// on completion, splices the new block into the file BY LINE NUMBER (deterministic — no
    /// whitespace matching), captures the before-text for a per-comment Revert, resolves the
    /// comment, and verifies (unless the change is comment-only).
    pub(crate) fn pump_replace(&mut self) {
        let Some(r) = &self.replace else {
            return;
        };
        let events = r.session.drain();
        let mut done: Option<(String, String)> = None; // (raw reply, none) sentinel via loop
        for ev in events {
            match ev {
                sc_win::chat_session::ChatEvent::Token(delta) => {
                    // Pull the fields we need out of the in-flight replace, ending the mutable
                    // borrow before we touch `self` again (for workspace_root / self.code).
                    let preview_bits = self.replace.as_mut().map(|r| {
                        r.streamed.push_str(&delta);
                        (
                            r.comment.file.clone(),
                            r.comment.start,
                            r.comment.end,
                            r.comment.selection.clone(),
                            sc_win::linecomment::extract_replacement(&r.streamed)
                                .unwrap_or_default(),
                        )
                    });
                    if let Some((file, hstart, hend, selection, preview)) = preview_bits {
                        if !preview.is_empty() {
                            let cur = std::fs::read_to_string(self.workspace_root().join(&file))
                                .unwrap_or_default();
                            // Preview at the block's TRUE current location (same re-anchoring the
                            // real write uses), so the live preview lands where the edit will.
                            if let Some((start, end)) =
                                sc_win::linecomment::locate_range(&cur, hstart, hend, &selection)
                            {
                                let spliced =
                                    sc_win::linecomment::splice_lines(&cur, start, end, &preview);
                                self.selected_file = Some(file.clone());
                                self.follow_agent = false;
                                self.code = Some(sc_win::codeview::from_text(&file, &spliced));
                            }
                        }
                    }
                }
                sc_win::chat_session::ChatEvent::Reply(raw, _) => {
                    done = Some((raw, String::new()));
                    break;
                }
                sc_win::chat_session::ChatEvent::Failed(msg) => {
                    self.working = None;
                    self.replace = None;
                    self.chat_turns.push(sc_win::chat::Turn {
                        role: sc_win::chat::Speaker::Agent,
                        text: format!("⚠ {msg}"),
                    });
                    return;
                }
            }
        }
        if let Some((raw, _)) = done {
            self.apply_line_replace(&raw);
        }
    }
}
