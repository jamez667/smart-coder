//! Aggregate per-task results into a human-readable report. The headline metric
//! is pass rate (pass@1 with a deterministic solver), per spec 07 / spec 11.

use std::fmt::Write as _;

use sc_core::ToolCallMetrics;

use crate::runner::{Outcome, TaskResult};

/// A scored suite run.
pub struct Report {
    pub results: Vec<TaskResult>,
}

impl Report {
    pub fn new(results: Vec<TaskResult>) -> Self {
        Self { results }
    }

    pub fn total(&self) -> usize {
        self.results.len()
    }

    pub fn passed(&self) -> usize {
        self.results.iter().filter(|r| r.outcome.is_pass()).count()
    }

    /// Fraction in [0.0, 1.0]; 0.0 for an empty suite.
    pub fn pass_rate(&self) -> f64 {
        if self.results.is_empty() {
            return 0.0;
        }
        self.passed() as f64 / self.total() as f64
    }

    /// True only if every task passed (the suite's overall gate).
    pub fn all_passed(&self) -> bool {
        !self.results.is_empty() && self.passed() == self.total()
    }

    /// Tool-call metrics summed across every model-driven task in the suite. This
    /// is the source for the M1 ≥95% valid-call rate (spec 07). Tasks with no
    /// metrics (non-agent solvers, or runs that never reached solving) contribute
    /// nothing.
    pub fn tool_call_metrics(&self) -> ToolCallMetrics {
        let mut agg = ToolCallMetrics::default();
        for r in &self.results {
            if let Some(m) = &r.metrics {
                agg.merge(m);
            }
        }
        agg
    }

    /// A multi-line human summary.
    pub fn summary(&self) -> String {
        let mut s = String::new();
        for r in &self.results {
            let detail = match &r.outcome {
                Outcome::ContractTampered(p) => format!(" ({p})"),
                Outcome::SolverError(m) | Outcome::HarnessError(m) => format!(" ({m})"),
                _ => String::new(),
            };
            let _ = writeln!(s, "  [{}] {}{}", r.outcome.symbol(), r.id, detail);
            // On anything but a pass, say what the run actually did. A bare
            // STILL-RED reads the same whether the model exhausted its steps,
            // stopped early believing it was done, or edited confidently and got
            // the logic wrong -- three problems with three different fixes.
            if !matches!(r.outcome, Outcome::Pass) {
                if let Some(run) = &r.run {
                    let selfv = match run.self_verified {
                        Some(true) => ", agent thought it was GREEN",
                        Some(false) => ", agent knew it was red",
                        None => "",
                    };
                    let _ = writeln!(
                        s,
                        "         {} steps, stopped: {}{}{}",
                        run.steps,
                        run.stop_reason,
                        selfv,
                        if run.interventions > 0 {
                            format!(", {} interventions", run.interventions)
                        } else {
                            String::new()
                        }
                    );
                }
            }
        }
        let _ = write!(
            s,
            "{}/{} passed ({:.0}%)",
            self.passed(),
            self.total(),
            self.pass_rate() * 100.0
        );
        // Surface the M1 tool-call validity rate when there's anything to report.
        let m = self.tool_call_metrics();
        if m.total() > 0 {
            let _ = write!(
                s,
                "\ntool calls: {}/{} valid ({:.1}%)",
                m.valid,
                m.total(),
                m.valid_rate() * 100.0
            );
        }
        // The largest reply any task produced, against the reserve set aside for it.
        //
        // `response_reserve_tokens` is subtracted from the prompt budget EVERY turn,
        // so an over-generous reserve costs context on every request of the run. It
        // is a per-model number and was previously invisible: on the same ten rungs
        // one model peaked at 1,328 tokens and another at 14,202, both charged the
        // same 12,288.
        if let Some(peak) = self
            .results
            .iter()
            .filter_map(|r| r.run.as_ref().map(|x| x.peak_reply_tokens))
            .max()
        {
            if peak > 0 {
                let _ = write!(s, "\nlargest reply: {peak} tokens");
            }
        }

        // HARNESS FAULTS, loudly.
        //
        // A run that degraded its own input must not print like a clean one. These
        // events existed from the start and were reachable only by parsing an NDJSON
        // log by hand, so a suite with sixteen truncated replies -- every one a lost
        // turn -- printed exactly like a suite with none. Aggregated across tasks and
        // marked, because the whole point is that it should be hard to miss.
        let mut totals: Vec<(sc_core::FaultKind, usize)> = Vec::new();
        for r in &self.results {
            for (kind, n) in r.run.iter().flat_map(|x| x.harness_faults.iter()) {
                match totals.iter_mut().find(|(k, _)| k == kind) {
                    Some((_, t)) => *t += n,
                    None => totals.push((*kind, *n)),
                }
            }
        }
        if !totals.is_empty() {
            totals.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            let total: usize = totals.iter().map(|(_, n)| n).sum();
            let _ = write!(
                s,
                "\n\n!! {total} HARNESS FAULT(S) - this run was degraded:"
            );
            for (kind, n) in &totals {
                let _ = write!(s, "\n     {n:>3}x {}", kind.label());
            }
            let _ = write!(
                s,
                "\n   A fault means the HARNESS damaged the model's input or output. \
                 Treat the scores above as a lower bound until these are gone."
            );
        }
        s
    }
}

// ---------------------------------------------------------------------------
// The ladder A/B report: N arms side by side, from the rows they produced.
// ---------------------------------------------------------------------------

use crate::results::ResultRow;

/// Distinct values of `key` over `rows`, in first-appearance order.
fn distinct<'a>(rows: &'a [ResultRow], key: impl Fn(&'a ResultRow) -> &'a str) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for r in rows {
        let k = key(r);
        if !out.contains(&k) {
            out.push(k);
        }
    }
    out
}

/// (solved, total) over `rows`.
fn solved<'a>(rows: impl Iterator<Item = &'a ResultRow>) -> (usize, usize) {
    let mut n = 0;
    let mut s = 0;
    for r in rows {
        n += 1;
        if r.is_pass() {
            s += 1;
        }
    }
    (s, n)
}

/// Prompt tokens summed across runs, and how many runs reported them.
///
/// A run that errored before its first turn reports nothing, and averaging over
/// it would flatter whichever arm failed earliest.
fn tokens<'a>(rows: impl Iterator<Item = &'a ResultRow>) -> (usize, usize) {
    let counted: Vec<usize> = rows
        .map(|r| r.total_prompt_tokens)
        .filter(|t| *t > 0)
        .collect();
    (counted.iter().sum(), counted.len())
}

fn pct(n: usize, of: usize) -> u32 {
    if of == 0 {
        return 0;
    }
    ((n as f64 / of as f64) * 100.0).round() as u32
}

/// The rows of one arm.
fn by_arm<'a>(rows: &'a [ResultRow], arm: &'a str) -> impl Iterator<Item = &'a ResultRow> + 'a {
    rows.iter().filter(move |r| r.arm == arm)
}

/// The rung a row belongs to, for grouping.
fn rung(r: &ResultRow) -> &str {
    r.rung.as_deref().unwrap_or("(untagged)")
}

/// The scorecard for an N-arm run: solve rate per arm, solve rate per rung and
/// arm, context cost per arm and per task, the process metrics, and the caveats
/// the numbers do not carry on their own.
///
/// Built from [`ResultRow`]s rather than `TaskResult`s so the printed report and
/// the `rows.jsonl` it sits beside are the same numbers by construction.
pub fn ab_report(rows: &[ResultRow], repeat: usize) -> String {
    let mut s = String::new();
    let arms = distinct(rows, |r| r.arm.as_str());
    let tasks = distinct(rows, |r| r.task.as_str());
    let rungs = distinct(rows, rung);
    let width = arms.iter().map(|a| a.len()).max().unwrap_or(4).max(4);

    let _ = writeln!(s, "\n=== ladder A/B ===\n");
    let _ = writeln!(
        s,
        "{:<width$} {:>7} {:>7} {:>6}",
        "arm", "solved", "of", "rate"
    );
    for arm in &arms {
        let (p, n) = solved(by_arm(rows, arm));
        let _ = writeln!(s, "{arm:<width$} {p:>7} {n:>7} {:>5}%", pct(p, n));
    }

    // Per rung: the rung IS the measurement. A total says one arm scored 6/10;
    // this says which rung it fell off, and whether the others fell off the same
    // one.
    if rungs.len() > 1 || rungs.first().is_some_and(|r| *r != "(untagged)") {
        let rw = rungs.iter().map(|r| r.len()).max().unwrap_or(4).max(4);
        let _ = write!(
            s,
            "\n--- per rung (solved/total across {repeat} round(s)) ---\n"
        );
        let _ = write!(s, "{:<rw$}", "rung");
        for arm in &arms {
            let _ = write!(s, " {arm:>width$}");
        }
        let _ = writeln!(s);
        for rg in &rungs {
            let _ = write!(s, "{rg:<rw$}");
            for arm in &arms {
                let (p, n) = solved(by_arm(rows, arm).filter(|r| rung(r) == *rg));
                let cell = format!("{p}/{n}");
                let _ = write!(s, " {cell:>width$}");
            }
            let _ = writeln!(s);
        }
    }

    // **The measurement this A/B can actually make.** Solve rate needs a ladder
    // that discriminates; context cost does not — every run consumes tokens
    // whether it succeeds or fails, so the number means something even when the
    // arms score identically.
    let _ = write!(
        s,
        "\n--- context cost (prompt tokens summed over every turn) ---\n"
    );
    let _ = writeln!(
        s,
        "{:<width$} {:>12} {:>12} {:>10}",
        "arm", "total", "per task", "vs first"
    );
    let (first_total, _) = arms
        .first()
        .map(|a| tokens(by_arm(rows, a)))
        .unwrap_or((0, 0));
    for arm in &arms {
        let (t, n) = tokens(by_arm(rows, arm));
        let vs = if first_total > 0 && t > 0 {
            format!("{:.0}%", (t as f64 / first_total as f64) * 100.0)
        } else {
            "-".to_string()
        };
        let _ = writeln!(
            s,
            "{arm:<width$} {t:>12} {:>12} {vs:>10}",
            t.checked_div(n).unwrap_or(0)
        );
    }
    // Per task, so one runaway task cannot masquerade as a trend.
    if tasks.len() > 1 {
        let tw = tasks.iter().map(|t| t.len()).max().unwrap_or(4).max(4);
        let _ = write!(s, "\n  {:<tw$}", "task");
        for arm in &arms {
            let _ = write!(s, " {arm:>width$}");
        }
        let _ = writeln!(s);
        for task in &tasks {
            let _ = write!(s, "  {task:<tw$}");
            for arm in &arms {
                let (t, _) = tokens(by_arm(rows, arm).filter(|r| r.task == *task));
                let _ = write!(s, " {t:>width$}");
            }
            let _ = writeln!(s);
        }
    }

    // **Prefix-cache reuse: the number the token totals above cannot show.**
    //
    // `total` is what the harness SENT. Keeping the prompt prefix byte-stable
    // between turns does not change that by a single token -- what it changes is
    // how much of it the server has to RE-PREFILL, and that only appears here. A
    // high hit rate means the append-only prompt is reaching the server intact; a
    // rate near 0 on a multi-turn task means it is not, however stable the prefix
    // looked on our side.
    //
    // Only arms whose backend reports the split appear; the rest say "not
    // reported", because a zero here would be a claim about the cache rather than
    // about the reporting.
    let _ = write!(
        s,
        "\n--- prefix cache (what the SERVER re-prefilled, vs what we sent) ---\n"
    );
    let _ = writeln!(
        s,
        "{:<width$} {:>12} {:>12} {:>12} {:>9}",
        "arm", "sent", "prefilled", "cached", "hit rate"
    );
    let mut any_reported = false;
    for arm in &arms {
        let rs: Vec<&ResultRow> = by_arm(rows, arm).collect();
        let (sent, _) = tokens(rs.iter().copied());
        let reported: Vec<&&ResultRow> = rs
            .iter()
            .filter(|r| r.cache_hit_percent.is_some())
            .collect();
        if reported.is_empty() {
            let _ = writeln!(
                s,
                "{arm:<width$} {sent:>12} {:>12} {:>12} {:>9}",
                "-", "-", "not reported"
            );
            continue;
        }
        any_reported = true;
        let prefilled: usize = reported.iter().map(|r| r.prefilled_prompt_tokens).sum();
        let cached: usize = reported.iter().map(|r| r.cached_prompt_tokens).sum();
        let hit = format!("{}%", pct(cached, cached + prefilled));
        let _ = writeln!(
            s,
            "{arm:<width$} {sent:>12} {prefilled:>12} {cached:>12} {hit:>9}"
        );
    }
    if any_reported {
        let _ = writeln!(
            s,
            "  a HIGH hit rate is the append-only prompt working: only the newly \
             appended tokens\n  were prefilled. A rate near 0% on a multi-turn task \
             means the prefix is NOT holding."
        );
    }

    // How each arm got there. A solve rate cannot see a model that reads for
    // twenty turns before its first edit, or one that re-reads what the harness
    // just evicted.
    let _ = write!(s, "\n--- process (per task, averaged) ---\n");
    let _ = writeln!(
        s,
        "{:<width$} {:>6} {:>11} {:>9} {:>7} {:>8}",
        "arm", "steps", "first edit", "re-reads", "wasted", "secs"
    );
    for arm in &arms {
        let rs: Vec<&ResultRow> = by_arm(rows, arm).filter(|r| r.steps > 0).collect();
        if rs.is_empty() {
            let _ = writeln!(s, "{arm:<width$} {:>6}", "-");
            continue;
        }
        let n = rs.len() as f64;
        let steps = rs.iter().map(|r| r.steps).sum::<usize>() as f64 / n;
        let edits: Vec<usize> = rs.iter().filter_map(|r| r.turns_to_first_edit).collect();
        let first_edit = if edits.is_empty() {
            "never".to_string()
        } else {
            format!(
                "{:.1}",
                edits.iter().sum::<usize>() as f64 / edits.len() as f64
            )
        };
        let re_reads = rs.iter().map(|r| r.re_reads).sum::<usize>() as f64 / n;
        let wasted = rs.iter().map(|r| r.wasted_turns).sum::<usize>() as f64 / n;
        let secs = rs.iter().map(|r| r.wall_ms).sum::<u128>() as f64 / 1000.0 / n;
        let _ = writeln!(
            s,
            "{arm:<width$} {steps:>6.1} {first_edit:>11} {re_reads:>9.1} {wasted:>7.1} {secs:>8.0}"
        );
    }

    // The interpretation the numbers do NOT carry on their own.
    let _ = write!(s, "\n--- reading this ---\n");
    if arms.len() > 1 {
        let counts: Vec<usize> = arms.iter().map(|a| solved(by_arm(rows, a)).0).collect();
        let spread = counts.iter().max().unwrap_or(&0) - counts.iter().min().unwrap_or(&0);
        if spread <= 1 && repeat == 1 {
            let _ = writeln!(
                s,
                "  a {spread}-task spread over one pass is NOISE, not a result. Re-run with \
                 --repeat 3 before concluding anything."
            );
        }
        // A task every arm always solves, or always fails, carries no information —
        // and a ladder mostly made of those cannot detect a difference at all.
        let flat = tasks
            .iter()
            .filter(|task| {
                let per_arm: Vec<(usize, usize)> = arms
                    .iter()
                    .map(|a| solved(by_arm(rows, a).filter(|r| r.task == **task)))
                    .collect();
                per_arm.iter().all(|(p, n)| *p == 0 || p == n)
                    && per_arm.windows(2).all(|w| (w[0].0 == 0) == (w[1].0 == 0))
            })
            .count();
        let _ = writeln!(
            s,
            "  {flat} of {} tasks scored identically on every arm every round — \
             only the remaining {} could show a difference at all.",
            tasks.len(),
            tasks.len() - flat
        );
    }
    s
}

#[cfg(test)]
mod ab_tests {
    use super::*;

    fn row(task: &str, arm: &str, rung: Option<&str>, pass: bool, tokens: usize) -> ResultRow {
        ResultRow {
            task: task.into(),
            arm: arm.into(),
            model: "m".into(),
            commit: "c".into(),
            tree_commit: None,
            repeat: 1,
            outcome: if pass { "PASS" } else { "STILL-RED" }.into(),
            steps: 5,
            total_prompt_tokens: tokens,
            cached_prompt_tokens: tokens * 8 / 10,
            prefilled_prompt_tokens: tokens * 2 / 10,
            cache_hit_percent: Some(80),
            peak_prompt_tokens: tokens / 2,
            peak_reply_tokens: 100,
            wall_ms: 2000,
            faults: Vec::new(),
            interventions: 0,
            turns_to_first_edit: Some(2),
            re_reads: 1,
            wasted_turns: 0,
            rung: rung.map(str::to_string),
        }
    }

    #[test]
    fn the_scorecard_has_one_line_per_arm_in_first_seen_order() {
        let rows = vec![
            row("a", "control(6)", Some("stated"), true, 1000),
            row("a", "raw", Some("stated"), false, 500),
            row("a", "pi", Some("stated"), true, 700),
        ];
        let s = ab_report(&rows, 1);
        let control = s.find("control(6)").unwrap();
        let raw = s.find("\nraw").unwrap();
        let pi = s.find("\npi").unwrap();
        assert!(control < raw && raw < pi, "{s}");
        assert!(s.contains("100%"), "{s}");
    }

    #[test]
    fn the_rung_table_groups_tasks_by_tag_and_columns_by_arm() {
        let rows = vec![
            row("a", "control(6)", Some("stated"), true, 1000),
            row("b", "control(6)", Some("located"), false, 1000),
            row("a", "raw", Some("stated"), false, 1000),
            row("b", "raw", Some("located"), false, 1000),
        ];
        let s = ab_report(&rows, 1);
        assert!(s.contains("per rung"), "{s}");
        let stated = s
            .lines()
            .find(|l| l.starts_with("stated"))
            .expect("stated row");
        assert!(stated.contains("1/1") && stated.contains("0/1"), "{stated}");
        let located = s
            .lines()
            .find(|l| l.starts_with("located"))
            .expect("located row");
        assert_eq!(located.matches("0/1").count(), 2, "{located}");
    }

    #[test]
    fn context_cost_is_relative_to_the_first_arm() {
        let rows = vec![
            row("a", "control(6)", None, true, 1000),
            row("a", "gateway(5)", None, true, 500),
        ];
        let s = ab_report(&rows, 1);
        let cost = s
            .split("context cost")
            .nth(1)
            .expect("context cost section");
        let gw = cost.lines().find(|l| l.starts_with("gateway(5)")).unwrap();
        assert!(gw.contains("50%"), "{gw}");
        // No rung tags anywhere: no rung table.
        assert!(!s.contains("per rung"), "{s}");
    }

    #[test]
    fn a_one_pass_tie_is_called_noise_and_flat_tasks_are_counted() {
        let rows = vec![
            row("a", "control(6)", None, true, 1000),
            row("b", "control(6)", None, false, 1000),
            row("a", "raw", None, true, 1000),
            row("b", "raw", None, true, 1000),
        ];
        let s = ab_report(&rows, 1);
        assert!(s.contains("NOISE"), "{s}");
        assert!(s.contains("1 of 2 tasks scored identically"), "{s}");
    }

    #[test]
    fn an_empty_run_does_not_panic() {
        let s = ab_report(&[], 1);
        assert!(s.contains("ladder A/B"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn res(id: &str, outcome: Outcome) -> TaskResult {
        TaskResult {
            id: id.into(),
            solver: "t".into(),
            outcome,
            metrics: None,
            run: None,
        }
    }

    fn res_with(id: &str, outcome: Outcome, metrics: ToolCallMetrics) -> TaskResult {
        TaskResult {
            id: id.into(),
            solver: "agent".into(),
            outcome,
            metrics: Some(metrics),
            run: None,
        }
    }

    #[test]
    fn computes_pass_rate_and_gate() {
        let report = Report::new(vec![res("a", Outcome::Pass), res("b", Outcome::StillRed)]);
        assert_eq!(report.total(), 2);
        assert_eq!(report.passed(), 1);
        assert!((report.pass_rate() - 0.5).abs() < f64::EPSILON);
        assert!(!report.all_passed());
    }

    #[test]
    fn all_passed_requires_nonempty_and_full() {
        assert!(!Report::new(vec![]).all_passed());
        assert!(Report::new(vec![res("a", Outcome::Pass)]).all_passed());
    }

    #[test]
    fn aggregates_tool_call_metrics_across_the_suite() {
        let report = Report::new(vec![
            res_with(
                "a",
                Outcome::Pass,
                ToolCallMetrics {
                    valid: 9,
                    invalid: 1,
                },
            ),
            res_with(
                "b",
                Outcome::Pass,
                ToolCallMetrics {
                    valid: 10,
                    invalid: 0,
                },
            ),
            res("c", Outcome::StillRed), // no metrics — contributes nothing
        ]);
        let m = report.tool_call_metrics();
        assert_eq!(m.total(), 20);
        assert_eq!(m.valid, 19);
        assert!((m.valid_rate() - 0.95).abs() < f64::EPSILON);
        assert!(report.summary().contains("19/20 valid"));
    }

    #[test]
    fn summary_mentions_each_task() {
        let report = Report::new(vec![
            res("alpha", Outcome::Pass),
            res("beta", Outcome::ContractTampered("test.sh".into())),
        ]);
        let s = report.summary();
        assert!(s.contains("alpha"));
        assert!(s.contains("beta"));
        assert!(s.contains("test.sh"));
        assert!(s.contains("1/2 passed"));
    }
}
