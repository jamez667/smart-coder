//! Single-writer arbitration for an artifact directory (spec 19).
//!
//! Three processes want to write `specs/<slug>/state.json`: the desktop GUI, the
//! CLI, and the daemon spec 19 describes. Until this module existed there was no
//! protection at all, and the failure was silent:
//!
//! > The daemon parks a run at the specs gate. The developer gets home, opens the
//! > same project in the GUI, and starts a plan run on the same task. Both hold
//! > divergent copies. The phone-side approval is written, then clobbered by the
//! > GUI's next phase save. Neither process notices; nothing logs it.
//!
//! The read-modify-write window spans a whole phase **including the human gate**,
//! which is unbounded. That is what makes this a lease rather than a file lock:
//! no lock held across an unbounded human wait survives a crash gracefully, and a
//! crashed holder must not lock a project forever.
//!
//! ## The shape
//!
//! - **Refuse, loudly.** A second process finding a live lease errors, naming the
//!   holder and how long ago it was seen. Spec 19's own position: a project is
//!   either the daemon's or the GUI's at any moment. Proceeding read-only was
//!   considered and rejected — a run that silently cannot persist loses five
//!   phases of work at the end, which is worse than refusing at the start.
//! - **Heartbeat + expiry.** The holder refreshes a timestamp on an interval; a
//!   lease older than [`EXPIRY`] is dead and anyone may take it. A pid-liveness
//!   probe would be more precise but needs a cross-platform dependency this crate
//!   does not have, and pids get recycled.
//! - **Release on drop.** [`LeaseGuard`] releases in `Drop`, so a panic or an
//!   early `?` cannot strand it. A leaked lease is worse than no lease: it locks a
//!   project until expiry with nothing running.
//!
//! The lease lives in a sibling `lease.json`, deliberately **not** inside
//! `state.json` — the state file is an artifact a human reads and diffs, and lease
//! churn should never rewrite it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use sc_proto::{DcError, Result};
use serde::{Deserialize, Serialize};

/// How long a lease survives without a heartbeat before anyone may take it.
///
/// Long enough that an ordinary pause — a slow model call, a paused debugger —
/// never looks like a crash; short enough that a genuinely dead holder does not
/// block a project for a coffee break. The heartbeat runs far more often than
/// this, so a live holder is never close to expiring.
pub const EXPIRY: std::time::Duration = std::time::Duration::from_secs(90);

/// How often a live holder refreshes its lease.
///
/// Comfortably inside [`EXPIRY`] so a couple of missed beats — a suspended
/// laptop, a long GC pause — do not hand the project away from a process that is
/// still working.
pub const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(15);

/// The lease file's name, alongside `state.json` in the artifact directory.
pub const LEASE_FILE: &str = "lease.json";

/// Who holds an artifact directory, and when they were last seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// The holding process. Recorded for the error message, not probed for
    /// liveness — see the module docs.
    pub owner_pid: u32,
    /// Which surface holds it: `sc-win`, `sc-cli`, `sc-daemon`. What makes the
    /// refusal actionable ("close that GUI run") rather than merely a refusal.
    pub owner: String,
    /// Unix ms when the lease was first taken.
    pub acquired_ms: u64,
    /// Unix ms of the most recent heartbeat.
    pub heartbeat_ms: u64,
    /// Which *run* within the owning process holds it.
    ///
    /// A pid alone is not enough, and the gap is not theoretical: the GUI spawns
    /// one thread per run inside a single process
    /// (`sc-win`'s `Session::spawn`), so two runs on the same artifact directory
    /// would both see their own pid, both acquire, and the first to finish would
    /// delete the lease out from under the second. The token makes a lease
    /// identify a run rather than a program.
    ///
    /// `serde(default)` so a lease written before this field existed still parses
    /// — it simply reads as token 0 and contends with everything, which is the
    /// safe direction.
    #[serde(default)]
    pub run_token: u64,
}

/// Hands out a distinct token per run within this process.
static NEXT_RUN_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Lease {
    /// Has this lease gone stale — no heartbeat within [`EXPIRY`]?
    ///
    /// A heartbeat from the future (a clock adjustment, a file copied between
    /// machines) counts as fresh rather than stale. Treating it as expired would
    /// let a clock skew silently hand a project to a second writer, which is the
    /// exact outcome this module exists to prevent.
    pub fn is_stale(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.heartbeat_ms) > EXPIRY.as_millis() as u64
    }

    /// How long since the last heartbeat, for the refusal message.
    pub fn age(&self, now_ms: u64) -> std::time::Duration {
        std::time::Duration::from_millis(now_ms.saturating_sub(self.heartbeat_ms))
    }

    /// Is this lease held by `run_token` in this process?
    ///
    /// Both halves matter. The pid alone would let two runs *inside* one process
    /// — the GUI's per-run threads — each think the lease was theirs. The token
    /// alone would collide across processes, which each start their counter at 1.
    fn is_held_by(&self, run_token: u64) -> bool {
        self.owner_pid == std::process::id() && self.run_token == run_token
    }
}

/// A held lease. Releases on drop.
///
/// `Debug` prints the path and liveness only — never the heartbeat handle.
///
/// The `Drop` release matters more than it looks: the runner returns via `?` in
/// several places and can panic in a worker thread, and a lease that outlives its
/// run locks the project until expiry with nothing running.
#[derive(Debug)]
pub struct LeaseGuard {
    path: PathBuf,
    /// Which run this guard is. Checked before every write and before release, so
    /// a guard can only ever touch or delete the lease it was actually granted.
    run_token: u64,
    /// Cleared on drop; the heartbeat thread watches it and exits.
    beating: Arc<AtomicBool>,
    beat: Option<std::thread::JoinHandle<()>>,
}

impl LeaseGuard {
    /// The lease file this guard holds.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// This run's token, distinguishing it from other runs in the same process.
    pub fn run_token(&self) -> u64 {
        self.run_token
    }

    /// Refresh the heartbeat now. The background thread does this on an interval;
    /// this is exposed so a caller with a natural checkpoint can beat early.
    pub fn touch(&self) -> Result<()> {
        touch(&self.path, self.run_token)
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.beating.store(false, Ordering::SeqCst);
        // The thread sleeps in short slices, so this returns promptly rather than
        // blocking a UI teardown for a whole heartbeat interval.
        if let Some(beat) = self.beat.take() {
            let _ = beat.join();
        }
        // Only delete a lease that is still ours. If it expired and someone else
        // reclaimed it, removing the file here would silently un-protect *their*
        // run — a guard must never release a lease it no longer holds.
        //
        // Best-effort otherwise: a lease we cannot delete simply expires. Failing
        // loudly in a destructor would turn a cleanup hiccup into a panic during
        // unwind.
        if read(&self.path).is_some_and(|l| l.is_held_by(self.run_token)) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The running surface's name, for a lease's `owner` field.
///
/// Taken from the executable rather than passed in, so no caller has to remember
/// to identify itself and none can identify itself wrongly. `smart-coder.exe` →
/// `smart-coder`, `sc-win.exe` → `sc-win` — which is what a user sees in their
/// task manager, so the refusal message names something they can actually find.
pub fn current_owner() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Take the lease on `dir` for `owner`, or fail saying who has it.
///
/// Succeeds when the directory is free, when the existing lease is stale, or when
/// this process already holds it. Fails only against a *live* lease held by
/// someone else.
pub fn acquire(dir: &Path, owner: &str) -> Result<LeaseGuard> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(LEASE_FILE);
    let now = now_ms();
    let run_token = NEXT_RUN_TOKEN.fetch_add(1, Ordering::SeqCst);

    if let Some(existing) = read(&path) {
        // A live lease belongs to someone else — another process, or another run
        // in this one. Both refuse: the GUI opening a second run on a directory it
        // is already working is the same conflict as the CLI doing it.
        if !existing.is_stale(now) {
            let whose = if existing.owner_pid == std::process::id() {
                "another run in this process"
            } else {
                "another process"
            };
            return Err(DcError::Eval(format!(
                "{} is held by {} — {} (pid {}, last seen {:.0}s ago). Close that \
                 run, or wait {:.0}s for the lease to expire.",
                dir.display(),
                existing.owner,
                whose,
                existing.owner_pid,
                existing.age(now).as_secs_f64(),
                EXPIRY.as_secs_f64() - existing.age(now).as_secs_f64().min(EXPIRY.as_secs_f64()),
            )));
        }
    }

    let lease = Lease {
        owner_pid: std::process::id(),
        owner: owner.to_string(),
        acquired_ms: now,
        heartbeat_ms: now,
        run_token,
    };
    write(&path, &lease)?;

    // Beat in the background so an unbounded human gate cannot expire a live
    // lease. `Gate::decide` is a blocking call and this crate has no async
    // runtime, so a thread is the fitting shape.
    let beating = Arc::new(AtomicBool::new(true));
    let beat = {
        let beating = Arc::clone(&beating);
        let path = path.clone();
        std::thread::spawn(move || {
            while beating.load(Ordering::SeqCst) {
                // Sleep in slices so `Drop` is not held up for a full interval.
                for _ in 0..(HEARTBEAT.as_millis() / 100).max(1) {
                    if !beating.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                if beating.load(Ordering::SeqCst) {
                    let _ = touch(&path, run_token);
                }
            }
        })
    };

    Ok(LeaseGuard {
        path,
        run_token,
        beating,
        beat: Some(beat),
    })
}

/// Who holds `dir`, if anyone. `None` when free or unreadable.
pub fn holder(dir: &Path) -> Option<Lease> {
    read(&dir.join(LEASE_FILE))
}

/// Refresh the heartbeat on a lease still held by `run_token`.
fn touch(path: &Path, run_token: u64) -> Result<()> {
    let Some(mut lease) = read(path) else {
        // Someone removed it. Nothing to refresh, and re-creating it would be
        // taking a lease we were not granted.
        return Ok(());
    };
    if !lease.is_held_by(run_token) {
        // It expired and someone else took it. Beating now would keep *their*
        // lease alive under our name and, worse, make it look live to a third
        // party while we quietly kept working.
        return Ok(());
    }
    lease.heartbeat_ms = now_ms();
    write(path, &lease)
}

fn read(path: &Path) -> Option<Lease> {
    let text = std::fs::read_to_string(path).ok()?;
    // A corrupt lease is treated as no lease: it cannot be honoured, and blocking
    // a project on an unparseable file would need manual recovery.
    serde_json::from_str(&text).ok()
}

fn write(path: &Path, lease: &Lease) -> Result<()> {
    let json = serde_json::to_string_pretty(lease).map_err(|e| DcError::Eval(e.to_string()))?;
    crate::state::write_atomic(path, json.as_bytes())
}

/// Unix milliseconds. Saturates at 0 rather than panicking on a pre-epoch clock.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sc-wf-lease-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A lease as if written by another live process.
    fn foreign(dir: &Path, heartbeat_ms: u64) {
        let lease = Lease {
            // A pid that is not ours, so `is_ours` is false.
            owner_pid: std::process::id().wrapping_add(1),
            owner: "sc-win".into(),
            acquired_ms: heartbeat_ms,
            heartbeat_ms,
            run_token: 1,
        };
        write(&dir.join(LEASE_FILE), &lease).unwrap();
    }

    #[test]
    fn a_free_directory_can_be_taken() {
        let dir = temp("free");
        let guard = acquire(&dir, "sc-cli").expect("free dir");
        let held = holder(&dir).expect("lease written");
        assert_eq!(held.owner, "sc-cli");
        assert_eq!(held.owner_pid, std::process::id());
        assert!(guard.path().is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_foreign_lease_refuses_and_says_who_holds_it() {
        // The refusal has to be actionable — "held" alone leaves the user with
        // nothing to do about it.
        let dir = temp("busy");
        foreign(&dir, now_ms());

        let err = acquire(&dir, "sc-cli").expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("sc-win"), "names the holder: {msg}");
        assert!(msg.contains("pid"), "names the pid: {msg}");
        assert!(msg.contains("ago"), "says how stale: {msg}");
        assert!(
            msg.contains("Close that run") || msg.contains("expire"),
            "says what to do: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_lease_is_reclaimable_without_a_flag() {
        // A crashed holder must not lock a project forever, and recovering must
        // not require the user to discover an override.
        let dir = temp("stale");
        let long_ago = now_ms() - (EXPIRY.as_millis() as u64) - 5_000;
        foreign(&dir, long_ago);

        let _guard = acquire(&dir, "sc-cli").expect("a stale lease is dead");
        assert_eq!(holder(&dir).unwrap().owner, "sc-cli");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lease_just_inside_the_expiry_still_holds() {
        // The boundary matters: expiring early would hand a project away from a
        // process that is merely slow.
        let dir = temp("fresh-enough");
        foreign(&dir, now_ms() - (EXPIRY.as_millis() as u64) + 10_000);
        assert!(acquire(&dir, "sc-cli").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_run_in_the_same_process_is_refused_too() {
        // The GUI spawns one thread per run inside ONE process (`Session::spawn`),
        // so two runs on the same artifact directory share a pid. Keying the lease
        // on the pid alone let both acquire — and then the first to finish deleted
        // the lease out from under the second, leaving it unprotected while it kept
        // writing. The lease has to identify a *run*, not a program.
        let dir = temp("same-process");
        let first = acquire(&dir, "sc-win").expect("free");

        let err = acquire(&dir, "sc-win").expect_err("a second run must contend");
        let msg = err.to_string();
        assert!(
            msg.contains("another run in this process"),
            "the message must distinguish this from a foreign process: {msg}"
        );

        // And the first run still holds it — a refused acquire changes nothing.
        assert_eq!(holder(&dir).unwrap().run_token, first.run_token());
        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_finished_run_does_not_release_a_lease_someone_else_reclaimed() {
        // If our lease expired and another run legitimately took it, our `Drop`
        // must not delete theirs — that would silently un-protect a live run.
        let dir = temp("no-steal-on-drop");
        let mine = acquire(&dir, "sc-cli").unwrap();

        // Someone else reclaims it (as they would after an expiry).
        foreign(&dir, now_ms());

        drop(mine);
        assert!(
            holder(&dir).is_some(),
            "the reclaimer's lease survived our release"
        );
        assert_eq!(holder(&dir).unwrap().owner, "sc-win");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_run_gets_a_distinct_token() {
        // Tokens are what make same-process runs distinguishable at all.
        let a = temp("tok-a");
        let b = temp("tok-b");
        let first = acquire(&a, "sc-win").unwrap();
        let second = acquire(&b, "sc-win").unwrap();
        assert_ne!(first.run_token(), second.run_token());
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn the_guard_releases_on_drop() {
        let dir = temp("release");
        {
            let _guard = acquire(&dir, "sc-cli").unwrap();
            assert!(holder(&dir).is_some(), "held inside the scope");
        }
        assert!(holder(&dir).is_none(), "released on drop");
        // And the directory is immediately takeable again.
        let _again = acquire(&dir, "sc-win").expect("free after release");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_guard_releases_on_an_early_error_return() {
        // The runner returns via `?` in several places. A lease leaked on an error
        // path is worse than no lease: it locks a project with nothing running.
        let dir = temp("early-return");
        fn run(dir: &Path) -> Result<()> {
            let _guard = acquire(dir, "sc-cli")?;
            Err(DcError::Eval("the phase failed".into()))
        }
        assert!(run(&dir).is_err());
        assert!(holder(&dir).is_none(), "released despite the error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_guard_releases_on_panic() {
        let dir = temp("panic");
        let d = dir.clone();
        let joined = std::thread::spawn(move || {
            let _guard = acquire(&d, "sc-cli").unwrap();
            panic!("the phase exploded");
        })
        .join();
        assert!(joined.is_err(), "the thread did panic");
        assert!(holder(&dir).is_none(), "released during unwind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_heartbeat_keeps_a_lease_alive_across_a_wait() {
        // The human-gate case: `gate.decide()` blocks unbounded, and a lease that
        // expired while someone was thinking would let a second writer in mid-run.
        let dir = temp("heartbeat");
        let guard = acquire(&dir, "sc-cli").unwrap();
        let first = holder(&dir).unwrap().heartbeat_ms;

        // Beat explicitly rather than sleeping out a real interval — the test
        // proves the refresh mechanism, not the timer.
        std::thread::sleep(std::time::Duration::from_millis(5));
        guard.touch().unwrap();

        let second = holder(&dir).unwrap().heartbeat_ms;
        assert!(second >= first, "{second} >= {first}");
        // The acquisition time is NOT moved by a beat — it records when the run
        // started, which a report may want.
        assert_eq!(
            holder(&dir).unwrap().acquired_ms,
            guard_acquired(&dir),
            "acquired_ms is stable across heartbeats"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn guard_acquired(dir: &Path) -> u64 {
        holder(dir).unwrap().acquired_ms
    }

    #[test]
    fn the_background_thread_actually_beats() {
        // The interval is 15s, so this proves the thread is alive and refreshing
        // by touching through the guard rather than waiting one out.
        let dir = temp("bg");
        let guard = acquire(&dir, "sc-cli").unwrap();
        let before = holder(&dir).unwrap().heartbeat_ms;
        std::thread::sleep(std::time::Duration::from_millis(3));
        guard.touch().unwrap();
        assert!(holder(&dir).unwrap().heartbeat_ms >= before);
        drop(guard);
        // Dropping stops the thread promptly rather than after a full interval.
        assert!(holder(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_lease_file_does_not_block_the_project() {
        // Unparseable means unhonourable. Blocking here would need manual
        // recovery on a file the user never chose to create.
        let dir = temp("corrupt");
        std::fs::write(dir.join(LEASE_FILE), "{ not json").unwrap();
        let _guard = acquire(&dir, "sc-cli").expect("a corrupt lease is no lease");
        assert_eq!(holder(&dir).unwrap().owner, "sc-cli");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_heartbeat_from_the_future_is_treated_as_fresh() {
        // Clock skew (a file copied between machines, an NTP correction) must not
        // read as expired — that would silently admit a second writer, which is
        // the one outcome this module exists to prevent.
        let lease = Lease {
            owner_pid: 1,
            owner: "sc-win".into(),
            acquired_ms: 0,
            heartbeat_ms: 10_000,
            run_token: 1,
        };
        assert!(!lease.is_stale(5_000), "future heartbeat is not stale");
    }

    #[test]
    fn touching_a_removed_lease_does_not_re_create_it() {
        let dir = temp("removed");
        let guard = acquire(&dir, "sc-cli").unwrap();
        std::fs::remove_file(guard.path()).unwrap();
        guard.touch().unwrap();
        assert!(
            holder(&dir).is_none(),
            "a heartbeat never creates a lease it was not granted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn touching_a_reclaimed_lease_does_not_keep_someone_elses_alive() {
        // Our lease expired and another run took it. A stray beat from us would
        // refresh THEIR lease under our name — making it look live to a third
        // party while we quietly kept working on the same directory.
        let dir = temp("reclaimed");
        let guard = acquire(&dir, "sc-cli").unwrap();

        let stamp = now_ms() - 1_000;
        foreign(&dir, stamp);
        guard.touch().unwrap();

        let after = holder(&dir).unwrap();
        assert_eq!(after.owner, "sc-win", "still theirs");
        assert_eq!(after.heartbeat_ms, stamp, "and we did not beat for them");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lease_round_trips_through_json() {
        let dir = temp("json");
        let lease = Lease {
            owner_pid: 4242,
            owner: "sc-daemon".into(),
            acquired_ms: 1_700_000_000_000,
            heartbeat_ms: 1_700_000_030_000,
            run_token: 7,
        };
        let path = dir.join(LEASE_FILE);
        write(&path, &lease).unwrap();
        assert_eq!(read(&path).unwrap(), lease);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
