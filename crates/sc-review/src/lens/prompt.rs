//! The lens prompts (spec 16 — "Lenses, not one reviewer").
//!
//! One call, one question. A single "review this diff" prompt returns whatever
//! the model noticed first; a reviewer asked one question answers it far better
//! than a reviewer asked four.
//!
//! Every prompt shares three commitments:
//!
//! * **Finding nothing is a normal outcome**, stated explicitly, because a
//!   reviewer that always finds something is a reviewer nobody reads.
//! * **Anchors are chosen from the hunks shown**, never counted. The model picks
//!   an id from a list.
//! * **Grounding comes before the question**, so the model is asked the part only
//!   it can do.

use crate::diff::IntegratedDiff;
use crate::finding::Lens;
use crate::ground::Grounding;

/// The shared framing: what a reviewer is for, and what it is emphatically not.
const SYSTEM: &str = "You are a code reviewer examining one change that has ALREADY been \
    integrated and ALREADY passes its test suite. You are not looking for bugs the tests \
    would catch — they passed. You answer exactly one question, asked below.\n\n\
    You never rewrite code. You report; someone else decides.\n\n\
    Reply with a JSON array of findings. Each finding is an object:\n\
    {\"hunk\": \"H0\", \"file\": \"src/a.rs\", \"symbol\": \"fn_or_type_name\", \
    \"severity\": \"low|medium|high\", \"summary\": \"one sentence\"}\n\n\
    Rules:\n\
    - `hunk` MUST be one of the hunk ids shown to you (H0, H1, ...). Never invent one, \
    and never cite a line number instead.\n\
    - `symbol` is the enclosing function or type name, copied exactly from the code.\n\
    - Finding NOTHING is a normal and common outcome. Reply with [] and nothing else. \
    Do not manufacture a finding to seem useful.\n\
    - Output the JSON array only. No prose, no markdown fences. /no_think";

/// The one question each lens asks, and the guidance that keeps it from drifting
/// into the other three.
fn question(lens: Lens) -> &'static str {
    match lens {
        Lens::Duplication => {
            "QUESTION: Does this change reimplement something the repository already has?\n\n\
             You have been given a pre-retrieved list of existing symbols that share a name \
             with something this diff adds. The index already answered \"does something like \
             this exist?\". Your job is the part only you can do: is it THE SAME THING? A \
             name collision between two genuinely different functions is not a duplicate. \
             Report a finding only when the new code could be replaced by calling the \
             existing one."
        }
        Lens::ErrorHandling => {
            "QUESTION: Does this change swallow a failure, or leave an error path untested?\n\n\
             Look for: an exception caught and ignored, an error return discarded, a fallback \
             value substituted for a real failure, a broad catch that hides the specific case. \
             A swallowed error IS a passing test, which is why the suite cannot see this. \
             Ordinary, deliberate, documented fallbacks are not findings."
        }
        Lens::AbstractionFit => {
            "QUESTION: Does this change match how the surrounding code solves this problem?\n\n\
             You have the full text of each changed file, not just the changed lines, because \
             this question is unanswerable from a hunk alone. Look for: a hand-rolled loop \
             where the file uses a helper, a new error type where the file has one, a widened \
             signature nothing asked for. Report a mismatch with the code AROUND it — not a \
             preference of your own. Differing from your taste is not a finding."
        }
        Lens::UnrelatedChanges => {
            "QUESTION: Does this change touch things the subtask did not ask about?\n\n\
             The concern is TANGENCY, not volume. A large diff entirely on topic is fine. \
             Three lines of drive-by refactoring are not, because nobody asked for them and \
             no test covers the change in intent: a renamed nearby struct, a tidied import, \
             a function refactored because the worker scrolled past it.\n\
             If the subtask goal below is empty or too vague to judge tangency against, \
             report NOTHING — say so by replying []. Do not guess."
        }
    }
}

/// What one review call is handed: the shared system framing, then grounding,
/// then the diff, then the question.
pub struct LensPrompt {
    pub system: String,
    pub user: String,
}

/// Build the prompt for `lens` over `diff`, grounded by `grounding`. `goal` is
/// the subtask's stated goal — the only thing the unrelated-changes lens has to
/// judge tangency against.
pub fn build(lens: Lens, diff: &IntegratedDiff, grounding: &Grounding, goal: &str) -> LensPrompt {
    let mut user = String::new();

    // Grounding first — retrieve, then ask (spec 16). The repo map is the view
    // the worker that wrote this code never had.
    user.push_str("=== REPOSITORY MAP (structural view of what this repo contains) ===\n");
    user.push_str(&grounding.repo_map);
    user.push_str("\n\n");

    match lens {
        Lens::Duplication => {
            user.push_str("=== EXISTING SYMBOLS RESEMBLING WHAT THIS DIFF ADDS ===\n");
            user.push_str(&grounding.render_similar());
            user.push_str("\n\n");
        }
        Lens::AbstractionFit => {
            user.push_str("=== THE CHANGED FILES IN FULL ===\n");
            user.push_str(&grounding.render_surrounding());
            user.push_str("\n\n");
        }
        Lens::UnrelatedChanges => {
            user.push_str("=== WHAT THE SUBTASK ASKED FOR ===\n");
            if goal.trim().is_empty() {
                user.push_str("(no goal was recorded for this subtask)");
            } else {
                user.push_str(goal.trim());
            }
            user.push_str("\n\n");
        }
        Lens::ErrorHandling => {}
    }

    user.push_str("=== THE INTEGRATED DIFF (cite these hunk ids) ===\n");
    user.push_str(&diff.render());
    user.push('\n');
    user.push_str(question(lens));
    user.push_str("\n\nFindings as a JSON array:");

    LensPrompt {
        system: SYSTEM.to_string(),
        user,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ground::{Grounding, SimilarSymbol};
    use sc_index::SymbolHit;

    fn diff() -> IntegratedDiff {
        IntegratedDiff::from_changes([(
            "src/report/render.rs",
            Some("fn render() {}\n"),
            Some("fn render() {}\nfn format_date() {}\n"),
        )])
    }

    fn grounding() -> Grounding {
        Grounding {
            repo_map: "src/utils/date.rs:41  format_date".into(),
            similar: vec![SimilarSymbol {
                added: "format_date".into(),
                existing: SymbolHit {
                    name: "format_date".into(),
                    path: "src/utils/date.rs".into(),
                    line: 41,
                },
            }],
            surrounding: vec![(
                "src/report/render.rs".into(),
                "fn render() {}\nfn format_date() {}\n".into(),
            )],
        }
    }

    #[test]
    fn every_lens_gets_the_repo_map_and_the_hunk_ids() {
        for lens in Lens::ALL {
            let p = build(lens, &diff(), &grounding(), "add a report renderer");
            assert!(
                p.user.contains("REPOSITORY MAP"),
                "{lens} lost its grounding"
            );
            assert!(p.user.contains("hunk H0"), "{lens} lost the hunk ids");
            // Finding nothing must be an offered outcome in every prompt.
            assert!(p.system.contains("Finding NOTHING"), "{lens}");
        }
    }

    #[test]
    fn duplication_is_handed_the_lookup_result_not_asked_to_search() {
        let p = build(Lens::Duplication, &diff(), &grounding(), "");
        assert!(p.user.contains("src/utils/date.rs:41"), "{}", p.user);
        assert!(p.user.contains("is it THE SAME THING"), "{}", p.user);
    }

    #[test]
    fn abstraction_fit_is_handed_the_whole_file() {
        let p = build(Lens::AbstractionFit, &diff(), &grounding(), "");
        assert!(p.user.contains("THE CHANGED FILES IN FULL"), "{}", p.user);
        assert!(p.user.contains("fn render() {}"), "{}", p.user);
    }

    #[test]
    fn unrelated_changes_carries_the_goal_and_says_so_when_there_is_none() {
        let with = build(Lens::UnrelatedChanges, &diff(), &grounding(), "add a field");
        assert!(with.user.contains("add a field"), "{}", with.user);

        // With no goal the prompt says so plainly rather than leaving the model to
        // infer tangency from nothing — the lens must not silently pass.
        let without = build(Lens::UnrelatedChanges, &diff(), &grounding(), "   ");
        assert!(
            without.user.contains("no goal was recorded"),
            "{}",
            without.user
        );
    }

    #[test]
    fn no_prompt_asks_the_model_to_count_lines() {
        for lens in Lens::ALL {
            let p = build(lens, &diff(), &grounding(), "goal");
            assert!(
                p.system.contains("never cite a line number"),
                "{lens} must forbid line citation"
            );
        }
    }
}
