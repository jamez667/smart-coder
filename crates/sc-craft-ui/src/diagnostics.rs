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
    /// Neither — a plugin's hint, note or suggestion (spec 29).
    ///
    /// **Never produced by [`parse`]**: no supported toolchain emits it, and a compiler
    /// that did would be saying something about the build rather than about the code.
    /// It exists because the wire protocol has carried three severities since v1 while
    /// this type had two, and the missing one had to land somewhere. Counting a hint as
    /// a warning would make `summary()` report a problem the plugin did not claim.
    Info,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

/// Who produced a set of diagnostics.
///
/// `Ord` matters: it is the display order in the Problems panel, and [`Compile`] sorts
/// first because the compiler is the authority on whether the code builds. Plugins sort
/// after, among themselves by id, so the order does not change when a plugin restarts.
///
/// [`Compile`]: DiagnosticSource::Compile
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticSource {
    /// The project's own toolchain, via [`parse`].
    Compile,
    /// A plugin, by its manifest id (spec 29).
    Plugin(String),
}

impl DiagnosticSource {
    /// What the panel calls this source in a group header.
    pub fn label(&self) -> &str {
        match self {
            DiagnosticSource::Compile => "compile",
            DiagnosticSource::Plugin(id) => id,
        }
    }
}

/// Diagnostics kept per source. Beyond this the newest are dropped and the panel says so.
///
/// A plugin can push arbitrarily many, and the Problems panel is a widget per row — the
/// same hazard `Content::element_count` exists for, and the same answer. A plugin that
/// trips this has a bug, and hiding that makes it harder to find.
pub const MAX_DIAGNOSTICS_PER_SOURCE: usize = 1_000;

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
///
/// **The diagnostics themselves are not here.** They live in [`Diagnostics`] under
/// [`DiagnosticSource::Compile`], because the panel has more than one producer since
/// spec 29 and a single `Vec` on this struct made the last writer win. What remains is
/// what only a compile has: the exit code and the reason a run failed to happen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompileReport {
    /// The toolchain's exit code. `None` if it never ran or was cancelled.
    pub exit_code: Option<i32>,
    /// A reason the run itself failed (couldn't spawn, project locked) — distinct from the code
    /// failing to compile. Confusing the two sends the user hunting for a bug in their code.
    pub failure: Option<String>,
}

impl CompileReport {
    /// Whether the compile succeeded, given the diagnostics it produced.
    ///
    /// Requires BOTH a zero exit and no errors. Unity in particular can exit zero while having
    /// logged compiler errors, so trusting the exit code alone would report a broken project as
    /// green — the worst possible failure for this feature.
    ///
    /// Takes the diagnostics rather than owning them: they are the store's now, and a
    /// copy kept here to answer this question would be the second producer's slot all
    /// over again.
    pub fn ok(&self, diagnostics: &[Diagnostic]) -> bool {
        self.failure.is_none()
            && self.exit_code == Some(0)
            && count(diagnostics, Severity::Error) == 0
    }

    /// One line for the panel header, describing `diagnostics` — the compile's own.
    pub fn summary(&self, diagnostics: &[Diagnostic]) -> String {
        if let Some(f) = &self.failure {
            return f.clone();
        }
        let (e, w) = (
            count(diagnostics, Severity::Error),
            count(diagnostics, Severity::Warning),
        );
        match (e, w) {
            (0, 0) if self.ok(diagnostics) => "No problems.".to_string(),
            (0, 0) => {
                "Finished with no diagnostics, but the compiler reported failure.".to_string()
            }
            (0, w) => format!("{w} warning{}.", plural(w)),
            (e, 0) => format!("{e} error{}.", plural(e)),
            (e, w) => format!("{e} error{}, {w} warning{}.", plural(e), plural(w)),
        }
    }
}

/// How many of `diagnostics` have severity `s`.
pub fn count(diagnostics: &[Diagnostic], s: Severity) -> usize {
    diagnostics.iter().filter(|d| d.severity == s).count()
}

/// Every source's diagnostics, keyed by who produced them (spec 29).
///
/// **The Problems panel had one producer and now has several.** A single `Vec` meant a
/// plugin publishing would erase `cargo`'s diagnostics and the next compile would erase
/// the plugin's — whoever wrote last would win, and the panel would silently show a
/// fraction of what is wrong with the code. Keying by source is what makes the panel
/// additive instead.
///
/// `BTreeMap` rather than `HashMap` because the key order **is** the display order:
/// [`DiagnosticSource::Compile`] first, then plugins by id, stable across restarts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnostics {
    by_source: std::collections::BTreeMap<DiagnosticSource, Vec<Diagnostic>>,
    /// Sources that pushed more than [`MAX_DIAGNOSTICS_PER_SOURCE`] and were cut short.
    truncated: std::collections::BTreeSet<DiagnosticSource>,
}

impl Diagnostics {
    /// Replace everything `source` said **about `file`**, wholesale.
    ///
    /// Never a merge: a plugin that publishes on every change while appending produces a
    /// growing pile of stale problems. An empty `diagnostics` is how a source says "this
    /// file is clean now", which is why there is no delete message and does not need to
    /// be one — the same rule LSP settled on, for the same reason. A plugin that crashes
    /// mid-update leaves a stale set, not a corrupt one.
    pub fn publish(&mut self, source: DiagnosticSource, file: &str, diagnostics: Vec<Diagnostic>) {
        let entry = self.by_source.entry(source.clone()).or_default();
        entry.retain(|d| d.file != file);
        entry.extend(diagnostics);

        // Bound per source, not per file: the cap protects the UI thread, and a plugin
        // that spread 200,000 rows over a thousand files would slip a per-file cap while
        // freezing the editor just the same.
        let over = entry.len() > MAX_DIAGNOSTICS_PER_SOURCE;
        if over {
            entry.truncate(MAX_DIAGNOSTICS_PER_SOURCE);
        }
        // Nothing left from this source — drop the key so it stops appearing at all.
        let now_empty = entry.is_empty();

        if over {
            self.truncated.insert(source);
        } else {
            self.truncated.remove(&source);
            if now_empty {
                self.by_source.remove(&source);
            }
        }
    }

    /// Replace **everything** `source` has said, across every file.
    ///
    /// What a compile does: a fresh run supersedes the previous one entirely, including
    /// for files it no longer mentions.
    pub fn replace_all(&mut self, source: DiagnosticSource, mut diagnostics: Vec<Diagnostic>) {
        if diagnostics.len() > MAX_DIAGNOSTICS_PER_SOURCE {
            diagnostics.truncate(MAX_DIAGNOSTICS_PER_SOURCE);
            self.truncated.insert(source.clone());
        } else {
            self.truncated.remove(&source);
        }
        if diagnostics.is_empty() {
            self.by_source.remove(&source);
        } else {
            self.by_source.insert(source, diagnostics);
        }
    }

    /// Forget `source` entirely.
    ///
    /// **A stopped plugin's diagnostics are dropped**, unlike its panel content. These are
    /// opposite cases: panel content is evidence of what the plugin was doing when it
    /// died, whereas a diagnostic is a claim about the current state of a file that
    /// nothing is left to retract. A dead plugin's errors pointing at lines the user has
    /// since fixed is the same stale-diagnostics failure the compile path already guards
    /// against on a project switch.
    pub fn clear(&mut self, source: &DiagnosticSource) {
        self.by_source.remove(source);
        self.truncated.remove(source);
    }

    /// Drop everything, from every source — a new workspace.
    pub fn clear_all(&mut self) {
        self.by_source.clear();
        self.truncated.clear();
    }

    /// What `source` currently says.
    pub fn get(&self, source: &DiagnosticSource) -> &[Diagnostic] {
        self.by_source.get(source).map_or(&[], Vec::as_slice)
    }

    /// Whether `source` had diagnostics dropped at [`MAX_DIAGNOSTICS_PER_SOURCE`].
    pub fn is_truncated(&self, source: &DiagnosticSource) -> bool {
        self.truncated.contains(source)
    }

    /// Every source with diagnostics, in display order.
    pub fn sources(&self) -> impl Iterator<Item = (&DiagnosticSource, &Vec<Diagnostic>)> {
        self.by_source.iter()
    }

    /// Every diagnostic, in display order, paired with the source that produced it.
    ///
    /// **This is what the panel renders and what a click indexes into**, so the two agree
    /// by construction. A view that flattened separately from the handler would open the
    /// wrong file the moment a plugin published.
    pub fn flattened(&self) -> Vec<(&DiagnosticSource, &Diagnostic)> {
        self.by_source
            .iter()
            .flat_map(|(s, ds)| ds.iter().map(move |d| (s, d)))
            .collect()
    }

    /// Whether nothing anywhere has anything to say.
    pub fn is_empty(&self) -> bool {
        self.by_source.values().all(Vec::is_empty)
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
        let ds = parse(out, &root());
        let report = CompileReport {
            exit_code: Some(1),
            failure: None,
        };
        assert_eq!(count(&ds, Severity::Error), 1);
        assert_eq!(count(&ds, Severity::Warning), 1);
        assert_eq!(report.summary(&ds), "1 error, 1 warning.");
        assert!(!report.ok(&ds));
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
        let ds = parse("Assets/A.cs(1,1): error CS1002: ; expected", &root());
        let report = CompileReport {
            exit_code: Some(0),
            failure: None,
        };
        assert!(!report.ok(&ds), "errors beat a zero exit code");
        assert_eq!(report.summary(&ds), "1 error.");

        // And the clean case really is clean.
        let good = CompileReport {
            exit_code: Some(0),
            failure: None,
        };
        assert!(good.ok(&[]));
        assert_eq!(good.summary(&[]), "No problems.");
    }

    #[test]
    fn a_run_that_never_started_is_reported_as_such() {
        // "Couldn't launch the compiler" and "your code is broken" are different problems with
        // different fixes; conflating them sends the user hunting through their own source.
        let report = CompileReport {
            exit_code: None,
            failure: Some("Unity 2022.3.10f1 was not found.".to_string()),
        };
        assert!(!report.ok(&[]));
        assert_eq!(report.summary(&[]), "Unity 2022.3.10f1 was not found.");
    }

    /// Build a diagnostic in `file` at `line`, for the store tests.
    fn diag(file: &str, line: usize, severity: Severity) -> Diagnostic {
        Diagnostic {
            file: file.to_string(),
            line,
            col: 1,
            severity,
            code: None,
            message: format!("problem at {line}"),
        }
    }

    #[test]
    fn a_plugin_publishing_does_not_erase_the_compiler() {
        // THE bug spec 29 exists to fix. The panel was a single-producer slot: whoever
        // wrote last won, and the user saw a fraction of what was wrong with the code.
        let mut d = Diagnostics::default();
        d.replace_all(
            DiagnosticSource::Compile,
            vec![diag("a.rs", 1, Severity::Error)],
        );
        d.publish(
            DiagnosticSource::Plugin("shaders".into()),
            "b.wgsl",
            vec![diag("b.wgsl", 9, Severity::Error)],
        );

        assert_eq!(
            d.get(&DiagnosticSource::Compile).len(),
            1,
            "compiler survived"
        );
        assert_eq!(d.flattened().len(), 2, "both are shown");
    }

    #[test]
    fn the_compiler_sorts_before_plugins_and_plugins_sort_by_id() {
        // The display order is the key order, and it must not move when a plugin
        // restarts — a list that reshuffles under the cursor is unclickable.
        let mut d = Diagnostics::default();
        d.publish(
            DiagnosticSource::Plugin("zebra".into()),
            "z.rs",
            vec![diag("z.rs", 1, Severity::Error)],
        );
        d.publish(
            DiagnosticSource::Plugin("alpha".into()),
            "a.rs",
            vec![diag("a.rs", 1, Severity::Error)],
        );
        d.replace_all(
            DiagnosticSource::Compile,
            vec![diag("c.rs", 1, Severity::Error)],
        );

        let order: Vec<String> = d.sources().map(|(s, _)| s.label().to_string()).collect();
        assert_eq!(order, vec!["compile", "alpha", "zebra"]);
    }

    #[test]
    fn publishing_replaces_that_file_and_leaves_the_others_alone() {
        // Replace, not append — and scoped to the file named, or a plugin reporting on
        // one file would silently retract what it said about every other.
        let mut d = Diagnostics::default();
        let src = DiagnosticSource::Plugin("shaders".into());
        d.publish(
            src.clone(),
            "a.wgsl",
            vec![diag("a.wgsl", 1, Severity::Error)],
        );
        d.publish(
            src.clone(),
            "b.wgsl",
            vec![diag("b.wgsl", 2, Severity::Error)],
        );

        // Re-publishing a.wgsl replaces only a.wgsl.
        d.publish(
            src.clone(),
            "a.wgsl",
            vec![diag("a.wgsl", 7, Severity::Warning)],
        );

        let lines: Vec<usize> = d.get(&src).iter().map(|x| x.line).collect();
        assert_eq!(lines.len(), 2, "one per file, not three");
        assert!(lines.contains(&7), "a.wgsl was replaced");
        assert!(lines.contains(&2), "b.wgsl untouched");
    }

    #[test]
    fn an_empty_publish_is_how_a_file_is_declared_clean() {
        // There is no delete message and there does not need to be one: this is the rule
        // LSP settled on, and it means a plugin that crashes mid-update leaves a stale
        // set rather than a corrupt one.
        let mut d = Diagnostics::default();
        let src = DiagnosticSource::Plugin("shaders".into());
        d.publish(
            src.clone(),
            "a.wgsl",
            vec![diag("a.wgsl", 1, Severity::Error)],
        );
        d.publish(src.clone(), "a.wgsl", Vec::new());

        assert!(d.get(&src).is_empty(), "the file is clean now");
        assert!(d.is_empty(), "and the source stops appearing at all");
    }

    #[test]
    fn a_stopped_plugins_diagnostics_are_dropped_but_the_compilers_are_not() {
        // Panel content survives a plugin's death as evidence; a diagnostic does not,
        // because it is a claim about the CURRENT state of a file and nothing is left to
        // retract it.
        let mut d = Diagnostics::default();
        let src = DiagnosticSource::Plugin("shaders".into());
        d.replace_all(
            DiagnosticSource::Compile,
            vec![diag("a.rs", 1, Severity::Error)],
        );
        d.publish(
            src.clone(),
            "b.wgsl",
            vec![diag("b.wgsl", 1, Severity::Error)],
        );

        d.clear(&src);

        assert!(d.get(&src).is_empty(), "the dead plugin's claims are gone");
        assert_eq!(
            d.get(&DiagnosticSource::Compile).len(),
            1,
            "the compiler's are not"
        );
    }

    #[test]
    fn a_runaway_source_is_truncated_and_says_so() {
        // The Problems panel is a widget per row. Truncation is reported rather than
        // silent: a plugin that trips this has a bug, and hiding it makes it harder to find.
        let mut d = Diagnostics::default();
        let src = DiagnosticSource::Plugin("runaway".into());
        let flood: Vec<Diagnostic> = (1..=MAX_DIAGNOSTICS_PER_SOURCE + 500)
            .map(|i| diag("a.rs", i, Severity::Error))
            .collect();
        d.publish(src.clone(), "a.rs", flood);

        assert_eq!(d.get(&src).len(), MAX_DIAGNOSTICS_PER_SOURCE, "bounded");
        assert!(d.is_truncated(&src), "and the panel is told");
    }

    #[test]
    fn the_cap_drops_the_newest_and_keeps_what_other_files_already_said() {
        // The cap is per SOURCE, so a flood about one file could in principle evict
        // another file's problems from the same plugin. It does not: `retain` leaves the
        // other files in place and the flood is appended after them, so truncation eats
        // the newest — which is the spec's rule and also the safe one.
        let mut d = Diagnostics::default();
        let src = DiagnosticSource::Plugin("shaders".into());
        d.publish(
            src.clone(),
            "quiet.wgsl",
            vec![diag("quiet.wgsl", 1, Severity::Error)],
        );

        let flood: Vec<Diagnostic> = (1..=MAX_DIAGNOSTICS_PER_SOURCE + 50)
            .map(|i| diag("noisy.wgsl", i, Severity::Error))
            .collect();
        d.publish(src.clone(), "noisy.wgsl", flood);

        let kept = d.get(&src);
        assert_eq!(kept.len(), MAX_DIAGNOSTICS_PER_SOURCE);
        assert!(
            kept.iter().any(|x| x.file == "quiet.wgsl"),
            "the quiet file's problem was not evicted by the noisy one"
        );
        assert!(d.is_truncated(&src));
    }

    #[test]
    fn a_source_that_comes_back_under_the_cap_stops_being_truncated() {
        // The note must not outlive the flood that caused it.
        let mut d = Diagnostics::default();
        let src = DiagnosticSource::Plugin("runaway".into());
        let flood: Vec<Diagnostic> = (1..=MAX_DIAGNOSTICS_PER_SOURCE + 1)
            .map(|i| diag("a.rs", i, Severity::Error))
            .collect();
        d.publish(src.clone(), "a.rs", flood);
        assert!(d.is_truncated(&src));

        d.publish(src.clone(), "a.rs", vec![diag("a.rs", 1, Severity::Error)]);
        assert!(!d.is_truncated(&src), "recovered");
    }

    #[test]
    fn a_plugins_info_is_not_counted_as_a_warning() {
        // The wire has carried three severities since v1 and this type had two. Folding
        // Info into Warning would make the summary report a problem nobody claimed.
        let ds = vec![diag("a.rs", 1, Severity::Info)];
        assert_eq!(count(&ds, Severity::Warning), 0);
        assert_eq!(count(&ds, Severity::Error), 0);
        assert_eq!(Severity::Info.label(), "info");
    }

    #[test]
    fn flattening_walks_the_groups_in_the_same_order_the_panel_renders_them() {
        // The panel renders `sources()` group by group, counting rows as it goes, and a
        // click indexes `flattened()`. These are two different traversals of the same map
        // and they must agree element for element — if they ever diverge, every click
        // below the first group opens the wrong file.
        let mut d = Diagnostics::default();
        d.replace_all(
            DiagnosticSource::Compile,
            vec![
                diag("c.rs", 1, Severity::Error),
                diag("c.rs", 2, Severity::Warning),
            ],
        );
        d.publish(
            DiagnosticSource::Plugin("zebra".into()),
            "z.rs",
            vec![diag("z.rs", 3, Severity::Error)],
        );
        d.publish(
            DiagnosticSource::Plugin("alpha".into()),
            "a.rs",
            vec![diag("a.rs", 4, Severity::Info)],
        );

        // Exactly what the view's loop does.
        let rendered: Vec<(&DiagnosticSource, &Diagnostic)> = d
            .sources()
            .flat_map(|(s, ds)| ds.iter().map(move |x| (s, x)))
            .collect();

        assert_eq!(rendered, d.flattened(), "the view and the click must agree");
        assert_eq!(d.flattened().len(), 4);
    }

    #[test]
    fn flattening_is_what_a_click_indexes_into() {
        // The view renders `flattened()` and the click handler indexes it. If they were
        // derived separately, clicking a row would open a different file the moment a
        // plugin published.
        let mut d = Diagnostics::default();
        d.replace_all(
            DiagnosticSource::Compile,
            vec![diag("c.rs", 3, Severity::Error)],
        );
        d.publish(
            DiagnosticSource::Plugin("p".into()),
            "p.rs",
            vec![diag("p.rs", 9, Severity::Error)],
        );

        let flat = d.flattened();
        assert_eq!(flat[0].1.file, "c.rs", "compile first");
        assert_eq!(flat[1].1.file, "p.rs");
        assert_eq!(flat[1].0.label(), "p", "and it knows who said it");
    }
}
