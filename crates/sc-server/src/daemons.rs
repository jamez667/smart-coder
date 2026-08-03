//! Which daemons are polling, and what they offer to serve.
//!
//! Once the server hands a daemon only the repositories it declared, a request
//! for a repository **nobody** declares waits for ever. That is the right
//! behaviour — the alternative is handing it to a machine that cannot do it, and
//! that used to destroy the request — but "waiting for a daemon to pick it up"
//! is a true and useless thing to tell somebody whose request will never move.
//!
//! So the server remembers who asked for what, and the page says which case this
//! is: nothing has connected at all, or something has but not for this
//! repository. The two need different answers from the operator, and guessing
//! between them is most of the work of diagnosing a queue that will not drain.
//!
//! ## Why not a request state
//!
//! Because it is not a fact about the request. It is a fact about who happens to
//! be polling this minute, so a daemon coming back would have to un-set it and
//! the state would flap. A `RequestState` variant is also expensive: it touches
//! the label, the list order, the claimable gate, two i18n catalogues and an
//! exhaustive match that exists to force exactly that review — a lot of blast
//! radius for something derivable.
//!
//! ## Why in memory
//!
//! This describes who is polling **now**, a fact with a lifetime of seconds.
//! Persisting it would survive a restart as a confident claim about daemons that
//! are no longer running, and a page built on that would mislead rather than
//! inform. Losing it on restart is correct: every live daemon re-declares itself
//! within one poll.

use std::collections::HashMap;

/// How long a poll counts as evidence a daemon is alive.
///
/// Comfortably longer than one hold (`wire::POLL_TIMEOUT` is 30s), so a daemon
/// part-way through a long poll is never reported missing; short enough that one
/// switched off disappears from the page in a couple of minutes rather than
/// being remembered as present indefinitely.
pub const FRESH_MS: u64 = 5 * 60 * 1000;

/// Whether anything is offering to serve a given repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// No daemon has polled recently at all. The operator needs to start one —
    /// which is a different problem from the one below, and telling them to add
    /// a repository would send them to the wrong place.
    NoDaemonSeen,
    /// A daemon is polling and offers this repository.
    Served,
    /// Daemons are polling, but none offers this one. Usually a repository name
    /// that does not match what `queue add-repo` was given — including the
    /// `SC_SERVER_PUBLIC_REPO` mismatch, which is the way this system most often
    /// wedges.
    Unserved,
}

/// What the server has recently seen daemons offer.
#[derive(Debug, Default)]
pub struct Seen {
    /// Keyed on the daemon's label, so a machine polling again replaces its own
    /// entry rather than accumulating one per poll.
    by_label: HashMap<String, Entry>,
}

#[derive(Debug)]
struct Entry {
    repos: Vec<String>,
    /// `None` when the daemon declared nothing — an older build. It is serving
    /// *something*, but the server cannot say what, so it cannot be counted as
    /// evidence for or against any particular repository.
    declared: bool,
    last_ms: u64,
}

impl Seen {
    /// Record a poll.
    pub fn saw(&mut self, label: &str, repos: &[String], declared: bool, now_ms: u64) {
        self.by_label.insert(
            label.to_string(),
            Entry {
                repos: repos.to_vec(),
                declared,
                last_ms: now_ms,
            },
        );
    }

    /// Is anything currently offering to serve `repo`?
    pub fn coverage(&self, repo: &str, now_ms: u64) -> Coverage {
        let mut any_fresh = false;
        for entry in self.by_label.values() {
            // `saturating_sub` for the same reason `reclaim_stale` uses it: a
            // clock that jumped backwards must not silently expire a daemon that
            // polled a moment ago.
            if now_ms.saturating_sub(entry.last_ms) > FRESH_MS {
                continue;
            }
            any_fresh = true;
            // A daemon that declared nothing might serve anything, so it cannot
            // be used to prove a repository *unserved*. Treated as covering
            // everything, which matches what it is actually handed.
            if !entry.declared || entry.repos.iter().any(|r| r == repo) {
                return Coverage::Served;
            }
        }
        if any_fresh {
            Coverage::Unserved
        } else {
            Coverage::NoDaemonSeen
        }
    }

    /// Every repository a live daemon offers, sorted, for telling an operator
    /// what *is* on offer when the one they wanted is not.
    pub fn offered(&self, now_ms: u64) -> Vec<String> {
        let mut names: Vec<String> = self
            .by_label
            .values()
            .filter(|e| now_ms.saturating_sub(e.last_ms) <= FRESH_MS)
            .flat_map(|e| e.repos.iter().cloned())
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repos(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_repository_a_daemon_offers_is_served() {
        let mut seen = Seen::default();
        seen.saw("laptop", &repos(&["alpha", "beta"]), true, 1_000);
        assert_eq!(seen.coverage("alpha", 1_000), Coverage::Served);
    }

    #[test]
    fn a_repository_no_daemon_offers_is_distinguishable_from_no_daemon_at_all() {
        // The distinction the page rests on: "start a daemon" and "add this
        // repository to the one you have" send an operator to different places,
        // and getting it wrong wastes the time this exists to save.
        let mut seen = Seen::default();
        assert_eq!(seen.coverage("alpha", 1_000), Coverage::NoDaemonSeen);

        seen.saw("laptop", &repos(&["beta"]), true, 1_000);
        assert_eq!(seen.coverage("alpha", 1_000), Coverage::Unserved);
        assert_eq!(seen.coverage("beta", 1_000), Coverage::Served);
    }

    #[test]
    fn a_daemon_that_stopped_polling_stops_counting_as_serving() {
        let mut seen = Seen::default();
        seen.saw("laptop", &repos(&["alpha"]), true, 1_000);
        assert_eq!(seen.coverage("alpha", 1_000 + FRESH_MS), Coverage::Served);
        assert_eq!(
            seen.coverage("alpha", 1_000 + FRESH_MS + 1),
            Coverage::NoDaemonSeen,
            "once it goes quiet it is not evidence of anything"
        );
    }

    #[test]
    fn a_daemon_that_declared_nothing_covers_everything() {
        // An older daemon is handed everything, so it cannot be used to prove
        // any repository unserved — saying otherwise would put a "no daemon
        // serves this" note on a request that is about to be drafted.
        let mut seen = Seen::default();
        seen.saw("old", &[], false, 1_000);
        assert_eq!(seen.coverage("anything", 1_000), Coverage::Served);
    }

    #[test]
    fn polling_again_replaces_a_daemons_entry_rather_than_adding_one() {
        // Otherwise a daemon that dropped a repository would go on appearing to
        // serve it for as long as it kept polling.
        let mut seen = Seen::default();
        seen.saw("laptop", &repos(&["alpha"]), true, 1_000);
        seen.saw("laptop", &repos(&["beta"]), true, 2_000);
        assert_eq!(seen.coverage("alpha", 2_000), Coverage::Unserved);
        assert_eq!(seen.coverage("beta", 2_000), Coverage::Served);
    }

    #[test]
    fn what_is_on_offer_is_listed_without_duplicates() {
        let mut seen = Seen::default();
        seen.saw("laptop", &repos(&["alpha", "beta"]), true, 1_000);
        seen.saw("office", &repos(&["beta", "gamma"]), true, 1_000);
        assert_eq!(seen.offered(1_000), ["alpha", "beta", "gamma"]);
        // And a daemon that has gone quiet is not counted.
        assert!(seen.offered(1_000 + FRESH_MS + 1).is_empty());
    }

    #[test]
    fn a_backwards_clock_does_not_expire_a_live_daemon() {
        let mut seen = Seen::default();
        seen.saw("laptop", &repos(&["alpha"]), true, 10_000);
        assert_eq!(seen.coverage("alpha", 1), Coverage::Served);
    }
}
