//! The screener's eval corpus, and what it proves.
//!
//! A spam filter nobody measures is one you cannot tell has stopped working —
//! and this one talks to a third-party model that can change under you without
//! notice. So the corpus is checked two ways, answering different questions:
//!
//! | Run | Question | Cost |
//! |---|---|---|
//! | [`Corpus::bundled`] + the tests below | do the **containment** properties hold? | free, every commit |
//! | `smart-coder screen-eval` | does the **model** still classify well? | a key and a few cents |
//!
//! The split matters. Containment is what makes the screener safe to have at all
//! — it must hold whatever the model does, including "the model has been fully
//! talked round". Accuracy is a quality measure that can regress without anything
//! being unsafe. Only the first belongs in a gate.

use sc_proto::{DcError, Result};
use serde::Deserialize;

use crate::screen::{Screener, Verdict};

/// What a correct classifier should say about a case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Label {
    /// A legitimate request. Quarantining it is a **false positive** — the
    /// expensive failure, because a real person is told their report went
    /// through when it did not.
    Ok,
    /// Junk. Admitting it is a false negative: one wasted drafting run, visible
    /// in the queue and cheap to discard.
    Spam,
}

/// One labelled request.
#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    pub id: String,
    pub label: Label,
    pub text: String,
    /// Does this case try to talk the classifier out of its job?
    ///
    /// Labelled by what the text **is**, not by what it asks to be called — an
    /// injection dressed as a bug report is still spam when there is no bug in
    /// it.
    #[serde(default)]
    pub injection: bool,
}

/// The corpus.
#[derive(Debug, Clone, Deserialize)]
pub struct Corpus {
    #[serde(rename = "case")]
    pub cases: Vec<Case>,
}

impl Corpus {
    /// Parse a corpus from TOML.
    pub fn parse(text: &str) -> Result<Corpus> {
        toml::from_str(text).map_err(|e| DcError::Eval(format!("screen corpus: {e}")))
    }

    /// The corpus shipped with the crate, compiled in.
    ///
    /// `include_str!` rather than a path read, so the corpus travels with the
    /// binary and `screen-eval` works inside the container — where there is no
    /// source tree to read from.
    pub fn bundled() -> Result<Corpus> {
        Corpus::parse(include_str!("../evals/screen.toml"))
    }

    pub fn injections(&self) -> impl Iterator<Item = &Case> {
        self.cases.iter().filter(|c| c.injection)
    }
}

/// How a screener scored against the corpus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Score {
    /// Spam, correctly quarantined.
    pub true_positive: usize,
    /// **Legitimate, wrongly quarantined.** The failure that matters.
    pub false_positive: usize,
    /// Spam that got through. One wasted drafting run.
    pub false_negative: usize,
    /// Legitimate, correctly admitted.
    pub true_negative: usize,
    /// The ids of every legitimate request that was quarantined, so a report can
    /// name them rather than only counting them.
    pub wrongly_held: Vec<String>,
}

impl Score {
    pub fn total(&self) -> usize {
        self.true_positive + self.false_positive + self.false_negative + self.true_negative
    }

    /// Of the things it quarantined, how many deserved it?
    ///
    /// `None` when it quarantined nothing — a ratio over zero is not 100%, it is
    /// unmeasured, and reporting it as perfect would be the most misleading
    /// number in the report.
    pub fn precision(&self) -> Option<f64> {
        let held = self.true_positive + self.false_positive;
        (held > 0).then(|| self.true_positive as f64 / held as f64)
    }

    /// Of the spam present, how much did it catch?
    pub fn recall(&self) -> Option<f64> {
        let spam = self.true_positive + self.false_negative;
        (spam > 0).then(|| self.true_positive as f64 / spam as f64)
    }
}

/// Score a screener against a corpus.
///
/// Works against any [`Screener`], so the offline tests drive a scripted one and
/// `screen-eval` drives the real thing through the identical path — the scoring
/// cannot disagree between them.
pub fn score(screener: &dyn Screener, corpus: &Corpus) -> Score {
    let mut s = Score::default();
    for case in &corpus.cases {
        match (case.label, screener.screen(&case.text)) {
            (Label::Spam, Verdict::Quarantine) => s.true_positive += 1,
            (Label::Spam, Verdict::Admit) => s.false_negative += 1,
            (Label::Ok, Verdict::Quarantine) => {
                s.false_positive += 1;
                s.wrongly_held.push(case.id.clone());
            }
            (Label::Ok, Verdict::Admit) => s.true_negative += 1,
        }
    }
    s
}

/// A human-readable report.
pub fn report(s: &Score) -> String {
    let pct = |v: Option<f64>| match v {
        Some(x) => format!("{:.0}%", x * 100.0),
        None => "n/a".to_string(),
    };
    let mut out = format!(
        "screened {} cases\n\
         \n  caught spam      {:>3}\n  missed spam      {:>3}\n\
         \n  precision        {}\n  recall           {}\n",
        s.total(),
        s.true_positive,
        s.false_negative,
        pct(s.precision()),
        pct(s.recall()),
    );
    if s.wrongly_held.is_empty() {
        out.push_str("\nNo legitimate request was held.\n");
    } else {
        out.push_str(&format!(
            "\n{} legitimate request(s) WRONGLY HELD — the failure that matters, \
             because a real person is told their report went through:\n",
            s.wrongly_held.len()
        ));
        for id in &s.wrongly_held {
            out.push_str(&format!("  · {id}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::{testing::Scripted, AdmitAll};

    #[test]
    fn the_bundled_corpus_parses_and_covers_all_three_kinds() {
        // A corpus that lost its injection cases, or its legitimate ones, would
        // still "pass" every accuracy run while measuring nothing useful.
        let c = Corpus::bundled().expect("the shipped corpus parses");
        assert!(c.cases.len() >= 15, "{} cases", c.cases.len());

        let ok = c.cases.iter().filter(|x| x.label == Label::Ok).count();
        let spam = c.cases.iter().filter(|x| x.label == Label::Spam).count();
        let inj = c.injections().count();
        assert!(ok >= 5, "legitimate requests: {ok}");
        assert!(spam >= 4, "spam: {spam}");
        assert!(inj >= 4, "injection attempts: {inj}");
    }

    #[test]
    fn every_case_has_a_unique_id() {
        // Duplicated ids make a report ambiguous about which case failed.
        let c = Corpus::bundled().unwrap();
        let mut ids: Vec<&str> = c.cases.iter().map(|x| x.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before);
    }

    #[test]
    fn the_corpus_includes_a_legitimate_request_about_spam() {
        // The `contains("SPAM")` bug as a corpus entry: a real report is allowed
        // to be about spam, and a filter that holds it is broken.
        let c = Corpus::bundled().unwrap();
        assert!(
            c.cases
                .iter()
                .any(|x| x.label == Label::Ok && x.text.contains("SPAM")),
            "the corpus must cover a legitimate request that mentions spam"
        );
    }

    #[test]
    fn a_screener_that_admits_everything_holds_nothing_legitimate() {
        // The fail-open posture, scored: switched off, or unreachable, it misses
        // all the spam and wrongly holds nobody. That is the intended failure.
        let c = Corpus::bundled().unwrap();
        let s = score(&AdmitAll, &c);
        assert_eq!(s.false_positive, 0);
        assert_eq!(s.true_positive, 0);
        assert!(s.false_negative > 0);
        // And precision is reported as unmeasured, not as a perfect score.
        assert_eq!(s.precision(), None);
    }

    #[test]
    fn a_screener_that_quarantines_everything_is_scored_as_the_disaster_it_is() {
        // The opposite failure — a model stuck on SPAM — must show up loudly
        // rather than as a high recall number.
        let c = Corpus::bundled().unwrap();
        let s = score(&Scripted::always(Verdict::Quarantine), &c);
        assert!(s.false_positive > 0);
        assert_eq!(s.recall(), Some(1.0), "recall alone looks perfect");

        let text = report(&s);
        assert!(text.contains("WRONGLY HELD"), "{text}");
        assert!(text.contains("told their report went through"), "{text}");
    }

    #[test]
    fn the_report_never_claims_perfect_precision_for_holding_nothing() {
        // A ratio over zero is unmeasured, not 100%. Reporting it as perfect
        // would be the most misleading number in the whole report.
        let s = Score::default();
        assert_eq!(s.precision(), None);
        assert!(report(&s).contains("n/a"));
    }

    #[test]
    fn scoring_names_which_legitimate_requests_were_held() {
        // A count tells you something is wrong; the ids tell you what to look at.
        let c = Corpus::bundled().unwrap();
        let s = score(&Scripted::always(Verdict::Quarantine), &c);
        assert_eq!(s.wrongly_held.len(), s.false_positive);
        assert!(s.wrongly_held.iter().any(|id| id.starts_with("ok-")));
    }

    #[test]
    fn injection_cases_carry_no_special_scoring_power() {
        // Labelled by what the text IS, not by what it asks to be called. An
        // injection dressed as a bug report is still spam when there is no bug
        // in it — otherwise the corpus rewards the model for believing the text.
        let c = Corpus::bundled().unwrap();
        for case in c.injections() {
            assert_eq!(case.label, Label::Spam, "{}", case.id);
        }
    }
}
