use super::*;
use crate::finding::Anchor;
use crate::test_support::{failing_backend, temp_repo, ScriptedReviewer};

/// A repo where a worker reimplemented `format_date` that already exists in
/// `src/utils/date.rs` — the scenario the spec is built around. Returns the
/// workspace and the integrated diff over it.
fn duplicate_repo() -> (std::path::PathBuf, IntegratedDiff) {
    let root = temp_repo("engine");
    std::fs::create_dir_all(root.join("src/utils")).unwrap();
    std::fs::create_dir_all(root.join("src/report")).unwrap();
    std::fs::write(
        root.join("src/utils/date.rs"),
        "fn other() {}\n\
         fn format_date(d: u64) -> String {\n    String::new()\n}\n",
    )
    .unwrap();
    let before = "fn render() -> String {\n    String::new()\n}\n";
    let after = "fn render() -> String {\n    format_date(0)\n}\n\
         fn format_date(d: u64) -> String {\n    let s = String::new();\n    s\n}\n";
    std::fs::write(root.join("src/report/render.rs"), after).unwrap();
    let diff = IntegratedDiff::from_changes([("src/report/render.rs", Some(before), Some(after))]);
    (root, diff)
}

fn cfg() -> ReviewConfig {
    ReviewConfig {
        enabled: true,
        min_changed_lines: 1,
        ..Default::default()
    }
}

#[test]
fn a_corroborated_duplicate_produces_an_actionable_retry_prompt() {
    // The end-to-end path the spec exists for: the index found the original
    // while building the prompt, the model agreed it is the same thing, and the
    // worker gets a named target rather than "you duplicated something".
    let (root, diff) = duplicate_repo();
    let backend = ScriptedReviewer::new(
        "qwen",
        vec![(
            "QUESTION: Does this change reimplement",
            r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"format_date",
                 "severity":"high","summary":"this already exists in utils"}]"#,
        )],
    );
    let out = review(
        &[Reviewer::new("qwen", &backend)],
        &diff,
        &root,
        "render a report",
        &[],
        &cfg(),
    );

    assert_eq!(out.findings.len(), 1, "{:?}", out.findings);
    let f = &out.findings[0];
    assert!(f.corroborated, "the index found the original");
    assert!(f.may_act());

    let feedback = out.retry_feedback().expect("actionable feedback");
    assert!(feedback.contains("format_date"), "the symbol: {feedback}");
    assert!(
        feedback.contains("src/utils/date.rs:2"),
        "AND its location: {feedback}"
    );
    // The model's prose stays in the report, out of the worker's prompt.
    assert!(!feedback.contains("already exists in utils"), "{feedback}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_uncorroborated_finding_is_reported_but_can_never_block() {
    let (root, diff) = duplicate_repo();
    // A high-severity opinion about a symbol the index cannot corroborate.
    let backend = ScriptedReviewer::new(
        "qwen",
        vec![(
            "QUESTION: Does this change match how the surrounding",
            r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"render",
                 "severity":"high","summary":"I'd have used a builder"}]"#,
        )],
    );
    let out = review(
        &[Reviewer::new("qwen", &backend)],
        &diff,
        &root,
        "render a report",
        &[],
        &cfg(),
    );

    assert_eq!(out.findings.len(), 1);
    assert!(!out.findings[0].corroborated);
    assert_eq!(out.blocking(Severity::High), 0, "taste never stops a run");
    assert_eq!(out.blocking(Severity::Low), 0);
    assert!(out.retry_feedback().is_none(), "no evidence to inject");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unreachable_reviewer_is_skipped_not_fatal() {
    // A review that failed closed on a network error would make the gate
    // hostage to an API outage.
    let (root, diff) = duplicate_repo();
    let working = ScriptedReviewer::new(
        "qwen",
        vec![(
            "QUESTION: Does this change reimplement",
            r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"format_date",
                 "severity":"high","summary":"duplicate"}]"#,
        )],
    );
    let dead = failing_backend();
    let out = review(
        &[
            Reviewer::new("qwen", &working),
            Reviewer::new("offline", &dead),
        ],
        &diff,
        &root,
        "render a report",
        &[],
        &cfg(),
    );

    assert!(!out.skipped, "the review still ran");
    assert_eq!(out.reviewers_skipped, vec![ModelId::new("offline")]);
    assert_eq!(
        out.findings.len(),
        1,
        "the reachable reviewer still reported"
    );
    // The skipped reviewer is NOT counted as having considered the diff —
    // otherwise the finding would read as contested by a model that never ran.
    assert_eq!(out.findings[0].considered_by, vec![ModelId::new("qwen")]);
    assert!(!out.findings[0].is_contested());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn two_models_flagging_different_problems_in_one_hunk_stay_two_findings() {
    let (root, diff) = duplicate_repo();
    let a = ScriptedReviewer::new(
        "qwen",
        vec![(
            "QUESTION: Does this change match how the surrounding",
            r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"render",
                 "severity":"medium","summary":"render should delegate"}]"#,
        )],
    );
    let b = ScriptedReviewer::new(
        "gemini",
        vec![(
            "QUESTION: Does this change match how the surrounding",
            r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"format_date",
                 "severity":"medium","summary":"format_date is misplaced here"}]"#,
        )],
    );
    let out = review(
        &[Reviewer::new("qwen", &a), Reviewer::new("gemini", &b)],
        &diff,
        &root,
        "render a report",
        &[],
        &cfg(),
    );

    assert_eq!(out.findings.len(), 2, "{:#?}", out.findings);
    assert!(
        out.findings.iter().all(|f| f.votes() == 1),
        "neither may claim the other's vote"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn two_models_agreeing_merge_into_one_finding_that_still_cannot_block() {
    let (root, diff) = duplicate_repo();
    let reply = r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"render",
                     "severity":"high","summary":"restructure this"}]"#;
    let a = ScriptedReviewer::new(
        "qwen",
        vec![("QUESTION: Does this change match how", reply)],
    );
    let b = ScriptedReviewer::new(
        "gemini",
        vec![("QUESTION: Does this change match how", reply)],
    );
    let out = review(
        &[Reviewer::new("qwen", &a), Reviewer::new("gemini", &b)],
        &diff,
        &root,
        "render a report",
        &[],
        &cfg(),
    );

    assert_eq!(out.findings.len(), 1);
    assert_eq!(out.findings[0].votes(), 2);
    assert_eq!(
        out.blocking(Severity::Low),
        0,
        "agreement ranks; it never converts an opinion into a fact"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_finding_whose_named_symbol_does_not_resolve_is_marked_and_drops_in_rank() {
    let (root, diff) = duplicate_repo();
    let backend = ScriptedReviewer::new(
        "qwen",
        vec![(
            "QUESTION: Does this change match how the surrounding",
            r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"render",
                 "severity":"medium","summary":"real anchor"},
                {"hunk":"H0","file":"src/report/render.rs","symbol":"never_written",
                 "severity":"medium","summary":"invented anchor"}]"#,
        )],
    );
    let out = review(
        &[Reviewer::new("qwen", &backend)],
        &diff,
        &root,
        "render a report",
        &[],
        &cfg(),
    );

    assert_eq!(out.findings.len(), 2);
    let invented = out
        .findings
        .iter()
        .find(|f| f.anchor.symbol.as_deref() == Some("never_written"))
        .expect("still reported, just ranked down");
    assert!(invented.anchor_unresolved);
    assert!(
        !out.findings[0].anchor_unresolved,
        "the real anchor ranks first"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_small_diff_is_skipped_entirely() {
    // A three-line change does not need four lenses. Skipped is distinct from
    // "ran and found nothing".
    let (root, diff) = duplicate_repo();
    let backend = ScriptedReviewer::new("qwen", vec![]);
    let out = review(
        &[Reviewer::new("qwen", &backend)],
        &diff,
        &root,
        "goal",
        &[],
        &ReviewConfig {
            enabled: true,
            min_changed_lines: 500,
            ..Default::default()
        },
    );
    assert!(out.skipped);
    assert!(out.findings.is_empty());
    assert!(
        backend.seen.lock().unwrap().is_empty(),
        "no model call was paid for"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn review_is_off_by_default() {
    let (root, diff) = duplicate_repo();
    let backend = ScriptedReviewer::new("qwen", vec![]);
    let out = review(
        &[Reviewer::new("qwen", &backend)],
        &diff,
        &root,
        "goal",
        &[],
        &ReviewConfig::default(),
    );
    assert!(out.skipped);
    assert!(backend.seen.lock().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn every_lens_is_asked_and_each_gets_its_grounding() {
    let (root, diff) = duplicate_repo();
    let backend = ScriptedReviewer::new("qwen", vec![]);
    let _ = review(
        &[Reviewer::new("qwen", &backend)],
        &diff,
        &root,
        "render a report",
        &[],
        &cfg(),
    );

    let seen = backend.seen.lock().unwrap();
    assert_eq!(seen.len(), 4, "one call per lens, in parallel");
    // Every call carried the repo map — the view the worker never had.
    assert!(
        seen.iter().all(|p| p.contains("REPOSITORY MAP")),
        "a lens lost its grounding"
    );
    // And duplication specifically got the pre-retrieved lookalike.
    assert!(
        seen.iter().any(|p| p.contains("src/utils/date.rs:2")),
        "the duplication lookup did not reach the prompt"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_reviewer_that_finds_nothing_is_the_normal_outcome() {
    let (root, diff) = duplicate_repo();
    // Unscripted: every lens replies `[]`.
    let backend = ScriptedReviewer::new("qwen", vec![]);
    let out = review(
        &[Reviewer::new("qwen", &backend)],
        &diff,
        &root,
        "render a report",
        &[],
        &cfg(),
    );
    assert!(!out.skipped, "it ran");
    assert!(out.findings.is_empty(), "and found nothing — normal");
    assert_eq!(out.blocking(Severity::Low), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_engine_never_writes_to_the_workspace() {
    // Review never rewrites code. The strongest form of the test available:
    // the workspace is byte-identical after a review that found something.
    let (root, diff) = duplicate_repo();
    let before = std::fs::read_to_string(root.join("src/report/render.rs")).unwrap();
    let backend = ScriptedReviewer::new(
        "qwen",
        vec![(
            "QUESTION: Does this change reimplement",
            r#"[{"hunk":"H0","file":"src/report/render.rs","symbol":"format_date",
                 "severity":"high","summary":"duplicate"}]"#,
        )],
    );
    let out = review(
        &[Reviewer::new("qwen", &backend)],
        &diff,
        &root,
        "render a report",
        &[],
        &cfg(),
    );
    assert!(out.findings[0].corroborated, "it did find something");
    assert_eq!(
        std::fs::read_to_string(root.join("src/report/render.rs")).unwrap(),
        before,
        "and changed nothing"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn anchor_resolution_only_judges_findings_that_named_a_symbol() {
    let root = temp_repo("anchor");
    std::fs::write(root.join("a.rs"), "fn real() {}\n").unwrap();
    let no_symbol = Finding::new(
        Lens::Duplication,
        Severity::Low,
        Anchor::file("a.rs"),
        "s",
        ModelId::new("q"),
    );
    assert!(
        anchor_resolves(&root, &no_symbol),
        "claimed nothing to check"
    );

    let real = Finding::new(
        Lens::Duplication,
        Severity::Low,
        Anchor::file("a.rs").with_symbol("real"),
        "s",
        ModelId::new("q"),
    );
    assert!(anchor_resolves(&root, &real));

    let wrong_file = Finding::new(
        Lens::Duplication,
        Severity::Low,
        Anchor::file("b.rs").with_symbol("real"),
        "s",
        ModelId::new("q"),
    );
    assert!(
        !anchor_resolves(&root, &wrong_file),
        "right name, wrong file"
    );
    let _ = std::fs::remove_dir_all(&root);
}
