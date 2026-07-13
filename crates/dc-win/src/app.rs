//! The iced application — thin rendering glue over the tested `dc_win` library.
//!
//! All "what to show / what to run" logic lives in [`crate::view`], [`crate::config`],
//! [`crate::session`], and [`crate::bridge`]; this file only lays those out as
//! widgets, pumps the worker channels on a timer tick, and routes button clicks back
//! to the blocking decision seams. Keep it thin.

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use iced::widget::{button, checkbox, column, container, row, scrollable, text, text_input, Space};
use iced::{Background, Border, Color, Element, Fill, Length, Subscription, Task, Theme};

use dc_core::Confirmation;
use dc_win::bridge::Pending;
use dc_win::config::ToolCalling;
use dc_win::session::{RunKind, Session, UiEvent};
use dc_win::view::{agent_rows, swarm_rows, Row};
use dc_win::UiConfig;
use dc_workflow::{Decision, Phase};

// --- Visual design tokens (Tokyo Night-aligned) -----------------------------------
// A small, consistent palette + spacing so panels read as cards on a dark canvas,
// not bare text floating on the background.

/// Panel/card surface — a hair lighter than the window background.
const SURFACE: Color = Color::from_rgb(0.106, 0.118, 0.18);
/// A subtle border around cards.
const CARD_BORDER: Color = Color::from_rgb(0.20, 0.22, 0.32);
/// Primary text.
const FG: Color = Color::from_rgb(0.84, 0.86, 0.93);
/// Muted / secondary text (section labels, hints).
const FG_MUTED: Color = Color::from_rgb(0.52, 0.55, 0.66);
/// Accent (the build action, current step, active flows).
const ACCENT: Color = Color::from_rgb(0.48, 0.65, 0.98);
const GOOD: Color = Color::from_rgb(0.45, 0.78, 0.55);
const BAD: Color = Color::from_rgb(0.93, 0.45, 0.50);
/// Amber — the "agent is working on these lines" highlight (pulses while a fix is in flight).
const AMBER: Color = Color::from_rgb(0.95, 0.72, 0.35);

/// Card surface style: filled rounded panel with a thin border.
fn card_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SURFACE)),
        border: Border {
            color: CARD_BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// Primary (accent-filled) button style for the build action.
fn primary_button(_t: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => Color::from_rgb(0.56, 0.72, 1.0),
        _ => ACCENT,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: Color::from_rgb(0.06, 0.07, 0.11),
        border: Border {
            radius: 6.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A borderless, transparent button style for file-tree rows — so the explorer reads
/// as a clickable list, not a wall of buttons. A faint wash on hover gives feedback.
fn tree_button(_t: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => Some(Background::Color(Color { a: 0.06, ..ACCENT })),
        _ => None,
    };
    button::Style {
        background: bg,
        text_color: FG,
        border: Border {
            radius: 4.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The top menu bar's background: a flat strip a touch darker than the cards.
fn menu_bar_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.08, 0.09, 0.14))),
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// A menu-bar title button: transparent, faint wash when its dropdown is open or hovered.
fn menu_title_style(open: bool, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered);
    let bg = if open || hovered {
        Some(Background::Color(Color { a: 0.10, ..ACCENT }))
    } else {
        None
    };
    button::Style {
        background: bg,
        text_color: FG,
        border: Border {
            radius: 4.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A Windows-style menu item button: transparent, full accent-wash highlight on hover
/// (the classic "whole row highlights" behaviour), square corners for a native feel.
fn menu_item_style(_t: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: hovered.then(|| Background::Color(ACCENT)),
        text_color: if hovered {
            Color::from_rgb(0.06, 0.07, 0.11)
        } else {
            FG
        },
        border: Border {
            radius: 3.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A code line container's wash, by state (precedence: selection → change → working):
///  • `selected` → faint accent (you're commenting on it),
///  • `changed` → faint GREEN (differs from HEAD — a git change, PR-style),
///  • `working = Some(alpha)` → pulsing AMBER (the agent is working these lines right now),
///  • else transparent.
fn code_line_container(selected: bool, changed: bool, working: Option<f32>) -> container::Style {
    let bg = if selected {
        Some(Background::Color(Color { a: 0.14, ..ACCENT }))
    } else if changed {
        Some(Background::Color(Color { a: 0.12, ..GOOD }))
    } else {
        working.map(|a| Background::Color(Color { a, ..AMBER }))
    };
    container::Style {
        background: bg,
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// The floating dropdown card: an opaque surface with a border so it reads above the body.
fn dropdown_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.12, 0.13, 0.20))),
        border: Border {
            color: CARD_BORDER,
            width: 1.0,
            radius: 6.0.into(),
        },
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// A section header label (muted, uppercase-ish small caps feel via size).
fn section(label: &str) -> iced::widget::Text<'_> {
    text(label).size(12).color(FG_MUTED)
}

/// One stored inline comment rendered under its line (PR-style): a pending one shows the text
/// + a dismiss ✕; a resolved one shows a ✓ and dimmer text (the running "done" record).
fn view_inline_comment(i: usize, c: dc_win::comments::Comment) -> Element<'static, Message> {
    let (mark, mark_color, txt_color) = if c.resolved {
        ("✓", GOOD, FG_MUTED)
    } else {
        ("💬", ACCENT, FG)
    };
    let resolved = c.resolved;
    let head = row![
        text(mark).size(12).color(mark_color),
        text(c.text.clone()).size(12).color(txt_color),
        Space::new().width(Fill),
        button(text("✕").size(11))
            .on_press(Message::DismissComment(i))
            .padding([0, 6])
            .style(menu_item_style),
    ]
    .spacing(6)
    .align_y(iced::Alignment::Center);
    container(head)
        .width(Fill)
        .padding([4, 8])
        .style(move |_t: &Theme| container::Style {
            background: Some(Background::Color(Color {
                a: 0.06,
                ..if resolved { GOOD } else { ACCENT }
            })),
            border: Border {
                color: CARD_BORDER,
                width: 1.0,
                radius: 4.0.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// Launch the desktop app.
pub fn run() -> iced::Result {
    // iced 0.14: `application(boot, update, view)` where boot returns the initial
    // (State, Task); title/subscription/theme are builder methods. If a project was
    // remembered from last session, greet with its README/roadmap on boot.
    iced::application(
        || {
            let mut app = App::default();
            if app.picked_workspace.is_some() {
                app.show_welcome();
                app.open_conversation();
            }
            (app, Task::none())
        },
        App::update,
        App::view,
    )
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
    /// True while the current/last run is an ITERATE (in-place edit) run, so the outcome
    /// banner reports "N files changed" from what the agent actually edited — never a
    /// whole-repo "files built" scan (which would count thousands in an existing project).
    iterating: bool,
    /// The files the agent actually *edited/wrote* this run (workspace-relative, de-duped),
    /// for the iterate outcome banner. Reset at each run start.
    edited_files: Vec<String>,

    // --- IDE shell state (explorer + code viewer) --------------------------------
    /// Collapsed directories in the explorer (workspace-relative paths). Everything
    /// expanded by default; clicking a dir toggles it here.
    collapsed_dirs: std::collections::HashSet<String>,
    /// The file shown in the code panel (workspace-relative). `None` before any file
    /// is chosen / touched.
    selected_file: Option<String>,
    /// When true, the code panel follows the agent (auto-jumps to the file it's
    /// editing). Clicking a file in the tree pins it (sets this false); it re-arms when
    /// a new run starts. This is the "watch it work" behaviour, escapable on demand.
    follow_agent: bool,
    /// The rendered contents of `selected_file`, recomputed when the selection changes
    /// or the file is edited. Cached so `view()` doesn't hit the disk every frame.
    code: Option<dc_win::CodeView>,
    /// Which top-bar menu is currently open (File / View), if any. `None` = all closed.
    open_menu: Option<Menu>,

    // --- Conversation (plan-first chat) ------------------------------------------
    /// The planning conversation with the model (mode-shaped, plans-as-files). `None`
    /// until a project folder is opened.
    conversation: Option<dc_win::chat::Conversation>,
    /// The chat thread shown in the middle column (you ⟷ agent), in order.
    chat_turns: Vec<dc_win::chat::Turn>,
    /// An in-flight chat turn (a `generate` call on a worker thread), if any.
    chat_session: Option<dc_win::chat_session::ChatSession>,
    /// Plan-file changes the assistant proposed in its latest reply, awaiting Apply.
    proposed_files: Vec<dc_win::chat::ProposedFile>,
    /// Whether the next chat turn should let the model *reason* (slower, deeper) vs. answer
    /// directly (`/no_think`, fast). Off by default — this 8B rambles when left to think, so
    /// fast conclusions are the default and Think is opt-in per the composer toggle.
    think: bool,
    /// Which bottom-panel tab is selected (Activity / Verification / Build).
    bottom_tab: BottomTab,

    // --- Line comments (PR-style) ------------------------------------------------
    /// The committed line range being commented on (1-based, inclusive `(start, end)`), if a
    /// comment box is open. A single-line comment is `(n, n)`.
    comment_range: Option<(usize, usize)>,
    /// The comment text being typed for `comment_range`.
    comment_draft: String,
    /// Drag-select state: `Some((anchor, current))` while the mouse is pressed and dragging
    /// across lines in the code view; `None` when not dragging. On release it becomes the
    /// committed `comment_range`.
    drag: Option<(usize, usize)>,
    /// A line-comment classification in flight (the small/big triage call), if any. Carries
    /// the comment so the result can be routed once the verdict arrives.
    triage: Option<TriageInFlight>,
    /// True when the current iterate run was triggered by a small line-comment fix, so its
    /// outcome (files changed + verify result) is reported back into the chat thread.
    iterate_from_comment: bool,
    /// The in-flight assistant reply as it streams in token-by-token (the live "typing"
    /// bubble). `None` when nothing is streaming; replaced by a finished turn on completion.
    streaming: Option<String>,
    /// Debug mode: when on, every prompt sent to the model is echoed into the chat as a
    /// (dimmed, collapsible) debug turn, so you can see exactly what the agent receives.
    debug: bool,
    /// Lines of the currently-shown file that differ from HEAD right now (from `git diff`),
    /// highlighted GitHub-PR-style. Refreshed as a fix edits the file, so you SEE changes land.
    changed_lines: std::collections::BTreeSet<usize>,
    /// The range the agent is actively working on (the lines you commented on), highlighted in
    /// a pulsing amber from submit until the change lands — so the "thinking" gap feels active.
    /// `(file, start, end)`; `None` when nothing is in flight.
    working: Option<(String, usize, usize)>,

    // --- PR-review state ---------------------------------------------------------
    /// Persisted inline code comments (`.dc/comments.json`), rendered under their lines and
    /// marked resolved when the agent finishes the change.
    comments: dc_win::comments::Comments,
    /// Working-tree file statuses (path → M/A/D) for the PR-style file tree, refreshed as
    /// fixes land.
    file_status: std::collections::BTreeMap<String, dc_win::gitdiff::FileStatus>,
    /// The current git branch (shown in the explorer header), if any.
    branch: Option<String>,
}

/// A line-comment triage running on a worker thread: the classify call + the comment it's
/// deciding, so `pump` can route to a small fix or a planning turn when the verdict lands.
struct TriageInFlight {
    comment: dc_win::linecomment::LineComment,
    session: dc_win::chat_session::ChatSession,
}

/// The bottom panel's tabs — the verify output and the last run's build outcome. Tabbed
/// (not stacked) so they share the bottom space. (Activity was dropped: the chat column
/// now carries "what the agent is doing", so a separate activity log is redundant.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BottomTab {
    Verification,
    Build,
}

/// The top menu-bar dropdowns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Menu {
    File,
    View,
}

impl Default for App {
    fn default() -> Self {
        // Seed the editable input boxes from the config defaults, so the settings
        // panel shows the active values and `start()` never commits a blank input
        // over a sensible default (URL, /no_think suffix, …).
        let cfg = UiConfig::default();
        // Re-open the last project the user worked in (if it still exists on disk), so the
        // app comes back to where they left off instead of the empty scratch base.
        let picked_workspace = dc_win::persist::load().last_project;
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
            picked_workspace,
            iterating: false,
            edited_files: Vec::new(),
            collapsed_dirs: std::collections::HashSet::new(),
            selected_file: None,
            follow_agent: true,
            code: None,
            open_menu: None,
            conversation: None,
            chat_turns: Vec::new(),
            chat_session: None,
            proposed_files: Vec::new(),
            think: false,
            bottom_tab: BottomTab::Verification,
            comment_range: None,
            comment_draft: String::new(),
            drag: None,
            triage: None,
            iterate_from_comment: false,
            streaming: None,
            debug: false,
            changed_lines: std::collections::BTreeSet::new(),
            working: None,
            comments: dc_win::comments::Comments::default(),
            file_status: std::collections::BTreeMap::new(),
            branch: None,
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
    /// Run the in-place iterate loop (the default when a project folder is picked).
    RunIterate,
    Tick,
    // Explorer / code-viewer interaction.
    /// Select a file in the tree → show it in the code panel (and pin, stop following).
    SelectFile(String),
    /// Toggle a directory's collapsed state in the explorer.
    ToggleDir(String),
    // Top menu bar.
    /// Open (or toggle) a top-bar dropdown menu.
    ToggleMenu(Menu),
    // Conversation.
    /// Send the composer text as a chat message to the planning agent.
    ChatSend,
    /// Apply the Nth proposed plan-file to disk (writes README.md / TODO.md).
    ApplyFile(usize),
    /// Toggle "think" mode for the next chat turn (reason vs. answer directly).
    ToggleThink(bool),
    /// Toggle debug mode: echo every prompt sent to the model into the chat.
    ToggleDebug(bool),
    /// Undo the last fix: git-revert exactly the files it changed, back to committed state.
    UndoLastChange,
    /// Dismiss the stored inline comment at this index (remove it from the review list).
    DismissComment(usize),
    /// Cancel the in-flight run/fix (stops the agent at its next turn; reverts partial edits).
    CancelRun,
    /// Select a bottom-panel tab (Activity / Verification / Build).
    SelectBottomTab(BottomTab),
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
        if self.session.is_some()
            || self.chat_session.is_some()
            || self.triage.is_some()
            || self.working.is_some()
            || !self.gatebar.is_empty()
            || glowing
        {
            iced::time::every(Duration::from_millis(50)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        }
    }

    /// The run action for the primary button / intent submit, chosen by context: a
    /// picked project folder means "iterate in place"; otherwise the from-scratch build.
    fn run_message(&self) -> Message {
        if self.picked_workspace.is_some() {
            Message::RunIterate
        } else {
            Message::RunTdd
        }
    }

    /// The primary button label matching [`Self::run_message`].
    fn run_label(&self) -> &'static str {
        if self.picked_workspace.is_some() {
            "⚒  iterate"
        } else {
            "⚒  build"
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
        // Re-arm follow so the code panel tracks the agent through this run.
        self.follow_agent = true;
        // Track this run's mode + which files it edits (for the honest iterate banner).
        self.iterating = matches!(kind, RunKind::Iterate);
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
    fn now(&self) -> f32 {
        self.run_started
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0)
    }

    /// The workspace root the explorer/code panels read from: the picked project folder
    /// if any, else the current run's output dir, else the config base. This is the tree
    /// the user is actually working in.
    fn workspace_root(&self) -> std::path::PathBuf {
        self.picked_workspace
            .clone()
            .or_else(|| self.run_dir.clone())
            .unwrap_or_else(|| self.cfg.workspace.clone())
    }

    /// On opening a project, greet the user in the Activity stream: the project name, its
    /// README's TODO/roadmap excerpt (highlighted), and an invitation to say what to work
    /// on. No-op for a folder with no README (still greets, just no excerpt).
    fn show_welcome(&mut self) {
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
        let w = dc_win::welcome::build(&readme, &todo_md, &folder);

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

    /// Open a planning conversation for the current project: read the plan files, pick the
    /// mode (scratch vs existing), and seed the thread with the agent's opening line.
    fn open_conversation(&mut self) {
        let root = self.workspace_root();
        // Load persisted inline comments, ensure .dc/ is git-ignored, and pull the initial
        // git view (branch + file statuses) for the PR-style tree.
        self.comments = dc_win::comments::load(&root);
        dc_win::comments::ensure_gitignored(&root);
        self.refresh_git_view();
        let (readme, todo) = self.read_plan_files(&root);
        let convo = dc_win::chat::Conversation::open(&readme, &todo);
        self.chat_turns.clear();
        self.chat_turns.push(dc_win::chat::Turn {
            role: dc_win::chat::Speaker::Agent,
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
    fn read_plan_files(&self, root: &std::path::Path) -> (String, String) {
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
    fn plan_file_to_show(&self, root: &std::path::Path) -> Option<String> {
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
    fn send_chat(&mut self) {
        let text = self.intent.trim().to_string();
        if text.is_empty() || self.chat_session.is_some() || self.conversation.is_none() {
            return;
        }
        // Commit connection settings (mirrors `start`), so a chat uses the current backend.
        self.cfg.model = self.model_input.clone();
        self.cfg.base_url = self.url_input.clone();
        let think = self.think;

        // Update the conversation + build the request in one scoped mutable borrow.
        let req = {
            let convo = self.conversation.as_mut().expect("checked above");
            convo.user_turn(&text);
            convo.request(think)
        };
        self.chat_turns.push(dc_win::chat::Turn {
            role: dc_win::chat::Speaker::You,
            text,
        });
        self.proposed_files.clear();
        self.intent.clear();
        self.spawn_chat("chat", req);
    }

    /// Spawn a chat/generate call, first echoing its prompt into the chat when debug mode is
    /// on. Every model call that streams into the chat goes through here.
    fn spawn_chat(&mut self, label: &str, req: dc_model::GenerateRequest) {
        if self.debug {
            let joined = req
                .messages
                .iter()
                .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            self.debug_prompt(label, &joined);
        }
        self.chat_session = Some(dc_win::chat_session::ChatSession::spawn(
            self.cfg.clone(),
            req,
        ));
    }

    /// Submit the open line comment: capture the line + text, spawn the small/big triage
    /// classification (fast `/no_think`), and close the comment box. Routing happens in
    /// [`Self::pump_triage`] when the verdict arrives.
    fn submit_line_comment(&mut self) {
        let (Some((start, end)), Some(cv)) = (self.comment_range, self.code.as_ref()) else {
            return;
        };
        let comment = self.comment_draft.trim().to_string();
        if comment.is_empty() {
            return;
        }
        // The IDE does the scoping: hand the model the selected code + a small window, so it
        // doesn't slurp the whole file (what made a one-line fix slow + context-heavy).
        let (selection, context) = dc_win::linecomment::scope_context(&cv.lines, start, end);
        let lc = dc_win::linecomment::LineComment {
            file: cv.rel.clone(),
            start,
            end,
            selection,
            context,
            comment,
        };
        // Persist the comment inline (pending) — it stays visible in the code view and gets
        // marked resolved when the agent finishes. A Question is removed again in pump_triage
        // (it's answered, not a change to track).
        self.comments.add(dc_win::comments::Comment::new(
            lc.file.clone(),
            start,
            end,
            lc.comment.clone(),
        ));
        dc_win::comments::save(&self.workspace_root(), &self.comments);
        // Commit connection settings so the triage/edit use the current backend.
        self.cfg.model = self.model_input.clone();
        self.cfg.base_url = self.url_input.clone();
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
        self.chat_turns.push(dc_win::chat::Turn {
            role: dc_win::chat::Speaker::You,
            text: echo,
        });
        let req = lc.classify_request();
        if self.debug {
            let joined = req
                .messages
                .iter()
                .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            self.debug_prompt("triage", &joined);
        }
        let session = dc_win::chat_session::ChatSession::spawn(self.cfg.clone(), req);
        self.triage = Some(TriageInFlight {
            comment: lc,
            session,
        });
        self.comment_range = None;
        self.comment_draft.clear();
    }

    /// Drain the line-comment triage call. On a verdict: SMALL → run a scoped-but-coherent
    /// iterate fix now; BIG → seed a planning chat turn for approval.
    fn pump_triage(&mut self) {
        let Some(t) = &self.triage else {
            return;
        };
        let events = t.session.drain();
        for ev in events {
            let reply = match ev {
                // Triage is a one-word classify — ignore streamed tokens, act on the final.
                dc_win::chat_session::ChatEvent::Token(_) => continue,
                dc_win::chat_session::ChatEvent::Reply(r) => r,
                dc_win::chat_session::ChatEvent::Failed(_) => {
                    // On a failed triage, fall back to planning (the safe route).
                    "BIG".to_string()
                }
            };
            let verdict = dc_win::linecomment::parse_verdict(&reply);
            let comment = self.triage.take().expect("in flight").comment;
            match verdict {
                dc_win::linecomment::Verdict::Question => {
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
                        dc_win::comments::save(&self.workspace_root(), &self.comments);
                    }
                    self.cfg.model = self.model_input.clone();
                    self.cfg.base_url = self.url_input.clone();
                    let req = comment.question_request(self.think);
                    self.spawn_chat("question", req);
                }
                dc_win::linecomment::Verdict::Small => {
                    self.chat_turns.push(dc_win::chat::Turn {
                        role: dc_win::chat::Speaker::Agent,
                        text: "→ quick fix — making the change and checking it compiles…"
                            .to_string(),
                    });
                    self.start_iterate_with(comment.small_fix_instruction());
                }
                dc_win::linecomment::Verdict::Big => {
                    // Route into planning: seed a user turn and send it to the chat agent.
                    let seed = comment.planning_seed();
                    self.chat_turns.push(dc_win::chat::Turn {
                        role: dc_win::chat::Speaker::Agent,
                        text: "→ this needs a plan — let's talk it through first.".to_string(),
                    });
                    self.intent = seed;
                    self.send_chat();
                }
            }
            break;
        }
    }

    /// Start an iterate run from a ready-made instruction (used by the small-fix line-comment
    /// path). Mirrors `start(RunKind::Iterate)` but with an explicit instruction instead of
    /// the composer text.
    fn start_iterate_with(&mut self, instruction: String) {
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

    /// Write the Nth proposed plan-file to disk (README.md / TODO.md), then refresh the
    /// conversation's view of the plan files and re-open it in the code view.
    fn apply_proposed_file(&mut self, i: usize) {
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
            // Drop it from the pending list (applied).
            self.proposed_files.remove(i);
            // Confirm the write in the chat thread, so applying is visible in the record.
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: format!("✓ Applied {} to disk.", pf.name),
            });
        } else {
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: format!("⚠ Could not write {}.", pf.name),
            });
        }
    }

    /// Select `rel` for the code panel and load its contents from the workspace root.
    fn select_file(&mut self, rel: String) {
        let root = self.workspace_root();
        self.code = Some(dc_win::codeview::load(&root, &rel));
        self.selected_file = Some(rel);
        self.refresh_changed_lines();
    }

    /// Re-read the currently selected file from disk (after the agent edited it), so the
    /// code panel reflects the latest bytes — and refresh which lines differ from HEAD.
    fn reload_selected(&mut self) {
        if let Some(rel) = self.selected_file.clone() {
            let root = self.workspace_root();
            self.code = Some(dc_win::codeview::load(&root, &rel));
        }
        self.refresh_changed_lines();
    }

    /// Recompute which lines of the shown file differ from HEAD (git), for the PR-style
    /// highlight. Cheap `git diff -U0` on the one file; empty when nothing's selected.
    fn refresh_changed_lines(&mut self) {
        self.changed_lines = match &self.selected_file {
            Some(rel) => dc_win::gitdiff::file_changed_lines(&self.workspace_root(), rel),
            None => std::collections::BTreeSet::new(),
        };
    }

    /// Refresh the PR-view git state: the current branch and per-file M/A/D statuses. Called on
    /// project open and after a fix lands, so the file tree tracks working-tree changes.
    fn refresh_git_view(&mut self) {
        let root = self.workspace_root();
        self.file_status = dc_win::gitdiff::statuses(&root);
        self.branch = dc_win::gitdiff::current_branch(&root);
    }

    /// Whether a swarm run is (or was) active — i.e. the topology has nodes to draw.
    fn is_swarm(&self) -> bool {
        !self.topology.is_empty()
    }

    /// Compute the outcome banner when a run ends. Two shapes:
    ///  • ITERATE (editing an existing project) → report the files the agent actually
    ///    *changed* — never a whole-repo scan (which would count thousands) and no "open
    ///    output folder" (you're already in your own repo).
    ///  • FROM-SCRATCH build → the "N files built" summary + open-folder, as before.
    fn finish_run(&mut self, ok: bool, summary: &str) {
        // Surface the outcome: jump to the Build tab when a run ends.
        self.bottom_tab = BottomTab::Build;
        // The agent's done working the selection — drop the amber "working" highlight (the
        // green git-change highlight takes over).
        self.working = None;
        if self.iterating {
            self.finish_iterate(ok, summary);
            return;
        }
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

    /// Undo the last fix: `git checkout --` exactly the files it changed, restoring them to
    /// their committed state. A deliberate "I don't like this change" action — the safe way to
    /// reject a fix, instead of hand-reverting. No-op if there's no finished result with files.
    fn undo_last_change(&mut self) {
        let files: Vec<String> = self
            .result
            .as_ref()
            .map(|r| r.files.clone())
            .unwrap_or_default();
        if files.is_empty() {
            return;
        }
        let root = self.workspace_root();
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .arg("checkout")
            .arg("--")
            .args(&files)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        self.chat_turns.push(dc_win::chat::Turn {
            role: dc_win::chat::Speaker::Agent,
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
    fn finish_iterate(&mut self, ok: bool, summary: &str) {
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
                dc_win::comments::save(&self.workspace_root(), &self.comments);
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
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text,
            });
        }
        self.result = Some(RunResult {
            ok,
            headline,
            reason,
            files,
            dir: None, // no "open output folder" — you're iterating in your own repo
        });
    }

    /// Drain the in-flight chat turn, if any. On a reply: split prose from proposed
    /// plan-files, record the assistant turn, show the prose in the thread, stage the
    /// proposed files for Apply, and auto-open the first one in the code view.
    fn pump_chat(&mut self) {
        let Some(cs) = &self.chat_session else {
            return;
        };
        let events = cs.drain();
        for ev in events {
            match ev {
                dc_win::chat_session::ChatEvent::Token(delta) => {
                    // Grow the live "typing" bubble. Strip <think> for display as it streams
                    // (a reasoning delta shouldn't flash into the visible reply).
                    let buf = self.streaming.get_or_insert_with(String::new);
                    buf.push_str(&delta);
                }
                dc_win::chat_session::ChatEvent::Reply(raw) => {
                    self.streaming = None; // the live bubble is replaced by the finished turn
                    self.working = None; // a question answer is done → drop the amber highlight
                    let (prose, files) = dc_win::chat::parse_reply(&raw);
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
                    let shown = if prose.is_empty() {
                        // A reply that was only file blocks → note what changed.
                        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
                        format!("Proposed changes to {}.", names.join(", "))
                    } else {
                        prose
                    };
                    self.chat_turns.push(dc_win::chat::Turn {
                        role: dc_win::chat::Speaker::Agent,
                        text: shown,
                    });
                    self.proposed_files = files;
                    // Auto-open the first proposed file so you see the plan taking shape.
                    if let Some(first) = self.proposed_files.first() {
                        let name = first.name.clone();
                        let content = first.content.clone();
                        // Show the PROPOSED content directly (not the on-disk file, which
                        // hasn't been written yet).
                        self.follow_agent = false;
                        self.selected_file = Some(name.clone());
                        self.code = Some(dc_win::codeview::from_text(&name, &content));
                    }
                    self.chat_session = None;
                }
                dc_win::chat_session::ChatEvent::Failed(msg) => {
                    self.streaming = None;
                    self.working = None;
                    self.chat_turns.push(dc_win::chat::Turn {
                        role: dc_win::chat::Speaker::Agent,
                        text: format!("⚠ {msg}"),
                    });
                    self.chat_session = None;
                }
            }
        }
    }

    /// Drain the worker channels into UI state. Called each tick.
    fn pump(&mut self) {
        self.pump_chat();
        self.pump_triage();
        // While a fix run is in flight, keep the code view + change-highlight fresh from disk
        // so edits land live (the agent edits the real files). Cheap: reload the one shown
        // file + one `git diff` on it per tick. Done before borrowing `session` below.
        if self.iterate_from_comment && self.session.is_some() {
            self.reload_selected();
        }
        let Some(session) = &self.session else {
            return;
        };
        for ev in session.drain_events() {
            match ev {
                UiEvent::Agent(e) => {
                    // Live "watch it type": as the model streams a write/edit, preview the
                    // growing file content in the code view, word by word, before it lands.
                    if let dc_core::AgentEvent::ContentDelta { cumulative, .. } = &e {
                        if let Some(p) = dc_win::codeview::partial_edit_preview(cumulative) {
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
                                self.code = Some(dc_win::codeview::from_text(&name, &p.content));
                            }
                        }
                        continue; // a delta is preview-only; nothing else to fold
                    }
                    if let dc_core::AgentEvent::Planned { steps }
                    | dc_core::AgentEvent::PlanRevised { steps } = &e
                    {
                        self.board = steps.clone();
                    }
                    if let dc_core::AgentEvent::Verification { summary, .. } = &e {
                        self.verify_text = Some(summary.clone());
                    }
                    // Record files the agent actually edited/wrote (for the iterate banner).
                    if dc_win::codeview::is_mutating_touch(&e) {
                        if let Some(rel) = dc_win::codeview::file_touched_by(&e) {
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
                            self.chat_turns.push(dc_win::chat::Turn {
                                role: dc_win::chat::Speaker::Agent,
                                text: line,
                            });
                        }
                    }
                    // Follow the agent: when it touches a file and we're in follow mode,
                    // show that file in the code panel — so edits land in front of you.
                    if self.follow_agent {
                        if let Some(rel) = dc_win::codeview::file_touched_by(&e) {
                            self.select_file(rel);
                        }
                    }
                    // A tool result means a file may have just changed on disk; refresh the
                    // shown file so an edit/write to it is reflected (the ToolCall fired
                    // before the bytes landed). Cheap: only when something is selected.
                    if matches!(
                        e,
                        dc_core::AgentEvent::ToolResult {
                            is_error: false,
                            ..
                        }
                    ) {
                        self.reload_selected();
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
            Message::ToggleSettings => {
                self.open_menu = None;
                self.settings_open = !self.settings_open;
            }
            Message::ToggleYolo(v) => self.cfg.yolo = v,
            Message::ToggleDryRun(v) => self.cfg.dry_run = v,
            Message::RunTdd => self.start(RunKind::Tdd),
            Message::RunIterate => self.start(RunKind::Iterate),
            Message::Tick => self.pump(),
            Message::SelectFile(rel) => {
                // Click-to-pin: show this file and stop auto-following the agent until
                // the next run re-arms follow.
                self.follow_agent = false;
                self.select_file(rel);
            }
            Message::ToggleDir(rel) => {
                if !self.collapsed_dirs.remove(&rel) {
                    self.collapsed_dirs.insert(rel);
                }
            }
            Message::ToggleMenu(m) => {
                self.open_menu = if self.open_menu == Some(m) {
                    None
                } else {
                    Some(m)
                };
            }
            Message::ChatSend => self.send_chat(),
            Message::ApplyFile(i) => self.apply_proposed_file(i),
            Message::ToggleThink(v) => self.think = v,
            Message::ToggleDebug(v) => self.debug = v,
            Message::UndoLastChange => self.undo_last_change(),
            Message::DismissComment(i) => {
                self.comments.remove(i);
                dc_win::comments::save(&self.workspace_root(), &self.comments);
            }
            Message::CancelRun => {
                if let Some(s) = &self.session {
                    s.cancel();
                    self.chat_turns.push(dc_win::chat::Turn {
                        role: dc_win::chat::Speaker::Agent,
                        text: "⏹ cancelling — stopping at the next step…".to_string(),
                    });
                }
            }
            Message::SelectBottomTab(t) => self.bottom_tab = t,
            Message::LineDragStart(n) => {
                // Begin a drag-selection anchored at line n. Clear any open comment box.
                self.drag = Some((n, n));
                self.comment_range = None;
            }
            Message::LineDragTo(n) => {
                // Extend the drag to line n (only while a drag is active).
                if let Some((anchor, _)) = self.drag {
                    self.drag = Some((anchor, n));
                }
            }
            Message::LineDragEnd => {
                // Commit the drag into a comment range (normalized so start ≤ end) and open
                // the comment box. A no-drag (just a click) yields a single-line range.
                if let Some((a, b)) = self.drag.take() {
                    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                    self.comment_range = Some((lo, hi));
                    self.comment_draft.clear();
                }
            }
            Message::CommentDraftChanged(s) => self.comment_draft = s,
            Message::CommentSubmit => self.submit_line_comment(),
            Message::CommentCancel => {
                self.comment_range = None;
                self.drag = None;
                self.comment_draft.clear();
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
                self.open_menu = None;
                // Native folder dialog (blocking — fine for a button click). When a
                // folder is chosen, runs go there so follow-up prompts iterate on it.
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title("Choose a project folder to work in")
                    .pick_folder()
                {
                    self.picked_workspace = Some(dir.clone());
                    // A fresh project → drop any stale selection/collapse from the last one.
                    self.selected_file = None;
                    self.code = None;
                    self.collapsed_dirs.clear();
                    // Remember it for next launch.
                    dc_win::persist::save(&dc_win::persist::UiState {
                        last_project: Some(dir),
                    });
                    // Greet: show the README/roadmap in Activity, and open the planning
                    // conversation (mode-detected) in the chat column.
                    self.show_welcome();
                    self.open_conversation();
                }
            }
            Message::ClearWorkspace => {
                self.open_menu = None;
                self.picked_workspace = None;
                self.selected_file = None;
                self.code = None;
                // Forget the remembered project so a restart doesn't re-open it.
                dc_win::persist::save(&dc_win::persist::UiState { last_project: None });
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
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        // The IDE body: three columns — EXPLORER (file tree) · CENTER (activity stream +
        // the intent composer beneath it) · CODE (the file being edited). VS-Code-style.
        let center: Element<'_, Message> = if self.plan.started() && self.is_swarm() {
            // A swarm build in flight: the plan panel + live topology are the story.
            row![self.view_plan(), self.view_topology()]
                .spacing(12)
                .into()
        } else if self.plan.started() {
            // A staged build (single agent): plan panel beside the activity stream.
            row![self.view_plan(), self.view_center()]
                .spacing(12)
                .into()
        } else {
            // Iterate / idle: the activity stream + composer is the center column.
            self.view_center()
        };

        let body = row![
            self.view_explorer(),
            container(center).width(Length::FillPortion(4)).height(Fill),
            self.view_code(),
        ]
        .spacing(12)
        .height(Fill);

        let gate = self.view_gatebar();

        // The body content below the (flush, full-width) menu bar — this part is padded.
        // The run outcome now lives in the BUILD panel of the bottom strip (not a top
        // banner), so it no longer shoves the three columns down.
        let mut body_col = column![].spacing(10);
        if self.plan.started() {
            body_col = body_col.push(self.view_step_flow());
        }
        body_col = body_col.push(body);
        if let Some(strip) = self.view_bottom_strip() {
            body_col = body_col.push(strip);
        }
        if let Some(g) = gate {
            body_col = body_col.push(g);
        }

        // Base layer: the menu bar flush at the very top (no padding around it, full width),
        // then the padded body beneath it.
        let base = column![
            self.view_menu_bar(),
            container(body_col).width(Fill).height(Fill).padding(10),
        ]
        .width(Fill)
        .height(Fill);

        // Overlays float ABOVE the base (a Stack), so an open dropdown or the settings modal
        // never shifts the layout. Only one shows at a time.
        let mut layers = iced::widget::stack![base];
        if let Some(dd) = self.view_menu_dropdown() {
            layers = layers.push(dd);
        }
        if self.settings_open {
            layers = layers.push(self.view_settings_modal());
        }
        layers.width(Fill).height(Fill).into()
    }

    /// The left EXPLORER column: the workspace file tree, dirs-first, click a file to
    /// pin it in the code panel, click a dir to collapse/expand. Empty-state hint before
    /// a project folder is picked.
    fn view_explorer(&self) -> Element<'_, Message> {
        use dc_win::gitdiff::FileStatus;
        let root = self.workspace_root();
        let rows = dc_win::filetree::build_rows(&root, &self.collapsed_dirs);

        // GitHub-PR-style header: the current branch and what it's compared against (HEAD/the
        // working tree). Plus a count of changed files.
        let n_changed = self.file_status.len();
        let branch_line = match &self.branch {
            Some(b) => format!("⎇ {b}  ·  {n_changed} changed vs HEAD"),
            None => "not a git repo".to_string(),
        };
        let mut col = column![
            section("EXPLORER"),
            text(branch_line)
                .size(11)
                .color(iced::Color::from_rgb(0.55, 0.58, 0.70)),
        ]
        .spacing(2);
        if rows.is_empty() {
            col = col.push(
                text("File ▸ Open folder to work in")
                    .size(11)
                    .color(FG_MUTED),
            );
        }
        for r in rows.iter().take(600) {
            let indent = 8.0 + (r.depth as f32) * 12.0;
            let is_selected = !r.is_dir && self.selected_file.as_deref() == Some(r.rel.as_str());
            let glyph = if r.is_dir {
                if self.collapsed_dirs.contains(&r.rel) {
                    "▸"
                } else {
                    "▾"
                }
            } else {
                " "
            };
            // PR-style file status badge (M/A/D) + colouring for changed files.
            let status = (!r.is_dir).then(|| self.file_status.get(&r.rel)).flatten();
            let (badge, badge_color) = match status {
                Some(FileStatus::Added) => ("A", GOOD),
                Some(FileStatus::Modified) => ("M", AMBER),
                Some(FileStatus::Deleted) => ("D", BAD),
                None => (" ", FG_MUTED),
            };
            let name_color = if is_selected {
                ACCENT
            } else if let Some(s) = status {
                match s {
                    FileStatus::Added => GOOD,
                    FileStatus::Modified => AMBER,
                    FileStatus::Deleted => BAD,
                }
            } else if r.is_dir {
                FG
            } else {
                FG_MUTED
            };
            let msg = if r.is_dir {
                Message::ToggleDir(r.rel.clone())
            } else {
                Message::SelectFile(r.rel.clone())
            };
            let btn = button(
                row![
                    text(badge.to_string())
                        .size(11)
                        .font(iced::Font::MONOSPACE)
                        .color(badge_color),
                    text(format!("{glyph} {}", r.name))
                        .size(12)
                        .color(name_color),
                ]
                .spacing(4),
            )
            .on_press(msg)
            .padding([1, 4])
            .style(tree_button)
            .width(Fill);
            col = col.push(row![Space::new().width(Length::Fixed(indent)), btn]);
        }

        container(scrollable(col).height(Fill))
            .width(Length::FillPortion(2))
            .height(Fill)
            .padding(10)
            .style(card_style)
            .into()
    }

    /// The line range currently highlighted in the code view: the active drag (normalized so
    /// lo ≤ hi) if dragging, else the committed comment range. `None` when neither.
    fn selected_line_range(&self) -> Option<(usize, usize)> {
        if let Some((a, b)) = self.drag {
            Some(if a <= b { (a, b) } else { (b, a) })
        } else {
            self.comment_range
        }
    }

    /// The right CODE column: the selected/followed file with line numbers, read-only,
    /// rendered like a VS Code editor. The gutter (line numbers) and the code are TWO
    /// side-by-side columns; wrapping is disabled so a long line scrolls horizontally
    /// instead of wrapping into — and interrupting — the number gutter. One vertical
    /// scroll (whole editor) + one horizontal scroll (the code column) is what you get.
    fn view_code(&self) -> Element<'_, Message> {
        let header = match (&self.selected_file, self.follow_agent) {
            (Some(rel), true) => format!("CODE  ·  {rel}  ·  following"),
            (Some(rel), false) => format!("CODE  ·  {rel}  ·  pinned"),
            (None, _) => "CODE".to_string(),
        };

        let inner: Element<'_, Message> = match &self.code {
            Some(cv) if cv.note.is_some() => text(cv.note.clone().unwrap_or_default())
                .size(12)
                .color(FG_MUTED)
                .into(),
            Some(cv) => {
                // Per-line rows you can DRAG across to select a range, then comment (PR-style).
                // Each line is a mouse_area (press=start drag, enter=extend, release=commit)
                // wrapping ONE no-wrap monospace string; selected lines get an accent wash.
                // The comment box renders after the last line of the committed range.
                let sel = self.selected_line_range(); // active drag OR committed range
                                                      // The amber "working" range applies only when the shown file is the one being
                                                      // worked on. A pulsing alpha (sine on the animation clock) reads as "in progress".
                let working_here = self
                    .working
                    .as_ref()
                    .filter(|(f, _, _)| Some(f.as_str()) == self.selected_file.as_deref())
                    .map(|(_, lo, hi)| (*lo, *hi));
                let pulse = 0.10 + 0.10 * (0.5 + 0.5 * (self.now() * 3.0).sin());
                let mut col = column![].spacing(0);
                for (n, line) in &cv.lines {
                    let in_sel = sel.is_some_and(|(lo, hi)| *n >= lo && *n <= hi);
                    let working = working_here.is_some_and(|(lo, hi)| *n >= lo && *n <= hi);
                    // A line that differs from HEAD (git) → GitHub-PR-style green highlight
                    // with a `+` gutter marker, so you SEE what the agent changed, live.
                    let changed = self.changed_lines.contains(n);
                    let mark = if changed { "+" } else { " " };
                    let row_text = format!("{mark}{n:>4}  {line}");
                    let color = if in_sel {
                        ACCENT
                    } else if changed {
                        GOOD
                    } else if working {
                        AMBER
                    } else {
                        FG
                    };
                    let line_el = container(
                        text(row_text)
                            .size(13)
                            .font(iced::Font::MONOSPACE)
                            .color(color)
                            .wrapping(iced::widget::text::Wrapping::None),
                    )
                    .padding([0, 4])
                    .style(move |_t: &Theme| {
                        code_line_container(in_sel, changed, working.then_some(pulse))
                    });
                    col = col.push(
                        iced::widget::mouse_area(line_el)
                            .on_press(Message::LineDragStart(*n))
                            .on_enter(Message::LineDragTo(*n))
                            .on_release(Message::LineDragEnd),
                    );
                    // Stored inline comments whose range ENDS on this line — render them (PR
                    // style), struck-through + ✓ once resolved.
                    if let Some(file) = self.selected_file.clone() {
                        let here: Vec<(usize, dc_win::comments::Comment)> = self
                            .comments
                            .on_file(&file)
                            .filter(|(_, c)| c.end == *n)
                            .map(|(i, c)| (i, c.clone()))
                            .collect();
                        for (i, c) in here {
                            col = col.push(view_inline_comment(i, c));
                        }
                    }
                    // The (new) comment box after the last line of the committed range.
                    if self.comment_range.is_some_and(|(_, hi)| hi == *n) {
                        col = col.push(self.view_comment_box());
                    }
                }
                if cv.truncated {
                    col = col.push(
                        text(format!(
                            "… truncated at {} lines",
                            dc_win::codeview::MAX_LINES
                        ))
                        .size(11)
                        .color(FG_MUTED),
                    );
                }
                // One scrollable, BOTH axes: vertical for the file, horizontal for long lines.
                scrollable(col)
                    .direction(scrollable::Direction::Both {
                        vertical: scrollable::Scrollbar::new(),
                        horizontal: scrollable::Scrollbar::new().width(6).scroller_width(6),
                    })
                    .height(Fill)
                    .width(Fill)
                    .into()
            }
            None => text("the file the agent edits appears here — or click one in the tree")
                .size(12)
                .color(FG_MUTED)
                .into(),
        };

        // Header stays fixed; the code area is the single scrollable (no outer scroll wrap,
        // which is what previously collapsed the inner one).
        let body = column![text(header).size(12).color(FG_MUTED), inner].spacing(6);
        container(body)
            .width(Length::FillPortion(4))
            .height(Fill)
            .padding(10)
            .style(card_style)
            .into()
    }

    /// The inline comment box shown under the selected range (PR-style): a text input +
    /// Comment / Cancel. Submitting triages small (fix now) vs. big (plan first).
    fn view_comment_box(&self) -> Element<'_, Message> {
        let placeholder = match self.comment_range {
            Some((lo, hi)) if lo != hi => format!("comment on lines {lo}–{hi}…"),
            _ => "comment on this line…".to_string(),
        };
        let input = text_input(&placeholder, &self.comment_draft)
            .on_input(Message::CommentDraftChanged)
            .on_submit(Message::CommentSubmit)
            .padding(8)
            .width(Fill);
        let submit = button(text("Comment").size(13))
            .on_press(Message::CommentSubmit)
            .padding([4, 12])
            .style(primary_button);
        let cancel = button(text("Cancel").size(13))
            .on_press(Message::CommentCancel)
            .padding([4, 12])
            .style(menu_item_style);
        let hint = text("small fix → done inline · bigger → we'll plan it")
            .size(10)
            .color(FG_MUTED);
        container(column![input, row![submit, cancel].spacing(6), hint].spacing(6))
            .width(Fill)
            .padding(8)
            .style(dropdown_style)
            .into()
    }

    /// The bottom panel — a tabbed strip of **Verification** (verify output) and **Build**
    /// (last run outcome). It auto-hides when there's nothing to show — no run in flight, no
    /// verify output, no build result — so planning gets the full height. During a swarm
    /// build the coder prompt/output panel takes the whole strip.
    fn view_bottom_strip(&self) -> Option<Element<'_, Message>> {
        if self.is_swarm() {
            return Some(self.view_coder_io());
        }
        // Nothing to build/verify → hide the strip entirely.
        let has_content =
            self.session.is_some() || self.verify_text.is_some() || self.result.is_some();
        if !has_content {
            return None;
        }
        let tabs = row![
            self.bottom_tab_button("Verification", BottomTab::Verification),
            self.bottom_tab_button("Build", BottomTab::Build),
        ]
        .spacing(4);
        let content = match self.bottom_tab {
            BottomTab::Verification => self.view_verification_tab(),
            BottomTab::Build => self.view_build_tab(),
        };
        Some(
            container(column![tabs, content].spacing(6))
                .width(Fill)
                .height(Length::Fixed(180.0))
                .padding(10)
                .style(card_style)
                .into(),
        )
    }

    /// One bottom-tab button; highlighted when it's the selected tab.
    fn bottom_tab_button(&self, label: &str, tab: BottomTab) -> Element<'_, Message> {
        let selected = self.bottom_tab == tab;
        button(
            text(label.to_string())
                .size(12)
                .color(if selected { ACCENT } else { FG_MUTED }),
        )
        .on_press(Message::SelectBottomTab(tab))
        .padding([3, 10])
        .style(move |_t, status| menu_title_style(selected, status))
        .into()
    }

    /// The Verification tab: the verify command's captured output (failure-first).
    fn view_verification_tab(&self) -> Element<'_, Message> {
        let inner: Element<'_, Message> = match &self.verify_text {
            Some(v) => text(v.clone()).size(12).into(),
            None => text("cargo check / test output shows here after the agent verifies")
                .size(12)
                .color(FG_MUTED)
                .into(),
        };
        scrollable(inner).height(Fill).into()
    }

    /// The Build tab: the last run's outcome — ✓/✗ headline, reason, changed/built files,
    /// and (from-scratch only) an open-folder button.
    fn view_build_tab(&self) -> Element<'_, Message> {
        let Some(r) = self.result.as_ref() else {
            return text("no build yet — describe a change and run it")
                .size(12)
                .color(FG_MUTED)
                .into();
        };
        let (mark, color) = if r.ok { ("✓", GOOD) } else { ("✗", BAD) };
        let mut col = column![text(format!("{mark}  {}", r.headline))
            .size(15)
            .color(color)]
        .spacing(4);
        if !r.reason.is_empty() {
            col = col.push(text(&r.reason).size(12).color(FG_MUTED));
        }
        let label = if self.iterating { "changed" } else { "built" };
        if !r.files.is_empty() {
            col = col.push(text(format!("files {label}:")).size(11).color(FG_MUTED));
        }
        for f in r.files.iter().take(10) {
            col = col.push(text(format!("  • {f}")).size(12));
        }
        if r.files.len() > 10 {
            col = col.push(
                text(format!("  … and {} more", r.files.len() - 10))
                    .size(12)
                    .color(FG_MUTED),
            );
        }
        if r.dir.is_some() {
            col = col.push(Space::new().height(Length::Fixed(4.0)));
            col = col.push(
                button(text("📂 open output folder"))
                    .on_press(Message::OpenOutputFolder)
                    .style(menu_item_style),
            );
        }
        // "I don't like this change" — undo an in-place fix's edits (git-revert its files).
        // Only for iterate runs that changed files (from-scratch builds have no committed base).
        if self.iterating && !r.files.is_empty() {
            col = col.push(Space::new().height(Length::Fixed(6.0)));
            col = col.push(
                button(text("↩ Undo this change").size(13))
                    .on_press(Message::UndoLastChange)
                    .padding([4, 12])
                    .style(|_t: &Theme, status| {
                        let hov = matches!(status, button::Status::Hovered);
                        button::Style {
                            background: Some(Background::Color(Color {
                                a: if hov { 0.22 } else { 0.14 },
                                ..BAD
                            })),
                            text_color: FG,
                            border: Border {
                                radius: 5.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        }
                    }),
            );
        }
        scrollable(col).height(Fill).into()
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
        .padding(10)
        .style(card_style)
        .into()
    }

    /// The top menu bar: the app title, the dropdown menu buttons (File / View), and —
    /// pushed to the right — the workspace/status line (what folder we're working in, and
    /// how). Clicking a title toggles its dropdown; items live in [`Self::view_menu_dropdown`].
    fn view_menu_bar(&self) -> Element<'_, Message> {
        let title = text("dumb-coder").size(14).color(ACCENT);
        let file = self.menu_title("File", Menu::File);
        let view_m = self.menu_title("View", Menu::View);
        // The status line, right-aligned in the bar.
        let status = text(self.workspace_status())
            .size(11)
            .color(iced::Color::from_rgb(0.55, 0.58, 0.70));
        let bar = row![
            title,
            Space::new().width(Length::Fixed(16.0)),
            file,
            view_m,
            Space::new().width(Fill), // push the status to the right edge
            status,
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center);
        container(bar)
            .width(Fill)
            .padding([4, 10])
            .style(menu_bar_style)
            .into()
    }

    /// The one-line workspace status shown at the right of the top bar: where the app is
    /// working and in which mode.
    fn workspace_status(&self) -> String {
        match (&self.picked_workspace, &self.run_dir) {
            (Some(dir), _) => format!("iterating in  {}", dir.display()),
            (None, Some(d)) => format!("output  {}", d.display()),
            (None, None) => "no project — File ▸ Open folder".to_string(),
        }
    }

    /// One clickable menu-bar title; highlighted while its dropdown is open.
    fn menu_title(&self, label: &str, which: Menu) -> Element<'_, Message> {
        let open = self.open_menu == Some(which);
        button(
            text(label.to_string())
                .size(13)
                .color(if open { ACCENT } else { FG }),
        )
        .on_press(Message::ToggleMenu(which))
        .padding([3, 10])
        .style(move |_t, status| menu_title_style(open, status))
        .into()
    }

    /// The open dropdown's items, rendered as a floating Windows-style card positioned
    /// directly under its menu-bar title (via top/left spacers in an overlay layer, so it
    /// never shifts the base layout). A full-window transparent backdrop behind it closes
    /// the menu on an outside click. Returns `None` when no menu is open.
    fn view_menu_dropdown(&self) -> Option<Element<'_, Message>> {
        let which = self.open_menu?;
        let items: Vec<(String, Message)> = match which {
            Menu::File => {
                if self.picked_workspace.is_some() {
                    vec![
                        (
                            "📁  Open a different folder…".to_string(),
                            Message::PickWorkspace,
                        ),
                        ("✕  Close project".to_string(), Message::ClearWorkspace),
                    ]
                } else {
                    vec![("📁  Open folder…".to_string(), Message::PickWorkspace)]
                }
            }
            Menu::View => vec![(
                if self.settings_open {
                    "⚙  Hide settings".to_string()
                } else {
                    "⚙  Settings…".to_string()
                },
                Message::ToggleSettings,
            )],
        };
        let mut col = column![].spacing(0);
        for (label, msg) in items {
            col = col.push(
                button(text(label).size(13).color(FG))
                    .on_press(msg)
                    .padding([6, 14])
                    .width(Length::Fixed(230.0))
                    .style(menu_item_style),
            );
        }
        let card = container(col).padding(3).style(dropdown_style);

        // Position under the right title: the menu bar is ~28px tall; File sits ~92px from
        // the left, View ~130px. Spacers place the card; a transparent full-window backdrop
        // behind it (a mouse_area) closes the menu on any outside click.
        let left = match which {
            Menu::File => 88.0,
            Menu::View => 128.0,
        };
        let positioned = column![
            Space::new().height(Length::Fixed(30.0)),
            row![Space::new().width(Length::Fixed(left)), card],
        ];
        // Backdrop: an invisible full-size mouse_area that closes the menu when clicked.
        let backdrop = iced::widget::mouse_area(container(Space::new()).width(Fill).height(Fill))
            .on_press(Message::ToggleMenu(which)); // re-toggle closes it

        Some(
            iced::widget::stack![backdrop, positioned]
                .width(Fill)
                .height(Fill)
                .into(),
        )
    }

    /// The Settings modal: a centered card floating over a dimmed backdrop (an overlay
    /// layer in `view`'s Stack). Clicking the backdrop or the ✕ closes it. This replaces the
    /// old inline panel that pushed the layout around.
    fn view_settings_modal(&self) -> Element<'_, Message> {
        // The dim backdrop; clicking it closes settings.
        let backdrop =
            iced::widget::mouse_area(container(Space::new()).width(Fill).height(Fill).style(
                |_t: &Theme| container::Style {
                    background: Some(Background::Color(Color {
                        a: 0.55,
                        ..Color::BLACK
                    })),
                    ..container::Style::default()
                },
            ))
            .on_press(Message::ToggleSettings);

        // The settings card itself, centered, with a close button in its header.
        let header = row![
            text("Settings").size(16).color(FG),
            Space::new().width(Fill),
            button(text("✕").size(14))
                .on_press(Message::ToggleSettings)
                .padding([2, 8])
                .style(menu_item_style),
        ]
        .align_y(iced::Alignment::Center);
        let card = container(column![header, self.view_settings_body()].spacing(12))
            .width(Length::Fixed(520.0))
            .max_width(560.0)
            .padding(18)
            .style(dropdown_style);

        // `opaque` stops clicks on the card from falling through to the backdrop.
        iced::widget::stack![backdrop, iced::widget::center(iced::widget::opaque(card))]
            .width(Fill)
            .height(Fill)
            .into()
    }

    /// The center column: the planning CHAT thread on top (you ⟷ agent), with the composer
    /// docked at the bottom. When the assistant proposes plan-file changes, they render as
    /// Apply cards under its message. Before a project is opened, a hint to open a folder.
    fn view_center(&self) -> Element<'_, Message> {
        let mut thread = column![section("CHAT  ·  PLAN")].spacing(8);
        if self.conversation.is_none() {
            thread = thread.push(
                text("Open a project folder (File ▸ Open folder) to start planning.")
                    .size(12)
                    .color(FG_MUTED),
            );
        }
        for turn in &self.chat_turns {
            thread = thread.push(self.view_chat_turn(turn));
        }
        // Proposed plan-file Apply cards, after the latest assistant message.
        for (i, pf) in self.proposed_files.iter().enumerate() {
            thread = thread.push(self.view_proposed_file(i, pf));
        }
        // The live "typing" bubble while a reply streams in: show the growing text (with any
        // <think> block hidden), or a thinking cue before the first token arrives.
        if self.chat_session.is_some() {
            let live = self
                .streaming
                .as_deref()
                .map(dc_win::chat::visible_so_far)
                .filter(|s| !s.is_empty());
            match live {
                Some(text_so_far) => {
                    thread = thread.push(
                        column![
                            text("agent").size(11).color(GOOD),
                            text(text_so_far).size(13).color(FG),
                        ]
                        .spacing(2),
                    );
                }
                None => {
                    thread = thread.push(text("agent is thinking…").size(12).color(FG_MUTED));
                }
            }
        }
        let thread = scrollable(thread).height(Fill);

        let composer = self.view_composer();
        container(column![thread, composer].spacing(8))
            .width(Length::FillPortion(2))
            .height(Fill)
            .padding(12)
            .style(card_style)
            .into()
    }

    /// One chat bubble: a small role label + the text, coloured by speaker. A Debug turn (the
    /// raw prompt echo) renders dimmed + monospace so it reads as diagnostic output, not chat.
    fn view_chat_turn<'a>(&self, turn: &'a dc_win::chat::Turn) -> Element<'a, Message> {
        if turn.role == dc_win::chat::Speaker::Debug {
            return container(
                column![
                    text("⛭ prompt sent to model").size(10).color(FG_MUTED),
                    text(turn.text.clone())
                        .size(11)
                        .font(iced::Font::MONOSPACE)
                        .color(FG_MUTED),
                ]
                .spacing(2),
            )
            .width(Fill)
            .padding(6)
            .style(|_t: &Theme| container::Style {
                background: Some(Background::Color(Color::from_rgb(0.08, 0.09, 0.14))),
                border: Border {
                    color: CARD_BORDER,
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..container::Style::default()
            })
            .into();
        }
        let (who, who_color) = match turn.role {
            dc_win::chat::Speaker::You => ("you", ACCENT),
            _ => ("agent", GOOD),
        };
        column![
            text(who).size(11).color(who_color),
            text(turn.text.clone()).size(13).color(FG),
        ]
        .spacing(2)
        .into()
    }

    /// If debug mode is on, echo `prompt` into the chat as a Debug turn (the raw text the
    /// model received). `label` names the call (e.g. "triage", "fix", "chat").
    fn debug_prompt(&mut self, label: &str, prompt: &str) {
        if self.debug {
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Debug,
                text: format!("[{label}]\n{prompt}"),
            });
        }
    }

    /// An Apply card for a proposed plan-file: the filename + an Apply button (writes it to
    /// disk). The file's contents show in the code view when it's the current proposal.
    fn view_proposed_file(
        &self,
        i: usize,
        pf: &dc_win::chat::ProposedFile,
    ) -> Element<'_, Message> {
        let lines = pf.content.lines().count();
        let head = row![
            text(format!("📄 proposed: {}", pf.name))
                .size(13)
                .color(ACCENT),
            Space::new().width(Fill),
            text(format!("{lines} lines")).size(11).color(FG_MUTED),
        ]
        .align_y(iced::Alignment::Center);
        let apply = button(text("✓ Apply to disk").size(13))
            .on_press(Message::ApplyFile(i))
            .padding([5, 12])
            .style(primary_button);
        container(column![head, apply].spacing(6))
            .width(Fill)
            .padding(10)
            .style(dropdown_style)
            .into()
    }

    /// The composer: the text input + send. When a project (conversation) is open it sends a
    /// CHAT turn; with no project it falls back to the from-scratch build action.
    fn view_composer(&self) -> Element<'_, Message> {
        let has_convo = self.conversation.is_some();
        let sending = self.chat_session.is_some();
        let (placeholder, send_msg, label): (&str, Message, &str) = if has_convo {
            (
                "Talk through the plan — ask, refine, decide what's next…",
                Message::ChatSend,
                "send",
            )
        } else {
            (
                "Open a project folder to start…",
                self.run_message(),
                self.run_label(),
            )
        };
        // A fix/iterate run in flight → a Cancel button (the run is the slow, cancellable
        // thing). A quick chat/triage reply just shows a busy "…".
        let run_active = self.session.is_some();
        let input = text_input(placeholder, &self.intent)
            .on_input(Message::IntentChanged)
            .on_submit(send_msg.clone())
            .padding(10)
            .width(Fill);
        let btn = if run_active {
            button(text("⏹ cancel").size(15))
                .on_press(Message::CancelRun)
                .width(Length::Fixed(110.0))
                .padding([8, 12])
                .style(|_t: &Theme, status| {
                    let hov = matches!(status, button::Status::Hovered);
                    button::Style {
                        background: Some(Background::Color(if hov {
                            Color::from_rgb(0.80, 0.35, 0.40)
                        } else {
                            BAD
                        })),
                        text_color: Color::from_rgb(0.06, 0.07, 0.11),
                        border: Border {
                            radius: 6.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                })
        } else if sending {
            button(text("…"))
                .width(Length::Fixed(90.0))
                .padding([8, 12])
        } else {
            button(text(label).size(15))
                .on_press(send_msg)
                .width(Length::Fixed(90.0))
                .padding([8, 12])
                .style(primary_button)
        };
        let mut bar = row![input, btn].spacing(8).align_y(iced::Alignment::Center);
        // The Think toggle (chat mode only): fast conclusions by default, deeper reasoning
        // when you flip it on for a hard planning question.
        if has_convo {
            bar = bar.push(
                checkbox(self.think)
                    .label("think")
                    .on_toggle(Message::ToggleThink),
            );
        }
        // Debug: echo every prompt sent to the model into the chat (always available).
        bar = bar.push(
            checkbox(self.debug)
                .label("debug")
                .on_toggle(Message::ToggleDebug),
        );
        bar.into()
    }

    fn view_topology(&self) -> Element<'_, Message> {
        let canvas = crate::canvas::TopologyCanvas::new(
            &self.topology,
            self.now(),
            self.selected_coder.as_deref(),
        )
        .view();
        container(column![section("SWARM TOPOLOGY"), canvas].spacing(6))
            .width(Length::FillPortion(2))
            .height(Fill)
            .padding(12)
            .style(card_style)
            .into()
    }

    /// The always-visible plan panel (TDD mode): the six workflow phases with status,
    /// the frozen tests written, and the readable subtask list — so you can see what it
    /// intends to do, before and while it does it.
    fn view_plan(&self) -> Element<'_, Message> {
        let mut col = column![section("PLAN  ·  TDD")].spacing(4);
        for step in self.plan.steps() {
            let mark = if step.done { "✓" } else { "·" };
            let line = text(format!("{mark} {}", step.title)).size(13);
            let line = if step.done {
                line.color(GOOD)
            } else {
                line.color(FG_MUTED)
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
                text(format!("frozen tests ({})", self.plan.frozen_tests.len()))
                    .size(13)
                    .color(GOOD),
            );
            for t in &self.plan.frozen_tests {
                col = col.push(text(format!("  🔒 {t}")).size(12).color(FG_MUTED));
            }
        }

        if !self.plan.subtasks.is_empty() {
            col = col.push(Space::new().height(Length::Fixed(8.0)));
            col = col.push(section("SUBTASKS TO IMPLEMENT"));
            for (i, g) in self.plan.subtasks.iter().enumerate() {
                col = col.push(text(format!("  {}. {g}", i + 1)).size(12));
            }
        }

        container(scrollable(col).height(Fill))
            .width(Length::FillPortion(2))
            .padding(12)
            .style(card_style)
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
                    text(format!("coder [{}] — {}", c.subtask, c.goal))
                        .size(14)
                        .color(ACCENT),
                    section("▸ PROMPT SENT TO THIS CODER"),
                    text(prompt).size(11),
                    Space::new().height(Length::Fixed(6.0)),
                    section("◂ OUTPUT IT PROPOSED"),
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
                let mut c = column![
                    section("CODER PROMPT & OUTPUT"),
                    text(hint).size(12).color(FG_MUTED)
                ]
                .spacing(4);
                if let Some(v) = &self.verify_text {
                    c = c.push(Space::new().height(Length::Fixed(6.0)));
                    c = c.push(section("LATEST VERIFICATION"));
                    c = c.push(text(v).size(11));
                }
                c
            }
        };

        container(scrollable(inner).height(Fill))
            .width(Fill)
            .height(Length::FillPortion(1))
            .padding(12)
            .style(card_style)
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
                        .padding(12)
                        .style(card_style)
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
                        .padding(12)
                        .style(card_style)
                        .into(),
                )
            }
        }
    }

    /// The settings form body (no outer card — the modal wraps it). A scrollable column of
    /// the connection + posture controls.
    fn view_settings_body(&self) -> Element<'_, Message> {
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

        let form = column![
            text("CODER  (does the file writing)")
                .size(11)
                .color(FG_MUTED),
            model,
            url,
            text("ORCHESTRATOR  (decomposes the task — needs a reasoning model)")
                .size(11)
                .color(FG_MUTED),
            orch_model,
            orch_url,
            text("ADVISOR  (junior asks senior on a stall)")
                .size(11)
                .color(FG_MUTED),
            advisor,
            advisor_url,
            text("VERIFY & BEHAVIOUR").size(11).color(FG_MUTED),
            verify,
            suffix,
            yolo,
            dry,
        ]
        .spacing(8);

        scrollable(form).height(Length::Fixed(440.0)).into()
    }
}

/// Map a live `AgentEvent` to a concise chat line for the line-comment fix feed, or `None`
/// for events too noisy to surface (plain reads, model turns, plan chatter). Keeps the feed
/// to the steps a human cares about: editing a file, and the verify result.
fn fix_feed_line(e: &dc_core::AgentEvent) -> Option<String> {
    use dc_core::AgentEvent::*;
    match e {
        ToolCall { tool, arg } => match tool.as_str() {
            "edit_file" | "write_file" | "append_file" | "create_file" => {
                Some(format!("✎ editing {}", arg.trim()))
            }
            "read_file" => Some(format!("· reading {}", arg.trim())),
            "run_verification" => Some("· checking it compiles…".to_string()),
            _ => None,
        },
        Verification { green, .. } => Some(if *green {
            "✓ compiles".to_string()
        } else {
            "✗ didn't compile — trying again".to_string()
        }),
        _ => None,
    }
}

/// Find a README in `dir`, case-insensitively (`README.md`, `readme.md`, `Readme.md`, or a
/// bare `README`). Returns the first match, or `None`.
fn find_readme(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let lower = name.to_ascii_lowercase();
            if lower == "readme.md" || lower == "readme" || lower == "readme.txt" {
                return Some(path);
            }
        }
    }
    None
}

/// Find a dedicated TODO file in `dir`, case-insensitively (`TODO.md`, `todo.md`, `TODO`,
/// `TODO.txt`). Returns the first match, or `None`.
fn find_todo_file(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let lower = name.to_ascii_lowercase();
            if lower == "todo.md" || lower == "todo" || lower == "todo.txt" {
                return Some(path);
            }
        }
    }
    None
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
