//! `screen-eval` — score the spam screener against a real model.
//!
//! The offline tests assert **containment**: exact-match rather than `contains`,
//! anything unexpected admits, markers stripped, the stored reason never model
//! output. Those gate every commit, cost nothing, and hold whatever the model
//! does — including "the model has been fully talked round".
//!
//! This answers the other question, which containment cannot: **does the model
//! actually sort these cases?** It needs a key and spends real money, so it is
//! deliberately *not* part of `check.sh` — a gate that costs money per run is a
//! gate somebody disables.
//!
//! ```text
//!   screen-eval                        # reads GEMINI_API_KEY, or .env
//!   screen-eval --key AI... --model gemini-2.5-flash-lite
//!   screen-eval --verbose              # every case, not just the failures
//! ```
//!
//! Run it when the model changes, when the prompt changes, or when spam starts
//! getting through. A filter nobody measures is one you cannot tell has stopped
//! working.

use sc_server::config::{DEFAULT_SCREEN_MODEL, DEFAULT_SCREEN_URL};
use sc_server::screen::{HttpScreener, Screener, Verdict};
use sc_server::screen_eval::{report, score, Corpus, Label};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{}", USAGE);
        return std::process::ExitCode::SUCCESS;
    }

    let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");
    let key = match flag(&args, "--key").or_else(read_key) {
        Some(k) => k,
        None => {
            eprintln!("no API key. Pass --key, set GEMINI_API_KEY, or put it in .env\n\n{USAGE}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let model = flag(&args, "--model").unwrap_or_else(|| DEFAULT_SCREEN_MODEL.to_string());
    let url = flag(&args, "--url").unwrap_or_else(|| DEFAULT_SCREEN_URL.to_string());

    let corpus = match Corpus::bundled() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("could not read the corpus: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };

    println!("screening {} cases against {model}\n", corpus.cases.len());
    let screener = HttpScreener::new(&url, &key, &model);

    // Screen **once** and score the recorded verdicts. Calling the model in a
    // display loop and again in `score` would double the spend and — because
    // even temperature 0 is not perfectly deterministic — let the per-case list
    // disagree with its own summary. It did, on the first run of this tool.
    let recorded = Recorded::of(&screener, &corpus);

    // Per-case output before the summary, so a run that dies partway through
    // still shows what it learned. A summary-only tool tells you nothing when
    // the twelfth call times out.
    for (case, verdict) in corpus.cases.iter().zip(&recorded.verdicts) {
        let correct = matches!(
            (case.label, verdict),
            (Label::Spam, Verdict::Quarantine) | (Label::Ok, Verdict::Admit)
        );
        if !correct || verbose {
            println!(
                "{}  {:<32} labelled {:<4} → {}{}",
                if correct { "  ok" } else { "MISS" },
                case.id,
                match case.label {
                    Label::Ok => "ok",
                    Label::Spam => "spam",
                },
                if matches!(verdict, Verdict::Quarantine) {
                    "held"
                } else {
                    "admitted"
                },
                if case.injection { "   (injection)" } else { "" },
            );
        }
    }

    // Scored through the same function the offline tests use, so the two cannot
    // disagree about what counts as a false positive.
    let scored = score(&recorded, &corpus);
    println!("\n{}", report(&scored));

    // Exit non-zero when a legitimate request was held. That is the expensive
    // failure — a real person told their report went through when it did not —
    // and it is the one worth making visible to a script.
    if scored.false_positive > 0 {
        eprintln!("FAILED: a legitimate request was quarantined.");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

/// Verdicts already obtained, replayed as a [`Screener`].
///
/// Lets the per-case list and the summary come from **one** set of calls, so the
/// tool costs what it looks like it costs and cannot contradict itself. Replay
/// is by position, which is safe because [`score`] walks the corpus in order —
/// the same order this recorded it in.
struct Recorded {
    verdicts: Vec<Verdict>,
    // A `Mutex` rather than a `Cell`: `Screener` is `Send + Sync`, and a mutex
    // satisfies that with no soundness argument to get wrong. It is uncontended
    // in a single-threaded CLI, so it costs nothing worth reasoning about.
    next: std::sync::Mutex<usize>,
}

impl Recorded {
    fn of(screener: &dyn Screener, corpus: &Corpus) -> Recorded {
        Recorded {
            verdicts: corpus
                .cases
                .iter()
                .map(|c| screener.screen(&c.text))
                .collect(),
            next: std::sync::Mutex::new(0),
        }
    }
}

impl Screener for Recorded {
    fn screen(&self, _text: &str) -> Verdict {
        let mut i = self.next.lock().unwrap_or_else(|p| p.into_inner());
        let verdict = self.verdicts.get(*i).cloned().unwrap_or(Verdict::Admit);
        *i += 1;
        verdict
    }
}

const USAGE: &str = "\
screen-eval — score the spam screener against a real model

USAGE:
    screen-eval [--key KEY] [--model NAME] [--url BASE] [--verbose]

The key is read from --key, then GEMINI_API_KEY, then a .env file beside the
workspace root. It is never printed.

Exits non-zero if any legitimate request was quarantined — the expensive
failure, because a real person is told their report went through when it did
not. Missed spam costs one wasted drafting run and does not fail the run.";

/// Read `--name VALUE` from the arguments.
fn flag(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).filter(|v| !v.starts_with("--")).cloned()
}

/// The API key, from the environment or a `.env` beside the workspace root.
///
/// `.env` is git-ignored and is where this project already keeps the Gemini key,
/// so reading it saves exporting one by hand for a tool run occasionally.
fn read_key() -> Option<String> {
    if let Ok(k) = std::env::var("GEMINI_API_KEY") {
        if !k.trim().is_empty() {
            return Some(k.trim().to_string());
        }
    }
    // Walk up: the binary may be run from a crate directory rather than the root.
    let mut dir = std::env::current_dir().ok()?;
    for _ in 0..4 {
        let candidate = dir.join(".env");
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            for line in text.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("GEMINI_API_KEY=") {
                    let v = rest.trim().trim_matches('"').trim_matches('\'');
                    if !v.is_empty() {
                        return Some(v.to_string());
                    }
                }
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}
