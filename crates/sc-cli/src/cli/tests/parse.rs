//! Argv parsing: subcommands, flag positions, the run-tail peel, and loud errors.

use crate::{Cli, Command, QueueAction, ToolCallingArg, DEFAULT_BASE_URL, DEFAULT_MODEL};

#[test]
fn defaults_to_chat_with_default_backend() {
    let cli = Cli::parse(Vec::<String>::new()).unwrap();
    assert_eq!(cli.command, Command::Chat);
    assert_eq!(cli.base_url, DEFAULT_BASE_URL);
    assert_eq!(cli.model, DEFAULT_MODEL);
    assert_eq!(cli.tool_calling, ToolCallingArg::None);
}

#[test]
fn parses_run_with_task_verify_and_plan() {
    let cli = Cli::parse([
        "run",
        "make",
        "the",
        "test",
        "pass",
        "--verify",
        "sh test.sh",
        "--plan",
    ])
    .unwrap();
    match &cli.command {
        Command::Run { task } => assert_eq!(task, "make the test pass"),
        other => panic!("expected Run, got {other:?}"),
    }
    assert_eq!(cli.verify_command.as_deref(), Some("sh test.sh"));
    assert!(cli.plan_first);
    // The config reflects the flags.
    let cfg = cli.agent_config();
    assert_eq!(cfg.verify_command.as_deref(), Some("sh test.sh"));
    assert!(cfg.plan_first);
}

#[test]
fn run_requires_a_task() {
    assert!(Cli::parse(["run"]).is_err());
}

#[test]
fn parses_gemini_planner_flags() {
    // The Gemini-as-planner invocation: local coder, orchestrator pointed at Gemini + a key.
    let cli = Cli::parse([
        "run",
        "build a todo app",
        "--orchestrator",
        "gemini-2.5-flash-lite",
        "--orchestrator-url",
        "https://generativelanguage.googleapis.com/v1beta/openai",
        "--orchestrator-key",
        "AIzaSECRET",
    ])
    .unwrap();
    assert_eq!(
        cli.orchestrator_model.as_deref(),
        Some("gemini-2.5-flash-lite")
    );
    assert_eq!(
        cli.orchestrator_url.as_deref(),
        Some("https://generativelanguage.googleapis.com/v1beta/openai")
    );
    assert_eq!(cli.orchestrator_key.as_deref(), Some("AIzaSECRET"));
    // The coder was NOT given a key on the command line, so it stays local (no accidental
    // key bleed) — unless the test environment happens to export GEMINI_API_KEY, which is a
    // legitimate fallback the parser honors.
    if std::env::var("GEMINI_API_KEY").is_err() {
        assert_eq!(cli.api_key, None);
    }
}

#[test]
fn coder_key_falls_through_to_the_planner_when_no_orchestrator_key() {
    // A single --key set on the coder also authenticates a same-provider planner.
    let cli = Cli::parse(["run", "task", "--key", "shared-key"]).unwrap();
    assert_eq!(cli.api_key.as_deref(), Some("shared-key"));
    assert_eq!(cli.orchestrator_key.as_deref(), Some("shared-key"));
}

#[test]
fn parses_swarm_with_orchestrator_and_workers() {
    let cli = Cli::parse([
        "swarm",
        "add validation",
        "--base-url",
        "http://localhost:11435/v1",
        "--model",
        "coder-0",
        "--orchestrator-url",
        "http://localhost:11434/v1",
        "--orchestrator",
        "advisor-e4b",
        "--max-workers",
        "3",
        "--max-retries",
        "4",
        "--frozen",
        "tests/test_a.py, tests\\test_b.py",
        "--verify",
        "pytest -q",
    ])
    .unwrap();
    match &cli.command {
        // The flags after the task must be peeled off, not swept into the goal
        // (regression: `--max-retries` once leaked into the task string).
        Command::Swarm { task } => assert_eq!(task, "add validation"),
        other => panic!("expected Swarm, got {other:?}"),
    }
    assert_eq!(cli.model, "coder-0"); // workers
    assert_eq!(cli.orchestrator_model.as_deref(), Some("advisor-e4b"));
    assert_eq!(
        cli.orchestrator_url.as_deref(),
        Some("http://localhost:11434/v1")
    );
    assert_eq!(cli.max_workers, 3);
    assert_eq!(cli.max_subtask_retries, 4);
    // `--frozen` parses + normalizes separators, and survives the task-peel.
    assert_eq!(cli.frozen_paths, vec!["tests/test_a.py", "tests/test_b.py"]);
    // The swarm config carries the verify command (gates integration) + workers.
    let sc = cli.swarm_config();
    assert_eq!(sc.max_workers, 3);
    assert_eq!(sc.max_subtask_retries, 4);
    assert_eq!(sc.frozen_paths, vec!["tests/test_a.py", "tests/test_b.py"]);
    assert_eq!(sc.verify_command.as_deref(), Some("pytest -q"));
    // Review is off unless asked for: it costs model calls per integrated diff.
    assert!(!cli.review);
    assert!(!sc.review.enabled);
}

#[test]
fn review_flags_survive_the_task_peel_and_reach_the_swarm_config() {
    // `--review` lands AFTER the task, so it goes through the second-pass parser.
    // Without handling there it would be an unknown token and the run would die.
    let cli = Cli::parse([
        "swarm",
        "add validation",
        "--review-action",
        "retry",
        "--review-gate",
        "medium",
    ])
    .unwrap();
    match &cli.command {
        Command::Swarm { task } => assert_eq!(task, "add validation"),
        other => panic!("expected Swarm, got {other:?}"),
    }
    // Naming what to do with findings implies wanting them.
    assert!(cli.review);
    assert_eq!(cli.review_action, sc_swarm::ReviewAction::Retry);
    assert_eq!(cli.review_gate, sc_swarm::Severity::Medium);

    let sc = cli.swarm_config();
    assert!(sc.review.enabled);
    assert_eq!(sc.review.action, sc_swarm::ReviewAction::Retry);
    assert_eq!(sc.review.gate_at, sc_swarm::Severity::Medium);
    // All four lenses by default, and small diffs skipped.
    assert_eq!(sc.review.lenses.len(), 4);
    assert!(sc.review.min_changed_lines > 0);
}

#[test]
fn bare_review_enables_it_at_the_reporting_default() {
    // The honest default: findings ride along and the run still succeeds.
    let cli = Cli::parse(["swarm", "add validation", "--review"]).unwrap();
    assert!(cli.review);
    assert_eq!(cli.review_action, sc_swarm::ReviewAction::Report);
    assert_eq!(cli.review_gate, sc_swarm::Severity::High);
}

#[test]
fn an_unknown_review_action_is_an_error_not_a_silent_downgrade() {
    // A user asking to gate and quietly getting report-only would believe a gate
    // was in place that never was.
    assert!(Cli::parse(["swarm", "t", "--review-action", "fix"]).is_err());
    assert!(Cli::parse(["swarm", "t", "--review-gate", "urgent"]).is_err());
}

#[test]
fn trace_parses_with_and_without_the_check_gate() {
    assert_eq!(
        Cli::parse(["trace"]).unwrap().command,
        Command::Trace { check: false },
        "bare `trace` reports without gating"
    );
    assert_eq!(
        Cli::parse(["trace", "--check"]).unwrap().command,
        Command::Trace { check: true }
    );
    // Order must not matter. A gate flag that silently does nothing because it
    // came first is a gate that is not running.
    assert_eq!(
        Cli::parse(["--check", "trace"]).unwrap().command,
        Command::Trace { check: true }
    );
    // `--json` needs no special parsing; it selects the machine-readable report.
    let cli = Cli::parse(["trace", "--json", "--check"]).unwrap();
    assert_eq!(cli.command, Command::Trace { check: true });
    assert!(cli.json);
}

#[test]
fn check_outside_trace_is_an_error_not_a_silent_no_op() {
    // A user who passes `--check` to the wrong command would otherwise believe a
    // gate was in place that never ran (spec 00 — fail loud).
    assert!(Cli::parse(["doctor", "--check"]).is_err());
    assert!(Cli::parse(["--check"]).is_err());
}

#[test]
fn staged_subcommand_parses_task_and_verify() {
    // `staged` is the headless entry; the task must peel cleanly and
    // `--verify` (the per-stage gate override) must survive the peel.
    let cli = Cli::parse(["staged", "wire the invite list", "--verify", "cargo check"]).unwrap();
    match &cli.command {
        Command::Staged { task } => assert_eq!(task, "wire the invite list"),
        other => panic!("expected Staged, got {other:?}"),
    }
    assert_eq!(cli.verify_command.as_deref(), Some("cargo check"));
}

#[test]
fn parses_swarm_cli_and_json_flags() {
    // `--cli` in the swarm tail switches to the line renderer.
    let cli = Cli::parse(["swarm", "add a test", "--cli"]).unwrap();
    assert!(cli.cli, "--cli should set the line-render flag");
    assert!(!cli.json);

    // As a top-level flag too (flags may appear in any order, spec 00).
    let cli = Cli::parse(["--cli", "swarm", "add a test"]).unwrap();
    assert!(cli.cli);

    // `--json` is parsed independently; the `--json ⇒ cli` implication is
    // applied at the call site, not here, so a bare --json leaves cli false.
    let cli = Cli::parse(["swarm", "add a test", "--json"]).unwrap();
    assert!(cli.json);
    assert!(!cli.cli);
}

#[test]
fn parses_doctor_with_overrides_in_any_order() {
    let cli = Cli::parse([
        "--model",
        "qwen2:1.5b",
        "doctor",
        "--base-url",
        "http://host:8000/v1",
    ])
    .unwrap();
    assert_eq!(cli.command, Command::Doctor);
    assert_eq!(cli.model, "qwen2:1.5b");
    assert_eq!(cli.base_url, "http://host:8000/v1");
}

#[test]
fn parses_json_log_yolo_allow_dry_run_top_level_and_in_run_tail() {
    // Top-level (before the subcommand).
    let top = Cli::parse([
        "--json",
        "--dry-run",
        "--yolo",
        "--allow",
        "cargo test",
        "--log",
        "out.jsonl",
        "run",
        "do it",
    ])
    .unwrap();
    assert!(top.json && top.dry_run && top.yolo);
    assert_eq!(top.allow, vec!["cargo test".to_string()]);
    assert_eq!(top.log.as_deref(), Some("out.jsonl"));

    // In the run tail (after the task) — and --allow repeats.
    let tail = Cli::parse([
        "run",
        "do it",
        "--json",
        "--dry-run",
        "--yolo",
        "--allow",
        "git status",
        "--allow",
        "ls",
        "--log",
        "x.jsonl",
    ])
    .unwrap();
    assert!(tail.json && tail.dry_run && tail.yolo);
    assert_eq!(tail.allow, vec!["git status".to_string(), "ls".to_string()]);
    assert_eq!(tail.log.as_deref(), Some("x.jsonl"));
    match &tail.command {
        Command::Run { task } => assert_eq!(task, "do it"),
        other => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn verbose_flag_parses_both_spellings_and_positions_and_wires_config() {
    for flag in ["--verbose", "-v"] {
        // Top-level and in the run tail.
        let top = Cli::parse([flag, "run", "x"]).unwrap();
        assert!(top.verbose, "top-level {flag}");
        let tail = Cli::parse(["run", "x", flag]).unwrap();
        assert!(tail.verbose, "run-tail {flag}");
        assert!(tail.agent_config().verbose, "{flag} → AgentConfig.verbose");
    }
    assert!(!Cli::parse(["run", "x"]).unwrap().verbose);
}

#[test]
fn parses_replay_and_requires_an_id() {
    let cli = Cli::parse(["replay", "1718000000000"]).unwrap();
    assert_eq!(
        cli.command,
        Command::Replay {
            session: "1718000000000".to_string()
        }
    );
    assert!(Cli::parse(["replay"]).is_err());
}

#[test]
fn help_is_recognized() {
    assert_eq!(Cli::parse(["help"]).unwrap().command, Command::Help);
    assert_eq!(Cli::parse(["--help"]).unwrap().command, Command::Help);
    assert_eq!(Cli::parse(["-h"]).unwrap().command, Command::Help);
}

#[test]
fn unknown_argument_is_an_error_not_silently_ignored() {
    let err = Cli::parse(["--frobnicate"]).unwrap_err();
    assert!(err.to_string().contains("unknown argument"), "got: {err}");
}

#[test]
fn flag_without_value_errors() {
    assert!(Cli::parse(["--model"]).is_err());
    assert!(Cli::parse(["--base-url"]).is_err());
}

#[test]
fn parses_comply_with_and_without_a_pack() {
    let bare = Cli::parse(["comply"]).unwrap();
    assert_eq!(bare.command, Command::Comply { pack: None });

    let with_pack = Cli::parse(["comply", "--pack", "packs/iso27001.toml"]).unwrap();
    assert_eq!(
        with_pack.command,
        Command::Comply {
            pack: Some("packs/iso27001.toml".to_string())
        }
    );
}

#[test]
fn comply_honours_the_port_flag() {
    let cli = Cli::parse(["comply", "--port", "9001"]).unwrap();
    assert_eq!(cli.command, Command::Comply { pack: None });
    assert_eq!(cli.port, 9001);
}

#[test]
fn pack_without_a_value_errors() {
    assert!(Cli::parse(["comply", "--pack"]).is_err());
}

#[test]
fn pack_outside_comply_is_rejected() {
    // Silently ignoring it would let `smart-coder run --pack x` look like it
    // did something.
    let err = Cli::parse(["doctor", "--pack", "x.toml"]).unwrap_err();
    assert!(format!("{err}").contains("only applies to"), "{err}");
}

#[test]
fn parses_comply_lint_with_and_without_a_pack() {
    let bare = Cli::parse(["comply-lint"]).unwrap();
    assert_eq!(bare.command, Command::ComplyLint { pack: None });

    let with_pack = Cli::parse(["comply-lint", "--pack", "packs/iso.toml"]).unwrap();
    assert_eq!(
        with_pack.command,
        Command::ComplyLint {
            pack: Some("packs/iso.toml".to_string())
        }
    );
}

#[test]
fn comply_and_comply_lint_are_distinct_subcommands() {
    // `comply` audits a codebase; `comply-lint` critiques the pack itself.
    // Conflating them would run an audit when a critique was asked for.
    assert_ne!(
        Cli::parse(["comply"]).unwrap().command,
        Cli::parse(["comply-lint"]).unwrap().command
    );
}

#[test]
fn no_token_is_off_by_default_and_opt_in() {
    // The security-relevant default. Auth must never disable itself.
    assert!(!Cli::parse(["comply"]).unwrap().no_token);
    assert!(Cli::parse(["comply", "--no-token"]).unwrap().no_token);
}

#[test]
fn parses_list_packs() {
    assert_eq!(
        Cli::parse(["--list-packs"]).unwrap().command,
        Command::ListPacks
    );
}

#[test]
fn pack_accepts_a_shipped_name_not_just_a_path() {
    // The interface that makes ten packs usable: `--pack iso27001` rather
    // than a path into the crate's internals.
    let cli = Cli::parse(["comply", "--pack", "iso27001"]).unwrap();
    assert_eq!(
        cli.command,
        Command::Comply {
            pack: Some("iso27001".to_string())
        }
    );
}

#[test]
fn comply_eval_collects_repeated_author_models() {
    // Repeatable is the whole point: one run, two models, side by side.
    let cli = Cli::parse([
        "comply-eval",
        "--author-model",
        "gemini-pro-latest@https://example.invalid/v1",
        "--author-model",
        "qwen3-coder-30b@http://localhost:11435/v1",
    ])
    .unwrap();
    match cli.command {
        Command::ComplyEval { models } => {
            assert_eq!(models.len(), 2);
            assert!(models[0].starts_with("gemini-pro-latest@"));
            assert!(models[1].starts_with("qwen3-coder-30b@"));
        }
        other => panic!("wrong command: {other:?}"),
    }
}

#[test]
fn comply_eval_accepts_a_bare_model_name() {
    let cli = Cli::parse(["comply-eval", "--author-model", "local-model"]).unwrap();
    assert_eq!(
        cli.command,
        Command::ComplyEval {
            models: vec!["local-model".to_string()]
        }
    );
}

#[test]
fn author_model_without_a_value_errors() {
    assert!(Cli::parse(["comply-eval", "--author-model"]).is_err());
}

#[test]
fn author_model_outside_comply_eval_is_rejected() {
    let err = Cli::parse(["doctor", "--author-model", "x"]).unwrap_err();
    assert!(
        format!("{err}").contains("only applies to `comply-eval`"),
        "{err}"
    );
}

#[test]
fn queue_file_needs_a_repo_chosen_by_name() {
    // A repository is picked from the daemon's configured set, never typed as a
    // path — that is what makes traversal unreachable rather than mitigated
    // (spec 18). So `--repo` is required and carries a NAME.
    let cli = Cli::parse(["queue", "file", "add seat types", "--repo", "city"]).unwrap();
    match &cli.command {
        Command::Queue {
            action: QueueAction::File { text, repo, kind },
        } => {
            assert_eq!(text, "add seat types");
            assert_eq!(repo, "city");
            // Feature is the least surprising default to get back.
            assert_eq!(*kind, sc_daemon::IntakeKind::Feature);
        }
        other => panic!("expected a File action, got {other:?}"),
    }

    // Without --repo the daemon would have to guess which repository, so refuse.
    assert!(Cli::parse(["queue", "file", "add seat types"]).is_err());
    // And a request with no text is not a request.
    assert!(Cli::parse(["queue", "file", "--repo", "city"]).is_err());
}

#[test]
fn queue_actions_parse() {
    let cases: Vec<(Vec<&str>, QueueAction)> = vec![
        (vec!["queue", "list"], QueueAction::List),
        (vec!["queue", "run"], QueueAction::Run),
        (vec!["queue", "repos"], QueueAction::Repos),
        (
            vec!["queue", "approve", "t-1"],
            QueueAction::Approve { id: "t-1".into() },
        ),
        (
            vec!["queue", "discard", "t-1"],
            QueueAction::Discard { id: "t-1".into() },
        ),
        (
            vec!["queue", "show", "t-1"],
            QueueAction::Show { id: "t-1".into() },
        ),
        (
            vec!["queue", "add-repo", "city", "../city"],
            QueueAction::AddRepo {
                name: "city".into(),
                path: "../city".into(),
            },
        ),
        (
            vec!["queue", "forget-repo", "city"],
            QueueAction::ForgetRepo {
                name: "city".into(),
            },
        ),
    ];
    for (argv, expected) in cases {
        let cli = Cli::parse(argv.clone()).unwrap();
        assert_eq!(cli.command, Command::Queue { action: expected }, "{argv:?}");
    }
}

#[test]
fn a_send_back_carries_its_note_as_free_text() {
    let cli = Cli::parse([
        "queue",
        "send-back",
        "t-1",
        "name",
        "the",
        "actual",
        "roles",
    ])
    .unwrap();
    match &cli.command {
        Command::Queue {
            action: QueueAction::SendBack { id, notes },
        } => {
            assert_eq!(id, "t-1");
            assert_eq!(notes, "name the actual roles");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_send_back_without_a_note_is_refused() {
    // Without one the redraft has nothing to go on and will likely produce the
    // same spec, which reads to the developer as the tool ignoring them.
    assert!(Cli::parse(["queue", "send-back", "t-1"]).is_err());
}

#[test]
fn an_action_missing_its_id_says_so_rather_than_guessing() {
    for argv in [
        vec!["queue", "approve"],
        vec!["queue", "discard"],
        vec!["queue", "show"],
    ] {
        let err = Cli::parse(argv.clone()).expect_err("{argv:?} should fail");
        assert!(err.to_string().contains("task id"), "{argv:?}: {err}");
    }
}

#[test]
fn queue_with_no_action_and_an_unknown_action_both_fail_loudly() {
    // Spec 00 — fail loud. A silently-ignored action would look like it worked.
    let bare = Cli::parse(["queue"]).expect_err("no action");
    assert!(bare.to_string().contains("needs an action"), "{bare}");

    let unknown = Cli::parse(["queue", "build"]).expect_err("no such action");
    assert!(
        unknown.to_string().contains("unknown queue action"),
        "{unknown}"
    );
    // …and specifically, there is no build action on this surface.
    assert!(!unknown.to_string().contains("build,"), "{unknown}");
}

#[test]
fn an_intake_kind_can_be_chosen_and_an_unknown_one_is_refused() {
    // A bug and a feature are not the same request wearing different labels —
    // the kind shapes the drafting prompt, so getting it wrong returns the wrong
    // shape of document.
    for (word, expected) in [
        ("bug", sc_daemon::IntakeKind::Bug),
        ("feature", sc_daemon::IntakeKind::Feature),
        ("improvement", sc_daemon::IntakeKind::Improvement),
        ("feedback", sc_daemon::IntakeKind::Feedback),
    ] {
        let cli = Cli::parse([
            "queue",
            "file",
            "something",
            "--repo",
            "city",
            "--kind",
            word,
        ])
        .unwrap();
        match &cli.command {
            Command::Queue {
                action: QueueAction::File { kind, .. },
            } => assert_eq!(*kind, expected, "{word}"),
            other => panic!("{other:?}"),
        }
    }

    // Silently defaulting an unrecognised kind would return a feature-shaped
    // spec for a crash (spec 00 — fail loud).
    let err = Cli::parse(["queue", "file", "x", "--repo", "city", "--kind", "urgent"])
        .expect_err("unknown kind");
    assert!(err.to_string().contains("unknown kind"), "{err}");
}

#[test]
fn the_feedback_actions_parse() {
    assert_eq!(
        Cli::parse(["queue", "feedback"]).unwrap().command,
        Command::Queue {
            action: QueueAction::Feedback {
                repo: None,
                all: false
            }
        }
    );
    assert_eq!(
        Cli::parse(["queue", "feedback", "--repo", "city", "--all"])
            .unwrap()
            .command,
        Command::Queue {
            action: QueueAction::Feedback {
                repo: Some("city".into()),
                all: true
            }
        }
    );
    assert_eq!(
        Cli::parse(["queue", "ack", "f-1", "--repo", "city"])
            .unwrap()
            .command,
        Command::Queue {
            action: QueueAction::AckFeedback {
                repo: "city".into(),
                id: "f-1".into()
            }
        }
    );
    // Feedback is stored per repository, so acknowledging needs to know which.
    assert!(Cli::parse(["queue", "ack", "f-1"]).is_err());
}
