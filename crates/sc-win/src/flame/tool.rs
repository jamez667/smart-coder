//! Finding a sampling profiler, and building the command that runs it.
//!
//! Separate from [`super`] because the viewer must not depend on this. Parsing and drawing a
//! flame graph works on any machine; *recording* one needs a tool that may not be installed and,
//! on Windows, may need an Administrator shell. Keeping the two apart is what lets the section
//! be useful in the common case where neither is true.
//!
//! # Why absence is a state, not an error
//!
//! Checked while building this: on the development machine here, `cargo-flamegraph`, `samply`,
//! `perf` and `dtrace` are all absent. That is the *normal* starting condition, not a broken
//! setup — so [`detect`] returns what it found, [`Missing::reason`] explains what to install,
//! and the UI shows an install hint next to a working Open-a-profile button. The same shape
//! [`crate::project::UnityMissing`] already uses for a missing editor.

use std::path::Path;

use crate::project::CompileCommand;

/// A sampling profiler this section knows how to drive.
///
/// Ordered by preference in [`detect`]. Both emit folded stacks, which is the only thing
/// required of them — see [`super::parse_folded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profiler {
    /// `cargo flamegraph` — the cargo subcommand from the `flamegraph` crate.
    ///
    /// Preferred for a Cargo project because it knows how to build the target with debug symbols
    /// and profile the resulting binary in one step. On Windows it samples through `blondie`,
    /// which usually requires an elevated shell.
    CargoFlamegraph,
    /// `samply record` — a standalone sampler.
    ///
    /// Works on an already-built binary, and on Windows it does not need elevation, which makes
    /// it the better answer here even though it is the second choice generally.
    Samply,
}

impl Profiler {
    /// The executable a `which`-style probe looks for.
    ///
    /// `cargo flamegraph` is a cargo *subcommand*, so what exists on `PATH` is the
    /// `cargo-flamegraph` binary, not a `flamegraph` one.
    pub fn program(self) -> &'static str {
        match self {
            Profiler::CargoFlamegraph => "cargo-flamegraph",
            Profiler::Samply => "samply",
        }
    }

    /// The arguments that make the tool prove it is runnable.
    ///
    /// **A cargo subcommand does not answer a bare `--version`.** `cargo-flamegraph --version`
    /// exits with "unexpected argument", because the binary expects to be invoked as
    /// `cargo flamegraph …` and so wants its own subcommand name first. Probing it the way a
    /// standalone tool is probed reports "not installed" on a machine that has it — which is
    /// exactly the bug this shape exists to prevent.
    pub fn probe_args(self) -> &'static [&'static str] {
        match self {
            Profiler::CargoFlamegraph => &["flamegraph", "--version"],
            Profiler::Samply => &["--version"],
        }
    }

    /// How the tool is named to a human.
    pub fn label(self) -> &'static str {
        match self {
            Profiler::CargoFlamegraph => "cargo flamegraph",
            Profiler::Samply => "samply",
        }
    }

    /// The one-line install instruction, for the UI to show and to copy.
    pub fn install_hint(self) -> &'static str {
        match self {
            Profiler::CargoFlamegraph => "cargo install flamegraph",
            Profiler::Samply => "cargo install samply",
        }
    }
}

/// Why no profile can be recorded here.
///
/// Carries enough to *act on*, never just "failed": the UI turns each of these into a sentence
/// with the exact command to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    /// No supported profiler was found on `PATH`.
    NoProfiler,
    /// A profiler exists, but this project isn't one it can drive.
    ///
    /// Recording is Cargo-only for now. Every *other* project kind can still open a folded file
    /// produced however the user likes, which is why this is not a hard block on the section.
    NotCargo { kind: &'static str },
}

impl Missing {
    /// A user-facing sentence naming the fix.
    pub fn reason(&self) -> String {
        match self {
            Missing::NoProfiler => format!(
                "No sampling profiler found. Install one to record a profile:\n  \
                 {}\n  {}\n\nYou can still open an existing .folded or collapsed-stack file.",
                Profiler::Samply.install_hint(),
                Profiler::CargoFlamegraph.install_hint(),
            ),
            Missing::NotCargo { kind } => format!(
                "Recording a profile is supported for Cargo projects; this is {kind}. \
                 You can still open an existing .folded or collapsed-stack file."
            ),
        }
    }
}

/// Look for a supported profiler on `PATH`.
///
/// `samply` first: it needs no elevation on Windows, where `cargo flamegraph`'s `blondie`
/// backend generally does, so preferring it means the button that appears is the one more
/// likely to work when pressed.
///
/// Probing spawns the tool rather than scanning `PATH`, because a `PATH` entry that is a
/// broken symlink or a wrong-architecture binary would pass a scan and fail on use.
pub fn detect() -> Option<Profiler> {
    [Profiler::Samply, Profiler::CargoFlamegraph]
        .into_iter()
        .find(|p| is_installed(*p))
}

/// Whether one profiler is present and runnable.
///
/// Public so the panel can re-probe on demand: [`detect`] runs at boot, and a user who installs
/// a profiler while the app is open must not have to restart to be believed.
pub fn is_installed(p: Profiler) -> bool {
    use std::process::Stdio;
    crate::proc::command(p.program())
        .args(p.probe_args())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// What to profile: one of the project's runnable targets.
///
/// Deliberately small. A profiler can be pointed at anything, but a *menu* of arbitrary targets
/// is a research project; these three cover "the thing I just built", "the benchmark I wrote"
/// and "the test that's slow", which is what people actually profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A binary target, by name. `None` means the package's default.
    Bin(Option<String>),
    /// A benchmark target, by name.
    Bench(String),
    /// The test binary, optionally filtered.
    Test(Option<String>),
}

impl Target {
    /// The cargo arguments selecting this target.
    fn args(&self) -> Vec<String> {
        // An unnamed bench or test selects ALL of them (`--benches` / `--tests`). Emitting the
        // singular flag with an empty name would hand cargo `--bench ""`, which is not "the
        // default bench" but an error — a target named the empty string.
        match self {
            Target::Bin(None) => vec![],
            Target::Bin(Some(n)) if n.trim().is_empty() => vec![],
            Target::Bin(Some(n)) => vec!["--bin".into(), n.clone()],
            Target::Bench(n) if n.trim().is_empty() => vec!["--benches".into()],
            Target::Bench(n) => vec!["--bench".into(), n.clone()],
            Target::Test(None) => vec!["--tests".into()],
            Target::Test(Some(n)) if n.trim().is_empty() => vec!["--tests".into()],
            Target::Test(Some(n)) => vec!["--test".into(), n.clone()],
        }
    }

    /// How the target reads in the UI.
    pub fn label(&self) -> String {
        match self {
            Target::Bin(None) => "default binary".to_string(),
            Target::Bin(Some(n)) => format!("bin: {n}"),
            Target::Bench(n) if n.trim().is_empty() => "benches".to_string(),
            Target::Bench(n) => format!("bench: {n}"),
            Target::Test(None) => "tests".to_string(),
            Target::Test(Some(n)) => format!("test: {n}"),
        }
    }
}

/// The file a run writes its folded stacks to, inside the workspace's target dir.
///
/// Under `target/` so it inherits the project's existing `.gitignore` — a profile is build
/// output, and nobody wants one committed.
pub fn folded_path(root: &Path) -> std::path::PathBuf {
    root.join("target").join("sc-profile.folded")
}

/// Build the command that records a profile.
///
/// Returns the command *and* nothing else: like [`crate::project::compile_command`], this is
/// pure so the argument list can be asserted without either tool installed.
///
/// # The `--` and what follows it
///
/// Both tools take profiler flags, then `--`, then the program's own arguments. `args` is the
/// user's free-text argument string, split on whitespace; empty means none.
pub fn profile_command(
    root: &Path,
    kind: crate::project::ProjectKind,
    profiler: Option<Profiler>,
    target: &Target,
    args: &str,
) -> Result<CompileCommand, Missing> {
    if kind != crate::project::ProjectKind::Cargo {
        return Err(Missing::NotCargo { kind: kind.label() });
    }
    let Some(p) = profiler else {
        return Err(Missing::NoProfiler);
    };
    let extra: Vec<String> = args.split_whitespace().map(str::to_string).collect();
    let folded = folded_path(root).to_string_lossy().into_owned();

    let cmd = match p {
        // `--post-process` is how flamegraph is told to keep the collapsed stacks; `-o` names
        // the SVG, and the `.folded` lands beside it. Asking for the folded file directly keeps
        // the parser as the single source of truth rather than scraping the SVG.
        Profiler::CargoFlamegraph => {
            let mut a = vec!["flamegraph".to_string()];
            a.extend(target.args());
            a.push("--output".into());
            a.push(folded.replace(".folded", ".svg"));
            a.push("--print-folded".into());
            if !extra.is_empty() {
                a.push("--".into());
                a.extend(extra);
            }
            CompileCommand {
                program: "cargo".into(),
                args: a,
            }
        }
        // `samply record --save-only` writes a profile without opening its web UI; `--profile-name`
        // is cosmetic. It profiles an already-built binary, so the caller builds first.
        Profiler::Samply => {
            let mut a = vec![
                "record".to_string(),
                "--save-only".into(),
                "--output".into(),
                folded,
            ];
            a.push("--".into());
            a.push("cargo".into());
            a.push("run".into());
            a.extend(target.args());
            if !extra.is_empty() {
                a.push("--".into());
                a.extend(extra);
            }
            CompileCommand {
                program: "samply".into(),
                args: a,
            }
        }
    };
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectKind;

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from("/w")
    }

    #[test]
    fn a_non_cargo_project_says_so_and_names_the_kind() {
        let e = profile_command(
            &root(),
            ProjectKind::Npm,
            Some(Profiler::Samply),
            &Target::Bin(None),
            "",
        )
        .unwrap_err();
        assert_eq!(e, Missing::NotCargo { kind: "npm" });
        // The sentence must still point at the import path, which is the whole point.
        assert!(e.reason().contains(".folded"));
    }

    #[test]
    fn no_profiler_explains_how_to_get_one() {
        let e =
            profile_command(&root(), ProjectKind::Cargo, None, &Target::Bin(None), "").unwrap_err();
        assert_eq!(e, Missing::NoProfiler);
        let why = e.reason();
        assert!(why.contains("cargo install samply"));
        assert!(why.contains("cargo install flamegraph"));
        // Absence of a tool must never read as "this section is broken".
        assert!(why.contains("still open an existing"));
    }

    #[test]
    fn cargo_flamegraph_asks_for_folded_output() {
        let c = profile_command(
            &root(),
            ProjectKind::Cargo,
            Some(Profiler::CargoFlamegraph),
            &Target::Bin(Some("sc-win".into())),
            "",
        )
        .unwrap();
        assert_eq!(c.program, "cargo");
        assert_eq!(c.args[0], "flamegraph");
        assert!(c.args.contains(&"--bin".to_string()));
        assert!(c.args.contains(&"sc-win".to_string()));
        // Without folded stacks there is nothing for the parser to read.
        assert!(c.args.contains(&"--print-folded".to_string()));
    }

    #[test]
    fn a_bench_target_selects_the_bench() {
        let c = profile_command(
            &root(),
            ProjectKind::Cargo,
            Some(Profiler::CargoFlamegraph),
            &Target::Bench("parse".into()),
            "",
        )
        .unwrap();
        assert!(c.args.windows(2).any(|w| w == ["--bench", "parse"]));
    }

    #[test]
    fn program_arguments_go_after_a_double_dash() {
        let c = profile_command(
            &root(),
            ProjectKind::Cargo,
            Some(Profiler::CargoFlamegraph),
            &Target::Bin(None),
            "--input big.json",
        )
        .unwrap();
        let dash = c
            .args
            .iter()
            .position(|a| a == "--")
            .expect("a -- separator");
        assert_eq!(&c.args[dash + 1..], ["--input", "big.json"]);
    }

    #[test]
    fn no_arguments_means_no_separator() {
        let c = profile_command(
            &root(),
            ProjectKind::Cargo,
            Some(Profiler::CargoFlamegraph),
            &Target::Bin(None),
            "   ",
        )
        .unwrap();
        assert!(!c.args.contains(&"--".to_string()));
    }

    #[test]
    fn samply_records_into_the_folded_file_we_then_read() {
        let c = profile_command(
            &root(),
            ProjectKind::Cargo,
            Some(Profiler::Samply),
            &Target::Bin(None),
            "",
        )
        .unwrap();
        assert_eq!(c.program, "samply");
        let out = folded_path(&root()).to_string_lossy().into_owned();
        assert!(c.args.contains(&out), "writes where the reader looks");
    }

    #[test]
    fn the_folded_file_lands_under_target_so_git_ignores_it() {
        let p = folded_path(&root());
        assert!(p.ends_with("sc-profile.folded"));
        assert!(p.to_string_lossy().contains("target"));
    }

    #[test]
    fn every_profiler_offers_an_install_line_and_a_probe_name() {
        for p in [Profiler::Samply, Profiler::CargoFlamegraph] {
            assert!(p.install_hint().starts_with("cargo install"));
            assert!(!p.program().is_empty());
            assert!(!p.label().is_empty());
        }
        // The cargo subcommand's binary is `cargo-flamegraph`, not `flamegraph`; probing the
        // wrong name would report "not installed" on a machine that has it.
        assert_eq!(Profiler::CargoFlamegraph.program(), "cargo-flamegraph");
    }

    #[test]
    fn an_unnamed_bench_or_test_selects_all_of_them() {
        // `--bench ""` is a target literally named the empty string, which cargo rejects. The
        // toolbar's cycle button produces exactly this state, so it must build valid arguments.
        for (t, want) in [
            (Target::Bench(String::new()), "--benches"),
            (Target::Test(None), "--tests"),
            (Target::Test(Some("  ".into())), "--tests"),
        ] {
            let c = profile_command(
                &root(),
                ProjectKind::Cargo,
                Some(Profiler::CargoFlamegraph),
                &t,
                "",
            )
            .unwrap();
            assert!(c.args.contains(&want.to_string()), "{t:?} -> {:?}", c.args);
            assert!(
                !c.args.iter().any(|a| a.is_empty()),
                "no empty argument may reach cargo: {:?}",
                c.args
            );
        }
    }

    #[test]
    fn a_cargo_subcommand_is_probed_with_its_subcommand_name() {
        // REGRESSION. `cargo-flamegraph --version` exits non-zero with "unexpected argument",
        // so a bare `--version` probe reported "no profiler found" on a machine that had it
        // installed. The subcommand name has to come first.
        assert_eq!(
            Profiler::CargoFlamegraph.probe_args(),
            ["flamegraph", "--version"]
        );
        // A standalone tool takes the plain form.
        assert_eq!(Profiler::Samply.probe_args(), ["--version"]);
        // Every probe must actually ask for the version, or it is not proving runnability.
        for p in [Profiler::Samply, Profiler::CargoFlamegraph] {
            assert!(p.probe_args().contains(&"--version"), "{p:?}");
        }
    }

    #[test]
    fn targets_read_clearly_in_the_picker() {
        assert_eq!(Target::Bin(None).label(), "default binary");
        assert_eq!(Target::Bench("x".into()).label(), "bench: x");
        assert_eq!(Target::Test(None).label(), "tests");
    }
}
