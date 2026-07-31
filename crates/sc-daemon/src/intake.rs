//! What kind of thing was filed.
//!
//! Four kinds go in; only three come out as specs.
//!
//! | Kind | Drafts a spec? | Where it goes |
//! | --- | --- | --- |
//! | [`Bug`](IntakeKind::Bug) | yes | `specs/<slug>/spec.md` in the repo |
//! | [`Feature`](IntakeKind::Feature) | yes | as above |
//! | [`Improvement`](IntakeKind::Improvement) | yes | as above |
//! | [`Feedback`](IntakeKind::Feedback) | **no** | the daemon's own store |
//!
//! ## Why the kinds shape the prompt
//!
//! A bug and a feature are not the same request wearing different labels. A bug
//! spec is useless without reproduction and expected-versus-actual; a feature
//! spec is useless without non-goals. Asking one generic question and hoping the
//! model infers which it was gets you a feature-shaped document about a crash —
//! the same reasoning spec 16 gives for asking each review lens one question
//! rather than one prompt four.
//!
//! The framing is added to the **task text** rather than to `sc-workflow`'s phase
//! prompt, deliberately: intake kinds are the daemon's concept, and the CLI and
//! GUI have no notion of them. Pushing them down would put a vocabulary in the
//! shared engine that only one of its three front-ends uses.
//!
//! ## Why feedback is not a spec
//!
//! Feedback is "this annoys me" or "that flow feels wrong" — a note, not a
//! request. Drafting a spec for it would manufacture a work item nobody asked
//! for, and put a phone-typed thought into a repository through a gate whose
//! whole job is to decide whether a *spec* is right. It costs no model call and
//! writes to no repository; it is simply kept, and read when the developer wants
//! to see what has been piling up.

use serde::{Deserialize, Serialize};

/// What kind of request was filed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IntakeKind {
    /// Something is broken.
    Bug,
    /// Something new is wanted.
    #[default]
    Feature,
    /// Something works but should work better.
    Improvement,
    /// A thought, kept rather than acted on. **Never drafts a spec.**
    Feedback,
}

impl IntakeKind {
    /// Parse what a user typed or a request body carried.
    ///
    /// Accepts the obvious short forms because this is typed on a phone as often
    /// as at a keyboard. Unknown input returns `None` so the caller can refuse
    /// loudly rather than silently defaulting — a bug filed as a feature comes
    /// back with the wrong kind of spec.
    pub fn parse(s: &str) -> Option<IntakeKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bug" | "b" | "defect" | "fix" => Some(IntakeKind::Bug),
            "feature" | "f" | "feat" | "new" => Some(IntakeKind::Feature),
            "improvement" | "i" | "improve" | "enhancement" => Some(IntakeKind::Improvement),
            "feedback" | "note" | "comment" => Some(IntakeKind::Feedback),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            IntakeKind::Bug => "bug",
            IntakeKind::Feature => "feature",
            IntakeKind::Improvement => "improvement",
            IntakeKind::Feedback => "feedback",
        }
    }

    /// Every kind, for a picker.
    pub const ALL: [IntakeKind; 4] = [
        IntakeKind::Bug,
        IntakeKind::Feature,
        IntakeKind::Improvement,
        IntakeKind::Feedback,
    ];

    /// Does this kind produce a drafted spec?
    ///
    /// The one place the distinction lives, so nothing downstream has to
    /// re-derive it.
    pub fn drafts_a_spec(self) -> bool {
        !matches!(self, IntakeKind::Feedback)
    }

    /// The framing prepended to the request when drafting its spec.
    ///
    /// Empty for [`Feedback`](IntakeKind::Feedback), which never reaches a model.
    ///
    /// Each asks for what that kind of document is useless without: a bug spec
    /// needs reproduction and expected-versus-actual; a feature spec needs
    /// non-goals, or it sprawls; an improvement needs to say what is wrong with
    /// the behaviour *today*, or it reads as a feature and loses the baseline.
    pub fn framing(self) -> &'static str {
        match self {
            IntakeKind::Bug => {
                "This is a BUG REPORT. Specify the fix: what is happening now, what should \
                 happen instead, how to reproduce it, and what else the same root cause \
                 might be affecting. If the report does not say how to reproduce it, say \
                 so plainly under a 'Missing information' heading rather than inventing \
                 steps — a spec that guesses at reproduction sends someone hunting the \
                 wrong bug."
            }
            IntakeKind::Feature => {
                "This is a FEATURE REQUEST. Specify it: goals, explicit NON-goals, and \
                 constraints. The non-goals matter as much as the goals — a feature spec \
                 without them sprawls into everything adjacent."
            }
            IntakeKind::Improvement => {
                "This is an IMPROVEMENT to something that already works. Specify it against \
                 the CURRENT behaviour: what it does today, what is wrong with that, and \
                 what better looks like concretely. Do not respecify the feature from \
                 scratch — the baseline is what makes an improvement reviewable."
            }
            IntakeKind::Feedback => "",
        }
    }

    /// The request text as the drafting run should see it.
    pub fn frame(self, text: &str) -> String {
        if !self.drafts_a_spec() {
            return text.to_string();
        }
        // The request goes FIRST: the slug is derived from the task's opening
        // line, and leading with the framing would make every bug's directory
        // `this-is-a-bug-report`.
        format!("{}\n\n{}", text.trim(), self.framing())
    }
}

impl std::fmt::Display for IntakeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_feedback_skips_drafting() {
        assert!(IntakeKind::Bug.drafts_a_spec());
        assert!(IntakeKind::Feature.drafts_a_spec());
        assert!(IntakeKind::Improvement.drafts_a_spec());
        assert!(
            !IntakeKind::Feedback.drafts_a_spec(),
            "feedback is a note, not a request — drafting one would manufacture a \
             work item nobody asked for"
        );
    }

    #[test]
    fn each_drafting_kind_asks_for_what_that_document_needs() {
        // A bug spec without reproduction, or a feature spec without non-goals,
        // is not a usable document. One generic prompt cannot ask for both.
        let bug = IntakeKind::Bug.framing().to_lowercase();
        assert!(bug.contains("reproduce"), "{bug}");
        assert!(bug.contains("should happen"), "{bug}");

        let feature = IntakeKind::Feature.framing().to_lowercase();
        assert!(feature.contains("non-goal"), "{feature}");

        let improvement = IntakeKind::Improvement.framing().to_lowercase();
        assert!(improvement.contains("current behaviour"), "{improvement}");

        // And they are genuinely different questions, not one with three labels.
        assert_ne!(IntakeKind::Bug.framing(), IntakeKind::Feature.framing());
        assert_ne!(
            IntakeKind::Feature.framing(),
            IntakeKind::Improvement.framing()
        );
    }

    #[test]
    fn a_bug_report_is_told_to_admit_missing_reproduction() {
        // Inventing steps sends someone hunting the wrong bug — worse than
        // saying the report did not include them.
        assert!(IntakeKind::Bug
            .framing()
            .to_lowercase()
            .contains("missing information"));
    }

    #[test]
    fn framing_follows_the_request_so_the_slug_stays_meaningful() {
        // The artifact directory is derived from the task's opening line. Leading
        // with the framing would give every bug the directory
        // `this-is-a-bug-report`.
        let framed = IntakeKind::Bug.frame("Login fails on Safari");
        assert!(framed.starts_with("Login fails on Safari"), "{framed}");
        assert!(framed.contains("BUG REPORT"));
    }

    #[test]
    fn feedback_is_never_reframed_because_no_model_sees_it() {
        let text = "the approve button is too easy to hit by accident";
        assert_eq!(IntakeKind::Feedback.frame(text), text);
    }

    #[test]
    fn kinds_parse_from_what_a_person_would_type() {
        for (input, expected) in [
            ("bug", IntakeKind::Bug),
            ("BUG", IntakeKind::Bug),
            (" fix ", IntakeKind::Bug),
            ("feature", IntakeKind::Feature),
            ("feat", IntakeKind::Feature),
            ("improvement", IntakeKind::Improvement),
            ("improve", IntakeKind::Improvement),
            ("feedback", IntakeKind::Feedback),
            ("note", IntakeKind::Feedback),
        ] {
            assert_eq!(IntakeKind::parse(input), Some(expected), "{input:?}");
        }
    }

    #[test]
    fn an_unknown_kind_is_refused_rather_than_defaulted() {
        // Silently defaulting would return a feature-shaped spec for a crash.
        assert_eq!(IntakeKind::parse("urgent"), None);
        assert_eq!(IntakeKind::parse(""), None);
    }

    #[test]
    fn kinds_round_trip_through_json() {
        for kind in IntakeKind::ALL {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(serde_json::from_str::<IntakeKind>(&json).unwrap(), kind);
        }
    }

    #[test]
    fn a_task_filed_before_kinds_existed_reads_as_a_feature() {
        // The queue is durable across upgrades: a record written by an older
        // build must still load rather than becoming unreadable.
        #[derive(Deserialize)]
        struct Holder {
            #[serde(default)]
            kind: IntakeKind,
        }
        let h: Holder = serde_json::from_str("{}").unwrap();
        assert_eq!(h.kind, IntakeKind::Feature);
    }
}
