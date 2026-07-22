//! Render the TUI to an in-memory buffer (ratatui's TestBackend) so the draw
//! path is exercised end-to-end without a real terminal — folding a realistic
//! event sequence into state and asserting the frame shows what it should.

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use sc_core::{AgentEvent, StopReason};
use sc_tui::{draw, TuiState};

fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
    let buf = terminal.backend().buffer();
    buf.content().iter().map(|c| c.symbol()).collect::<String>()
}

#[test]
fn renders_a_live_run_frame() {
    let mut state = TuiState::new();
    for e in [
        AgentEvent::RunStarted {
            task: "make is_even pass".into(),
            prompt_budget: 5120,
        },
        AgentEvent::Planned {
            steps: vec!["read impl".into(), "fix it".into(), "run tests".into()],
        },
        AgentEvent::ModelTurn {
            step: 1,
            prompt_tokens: 800,
            raw: "{\"tool\":\"read_file\",\"path\":\"impl.sh\"}".into(),
        },
        AgentEvent::ToolCall {
            tool: "read_file".into(),
            arg: "impl.sh".into(),
        },
        AgentEvent::ToolResult {
            summary: "read 1 line".into(),
            full: "read 1 line".into(),
            is_error: false,
        },
        AgentEvent::ToolCall {
            tool: "edit_file".into(),
            arg: "impl.sh".into(),
        },
        AgentEvent::ToolResult {
            summary: "edit_file impl.sh ok".into(),
            full: "edit_file impl.sh ok".into(),
            is_error: false,
        },
        AgentEvent::Verification {
            green: true,
            summary: "all 1 test(s) passed".into(),
            full: "all 1 test(s) passed".into(),
        },
        AgentEvent::Stopped {
            reason: StopReason::Finished,
        },
    ] {
        state.apply(&e);
    }

    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| draw(f, &state)).unwrap();

    let text = buffer_text(&terminal);
    // Title, task, plan, activity, and the honest finished status all rendered.
    assert!(text.contains("smart-coder"), "title missing");
    assert!(text.contains("make is_even pass"), "task missing");
    assert!(text.contains("read impl"), "plan step missing");
    assert!(text.contains("read_file"), "tool call missing");
    assert!(text.contains("finished"), "status missing");
}

#[test]
fn renders_a_stalled_run_frame() {
    let mut state = TuiState::new();
    state.apply(&AgentEvent::RunStarted {
        task: "t".into(),
        prompt_budget: 4096,
    });
    state.apply(&AgentEvent::Stalled {
        trigger: "looping".into(),
    });
    state.apply(&AgentEvent::Stopped {
        reason: StopReason::Stalled("looping".into()),
    });

    let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
    terminal.draw(|f| draw(f, &state)).unwrap();
    assert!(buffer_text(&terminal).contains("stalled"));
}
