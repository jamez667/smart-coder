//! M3 exit-criterion test (spec 07 / spec 11): given a failing unit test, the
//! agent drives it red→green on a sample repo **without breaking the suite or
//! weakening the test**.
//!
//! We script the model (no live LLM needed) to exercise the full M3 path:
//! verify (red) → anchored edit → verify (green) → finish, gated on the
//! whole-suite verification. Separately we prove the frozen contract test can't
//! be cheated and that `finish` is refused while the suite is red.

use std::cell::RefCell;
use std::path::Path;

use dc_core::{run_agent_with, AgentConfig, ParseRepair};
use dc_model::{Capabilities, GenerateRequest, GenerateResponse, ModelBackend, ToolCalling};
use dc_proto::Result;
use dc_tools::{default_registry, PermissionPolicy};

/// A backend that replays a fixed script of tool-call JSON, one per turn.
struct Scripted(RefCell<Vec<String>>);
impl Scripted {
    fn new(turns: Vec<&str>) -> Self {
        Scripted(RefCell::new(turns.into_iter().map(String::from).collect()))
    }
}
impl ModelBackend for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            max_context_tokens: 8_192,
            tool_calling: ToolCalling::None,
            on_device: false,
        }
    }
    fn generate(&self, _r: &GenerateRequest) -> Result<GenerateResponse> {
        let mut s = self.0.borrow_mut();
        let content = if s.is_empty() {
            r#"{"tool":"finish"}"#.to_string()
        } else {
            s.remove(0)
        };
        Ok(GenerateResponse { content })
    }
}

fn temp_repo(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "dc-core-tdd-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A red sample repo: `impl.sh` is a wrong stub; `test.sh` is the contract test
/// that fails until the impl is fixed.
fn red_repo() -> std::path::PathBuf {
    let ws = temp_repo("repo");
    std::fs::write(ws.join("impl.sh"), "is_even() { return 1; }\n").unwrap();
    std::fs::write(
        ws.join("test.sh"),
        ". ./impl.sh\nis_even 4 || exit 1\nif is_even 3; then exit 1; fi\nexit 0\n",
    )
    .unwrap();
    ws
}

fn config_with_verify() -> AgentConfig {
    AgentConfig {
        verify_command: Some("sh test.sh".to_string()),
        permission: PermissionPolicy::with_frozen(vec!["test.sh".to_string()]),
        ..Default::default()
    }
}

fn run(backend: &dyn ModelBackend, ws: &Path, cfg: &AgentConfig) -> dc_core::AgentReport {
    let registry = default_registry();
    run_agent_with(
        backend,
        &registry,
        &ParseRepair,
        "Make is_even report even numbers correctly.",
        ws,
        cfg,
    )
    .unwrap()
}

#[test]
fn agent_drives_a_failing_test_red_to_green() {
    let ws = red_repo();
    // verify (red) -> fix impl with an anchored edit -> verify (green) -> finish.
    let backend = Scripted::new(vec![
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"edit_file","path":"impl.sh","old_str":"return 1;","new_str":"[ $(( $1 % 2 )) -eq 0 ];"}"#,
        r#"{"tool":"run_verification"}"#,
        r#"{"tool":"finish"}"#,
    ]);

    let report = run(&backend, &ws, &config_with_verify());

    assert!(report.finished, "should finish");
    assert_eq!(report.verified, Some(true), "whole suite must be green");
    // The contract test is byte-for-byte unchanged (not weakened).
    let test_after = std::fs::read_to_string(ws.join("test.sh")).unwrap();
    assert!(
        test_after.contains("is_even 4 || exit 1"),
        "test was altered"
    );
    // The journal recorded the impl edit.
    assert!(
        report.change_summary.contains("impl.sh"),
        "{}",
        report.change_summary
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn auto_finishes_when_verification_passes_without_a_finish_call() {
    // A small model often fixes the code and runs the tests green, then keeps
    // going (or stalls) instead of calling `finish`. The harness should honor the
    // win: a green run_verification ends the run as Finished on its own.
    let ws = red_repo();
    let backend = Scripted::new(vec![
        r#"{"tool":"edit_file","path":"impl.sh","old_str":"return 1;","new_str":"[ $(( $1 % 2 )) -eq 0 ];"}"#,
        r#"{"tool":"run_verification"}"#,
        // No finish — and the script would otherwise loop on read_file forever.
        r#"{"tool":"read_file","path":"impl.sh"}"#,
    ]);

    let report = run(&backend, &ws, &config_with_verify());

    assert!(report.finished, "green verification should auto-finish");
    assert_eq!(report.verified, Some(true));
    // It stopped right at the green verification, not at the step budget.
    assert!(
        report.steps <= 2,
        "should finish promptly, took {}",
        report.steps
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn finish_is_refused_while_the_suite_is_red() {
    let ws = red_repo();
    // The model tries to finish without fixing anything. The gate refuses, so the
    // run hits the step budget rather than a bogus green finish.
    let backend = Scripted::new(vec![r#"{"tool":"finish"}"#]);
    let cfg = AgentConfig {
        max_steps: 3,
        ..config_with_verify()
    };
    let report = run(&backend, &ws, &cfg);
    assert!(!report.finished, "must not finish on a red suite");
    assert_eq!(report.verified, Some(false));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn cheating_by_editing_the_frozen_test_is_denied() {
    let ws = red_repo();
    // The model tries to neuter the contract test, then finish. The permission
    // gate denies the edit; the test file is untouched; finish stays refused.
    let backend = Scripted::new(vec![
        r#"{"tool":"edit_file","path":"test.sh","old_str":"is_even 4 || exit 1","new_str":"true"}"#,
        r#"{"tool":"finish"}"#,
    ]);
    let cfg = AgentConfig {
        max_steps: 4,
        ..config_with_verify()
    };
    let report = run(&backend, &ws, &cfg);

    assert!(!report.finished, "cheating must not yield a finish");
    // The contract test is intact.
    let test_after = std::fs::read_to_string(ws.join("test.sh")).unwrap();
    assert!(
        test_after.contains("is_even 4 || exit 1"),
        "frozen test was edited!"
    );
    let _ = std::fs::remove_dir_all(&ws);
}
