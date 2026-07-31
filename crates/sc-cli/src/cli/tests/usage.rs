//! The help text must actually document the surface it ships.

use crate::usage;

#[test]
fn usage_lists_comply() {
    assert!(usage().contains("comply"));
    assert!(usage().contains("comply-lint"));
    assert!(usage().contains("comply-eval"));
    assert!(usage().contains("--pack"));
    assert!(usage().contains("--list-packs"));
}

#[test]
fn usage_documents_trace_and_its_gate() {
    let u = usage();
    assert!(u.contains("trace"));
    assert!(u.contains("--check"));
    // The reader must be able to tell what gates and what merely warns —
    // otherwise `unknown` gets read as a failure and the gate gets bypassed.
    assert!(
        u.contains("never gates"),
        "what does not gate must be stated"
    );
}

#[test]
fn usage_warns_against_tunnelling_a_tokenless_run() {
    let u = usage();
    assert!(u.contains("--no-token"));
    assert!(
        u.contains("tailscale serve"),
        "the tunnel warning must be visible"
    );
}

#[test]
fn usage_documents_the_queue_and_its_limits() {
    let u = usage();
    assert!(u.contains("queue"));
    assert!(u.contains("--repo"), "a repo is chosen by name");
    // The one thing a reader must not have to guess: approving does not build.
    assert!(
        u.contains("starts nothing"),
        "approve must say it starts nothing: {u}"
    );
}
