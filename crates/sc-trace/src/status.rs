//! The status lattice and the tally.
//!
//! This is the honesty layer of the crate, and it is copied deliberately from
//! `sc-comply`'s ([13](../../../docs/specs/13-compliance-evidence.md)): a check
//! that cannot determine something must **say so**, not guess. The reasoning
//! transfers exactly, and it was hard-won there.
//!
//! Two properties are load-bearing:
//!
//! * [`Unknown`](ClaimStatus::Unknown) is first-class and is never coerced into
//!   [`Ok`](ClaimStatus::Ok). Collapsing it is how a checker starts lying
//!   quietly — it turns "we could not look" into "we looked and it was fine".
//! * There is **no headline score**. A "94% traceable" number invites exactly
//!   the misreading spec 13 refuses: the missing 6% is where the drift is.
//!   [`Tally`] therefore exposes counts and nothing that blends them.

use serde::{Deserialize, Serialize};

/// What became of one anchored claim.
///
/// The variant order is load-bearing: `Ord` is derived, so aggregating a set of
/// claims is literally `.max()` over this lattice — worst wins.
///
/// Two orderings are worth justifying:
///
/// - `Ungoverned` sits just above `Ok` because it *warns* rather than fails. A
///   crate no spec mentions is a documentation gap, not a false document, and
///   failing a build for it would block legitimate work-in-progress.
/// - `Broken` outranks `Stale` because a dangling anchor means we could not even
///   evaluate the claim. A spec asserting something false about code that exists
///   is bad; a spec pointing at code that is *gone* is worse, and unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClaimStatus {
    /// The anchor resolved and its assertion (if any) holds.
    Ok,
    /// A crate no spec mentions. Reported at crate granularity only — pitched
    /// finer this produces the noise that gets a check deleted (spec 17).
    Ungoverned,
    /// The anchor could not be resolved **because the checker is limited** — an
    /// unsupported language, an ambiguous name, a macro-generated symbol.
    ///
    /// Distinct from [`Broken`](ClaimStatus::Broken), and never counted as
    /// passing. "We could not look" and "it is not there" are different claims.
    Unknown,
    /// The anchor resolved but its assertion is false — `len=5` against six
    /// members. The highest-value finding, and the one that would have caught
    /// the phase drift this spec was written for.
    Stale,
    /// The anchor names a symbol or path that no longer exists. Deterministic,
    /// unambiguous, and always an error: either the spec is stale or the anchor
    /// is wrong, and both need a human.
    Broken,
}

impl ClaimStatus {
    /// A short label for tables and JSON.
    pub fn label(self) -> &'static str {
        match self {
            ClaimStatus::Ok => "ok",
            ClaimStatus::Ungoverned => "ungoverned",
            ClaimStatus::Unknown => "unknown",
            ClaimStatus::Stale => "stale",
            ClaimStatus::Broken => "broken",
        }
    }

    /// Sort key for the report: problems first, then what needs a human, then
    /// the good news. Never sort a drift table by spec id — the reader wants the
    /// drift at the top.
    pub fn report_order(self) -> u8 {
        match self {
            ClaimStatus::Broken => 0,
            ClaimStatus::Stale => 1,
            ClaimStatus::Unknown => 2,
            ClaimStatus::Ungoverned => 3,
            ClaimStatus::Ok => 4,
        }
    }

    /// Does this status make `trace --check` exit non-zero?
    ///
    /// `Broken` and `Stale` only. `Unknown` must not gate: it means the checker
    /// could not look, and failing a build over the checker's own limits teaches
    /// people to bypass it. `Ungoverned` warns by design (spec 17) — adding a
    /// crate and its spec in one commit is good practice, but a hard failure
    /// would block legitimate work-in-progress.
    pub fn fails_check(self) -> bool {
        matches!(self, ClaimStatus::Broken | ClaimStatus::Stale)
    }
}

impl std::fmt::Display for ClaimStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Counts per status.
///
/// Deliberately **not** a percentage, and deliberately without any method that
/// blends these into one number. "94% traceable" lets a reader stop reading, and
/// the 6% is the entire point of the tool. Reporting counts is fine and useful;
/// a single blended figure is banned (spec 13/17).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    pub ok: usize,
    pub broken: usize,
    pub stale: usize,
    pub unknown: usize,
    pub ungoverned: usize,
}

impl Tally {
    /// Count a run of statuses.
    pub fn of(statuses: impl IntoIterator<Item = ClaimStatus>) -> Tally {
        let mut t = Tally::default();
        for s in statuses {
            t.add(s);
        }
        t
    }

    pub fn add(&mut self, status: ClaimStatus) {
        match status {
            ClaimStatus::Ok => self.ok += 1,
            ClaimStatus::Broken => self.broken += 1,
            ClaimStatus::Stale => self.stale += 1,
            ClaimStatus::Unknown => self.unknown += 1,
            ClaimStatus::Ungoverned => self.ungoverned += 1,
        }
    }

    /// Everything counted.
    pub fn total(&self) -> usize {
        self.ok + self.broken + self.stale + self.unknown + self.ungoverned
    }

    /// How many findings would fail `trace --check`.
    pub fn blocking(&self) -> usize {
        self.broken + self.stale
    }

    /// The one-line summary. Counts, separated — never a ratio.
    pub fn summary_line(&self) -> String {
        format!(
            "{} ok · {} broken · {} stale · {} unknown · {} ungoverned",
            self.ok, self.broken, self.stale, self.unknown, self.ungoverned
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregation_is_max_over_the_lattice() {
        // Worst wins, so a single Broken dominates any number of Ok claims.
        let worst = [ClaimStatus::Ok, ClaimStatus::Broken, ClaimStatus::Ok]
            .into_iter()
            .max()
            .unwrap();
        assert_eq!(worst, ClaimStatus::Broken);

        // Broken outranks Stale: a dangling anchor could not even be evaluated.
        assert!(ClaimStatus::Broken > ClaimStatus::Stale);
        // Stale outranks Unknown: a false document beats an unreadable one.
        assert!(ClaimStatus::Stale > ClaimStatus::Unknown);
        // And Unknown outranks Ungoverned, which only ever warns.
        assert!(ClaimStatus::Unknown > ClaimStatus::Ungoverned);
        assert!(ClaimStatus::Ungoverned > ClaimStatus::Ok);
    }

    #[test]
    fn unknown_is_never_counted_as_ok() {
        // The anti-lying test. `Unknown` means the checker could not look, and
        // folding it into `Ok` would report a blind spot as a clean result.
        let t = Tally::of([ClaimStatus::Ok, ClaimStatus::Unknown, ClaimStatus::Unknown]);
        assert_eq!(t.ok, 1);
        assert_eq!(t.unknown, 2);
        assert_eq!(t.total(), 3);
    }

    #[test]
    fn only_broken_and_stale_fail_the_check() {
        assert!(ClaimStatus::Broken.fails_check());
        assert!(ClaimStatus::Stale.fails_check());
        // The checker's own limits must never fail a build.
        assert!(!ClaimStatus::Unknown.fails_check());
        // Ungoverned warns by design (spec 17).
        assert!(!ClaimStatus::Ungoverned.fails_check());
        assert!(!ClaimStatus::Ok.fails_check());
    }

    #[test]
    fn blocking_counts_exactly_what_gates() {
        let t = Tally::of([
            ClaimStatus::Ok,
            ClaimStatus::Broken,
            ClaimStatus::Stale,
            ClaimStatus::Unknown,
            ClaimStatus::Ungoverned,
        ]);
        assert_eq!(t.blocking(), 2, "broken + stale, nothing else");
    }

    #[test]
    fn the_report_sorts_problems_first() {
        let mut statuses = vec![
            ClaimStatus::Ok,
            ClaimStatus::Unknown,
            ClaimStatus::Broken,
            ClaimStatus::Ungoverned,
            ClaimStatus::Stale,
        ];
        statuses.sort_by_key(|s| s.report_order());
        assert_eq!(
            statuses,
            vec![
                ClaimStatus::Broken,
                ClaimStatus::Stale,
                ClaimStatus::Unknown,
                ClaimStatus::Ungoverned,
                ClaimStatus::Ok,
            ]
        );
    }

    #[test]
    fn the_summary_is_counts_and_carries_no_ratio() {
        let t = Tally::of([ClaimStatus::Ok, ClaimStatus::Broken]);
        let line = t.summary_line();
        assert!(line.contains("1 ok"), "{line}");
        assert!(line.contains("1 broken"), "{line}");
        // A blended figure lets a reader stop reading, which is the failure this
        // whole tool exists to prevent.
        assert!(!line.contains('%'), "no headline percentage: {line}");
    }

    #[test]
    fn statuses_round_trip_through_json() {
        for s in [
            ClaimStatus::Ok,
            ClaimStatus::Ungoverned,
            ClaimStatus::Unknown,
            ClaimStatus::Stale,
            ClaimStatus::Broken,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(serde_json::from_str::<ClaimStatus>(&json).unwrap(), s);
        }
    }
}
