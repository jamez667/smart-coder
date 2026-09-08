// Contract test for the retry budget. FROZEN: a solver must not modify this file.
#[path = "report.rs"]
mod report;

use report::admin;
use report::admin::queue;
use report::admin::queue::budget::{plan, Budget};

// `plan` must now take the deadline the schedule has to fit inside, and never
// plan a schedule that overruns it: attempts are cut until
// `attempts as u64 * backoff_ms <= deadline_ms`, with a floor of one attempt.

#[test]
fn a_generous_deadline_leaves_the_old_plan_alone() {
    // weight 1: 5 attempts, 100ms backoff = 500ms, fits in 30s.
    assert_eq!(plan(1, 30_000), Budget { attempts: 5, backoff_ms: 100 });
}

#[test]
fn a_tight_deadline_cuts_attempts() {
    // weight 1 wants 5 x 100ms = 500ms; only 250ms available => 2 attempts.
    assert_eq!(plan(1, 250), Budget { attempts: 2, backoff_ms: 100 });
}

#[test]
fn one_attempt_is_the_floor() {
    // weight 8: backoff 800ms, and 100ms of budget. Still one attempt.
    assert_eq!(plan(8, 100), Budget { attempts: 1, backoff_ms: 800 });
}

// --- the four call sites, each with its own right answer ---

#[test]
fn an_ordinary_job_uses_the_deadline_its_caller_passed() {
    // The caller says 250ms, so the job gets the 250ms plan, not the 30s one.
    let job = queue::enqueue("resize", 1, 250);
    assert_eq!(job.budget, Budget { attempts: 2, backoff_ms: 100 });

    // ...and a different caller deadline gives a different plan, so the value
    // really is threaded through rather than hard-coded.
    let job = queue::enqueue("resize", 1, 30_000);
    assert_eq!(job.budget, Budget { attempts: 5, backoff_ms: 100 });
}

#[test]
fn a_probe_is_bound_by_the_liveness_window() {
    // PROBE_DEADLINE_MS is 1000ms; weight 1 backs off 100ms, so 5 attempts
    // (500ms) still fit -- but the deadline passed must be the probe's own,
    // which the next assertion pins.
    let job = queue::enqueue_probe("ping");
    assert_eq!(job.weight, 1);
    assert_eq!(job.budget, plan(1, queue::PROBE_DEADLINE_MS));
    // A probe must NOT be planned against the admin or status-page deadline.
    assert_ne!(queue::PROBE_DEADLINE_MS, admin::ADMIN_DEADLINE_MS);
}

#[test]
fn a_probe_schedule_fits_inside_one_second() {
    let job = queue::enqueue_probe("ping");
    let span = job.budget.attempts as u64 * job.budget.backoff_ms;
    assert!(span <= queue::PROBE_DEADLINE_MS, "probe schedule {span}ms overruns the liveness window");
}

#[test]
fn an_admin_rebuild_uses_the_five_minute_deadline() {
    // weight 8 => 2 attempts, 800ms backoff = 1600ms, well inside 300s.
    assert_eq!(admin::rebuild_budget(), Budget { attempts: 2, backoff_ms: 800 });
    assert_eq!(admin::rebuild_budget(), plan(admin::ADMIN_WEIGHT, admin::ADMIN_DEADLINE_MS));
}

#[test]
fn the_status_page_summarises_against_its_own_refresh_window() {
    // 30s refresh: weight 8 wants 2 x 800ms = 1600ms, which fits.
    assert_eq!(report::summarise(8), "2x/800ms");
    // But a weight the refresh window cannot hold is cut. weight 200 backs off
    // 20000ms; two attempts would be 40s > 30s, so one attempt.
    assert_eq!(report::summarise(200), "1x/20000ms");
    // Pin it to the page's own constant, not to some other module's.
    let b = plan(200, report::STATUS_PAGE_DEADLINE_MS);
    assert_eq!(report::summarise(200), format!("{}x/{}ms", b.attempts, b.backoff_ms));
}
