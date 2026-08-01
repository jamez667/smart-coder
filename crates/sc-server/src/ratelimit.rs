//! Per-credential rate limiting.
//!
//! **Keyed on the credential, not the IP.** Behind a reverse proxy — which is how
//! this is deployed, since Portainer users put a proxy in front of everything —
//! every request arrives from the same address, so an IP-keyed limiter is either a
//! global limit that one busy device trips for everyone, or no limit at all
//! (spec 18).
//!
//! A fixed window rather than a token bucket: the thing being defended against is
//! a credential-guessing loop, and a window that says "you have made 200 requests
//! this minute, wait" defends that just as well while being something a developer
//! can reason about from the outside.

use std::collections::HashMap;

/// The window length.
const WINDOW_MS: u64 = 60_000;

/// How many requests one credential may make per window.
///
/// Generous, because the legitimate caller is a browser loading a page and its
/// assets, plus a daemon long-polling. The limit exists to stop a loop, not to
/// shape normal traffic — a limit tight enough to inconvenience real use gets
/// raised until it does nothing.
const LIMIT: u32 = 240;

/// Unauthenticated attempts get a much tighter budget: enrolment and login are
/// exactly where guessing happens, and no honest caller needs many tries.
const ANON_LIMIT: u32 = 20;

/// The bucket a caller is counted in.
///
/// Unauthenticated callers share one bucket deliberately: they have presented no
/// identity, so there is nothing to key on, and letting each *claimed* identity
/// have its own budget would let a guesser mint unlimited budgets by varying what
/// they claim.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Bucket {
    /// A known credential, keyed by its hash — never the credential itself, so a
    /// memory dump or a debug print leaks nothing.
    Credential(String),
    /// Everyone who has not authenticated.
    Anonymous,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    started_ms: u64,
    count: u32,
}

/// A fixed-window limiter.
#[derive(Debug, Default)]
pub struct RateLimiter {
    windows: HashMap<Bucket, Window>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a request. `false` means it should be refused.
    pub fn allow(&mut self, bucket: Bucket, now_ms: u64) -> bool {
        let limit = match bucket {
            Bucket::Anonymous => ANON_LIMIT,
            Bucket::Credential(_) => LIMIT,
        };
        let w = self.windows.entry(bucket).or_insert(Window {
            started_ms: now_ms,
            count: 0,
        });
        if now_ms.saturating_sub(w.started_ms) >= WINDOW_MS {
            w.started_ms = now_ms;
            w.count = 0;
        }
        w.count += 1;
        w.count <= limit
    }

    /// Drop windows that have fully expired.
    ///
    /// Without this the map grows once per distinct credential forever, which on a
    /// long-lived server is an unbounded allocation driven by whatever an attacker
    /// chooses to present.
    pub fn sweep(&mut self, now_ms: u64) {
        self.windows
            .retain(|_, w| now_ms.saturating_sub(w.started_ms) < WINDOW_MS);
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.windows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(s: &str) -> Bucket {
        Bucket::Credential(s.to_string())
    }

    #[test]
    fn normal_traffic_is_never_touched() {
        // A browser loading a page and a daemon long-polling must sail through;
        // a limit that inconveniences real use gets raised until it does nothing.
        let mut rl = RateLimiter::new();
        for i in 0..LIMIT {
            assert!(rl.allow(cred("a"), 1000 + i as u64), "request {i}");
        }
    }

    #[test]
    fn a_loop_is_stopped_once_it_passes_the_limit() {
        let mut rl = RateLimiter::new();
        for _ in 0..LIMIT {
            assert!(rl.allow(cred("a"), 1000));
        }
        assert!(!rl.allow(cred("a"), 1000), "one past the limit");
    }

    #[test]
    fn one_busy_device_does_not_throttle_another() {
        // The whole reason for keying on the credential: behind a proxy every
        // request shares one IP, so an IP-keyed limiter is a global limit.
        let mut rl = RateLimiter::new();
        for _ in 0..=LIMIT {
            rl.allow(cred("phone"), 1000);
        }
        assert!(!rl.allow(cred("phone"), 1000));
        assert!(rl.allow(cred("laptop"), 1000), "a different credential");
    }

    #[test]
    fn unauthenticated_callers_get_a_much_tighter_budget() {
        // Enrolment and login are where guessing happens, and no honest caller
        // needs many tries.
        let mut rl = RateLimiter::new();
        for _ in 0..ANON_LIMIT {
            assert!(rl.allow(Bucket::Anonymous, 1000));
        }
        assert!(!rl.allow(Bucket::Anonymous, 1000));
        const { assert!(ANON_LIMIT < LIMIT) };
    }

    #[test]
    fn everyone_unauthenticated_shares_one_bucket() {
        // Otherwise a guesser mints an unlimited budget by varying what they
        // claim to be — a per-claimed-identity limit is no limit at all.
        let mut rl = RateLimiter::new();
        for _ in 0..=ANON_LIMIT {
            rl.allow(Bucket::Anonymous, 1000);
        }
        assert!(
            !rl.allow(Bucket::Anonymous, 1000),
            "a second guesser is already over budget"
        );
    }

    #[test]
    fn the_window_rolls_so_a_throttled_caller_recovers() {
        // A limiter that never forgives locks the developer out of their own
        // server for as long as it runs.
        let mut rl = RateLimiter::new();
        for _ in 0..=LIMIT {
            rl.allow(cred("a"), 1000);
        }
        assert!(!rl.allow(cred("a"), 1000));
        assert!(rl.allow(cred("a"), 1000 + WINDOW_MS), "a fresh window");
    }

    #[test]
    fn expired_windows_are_swept_so_the_map_stays_bounded() {
        // Without this, the map grows once per distinct credential presented —
        // an unbounded allocation driven entirely by an attacker's choices.
        let mut rl = RateLimiter::new();
        for i in 0..500 {
            rl.allow(cred(&format!("guess-{i}")), 1000);
        }
        assert_eq!(rl.tracked(), 500);
        rl.sweep(1000 + WINDOW_MS);
        assert_eq!(rl.tracked(), 0);
    }

    #[test]
    fn a_bucket_holds_a_hash_and_never_a_credential() {
        // The limiter's keys end up in memory dumps and debug output; they must
        // be as unrevealing as the store on disk.
        let b = cred(&crate::auth::hash("secret-token"));
        let shown = format!("{b:?}");
        assert!(!shown.contains("secret-token"), "{shown}");
    }
}
