//! The classifier: need in, capability out — or an honest refusal.
//!
//! Two stages, in this order for a reason:
//!
//! 1. **Deterministic scoring** over the capability table. Free, instant,
//!    explainable, and testable without a model.
//! 2. **LLM router**, consulted *only* when scoring is ambiguous, and given a
//!    menu of the top candidates rather than the whole table.
//!
//! The failure this file exists to prevent is the **silent misroute**: the
//! wrong capability produces a plausible answer, the model believes it, and
//! nothing in the transcript says anything went wrong. A refusal is loud and
//! recoverable; a misroute is neither. So when the evidence is thin, this
//! refuses — `Unknown` is a first-class outcome, not an error path.

use sc_model::{GenerateRequest, Message, ModelBackend};

use crate::capability::{contains_word, score, Capability};
use crate::types::{Need, Trace};

/// A routing decision.
#[derive(Debug, Clone)]
pub enum Route {
    /// Route to this capability (an index into the table).
    To { index: usize, trace: Trace },
    /// Refuse: nothing scored well enough, or two candidates tied.
    Unknown { trace: Trace },
}

/// Minimum score for a route to be taken at all.
///
/// One hint alone (3) is not evidence — it's a noun that happened to appear.
/// One verb (10) is. Set at the verb threshold deliberately.
pub const MIN_SCORE: u32 = 10;

/// How far ahead the winner must be to win outright.
///
/// Below this the two candidates are genuinely confusable, and picking by
/// arithmetic noise is exactly the silent misroute. Escalate or refuse instead.
///
/// A flat margin alone is wrong at both ends: a 3-point gap is noise between
/// two verb-matches (10 vs 13 hint-padding) but decisive between a verb-match
/// and a bare hint. So a tie is broken when EITHER the absolute gap clears this
/// margin, or the winner matched strictly more verbs — matching the operation
/// word is the evidence that actually distinguishes two capabilities.
pub const AMBIGUITY_MARGIN: u32 = 4;

/// What a stated line range is worth to a capability that can window.
///
/// A full verb's weight: naming "lines 42 through 65" is as strong a statement
/// of intent as naming the operation, and a capability that cannot honour it
/// loses half that much.
pub const LINE_RANGE_BONUS: u32 = 10;

/// What naming an identifier is worth to a capability that looks symbols up.
///
/// A verb's weight: "locate StallDetector" states its target as clearly as it
/// states its operation, and the target is the half that distinguishes a symbol
/// lookup from a text search.
pub const SYMBOL_SHAPE_BONUS: u32 = 10;

/// Verbs that name an ACTION but not what kind of thing is being sought.
///
/// "find"/"locate"/"search" apply equally to a symbol and to raw text, so they
/// do not settle the question and shape is allowed to. "grep" does settle it —
/// it is a request about text — and so is excluded here.
const NEUTRAL_VERBS: [&str; 4] = ["find", "locate", "search", "show"];

/// Classify a need against the table, without a model.
///
/// Returns `Unknown` when nothing clears [`MIN_SCORE`], or when the top two are
/// within [`AMBIGUITY_MARGIN`]. Both refusals name what was considered, so the
/// caller can see whether the table is missing a capability or a description is
/// too vague.
pub fn classify(need: &Need, table: &[Capability]) -> Route {
    let lower = need.text.to_lowercase();
    // A stated line range is primary evidence for a capability that can WINDOW,
    // and it has to weigh in the score itself rather than in a tie-break: at
    // 10-vs-3 the rival wins outright and no tie-break is ever consulted. That
    // is how "pathfind.rs lines 42 through 65 (the body of astar_tile_grid)"
    // silently routed to `code.function`, which cannot honour a line range at
    // all — a misroute, and worse than the refusal it replaced.
    //
    // Structural, not lexical: it asks what the capability can DO with what the
    // need contains, not what the need sounds like.
    let ranged = crate::capability::names_a_line_range(&lower);
    // A need whose target is an IDENTIFIER is a symbol question, not a text
    // search. "locate StallDetector" and "search for handle_timeout" use the
    // same verbs, and the only thing separating them from "grep for TODO" is
    // the shape of what they name.
    //
    // Scored rather than tie-broken because `code.symbol` may not match the
    // verb at all — "locate" is a search word — so there would be no tie to
    // break. Removing `locate` from code.search instead (tried, reverted) just
    // broke "find TODO", because the verb was never the problem.
    let names_identifier = need
        .text
        .split_whitespace()
        .any(crate::capability::is_identifier_like);
    // Does the need name an operation that is INCOMPATIBLE with a symbol
    // lookup? "grep the codebase for StallDetector" and "the full contents of
    // stall.rs, especially handle_timeout" both name an identifier, but they
    // also state plainly what they want done with it — and lifting the symbol
    // lookup there overrode a stated operation with a guess (six cases broke
    // that way in one run).
    //
    // Checked per-capability rather than globally: a global "did any verb
    // match" also suppresses the bonus for `locate StallDetector`, where the
    // matched verb belongs to the rival and the shape is the only thing
    // separating them.
    let states_other_operation = table.iter().any(|cap| {
        !cap.requires
            .contains(&crate::capability::Requirement::Symbol)
            && cap
                .verbs
                .iter()
                .any(|v| contains_word(&lower, v) && !NEUTRAL_VERBS.contains(v))
    });
    let mut scored: Vec<(usize, u32)> = table
        .iter()
        .enumerate()
        .map(|(i, cap)| {
            let mut s = score(cap, &lower);
            // Applied ONLY when nothing else scored a verb. Naming an
            // identifier says what the caller is asking ABOUT, not what they
            // want done with it: "grep the codebase for StallDetector" names an
            // identifier and explicitly asks for a grep, and lifting the symbol
            // lookup there overrode a stated operation with a guess. Six cases
            // broke that way in one run.
            if names_identifier
                && !states_other_operation
                && cap
                    .requires
                    .contains(&crate::capability::Requirement::Symbol)
                && fully_satisfies(cap, need)
            {
                s += SYMBOL_SHAPE_BONUS;
            }
            if ranged && s > 0 {
                if cap.windowed {
                    s += LINE_RANGE_BONUS;
                } else {
                    // It cannot use the range the caller took the trouble to
                    // state. That is evidence AGAINST it, not neutral.
                    s = s.saturating_sub(LINE_RANGE_BONUS / 2);
                }
            }
            (i, s)
        })
        .filter(|(_, s)| *s > 0)
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let considered: Vec<(String, u32)> = scored
        .iter()
        .map(|(i, s)| (table[*i].name.to_string(), *s))
        .collect();

    let Some(&(best, best_score)) = scored.first() else {
        // Nothing matched a word — but a bare "src/lib.rs" or "show me
        // handle_timeout" still names a target unambiguously. Consider every
        // capability on requirements alone, and route iff exactly one can run.
        if let Some(index) = sole_runnable_anywhere(need, table) {
            return Route::To {
                index,
                trace: Trace {
                    route: Some(table[index].name.to_string()),
                    class: Some(table[index].class),
                    reason: "no operation named; the only capability the need can feed".to_string(),
                    considered,
                    ..Trace::default()
                },
            };
        }
        return Route::Unknown {
            trace: Trace::unknown("no capability matched the need", considered),
        };
    };

    if best_score < MIN_SCORE {
        // Before refusing for want of a verb: does exactly ONE capability have
        // everything it needs to run? "what is in src/lib.rs" and "src/lib.rs"
        // name a file and no operation, but there is only one thing you can do
        // with a bare file, and refusing it teaches a caller the harness is
        // unhelpful.
        //
        // Structural, like the tie-breaks below: it asks what the need SUPPLIES,
        // not what verb it happens to use. A verb list can never cover how people
        // phrase things — six rounds of adding words each fixed one phrasing and
        // broke another — but "this need contains a path and only file.read can
        // use a bare path" is a fact.
        //
        // Still strict: EXACTLY one candidate may qualify. Two capabilities that
        // could both run is the ambiguity this design refuses on.
        if let Some(index) = sole_runnable(need, table, &scored) {
            return Route::To {
                index,
                trace: Trace {
                    route: Some(table[index].name.to_string()),
                    class: Some(table[index].class),
                    reason: "the only capability the need supplies arguments for".to_string(),
                    considered,
                    ..Trace::default()
                },
            };
        }
        return Route::Unknown {
            trace: Trace::unknown(
                format!(
                    "best match {} scored {best_score}, below threshold",
                    table[best].name
                ),
                considered,
            ),
        };
    }

    // A capability that needs a path, asked without one, is not a match — it
    // would run against an empty path and return a confident wrong answer.
    if table[best].needs_path && need.scope.is_none() && !mentions_path(&need.text) {
        // Before refusing: a RUNNABLE alternative among the candidates is a
        // better answer than nothing. "print the handle_timeout function" names
        // a function and no file, so `code.function` genuinely cannot run — but
        // `code.symbol` can, and telling the caller where the symbol lives beats
        // telling them their question was unanswerable.
        //
        // Only a capability that scored (so the need gestured at it) and is
        // fully satisfied qualifies, and only if it is the sole such candidate.
        // Searched over the WHOLE table, not just what scored: the alternative
        // to a path-less `code.function` is `code.symbol`, which the need never
        // gestured at by name. Requirements are the evidence here, not wording.
        if let Some(index) = sole_runnable_anywhere(need, table) {
            if index != best {
                return Route::To {
                    index,
                    trace: Trace {
                        route: Some(table[index].name.to_string()),
                        class: Some(table[index].class),
                        reason: format!(
                            "{} needs a path; this is the runnable alternative",
                            table[best].name
                        ),
                        considered,
                        ..Trace::default()
                    },
                };
            }
        }
        return Route::Unknown {
            trace: Trace::unknown(
                format!("{} needs a path and the need names none", table[best].name),
                considered,
            ),
        };
    }

    if let Some(&(runner_idx, runner_up)) = scored.get(1) {
        // Checked FIRST, ahead of every other tie-break. A stated line range is
        // a harder fact than any of them: `read_file` takes a window and the
        // function/symbol lookups do not, so "lines 42-65 of pathfind.rs" is a
        // windowed read whatever else the sentence explains.
        //
        // Order matters and was wrong once. The requirements rule below fires
        // when one candidate is FULLY satisfied — which rewards the capability
        // that demanded MORE arguments, so `code.function` (path + symbol) beat
        // `file.read` (path) and quietly misrouted a plain line-range read. A
        // misroute is worse than the refusal it replaced.
        if best_score.abs_diff(runner_up) < AMBIGUITY_MARGIN {
            if let Some(index) = only_one_can_use(need, table, best, runner_idx) {
                return Route::To {
                    index,
                    trace: Trace {
                        route: Some(table[index].name.to_string()),
                        class: Some(table[index].class),
                        reason: "only capability that can use the stated line range".to_string(),
                        considered,
                        ..Trace::default()
                    },
                };
            }
        }

        // A whole-file request that also mentions what is inside it. "full
        // contents of pathfind.rs, especially the astar_tile_grid function" is
        // not two competing claims: the file read SUBSUMES the function, so the
        // mention is context, not a rival intent.
        //
        // Structural again — it turns on the caller having asked for the whole
        // thing, which `subsumes_a_narrower_read` reads from the need, not from
        // how the sentence is phrased.
        if best_score.abs_diff(runner_up) < AMBIGUITY_MARGIN {
            if let Some(index) = whole_file_subsumes(need, table, best, runner_idx) {
                return Route::To {
                    index,
                    trace: Trace {
                        route: Some(table[index].name.to_string()),
                        class: Some(table[index].class),
                        reason: "whole-file request subsumes the narrower one".to_string(),
                        considered,
                        ..Trace::default()
                    },
                };
            }
        }

        let decisive_verbs =
            verb_matches(&table[best], &lower) > verb_matches(&table[runner_idx], &lower);
        // **The structural tie-break.** Vocabulary describes what a need sounds
        // like; requirements describe what it actually CONTAINS. A caller who
        // names a file and a line range has supplied everything `file.read`
        // needs, and no trailing explanation of why they want it changes that.
        //
        // This exists because six rounds of vocabulary tuning each fixed one
        // phrasing and broke another. The A/B's remaining refusals were 14 of 18
        // identical: "show me pathfind.rs lines 42-65 (the body of
        // astar_tile_grid)" — a decisive primary signal, tied into a refusal by
        // the explanation trailing it. The classifier was punishing the model for
        // being specific, and a vaguer need would have routed fine.
        // Symmetric on purpose. This used to fire only when the WINNER was
        // fully satisfied, so a tie where the runner-up was the runnable one
        // refused instead of routing: "show me the top 3 slowest functions"
        // (scoped to a .folded profile) tied `code.function` against
        // `perf.hotspots` on the inflected word "functions", and only the latter
        // could actually run. Which candidate arithmetic happened to sort first
        // is not evidence about either.
        let best_ok = fully_satisfies(&table[best], need);
        let runner_ok = fully_satisfies(&table[runner_idx], need);
        if best_score.abs_diff(runner_up) < AMBIGUITY_MARGIN && runner_ok && !best_ok {
            return Route::To {
                index: runner_idx,
                trace: Trace {
                    route: Some(table[runner_idx].name.to_string()),
                    class: Some(table[runner_idx].class),
                    reason: "the only tied candidate the need supplies arguments for".to_string(),
                    considered,
                    ..Trace::default()
                },
            };
        }
        let decisive_requirements = best_ok && !runner_ok;
        if best_score - runner_up < AMBIGUITY_MARGIN && !decisive_verbs && !decisive_requirements {
            return Route::Unknown {
                trace: Trace::unknown(
                    format!(
                        "ambiguous: {} ({best_score}) vs {} ({runner_up})",
                        table[best].name, table[scored[1].0].name
                    ),
                    considered,
                ),
            };
        }
    }

    Route::To {
        index: best,
        trace: Trace {
            route: Some(table[best].name.to_string()),
            class: Some(table[best].class),
            reason: format!("deterministic score {best_score}"),
            considered,
            ..Trace::default()
        },
    }
}

/// How many of a capability's verbs the need actually contains.
///
/// The tie-breaker: hints pad a score, verbs identify an operation. A candidate
/// that matched more verbs is ahead for a reason, not by arithmetic.
fn verb_matches(cap: &Capability, need_lower: &str) -> usize {
    cap.verbs
        .iter()
        .filter(|v| contains_word(need_lower, v))
        .count()
}

/// A need asking for a WHOLE file beats a narrower read of part of it.
///
/// Returns the whole-file candidate when exactly one of the two is `file.read`
/// and the need asks for the entire thing. The rival (`code.function`,
/// `code.symbol`) would return strictly less than what was asked for, so
/// choosing it is a quiet under-delivery the caller cannot see.
fn whole_file_subsumes(need: &Need, table: &[Capability], a: usize, b: usize) -> Option<usize> {
    const WHOLE: [&str; 6] = ["full", "entire", "whole", "complete", "all", "untruncated"];
    let lower = need.text.to_lowercase();
    if !WHOLE.iter().any(|w| contains_word(&lower, w)) {
        return None;
    }
    // A stated line range means the caller does NOT want the whole file, whatever
    // the word "full" is doing in the sentence.
    if crate::capability::names_a_line_range(&lower) {
        return None;
    }
    match (table[a].name, table[b].name) {
        ("file.read", _) => Some(a),
        (_, "file.read") => Some(b),
        _ => None,
    }
}

/// When a need states a line range, the capability that can WINDOW is the one
/// that can honour it — if exactly one of the two candidates can.
///
/// Returns the index to route to, or `None` when this does not discriminate.
fn only_one_can_use(need: &Need, table: &[Capability], a: usize, b: usize) -> Option<usize> {
    if !crate::capability::names_a_line_range(&need.text.to_lowercase()) {
        return None;
    }
    match (table[a].windowed, table[b].windowed) {
        (true, false) => Some(a),
        (false, true) => Some(b),
        // Both or neither: the range says nothing about which to pick.
        _ => None,
    }
}

/// Like [`sole_runnable`], but over the WHOLE table — for a need that matched no
/// vocabulary at all.
///
/// Capabilities that require nothing are excluded by `fully_satisfies`, so a
/// bare noun cannot fall into `repo.map` or `verify.run`: those have nothing to
/// prove and must still be asked for by name.
fn sole_runnable_anywhere(need: &Need, table: &[Capability]) -> Option<usize> {
    let all: Vec<(usize, u32)> = (0..table.len()).map(|i| (i, 0)).collect();
    sole_runnable(need, table, &all)
}

/// The single capability whose requirements the need fully supplies.
///
/// Only considers capabilities that scored at all — a need must still gesture at
/// a capability, this just lowers the bar from "named the operation" to
/// "supplied what it needs". Returns `None` when none or several qualify, so an
/// ambiguous need still refuses.
fn sole_runnable(need: &Need, table: &[Capability], scored: &[(usize, u32)]) -> Option<usize> {
    let mut runnable = scored
        .iter()
        .map(|(i, _)| *i)
        .filter(|i| fully_satisfies(&table[*i], need));
    let first = runnable.next()?;
    runnable.next().is_none().then_some(first)
}

/// Does the need supply everything this capability needs to run?
///
/// A capability that requires nothing returns `false`: it has nothing
/// structural to prove, so it must win on vocabulary like before. Only a
/// capability that asked for something AND got it earns the tie-break.
fn fully_satisfies(cap: &Capability, need: &Need) -> bool {
    let (met, total) = crate::capability::requirements_met(cap, need);
    total > 0 && met == total
}

/// Does the need name something path-shaped? Cheap and deliberately strict —
/// a false negative costs a refusal, a false positive costs a wrong answer.
fn mentions_path(text: &str) -> bool {
    // Reuses the scorer's own path predicate rather than a second rule that can
    // drift from it. The A/B caught the drift: this used to reject
    // "please show me the full contents of test.rs." because the trailing
    // sentence period made `ends_with('.')` true, so a need whose path is right
    // there was refused with "the need names none".
    text.split_whitespace().any(crate::capability::is_path_like)
}

/// Escalate an ambiguous need to a model, choosing from a *shortlist*.
///
/// Only reached when [`classify`] refused. The model sees the candidate names
/// and descriptions and nothing else — no workspace, no history — so this is a
/// small, bounded, cacheable call rather than another agent turn.
///
/// Returns `None` if the model names something not on the shortlist, which is
/// treated as a refusal rather than a guess.
pub fn classify_with_model(
    need: &Need,
    table: &[Capability],
    shortlist: &[usize],
    backend: &dyn ModelBackend,
) -> Option<usize> {
    if shortlist.is_empty() {
        return None;
    }
    let menu = shortlist
        .iter()
        .map(|i| format!("- {}: {}", table[*i].name, table[*i].description))
        .collect::<Vec<_>>()
        .join("\n");

    // Greedy and short: routing is a classification, not a composition. Any
    // creativity here shows up as a misroute.
    let mut req = GenerateRequest::new(vec![
        Message::system(
            "You route a request to exactly one capability. Reply with the capability \
             name alone, or the word NONE if no capability fits. No explanation.",
        ),
        Message::user(format!("Request: {}\n\nCapabilities:\n{menu}", need.text)),
    ]);
    req.temperature = 0.0;
    req.max_tokens = 24;

    let resp = backend.generate(&req).ok()?;
    let picked = resp.content.trim().trim_matches('"').to_lowercase();
    shortlist
        .iter()
        .copied()
        .find(|i| table[*i].name.to_lowercase() == picked)
}
