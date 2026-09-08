//! The ladder A/B: the same model, the same rungs, several ways of driving it.
//!
//! Runs the capability ladder once per arm against the same model and prints
//! the scorecards side by side, with a per-rung breakdown. Every row is also
//! appended to `<out>/rows.jsonl`, so a run survives its terminal.
//!
//! ```text
//! cargo run --release -p sc-eval --bin ladder-ab -- \
//!     --url http://localhost:11436/v1 --model tiel-coder-35b \
//!     [--arms control,raw,pi,gateway] [--repeat N] [--steps N] \
//!     [--suite <path>] [--out <dir>]
//! ```
//!
//! Arms (default `control,raw,pi`):
//!
//! * `control` -- the `sc_core` loop with the measured six tools.
//! * `raw` -- the model with no harness at all ([`sc_eval::raw_arm`]).
//! * `pi` -- the pi-style loop ([`sc_eval::pi_arm`]).
//! * `gateway` -- the `sc_core` loop with one classified `ask` replacing the
//!   reading half of the six ([`sc_eval::gateway_arm`]).
//!
//! # What makes this a fair comparison
//!
//! Every arm goes through the same `run_task`, so the TDD invariants (red
//! first, frozen contract tests, green after) are enforced identically and no
//! arm can pass by editing a test. Every arm gets the same per-task config, the
//! same step cap, the same model. The backend is built exactly as the ladder
//! runner (`sc-eval --agent`) builds it -- detected context, native tools -- so
//! the control arm here IS the runner's strategy, not an approximation of it.
//! Task order is fixed and the workspace is rebuilt per task, so the arms
//! cannot contaminate each other.
//!
//! # What it cannot tell you
//!
//! One pass over a ~10-task ladder against a nondeterministic model is a
//! **signal, not a result**. A 1-2 task difference is noise. `--repeat N` runs
//! the whole thing N times so the spread is visible; a difference that does not
//! survive repetition is not a difference.

use std::path::PathBuf;
use std::time::Instant;

use sc_eval::gateway_arm::{build_arm, Arm, AskStats};
use sc_eval::results::{current_commit, write_rows, MetricsSink, ResultRow};
use sc_eval::runner::{run_task, Outcome};
use sc_eval::task::TaskSuite;
use sc_model::OpenAiBackend;

const DEFAULT_ARMS: &str = "control,raw,pi";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "ladder-ab — run the capability ladder once per arm and compare\n\n\
             USAGE:\n  \
               ladder-ab --url <base> --model <name> [--arms a,b,c] [--suite <path>]\n  \
                         [--repeat N] [--steps N] [--out <dir>]\n\n\
             ARMS: control raw pi gateway   (default: {DEFAULT_ARMS})\n\
             The suite defaults to evals/ladder/suite.toml; --out defaults to evals/ladder/results."
        );
        return;
    }

    let url = flag(&args, "--url").unwrap_or_else(|| "http://localhost:11436/v1".to_string());
    let model = flag(&args, "--model").unwrap_or_else(|| "tiel-coder-35b".to_string());
    let repeat: usize = flag(&args, "--repeat")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let steps: Option<usize> = flag(&args, "--steps").and_then(|s| s.parse().ok());
    let suite_path = flag(&args, "--suite")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("evals/ladder/suite.toml"));
    let out_dir = flag(&args, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("evals/ladder/results"));
    let arms = match Arm::parse_list(&flag(&args, "--arms").unwrap_or_else(|| DEFAULT_ARMS.into()))
    {
        Ok(a) => a,
        Err(e) => {
            eprintln!("--arms: {e}");
            std::process::exit(2);
        }
    };

    let suite = match TaskSuite::load(&suite_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot load {}: {e}", suite_path.display());
            std::process::exit(2);
        }
    };

    // The SAME backend the ladder runner builds. A bare `OpenAiBackend::new`
    // assumes an 8192 context and parse-and-repair tool calls -- a different
    // strategy from the one being scored, which made the old control arm a
    // comparison against nothing in particular.
    let backend = OpenAiBackend::new(&url, &model)
        .with_detected_context()
        .with_native_tools();
    let commit = current_commit();
    let labels: Vec<&str> = arms.iter().map(|a| a.label()).collect();
    eprintln!(
        "model {model} at {url}, commit {commit} ({} tasks x {repeat} run(s) x {} arm(s): {})\n",
        suite.tasks.len(),
        arms.len(),
        labels.join(" ")
    );

    let mut rows: Vec<ResultRow> = Vec::new();
    let mut ask_totals = AskStats::default();
    let metrics = MetricsSink::new();

    for round in 1..=repeat {
        for &arm in &arms {
            let mut cfg = sc_core::AgentConfig::default();
            if let Some(n) = steps {
                cfg.max_steps = n;
            }
            let run = build_arm(arm, &backend, &url, &model, cfg, Some(&metrics));
            for task in &suite.tasks {
                metrics.reset();
                let started = Instant::now();
                let result = run_task(task, run.solver.as_ref());
                let wall_ms = started.elapsed().as_millis();
                let observed = metrics.take();
                eprintln!(
                    "  [{round}/{repeat}] {:<12} {:<26} {:<28} {:>6.0}s",
                    arm.label(),
                    task.id,
                    short(&result.outcome),
                    wall_ms as f64 / 1000.0
                );
                rows.push(ResultRow::new(
                    task,
                    arm.label(),
                    &model,
                    &commit,
                    round,
                    &result,
                    &observed,
                    wall_ms,
                ));
            }
            if let Some(stats) = run.ask_stats() {
                merge(&mut ask_totals, stats);
            }
        }
    }

    match write_rows(&out_dir, &rows) {
        Ok(path) => eprintln!("\nrows appended to {}", path.display()),
        Err(e) => eprintln!(
            "\nwarning: could not write rows to {}: {e}",
            out_dir.display()
        ),
    }

    print!("{}", sc_eval::ab_report(&rows, repeat));
    if arms.contains(&Arm::Gateway) {
        print!("{}", ask_report(&ask_totals));
    }
}

/// What `ask` did on the gateway arm, and the reading that goes with it.
fn ask_report(ask: &AskStats) -> String {
    let mut s = String::new();
    s.push_str("\n--- what `ask` did ---\n");
    if ask.calls == 0 {
        s.push_str("  the model never called it\n");
        return s;
    }
    s.push_str(&format!(
        "  {} calls: {} answered ({}%), {} refused\n",
        ask.calls,
        ask.routed,
        ask.answer_percent(),
        ask.refused
    ));
    s.push_str(&format!(
        "  {} -> {} bytes ({}% retained)\n",
        ask.raw_bytes,
        ask.out_bytes,
        ask.retained_percent()
    ));
    let caps: Vec<String> = ask
        .by_capability
        .iter()
        .map(|(n, c)| format!("{n}x{c}"))
        .collect();
    s.push_str(&format!("  capabilities: {}\n", caps.join(" ")));

    // The to-do list. A refusal RATE is a number to shrug at; the phrasings
    // themselves say exactly which capability is missing or too narrow.
    if !ask.refusals.is_empty() {
        s.push_str("\n--- what it could NOT answer ---\n");
        for (need, reason, count) in ask.refusals.iter().take(15) {
            let times = if *count > 1 {
                format!(" (x{count})")
            } else {
                String::new()
            };
            s.push_str(&format!("  {need:?}{times}\n      -> {reason}\n"));
        }
        if ask.refusals.len() > 15 {
            s.push_str(&format!(
                "  ... and {} more distinct needs\n",
                ask.refusals.len() - 15
            ));
        }
    }
    if ask.answer_percent() < 60 {
        s.push_str(&format!(
            "  `ask` refused {}% of calls — a solve rate at this refusal rate is \
             the model working AROUND the gateway, not through it.\n",
            100 - ask.answer_percent()
        ));
    }
    s.push_str("  the gateway arm has no shell; the control arm does (see gateway_arm docs).\n");
    s
}

fn merge(into: &mut AskStats, from: AskStats) {
    into.calls += from.calls;
    into.routed += from.routed;
    into.refused += from.refused;
    into.raw_bytes += from.raw_bytes;
    into.out_bytes += from.out_bytes;
    for (name, n) in from.by_capability {
        match into.by_capability.iter_mut().find(|(m, _)| *m == name) {
            Some((_, c)) => *c += n,
            None => into.by_capability.push((name, n)),
        }
    }
    for (need, reason, n) in from.refusals {
        match into.refusals.iter_mut().find(|(m, _, _)| *m == need) {
            Some((_, _, c)) => *c += n,
            None => into.refusals.push((need, reason, n)),
        }
    }
    into.by_capability
        .sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    into.refusals
        .sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
}

fn short(o: &Outcome) -> String {
    match o {
        Outcome::Pass => "PASS".to_string(),
        Outcome::StillRed => "still red".to_string(),
        Outcome::NotRedFirst => "NOT RED FIRST (fixture bug)".to_string(),
        Outcome::ContractTampered(p) => format!("TAMPERED {p}"),
        Outcome::SolverError(e) => format!("solver error: {}", first_line(e)),
        Outcome::HarnessError(e) => format!("harness error: {}", first_line(e)),
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crates/sc-eval has two ancestors")
        .to_path_buf()
}
