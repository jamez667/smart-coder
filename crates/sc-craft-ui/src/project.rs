//! What kind of project is open, and how to compile it.
//!
//! Pure and host-testable: detection is a question about the file tree, and building the command
//! line is string work. Neither needs a GUI, a toolchain, or a Unity install to test.
//!
//! **The seam is `project kind → compile command → parsed diagnostics`** (spec 21). Unity is the
//! case that motivated it and the most involved implementation, but it is deliberately not
//! special-cased: every kind here answers the same three questions, so adding one is adding a
//! match arm rather than a subsystem.
//!
//! This matters most in Craft mode. With the agent switched off there is nobody to ask "does it
//! compile?", so the loop the model used to close has to close with a button.

/// A recognised project type.
///
/// Detected from the tree, never configured — the same rule
/// [`crate::config::detect_verify_command`] already follows. A project we don't recognise is
/// [`ProjectKind::Unknown`], which is a first-class answer: the panel says what it looked for
/// rather than showing a button that cannot work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    /// `Assets/` + `ProjectSettings/ProjectVersion.txt`.
    Unity,
    /// `Cargo.toml`.
    Cargo,
    /// A `.sln` or `.csproj` (and not a Unity project — Unity generates these too).
    DotNet,
    /// `package.json`.
    Npm,
    /// Nothing recognised.
    Unknown,
}

impl ProjectKind {
    /// Human label for the Problems panel.
    pub fn label(self) -> &'static str {
        match self {
            ProjectKind::Unity => "Unity",
            ProjectKind::Cargo => "Cargo",
            ProjectKind::DotNet => ".NET",
            ProjectKind::Npm => "npm",
            ProjectKind::Unknown => "unknown",
        }
    }

    /// Whether a compile can be offered at all.
    pub fn compilable(self) -> bool {
        !matches!(self, ProjectKind::Unknown)
    }
}

/// Detect the project kind from `root`'s contents.
///
/// **Unity is checked first, and that order is load-bearing:** Unity generates `.csproj`/`.sln`
/// files for IDE integration, so a Unity project also looks like a .NET one. Compiling it with
/// `dotnet build` would either fail or — worse — appear to succeed while checking something
/// other than what Unity actually builds.
pub fn detect(root: &std::path::Path) -> ProjectKind {
    if is_unity(root) {
        return ProjectKind::Unity;
    }
    if root.join("Cargo.toml").is_file() {
        return ProjectKind::Cargo;
    }
    if has_dotnet_project(root) {
        return ProjectKind::DotNet;
    }
    if root.join("package.json").is_file() {
        return ProjectKind::Npm;
    }
    ProjectKind::Unknown
}

/// A Unity project: an `Assets/` directory AND `ProjectSettings/ProjectVersion.txt`.
///
/// Both are required. `Assets/` alone is a common enough directory name to be a false positive,
/// and `ProjectVersion.txt` is what makes the build reproducible — it names the exact editor
/// version, which is the difference between building the project and guessing at it.
fn is_unity(root: &std::path::Path) -> bool {
    root.join("Assets").is_dir() && root.join("ProjectSettings/ProjectVersion.txt").is_file()
}

/// Whether `root` holds a `.sln` or `.csproj` at its top level.
fn has_dotnet_project(root: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|e| {
        let p = e.path();
        p.extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("sln") || x.eq_ignore_ascii_case("csproj"))
    })
}

/// The Unity editor version a project was made with, from `ProjectSettings/ProjectVersion.txt`.
///
/// The file's first line is `m_EditorVersion: 2022.3.10f1`. Returns `None` if it's missing or
/// shaped differently — the caller then says *which* version it wanted rather than guessing at
/// an install, because launching the wrong editor version can silently upgrade the project.
pub fn unity_version(root: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("ProjectSettings/ProjectVersion.txt")).ok()?;
    parse_unity_version(&text)
}

/// Pull `m_EditorVersion` out of a `ProjectVersion.txt`. Pure, so the parse is testable without
/// a Unity install.
pub fn parse_unity_version(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("m_EditorVersion:"))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Where Unity Hub installs editors, by convention. Used only as a search root — an explicit
/// override in Settings always wins, because Hub's location is configurable and this is a guess.
#[cfg(windows)]
fn unity_hub_roots() -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(var) {
            roots.push(std::path::PathBuf::from(base).join("Unity/Hub/Editor"));
        }
    }
    roots
}

#[cfg(not(windows))]
fn unity_hub_roots() -> Vec<std::path::PathBuf> {
    vec![
        std::path::PathBuf::from("/Applications/Unity/Hub/Editor"),
        std::path::PathBuf::from("/opt/unity/Hub/Editor"),
    ]
}

/// The editor executable inside a Hub install directory for one version.
#[cfg(windows)]
fn unity_exe_in(version_dir: &std::path::Path) -> std::path::PathBuf {
    version_dir.join("Editor/Unity.exe")
}

#[cfg(not(windows))]
fn unity_exe_in(version_dir: &std::path::Path) -> std::path::PathBuf {
    version_dir.join("Editor/Unity")
}

/// Why the Unity editor couldn't be located — stated, never guessed around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnityMissing {
    /// No `ProjectVersion.txt`, or it didn't name a version.
    NoVersion,
    /// The version is known but no install was found. Carries the version so the message can
    /// name it: "Unity 2022.3.10f1 not found" is actionable; "Unity not found" is not.
    NotInstalled { version: String },
}

impl UnityMissing {
    /// The sentence shown in the Problems panel.
    pub fn reason(&self) -> String {
        match self {
            UnityMissing::NoVersion => {
                "ProjectSettings/ProjectVersion.txt is missing or doesn't name an editor \
                 version, so the right Unity can't be chosen."
                    .to_string()
            }
            UnityMissing::NotInstalled { version } => format!(
                "Unity {version} was not found. Install it via Unity Hub, or set the editor \
                 path in Settings."
            ),
        }
    }
}

/// Locate the Unity editor for the project at `root`.
///
/// `override_path`, when set and present on disk, always wins — Hub's install root is
/// configurable and machines vary, so the convention below is a convenience, not an assumption.
pub fn find_unity(
    root: &std::path::Path,
    override_path: Option<&str>,
) -> Result<std::path::PathBuf, UnityMissing> {
    if let Some(p) = override_path.map(str::trim).filter(|s| !s.is_empty()) {
        let p = std::path::PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    let Some(version) = unity_version(root) else {
        return Err(UnityMissing::NoVersion);
    };
    for hub in unity_hub_roots() {
        let exe = unity_exe_in(&hub.join(&version));
        if exe.is_file() {
            return Ok(exe);
        }
    }
    Err(UnityMissing::NotInstalled { version })
}

/// A compile invocation: the program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl CompileCommand {
    /// How the command reads in the UI, so the user can see exactly what will run.
    pub fn display(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

/// The Unity headless-compile arguments.
///
/// `-quit -batchmode` compiles the assemblies and exits without entering play mode or opening
/// the GUI. `-logFile -` sends the log to stdout, which is what we parse; without it Unity
/// writes to a platform-specific file and prints nothing. `-nographics` avoids initialising a
/// graphics device on a machine that may not have one available to a background process.
///
/// Pure and separate from [`compile_command`] so the argument list can be asserted on without a
/// Unity install present.
pub fn unity_args(project: &str) -> Vec<String> {
    vec![
        "-quit".to_string(),
        "-batchmode".to_string(),
        "-nographics".to_string(),
        "-projectPath".to_string(),
        project.to_string(),
        "-logFile".to_string(),
        "-".to_string(),
    ]
}

/// The command that compiles the project at `root`, or the reason there isn't one.
///
/// Unity needs its editor located first, which is why this can fail where the others can't.
pub fn compile_command(
    root: &std::path::Path,
    kind: ProjectKind,
    unity_override: Option<&str>,
) -> Result<CompileCommand, String> {
    match kind {
        ProjectKind::Unity => {
            let exe = find_unity(root, unity_override).map_err(|e| e.reason())?;
            Ok(CompileCommand {
                program: exe.to_string_lossy().to_string(),
                args: unity_args(&root.to_string_lossy()),
            })
        }
        // `cargo check` rather than `build`: the question the button answers is "does it
        // compile?", and check answers it several times faster. `--message-format=short` gives
        // the one-line `file:line:col: error: msg` form the parser reads.
        ProjectKind::Cargo => Ok(CompileCommand {
            program: "cargo".to_string(),
            args: vec![
                "check".to_string(),
                "--workspace".to_string(),
                "--all-targets".to_string(),
                "--message-format=short".to_string(),
            ],
        }),
        ProjectKind::DotNet => Ok(CompileCommand {
            program: "dotnet".to_string(),
            args: vec!["build".to_string(), "--nologo".to_string()],
        }),
        // Type-checking is the closest thing to "compile" in a JS project, and `--noEmit` means
        // it reports without writing output.
        ProjectKind::Npm => Ok(CompileCommand {
            program: "npx".to_string(),
            args: vec![
                "tsc".to_string(),
                "--noEmit".to_string(),
                "--pretty".to_string(),
                "false".to_string(),
            ],
        }),
        ProjectKind::Unknown => Err("No recognised project here. Looked for a Unity project \
             (Assets/ + ProjectSettings/), Cargo.toml, a .sln/.csproj, or package.json."
            .to_string()),
    }
}

/// Whether Unity's output says the project is locked by a running editor.
///
/// A headless build fails while the GUI editor has the project open, and Unity's own message for
/// it is easy to miss in a long log. Detected so the panel can say the actionable thing ("close
/// the Unity editor") instead of surfacing a lock-file error.
pub fn is_unity_lock_error(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    lower.contains("multiple unity instances cannot open the same project")
        || (lower.contains("another unity instance") && lower.contains("running"))
        || lower.contains("failed to acquire the project lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temp dir with the given relative paths created (dirs end in `/`).
    fn tree(name: &str, entries: &[&str]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "sc-win-proj-{name}-{:?}",
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for e in entries {
            let p = root.join(e.trim_end_matches('/'));
            if e.ends_with('/') {
                std::fs::create_dir_all(&p).unwrap();
            } else {
                if let Some(d) = p.parent() {
                    std::fs::create_dir_all(d).unwrap();
                }
                std::fs::write(&p, "x").unwrap();
            }
        }
        root
    }

    #[test]
    fn a_unity_project_needs_both_markers() {
        // Assets/ alone is far too common a directory name to be conclusive.
        let only_assets = tree("assets-only", &["Assets/"]);
        assert_eq!(detect(&only_assets), ProjectKind::Unknown);

        let full = tree("unity", &["Assets/", "ProjectSettings/ProjectVersion.txt"]);
        assert_eq!(detect(&full), ProjectKind::Unity);

        let _ = std::fs::remove_dir_all(only_assets);
        let _ = std::fs::remove_dir_all(full);
    }

    #[test]
    fn unity_wins_over_the_csproj_files_it_generates() {
        // THE ordering bug this guards: Unity emits .csproj/.sln for IDE integration, so a
        // Unity project also looks like a .NET one. Building it with `dotnet build` would check
        // something other than what Unity actually compiles.
        let root = tree(
            "unity-csproj",
            &[
                "Assets/",
                "ProjectSettings/ProjectVersion.txt",
                "Assembly-CSharp.csproj",
                "MyGame.sln",
            ],
        );
        assert_eq!(detect(&root), ProjectKind::Unity, "not DotNet");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_other_kinds_are_detected_too() {
        let cargo = tree("cargo", &["Cargo.toml"]);
        assert_eq!(detect(&cargo), ProjectKind::Cargo);

        let dotnet = tree("dotnet", &["App.csproj"]);
        assert_eq!(detect(&dotnet), ProjectKind::DotNet);

        let npm = tree("npm", &["package.json"]);
        assert_eq!(detect(&npm), ProjectKind::Npm);

        let empty = tree("empty", &["readme.md"]);
        assert_eq!(detect(&empty), ProjectKind::Unknown);
        assert!(!ProjectKind::Unknown.compilable());

        for d in [cargo, dotnet, npm, empty] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn the_editor_version_is_read_from_project_version_txt() {
        // The real file's shape, including the second line Unity writes.
        let text = "m_EditorVersion: 2022.3.10f1\nm_EditorVersionWithRevision: 2022.3.10f1 (abc)\n";
        assert_eq!(parse_unity_version(text).as_deref(), Some("2022.3.10f1"));
        // Missing / malformed → None, so the caller says what it wanted rather than guessing an
        // install. Launching the WRONG editor version can silently upgrade the project.
        assert_eq!(parse_unity_version("").as_deref(), None);
        assert_eq!(parse_unity_version("nothing useful\n").as_deref(), None);
        assert_eq!(
            parse_unity_version("m_EditorVersion:   \n").as_deref(),
            None
        );
    }

    #[test]
    fn the_unity_command_line_compiles_without_opening_the_editor() {
        let args = unity_args("C:/game");
        // -quit and -batchmode are what make this a compile rather than a launch; without
        // -logFile - Unity writes to a platform-specific file and prints nothing to parse.
        for expected in ["-quit", "-batchmode", "-nographics", "-logFile"] {
            assert!(
                args.iter().any(|a| a == expected),
                "missing {expected}: {args:?}"
            );
        }
        let i = args.iter().position(|a| a == "-projectPath").unwrap();
        assert_eq!(args[i + 1], "C:/game", "the project path follows the flag");
        let i = args.iter().position(|a| a == "-logFile").unwrap();
        assert_eq!(args[i + 1], "-", "log goes to stdout so we can read it");
    }

    #[test]
    fn an_unknown_project_explains_what_was_looked_for() {
        // A dead button with no explanation is the failure mode here — the user can't tell
        // whether the feature is broken or their project simply isn't recognised.
        let root = tree("unknown-cmd", &["notes.txt"]);
        let err = compile_command(&root, ProjectKind::Unknown, None).unwrap_err();
        assert!(err.contains("Unity"), "names what it looked for: {err}");
        assert!(err.contains("Cargo.toml"));
        assert!(err.contains("package.json"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_missing_unity_names_the_version_it_wanted() {
        let root = tree("no-unity", &["Assets/", "ProjectSettings/"]);
        std::fs::write(
            root.join("ProjectSettings/ProjectVersion.txt"),
            "m_EditorVersion: 9999.1.0f1\n",
        )
        .unwrap();

        match find_unity(&root, None) {
            Err(e @ UnityMissing::NotInstalled { .. }) => {
                assert!(
                    e.reason().contains("9999.1.0f1"),
                    "the message must name the version: {}",
                    e.reason()
                );
            }
            // If a real Unity 9999.1.0f1 exists on this machine, something is very wrong.
            other => panic!("expected NotInstalled, got {other:?}"),
        }

        // No version at all is a different failure with a different fix.
        std::fs::write(root.join("ProjectSettings/ProjectVersion.txt"), "junk\n").unwrap();
        assert_eq!(find_unity(&root, None), Err(UnityMissing::NoVersion));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn an_explicit_editor_path_overrides_the_hub_convention() {
        // Hub's install root is configurable, so the convention is a convenience. A path the
        // user set must always win — as long as it actually exists.
        let root = tree("override", &["Assets/", "ProjectSettings/"]);
        std::fs::write(
            root.join("ProjectSettings/ProjectVersion.txt"),
            "m_EditorVersion: 2022.3.10f1\n",
        )
        .unwrap();
        let fake_editor = root.join("MyUnity.exe");
        std::fs::write(&fake_editor, "not really unity").unwrap();

        let found = find_unity(&root, Some(fake_editor.to_str().unwrap())).unwrap();
        assert_eq!(found, fake_editor);

        // A path that doesn't exist falls through to the search rather than being trusted.
        assert!(find_unity(&root, Some("Z:/nope/Unity.exe")).is_err());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_locked_project_is_recognised_from_unitys_output() {
        // Unity's message is easy to lose in a long log; the panel should say "close the
        // editor" rather than surfacing a lock-file error the user has to interpret.
        assert!(is_unity_lock_error(
            "Multiple Unity instances cannot open the same project."
        ));
        assert!(is_unity_lock_error(
            "Another Unity instance is running with this project open"
        ));
        assert!(!is_unity_lock_error("Compilation succeeded"));
    }
}
