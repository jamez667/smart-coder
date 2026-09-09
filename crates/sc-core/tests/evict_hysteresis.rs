//! Eviction has HYSTERESIS: once it fires it evicts down to `WINDOW_EVICT_TARGET` of the
//! budget, not merely to "just fits".
//!
//! Why this matters is a cache property, not a taste: llama.cpp reuses the KV cache for the
//! longest byte-identical PREFIX of the previous request, so any turn whose prompt SHRINKS
//! invalidates the cache and forces a full re-prefill of everything that remains. Evicting
//! the minimum leaves the prompt on the budget ceiling, so the next turn overflows and
//! shrinks again -- measured on a real 15-turn refactor
//! (`evals/results/2026-09-08-refactor-fixed`) as three consecutive shrinks at the end of the
//! run and a 58% cache hit against 81% on the eval ladder.
//!
//! These tests drive the real loop with a scripted backend and read every turn's prompt size
//! out of the verbose `PromptAssembled` event, the same seam `prefix_stability.rs` uses.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sc_core::{run_agent_observed, AgentConfig, AgentEvent, FnSink, ParseRepair};
use sc_model::{
    CallbackBackend, Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling,
};
use sc_tools::default_registry;

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sc-core-evict-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One assembled prompt: its token count and its `(role, content)` messages.
struct Turn {
    tokens: usize,
    messages: Vec<(String, String)>,
}

fn read(path: &str) -> String {
    format!(r#"{{"tool":"read_file","path":"{path}"}}"#)
}

/// A scripted backend that declares a small context window, to force budget eviction.
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

/// Run the loop verbosely and return every turn's assembled prompt, in turn order.
fn turns_of(backend: &dyn ModelBackend, task: &str, ws: &Path, cfg: AgentConfig) -> Vec<Turn> {
    let cfg = AgentConfig {
        verbose: true,
        // These scripts read without editing; keep the stall ladder out of the way.
        repeat_limit: 40,
        no_progress_limit: 40,
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
    events
        .into_inner()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e {
            AgentEvent::PromptAssembled {
                tokens, messages, ..
            } => Some(Turn {
                tokens,
                messages: messages.into_iter().map(|m| (m.role, m.content)).collect(),
            }),
            _ => None,
        })
        .collect()
}

/// Write `n` files of roughly one page each, small enough that several fit in the window at
/// once -- so eviction is a matter of ACCUMULATION over turns, which is the case the ceiling
/// bug shows up in, rather than a single observation blowing the budget on its own.
fn write_pages(ws: &Path, names: &[&str], lines: usize) {
    for f in names {
        let body: String = (0..lines)
            .map(|i| format!("{f} line {i:04} {}\n", "filler ".repeat(8)))
            .collect();
        std::fs::write(ws.join(f), body).unwrap();
    }
}

/// A saturating script of steadily GROWING reads, written into `ws` as it goes.
///
/// Uniform observations do not reproduce the bug: evict-one/append-one nets out and the
/// prompt drifts gently upward. It takes a run whose observations grow -- the ordinary case,
/// and the measured trace's own shape -- for the pair arriving to outweigh the pair leaving,
/// which is when a minimum-eviction policy overflows again on the very next turn.
fn growing_reads(ws: &Path) -> Vec<String> {
    let mut script = Vec::new();
    for i in 0..22usize {
        // Each read is slightly LARGER than the last. Once the window saturates, the pair
        // arriving is bigger than the pair leaving, so the minimum-eviction policy is over
        // budget again on the very next turn -- eviction every turn, the prefix broken every
        // turn. Growing observations are the ordinary case in a real run (a model reads
        // progressively wider slices as it orients), and they are what the ceiling bug bites.
        // The run ORIENTS on progressively wider reads -- a model reading steadily larger
        // slices as it works out where it is, which is the ordinary shape of a real run and
        // the shape of the measured trace (it grew monotonically for twelve calls). Growing
        // observations are what make the pair ARRIVING bigger than the pair LEAVING, so a
        // minimum-eviction policy is over budget again on the very next turn: eviction every
        // turn, and the prefix broken every turn.
        let lines = 40 + i * 6;
        let name = format!("p{i:02}.txt");
        write_pages(ws, &[name.as_str()], lines);
        script.push(read(&name));
    }
    script
}

/// Which turns SHRANK relative to the turn before (a broken prefix), as 1-based turn numbers.
fn shrink_turns(turns: &[Turn]) -> Vec<usize> {
    (1..turns.len())
        .filter(|&i| turns[i].tokens < turns[i - 1].tokens)
        .map(|i| i + 1)
        .collect()
}

/// THE REGRESSION. Under a saturated window, eviction must not fire on consecutive turns:
/// after a shrink the following turns must APPEND cleanly (the prompt grows).
///
/// Before the hysteresis fix the loop evicted the minimum to fit, so the prompt settled on
/// the budget ceiling and every subsequent turn shrank again. Reverting the fix (making the
/// loop stop at `over` alone) makes this assert fail with consecutive shrink turns.
#[test]
fn eviction_does_not_fire_on_consecutive_turns() {
    let ws = temp("consecutive");
    let script = growing_reads(&ws);
    let backend = small_window_backend(12_000, script.clone());
    let cfg = AgentConfig {
        keep_recent_turns: 1,
        response_reserve_tokens: 256,
        max_steps: script.len() + 2,
        ..AgentConfig::default()
    };
    let turns = turns_of(&backend, "read every file", &ws, cfg);
    assert!(
        turns.len() >= 12,
        "need a long enough run to saturate, got {}",
        turns.len()
    );

    let sizes: Vec<usize> = turns.iter().map(|t| t.tokens).collect();
    let shrinks = shrink_turns(&turns);
    assert!(
        shrinks.len() >= 3,
        "the window never saturated, so this test proves nothing: {sizes:?}"
    );

    // THE ASSERT. No two shrinks back to back: a shrink at turn k must be followed by at
    // least one turn that APPENDS. Before the hysteresis fix this scenario shrinks at turns
    // 10 and 11 in a row (verified by reverting the fix), because the minimum-eviction
    // policy leaves the prompt on the budget ceiling and the next turn overflows it again.
    for w in shrinks.windows(2) {
        assert!(
            w[1] > w[0] + 1,
            "eviction fired on consecutive turns {} and {} -- the prefix broke twice in a              row. Sizes: {sizes:?}, shrinks: {shrinks:?}",
            w[0],
            w[1]
        );
    }
    // Each eviction genuinely left headroom: the turn after a shrink grew. That is the
    // headroom being spent, and it is only there because eviction went past "just fits".
    for &k in &shrinks {
        if k < turns.len() {
            assert!(
                sizes[k] > sizes[k - 1],
                "turn {} did not append after the eviction at turn {k}. Sizes: {sizes:?}",
                k + 1
            );
        }
    }
    let _ = std::fs::remove_dir_all(&ws);
}

/// A prompt that FITS is untouched: no eviction, no summary, and turn N stays a
/// byte-identical prefix of turn N+1. The deeper target must never reach a run that was
/// never over budget -- that is the common case.
#[test]
fn a_prompt_that_fits_is_left_completely_alone() {
    let ws = temp("fits");
    write_pages(&ws, &["a.txt", "b.txt", "c.txt"], 4);
    let backend = small_window_backend(
        128_000,
        vec![
            read("a.txt"),
            read("b.txt"),
            read("c.txt"),
            r#"{"tool":"finish"}"#.to_string(),
        ],
    );
    let turns = turns_of(&backend, "look at the files", &ws, AgentConfig::default());
    assert_eq!(turns.len(), 4);

    for t in &turns {
        assert!(
            !t.messages.iter().any(|(_, c)| c.starts_with("Earlier (")),
            "nothing should have been evicted into the summary"
        );
    }
    for n in 0..turns.len() - 1 {
        let (prev, next) = (&turns[n].messages, &turns[n + 1].messages);
        assert_eq!(
            next.len(),
            prev.len() + 2,
            "turn {} adds exactly one assistant/user pair",
            n + 2
        );
        assert_eq!(
            &next[..prev.len()],
            &prev[..],
            "turn {} must be a byte-identical prefix of turn {}",
            n + 1,
            n + 2
        );
    }
    let _ = std::fs::remove_dir_all(&ws);
}

/// `keep_recent_turns` is an ABSOLUTE floor. The deeper target may want to keep evicting
/// long past it -- it must stop, every turn, at the configured verbatim minimum.
#[test]
fn the_keep_recent_turns_floor_is_never_breached() {
    let ws = temp("floor");
    let names = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt", "f.txt"];
    // Pages big enough that even ONE will not leave the prompt under the 0.8 target, so the
    // deeper pass would evict forever if the floor did not stop it.
    write_pages(&ws, &names, 200);
    const FLOOR: usize = 3;
    let backend = small_window_backend(16_000, names.iter().map(|f| read(f)).collect::<Vec<_>>());
    let cfg = AgentConfig {
        keep_recent_turns: FLOOR,
        response_reserve_tokens: 256,
        max_steps: names.len() + 2,
        ..AgentConfig::default()
    };
    let turns = turns_of(&backend, "read every file", &ws, cfg);

    // The window holds one assistant action plus at least one user message per turn, so the
    // count of assistant messages in the prompt is the window's turn count. Once the run has
    // produced more turns than the floor, the window must hold exactly the floor, never less.
    for (i, t) in turns.iter().enumerate() {
        let completed = i; // turns already in the window when turn i+1 was assembled
        let in_window = t.messages.iter().filter(|(r, _)| r == "assistant").count();
        let expected_floor = FLOOR.min(completed);
        assert!(
            in_window >= expected_floor,
            "turn {} kept only {in_window} verbatim turns, below the keep_recent_turns \
             floor of {expected_floor}",
            i + 1
        );
    }
    let _ = std::fs::remove_dir_all(&ws);
}
