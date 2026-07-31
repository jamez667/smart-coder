//! Corroboration: where a deterministic check *can* speak to a finding, it is
//! run, and its answer outranks the model's (spec 16).
//!
//! This is the asymmetry the whole spec rests on. A corroborated finding may gate
//! or feed a retry; an uncorroborated one is reported and ranked and can never
//! stop a run. So the bar here is set deliberately high: a check corroborates
//! only when it has found a **specific, locatable fact** it can hand to a worker,
//! because that fact is what a retry prompt injects. "Probably" does not
//! corroborate. Nothing here may guess.
//!
//! Only three of the four lenses have anything deterministic to say, and the
//! fourth says so honestly rather than passing silently:
//!
//! | Lens | The check |
//! | --- | --- |
//! | Duplication | `sc-index` found the named symbol elsewhere — nearly free, the lookup already ran to build the prompt |
//! | Error handling | The swallow is syntactically visible in the added lines |
//! | Abstraction fit | Nothing. Taste has no oracle. |
//! | Unrelated changes | Only when the subtask names its files; empty is [`Unknown`](Corroboration::Unknown), never a pass |

use crate::diff::IntegratedDiff;
use crate::finding::{Finding, Lens};
use crate::ground::Grounding;

/// What a deterministic check had to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Corroboration {
    /// The check ran and agreed, and this is what it found — the text a retry
    /// prompt injects, naming a real symbol at a real location.
    Confirmed(String),
    /// The check ran and disagreed: the claim is a suspicion, and stays one.
    Refuted,
    /// No check can speak to this. Distinct from `Refuted` and never folded into
    /// it — the same commitment spec 13 makes by keeping `Unknown` first-class.
    Unknown,
}

/// The inputs a corroboration check reads. Everything deterministic that is known
/// about the change, gathered before any model call.
pub struct Context<'a> {
    pub diff: &'a IntegratedDiff,
    pub grounding: &'a Grounding,
    /// The subtask's declared files. A decomposer *hint*, explicitly not enforced
    /// and frequently empty — which is exactly why the unrelated-changes check
    /// reports `Unknown` rather than passing when it is.
    pub subtask_files: &'a [String],
}

/// Run the deterministic check for `finding`'s lens.
pub fn check(finding: &Finding, ctx: &Context<'_>) -> Corroboration {
    match finding.lens {
        Lens::Duplication => duplication(finding, ctx),
        Lens::ErrorHandling => error_handling(finding, ctx),
        // Taste has no oracle. Saying so is the honest answer; inventing a
        // heuristic here would let style findings gate a run.
        Lens::AbstractionFit => Corroboration::Unknown,
        Lens::UnrelatedChanges => unrelated_changes(finding, ctx),
    }
}

/// Apply the check to a finding, corroborating it in place when confirmed.
pub fn apply(finding: &mut Finding, ctx: &Context<'_>) -> Corroboration {
    let outcome = check(finding, ctx);
    if let Corroboration::Confirmed(evidence) = &outcome {
        finding.corroborate(evidence.clone());
    }
    outcome
}

/// **Duplication** — did the index find the named symbol somewhere the diff did
/// not touch? The lookup already ran to build the prompt, so this is nearly free.
///
/// Corroboration requires the symbol *and its location*, never a boolean: a
/// worker told "you duplicated something" thrashes, and a worker told
/// "`format_date` already exists at src/utils/date.rs:41, import it" acts.
fn duplication(finding: &Finding, ctx: &Context<'_>) -> Corroboration {
    // A duplication finding that names no symbol has nothing to look up. Not
    // refuted — unknowable, which is a different thing.
    let Some(symbol) = &finding.anchor.symbol else {
        return Corroboration::Unknown;
    };
    match ctx.grounding.lookalike(symbol) {
        Some(hit) => Corroboration::Confirmed(format!(
            "You added `{}` in {}. An equivalent already exists: {}. \
             Import and use it instead of reimplementing it.",
            symbol,
            finding.anchor.file,
            hit.describe()
        )),
        // The index looked and found nothing by that name outside the diff. The
        // model may still be right about a *renamed* duplicate — which is why this
        // refutes the corroboration, not the finding: it is still reported, just
        // never allowed to act.
        None => Corroboration::Refuted,
    }
}

/// **Error handling** — a swallowed error is often syntactically visible. Look at
/// the added lines of the anchored hunk for the shapes that discard a failure.
///
/// Text patterns, not a parser, and scoped to the *added* lines of the hunk the
/// reviewer pointed at. That is narrow on purpose: the check exists to confirm a
/// specific claim, not to find swallows on its own.
fn error_handling(finding: &Finding, ctx: &Context<'_>) -> Corroboration {
    let Some(file) = ctx.diff.file(&finding.anchor.file) else {
        return Corroboration::Unknown;
    };
    // Without a hunk there is no bounded region to examine; searching the whole
    // file would corroborate a finding from an unrelated pre-existing swallow.
    let Some(hunk_id) = finding.anchor.hunk else {
        return Corroboration::Unknown;
    };
    let Some(hunk) = file.hunks.iter().find(|h| h.id == hunk_id) else {
        return Corroboration::Unknown;
    };

    for line in &hunk.added {
        if let Some(pattern) = swallow_pattern(line) {
            return Corroboration::Confirmed(
                format!(
                    "In {} you added `{}` — this discards the failure instead of handling \
                 or propagating it. Handle the specific error, or let it propagate.",
                    finding.anchor.file,
                    line.trim()
                ) + &format!(" (matched: {pattern})"),
            );
        }
    }
    Corroboration::Refuted
}

/// The syntactic shapes that discard a failure, across the languages the repo
/// targets. Each returns the name of what matched, so the evidence says *why*.
///
/// Deliberately conservative: every pattern here is a construct whose whole
/// effect is to drop an error. Anything requiring judgement (a bare `catch` that
/// logs, a documented fallback) is left to the model, uncorroborated.
fn swallow_pattern(line: &str) -> Option<&'static str> {
    let t = line.trim();
    let squashed: String = t.chars().filter(|c| !c.is_whitespace()).collect();

    // Python: `except: pass`, `except Exception: pass`, and the bare-except form.
    if squashed.starts_with("except") && squashed.ends_with(":pass") {
        return Some("except: pass");
    }
    if t == "pass" {
        // Only meaningful with the `except` on its own line — handled by the caller
        // scanning every added line, so a lone `pass` is not enough on its own.
        return None;
    }
    // Rust: swallowing a Result with `let _ = ...`, or `.ok();`/`.unwrap_or_default()`
    // on a fallible call are judgement calls — only the outright discard is certain.
    if squashed.starts_with("let_=") {
        return Some("let _ = (discarded Result)");
    }
    // JS/C#: an empty catch block.
    if squashed.contains("catch{}") || squashed.contains("catch(e){}") {
        return Some("empty catch block");
    }
    None
}

/// **Unrelated changes** — the weakest of the four, and worth stating plainly.
///
/// A subtask's `files` list is a decomposer hint, not enforced, and often empty.
/// Integration already draws its merge targets from that same list, so a diff
/// largely cannot exceed it at *file* granularity — which means this check can
/// only ever speak to the uninteresting version of the question. With an empty
/// list it reports [`Unknown`](Corroboration::Unknown), never a silent pass.
fn unrelated_changes(finding: &Finding, ctx: &Context<'_>) -> Corroboration {
    if ctx.subtask_files.is_empty() {
        return Corroboration::Unknown;
    }
    let norm = |s: &str| s.replace('\\', "/");
    let declared: Vec<String> = ctx.subtask_files.iter().map(|s| norm(s)).collect();
    if declared.contains(&norm(&finding.anchor.file)) {
        // The file was asked for. Within-file tangency is the interesting version
        // of this question and no deterministic check speaks to it.
        Corroboration::Unknown
    } else {
        Corroboration::Confirmed(format!(
            "{} is not among the files this subtask declared ({}). Revert the changes \
             to it and keep the subtask to what it was asked for.",
            finding.anchor.file,
            declared.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::HunkId;
    use crate::finding::{Anchor, ModelId, Severity};
    use crate::ground::SimilarSymbol;
    use sc_index::SymbolHit;

    fn finding(lens: Lens, anchor: Anchor) -> Finding {
        Finding::new(
            lens,
            Severity::Medium,
            anchor,
            "a claim",
            ModelId::new("qwen"),
        )
    }

    fn dup_grounding() -> Grounding {
        Grounding {
            similar: vec![SimilarSymbol {
                added: "format_date".into(),
                existing: SymbolHit {
                    name: "format_date".into(),
                    path: "src/utils/date.rs".into(),
                    line: 41,
                },
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_corroborated_duplicate_yields_evidence_naming_the_symbol_and_its_location() {
        // The bar from the spec: a retry prompt must be actionable, which means it
        // carries BOTH the symbol and where it lives.
        let diff = IntegratedDiff::from_changes([(
            "src/report/render.rs",
            Some("fn render() {}\n"),
            Some("fn render() {}\nfn format_date() {}\n"),
        )]);
        let g = dup_grounding();
        let ctx = Context {
            diff: &diff,
            grounding: &g,
            subtask_files: &[],
        };
        let mut f = finding(
            Lens::Duplication,
            Anchor::file("src/report/render.rs")
                .with_hunk(HunkId(0))
                .with_symbol("format_date"),
        );

        assert!(matches!(apply(&mut f, &ctx), Corroboration::Confirmed(_)));
        assert!(f.may_act());
        let ev = f.evidence.clone().unwrap();
        assert!(ev.contains("format_date"), "{ev}");
        assert!(ev.contains("src/utils/date.rs:41"), "names WHERE: {ev}");
        assert!(ev.contains("Import and use it"), "actionable: {ev}");
    }

    #[test]
    fn a_duplicate_the_index_cannot_find_stays_a_suspicion() {
        let diff = IntegratedDiff::from_changes([(
            "a.rs",
            Some("fn a() {}\n"),
            Some("fn a() { z(); }\n"),
        )]);
        let g = Grounding::default();
        let ctx = Context {
            diff: &diff,
            grounding: &g,
            subtask_files: &[],
        };
        let mut f = finding(
            Lens::Duplication,
            Anchor::file("a.rs")
                .with_hunk(HunkId(0))
                .with_symbol("ghost"),
        );
        assert_eq!(apply(&mut f, &ctx), Corroboration::Refuted);
        assert!(!f.may_act(), "reported and ranked, but never able to act");
        assert!(f.evidence.is_none());
    }

    #[test]
    fn a_duplication_finding_naming_no_symbol_is_unknown_not_refuted() {
        let diff = IntegratedDiff::from_changes([("a.rs", Some("x\n"), Some("y\n"))]);
        let g = dup_grounding();
        let ctx = Context {
            diff: &diff,
            grounding: &g,
            subtask_files: &[],
        };
        let f = finding(Lens::Duplication, Anchor::file("a.rs").with_hunk(HunkId(0)));
        assert_eq!(check(&f, &ctx), Corroboration::Unknown);
    }

    #[test]
    fn a_syntactically_visible_swallow_corroborates() {
        let diff = IntegratedDiff::from_changes([(
            "app.py",
            Some("def f():\n    return load()\n"),
            Some("def f():\n    try:\n        return load()\n    except Exception: pass\n"),
        )]);
        let g = Grounding::default();
        let ctx = Context {
            diff: &diff,
            grounding: &g,
            subtask_files: &[],
        };
        let mut f = finding(
            Lens::ErrorHandling,
            Anchor::file("app.py").with_hunk(HunkId(0)).with_symbol("f"),
        );
        assert!(matches!(apply(&mut f, &ctx), Corroboration::Confirmed(_)));
        let ev = f.evidence.unwrap();
        assert!(ev.contains("except Exception: pass"), "{ev}");
    }

    #[test]
    fn error_handling_without_a_hunk_is_unknown_not_a_whole_file_search() {
        // Searching the whole file would corroborate a fresh claim from a swallow
        // that was already there — evidence for the wrong thing.
        let diff = IntegratedDiff::from_changes([(
            "app.py",
            Some("try:\n    x()\nexcept: pass\n"),
            Some("try:\n    x()\nexcept: pass\ny = 1\n"),
        )]);
        let g = Grounding::default();
        let ctx = Context {
            diff: &diff,
            grounding: &g,
            subtask_files: &[],
        };
        let f = finding(Lens::ErrorHandling, Anchor::file("app.py").with_symbol("x"));
        assert_eq!(check(&f, &ctx), Corroboration::Unknown);
    }

    #[test]
    fn abstraction_fit_has_no_oracle_and_says_so() {
        let diff = IntegratedDiff::from_changes([("a.rs", Some("x\n"), Some("y\n"))]);
        let g = Grounding::default();
        let ctx = Context {
            diff: &diff,
            grounding: &g,
            subtask_files: &[],
        };
        let f = finding(
            Lens::AbstractionFit,
            Anchor::file("a.rs").with_hunk(HunkId(0)),
        );
        // Never Refuted: the check did not disagree, it has nothing to say.
        assert_eq!(check(&f, &ctx), Corroboration::Unknown);
    }

    #[test]
    fn unrelated_changes_with_an_empty_file_list_is_unknown_not_a_pass() {
        // The spec is explicit: with an empty list the lens must report Unknown.
        // Passing silently would report "reviewed" over a question nobody asked.
        let diff = IntegratedDiff::from_changes([("a.rs", Some("x\n"), Some("y\n"))]);
        let g = Grounding::default();
        let ctx = Context {
            diff: &diff,
            grounding: &g,
            subtask_files: &[],
        };
        let f = finding(
            Lens::UnrelatedChanges,
            Anchor::file("a.rs").with_hunk(HunkId(0)),
        );
        assert_eq!(check(&f, &ctx), Corroboration::Unknown);
    }

    #[test]
    fn unrelated_changes_corroborates_only_a_file_outside_the_declared_set() {
        let diff = IntegratedDiff::from_changes([
            ("asked.rs", Some("x\n"), Some("y\n")),
            ("drive_by.rs", Some("p\n"), Some("q\n")),
        ]);
        let g = Grounding::default();
        let declared = vec!["asked.rs".to_string()];
        let ctx = Context {
            diff: &diff,
            grounding: &g,
            subtask_files: &declared,
        };

        // Within a declared file: no deterministic check speaks to within-file tangency.
        let inside = finding(
            Lens::UnrelatedChanges,
            Anchor::file("asked.rs").with_hunk(HunkId(0)),
        );
        assert_eq!(check(&inside, &ctx), Corroboration::Unknown);

        // Outside it: a fact, and an actionable one.
        let outside = finding(
            Lens::UnrelatedChanges,
            Anchor::file("drive_by.rs").with_hunk(HunkId(0)),
        );
        match check(&outside, &ctx) {
            Corroboration::Confirmed(ev) => {
                assert!(ev.contains("drive_by.rs"), "{ev}");
                assert!(ev.contains("asked.rs"), "names what WAS asked for: {ev}");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn swallow_patterns_are_conservative() {
        assert!(swallow_pattern("    except Exception: pass").is_some());
        assert!(swallow_pattern("except: pass").is_some());
        assert!(swallow_pattern("let _ = risky();").is_some());
        assert!(swallow_pattern("} catch (e) {}").is_some());
        // Judgement calls stay with the model, uncorroborated.
        assert!(swallow_pattern("except Exception as e: log.warn(e)").is_none());
        assert!(swallow_pattern("    pass").is_none());
        assert!(swallow_pattern("let x = risky()?;").is_none());
    }
}
