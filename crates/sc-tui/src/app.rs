//! The TUI run harness: set up the terminal, run the agent on a background
//! thread that streams events over a channel, and draw the live view on the main
//! thread until the run ends or the user quits.
//!
//! The agent loop is synchronous and blocking, so we move it off the render
//! thread. The agent's [`sc_core::EventSink`] is a channel sender; the UI drains
//! the channel each frame and folds events into [`TuiState`].

use std::io;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use sc_core::{run_agent_observed, AgentConfig, AgentEvent, AgentReport, FnSink, ToolCallStrategy};
use sc_model::ModelBackend;
use sc_tools::ToolRegistry;

use crate::render::draw;
use crate::state::TuiState;

/// Everything the harness needs to drive a run. Backends are owned (moved onto
/// the worker thread), so this takes them by value. The strategy is boxed (the
/// usual product of [`sc_core::select_strategy`]).
pub struct TuiRun<B, A>
where
    B: ModelBackend + Send + 'static,
    A: ModelBackend + Send + 'static,
{
    pub backend: B,
    pub advisor: Option<A>,
    pub registry: ToolRegistry,
    pub strategy: Box<dyn ToolCallStrategy + Send + Sync>,
    pub instruction: String,
    pub workspace: std::path::PathBuf,
    pub config: AgentConfig,
    /// Optional session log: when set, the worker tees every event into this file
    /// as JSON lines (alongside the live channel) so the run is replayable (spec
    /// 06). `None` disables logging.
    pub log: Option<std::path::PathBuf>,
}

/// Run a task with the live TUI. Returns the agent's report once the run ends and
/// the user dismisses the view (or immediately on a finished run if they quit).
pub fn run<B, A>(spec: TuiRun<B, A>) -> io::Result<Option<AgentReport>>
where
    B: ModelBackend + Send + 'static,
    A: ModelBackend + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<AgentEvent>();

    // Drive the agent on a worker thread; it streams events through the channel.
    let worker = thread::spawn(move || {
        let channel_sink = FnSink(move |e: &AgentEvent| {
            // If the UI is gone, the send fails — that's fine, the run can finish.
            let _ = tx.send(e.clone());
        });
        // Optionally tee the same stream into a JSON-lines session log for replay
        // (spec 06). Opening it here keeps the non-Send file on the worker thread.
        let log_sink = spec
            .log
            .as_ref()
            .and_then(|path| open_log(path).map(sc_core::JsonLinesSink::new));
        let mut sinks: Vec<&dyn sc_core::EventSink> = vec![&channel_sink];
        if let Some(ref s) = log_sink {
            sinks.push(s);
        }
        let sink = sc_core::TeeSink::new(sinks);
        run_agent_observed(
            &spec.backend,
            spec.advisor.as_ref().map(|a| a as &dyn ModelBackend),
            &spec.registry,
            spec.strategy.as_ref(),
            &spec.instruction,
            &spec.workspace,
            &spec.config,
            &sink,
        )
    });

    let report = render_loop(rx)?;
    // Worker has finished emitting by the time we see `Stopped`; join for the
    // report. (If the user quit early, this still returns the final result.)
    let agent_result = worker.join().ok().and_then(|r| r.ok());
    Ok(report.or(agent_result))
}

/// The main-thread render loop: drain events, redraw, handle input.
fn render_loop(rx: Receiver<AgentEvent>) -> io::Result<Option<AgentReport>> {
    let mut terminal = setup_terminal()?;
    let mut state = TuiState::new();
    let mut result = None;

    let outcome = (|| -> io::Result<()> {
        loop {
            // Drain all pending events into the state.
            while let Ok(ev) = rx.try_recv() {
                state.apply(&ev);
            }

            terminal.draw(|f| draw(f, &state))?;

            // Poll for quit; keep the frame rate modest.
            if event::poll(Duration::from_millis(80))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press
                        && matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                    {
                        break;
                    }
                }
            }

            // When the run is done, drain any stragglers and keep the final frame
            // up until the user quits (so they can read the result).
            if state.is_done() && rx.try_recv().is_err() {
                // Stay until 'q', but stop spinning the agent.
                result = state.stop.clone();
            }
        }
        Ok(())
    })();

    restore_terminal(&mut terminal)?;
    outcome?;
    // `result` here is only the StopReason; the real AgentReport comes from the
    // worker join in `run`. Signal "we saw a stop" by returning None — `run`
    // prefers the worker's report.
    let _ = result;
    Ok(None)
}

/// Open (create/truncate) the session log, creating its parent dir. Best-effort:
/// returns `None` on failure (and stays silent — the TUI owns the alt-screen, so
/// printing here would corrupt the display; a failed log must never break a run).
fn open_log(path: &std::path::Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::File::create(path).ok()
}

type Term = Terminal<CrosstermBackend<io::Stdout>>;

fn setup_terminal() -> io::Result<Term> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Term) -> io::Result<()> {
    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
