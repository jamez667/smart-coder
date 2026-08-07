//! Compiler output → a list of problems you can click.
//!
//! Pure and host-testable: parsing is string work, and pinning the real formats down in tests is
//! the only way to know the panel will populate against a live toolchain.
//!
//! **Why parse rather than dump:** the terminal already shows a wall of text, so a Problems panel
//! that did the same would add nothing. The value is a *list* — file, line, column, message —
//! where clicking a row lands the caret on the offending character (spec 21).
//!
//! Every supported toolchain emits a `file(line,col): severity code: message` shape with small
//! variations, so one parser handles them all rather than one per language.

/// How bad a diagnostic is.
///
/// Errors and warnings are counted separately so "did it build?" is answerable at a glance —
/// a single total makes 200 warnings look like a failure and one error look survivable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Compilation failed.
    Error,
    /// Compiled, but the toolchain objected.
    Warning,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One problem, located precisely enough to jump to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Path as the compiler reported it — workspace-relative where possible.
    pub file: String,
    /// 1-based line.
    pub line: usize,
    /// 1-based column. `0` when the toolchain didn't say.
    pub col: usize,
    pub severity: Severity,
    /// The compiler's code (`CS0103`, `E0425`), when there is one. Kept separate from the
    /// message so the list can show it without duplicating it into the prose.
    pub code: Option<String>,
    pub message: String,
}

impl Diagnostic {
    /// `Assets/Player.cs:12:7` — the location, as shown in the list.
    pub fn location(&self) -> String {
        if self.col > 0 {
            format!("{}:{}:{}", self.file, self.line, self.col)
        } else {
            format!("{}:{}", self.file, self.line)
        }
    }
}

/// The outcome of a compile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompileReport {
    pub diagnostics: Vec<Diagnostic>,
    /// The toolchain's exit code. `None` if it never ran or was cancelled.
    pub exit_code: Option<i32>,
    /// A reason the run itself failed (couldn't spawn, project locked) — distinct from the code
    /// failing to compile. Confusing the two sends the user hunting for a bug in their code.
    pub failure: Option<String>,
}

impl CompileReport {
    pub fn errors(&self) -> usize {
        self.count(Severity::Error)
    }

    pub fn warnings(&self) -> usize {
        self.count(Severity::Warning)
    }

    fn count(&self, s: Severity) -> usize {
        self.diagnostics.iter().filter(|d| d.severity == s).count()
    }

    /// Whether the compile succeeded.
    ///
    /// Requires BOTH a zero exit and no errors. Unity in particular can exit zero while having
    /// logged compiler errors, so trusting the exit code alone would report a broken project as
    /// green — the worst possible failure for this feature.
    pub fn ok(&self) -> bool {
        self.failure.is_none() && self.exit_code == Some(0) && self.errors() == 0
    }

    /// One line for the panel header.
    pub fn summary(&self) -> String {
        if let Some(f) = &self.failure {
            return f.clone();
        }
        let (e, w) = (self.errors(), self.warnings());
        match (e, w) {
            (0, 0) if self.ok() => "No problems.".to_string(),
            (0, 0) => {
                "Finished with no diagnostics, but the compiler reported failure.".to_string()
            }
            (0, w) => format!("{w} warning{}.", plural(w)),
            (e, 0) => format!("{e} error{}.", plural(e)),
            (e, w) => format!("{e} error{}, {w} warning{}.", plural(e), plural(w)),
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Parse a toolchain's output into diagnostics, newest-first order preserved.
///
/// Deduplicates: Unity echoes the same compiler error several times in one log (once per
/// assembly pass), and a list with each problem repeated four times reads as four problems.
pub fn parse(output: &str, root: &std::path::Path) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = Vec::new();
    for line in output.lines() {
        if let Some(d) = parse_line(line, root) {
            if !out.iter().any(|e| {
                e.file == d.file && e.line == d.line && e.col == d.col && e.message == d.message
            }) {
                out.push(d);
            }
        }
    }
    out
}

/// Parse one line, if it is a diagnostic.
///
/// Handles the two shapes every supported toolchain uses:
///   * `Assets/Player.cs(12,7): error CS0103: The name 'x' does not exist` — C#/MSBuild/Unity
///   * `src/main.rs:12:7: error[E0425]: cannot find value` — rustc short format
pub fn parse_line(line: &str, root: &std::path::Path) -> Option<Diagnostic> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    parse_csharp(line, root).or_else(|| parse_rust(line, root))
}

/// `path(line,col): severity CODE: message`
fn parse_csharp(line: &str, root: &std::path::Path) -> Option<Diagnostic> {
    let open = line.find('(')?;
    let close = line[open..].find(')')? + open;
    let (l, c) = split_line_col(&line[open + 1..close])?;

    let rest = line[close + 1..].strip_prefix(':')?.trim_start();
    let (severity, rest) = take_severity(rest)?;

    // An optional code before the colon: `CS0103: message`.
    let (code, message) = match rest.split_once(':') {
        Some((head, tail)) if is_code(head.trim()) => {
            (Some(head.trim().to_string()), tail.trim().to_string())
        }
        _ => (None, rest.trim_start_matches(':').trim().to_string()),
    };

    Some(Diagnostic {
        file: relativize(&line[..open], root),
        line: l,
        col: c,
        severity,
        code,
        message,
    })
}

/// `path:line:col: severity[CODE]: message`
///
/// Scans colons left to right looking for `<file>:<line>:<col>: `, so a Windows drive letter
/// (`C:\src\main.rs:12:7: ...`) doesn't derail the split — `C` never parses as a line number,
/// so that candidate is simply skipped.
fn parse_rust(line: &str, root: &std::path::Path) -> Option<Diagnostic> {
    let (file, l, c, rest) = split_rust_head(line)?;
    let (severity, rest) = take_severity(rest.trim_start())?;

    // rustc puts the code in brackets: `error[E0425]`.
    let (code, message) = match rest.strip_prefix('[') {
        Some(after) => match after.split_once(']') {
            Some((c, tail)) => (
                Some(c.to_string()),
                tail.trim_start_matches(':').trim().to_string(),
            ),
            None => (None, rest.trim_start_matches(':').trim().to_string()),
        },
        None => (None, rest.trim_start_matches(':').trim().to_string()),
    };

    Some(Diagnostic {
        file: relativize(&file, root),
        line: l,
        col: c,
        severity,
        code,
        message,
    })
}

/// Pull `file:line:col` off the front of a rustc-style line, returning it and the remainder
/// after the following `": "`.
fn split_rust_head(line: &str) -> Option<(String, usize, usize, &str)> {
    // Candidate split points: every colon. For each, try to read `line:col:` immediately after.
    for (i, _) in line.match_indices(':') {
        let after = &line[i + 1..];
        let Some((l_str, tail)) = after.split_once(':') else {
            continue;
        };
        let Ok(l) = l_str.trim().parse::<usize>() else {
            continue; // e.g. the `\` of a drive letter — not a line number
        };
        let Some((c_str, rest)) = tail.split_once(':') else {
            continue;
        };
        let Ok(c) = c_str.trim().parse::<usize>() else {
            continue;
        };
        if !rest.starts_with(' ') {
            continue;
        }
        return Some((line[..i].to_string(), l, c, rest));
    }
    None
}

/// `12,7` → `(12, 7)`; `12` → `(12, 0)`.
fn split_line_col(s: &str) -> Option<(usize, usize)> {
    match s.split_once(',') {
        Some((l, c)) => Some((l.trim().parse().ok()?, c.trim().parse().unwrap_or(0))),
        None => Some((s.trim().parse().ok()?, 0)),
    }
}

/// Take a leading `error`/`warning` word, returning it and the rest.
fn take_severity(s: &str) -> Option<(Severity, &str)> {
    for (word, sev) in [
        ("error", Severity::Error),
        ("warning", Severity::Warning),
        ("Error", Severity::Error),
        ("Warning", Severity::Warning),
    ] {
        if let Some(rest) = s.strip_prefix(word) {
            // Must be a whole word: `errors:` in prose isn't a diagnostic.
            if rest.starts_with([' ', ':', '[']) {
                return Some((sev, rest.trim_start()));
            }
        }
    }
    None
}

/// Whether `s` looks like a compiler code (`CS0103`, `E0425`) rather than prose.
fn is_code(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 12
        && !s.contains(' ')
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && s.chars().any(|c| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Make `path` workspace-relative with forward slashes, so it matches the file-tree keys the
/// editor opens by. Absolute paths outside the workspace are left alone.
fn relativize(path: &str, root: &std::path::Path) -> String {
    let cleaned = path.trim().replace('\\', "/");
    let root_s = root.to_string_lossy().replace('\\', "/");
    let root_s = root_s.trim_end_matches('/');
    match cleaned
        .strip_prefix(root_s)
        .map(|r| r.trim_start_matches('/'))
    {
        Some(rel) if !rel.is_empty() => rel.to_string(),
        _ => cleaned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> std::path::PathBuf {
        std::path::PathBuf::from("C:/game")
    }

    #[test]
    fn a_unity_compiler_error_becomes_a_clickable_row() {
        // The exact shape Unity logs for a C# error. If this stops parsing, the panel silently
        // shows nothing on a broken project — the worst outcome for this feature.
        let d = parse_line(
            "Assets/Scripts/Player.cs(12,7): error CS0103: The name 'foo' does not exist",
            &root(),
        )
        .expect("must parse");
        assert_eq!(d.file, "Assets/Scripts/Player.cs");
        assert_eq!((d.line, d.col), (12, 7));
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.code.as_deref(), Some("CS0103"));
        assert_eq!(d.message, "The name 'foo' does not exist");
        assert_eq!(d.location(), "Assets/Scripts/Player.cs:12:7");
    }

    #[test]
    fn warnings_parse_and_are_counted_apart_from_errors() {
        let out = "Assets/A.cs(1,1): warning CS0168: variable declared but never used\n\
                   Assets/B.cs(9,3): error CS1002: ; expected\n";
        let report = CompileReport {
            diagnostics: parse(out, &root()),
            exit_code: Some(1),
            failure: None,
        };
        assert_eq!(report.errors(), 1);
        assert_eq!(report.warnings(), 1);
        assert_eq!(report.summary(), "1 error, 1 warning.");
        assert!(!report.ok());
    }

    #[test]
    fn absolute_paths_are_made_workspace_relative() {
        // The compiler reports absolute paths; the editor opens workspace-relative ones. If
        // these don't match, clicking a diagnostic opens nothing.
        let d = parse_line(
            r"C:\game\Assets\Player.cs(4,2): error CS0103: nope",
            &root(),
        )
        .unwrap();
        assert_eq!(d.file, "Assets/Player.cs", "relative, forward slashes");
    }

    #[test]
    fn rustc_short_format_parses_too() {
        // The seam is meant to generalise beyond Unity; this is the proof.
        let d = parse_line(
            "src/main.rs:12:7: error[E0425]: cannot find value `x` in this scope",
            &root(),
        )
        .unwrap();
        assert_eq!(d.file, "src/main.rs");
        assert_eq!((d.line, d.col), (12, 7));
        assert_eq!(d.code.as_deref(), Some("E0425"));
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.message, "cannot find value `x` in this scope");
    }

    #[test]
    fn a_windows_drive_letter_does_not_derail_the_rust_split() {
        // `C:` looks like the start of `file:line:col` but `\src\main.rs` is not a line number.
        // The scan must skip that candidate rather than giving up — this is a Windows-first
        // client, so absolute paths with drive letters are the norm, not an edge case.
        let d = parse_line(
            "C:/game/src/main.rs:12:7: error[E0425]: cannot find value `x`",
            &root(),
        )
        .expect("must parse past the drive letter");
        assert_eq!(d.file, "src/main.rs");
        assert_eq!((d.line, d.col), (12, 7));
    }

    #[test]
    fn ordinary_log_noise_is_not_mistaken_for_a_diagnostic() {
        // Unity logs thousands of lines. Anything that isn't a real diagnostic must be dropped,
        // or the panel fills with garbage and the real errors are lost in it.
        for noise in [
            "Compilation succeeded",
            "Refreshing native plugins compatible for Editor",
            "[Licensing::Client] Handshaking with LicensingClient",
            "- Completed reload, in  0.842 seconds",
            "",
            "Mono: successfully reloaded assembly",
        ] {
            assert_eq!(parse_line(noise, &root()), None, "parsed noise: {noise:?}");
        }
    }

    #[test]
    fn unity_repeating_an_error_per_assembly_pass_shows_once() {
        // Unity echoes the same error on each compilation pass. Four copies of one problem reads
        // as four problems.
        let repeated = "Assets/A.cs(3,5): error CS1002: ; expected\n".repeat(4);
        assert_eq!(parse(&repeated, &root()).len(), 1);
    }

    #[test]
    fn a_zero_exit_with_errors_logged_is_still_a_failure() {
        // Unity can exit 0 having logged compiler errors. Trusting the exit code alone would
        // report a broken project as green — the single worst bug this feature could have.
        let report = CompileReport {
            diagnostics: parse("Assets/A.cs(1,1): error CS1002: ; expected", &root()),
            exit_code: Some(0),
            failure: None,
        };
        assert!(!report.ok(), "errors beat a zero exit code");
        assert_eq!(report.summary(), "1 error.");

        // And the clean case really is clean.
        let good = CompileReport {
            diagnostics: Vec::new(),
            exit_code: Some(0),
            failure: None,
        };
        assert!(good.ok());
        assert_eq!(good.summary(), "No problems.");
    }

    #[test]
    fn a_run_that_never_started_is_reported_as_such() {
        // "Couldn't launch the compiler" and "your code is broken" are different problems with
        // different fixes; conflating them sends the user hunting through their own source.
        let report = CompileReport {
            diagnostics: Vec::new(),
            exit_code: None,
            failure: Some("Unity 2022.3.10f1 was not found.".to_string()),
        };
        assert!(!report.ok());
        assert_eq!(report.summary(), "Unity 2022.3.10f1 was not found.");
    }
}
