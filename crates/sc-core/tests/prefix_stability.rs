//! The assembled prompt is APPEND-ONLY between turns (plan phase 1.2).
//!
//! llama.cpp reuses its KV cache for the longest byte-identical prefix of the previous
//! request, so a turn that changes nothing must send exactly the previous prompt plus the
//! new action/observation pair. These tests drive the real loop with scripted backends and
//! inspect every turn's assembled messages via the verbose `PromptAssembled` event.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sc_core::{run_agent_observed, AgentConfig, AgentEvent, FnSink, ParseRepair};
use sc_model::{
    CallbackBackend, Capabilities, GenerateRequest, GenerateResponse, MockBackend, ModelBackend,
    ToolCalling,
};
use sc_tools::default_registry;

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-core-prefix-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One assembled prompt: `(role, content)` per message, in the order the model sees them.
type Prompt = Vec<(String, String)>;

/// Run the loop verbosely and return every turn's assembled prompt, in turn order.
fn prompts_of(backend: &dyn ModelBackend, task: &str, ws: &Path, cfg: AgentConfig) -> Vec<Prompt> {
    let cfg = AgentConfig {
        verbose: true,
        // These scripts read without editing; keep the stall ladder out of the way.
        repeat_limit: 20,
        no_progress_limit: 20,
        ..cfg
    };
    let events: Mutex<Vec<AgentEvent>> = Mutex::new(Vec::new());
    let sink = FnSink(|e: &AgentEvent| events.lock().unwrap().push(e.clone()));
    run_agent_observed(
        backend,
        None,
        &default_registry(),
        &ParseRepair,
        task,
        ws,
        &cfg,
        &sink,
    )
    .unwrap();
    let evs = events.into_inner().unwrap();
    evs.into_iter()
        .filter_map(|e| match e {
            AgentEvent::PromptAssembled { messages, .. } => Some(
                messages
                    .into_iter()
                    .map(|m| (m.role, m.content))
                    .collect::<Prompt>(),
            ),
            _ => None,
        })
        .collect()
}

/// The prompt of turn `k` (1-based) minus its recent window, which holds one
/// action/observation pair per completed turn when no harness note was attached.
fn before_window(prompts: &[Prompt], k: usize) -> &[(String, String)] {
    let p = &prompts[k - 1];
    &p[..p.len() - 2 * (k - 1)]
}

fn read(path: &str) -> String {
    format!(r#"{{"tool":"read_file","path":"{path}"}}"#)
}

/// (a) With no edit, turn N's messages are a byte-identical prefix of turn N+1's, which
/// adds exactly the new action/observation pair. (d) The newest observation is last.
#[test]
fn each_turn_extends_the_previous_prompt_by_exactly_one_pair() {
    let ws = temp("append");
    for (f, body) in [
        ("a.txt", "alpha body"),
        ("b.txt", "bravo body"),
        ("c.txt", "charlie body"),
    ] {
        std::fs::write(ws.join(f), body).unwrap();
    }
    let backend = MockBackend::new([
        read("a.txt"),
        read("b.txt"),
        read("c.txt"),
        r#"{"tool":"finish"}"#.to_string(),
    ]);
    let prompts = prompts_of(
        &backend,
        "look at the three files",
        &ws,
        AgentConfig::default(),
    );
    assert_eq!(prompts.len(), 4, "one assembled prompt per turn");

    for n in 0..3 {
        let (prev, next) = (&prompts[n], &prompts[n + 1]);
        assert_eq!(
            next.len(),
            prev.len() + 2,
            "turn {} adds exactly one assistant/user pair",
            n + 2
        );
        assert_eq!(
            &next[..prev.len()],
            &prev[..],
            "turn {} must be byte-identical to turn {} before the new pair",
            n + 2,
            n + 1
        );
        assert_eq!(next[next.len() - 2].0, "assistant");
        assert_eq!(next[next.len() - 1].0, "user");
    }
    // (d) The last message is the newest observation: turn 4's prompt ends with c.txt.
    let last = &prompts[3].last().unwrap().1;
    assert!(
        last.contains("charlie body"),
        "newest observation last, got: {last}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// (b) Focus mode: an edit re-renders the FocusFile segment and nothing else before the
/// window.
#[test]
fn an_edit_moves_only_the_focus_segment_in_focus_mode() {
    let ws = temp("focus-edit");
    std::fs::write(ws.join("app.py"), "def handler():\n    return 1\n").unwrap();
    std::fs::write(ws.join("helper.py"), "def helper():\n    return 2\n").unwrap();
    std::fs::write(ws.join("other.py"), "def other():\n    return 3\n").unwrap();
    let backend = MockBackend::new([
        read("helper.py"),
        r#"{"tool":"edit_file","path":"app.py","old_str":"return 1","new_str":"return 42"}"#
            .to_string(),
        read("other.py"),
        r#"{"tool":"finish"}"#.to_string(),
    ]);
    let cfg = AgentConfig {
        focus_files: vec!["app.py".to_string()],
        ..AgentConfig::default()
    };
    let prompts = prompts_of(&backend, "edit app.py", &ws, cfg);
    assert_eq!(prompts.len(), 4);

    // The read changed nothing: turn 2's prefix equals turn 1's.
    assert_eq!(before_window(&prompts, 2), before_window(&prompts, 1));
    // The edit: same shape, exactly one message differs, and it is the focus render.
    let (p2, p3) = (before_window(&prompts, 2), before_window(&prompts, 3));
    assert_eq!(p2.len(), p3.len(), "an edit adds/removes no segment");
    let moved: Vec<usize> = (0..p2.len()).filter(|&i| p2[i] != p3[i]).collect();
    assert_eq!(moved.len(), 1, "exactly one segment moved, got {moved:?}");
    let focus = &p3[moved[0]].1;
    assert!(focus.contains("app.py (line-numbered)"), "{focus}");
    assert!(focus.contains("return 42"), "{focus}");
    assert!(p2[moved[0]].1.contains("return 1"));
    // And the focus segment sits right before the window.
    assert_eq!(
        moved[0],
        p3.len() - 1,
        "focus file is the last thing before the window"
    );
    // The next read changes nothing again.
    assert_eq!(before_window(&prompts, 4), before_window(&prompts, 3));
    let _ = std::fs::remove_dir_all(&ws);
}

/// (b) Whole-task mode: an edit to an existing file moves NOTHING before the window (the
/// ledger lists the same files; the repo map is fixed for the run).
#[test]
fn an_edit_moves_nothing_before_the_window_in_whole_task_mode() {
    let ws = temp("whole-edit");
    std::fs::write(ws.join("app.py"), "def handler():\n    return 1\n").unwrap();
    std::fs::write(ws.join("helper.py"), "def helper():\n    return 2\n").unwrap();
    let backend = MockBackend::new([
        read("helper.py"),
        r#"{"tool":"edit_file","path":"app.py","old_str":"return 1","new_str":"return 42"}"#
            .to_string(),
        read("app.py"),
        r#"{"tool":"finish"}"#.to_string(),
    ]);
    let prompts = prompts_of(&backend, "fix app.py", &ws, AgentConfig::default());
    assert_eq!(prompts.len(), 4);
    for k in 1..4 {
        assert_eq!(
            before_window(&prompts, k + 1),
            before_window(&prompts, k),
            "turn {} moved something before the window",
            k + 1
        );
    }
    let _ = std::fs::remove_dir_all(&ws);
}

/// A scripted backend with a small context window, to force budget eviction.
fn small_window_backend(
    max_context_tokens: usize,
    script: Vec<String>,
) -> CallbackBackend<impl Fn(&GenerateRequest) -> sc_proto::Result<GenerateResponse>> {
    let queue = RefCell::new(script.into_iter().collect::<VecDeque<_>>());
    CallbackBackend::new(
        "small",
        Capabilities {
            max_context_tokens,
            tool_calling: ToolCalling::None,
            on_device: false,
        },
        move |_req: &GenerateRequest| {
            let next = queue
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| r#"{"tool":"finish"}"#.to_string());
            Ok(GenerateResponse::new(next))
        },
    )
}

/// (c) Under budget pressure the OLDEST whole pair leaves the window first, into the
/// history summary, which names what it did.
#[test]
fn the_oldest_pair_is_evicted_first_and_summarized() {
    let ws = temp("evict");
    // Three big files, each under `read_file`'s 400-line page so a read shows the whole
    // body (~8k tokens): the budget below (0.75 * 20k - 256 = 14,744) holds ONE such
    // observation beside any plausible system prompt, and never two.
    for f in ["a.txt", "b.txt", "c.txt"] {
        let body: String = (0..390)
            .map(|i| format!("{f} line {i:04} {}\n", "filler ".repeat(8)))
            .collect();
        std::fs::write(ws.join(f), body).unwrap();
    }
    let backend = small_window_backend(
        20_000,
        vec![
            read("a.txt"),
            read("b.txt"),
            read("c.txt"),
            r#"{"tool":"finish"}"#.to_string(),
        ],
    );
    let cfg = AgentConfig {
        // The verbatim FLOOR: eviction may take the window down to one pair.
        keep_recent_turns: 1,
        response_reserve_tokens: 256,
        ..AgentConfig::default()
    };
    let prompts = prompts_of(&backend, "read the files", &ws, cfg);
    assert_eq!(prompts.len(), 4);

    let summary_of = |p: &Prompt| {
        p.iter()
            .find(|(_, c)| c.starts_with("Earlier ("))
            .map(|(_, c)| c.clone())
    };
    let joined = |p: &Prompt| p.iter().map(|(_, c)| c.as_str()).collect::<String>();

    // Turn 2: only a.txt has been read; it fits, nothing is summarized.
    assert!(summary_of(&prompts[1]).is_none(), "nothing evicted yet");
    assert!(joined(&prompts[1]).contains("a.txt line 0389"));

    // Turn 3: a.txt + b.txt do not fit. The OLDEST (a.txt) leaves, b.txt stays verbatim.
    let s3 = summary_of(&prompts[2]).expect("turn 3 evicted the oldest pair");
    assert!(
        s3.contains("read:") && s3.contains("a.txt"),
        "summary names the tool+arg: {s3}"
    );
    assert!(
        !s3.contains("b.txt"),
        "the newer pair was not evicted: {s3}"
    );
    let j3 = joined(&prompts[2]);
    assert!(
        !j3.contains("a.txt line 0389"),
        "a.txt's body left the window"
    );
    assert!(
        j3.contains("b.txt line 0389"),
        "b.txt's body is still verbatim"
    );

    // Turn 4: b.txt goes the same way; the summary now covers both, in order.
    let s4 = summary_of(&prompts[3]).expect("turn 4 evicted again");
    assert!(s4.contains("a.txt, b.txt"), "{s4}");
    assert!(joined(&prompts[3]).contains("c.txt line 0389"));
    // (d) and the newest observation is still the last message.
    assert!(prompts[3].last().unwrap().1.contains("c.txt line 0389"));
    let _ = std::fs::remove_dir_all(&ws);
}
