//! The known-answer test for the SWE-bench pipeline.
//!
//! `#[ignore]` because it needs Docker, a ~2.8GB image pull and the network —
//! `scripts/check.sh` runs `cargo test --workspace` on machines that have none of
//! those, and a gate that fails for lack of a daemon is a gate people stop running.
//!
//!     cargo test -p sc-eval --test swebench_live -- --ignored --nocapture
//!
//! What it establishes, before any model is involved: the gold patch scores
//! **resolved**, and changing nothing scores **unresolved**. A harness that cannot
//! separate those two is not measuring a model, and every number it produces is noise.

use sc_eval::swebench::{run_instance, GoldPatchSolver, NoopSweSolver, Subset};

/// The smallest instance in the subset: 2 FAIL_TO_PASS, 6 PASS_TO_PASS, one test file.
const INSTANCE: &str = "pylint-dev__pylint-6506";

fn subset() -> Subset {
    Subset::bundled().expect("the bundled subset parses")
}

/// Doing nothing leaves the instance red — the floor of the measurement.
#[test]
#[ignore = "needs docker + the instance image"]
fn an_untouched_instance_does_not_resolve() {
    let s = subset();
    let inst = s.get(INSTANCE).expect("instance is in the subset");

    let run = run_instance(inst, &NoopSweSolver);
    println!("{}: {}", run.instance_id, run.score.line());

    assert!(
        run.harness_error.is_none(),
        "the harness itself failed: {:?}",
        run.harness_error
    );
    assert!(!run.resolved, "doing nothing cannot resolve an instance");
    // Red means the fix is absent, NOT that the rest of the suite is broken.
    assert_eq!(run.score.f2p_passed.len(), 0);
    assert_eq!(run.score.f2p_failed.len(), 2);
    assert_eq!(run.score.p2p_passed.len(), 6);
    assert!(run.score.missing.is_empty(), "every test was accounted for");
}

/// The correct fix scores resolved. Without this the pipeline could be uniformly
/// broken and every model would simply score zero, which looks like a hard benchmark
/// rather than a broken one.
#[test]
#[ignore = "needs docker + the instance image + network"]
fn the_gold_patch_resolves_the_instance() {
    let s = subset();
    let inst = s.get(INSTANCE).expect("instance is in the subset");

    // Fetched, not vendored: the gold patch is the answer, and it must not sit in a
    // file next to the tasks where a context builder could pick it up.
    let gold = fetch_gold_patch(INSTANCE);
    let solver = GoldPatchSolver::new(gold);

    let run = run_instance(inst, &solver);
    println!("{}: {}", run.instance_id, run.score.line());

    assert!(solver.applied(), "the gold patch applied to the workspace");
    assert!(
        run.harness_error.is_none(),
        "the harness itself failed: {:?}",
        run.harness_error
    );
    assert!(
        run.tampered.is_none(),
        "the gold patch touches no test file"
    );
    assert!(
        run.resolved,
        "the gold patch must resolve the instance: {}",
        run.score.line()
    );
    assert_eq!(run.score.f2p_failed.len(), 0);
    assert_eq!(run.score.p2p_broken.len(), 0);
}

/// The `patch` column of the dataset row — the upstream fix.
fn fetch_gold_patch(instance_id: &str) -> String {
    for off in (0..300).step_by(100) {
        let url = format!(
            "https://datasets-server.huggingface.co/rows\
             ?dataset=princeton-nlp%2FSWE-bench_Lite&config=default&split=test\
             &offset={off}&length=100"
        );
        let body: serde_json::Value = ureq::get(&url)
            .call()
            .expect("dataset reachable")
            .body_mut()
            .read_json()
            .expect("dataset is json");
        for row in body["rows"].as_array().expect("rows") {
            let r = &row["row"];
            if r["instance_id"].as_str() == Some(instance_id) {
                return r["patch"].as_str().expect("patch column").to_string();
            }
        }
    }
    panic!("{instance_id} not found in SWE-bench Lite");
}
