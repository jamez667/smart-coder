//! Flags → backends, agent/swarm config, permission policy, and the plan gate policy.

use crate::{Cli, Command, ToolCallingArg};

#[test]
fn parses_tool_calling_modes_and_maps_to_backend() {
    use sc_model::{ModelBackend, ToolCalling};
    let native = Cli::parse(["--tool-calling", "native"]).unwrap();
    assert_eq!(native.tool_calling, ToolCallingArg::Native);
    assert_eq!(
        native.backend().capabilities().tool_calling,
        ToolCalling::OpenAiStyle
    );

    let gbnf = Cli::parse(["--tool-calling", "gbnf"]).unwrap();
    assert_eq!(
        gbnf.backend().capabilities().tool_calling,
        ToolCalling::Gbnf
    );

    assert!(Cli::parse(["--tool-calling", "bogus"]).is_err());
}

#[test]
fn swarm_orchestrator_defaults_to_worker_endpoint() {
    let cli = Cli::parse(["swarm", "task", "--model", "m", "--base-url", "http://x/v1"]).unwrap();
    assert_eq!(cli.max_workers, 2); // default
                                    // No --orchestrator-* → orchestrator() reuses base_url/model (built ok).
    let _ = cli.orchestrator();
    assert!(cli.orchestrator_model.is_none());
}

#[test]
fn parses_advisor_and_builds_a_second_backend() {
    use sc_model::ModelBackend;
    // As a top-level flag and inside a `run`/`serve` tail.
    let cli = Cli::parse([
        "run",
        "fix it",
        "--model",
        "gemma4:e2b",
        "--advisor",
        "gemma4:e4b",
    ])
    .unwrap();
    assert_eq!(cli.advisor_model.as_deref(), Some("gemma4:e4b"));
    let advisor = cli.advisor().expect("advisor backend");
    assert_eq!(advisor.name(), "openai-compat");
    // No --advisor → no advisor backend.
    assert!(Cli::parse(["run", "x"]).unwrap().advisor().is_none());
}

#[test]
fn plan_is_autonomous_by_default_and_gated_with_interactive() {
    let auto = Cli::parse(["plan", "build a parser"]).unwrap();
    assert_eq!(
        auto.command,
        Command::Plan {
            task: "build a parser".to_string(),
            interactive: false,
        }
    );
    // Both spellings turn on the human checkpoints; the flag is peeled out of
    // the greedily-collected task.
    for flag in ["--interactive", "--gate"] {
        let gated = Cli::parse(["plan", "build a parser", flag]).unwrap();
        assert_eq!(
            gated.command,
            Command::Plan {
                task: "build a parser".to_string(),
                interactive: true,
            }
        );
    }
}

#[test]
fn ceremony_tier_resolves_to_its_gate_set() {
    use sc_workflow::Ceremony;
    let cli = Cli::parse(["plan", "fix a typo", "--ceremony", "standard"]).unwrap();
    assert_eq!(cli.ceremony, Some(Ceremony::Standard));
    assert_eq!(cli.ceremony_gates(), Ceremony::Standard.gates());
    // A bad tier is a loud error.
    assert!(Cli::parse(["plan", "t", "--ceremony", "lavish"]).is_err());
}

#[test]
fn explicit_gates_override_the_tier_and_parse_slugs() {
    use sc_workflow::{Phase, PhaseSet};
    let cli = Cli::parse([
        "plan",
        "do it",
        "--ceremony",
        "minimal",
        "--gates",
        "specs,stage-breakdown",
    ])
    .unwrap();
    // --gates wins over --ceremony.
    assert_eq!(
        cli.ceremony_gates(),
        PhaseSet::of([Phase::Specs, Phase::StageBreakdown])
    );
}

#[test]
fn gates_with_unknown_slug_is_an_error() {
    let err = Cli::parse(["plan", "t", "--gates", "specs,frobnicate"]).unwrap_err();
    assert!(err.to_string().contains("unknown phase"), "got: {err}");
}

#[test]
fn bare_interactive_gates_every_phase() {
    use sc_workflow::Ceremony;
    // No ceremony/gates flag → ceremony_gates() defaults to Full (today's
    // behavior preserved), and the run is gated.
    let cli = Cli::parse(["plan", "t", "--interactive"]).unwrap();
    assert!(cli.ceremony.is_none() && cli.gates.is_none());
    assert_eq!(cli.ceremony_gates(), Ceremony::Full.gates());
    assert!(cli.plan_is_gated(true));
}

#[test]
fn no_ceremony_flags_runs_autonomously() {
    let cli = Cli::parse(["plan", "t"]).unwrap();
    // interactive=false and no policy → not gated.
    assert!(!cli.plan_is_gated(false));
}

#[test]
fn ceremony_and_gates_imply_interactive() {
    // Naming a policy turns the gates on even without --interactive.
    let tier = Cli::parse(["plan", "t", "--ceremony", "minimal"]).unwrap();
    assert!(tier.plan_is_gated(false));
    let explicit = Cli::parse(["plan", "t", "--gates", "specs"]).unwrap();
    assert!(explicit.plan_is_gated(false));
}

#[test]
fn safety_flags_populate_the_permission_policy_and_dry_run() {
    let cli = Cli::parse(["run", "x", "--yolo", "--allow", "cargo test", "--dry-run"]).unwrap();
    let cfg = cli.agent_config();
    assert!(cfg.permission.allow_shell, "--yolo → allow_shell");
    assert_eq!(
        cfg.permission.shell_allowlist,
        vec!["cargo test".to_string()]
    );
    assert!(cfg.dry_run, "--dry-run → dry_run");

    // Defaults: no flags → conservative policy, no dry-run, no verbose.
    let plain = Cli::parse(["run", "x"]).unwrap().agent_config();
    assert!(!plain.permission.allow_shell);
    assert!(plain.permission.shell_allowlist.is_empty());
    assert!(!plain.dry_run);
    assert!(!plain.verbose);
}
