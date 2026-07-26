//! The doctor report and the preflight probes.

use sc_model::ModelBackend;
use sc_proto::DcError;

use crate::{doctor_report, preflight, Cli, DEFAULT_MODEL};

#[test]
fn preflight_names_the_unreachable_backend() {
    use sc_model::MockBackend;
    // An exhausted mock errors on generate → stands in for a down server.
    let down = MockBackend::new(Vec::<String>::new());
    let err = preflight(&[("orchestrator", &down)]).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("orchestrator"), "{msg}");
    assert!(msg.contains("isn't responding"), "{msg}");
}

#[test]
fn preflight_passes_when_reachable() {
    use sc_model::MockBackend;
    // Two pings for the single distinct backend (probe consumes one).
    let a = MockBackend::new(["pong", "pong"]);
    assert!(preflight(&[("orchestrator", &a)]).is_ok());
}

#[test]
fn preflight_probes_a_shared_endpoint_once() {
    use sc_model::MockBackend;
    // Orchestrator and advisor are the SAME model (e.g. advisor-e4b on one
    // server): one scripted ping is enough because the duplicate is skipped.
    let shared = MockBackend::new(["pong"]);
    assert!(preflight(&[("orchestrator", &shared), ("advisor", &shared)]).is_ok());
}

#[test]
fn doctor_report_shows_reachable_status_and_budget() {
    let cli = Cli::parse(["doctor"]).unwrap();
    let caps = cli.backend().capabilities();
    let report = doctor_report(&cli, &caps, &Ok(()));
    assert!(report.contains("reachable ✓"), "got: {report}");
    assert!(report.contains("8192 tokens"), "got: {report}");
    assert!(report.contains(DEFAULT_MODEL), "got: {report}");
}

#[test]
fn doctor_report_surfaces_an_unreachable_backend() {
    let cli = Cli::parse(["doctor"]).unwrap();
    let caps = cli.backend().capabilities();
    let report = doctor_report(
        &cli,
        &caps,
        &Err(DcError::Backend("connection refused".to_string())),
    );
    assert!(report.contains("UNREACHABLE ✗"), "got: {report}");
    assert!(report.contains("connection refused"), "got: {report}");
}
