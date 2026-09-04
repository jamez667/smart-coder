//! The A/B that decides whether investigate leads ship on (spec 23 M7).
//!
//! Runs the SAME question through the SAME investigate path twice — once with the
//! leads block in the task anchor, once without — and writes both transcripts plus a
//! comparison to `logs/leads-probe.md`.
//!
//! The bar it exists to test, from spec 23: leads ship default-on only if the probes
//! answer in **fewer steps** with them than without. A retrieval feature that costs
//! anchor tokens without shortening runs is deleted, not tuned. This test therefore
//! **asserts nothing about which arm wins** — it produces the number a human reads.
//! Encoding the expected result as an assertion would make the measurement decorative.
//!
//! Ignored by default: it needs the local backend and runs two full investigations.
//!
//! Run with:
//!   cargo test -p sc-win --test leads_probe -- --ignored --nocapture

use std::fmt::Write as _;
use std::io::Write as _;

const QUESTION: &str = "Can you investigate why on the jump screen the trail behind the \
                        stars it thin before it gets thick? it should be the other way around.";

/// What one arm of the A/B did.
struct Arm {
    leads: bool,
    answered: bool,
    steps: usize,
    tool_calls: usize,
    faults: usize,
    secs: u64,
    answer: String,
    /// The task anchor as assembled — the thing the arms actually differ by.
    task: String,
    transcript: String,
}

#[test]
#[ignore]
fn leads_on_versus_leads_off() {
    let ws = std::path::PathBuf::from(r"C:\Users\mail\working\Personal\Games\void-claim");
    assert!(ws.join("crates").is_dir(), "void-claim not found at {ws:?}");
    let cfg = sc_win::config::UiConfig::load();

    // Warm the index once, outside both arms: the first build costs seconds, and
    // charging it to whichever arm happens to run first would make the comparison a
    // measurement of cache state rather than of leads.
    let _ = sc_index::RepoIndex::open(&ws);

    println!("== arm A: leads OFF ==");
    let off = run_arm(&cfg, &ws, false);
    println!("== arm B: leads ON ==");
    let on = run_arm(&cfg, &ws, true);

    let mut out = String::new();
    let _ = writeln!(out, "# Investigate leads — A/B probe\n");
    let _ = writeln!(out, "- workspace: `{}`", ws.display());
    let _ = writeln!(out, "- model: `{}`", cfg.model);
    let _ = writeln!(out, "- question:\n\n> {QUESTION}\n");

    let _ = writeln!(out, "## Result\n");
    let _ = writeln!(
        out,
        "| | answered | steps | tool calls | faults | elapsed |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|");
    for a in [&off, &on] {
        let _ = writeln!(
            out,
            "| leads {} | {} | {} | {} | {} | {}s |",
            if a.leads { "ON" } else { "OFF" },
            if a.answered { "yes" } else { "**no**" },
            a.steps,
            a.tool_calls,
            a.faults,
            a.secs
        );
    }

    // State the verdict against spec 23's bar, in one line, so nobody has to
    // reconstruct the rule from the table.
    let _ = writeln!(out, "\n## Verdict\n");
    let verdict = match (off.answered, on.answered) {
        (true, false) => "**Do not ship.** Leads ON failed to answer a question that OFF answered.",
        (false, true) => "**Ships.** Leads ON answered a question that OFF did not.",
        (false, false) => "**Inconclusive.** Neither arm answered; fix that before judging leads.",
        (true, true) => match on.steps.cmp(&off.steps) {
            std::cmp::Ordering::Less => {
                "**Ships.** Both answered and ON used fewer steps — the bar spec 23 set."
            }
            std::cmp::Ordering::Equal => {
                "**Do not ship.** Both answered in the same number of steps, so the anchor \
                 tokens bought nothing."
            }
            std::cmp::Ordering::Greater => {
                "**Do not ship.** Both answered and ON used MORE steps. Delete the feature \
                 rather than tuning it."
            }
        },
    };
    let _ = writeln!(out, "{verdict}\n");
    let _ = writeln!(
        out,
        "The bar (spec 23): leads ship default-on only if the probe answers in fewer \
         steps with them than without, and the sorted-map result still holds. Flipping \
         the default is a human decision; this file is the evidence for it.\n"
    );

    for a in [&off, &on] {
        let _ = writeln!(
            out,
            "\n---\n\n# Arm: leads {}\n",
            if a.leads { "ON" } else { "OFF" }
        );
        let _ = writeln!(out, "## Task anchor\n\n```\n{}\n```\n", a.task);
        let _ = writeln!(
            out,
            "## Answer\n\n{}\n",
            if a.answer.trim().is_empty() {
                "_none — the loop never called `finish`._"
            } else {
                a.answer.trim()
            }
        );
        let _ = writeln!(out, "## Transcript\n\n{}", a.transcript);
    }

    let dir = std::path::Path::new("logs");
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("leads-probe.md");
    let mut f = std::fs::File::create(&path).expect("write the probe log");
    f.write_all(out.as_bytes()).unwrap();

    println!("\n{verdict}");
    println!(
        "leads OFF: answered={} steps={}  |  leads ON: answered={} steps={}",
        off.answered, off.steps, on.answered, on.steps
    );
    println!("WROTE {} ({} bytes)", path.display(), out.len());
}

/// Run one investigation with leads on or off.
///
/// The env var is set for the duration of the arm and cleared after. Both arms run in
/// this one test rather than as two tests precisely so this stays sequential — two
/// tests would run concurrently and race on a process-global.
fn run_arm(cfg: &sc_win::config::UiConfig, ws: &std::path::Path, leads: bool) -> Arm {
    if leads {
        std::env::set_var("SC_INVESTIGATE_LEADS", "1");
    } else {
        std::env::remove_var("SC_INVESTIGATE_LEADS");
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let cfg2 = cfg.clone();
    let ws2 = ws.to_path_buf();
    let started = std::time::Instant::now();
    let h = std::thread::spawn(move || {
        sc_win::session::agent::investigate(cfg2, QUESTION.to_string(), ws2, tx);
    });

    let mut arm = Arm {
        leads,
        answered: false,
        steps: 0,
        tool_calls: 0,
        faults: 0,
        secs: 0,
        answer: String::new(),
        task: String::new(),
        transcript: String::new(),
    };

    while let Ok(ev) = rx.recv() {
        match ev {
            sc_win::session::UiEvent::Agent(a) => match a {
                sc_core::AgentEvent::RunStarted { task, .. } => arm.task = task,
                sc_core::AgentEvent::ModelTurn { step, raw, .. } => {
                    arm.steps = arm.steps.max(step);
                    let _ = writeln!(
                        arm.transcript,
                        "**step {step}** — reply {} chars\n",
                        raw.len()
                    );
                }
                sc_core::AgentEvent::ToolCall { tool, arg } => {
                    arm.tool_calls += 1;
                    let short: String = arg.chars().take(120).collect();
                    let _ = writeln!(arm.transcript, "- `{tool}` — `{short}`\n");
                    println!("  [{}] {tool}  {short}", arm.tool_calls);
                    if tool == "finish" {
                        arm.answer = arg;
                    }
                }
                sc_core::AgentEvent::HarnessFault { kind, detail, step } => {
                    arm.faults += 1;
                    let _ = writeln!(
                        arm.transcript,
                        "- **FAULT** ({}) at step {step}: {detail}\n",
                        kind.label()
                    );
                }
                _ => {}
            },
            sc_win::session::UiEvent::Failed(m) => {
                let _ = writeln!(arm.transcript, "\n**FAILED**: {m}\n");
            }
            sc_win::session::UiEvent::Done { ok, summary } => {
                let _ = writeln!(arm.transcript, "\n**DONE** ok={ok} — {summary}\n");
                break;
            }
            _ => {}
        }
    }
    h.join().unwrap();
    arm.secs = started.elapsed().as_secs();
    arm.answered = !arm.answer.trim().is_empty();
    std::env::remove_var("SC_INVESTIGATE_LEADS");
    arm
}
