//! The project's technology stack, detected from the workspace — the language-specific
//! constraint woven into every phase prompt.
//!
//! The workflow was originally hard-locked to Python/Flask (the small-model eval ladder ran
//! on that stack). Driving a real project — a Rust cargo workspace, say — needs the phase
//! prompts to speak that project's language, or the orchestrator designs a Flask `app.py`
//! against a Rust codebase. [`ProjectStack::detect`] reads the workspace once per run and
//! [`ProjectStack::constraint`] returns the stack rules to weave in; the engine threads it
//! into the phase prompts.

use std::path::Path;

/// The stack a workflow run targets, detected from the workspace. Determines the
/// language-specific constraint woven into every phase prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStack {
    /// A Rust cargo project/workspace (a `Cargo.toml` is present). Edit-in-place, `.rs`
    /// files, gate with `cargo check`.
    Rust,
    /// A JavaScript/web project (a `package.json` is present).
    JsWeb,
    /// The Python + Flask + pytest eval stack. Set EXPLICITLY by the eval ladder (its exact
    /// pytest wording is depended on); NOT the fallback for unrecognized projects — see
    /// [`ProjectStack::Unknown`].
    Python,
    /// A Unity C# project (an `Assets/` and `ProjectSettings/` dir are present). Edit
    /// `.cs` files in place, gate with the Unity Editor batchmode CLI.
    Unity,
    /// An unrecognized stack (no Cargo.toml / package.json / Python / Unity markers). Treated
    /// as a generic edit-in-place project: the design breakdown is ordered Markdown, NOT the
    /// Python pytest coverage array. Previously this fell back to [`Python`] and wrongly emitted
    /// `test_app.py` coverage JSON for e.g. a Go or C++ project (caught by an end-to-end run).
    Unknown,
}

impl ProjectStack {
    /// Detect the stack from what's on disk at the workspace root. Mirrors the verify-command
    /// detection in the app (`Cargo.toml` → Rust, `package.json` → JS). A Python marker
    /// (`requirements.txt`/`pyproject.toml`/`setup.py`) → Python; anything else → `Unknown`
    /// (a generic edit-in-place project), NOT Python — so a Go/C++/pre-init project doesn't get
    /// pytest coverage JSON.
    pub fn detect(workspace: &Path) -> ProjectStack {
        // Unity's canonical signature — checked first, since a Unity repo may also carry
        // stray manifests (and Editor-generated `.csproj`/`.sln` are unreliable markers).
        if workspace.join("Assets").is_dir() && workspace.join("ProjectSettings").is_dir() {
            ProjectStack::Unity
        } else if workspace.join("Cargo.toml").is_file() {
            ProjectStack::Rust
        } else if workspace.join("package.json").is_file() {
            ProjectStack::JsWeb
        } else if workspace.join("requirements.txt").is_file()
            || workspace.join("pyproject.toml").is_file()
            || workspace.join("setup.py").is_file()
        {
            ProjectStack::Python
        } else {
            ProjectStack::Unknown
        }
    }

    /// The `STACK: …` rules woven into every phase prompt for this stack. The Python variant
    /// is the original constraint verbatim (the eval ladder depends on its exact wording); the
    /// others are edit-in-place framings for a real existing project.
    pub fn constraint(&self) -> &'static str {
        match self {
            ProjectStack::Rust => RUST_CONSTRAINT,
            ProjectStack::JsWeb => JS_CONSTRAINT,
            ProjectStack::Python => PYTHON_CONSTRAINT,
            ProjectStack::Unity => UNITY_CONSTRAINT,
            ProjectStack::Unknown => GENERIC_CONSTRAINT,
        }
    }

    /// The file extensions that belong to this stack — the decomposition drift filter keeps
    /// files with these and drops other-language code (see `sc_swarm::parse_subtasks_on_stack`).
    /// An empty slice (for `Unknown`) disables the filter so a project in a language we don't
    /// model isn't stripped to an empty board.
    pub fn on_stack_exts(&self) -> &'static [&'static str] {
        match self {
            ProjectStack::Rust => &["rs"],
            ProjectStack::JsWeb => &["js", "ts", "tsx", "jsx", "html", "css", "json"],
            ProjectStack::Python => &["py", "js", "html", "css"],
            ProjectStack::Unity => &["cs"],
            ProjectStack::Unknown => &[],
        }
    }

    /// A short label for the stack, for logging / prompts.
    pub fn label(&self) -> &'static str {
        match self {
            ProjectStack::Rust => "Rust",
            ProjectStack::JsWeb => "JavaScript",
            ProjectStack::Python => "Python",
            ProjectStack::Unity => "Unity (C#)",
            ProjectStack::Unknown => "generic",
        }
    }
}

/// Rust: an existing cargo project. Edit in place, match the project's conventions, `.rs`
/// only, gate with `cargo check`. Deliberately generic — it fits any cargo layout rather
/// than assuming a fixed set of crates.
const RUST_CONSTRAINT: &str = "STACK: this is an EXISTING Rust (cargo) project — edit it in \
    place, do not scaffold a new project or a different language. Every source file is a `.rs` \
    file inside the existing crate/module layout; add new modules where they fit and wire them \
    into the existing tree (mod declarations, use paths). Match the surrounding code's \
    conventions, error handling, and dependencies — do NOT add a new crate dependency unless \
    the plan calls for it. The build/verify gate is `cargo check` (and `cargo test` where \
    tests exist). Do NOT introduce Python, JavaScript, or any other language.";

/// JS/web: an existing package.json project. Edit in place, match its module system.
const JS_CONSTRAINT: &str = "STACK: this is an EXISTING JavaScript/web project — edit it in \
    place, do not scaffold a new project or a different language. Match the project's module \
    system, framework, and conventions already present; add new files where they fit and wire \
    them in. Do NOT add a new dependency unless the plan calls for it. The build/verify gate is \
    the project's build/test script (e.g. `npm run build`, `npm test`). Do NOT introduce Python, \
    Rust, or any other backend language.";

/// Python/Flask — the original eval-ladder constraint, verbatim. Kept exact so the small-model
/// ladder that was tuned against this wording is unaffected.
const PYTHON_CONSTRAINT: &str = "STACK: backend in Python with Flask; a frontend, ONLY IF the \
    task needs a user interface, in plain JavaScript, HTML, and CSS. Build ONLY what the task \
    asks for — if it is a backend/JSON API with no UI, create NO frontend files (no index.html, \
    script.js, or styles.css) and write NO frontend tests; app.py alone is the whole project. \
    Do NOT use TypeScript, React, Vue, a build step, or any other backend language (no \
    Node.js/Express, no Java, no Go). Every source file must be a .py, .js, .html, or .css file. \
    LIBRARIES: the installed Python packages you may import are flask, flask_sqlalchemy, \
    flask_restful, flask_cors, marshmallow, requests, pytest, and the standard library. \
    Do NOT use any package outside that list (no FastAPI, no Django) — it is not installed \
    and the tests will fail to import. Frontend uses only the browser's built-in fetch and \
    DOM APIs (no npm packages). Write Flask route handlers as plain `def`, never `async def`.";

/// Unity: an existing Unity C# project. Edit `.cs` files in place under `Assets/`, match the
/// project's MonoBehaviour conventions, do not scaffold or touch generated files.
const UNITY_CONSTRAINT: &str = "STACK: this is an EXISTING Unity project written in C# — edit it \
    in place, do not scaffold a new project or a different language. Source files are `.cs` \
    files under `Assets/` (typically `Assets/Scripts/…`); add new scripts where they fit and \
    match the surrounding MonoBehaviour/ScriptableObject conventions and the `UnityEngine`/\
    `UnityEditor` namespaces already in use. Do NOT edit generated files or directories \
    (`.csproj`, `.sln`, `Library/`, `Temp/`, `obj/`) — Unity regenerates them. Do NOT add a new \
    package unless the plan calls for it. The build/verify gate is the Unity Editor batchmode \
    CLI (EditMode tests). Do NOT introduce Python, JavaScript, Rust, or any other language.";

/// Generic: an unrecognized existing project (no Cargo.toml/package.json/Python/Unity markers,
/// e.g. Go, C++, or a repo not yet initialized). Edit in place in whatever language the existing
/// files use — don't assume Python/Flask, and don't scaffold a new project.
const GENERIC_CONSTRAINT: &str = "STACK: this is an EXISTING project — edit it in place in the \
    SAME language and conventions as the files already present (infer the language from the \
    existing source and the plan's Files-to-touch). Do NOT scaffold a new project, and do NOT \
    assume Python/Flask, Node, or any specific framework. Add new files where they fit and wire \
    them into the existing layout. Do NOT add a new dependency unless the plan calls for it.";

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("dc-stack-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&p);
        p
    }

    #[test]
    fn detects_rust_from_cargo_toml() {
        let ws = temp("rust");
        std::fs::write(ws.join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        assert_eq!(ProjectStack::detect(&ws), ProjectStack::Rust);
        assert!(ProjectStack::Rust.constraint().contains("cargo"));
        assert!(!ProjectStack::Rust.constraint().to_lowercase().contains("flask"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn detects_js_from_package_json() {
        let ws = temp("js");
        std::fs::write(ws.join("package.json"), "{}").unwrap();
        assert_eq!(ProjectStack::detect(&ws), ProjectStack::JsWeb);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn detects_python_only_with_a_python_marker() {
        let ws = temp("py");
        std::fs::write(ws.join("requirements.txt"), "flask\n").unwrap();
        assert_eq!(ProjectStack::detect(&ws), ProjectStack::Python);
        assert!(ProjectStack::Python.constraint().contains("Flask"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn unrecognized_workspace_is_unknown_not_python() {
        // Regression: a project with no manifest (Go, C++, pre-init) must NOT be treated as
        // Python — that made the design breakdown emit pytest coverage JSON (caught end-to-end).
        let ws = temp("unknown");
        std::fs::write(ws.join("main.go"), "package main\n").unwrap();
        assert_eq!(ProjectStack::detect(&ws), ProjectStack::Unknown);
        let c = ProjectStack::Unknown.constraint();
        // It's the generic edit-in-place framing, NOT the Python/Flask eval constraint.
        assert_ne!(c, ProjectStack::Python.constraint(), "unknown != python constraint");
        assert!(c.contains("SAME language"), "generic infers the language: {c}");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn detects_unity_from_dir_signature() {
        let ws = temp("unity");
        // Unity signature plus a stray Cargo.toml — Unity still wins.
        std::fs::create_dir_all(ws.join("Assets")).unwrap();
        std::fs::create_dir_all(ws.join("ProjectSettings")).unwrap();
        std::fs::write(ws.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(ProjectStack::detect(&ws), ProjectStack::Unity);
        let c = ProjectStack::Unity.constraint();
        assert!(c.contains("Unity") && c.contains(".cs"));
        assert!(!c.to_lowercase().contains("flask"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn cargo_wins_when_both_present() {
        // A polyglot repo with both markers: Rust takes precedence (mirrors the app's
        // verify-command detection order).
        let ws = temp("both");
        std::fs::write(ws.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(ws.join("package.json"), "{}").unwrap();
        assert_eq!(ProjectStack::detect(&ws), ProjectStack::Rust);
        let _ = std::fs::remove_dir_all(&ws);
    }
}
