//! Exit-code evidence from a shell command. **Disabled by default.**
//!
//! A pack is data an auditor may have downloaded from a vendor. Letting that
//! data execute arbitrary shell commands against a checked-out repository makes
//! the pack format an attack vector — download `soc2.toml`, run it, get owned.
//! So `ComplyOptions::allow_commands` defaults to `false`, and this collector
//! reports `Unknown` with a stated reason rather than silently skipping. A
//! silent skip would read as a clean result, which is exactly the lie the
//! status lattice exists to prevent.
//!
//! Command construction returns a `std::process::Command` without spawning the
//! pack's command, following `sc-verify`'s discipline, so the argument shape is
//! testable without running the check. (On Windows it does probe once for a POSIX
//! shell — see `build_command`.)

use std::path::Path;
use std::process::Command;

use sc_proto::{DcError, Result};

use crate::collector::{AuditContext, Collector, Observation};
use crate::evidence::Evidence;
use crate::pack::{Check, CheckKind};

/// Tail of captured output retained as evidence.
const OUTPUT_TAIL_CHARS: usize = 400;

/// Handles `command-exit-code`.
pub struct CommandCollector;

/// Build the OS command that runs `command` in `workspace`.
///
/// Not quite pure: on Windows it probes once for a POSIX shell (see below). It
/// still spawns nothing for the *pack's* command, so tests can assert the argument
/// shape without running the check itself.
pub fn build_command(workspace: &Path, command: &str) -> Command {
    // Prefer a POSIX shell, on every platform. Pack checks are written POSIX --
    // `test -f x && grep -q y x`, pipes, `2>/dev/null` -- and `cmd` rejects those
    // outright, so a pack that passes on the Linux CI box fails on a Windows
    // desktop with a shell error that reads as a failed CHECK. An audit that
    // reports non-compliance because of the host's shell is worse than useless.
    //
    // Deliberately duplicated from `sc_verify::host_shell` rather than shared:
    // taking a dependency on the verification crate to answer "which shell" would
    // couple the compliance engine to the agent stack. If a third site ever needs
    // this, that is the point to lift it into `sc-proto`.
    let posix = Command::new("sh")
        .arg("-c")
        .arg("exit 0")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let (shell, flag) = if cfg!(windows) && !posix {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut c = Command::new(shell);
    c.arg(flag).arg(command).current_dir(workspace);
    c
}

impl Collector for CommandCollector {
    fn name(&self) -> &'static str {
        "command"
    }

    fn handles(&self, kind: &CheckKind) -> bool {
        matches!(kind, CheckKind::CommandExitCode { .. })
    }

    fn collect(&self, check: &Check, ctx: &AuditContext<'_>) -> Result<Observation> {
        let (command, expect_codes) = match &check.kind {
            CheckKind::CommandExitCode {
                command,
                expect_codes,
                ..
            } => (command, expect_codes),
            other => {
                return Err(DcError::Comply(format!(
                    "CommandCollector cannot handle {}",
                    other.label()
                )))
            }
        };

        if !ctx.options.allow_commands {
            return Ok(Observation::indeterminate(format!(
                "command checks are disabled for this run; {command:?} was not executed \
                 (enable with allow_commands after reviewing the pack)"
            )));
        }

        let output = match build_command(ctx.root, command).output() {
            Ok(o) => o,
            Err(e) => {
                // Failing to spawn is a *tool* failure, not a compliance
                // judgment, so it propagates as Err and becomes
                // ControlStatus::Error rather than a gap.
                return Err(DcError::Comply(format!(
                    "check {:?}: could not run {command:?}: {e}",
                    check.id
                )));
            }
        };

        let code = output.status.code();
        let matched = code.map(|c| expect_codes.contains(&c)).unwrap_or(false);

        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        let tail = tail_chars(&combined, OUTPUT_TAIL_CHARS);

        let excerpt = match code {
            Some(c) => format!("$ {command} -> exit {c}\n{tail}"),
            None => format!("$ {command} -> terminated by signal\n{tail}"),
        };

        Ok(Observation {
            matched: Some(matched),
            evidence: vec![Evidence::new(
                "<command>",
                None,
                excerpt,
                &check.id,
                self.name(),
            )],
            note: None,
        })
    }
}

/// Last `n` chars of `s`, on a char boundary.
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.trim().to_string();
    }
    s.chars()
        .skip(count - n)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::ComplyOptions;
    use crate::scan::TextFile;
    use crate::status::Outcome;

    fn check(id: &str, command: &str) -> Check {
        Check {
            id: id.to_string(),
            kind: CheckKind::CommandExitCode {
                command: command.to_string(),
                expect_codes: vec![0],
                timeout_secs: 30,
            },
            on_match: Outcome::Pass,
            on_no_match: Outcome::Gap,
            on_no_files: None,
            weight: 1.0,
            exclude_globs: vec![],
            tracked_only: false,
            rationale: String::new(),
        }
    }

    #[test]
    fn disabled_by_default_yields_unknown_with_a_reason() {
        // The security-critical default. Note this test never spawns anything.
        let files: Vec<TextFile> = vec![];
        let opts = ComplyOptions::default();
        assert!(!opts.allow_commands);

        let ctx = AuditContext::new(Path::new("/ws"), &files, &opts);
        let o = CommandCollector
            .collect(&check("audit", "cargo audit"), &ctx)
            .expect("collect");

        assert_eq!(
            o.matched, None,
            "a disabled check must never read as pass or gap"
        );
        let note = o.note.expect("must state why");
        assert!(note.contains("disabled"), "{note}");
        assert!(note.contains("cargo audit"), "{note}");
        assert!(o.evidence.is_empty());
    }

    #[test]
    fn the_disabled_capability_is_named_for_the_report() {
        let opts = ComplyOptions::default();
        assert_eq!(
            opts.disabled_capabilities(),
            vec!["command-exit-code".to_string()]
        );
    }

    #[test]
    fn builds_a_shell_command_without_spawning_it() {
        // Assert the shape; do not run the pack's command. (The shell probe itself
        // does spawn `sh -c 'exit 0'` once — that is not the check.)
        let cmd = build_command(Path::new("/ws"), "cargo audit");
        let program = cmd.get_program().to_string_lossy().into_owned();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        // Pack checks are written POSIX, so a POSIX shell is preferred wherever one
        // exists — including Windows, where this repo already requires one. `cmd`
        // is only the fallback when there is genuinely no `sh`, and it would fail
        // most packs' `test -f x && grep -q y x` shapes anyway.
        let (want_shell, want_flag) = if program == "cmd" {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        assert_eq!(program, want_shell);
        assert_eq!(args, vec![want_flag.to_string(), "cargo audit".to_string()]);
        if !cfg!(windows) {
            assert_eq!(program, "sh", "POSIX hosts always use sh");
        }
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/ws")));
    }

    #[test]
    fn handles_only_its_own_kind() {
        assert!(CommandCollector.handles(&CheckKind::CommandExitCode {
            command: "x".into(),
            expect_codes: vec![0],
            timeout_secs: 1,
        }));
        assert!(!CommandCollector.handles(&CheckKind::FileAbsent {
            path: ".env".into()
        }));
    }

    #[test]
    fn rejects_a_kind_it_does_not_handle() {
        let c = Check {
            id: "x".into(),
            kind: CheckKind::FileAbsent {
                path: ".env".into(),
            },
            on_match: Outcome::Gap,
            on_no_match: Outcome::Pass,
            on_no_files: None,
            weight: 1.0,
            exclude_globs: vec![],
            tracked_only: false,
            rationale: String::new(),
        };
        let files: Vec<TextFile> = vec![];
        // Enabled, to prove the rejection is about the kind and not the gate.
        let opts = ComplyOptions {
            allow_commands: true,
            ..Default::default()
        };
        let ctx = AuditContext::new(Path::new("/ws"), &files, &opts);
        assert!(CommandCollector.collect(&c, &ctx).is_err());
    }

    #[test]
    fn tail_keeps_the_end_and_respects_char_boundaries() {
        assert_eq!(tail_chars("short", 100), "short");
        let long = "é".repeat(500);
        assert_eq!(tail_chars(&long, 10).chars().count(), 10);
    }
}
