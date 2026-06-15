//! The iced application — thin rendering glue over the tested `dc_win` library.
//!
//! All "what to show / what to run" logic lives in [`crate::view`], [`crate::config`],
//! [`crate::session`], and [`crate::bridge`]; this file only lays those out as
//! widgets, pumps the worker channels on a timer tick, and routes button clicks back
//! to the blocking decision seams. Keep it thin.

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use iced::widget::{button, checkbox, column, container, row, scrollable, text, text_input, Space};
use iced::{Element, Fill, Length, Subscription, Task, Theme};

use dc_core::Confirmation;
use dc_win::bridge::Pending;
use dc_win::config::ToolCalling;
use dc_win::session::{RunKind, Session, UiEvent};
use dc_win::view::{agent_rows, swarm_rows, Row};
use dc_win::UiConfig;
use dc_workflow::{Decision, Phase};

/// Launch the desktop app.
pub fn run() -> iced::Result {
    // iced 0.14: `application(boot, update, view)` where boot returns the initial
    // (State, Task); title/subscription/theme are builder methods.
    iced::application(|| (App::default(), Task::none()), App::update, App::view)
        .title(App::title)
        .subscription(App::subscription)
        .theme(App::theme)
        .run()
}

/// A pending decision surfaced to the human, with the reply channel to answer it.
enum Gatebar {
    Confirm {
        command: String,
        reason: String,
        reply: Sender<Confirmation>,
    },
    Gate {
        phase: Phase,
        content: String,
        reply: Sender<Decision>,
    },
}

/// The outcome of a finished run, for the result banner. Computed when the run ends by
/// pairing the honest-stop status with a scan of the output folder.
struct RunResult {
    /// True only when the run finished cleanly AND actually produced source files.
    ok: bool,
    /// A one-line headline (e.g. "5 files built, tests green").
    headline: String,
    /// The specific reason on failure (e.g. "built 0 source files — decomposition
    /// produced no subtasks"). Empty on success.
    reason: String,
    /// Source files present in the output folder (what it built).
    files: Vec<String>,
    /// The output folder, for the "open folder" button.
    dir: Option<std::path::PathBuf>,
}

struct App {
    cfg: UiConfig,
    intent: String,
    /// Editable mirrors of the config for the settings panel. Seeded from
    /// `UiConfig::default()` so the boxes show the active values (and a run never
    /// reads a blank input over a good default — see [`App::default`]).
    model_input: String,
    url_input: String,
    advisor_input: String,
    advisor_url_input: String,
    orch_model_input: String,
    orch_url_input: String,
    verify_input: String,
    suffix_input: String,
    settings_open: bool,
    /// Activity rows accumulated from the event stream.
    rows: Vec<Row>,
    /// The latest single-run plan steps (right panel, agent mode).
    board: Vec<String>,
    /// The live per-subtask board (right panel, swarm mode).
    swarm_board: dc_win::SwarmBoard,
    /// The staged-workflow plan (TDD mode), shown in the always-visible plan panel.
    plan: dc_win::Plan,
    /// The live swarm topology (advisor/orchestrator/coders + glowing flows), drawn on
    /// the canvas during a swarm run.
    topology: dc_win::Topology,
    /// When the current run started, for the canvas's monotonic animation clock.
    run_started: Option<Instant>,
    /// The latest verification text (bottom panel), failure-first.
    verify_text: Option<String>,
    /// The closing honest-stop summary, set when the run ends.
    summary: Option<String>,
    /// The outcome banner of the last finished run (success/failure + files built).
    result: Option<RunResult>,
    /// The live run, if one is in flight.
    session: Option<Session>,
    /// Pending human decisions (FIFO; the oldest is shown).
    gatebar: Vec<Gatebar>,
    /// Send-back notes the human is typing for a workflow checkpoint.
    sendback_notes: String,
    /// The coder box selected on the topology canvas (shows its prompt + proposal in
    /// the detail panel). `None` = show the orchestrator's decomposition reply.
    selected_coder: Option<String>,
    /// The actual folder the current/last run writes to (a picked dir, or a fresh
    /// datetime folder).
    run_dir: Option<std::path::PathBuf>,
    /// A folder the user picked to work in. When set, runs go HERE (so a follow-up
    /// prompt iterates on the existing files) instead of a fresh datetime folder.
    picked_workspace: Option<std::path::PathBuf>,
}

impl Default for App {
    fn default() -> Self {
        // Seed the editable input boxes from the config defaults, so the settings
        // panel shows the active values and `start()` never commits a blank input
        // over a sensible default (URL, /no_think suffix, …).
        let cfg = UiConfig::default();
        Self {
            url_input: cfg.base_url.clone(),
            model_input: cfg.model.clone(),
            advisor_input: cfg.advisor_model.clone().unwrap_or_default(),
            advisor_url_input: cfg.advisor_url.clone().unwrap_or_default(),
            orch_model_input: cfg.orchestrator_model.clone().unwrap_or_default(),
            orch_url_input: cfg.orchestrator_url.clone().unwrap_or_default(),
            verify_input: cfg.verify_command.clone().unwrap_or_default(),
            suffix_input: cfg.system_suffix.clone().unwrap_or_default(),
            cfg,
            intent: String::new(),
            settings_open: false,
            rows: Vec::new(),
            board: Vec::new(),
            swarm_board: dc_win::SwarmBoard::default(),
            plan: dc_win::Plan::default(),
            topology: dc_win::Topology::default(),
            run_started: None,
            verify_text: None,
            summary: None,
            result: None,
            session: None,
            gatebar: Vec::new(),
            sendback_notes: String::new(),
            selected_coder: None,
            run_dir: None,
            picked_workspace: None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
    IntentChanged(String),
    ModelChanged(String),
    UrlChanged(String),
    AdvisorChanged(String),
    AdvisorUrlChanged(String),
    OrchModelChanged(String),
    OrchUrlChanged(String),
    VerifyChanged(String),
    SuffixChanged(String),
    ToggleSettings,
    ToggleYolo(bool),
    ToggleDryRun(bool),
    RunTdd,
    Tick,
    // Confirm-gate answers.
    ConfirmAllow,
    ConfirmDeny,
    ConfirmRemember,
    // Workflow-gate answers.
    NotesChanged(String),
    GateApprove,
    GateRevise,
    GateSendBack,
    GateAbort,
    // Topology canvas interaction.
    SelectCoder(String),
    ClearSelection,
    // Workspace folder.
    PickWorkspace,
    ClearWorkspace,
    /// Open the output folder of the last run in the system file manager.
    OpenOutputFolder,
}

impl App {
    fn title(&self) -> String {
        "dumb-coder — vibe coding".to_string()
    }

    fn theme(&self) -> Theme {
        Theme::TokyoNight
    }

    fn subscription(&self) -> Subscription<Message> {
        // Tick while there's anything live to pump or animate: a running session, an
        // open gate, or topology flows still glowing (so the fade animates to rest
        // even after the run ends). iced delivers the tick on the UI thread, so
        // draining the std::mpsc Receiver here is safe.
        let glowing = !self.topology.active_flows(self.now()).is_empty();
        if self.session.is_some() || !self.gatebar.is_empty() || glowing {
            iced::time::every(Duration::from_millis(50)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        }
    }

    fn start(&mut self, kind: RunKind) {
        if self.intent.trim().is_empty() || self.session.is_some() {
            return;
        }
        // Commit the settings inputs into the config before the run.
        self.cfg.model = self.model_input.clone();
        self.cfg.base_url = self.url_input.clone();
        self.cfg.advisor_model = non_empty(&self.advisor_input);
        self.cfg.advisor_url = non_empty(&self.advisor_url_input);
        self.cfg.orchestrator_model = non_empty(&self.orch_model_input);
        self.cfg.orchestrator_url = non_empty(&self.orch_url_input);
        self.cfg.verify_command = non_empty(&self.verify_input);
        self.cfg.system_suffix = non_empty(&self.suffix_input);

        self.rows.clear();
        self.board.clear();
        self.swarm_board = dc_win::SwarmBoard::default();
        self.plan = dc_win::Plan::default();
        self.topology = dc_win::Topology::default();
        self.selected_coder = None;
        self.run_started = Some(Instant::now());
        self.verify_text = None;
        self.summary = None;
        self.result = None;
        self.gatebar.clear();

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
    fn now(&self) -> f32 {
        self.run_started
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0)
    }

    /// Whether a swarm run is (or was) active — i.e. the topology has nodes to draw.
    fn is_swarm(&self) -> bool {
        !self.topology.is_empty()
    }

    /// Compute the outcome banner when a run ends: pair the honest-stop status with a
    /// scan of the output folder so the result is never ambiguous — you always see
    /// whether files were actually built and, if not, why.
    fn finish_run(&mut self, ok: bool, summary: &str) {
        let dir = self.run_dir.clone();
        let files = dir
            .as_deref()
            .map(dc_win::config::source_files)
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
        });
    }

    /// Drain the worker channels into UI state. Called each tick.
    fn pump(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        for ev in session.drain_events() {
            match ev {
                UiEvent::Agent(e) => {
                    if let dc_core::AgentEvent::Planned { steps }
                    | dc_core::AgentEvent::PlanRevised { steps } = &e
                    {
                        self.board = steps.clone();
                    }
                    if let dc_core::AgentEvent::Verification { summary, .. } = &e {
                        self.verify_text = Some(summary.clone());
                    }
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
                } => {
                    // Fold a staged-workflow phase into the plan (the plan panel) and
                    // note it in the activity stream.
                    self.plan.apply(phase, &content, &tests_written);
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
        if let Some(session) = &self.session {
            for p in session.drain_pending() {
                self.gatebar.push(match p {
                    Pending::Confirm {
                        command,
                        default_reason,
                        reply,
                    } => Gatebar::Confirm {
                        command,
                        reason: default_reason,
                        reply,
                    },
                    Pending::Gate {
                        phase,
                        content,
                        reply,
                    } => Gatebar::Gate {
                        phase,
                        content,
                        reply,
                    },
                });
            }
        }
    }

    /// Answer the oldest pending confirm with `c` (no-op if the front isn't a confirm).
    fn answer_confirm(&mut self, c: Confirmation) {
        if matches!(self.gatebar.first(), Some(Gatebar::Confirm { .. })) {
            if let Gatebar::Confirm { reply, .. } = self.gatebar.remove(0) {
                let _ = reply.send(c);
            }
        }
    }

    /// Answer the oldest pending workflow gate with `d`.
    fn answer_gate(&mut self, d: Decision) {
        if matches!(self.gatebar.first(), Some(Gatebar::Gate { .. })) {
            if let Gatebar::Gate { reply, .. } = self.gatebar.remove(0) {
                let _ = reply.send(d);
            }
            self.sendback_notes.clear();
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::IntentChanged(s) => self.intent = s,
            Message::ModelChanged(s) => self.model_input = s,
            Message::UrlChanged(s) => self.url_input = s,
            Message::AdvisorChanged(s) => self.advisor_input = s,
            Message::AdvisorUrlChanged(s) => self.advisor_url_input = s,
            Message::OrchModelChanged(s) => self.orch_model_input = s,
            Message::OrchUrlChanged(s) => self.orch_url_input = s,
            Message::VerifyChanged(s) => self.verify_input = s,
            Message::SuffixChanged(s) => self.suffix_input = s,
            Message::ToggleSettings => self.settings_open = !self.settings_open,
            Message::ToggleYolo(v) => self.cfg.yolo = v,
            Message::ToggleDryRun(v) => self.cfg.dry_run = v,
            Message::RunTdd => self.start(RunKind::Tdd),
            Message::Tick => self.pump(),
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
            Message::GateRevise => self.answer_gate(Decision::Revise),
            Message::GateSendBack => {
                let (target, notes) = match self.gatebar.first() {
                    Some(Gatebar::Gate { phase, .. }) => (*phase, non_empty(&self.sendback_notes)),
                    _ => (Phase::Specs, None),
                };
                self.answer_gate(Decision::SendBack { target, notes });
            }
            Message::GateAbort => self.answer_gate(Decision::Abort),
            Message::SelectCoder(id) => self.selected_coder = Some(id),
            Message::ClearSelection => self.selected_coder = None,
            Message::PickWorkspace => {
                // Native folder dialog (blocking — fine for a button click). When a
                // folder is chosen, runs go there so follow-up prompts iterate on it.
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title("Choose a project folder to work in")
                    .pick_folder()
                {
                    self.picked_workspace = Some(dir);
                }
            }
            Message::ClearWorkspace => self.picked_workspace = None,
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
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let header = self.view_header();
        // One run path (the TDD build): the plan panel on the left; on the right, the
        // live coder topology once implementation starts, else the activity stream.
        // Before a build, just the activity stream (idle / last result).
        let body: Element<'_, Message> = if self.plan.started() {
            // Plan panel on the left; on the right, the live coder topology once
            // implementation starts (click a coder → its prompt/output shows in the
            // bottom panel), else the activity stream.
            let right = if self.is_swarm() {
                self.view_topology()
            } else {
                self.view_activity()
            };
            row![self.view_plan(), right]
                .spacing(12)
                .height(Length::FillPortion(3))
                .into()
        } else {
            self.view_activity()
        };
        let tests = self.view_coder_io();
        let gate = self.view_gatebar();

        let mut col = column![header].spacing(12).padding(16);
        // The step-flow strip sits right under the header once a build is underway, so
        // you can see which phase it's on at a glance.
        if self.plan.started() {
            col = col.push(self.view_step_flow());
        }
        // A prominent outcome banner sits right under the header when a run has
        // finished, so the result is never ambiguous.
        if let Some(banner) = self.view_banner() {
            col = col.push(banner);
        }
        col = col.push(body).push(tests);
        if let Some(g) = gate {
            col = col.push(g);
        }
        if self.settings_open {
            col = col.push(self.view_settings());
        }
        container(col).width(Fill).height(Fill).into()
    }

    /// The outcome banner: a clear ✓/✗ headline, the reason on failure, the files
    /// built, and an "open folder" button — so the result is unmistakable.
    fn view_banner(&self) -> Option<Element<'_, Message>> {
        // Don't show a stale banner while a new run is in flight.
        if self.session.is_some() {
            return None;
        }
        let r = self.result.as_ref()?;
        let (mark, color) = if r.ok {
            ("✓", iced::Color::from_rgb(0.45, 0.78, 0.55))
        } else {
            ("✗", iced::Color::from_rgb(0.93, 0.42, 0.42))
        };
        let mut col = column![text(format!("{mark}  {}", r.headline))
            .size(18)
            .color(color)]
        .spacing(4);
        if !r.reason.is_empty() {
            col = col.push(text(&r.reason).size(13));
        }
        // The files it built (capped), so you can see the actual output.
        for f in r.files.iter().take(12) {
            col = col.push(text(format!("  • {f}")).size(12));
        }
        if r.files.len() > 12 {
            col = col.push(text(format!("  … and {} more", r.files.len() - 12)).size(12));
        }
        if r.dir.is_some() {
            col =
                col.push(button(text("📂 open output folder")).on_press(Message::OpenOutputFolder));
        }
        Some(container(col).width(Fill).padding(12).into())
    }

    /// The horizontal step-flow at the top: each phase with arrows between, the current
    /// phase highlighted, done phases checked, plus a final "Build" step that lights up
    /// once planning is complete and implementation begins.
    fn view_step_flow(&self) -> Element<'_, Message> {
        let current = self.plan.current_phase();
        let done_color = iced::Color::from_rgb(0.45, 0.78, 0.55); // green
        let now_color = iced::Color::from_rgb(0.48, 0.65, 0.98); // blue
        let dim_color = iced::Color::from_rgb(0.4, 0.43, 0.55);
        let arrow_color = iced::Color::from_rgb(0.35, 0.38, 0.5);

        let mut flow = row![].spacing(6).align_y(iced::Alignment::Center);
        let steps = self.plan.steps();
        for (i, step) in steps.iter().enumerate() {
            let is_current = current == Some(step.phase);
            let (mark, color, size) = if step.done {
                ("✓", done_color, 13)
            } else if is_current {
                ("▶", now_color, 15)
            } else {
                ("·", dim_color, 13)
            };
            let label = text(format!("{mark} {}", step.phase.title()))
                .size(size)
                .color(color);
            flow = flow.push(label);
            flow = flow.push(text("→").size(13).color(arrow_color));
            let _ = i;
        }
        // The final "Build" step: active once planning is complete (no current phase
        // left) and a swarm is running; done when source files were built.
        let built = self.result.as_ref().is_some_and(|r| r.ok);
        let building = current.is_none() && self.is_swarm();
        let (bmark, bcolor) = if built {
            ("✓", done_color)
        } else if building {
            ("▶", now_color)
        } else {
            ("·", dim_color)
        };
        flow = flow.push(
            text(format!("{bmark} Build"))
                .size(if building { 15 } else { 13 })
                .color(bcolor),
        );

        container(
            scrollable(flow).direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(2).scroller_width(2),
            )),
        )
        .width(Fill)
        .padding(6)
        .into()
    }

    fn view_header(&self) -> Element<'_, Message> {
        let running = self.session.is_some();
        let intent = text_input(
            "Describe what you want — it plans, writes the tests, then builds it…",
            &self.intent,
        )
        .on_input(Message::IntentChanged)
        .on_submit(Message::RunTdd)
        .padding(10)
        .width(Fill);

        // One path: the TDD build (plan → write tests first → implement to green). It's
        // the only run action — strictly better than a bare swarm or a single agent that
        // can't tell whether it actually succeeded.
        let build_btn = if running {
            button(text("building…")).width(Length::Fixed(120.0))
        } else {
            button(text("⚒ build"))
                .on_press(Message::RunTdd)
                .width(Length::Fixed(120.0))
        };
        // 📁 picks a project folder to iterate in; once picked it becomes a "new
        // project" button that clears the selection (back to fresh datetime folders).
        let folder_btn = if self.picked_workspace.is_some() {
            button(text("✕ new project"))
                .on_press(Message::ClearWorkspace)
                .width(Length::Fixed(120.0))
        } else {
            button(text("📁 folder"))
                .on_press(Message::PickWorkspace)
                .width(Length::Fixed(120.0))
        };
        let settings_btn = button(text("⚙ settings"))
            .on_press(Message::ToggleSettings)
            .width(Length::Fixed(110.0));

        let controls = row![intent, build_btn, folder_btn, settings_btn].spacing(8);
        // Show where output goes. A picked folder means follow-up prompts iterate on it;
        // otherwise each run gets a fresh datetime folder under the scratch base.
        let where_to = match (&self.picked_workspace, &self.run_dir) {
            (Some(dir), _) => format!(
                "working in: {}  (iterating — runs edit these files)",
                dir.display()
            ),
            (None, Some(d)) => format!("output: {}", d.display()),
            (None, None) => format!(
                "output base: {}  (a run-<datetime> folder per prompt; 📁 to pick a project)",
                self.cfg.workspace.display()
            ),
        };
        let ws_line = text(where_to)
            .size(11)
            .color(iced::Color::from_rgb(0.55, 0.58, 0.70));
        column![controls, ws_line].spacing(4).into()
    }

    fn view_activity(&self) -> Element<'_, Message> {
        let mut col = column![text("activity").size(14)].spacing(4);
        for r in &self.rows {
            let line = text(format!("{}  {}", r.icon, r.text)).size(13);
            let line = if r.is_error {
                line.color(iced::Color::from_rgb(0.93, 0.42, 0.42))
            } else {
                line
            };
            col = col.push(line);
        }
        if let Some(s) = &self.summary {
            col = col.push(Space::new().height(Length::Fixed(6.0)));
            col = col.push(text(s).size(14));
        }
        container(scrollable(col).height(Fill))
            .width(Length::FillPortion(2))
            .padding(8)
            .into()
    }

    fn view_topology(&self) -> Element<'_, Message> {
        let canvas = crate::canvas::TopologyCanvas::new(
            &self.topology,
            self.now(),
            self.selected_coder.as_deref(),
        )
        .view();
        container(column![text("swarm topology").size(14), canvas].spacing(4))
            .width(Length::FillPortion(2))
            .height(Fill)
            .padding(8)
            .into()
    }

    /// The always-visible plan panel (TDD mode): the six workflow phases with status,
    /// the frozen tests written, and the readable subtask list — so you can see what it
    /// intends to do, before and while it does it.
    fn view_plan(&self) -> Element<'_, Message> {
        let mut col = column![text("plan (TDD)").size(15)].spacing(4);
        for step in self.plan.steps() {
            let mark = if step.done { "✓" } else { "·" };
            let line = text(format!("{mark} {}", step.title)).size(13);
            let line = if step.done {
                line
            } else {
                line.color(iced::Color::from_rgb(0.5, 0.53, 0.66))
            };
            col = col.push(line);
            // Show the produced artifact text under each completed phase.
            if step.done && !step.content.is_empty() {
                let preview: String = step.content.lines().take(10).collect::<Vec<_>>().join("\n");
                col = col.push(
                    text(preview)
                        .size(11)
                        .color(iced::Color::from_rgb(0.7, 0.73, 0.84)),
                );
            }
        }

        if !self.plan.frozen_tests.is_empty() {
            col = col.push(Space::new().height(Length::Fixed(8.0)));
            col = col.push(
                text(format!("frozen tests ({}):", self.plan.frozen_tests.len()))
                    .size(13)
                    .color(iced::Color::from_rgb(0.45, 0.78, 0.55)),
            );
            for t in &self.plan.frozen_tests {
                col = col.push(text(format!("  🔒 {t}")).size(12));
            }
        }

        if !self.plan.subtasks.is_empty() {
            col = col.push(Space::new().height(Length::Fixed(8.0)));
            col = col.push(text("subtasks to implement:").size(13));
            for (i, g) in self.plan.subtasks.iter().enumerate() {
                col = col.push(text(format!("  {}. {g}", i + 1)).size(12));
            }
        }

        container(scrollable(col).height(Fill))
            .width(Length::FillPortion(2))
            .padding(8)
            .into()
    }

    /// The bottom panel: the prompt sent to the selected coder and the output it
    /// proposed back. Click a coder box on the topology to select one; otherwise it
    /// hints how, or shows the latest verification result.
    fn view_coder_io(&self) -> Element<'_, Message> {
        let selected = self
            .selected_coder
            .as_deref()
            .and_then(|id| self.topology.coder(id));

        let inner = match selected {
            Some(c) => {
                let prompt = c.prompt.clone().unwrap_or_else(|| "(not captured)".into());
                let proposal = c
                    .proposal
                    .clone()
                    .unwrap_or_else(|| "(still working…)".into());
                column![
                    text(format!("coder [{}] — {}", c.subtask, c.goal)).size(14),
                    text("▸ prompt sent to this coder").size(12),
                    text(prompt).size(11),
                    Space::new().height(Length::Fixed(6.0)),
                    text("◂ output it proposed").size(12),
                    text(proposal).size(11),
                ]
                .spacing(3)
            }
            None => {
                let hint = if self.is_swarm() {
                    "click a coder box above to see the prompt it got and the code it wrote"
                } else {
                    "the coders' prompts & output appear here during the build"
                };
                let mut c =
                    column![text("coder prompt & output").size(14), text(hint).size(12)].spacing(4);
                if let Some(v) = &self.verify_text {
                    c = c.push(Space::new().height(Length::Fixed(6.0)));
                    c = c.push(text("latest verification:").size(12));
                    c = c.push(text(v).size(11));
                }
                c
            }
        };

        container(scrollable(inner).height(Fill))
            .width(Fill)
            .height(Length::FillPortion(1))
            .padding(8)
            .into()
    }

    fn view_gatebar(&self) -> Option<Element<'_, Message>> {
        match self.gatebar.first()? {
            Gatebar::Confirm {
                command, reason, ..
            } => {
                let head = text(format!("⛳ run a shell command?  {command}")).size(14);
                let why = text(reason).size(12);
                let buttons = row![
                    button(text("Allow once")).on_press(Message::ConfirmAllow),
                    button(text("Allow & remember")).on_press(Message::ConfirmRemember),
                    button(text("Deny")).on_press(Message::ConfirmDeny),
                ]
                .spacing(8);
                Some(
                    container(column![head, why, buttons].spacing(6))
                        .width(Fill)
                        .padding(10)
                        .into(),
                )
            }
            Gatebar::Gate { phase, content, .. } => {
                let head = text(format!("⛳ checkpoint — {} phase", phase.title())).size(14);
                // Show the produced artifact so the human can read it before deciding.
                let preview = container(scrollable(text(content.clone()).size(12)))
                    .height(Length::Fixed(160.0));
                let notes = text_input("send-back notes (optional)…", &self.sendback_notes)
                    .on_input(Message::NotesChanged)
                    .padding(6);
                let buttons = row![
                    button(text("Approve")).on_press(Message::GateApprove),
                    button(text("Revise")).on_press(Message::GateRevise),
                    button(text("Send back")).on_press(Message::GateSendBack),
                    button(text("Abort")).on_press(Message::GateAbort),
                ]
                .spacing(8);
                Some(
                    container(column![head, preview, notes, buttons].spacing(6))
                        .width(Fill)
                        .padding(10)
                        .into(),
                )
            }
        }
    }

    fn view_settings(&self) -> Element<'_, Message> {
        let model = text_input("model", &self.model_input)
            .on_input(Message::ModelChanged)
            .padding(6);
        let url = text_input("backend url", &self.url_input)
            .on_input(Message::UrlChanged)
            .padding(6);
        let orch_model = text_input("orchestrator model (decomposer)", &self.orch_model_input)
            .on_input(Message::OrchModelChanged)
            .padding(6);
        let orch_url = text_input("orchestrator url", &self.orch_url_input)
            .on_input(Message::OrchUrlChanged)
            .padding(6);
        let advisor = text_input("advisor model (senior)", &self.advisor_input)
            .on_input(Message::AdvisorChanged)
            .padding(6);
        let advisor_url = text_input("advisor url", &self.advisor_url_input)
            .on_input(Message::AdvisorUrlChanged)
            .padding(6);
        let verify = text_input("verify command (optional)", &self.verify_input)
            .on_input(Message::VerifyChanged)
            .padding(6);
        let suffix = text_input("system suffix (e.g. /no_think)", &self.suffix_input)
            .on_input(Message::SuffixChanged)
            .padding(6);
        let yolo = checkbox(self.cfg.yolo)
            .label("yolo (allow shell without asking)")
            .on_toggle(Message::ToggleYolo);
        let dry = checkbox(self.cfg.dry_run)
            .label("dry-run (no writes)")
            .on_toggle(Message::ToggleDryRun);

        container(
            column![
                text("settings / connection").size(14),
                text("coder (does the file writing)").size(11),
                model,
                url,
                text("orchestrator (decomposes the task — needs a reasoning model)").size(11),
                orch_model,
                orch_url,
                text("advisor (junior asks senior on a stall)").size(11),
                advisor,
                advisor_url,
                verify,
                suffix,
                yolo,
                dry,
            ]
            .spacing(6),
        )
        .width(Fill)
        .padding(10)
        .into()
    }
}

/// "s" when `n != 1`, for plain pluralization.
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// `None` for empty/whitespace, else the trimmed value — for optional config inputs.
fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// The prefix to remember when a user clicks "Allow & remember": the command up to
/// and including the first space (so `git push` remembers `git `), or the whole
/// command if it has no space.
fn remember_prefix(command: &str) -> String {
    match command.find(' ') {
        Some(i) => command[..=i].to_string(),
        None => command.to_string(),
    }
}

// Keep `ToolCalling` referenced so the settings surface can grow into it without an
// unused-import churn; the v0 settings panel exposes the common knobs first.
#[allow(dead_code)]
const _: Option<ToolCalling> = None;
