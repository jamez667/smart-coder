//! The gateway benchmark: does one door actually beat a menu?
//!
//! Modelled on the retrieval eval (`sc-eval::retrieval`), for the same reason it
//! was built that way: routing is a pure function of the need text and the
//! capability table, so "did routing get worse?" is a question a unit test can
//! answer on any machine in milliseconds — no model, no GPU, no flakiness
//! allowance. A vocabulary change that would otherwise be an unfalsifiable vibe
//! ("it feels like it misroutes more now") becomes a red build naming the need.
//!
//! # What is graded
//!
//! Three things, kept separate because they fail independently and a single
//! blended score would hide which one moved:
//!
//! * **Routing** — right capability, wrong one, or a refusal. These are not
//!   equally bad, and the scoring says so (see [`Verdict`]).
//! * **Reduction** — bytes the capability produced vs. bytes the model sees.
//! * **Sufficiency** — whether the reduced text still contains what the caller
//!   needed. A reduction score without this one rewards deleting everything.
//!
//! # The rule that keeps it honest
//!
//! A suite of only-winnable needs measures nothing and quietly becomes a suite
//! somebody tuned the table against. So a case may declare `expect = "refuse"`
//! — a need the gateway is *supposed* to decline — and those count as passes
//! only when it actually declines. The day one starts routing, the suite goes
//! red, because a refusal turning into an answer is news either way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Ctx, Gateway, Level, Need};

/// What a case asserts should happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// Route to the named capability.
    Route,
    /// Refuse — the need is genuinely ambiguous or unanswerable.
    ///
    /// A first-class expectation, not a failure. The classifier's central
    /// promise is that it declines rather than guessing, and a promise nothing
    /// tests is a promise that erodes.
    Refuse,
}

/// How a case turned out. Ordered by how bad it is, worst last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// Routed to the expected capability, or refused as expected.
    Correct,
    /// Refused a need that should have routed.
    ///
    /// A miss, not a disaster: the caller sees "cannot answer that" and can
    /// rephrase. Costs a turn.
    OverRefused,
    /// Routed a need that should have been refused.
    ///
    /// Worse than over-refusing: the caller gets a confident answer from a
    /// capability nobody chose deliberately.
    UnderRefused,
    /// Routed to the WRONG capability.
    ///
    /// The failure this whole design exists to prevent. The caller gets a
    /// plausible answer from the wrong source and has no way to tell — there is
    /// no error to recover from, so it is scored as the worst outcome.
    Misrouted,
}

impl Verdict {
    /// Did this case pass?
    pub fn passed(self) -> bool {
        self == Verdict::Correct
    }

    /// Short label for a report line.
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Correct => "OK  ",
            Verdict::OverRefused => "MISS",
            Verdict::UnderRefused => "LOOSE",
            Verdict::Misrouted => "WRONG",
        }
    }
}

/// One benchmark case: a need, and what should happen to it.
#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    pub id: String,
    /// The need, phrased the way a caller would actually type it.
    pub need: String,
    /// Optional path scope, as the harness would supply it.
    #[serde(default)]
    pub scope: Option<String>,
    /// Fixture directory, relative to the suite file.
    pub fixture: String,
    /// The capability that should answer. Absent when `expect = "refuse"`.
    #[serde(default)]
    pub capability: Option<String>,
    /// `"refuse"` marks a need the gateway is expected to decline.
    #[serde(default)]
    pub expect: Option<String>,
    /// Substrings that MUST survive into the answer the model sees.
    ///
    /// This is the guard on the reduction score: without it, a simplifier that
    /// returns the empty string scores a perfect 0% retained.
    #[serde(default)]
    pub must_contain: Vec<String>,
    /// Substrings that must NOT survive — noise the simplifier should remove.
    #[serde(default)]
    pub must_not_contain: Vec<String>,
}

impl Case {
    fn expectation(&self) -> Expect {
        match self.expect.as_deref() {
            Some("refuse") => Expect::Refuse,
            _ => Expect::Route,
        }
    }
}

/// The parsed suite.
#[derive(Debug, Clone, Deserialize)]
pub struct BenchSuite {
    pub cases: Vec<Case>,
    #[serde(skip)]
    dir: PathBuf,
}

impl BenchSuite {
    /// Load a suite from TOML. Fixture paths inside resolve relative to the
    /// file, so the suite runs from any working directory.
    pub fn load(path: &Path) -> Result<BenchSuite, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut suite: BenchSuite =
            toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
        suite.dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        Ok(suite)
    }

    /// Run every case. No model, no network — the gateway is constructed with
    /// neither, so any case needing one is graded on its refusal.
    pub fn run(&self) -> Vec<CaseResult> {
        let gw = Gateway::new().with_level(Level::Extract);
        self.cases.iter().map(|c| self.grade(&gw, c)).collect()
    }

    fn grade(&self, gw: &Gateway, case: &Case) -> CaseResult {
        let fixture = self.dir.join(&case.fixture);
        if !fixture.is_dir() {
            return CaseResult::unrunnable(case, format!("no such fixture: {}", fixture.display()));
        }

        let need = match &case.scope {
            Some(s) => Need::scoped(&case.need, s),
            None => Need::new(&case.need),
        };
        let ctx = Ctx {
            workspace: &fixture,
            model: None,
            web: None,
            verify: None,
        };
        let answer = gw.ask_with(&need, &ctx);

        let routed = answer.trace.route.clone();
        let verdict = match (case.expectation(), &routed) {
            (Expect::Refuse, None) => Verdict::Correct,
            (Expect::Refuse, Some(_)) => Verdict::UnderRefused,
            (Expect::Route, None) => Verdict::OverRefused,
            (Expect::Route, Some(got)) => {
                if case.capability.as_deref() == Some(got.as_str()) {
                    Verdict::Correct
                } else {
                    Verdict::Misrouted
                }
            }
        };

        // Sufficiency is only meaningful when something was actually answered.
        let (missing, leaked) = if routed.is_some() {
            (
                case.must_contain
                    .iter()
                    .filter(|s| !answer.text.contains(s.as_str()))
                    .cloned()
                    .collect(),
                case.must_not_contain
                    .iter()
                    .filter(|s| answer.text.contains(s.as_str()))
                    .cloned()
                    .collect(),
            )
        } else {
            (Vec::new(), Vec::new())
        };

        CaseResult {
            id: case.id.clone(),
            verdict,
            expected: case.capability.clone(),
            routed,
            raw_bytes: answer.trace.raw_bytes,
            out_bytes: answer.trace.out_bytes,
            missing,
            leaked,
            note: None,
        }
    }
}

/// What one case did.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseResult {
    pub id: String,
    pub verdict: Verdict,
    pub expected: Option<String>,
    pub routed: Option<String>,
    pub raw_bytes: usize,
    pub out_bytes: usize,
    /// Required substrings the answer lost. Non-empty means the reduction ate
    /// something the caller needed.
    pub missing: Vec<String>,
    /// Noise substrings that survived.
    pub leaked: Vec<String>,
    /// Set when the case could not run at all.
    pub note: Option<String>,
}

impl CaseResult {
    fn unrunnable(case: &Case, note: String) -> CaseResult {
        CaseResult {
            id: case.id.clone(),
            verdict: Verdict::OverRefused,
            expected: case.capability.clone(),
            routed: None,
            raw_bytes: 0,
            out_bytes: 0,
            missing: Vec::new(),
            leaked: Vec::new(),
            note: Some(note),
        }
    }

    /// A case passes only if it routed correctly AND kept what was needed.
    ///
    /// Both halves matter: routing to the right capability and then handing back
    /// a gutted answer is not a success, and neither is a perfectly complete
    /// answer from the wrong place.
    pub fn passed(&self) -> bool {
        self.verdict.passed() && self.missing.is_empty() && self.leaked.is_empty()
    }

    /// A one-line report: what happened, and what was expected instead.
    pub fn line(&self) -> String {
        let what = match (&self.note, self.verdict, &self.routed) {
            (Some(n), _, _) => n.clone(),
            (None, Verdict::Correct, Some(r)) => {
                format!(
                    "{r} ({}% of {} bytes)",
                    self.retained_percent(),
                    self.raw_bytes
                )
            }
            (None, Verdict::Correct, None) => "refused, as declared".to_string(),
            (None, Verdict::Misrouted, Some(r)) => {
                format!(
                    "routed {r}, wanted {}",
                    self.expected.as_deref().unwrap_or("?")
                )
            }
            (None, Verdict::OverRefused, _) => format!(
                "refused, wanted {}",
                self.expected.as_deref().unwrap_or("?")
            ),
            (None, Verdict::UnderRefused, Some(r)) => format!("routed {r}, wanted a refusal"),
            (None, _, None) => "no route".to_string(),
        };
        let mut line = format!("{} {:<28} {what}", self.verdict.label(), self.id);
        if !self.missing.is_empty() {
            line.push_str(&format!("  LOST {:?}", self.missing));
        }
        if !self.leaked.is_empty() {
            line.push_str(&format!("  KEPT-NOISE {:?}", self.leaked));
        }
        line
    }

    /// How much of the raw output survived to the model.
    pub fn retained_percent(&self) -> u32 {
        if self.raw_bytes == 0 {
            return 100;
        }
        ((self.out_bytes as f64 / self.raw_bytes as f64) * 100.0).round() as u32
    }
}

/// Aggregate scores across a run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scorecard {
    pub total: usize,
    pub passed: usize,
    /// Cases by verdict, so a regression says WHICH way it moved.
    pub verdicts: BTreeMap<&'static str, usize>,
    pub raw_bytes: usize,
    pub out_bytes: usize,
    /// Cases where the reduction dropped something required.
    pub lossy: usize,
}

impl Scorecard {
    pub fn of(results: &[CaseResult]) -> Scorecard {
        let mut card = Scorecard {
            total: results.len(),
            ..Scorecard::default()
        };
        for r in results {
            if r.passed() {
                card.passed += 1;
            }
            *card.verdicts.entry(r.verdict.label().trim()).or_insert(0) += 1;
            card.raw_bytes += r.raw_bytes;
            card.out_bytes += r.out_bytes;
            if !r.missing.is_empty() {
                card.lossy += 1;
            }
        }
        card
    }

    /// Routing accuracy as a percentage of cases.
    pub fn routing_percent(&self) -> u32 {
        if self.total == 0 {
            return 100;
        }
        ((self.passed as f64 / self.total as f64) * 100.0).round() as u32
    }

    /// Bytes retained across every case that produced output.
    pub fn retained_percent(&self) -> u32 {
        if self.raw_bytes == 0 {
            return 100;
        }
        ((self.out_bytes as f64 / self.raw_bytes as f64) * 100.0).round() as u32
    }

    /// The headline, printed by the bench test.
    pub fn summary(&self) -> String {
        let breakdown: Vec<String> = self
            .verdicts
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        format!(
            "routing {}% ({}/{})  |  bytes {} -> {} ({}% retained)  |  lossy {}  |  {}",
            self.routing_percent(),
            self.passed,
            self.total,
            self.raw_bytes,
            self.out_bytes,
            self.retained_percent(),
            self.lossy,
            breakdown.join(" ")
        )
    }
}

// ---------------------------------------------------------------------------
// The reduction benchmark.
//
// Separate from the routing suite for a reason the first run made obvious: the
// routing fixtures are small, clean files, so every case reported 100% retained
// and the reduction column measured nothing. Extraction only fires on output
// that CARRIES waste — a real failing test run, a real build with warnings.
//
// So these cases are captured from actual cargo runs rather than written by
// hand. A hand-written "messy output" fixture measures the fixture author's
// idea of mess, which is exactly the fixture the extractor would be tuned to.
// ---------------------------------------------------------------------------

/// One captured output sample and what must survive reducing it.
#[derive(Debug, Clone, Deserialize)]
pub struct OutputCase {
    pub id: String,
    /// File under the suite's `output/` directory.
    pub file: String,
    /// Facts the model still needs after reduction. The guard that stops the
    /// score being gamed by returning less.
    #[serde(default)]
    pub must_contain: Vec<String>,
    /// Noise that must be gone.
    #[serde(default)]
    pub must_not_contain: Vec<String>,
    /// The most that may survive, as a percentage. A ceiling rather than a
    /// target: beating it is good news, missing it means the extractor stopped
    /// recognising this shape.
    pub max_retained_percent: u32,
}

/// The parsed reduction suite.
#[derive(Debug, Clone, Deserialize)]
pub struct OutputSuite {
    pub outputs: Vec<OutputCase>,
    #[serde(skip)]
    dir: PathBuf,
}

impl OutputSuite {
    pub fn load(path: &Path) -> Result<OutputSuite, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut suite: OutputSuite =
            toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
        suite.dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        Ok(suite)
    }

    /// Reduce every captured sample. Extraction only — the lossy summarizer
    /// needs a model, and a benchmark that needs one cannot be a build gate.
    pub fn run(&self) -> Vec<OutputResult> {
        self.outputs
            .iter()
            .map(|c| {
                let path = self.dir.join("output").join(&c.file);
                let Ok(raw) = std::fs::read_to_string(&path) else {
                    return OutputResult::unreadable(c, format!("cannot read {}", path.display()));
                };
                let mut trace = crate::Trace::default();
                let out = crate::simplify(&raw, Level::Extract, &mut trace, None);
                OutputResult {
                    id: c.id.clone(),
                    raw_bytes: trace.raw_bytes,
                    out_bytes: trace.out_bytes,
                    ceiling: c.max_retained_percent,
                    dropped: trace.dropped,
                    missing: c
                        .must_contain
                        .iter()
                        .filter(|s| !out.contains(s.as_str()))
                        .cloned()
                        .collect(),
                    leaked: c
                        .must_not_contain
                        .iter()
                        .filter(|s| out.contains(s.as_str()))
                        .cloned()
                        .collect(),
                    note: None,
                }
            })
            .collect()
    }
}

/// What one captured sample reduced to.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputResult {
    pub id: String,
    pub raw_bytes: usize,
    pub out_bytes: usize,
    pub ceiling: u32,
    /// What the simplifier said it removed — named, never silent.
    pub dropped: Vec<String>,
    pub missing: Vec<String>,
    pub leaked: Vec<String>,
    pub note: Option<String>,
}

impl OutputResult {
    fn unreadable(c: &OutputCase, note: String) -> OutputResult {
        OutputResult {
            id: c.id.clone(),
            raw_bytes: 0,
            out_bytes: 0,
            ceiling: c.max_retained_percent,
            dropped: Vec::new(),
            missing: Vec::new(),
            leaked: Vec::new(),
            note: Some(note),
        }
    }

    pub fn retained_percent(&self) -> u32 {
        if self.raw_bytes == 0 {
            return 100;
        }
        ((self.out_bytes as f64 / self.raw_bytes as f64) * 100.0).round() as u32
    }

    /// Passes when it shrank enough AND kept everything required.
    ///
    /// Both halves, always together: shrinking is only a win if the answer
    /// survived, and a complete answer that never shrank is the problem this
    /// crate was built to fix.
    pub fn passed(&self) -> bool {
        self.note.is_none()
            && self.retained_percent() <= self.ceiling
            && self.missing.is_empty()
            && self.leaked.is_empty()
    }

    pub fn line(&self) -> String {
        if let Some(n) = &self.note {
            return format!("FAIL {:<24} {n}", self.id);
        }
        let mark = if self.passed() { "OK  " } else { "FAIL" };
        let mut line = format!(
            "{mark} {:<24} {:>6} -> {:>6} bytes ({:>3}%, ceiling {}%)  dropped {:?}",
            self.id,
            self.raw_bytes,
            self.out_bytes,
            self.retained_percent(),
            self.ceiling,
            self.dropped
        );
        if !self.missing.is_empty() {
            line.push_str(&format!("  LOST {:?}", self.missing));
        }
        if !self.leaked.is_empty() {
            line.push_str(&format!("  KEPT-NOISE {:?}", self.leaked));
        }
        line
    }
}

// ---------------------------------------------------------------------------
// The captured-needs replay: the refusal rate, without a GPU.
//
// Measuring "can the classifier route what a model actually types" used to mean
// a 40-minute A/B against a live 35B. The last such run produced NO data:
// back-to-back runs degraded throughput to 11 tok/s, requests timed out, and
// the container was restarted mid-suite by autoheal.
//
// These are the SAME needs, captured verbatim from those runs, replayed against
// the classifier alone. Deterministic, instant, no model — so the refusal rate
// becomes a build gate rather than an expedition.
// ---------------------------------------------------------------------------

/// One captured need and what should happen to it now.
#[derive(Debug, Clone, Deserialize)]
pub struct CapturedNeed {
    pub text: String,
    /// `"route"` (the classifier should now handle it) or `"refuse"`.
    pub expect: String,
    /// The capability it should reach, when `expect = "route"`.
    #[serde(default)]
    pub capability: Option<String>,
}

/// The parsed capture file.
#[derive(Debug, Clone, Deserialize)]
pub struct CapturedSuite {
    pub needs: Vec<CapturedNeed>,
}

impl CapturedSuite {
    pub fn load(path: &Path) -> Result<CapturedSuite, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Classify every captured need. No workspace is touched: this asks only
    /// whether the need ROUTES, which is a pure function of the text and table.
    pub fn run(&self) -> Vec<CapturedResult> {
        let table = crate::table();
        self.needs
            .iter()
            .map(|n| {
                let routed = match crate::classify(&Need::new(&n.text), &table) {
                    crate::Route::To { index, .. } => Some(table[index].name.to_string()),
                    crate::Route::Unknown { trace } => {
                        return CapturedResult {
                            text: n.text.clone(),
                            expected: n.capability.clone(),
                            routed: None,
                            verdict: if n.expect == "refuse" {
                                Verdict::Correct
                            } else {
                                Verdict::OverRefused
                            },
                            reason: trace.reason,
                        }
                    }
                };
                let verdict = if n.expect == "refuse" {
                    Verdict::UnderRefused
                } else if n.capability == routed {
                    Verdict::Correct
                } else {
                    Verdict::Misrouted
                };
                CapturedResult {
                    text: n.text.clone(),
                    expected: n.capability.clone(),
                    routed,
                    verdict,
                    reason: String::new(),
                }
            })
            .collect()
    }
}

/// What one captured need did.
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedResult {
    pub text: String,
    pub expected: Option<String>,
    pub routed: Option<String>,
    pub verdict: Verdict,
    pub reason: String,
}

impl CapturedResult {
    pub fn passed(&self) -> bool {
        self.verdict.passed()
    }

    pub fn line(&self) -> String {
        let what = match (&self.routed, self.verdict) {
            (Some(r), Verdict::Correct) => format!("-> {r}"),
            (None, Verdict::Correct) => "refused, as declared".to_string(),
            (Some(r), _) => format!(
                "-> {r}, wanted {}",
                self.expected.as_deref().unwrap_or("a refusal")
            ),
            (None, _) => format!(
                "REFUSED ({}), wanted {}",
                self.reason,
                self.expected.as_deref().unwrap_or("?")
            ),
        };
        format!(
            "{} {:.58}
       {what}",
            self.verdict.label(),
            self.text
        )
    }
}
