//! App logic: run/iterate finalizers, pumps, session container, gating.

use super::*;

impl App {
    /// Whether a swarm run is (or was) active — i.e. the topology has nodes to draw.
    pub(crate) fn is_swarm(&self) -> bool {
        !self.topology.is_empty()
    }

    /// Compute the outcome banner when a run ends. Two shapes:
    ///  • ITERATE (editing an existing project) → report the files the agent actually
    ///    *changed* — never a whole-repo scan (which would count thousands) and no "open
    ///    output folder" (you're already in your own repo).
    ///  • FROM-SCRATCH build → the "N files built" summary + open-folder, as before.
    pub(crate) fn finish_run(&mut self, ok: bool, summary: &str) {
        // Commit the final phase's streamed reply as a turn — the run ending is the last chance;
        // nothing after it would flush the live bubble, so its content would otherwise vanish.
        self.commit_streaming_turn();
        // Surface the outcome: jump to the Build tab when a run ends.
        self.bottom_tab = BottomTab::Build;
        // The agent's done working the selection — drop the amber "working" highlight (the
        // green git-change highlight takes over).
        self.working = None;
        if self.iterating {
            self.finish_iterate(ok, summary);
            return;
        }
        // A plan-only (Execute-plan) run designs; it does NOT build. Report the plan outcome —
        // never a "N files built" whole-repo scan (that produced the bogus "13730 files built").
        if self.planning_only {
            self.result = Some(RunResult {
                ok,
                headline: if ok {
                    "plan ready".to_string()
                } else {
                    "planning did not finish".to_string()
                },
                reason: summary.to_string(),
                files: Vec::new(),
                dir: None,
                // A clean plan run → offer the Build + Commit follow-ons in the result view.
                plan_ready: ok,
            });
            return;
        }
        let dir = self.run_dir.clone();
        let files = dir
            .as_deref()
            .map(sc_win::config::source_files)
            .unwrap_or_default();
        let n = files.len();

        // The truthful classification: "ok" from the run only counts if it actually
        // produced source files. Building zero files is a failure no matter what the
        // swarm reported.
        let (banner_ok, headline, reason) = if n == 0 {
            (
                false,
                "Built 0 files".to_string(),
                if self.plan.subtasks.is_empty() {
                    "the planner produced no buildable subtasks (decomposition failed)".to_string()
                } else {
                    format!("implementation failed — {summary}")
                },
            )
        } else if ok {
            (true, format!("{n} file{} built", plural(n)), String::new())
        } else {
            (
                false,
                format!("{n} file{} built, but did not finish", plural(n)),
                summary.to_string(),
            )
        };

        self.result = Some(RunResult {
            ok: banner_ok,
            headline,
            reason,
            files,
            dir,
            plan_ready: false,
        });
    }

    /// Undo the last fix: `git checkout --` exactly the files it changed, restoring them to
    /// their committed state. A deliberate "I don't like this change" action — the safe way to
    /// reject a fix, instead of hand-reverting. No-op if there's no finished result with files.
    pub(crate) fn undo_last_change(&mut self) {
        let files: Vec<String> = self
            .result
            .as_ref()
            .map(|r| r.files.clone())
            .unwrap_or_default();
        if files.is_empty() {
            return;
        }
        let root = self.workspace_root();
        let ok = sc_win::proc::git()
            .arg("-C")
            .arg(&root)
            .arg("checkout")
            .arg("--")
            .args(&files)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        self.chat_turns.push(sc_win::chat::Turn {
            role: sc_win::chat::Speaker::Agent,
            text: if ok {
                format!(
                    "↩ Undid the change — reverted {} file(s) to committed state.",
                    files.len()
                )
            } else {
                "⚠ Couldn't undo (not a git repo, or the file was already committed).".to_string()
            },
        });
        if ok {
            // Clear the result banner + refresh the code view to the reverted content.
            self.result = None;
            self.reload_selected();
        }
    }

    /// The iterate outcome: report only the files the agent edited this run, and whether it
    /// finished green. No repo scan, no open-folder button.
    pub(crate) fn finish_iterate(&mut self, ok: bool, summary: &str) {
        let files = self.edited_files.clone();
        let n = files.len();
        // On a successful comment-driven fix, mark the comment resolved (in place) and refresh
        // the PR file-tree statuses. The resolved comment stays visible as a "done" record.
        if ok && self.iterate_from_comment {
            let mut changed = false;
            for f in &files {
                changed |= self.comments.resolve_latest_on(f);
            }
            if changed {
                sc_win::comments::save(&self.workspace_root(), &self.comments);
            }
        }
        self.refresh_git_view();
        let (headline, reason) = if ok && n > 0 {
            (
                format!("done — {n} file{} changed", plural(n)),
                String::new(),
            )
        } else if ok {
            // Finished cleanly but edited nothing (e.g. "already as requested").
            ("done — no changes needed".to_string(), summary.to_string())
        } else {
            (
                format!("stopped — {n} file{} changed, did not finish", plural(n)),
                summary.to_string(),
            )
        };
        // If this iterate came from a line comment, report the outcome IN THE CHAT so you
        // see what it did without hunting in the Build tab.
        if self.iterate_from_comment {
            self.iterate_from_comment = false;
            let text = if ok && n > 0 {
                let list = files.join(", ");
                let verified = if self.verify_text.is_some() {
                    " and it still compiles"
                } else {
                    ""
                };
                format!("✓ Done. Changed {n} file{} ({list}){verified}.", plural(n))
            } else if ok {
                "✓ Done — nothing needed changing.".to_string()
            } else {
                format!(
                    "⚠ I couldn't finish that cleanly ({} file{} touched). {summary}",
                    n,
                    plural(n)
                )
            };
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Agent,
                text,
            });
        }
        self.result = Some(RunResult {
            ok,
            headline,
            reason,
            files,
            dir: None, // no "open output folder" — you're iterating in your own repo
            plan_ready: false,
        });
    }

    /// Drain the in-flight chat turn, if any. On a reply: split prose from proposed
    /// plan-files, record the assistant turn, show the prose in the thread, stage the
    /// proposed files for Apply, and auto-open the first one in the code view.
    pub(crate) fn pump_chat(&mut self) {
        let Some(cs) = &self.chat_session else {
            return;
        };
        let events = cs.drain();
        for ev in events {
            match ev {
                sc_win::chat_session::ChatEvent::Token(delta) => {
                    // Grow the live "typing" bubble. Strip <think> for display as it streams
                    // (a reasoning delta shouldn't flash into the visible reply).
                    let buf = self.streaming.get_or_insert_with(String::new);
                    buf.push_str(&delta);
                    // Mirror the live typing to any remote client — same cleaned view the
                    // desktop shows (strips <think> reasoning + hides file-block fences), so
                    // the phone doesn't flash raw reasoning.
                    if let Some(m) = &self.remote {
                        let visible = sc_win::chat::visible_so_far(buf);
                        if !visible.is_empty() {
                            m.push(sc_core::AgentEvent::ChatDelta {
                                cumulative: visible,
                            });
                        }
                    }
                }
                sc_win::chat_session::ChatEvent::Reply(raw, intent) => {
                    self.streaming = None; // the live bubble is replaced by the finished turn
                    self.working = None; // a question answer is done → drop the amber highlight
                    let (prose, mut files) = sc_win::chat::parse_reply(&raw);
                    // Robustness for small local models: a FEATURE PLAN whose reply came back as
                    // bare prose (the model ignored the ```file: fence instruction — common when
                    // the prompt is large) is WRAPPED into a PLAN-<slug>.md proposal here, so a
                    // plan always yields an Apply/verify card rather than silently staying prose.
                    let is_plan_intent = matches!(
                        intent,
                        Some(sc_win::chat::ChatIntent::FeaturePlan)
                            | Some(sc_win::chat::ChatIntent::PlanFromTodo)
                    );
                    if is_plan_intent && files.is_empty() && !prose.trim().is_empty() {
                        let slug = self.plan_slug_for_reply();
                        files.push(sc_win::chat::wrap_plan_prose(&prose, &slug));
                    }
                    // Record the user's VERBATIM request at the top of the spec (provenance),
                    // whether the model emitted a file block or we wrapped its prose.
                    if is_plan_intent {
                        let request = self.last_user_request();
                        for f in &mut files {
                            if is_feature_plan(&f.name) {
                                f.content = sc_win::chat::prepend_request(&f.content, &request);
                            }
                        }
                    }
                    // A ```command block (the Command intent) → offer it as a one-click Run in
                    // the terminal, rather than auto-executing.
                    self.proposed_command = sc_win::chat::extract_command(&raw);
                    if let Some(convo) = self.conversation.as_mut() {
                        convo.record_reply(&raw);
                        // Fold any proposed plan-file content into the conversation's plan
                        // snapshot NOW (before Apply), so follow-up questions ("what's in the
                        // todo?") see what was just proposed — otherwise the system prompt
                        // keeps showing the stale on-disk file and the model contradicts its
                        // own proposal.
                        for f in &files {
                            if f.name.eq_ignore_ascii_case("README.md") {
                                convo.set_readme(&f.content);
                            } else if f.name.eq_ignore_ascii_case("TODO.md") {
                                convo.set_todo(&f.content);
                            }
                        }
                    }
                    let shown = if !prose.is_empty() {
                        prose
                    } else if let Some(cmd) = &self.proposed_command {
                        // A reply that was only a command block → say what will run.
                        format!("Run this in the terminal:  {cmd}")
                    } else {
                        // A reply that was only file blocks → note what changed.
                        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
                        format!("Proposed changes to {}.", names.join(", "))
                    };
                    self.chat_turns.push(sc_win::chat::Turn {
                        role: sc_win::chat::Speaker::Agent,
                        text: shown.clone(),
                    });
                    // Mirror the finished assistant turn to any remote client.
                    if let Some(m) = &self.remote {
                        m.push(sc_core::AgentEvent::ChatMessage {
                            role: "agent".into(),
                            text: shown,
                        });
                    }
                    self.proposed_files = files;
                    // Auto-open the first proposed file so you see the plan taking shape.
                    if let Some(first) = self.proposed_files.first() {
                        let name = first.name.clone();
                        let content = first.content.clone();
                        // Show the PROPOSED content directly (not the on-disk file, which
                        // hasn't been written yet).
                        self.follow_agent = false;
                        self.selected_file = Some(name.clone());
                        self.code = Some(sc_win::codeview::from_text(&name, &content));
                    }
                    self.chat_session = None;
                }
                sc_win::chat_session::ChatEvent::Failed(msg) => {
                    self.streaming = None;
                    self.working = None;
                    self.chat_turns.push(sc_win::chat::Turn {
                        role: sc_win::chat::Speaker::Agent,
                        text: format!("⚠ {msg}"),
                    });
                    self.chat_session = None;
                }
            }
        }
    }

    /// Drain the worker channels into UI state. Called each tick.
    /// Apply commands a remote (phone) client sent this tick, through the same paths the
    /// desktop's own buttons use: a chat message → `send_chat()`; a stop → `session.cancel()`.
    /// Approve/deny are resolved inside the mirror server directly (they reach the reply
    /// channel the gate bar shares), so they need no handling here.
    pub(crate) fn pump_remote(&mut self) {
        let Some(cmds) = self.remote.as_ref().map(|m| m.drain_inbound()) else {
            return;
        };
        for cmd in cmds {
            match cmd {
                sc_web::InboundCmd::Chat(text) => {
                    // A chat needs an open conversation; start one if the desktop hasn't yet.
                    if self.conversation.is_none() {
                        self.open_conversation();
                    }
                    self.intent = text;
                    self.send_chat();
                }
                sc_web::InboundCmd::Cancel => {
                    if let Some(session) = &self.session {
                        session.cancel();
                    }
                }
                sc_web::InboundCmd::Open(path) => {
                    // Security: only open a path the desktop itself published in recents —
                    // a remote client can never open an arbitrary folder on this machine.
                    let allowed = sc_win::persist::load()
                        .recents
                        .iter()
                        .any(|p| p.to_string_lossy() == path);
                    let dir = std::path::PathBuf::from(&path);
                    if allowed && dir.is_dir() {
                        self.open_workspace(dir);
                    }
                }
            }
        }
    }

    /// Drain any output from a running terminal command into the scrollback. When the command
    /// finishes (an `[exit]` was seen), drop the receiver so the tick can quiesce.
    pub(crate) fn pump_terminal(&mut self) {
        if let Some(rx) = &self.term_rx {
            if self.terminal.drain(rx) {
                self.term_rx = None;
            }
        }
    }

    /// Drive the backend health probe: adopt a finished probe's result, then kick a fresh
    /// probe on startup and roughly every 10s. The probe runs on a background thread with a
    /// short timeout so a dead backend never blocks the UI; the result returns via a channel.
    /// A real 1-token completion is used (not a `/models` ping) so a router with no model
    /// loaded reads as `NoModel`, not healthy.
    pub(crate) fn tick_health_probe(&mut self) {
        // 1) Adopt a completed probe.
        if let Some(rx) = &self.health_rx {
            if let Ok(h) = rx.try_recv() {
                self.backend_health = Some(h);
                self.health_rx = None;
            }
        }
        // 2) Kick a new one if none is in flight and it's been ~10s (or we've never probed).
        let due = self
            .last_health_probe
            .is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(10));
        if self.health_rx.is_none() && due {
            let (base_url, model) = (self.cfg.base_url.clone(), self.cfg.model.clone());
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                // A bare backend (no context-detection HTTP at build time) with a short probe
                // timeout — fail fast on a dead endpoint.
                let backend = sc_model::OpenAiBackend::new(base_url, model);
                let _ = tx.send(backend.health_probe(4));
            });
            self.health_rx = Some(rx);
            self.last_health_probe = Some(std::time::Instant::now());
        }
    }

    /// The slug for a wrapped feature plan, derived from the user's most recent request (the
    /// feature they asked to plan) so it lands as `PLAN-<feature>.md` — not from the open file
    /// (the old `plan_slug()` bug that produced `PLAN-todo.md` for a seat-types plan). Falls
    /// back to `feature` when there's no user turn to name it after.
    pub(crate) fn plan_slug_for_reply(&self) -> String {
        self.chat_turns
            .iter()
            .rev()
            .find(|t| t.role == sc_win::chat::Speaker::You)
            .map(|t| sc_win::chat::slug_for(&t.text))
            .filter(|s| s != "feature")
            .unwrap_or_else(|| "feature".to_string())
    }

    /// The user's most recent message (verbatim), for recording as the spec's `## Request`.
    pub(crate) fn last_user_request(&self) -> String {
        self.chat_turns
            .iter()
            .rev()
            .find(|t| t.role == sc_win::chat::Speaker::You)
            .map(|t| t.text.clone())
            .unwrap_or_default()
    }

    /// Decide where the next terminal command runs, starting the sandbox container on first
    /// use. Mirrors the agent: `self.cfg.sandbox()` is the single source of truth for Host vs
    /// Docker, so the terminal and the agent always execute in the same place.
    ///
    /// **Strict containment:** when Docker is the configured intent (`use_docker`), this NEVER
    /// silently falls back to the host. If it can't sandbox — no project open (nothing to
    /// mount) or the container won't start (Docker down) — it returns `Err(reason)` and the
    /// caller refuses to run the command. The host shell is used ONLY when sandboxing is
    /// explicitly off. This guarantees you can never *think* you're contained while running on
    /// the host.
    pub(crate) fn term_exec_mode(&mut self) -> Result<sc_win::terminal::ExecMode, String> {
        use sc_win::terminal::ExecMode;

        // Sandbox off → an explicit, plain host scratch shell (in the workspace if one's open).
        let image = match self.cfg.sandbox() {
            sc_verify::Sandbox::Host => {
                let cwd = self
                    .picked_workspace
                    .clone()
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                return Ok(ExecMode::Host { cwd });
            }
            sc_verify::Sandbox::Docker { image } => image,
            // Session is a runtime-only state built from the live container, never returned by
            // `cfg.sandbox()`; use its image for exhaustiveness.
            sc_verify::Sandbox::Session(c) => c.image().to_string(),
        };

        // Docker is intended. A project must be open — the container mounts the workspace.
        if self.picked_workspace.is_none() {
            return Err(
                "sandbox terminal needs a project open (open a folder to mount into the container)"
                    .to_string(),
            );
        }
        let sc = self.ensure_session_container(&image)?;
        Ok(ExecMode::Container(sc))
    }

    /// Ensure the workspace's shared session container is running (starting it once on first
    /// use, clearing any stale one first), and return a handle. Shared by the terminal and the
    /// agent so BOTH `docker exec` into the SAME container — the agent's file/state changes are
    /// then visible in the terminal and vice-versa. `Err` with a human reason if no project is
    /// open or Docker won't start.
    pub(crate) fn ensure_session_container(
        &mut self,
        image: &str,
    ) -> Result<sc_verify::SessionContainer, String> {
        let Some(cwd) = self.picked_workspace.clone() else {
            return Err("no project open".to_string());
        };
        let sc = self
            .term_container
            .get_or_insert_with(|| sc_verify::SessionContainer::new(&cwd, image))
            .clone();
        if !self.term_container_started {
            // Clear any stale container from a previous session, then start fresh, detached.
            let _ = sc.stop_command().output();
            match sc.start_command(&cwd).output() {
                Ok(o) if o.status.success() => {
                    self.terminal.note(format!(
                        "▶ sandbox container `{}` started — commands run inside it",
                        sc.name()
                    ));
                    self.term_container_started = true;
                }
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr);
                    self.term_container = None;
                    return Err(format!(
                        "could not start sandbox container: {} — is Docker running?",
                        err.trim()
                    ));
                }
                Err(e) => {
                    self.term_container = None;
                    return Err(format!("docker not available: {e}"));
                }
            }
        }
        Ok(sc)
    }

    /// The sandbox an agent run should use: the shared session container when sandboxing is on
    /// and a project is open (so the agent execs into the same container as the terminal),
    /// else whatever `cfg.sandbox()` decides (host, or per-run Docker as a fallback). Starting
    /// the container can fail (Docker down) — on failure we log and fall back to `cfg.sandbox()`
    /// so a run still proceeds rather than being blocked.
    pub(crate) fn agent_sandbox(&mut self) -> sc_verify::Sandbox {
        let image = match self.cfg.sandbox() {
            sc_verify::Sandbox::Docker { image } if self.picked_workspace.is_some() => image,
            other => return other,
        };
        match self.ensure_session_container(&image) {
            Ok(sc) => sc_verify::Sandbox::Session(sc),
            Err(reason) => {
                self.rows.push(Row::ok(
                    "⚠",
                    format!("sandbox container unavailable ({reason}) — using per-run container"),
                ));
                sc_verify::Sandbox::Docker { image }
            }
        }
    }

    /// Tear down the workspace sandbox container (force-remove) and reset its state. Called on
    /// project switch and app close so a container never outlives the project that owns it.
    pub(crate) fn teardown_term_container(&mut self) {
        if let Some(sc) = self.term_container.take() {
            let _ = sc.stop_command().output();
        }
        self.term_container_started = false;
    }

    pub(crate) fn pump(&mut self) {
        self.pump_remote();
        self.tick_health_probe();
        self.pump_terminal();
        self.pump_chat();
        self.pump_triage();
        self.pump_replace();
        // While a run is in flight, keep the code view + change-highlight fresh from disk so
        // edits land live (the agent edits the real files). This does a SYNCHRONOUS `git diff`
        // on the UI thread, which at the 50ms tick rate froze the UI on a big repo (void-claim)
        // — throttle to ~1s so the view stays live without starving rendering.
        // Live-view refresh is now driven asynchronously by `live_reload_task()` (called from the
        // Tick handler, which can return a Task) — never synchronously here, since it does a
        // file read + git diff that was blocking the UI thread 80–432ms per call.
        let Some(session) = &self.session else {
            return;
        };
        for ev in session.drain_events() {
            // Tee this event out to any remote (phone) mirror. Agent/Swarm events are
            // already serde `AgentEvent`s the Hub takes directly; the others become a
            // short ChatMessage note so the remote log still reflects run boundaries.
            if let Some(m) = &self.remote {
                match &ev {
                    UiEvent::Agent(e) => m.push(e.clone()),
                    UiEvent::Done { summary, .. } => m.push(sc_core::AgentEvent::ChatMessage {
                        role: "system".into(),
                        text: format!("run finished: {summary}"),
                    }),
                    UiEvent::Failed(msg) => m.push(sc_core::AgentEvent::ChatMessage {
                        role: "system".into(),
                        text: format!("run failed: {msg}"),
                    }),
                    // Swarm/Phase mirroring is out of v1 scope (agent + chat only).
                    _ => {}
                }
            }
            match ev {
                UiEvent::Agent(e) => {
                    // A staged run streams its per-phase prompt + reply into the chat thread as
                    // ChatMessage/ChatDelta (so a slow phase reads as alive, not frozen). Fold them
                    // the same way `pump_chat` folds a live chat turn: a ChatMessage is a terminal
                    // turn, a ChatDelta grows the live "typing" bubble. (The regular agent/iterate
                    // flow uses the ChatEvent path in `pump_chat`; the session's staged run has no
                    // ChatSession, so it routes through UiEvent::Agent here.)
                    if let sc_core::AgentEvent::ChatMessage { role, text } = &e {
                        // COMMIT any in-flight streamed reply as a finished turn before this new
                        // message — otherwise the streamed phase reply (which only lived in the
                        // transient `streaming` bubble) is thrown away when the NEXT phase's header
                        // arrives, and the chat shows headers with no content (the bug: only the
                        // last phase's reply survived).
                        self.commit_streaming_turn();
                        let speaker = match role.as_str() {
                            "you" => sc_win::chat::Speaker::You,
                            _ => sc_win::chat::Speaker::Agent,
                        };
                        self.chat_turns.push(sc_win::chat::Turn {
                            role: speaker,
                            text: text.clone(),
                        });
                        continue;
                    }
                    if let sc_core::AgentEvent::ChatDelta { cumulative } = &e {
                        // Grow the live bubble with the full cumulative reply so far. The view
                        // renders `self.streaming` whenever it's Some, so this types on screen; the
                        // next phase's header ChatMessage finalizes it (no duplicate terminal turn).
                        self.streaming = Some(cumulative.clone());
                        continue;
                    }
                    // Live "watch it type": as the model streams a write/edit, preview the
                    // growing file content in the code view, word by word, before it lands.
                    if let sc_core::AgentEvent::ContentDelta { cumulative, .. } = &e {
                        if let Some(p) = sc_win::codeview::partial_edit_preview(cumulative) {
                            if !p.content.is_empty() {
                                let name = p
                                    .file
                                    .clone()
                                    .or_else(|| self.selected_file.clone())
                                    .unwrap_or_else(|| "(writing…)".to_string());
                                if p.file.is_some() {
                                    self.selected_file = p.file.clone();
                                    self.follow_agent = false;
                                }
                                self.code = Some(sc_win::codeview::from_text(&name, &p.content));
                            }
                        }
                        continue; // a delta is preview-only; nothing else to fold
                    }
                    if let sc_core::AgentEvent::Planned { steps }
                    | sc_core::AgentEvent::PlanRevised { steps } = &e
                    {
                        self.board = steps.clone();
                    }
                    if let sc_core::AgentEvent::Verification { summary, .. } = &e {
                        self.verify_text = Some(summary.clone());
                    }
                    // Record files the agent actually edited/wrote (for the iterate banner).
                    if sc_win::codeview::is_mutating_touch(&e) {
                        if let Some(rel) = sc_win::codeview::file_touched_by(&e) {
                            if !self.edited_files.contains(&rel) {
                                self.edited_files.push(rel);
                            }
                        }
                    }
                    // During a line-comment fix, narrate the key steps into the chat so the
                    // work is VISIBLE (a fix used to run silently). Only meaningful steps —
                    // editing a file and the verify result — not every read.
                    if self.iterate_from_comment {
                        if let Some(line) = fix_feed_line(&e) {
                            self.chat_turns.push(sc_win::chat::Turn {
                                role: sc_win::chat::Speaker::Agent,
                                text: line,
                            });
                        }
                    }
                    // Follow the agent: when it touches a file and we're in follow mode,
                    // show that file in the code panel — so edits land in front of you.
                    if self.follow_agent {
                        if let Some(rel) = sc_win::codeview::file_touched_by(&e) {
                            self.select_file(rel);
                        }
                    }
                    // (A tool result may have changed the shown file on disk; the live view is
                    // refreshed asynchronously by the Tick-driven `live_reload_task`, not
                    // synchronously here — the reload does a file read + git diff that blocked the
                    // UI thread 80–432ms per call when done per-event.)
                    self.rows.extend(agent_rows(&e));
                }
                UiEvent::Swarm(e) => {
                    // Fold into the live topology (canvas) and the per-subtask board,
                    // and append to the flat activity stream.
                    self.topology.apply(&e, self.now());
                    self.swarm_board.apply(&e);
                    self.rows.extend(swarm_rows(&e));
                }
                UiEvent::Phase {
                    phase,
                    content,
                    tests_written,
                    dir,
                } => {
                    // Fold a staged-workflow phase into the plan (the plan panel) and
                    // note it in the activity stream. `dir` (workspace-relative artifact dir)
                    // teaches the plan each phase's file path — the master list opens it in
                    // the code view and harvests line-comments on it for send-back.
                    self.plan
                        .apply(phase, &content, &tests_written, dir.as_deref());
                    if tests_written.is_empty() {
                        self.rows
                            .push(Row::ok("◆", format!("plan · {}", phase.title())));
                    } else {
                        self.rows.push(Row::ok(
                            "✓",
                            format!("wrote {} frozen test file(s)", tests_written.len()),
                        ));
                    }
                }
                UiEvent::Done { ok, summary } => {
                    self.finish_run(ok, &summary);
                    self.session = None;
                }
                UiEvent::Failed(msg) => {
                    self.finish_run(false, &format!("error: {msg}"));
                    self.session = None;
                }
            }
        }
        // Move any new decision requests onto the gate bar.
        let pending: Vec<Pending> = match &self.session {
            Some(session) => session.drain_pending(),
            None => Vec::new(),
        };
        // When a workflow gate arrives, auto-open its phase file in CODE so the user can read
        // (and line-comment) the artifact immediately — the details live in the editor now.
        let mut open_gate_file: Option<String> = None;
        {
            for p in pending {
                self.gatebar.push(match p {
                    Pending::Confirm {
                        command,
                        default_reason,
                        reply,
                    } => {
                        // If the remote mirror is live, register this confirm so a phone can
                        // approve/deny it. Both the local buttons and the remote resolve hold
                        // a clone of the reply sender; whichever answers first wins (the
                        // worker's `recv()` takes the first `send`). `id` = 0 with no mirror.
                        let id = self
                            .remote
                            .as_ref()
                            .map(|m| m.register_confirm(&command, &default_reason, reply.clone()))
                            .unwrap_or(0);
                        Gatebar::Confirm {
                            id,
                            command,
                            reason: default_reason,
                            reply,
                        }
                    }
                    Pending::Gate {
                        phase,
                        content,
                        reply,
                    } => {
                        // Remember to open this phase's artifact file once the borrow ends.
                        if let Some(path) = self.plan.path_for(phase) {
                            open_gate_file = Some(path);
                        }
                        Gatebar::Gate {
                            phase,
                            content,
                            reply,
                        }
                    }
                });
            }
        }
        // Auto-open the gating artifact (outside the loop so `select_file`'s `&mut self` is free).
        // Pin the view (follow_agent = false) so a live run doesn't scroll it away.
        if let Some(path) = open_gate_file {
            self.follow_agent = false;
            self.select_file(path);
        }
    }

    /// Answer the oldest pending confirm with `c` (no-op if the front isn't a confirm).
    pub(crate) fn answer_confirm(&mut self, c: Confirmation) {
        if matches!(self.gatebar.first(), Some(Gatebar::Confirm { .. })) {
            if let Gatebar::Confirm { reply, .. } = self.gatebar.remove(0) {
                let _ = reply.send(c);
            }
        }
    }

    /// The phase currently stopped at a human gate, if any — the front gatebar entry when
    /// it's a workflow `Gate` (the worker blocks on one at a time). The master list marks
    /// this row as "gating" and shows its Approve / Send-back / Abort buttons inline.
    pub(crate) fn gating_phase(&self) -> Option<Phase> {
        match self.gatebar.first() {
            Some(Gatebar::Gate { phase, .. }) => Some(*phase),
            _ => None,
        }
    }

    /// Answer the oldest pending workflow gate with `d`.
    pub(crate) fn answer_gate(&mut self, d: Decision) {
        if matches!(self.gatebar.first(), Some(Gatebar::Gate { .. })) {
            if let Gatebar::Gate { phase, reply, .. } = self.gatebar.remove(0) {
                // Narrate the gate DECISION into the chat thread, so the staged run's back-and-forth
                // (prompt → streamed reply → decision) reads as a complete conversation. The session
                // can't see the decision itself — it's resolved here in the app over the gate's
                // private reply channel — so this is where it becomes a visible turn.
                let note = match &d {
                    Decision::Approve => format!("✓ {} approved", phase.title()),
                    Decision::Revise => format!("✎ {} revised", phase.title()),
                    Decision::SendBack { target, notes } => match notes {
                        Some(n) => {
                            format!("↩ sent {} back to {} — {n}", phase.title(), target.title())
                        }
                        None => format!("↩ sent {} back to {}", phase.title(), target.title()),
                    },
                    Decision::Abort => format!("■ aborted at {}", phase.title()),
                };
                // Commit the phase's streamed reply as a turn, THEN log the decision after it — so
                // the gated phase's content sticks in the thread instead of vanishing.
                self.commit_streaming_turn();
                self.chat_turns.push(sc_win::chat::Turn {
                    role: sc_win::chat::Speaker::You,
                    text: note,
                });
                let _ = reply.send(d);
            }
            self.sendback_notes.clear();
        }
    }
}
