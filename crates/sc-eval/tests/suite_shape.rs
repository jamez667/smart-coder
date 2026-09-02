//! Structural validation of the on-disk eval suites — no model, no execution.
//!
//! **A task can be added broken in ways nothing would notice.** The ladder is the
//! project's measurement instrument, and until now no test loaded it at all: a
//! nonexistent fixture path, a duplicate id, two rungs compiling to the same `.exe`
//! name, or a `contract_tests` entry that does not exist inside its fixture would
//! all ship silently. The last one is the worst — `hash_file` returns `None` for a
//! missing path, so the before/after snapshots match, the freeze check passes, and
//! the task is uncheatable in name only.
//!
//! This runs in milliseconds because it never executes a task. The companion
//! end-to-end check (`suite_smoke.rs`) proves the tasks actually work; this proves
//! they are *well-formed*, which is the part that regresses silently.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sc_eval::{EvalTask, TaskSuite};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/crates/sc-eval
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// Every suite the repo ships. A new suite added here is covered from day one.
fn suites() -> Vec<(&'static str, PathBuf)> {
    let root = repo_root().join("evals");
    vec![
        ("demo", root.join("suite.toml")),
        ("ladder", root.join("ladder").join("suite.toml")),
    ]
}

fn load(path: &Path) -> Vec<EvalTask> {
    TaskSuite::load(path)
        .unwrap_or_else(|e| panic!("{} failed to load: {e}", path.display()))
        .tasks
}

#[test]
fn every_suite_loads_and_is_not_empty() {
    for (name, path) in suites() {
        let tasks = load(&path);
        assert!(!tasks.is_empty(), "{name} suite has no tasks");
    }
}

/// A fixture or solution path that does not exist fails at RUN time, deep inside a
/// harness error, rather than here where the message can name the task.
#[test]
fn every_fixture_and_solution_directory_exists() {
    for (name, path) in suites() {
        for t in load(&path) {
            assert!(
                t.fixture.is_dir(),
                "{name}/{}: fixture {} is not a directory",
                t.id,
                t.fixture.display()
            );
            if let Some(sol) = &t.solution {
                assert!(
                    sol.is_dir(),
                    "{name}/{}: solution {} is not a directory",
                    t.id,
                    sol.display()
                );
            }
        }
    }
}

/// **The freeze check is vacuous when the path is wrong.**
///
/// `snapshot_contracts` hashes each contract test before and after the solve and
/// compares. `hash_file` returns `None` for a file that does not exist — so a typo'd
/// or repo-relative (rather than fixture-relative) path yields `None == None`, the
/// comparison passes, and the task advertises a frozen test it is not actually
/// protecting.
#[test]
fn every_contract_test_exists_inside_its_fixture() {
    for (name, path) in suites() {
        for t in load(&path) {
            for rel in &t.contract_tests {
                let full = t.fixture.join(rel);
                assert!(
                    full.is_file(),
                    "{name}/{}: contract test {rel:?} does not exist at {} — the freeze \
                     check would silently pass",
                    t.id,
                    full.display()
                );
            }
        }
    }
}

/// A solution that ships the contract test would overwrite it, which `run_task`
/// scores as tampering — a confusing way to learn the fixture is malformed.
#[test]
fn no_solution_ships_a_contract_test() {
    for (name, path) in suites() {
        for t in load(&path) {
            let Some(sol) = &t.solution else { continue };
            for rel in &t.contract_tests {
                assert!(
                    !sol.join(rel).exists(),
                    "{name}/{}: solution ships the frozen contract test {rel:?}; \
                     applying it would score as TAMPER",
                    t.id
                );
            }
        }
    }
}

/// Ids name the task in every report and log directory; duplicates make results
/// ambiguous and `--only` non-deterministic.
#[test]
fn task_ids_are_unique_within_a_suite() {
    for (name, path) in suites() {
        let tasks = load(&path);
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for t in &tasks {
            assert!(
                seen.insert(t.id.as_str()),
                "{name}: duplicate task id {:?}",
                t.id
            );
        }
    }
}

/// **Two rungs compiling to the same binary name race each other.**
///
/// Tasks run in separate temp workspaces today, so this is a latent rather than
/// live bug — but the `-o <name>` in a `verify_cmd` is easy to copy-paste when
/// adding a rung, and the failure it would produce (one task mysteriously running
/// another's tests) is exactly the kind that reads as a model problem.
#[test]
fn verify_command_output_binaries_are_unique() {
    for (name, path) in suites() {
        let tasks = load(&path);
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for t in &tasks {
            let Some(out) = output_binary(&t.verify_cmd) else {
                continue;
            };
            assert!(
                seen.insert(out.clone()),
                "{name}/{}: verify_cmd writes {out:?}, which another task also writes",
                t.id
            );
        }
    }
}

/// The `-o <name>` argument of a verify command, if it has one.
fn output_binary(cmd: &str) -> Option<String> {
    let mut parts = cmd.split_whitespace();
    while let Some(p) = parts.next() {
        if p == "-o" {
            return parts.next().map(str::to_string);
        }
    }
    None
}

/// Every task must state how it is verified; an empty command would score red
/// forever with no explanation.
#[test]
fn every_task_has_a_verify_command() {
    for (name, path) in suites() {
        for t in load(&path) {
            assert!(
                !t.verify_cmd.trim().is_empty(),
                "{name}/{}: empty verify_cmd",
                t.id
            );
            assert!(
                !t.description.trim().is_empty(),
                "{name}/{}: empty description — the description IS the task",
                t.id
            );
        }
    }
}

/// The ladder grades by rung, so a rung tag is what makes a result interpretable:
/// without it a task contributes to the total and to no breakdown.
#[test]
fn every_ladder_task_carries_a_rung_and_language_tag() {
    let path = repo_root().join("evals").join("ladder").join("suite.toml");
    for t in load(&path) {
        assert!(
            t.tags.iter().any(|g| g.starts_with("rung:")),
            "ladder/{}: no rung: tag",
            t.id
        );
        assert!(
            t.tags.iter().any(|g| g.starts_with("lang:")),
            "ladder/{}: no lang: tag",
            t.id
        );
    }
}
