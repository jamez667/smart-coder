//! Stack-trace resolution (spec 23 — stack traces).
//!
//! When a question carries a panic or a traceback, the harness resolves it before any
//! model sees it. Frame parsing is exactly the kind of mechanical work a small model
//! fumbles and a parser does not: the paths are absolute and belong to whoever built
//! the binary, the frames are ordered innermost-first or outermost-first depending on
//! the language, and most of them are library code the reader does not care about.
//!
//! A model handed resolved frames should never be asked to `search_code` for a
//! backtrace. So this is not a tool — it runs before the prompt is built, and its
//! output is evidence in the task anchor.

use crate::store::RepoIndex;

/// One frame, resolved as far as the index allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The path exactly as the trace wrote it — absolute, foreign separators and all.
    pub raw_path: String,
    /// Workspace-relative path, when this frame is in the indexed repo.
    pub path: Option<String>,
    pub line: Option<usize>,
    /// The enclosing definition, when the frame is in-repo and the index knows it.
    pub symbol: Option<String>,
    /// The function name the trace itself reported, when it gave one.
    pub reported: Option<String>,
}

impl Frame {
    /// Whether this frame points into the workspace.
    pub fn in_workspace(&self) -> bool {
        self.path.is_some()
    }
}

/// Find and resolve a stack trace in `text`, innermost frame first.
///
/// Returns an empty vector when `text` contains no recognizable trace, which is the
/// common case: most questions are prose, and a resolver that guessed would put
/// noise at the top of every prompt.
pub fn resolve_trace(text: &str, index: &RepoIndex) -> Vec<Frame> {
    let mut frames = parse_frames(text);
    if frames.is_empty() {
        return frames;
    }
    for f in &mut frames {
        if let Some(rel) = match_path(index, &f.raw_path) {
            f.symbol = f
                .line
                .and_then(|l| index.enclosing_symbol(&rel, l))
                .map(|s| s.name.clone());
            f.path = Some(rel);
        }
    }
    frames
}

/// Render resolved frames the way the model sees them, innermost first.
pub fn render_trace(frames: &[Frame]) -> String {
    if frames.is_empty() {
        return String::new();
    }
    let mut out = String::from("stack trace (innermost first):\n");
    for (i, f) in frames.iter().enumerate() {
        match (&f.path, f.line) {
            (Some(p), Some(l)) => {
                let what = f
                    .symbol
                    .clone()
                    .or_else(|| f.reported.clone())
                    .unwrap_or_default();
                let in_what = if what.is_empty() {
                    String::new()
                } else {
                    format!("  in {what}")
                };
                out.push_str(&format!("#{i} {p}:{l}{in_what}   (workspace)\n"));
            }
            _ => {
                let what = f.reported.clone().unwrap_or_else(|| f.raw_path.clone());
                out.push_str(&format!("#{i} <external> {what}\n"));
            }
        }
    }
    // Say it plainly when nothing landed in the repo: a trace entirely inside the
    // standard library is a real answer ("this is not our code"), not a failure, and
    // the model must not go hunting for files that were never mentioned.
    if !frames.iter().any(Frame::in_workspace) {
        out.push_str("(no frames in this workspace — the fault is in external code)\n");
    }
    out.trim_end().to_string()
}

/// The workspace-relative path an indexed file has, matched by longest path suffix.
///
/// A trace records the path of whoever *built* the binary
/// (`/home/ci/work/crates/void_engine/src/fx/starfield.rs`, or a Windows path with
/// backslashes); the index records workspace-relative ones. Suffix matching on path
/// *segments* is what bridges them, and matching on segments rather than characters
/// is what stops `field.rs` matching `starfield.rs`.
fn match_path(index: &RepoIndex, raw: &str) -> Option<String> {
    let normalized = raw.replace('\\', "/");
    let want: Vec<&str> = normalized
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if want.is_empty() {
        return None;
    }
    let mut best: Option<(usize, String)> = None;
    for path in index.paths() {
        let have: Vec<&str> = path.split('/').collect();
        let mut n = 0usize;
        while n < have.len()
            && n < want.len()
            && have[have.len() - 1 - n].eq_ignore_ascii_case(want[want.len() - 1 - n])
        {
            n += 1;
        }
        if n == 0 {
            continue;
        }
        // Longest suffix wins; ties break on the shorter path, then alphabetically,
        // so the result never depends on iteration order.
        let better = match &best {
            None => true,
            Some((bn, bp)) => n > *bn || (n == *bn && (path.len(), path) < (bp.len(), bp.as_str())),
        };
        if better {
            best = Some((n, path.to_string()));
        }
    }
    best.map(|(_, p)| p)
}

/// Extract frames from the three trace shapes the project actually meets.
///
/// Hand-rolled rather than regex-driven: the `regex` crate has no look-around, the
/// shapes differ more than a single pattern can express, and a parser that returns
/// nothing on unfamiliar input is exactly what is wanted here.
fn parse_frames(text: &str) -> Vec<Frame> {
    let rust = parse_rust(text);
    if !rust.is_empty() {
        return rust;
    }
    let py = parse_python(text);
    if !py.is_empty() {
        return py;
    }
    parse_dotnet(text)
}

/// Rust: a panic location line (`thread 'main' panicked at src/a.rs:10:5`) and/or a
/// `RUST_BACKTRACE` listing of `N: symbol` followed by `at path:line`.
///
/// Backtrace frames are already innermost-first, and the panic location *is* the
/// innermost frame, so it leads.
fn parse_rust(text: &str) -> Vec<Frame> {
    let lines: Vec<&str> = text.lines().collect();
    let mut frames = Vec::new();

    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        // `thread '...' panicked at <path>:<line>:<col>`
        if line.contains("panicked at") {
            if let Some(loc) = line.rsplit("panicked at ").next() {
                if let Some(f) = frame_from_location(loc.trim_end_matches(':')) {
                    frames.push(f);
                }
            }
            continue;
        }
        // A numbered backtrace entry: `  3: void_engine::fx::draw_trails`
        let numbered = line
            .split_once(':')
            .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if let Some((_, sym)) = numbered {
            let sym = sym.trim();
            // The `at <path>:<line>` that usually follows names the file.
            let at = lines
                .get(i + 1)
                .map(|l| l.trim())
                .filter(|l| l.starts_with("at "))
                .and_then(|l| frame_from_location(l.trim_start_matches("at ").trim()));
            match at {
                Some(mut f) => {
                    f.reported = Some(sym.to_string());
                    frames.push(f);
                }
                None if !sym.is_empty() => frames.push(Frame {
                    raw_path: sym.to_string(),
                    path: None,
                    line: None,
                    symbol: None,
                    reported: Some(sym.to_string()),
                }),
                None => {}
            }
        }
    }
    frames
}

/// Python: `File "<path>", line <n>, in <fn>`, outermost first — so the list is
/// reversed to match every other language's innermost-first convention.
fn parse_python(text: &str) -> Vec<Frame> {
    let mut frames = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if !line.starts_with("File \"") {
            continue;
        }
        let Some(rest) = line.strip_prefix("File \"") else {
            continue;
        };
        let Some((path, rest)) = rest.split_once('"') else {
            continue;
        };
        let number = rest
            .split_once("line ")
            .and_then(|(_, n)| n.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|n| n.parse::<usize>().ok());
        let reported = rest.split_once(" in ").map(|(_, f)| f.trim().to_string());
        frames.push(Frame {
            raw_path: path.to_string(),
            path: None,
            line: number,
            symbol: None,
            reported,
        });
    }
    frames.reverse();
    frames
}

/// .NET: `   at Namespace.Type.Method(args) in <path>:line <n>`. Already
/// innermost-first.
fn parse_dotnet(text: &str) -> Vec<Frame> {
    let mut frames = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        let Some(rest) = line.strip_prefix("at ") else {
            continue;
        };
        match rest.split_once(" in ") {
            Some((sym, loc)) => {
                let (path, number) = match loc.rsplit_once(":line ") {
                    Some((p, n)) => (p.to_string(), n.trim().parse::<usize>().ok()),
                    None => (loc.to_string(), None),
                };
                frames.push(Frame {
                    raw_path: path,
                    path: None,
                    line: number,
                    symbol: None,
                    reported: Some(sym.trim().to_string()),
                });
            }
            None => frames.push(Frame {
                raw_path: rest.to_string(),
                path: None,
                line: None,
                symbol: None,
                reported: Some(rest.to_string()),
            }),
        }
    }
    frames
}

/// `<path>:<line>` or `<path>:<line>:<col>` into a frame.
fn frame_from_location(loc: &str) -> Option<Frame> {
    let parts: Vec<&str> = loc.rsplitn(3, ':').collect();
    // rsplitn yields reversed: [col, line, path] or [line, path].
    let (path, number) = match parts.as_slice() {
        [col, number, path] if col.chars().all(|c| c.is_ascii_digit()) => {
            (path.to_string(), number.parse::<usize>().ok())
        }
        [number, path] => (path.to_string(), number.parse::<usize>().ok()),
        _ => return None,
    };
    if path.is_empty() || number.is_none() {
        return None;
    }
    Some(Frame {
        raw_path: path,
        path: None,
        line: number,
        symbol: None,
        reported: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn temp_repo(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sc-index-trace-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn fixture() -> PathBuf {
        let root = temp_repo("fx");
        write(
            &root,
            "src/fx/starfield.rs",
            "\
pub fn draw_stars() {
    let a = 1;
}

pub fn draw_trails() {
    let widths = compute();
    widths[9];
}
",
        );
        write(&root, "src/app.py", "def handler():\n    return boom()\n");
        write(
            &root,
            "src/Ship.cs",
            "class Ship {\n    void Dock() {\n        Fail();\n    }\n}\n",
        );
        root
    }

    #[test]
    fn resolves_a_rust_panic_location_to_its_function() {
        let root = fixture();
        let idx = RepoIndex::build(&root);
        let text = "thread 'main' panicked at src/fx/starfield.rs:7:5:\nindex out of bounds";
        let frames = resolve_trace(text, &idx);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].path.as_deref(), Some("src/fx/starfield.rs"));
        assert_eq!(frames[0].line, Some(7));
        assert_eq!(frames[0].symbol.as_deref(), Some("draw_trails"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A trace records the builder's absolute path, not the workspace's.** Suffix
    /// matching on path segments is the whole bridge, and it must work with either
    /// separator because the trace may come from a Linux CI box or a Windows one.
    #[test]
    fn matches_absolute_paths_with_either_separator() {
        let root = fixture();
        let idx = RepoIndex::build(&root);
        for raw in [
            "/home/ci/build/src/fx/starfield.rs:7:5",
            r"C:\Users\someone\proj\src\fx\starfield.rs:7:5",
            "./src/fx/starfield.rs:7",
        ] {
            let text = format!("thread 'main' panicked at {raw}:");
            let frames = resolve_trace(&text, &idx);
            assert_eq!(
                frames.first().and_then(|f| f.path.clone()),
                Some("src/fx/starfield.rs".to_string()),
                "failed for {raw}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolves_a_rust_backtrace_innermost_first() {
        let root = fixture();
        let idx = RepoIndex::build(&root);
        let text = "\
stack backtrace:
   0: core::panicking::panic_bounds_check
   1: game::fx::starfield::draw_trails
             at ./src/fx/starfield.rs:7
   2: game::main
             at ./src/main.rs:4
";
        let frames = resolve_trace(text, &idx);
        assert_eq!(frames.len(), 3);
        // Frame 0 is library code: no workspace path, but it is still reported.
        assert!(!frames[0].in_workspace());
        assert!(frames[0].reported.as_deref().unwrap().contains("panicking"));
        // Frame 1 is ours, and the index names the function.
        assert_eq!(frames[1].path.as_deref(), Some("src/fx/starfield.rs"));
        assert_eq!(frames[1].symbol.as_deref(), Some("draw_trails"));
        // Frame 2 names a file this workspace does not have.
        assert!(!frames[2].in_workspace());

        let out = render_trace(&frames);
        assert!(out.starts_with("stack trace (innermost first):"), "{out}");
        assert!(
            out.contains("#1 src/fx/starfield.rs:7  in draw_trails"),
            "{out}"
        );
        assert!(out.contains("<external>"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Python lists frames outermost-first. Reversing them is not cosmetic: the
    /// innermost frame is the one the reader wants, and putting it last buries it.
    #[test]
    fn a_python_traceback_is_reversed_to_innermost_first() {
        let root = fixture();
        let idx = RepoIndex::build(&root);
        let text = "\
Traceback (most recent call last):
  File \"/srv/app/main.py\", line 12, in <module>
    handler()
  File \"/srv/app/src/app.py\", line 2, in handler
    return boom()
NameError: name 'boom' is not defined
";
        let frames = resolve_trace(text, &idx);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].path.as_deref(), Some("src/app.py"));
        assert_eq!(frames[0].line, Some(2));
        assert_eq!(frames[0].reported.as_deref(), Some("handler"));
        assert_eq!(frames[0].symbol.as_deref(), Some("handler"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolves_a_dotnet_stack_trace() {
        let root = fixture();
        let idx = RepoIndex::build(&root);
        let text = "\
System.NullReferenceException: Object reference not set to an instance of an object.
   at Game.Ship.Dock() in C:\\build\\src\\Ship.cs:line 3
   at Game.Program.Main(String[] args) in C:\\build\\src\\Program.cs:line 9
";
        let frames = resolve_trace(text, &idx);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].path.as_deref(), Some("src/Ship.cs"));
        assert_eq!(frames[0].line, Some(3));
        assert_eq!(frames[0].symbol.as_deref(), Some("Dock"));
        assert!(!frames[1].in_workspace());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **Prose is not a trace.** A resolver that guessed would put noise at the top
    /// of every investigate prompt, which is the opposite of the point.
    #[test]
    fn ordinary_prose_yields_no_frames() {
        let root = fixture();
        let idx = RepoIndex::build(&root);
        for text in [
            "why is the trail behind the stars thin before it gets thick",
            "the ratio is 3:2 and the timeout is 30:00",
            "",
        ] {
            assert!(resolve_trace(text, &idx).is_empty(), "{text:?}");
        }
        assert!(render_trace(&[]).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A trace entirely inside the standard library is a real answer — "this is not
    /// our code" — and must be said, not silently rendered as a list of nothing.
    #[test]
    fn a_trace_with_no_workspace_frames_degrades_gracefully() {
        let root = fixture();
        let idx = RepoIndex::build(&root);
        let text = "thread 'main' panicked at /rustc/deadbeef/library/core/src/slice.rs:117:5:";
        let frames = resolve_trace(text, &idx);
        assert_eq!(frames.len(), 1);
        assert!(!frames[0].in_workspace());
        let out = render_trace(&frames);
        assert!(out.contains("no frames in this workspace"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Suffix matching is on path SEGMENTS, so a shorter filename that happens to end
    /// the same characters is not a match.
    #[test]
    fn suffix_matching_does_not_confuse_similar_filenames() {
        let root = temp_repo("similar");
        write(&root, "src/field.rs", "pub fn a() {}\n");
        write(&root, "src/starfield.rs", "pub fn b() {}\n");
        let idx = RepoIndex::build(&root);
        let frames = resolve_trace("thread 'main' panicked at /x/src/field.rs:1:1:", &idx);
        assert_eq!(frames[0].path.as_deref(), Some("src/field.rs"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolution_is_deterministic() {
        let root = fixture();
        let idx = RepoIndex::build(&root);
        let text = "thread 'main' panicked at src/fx/starfield.rs:7:5:";
        let a = render_trace(&resolve_trace(text, &idx));
        let b = render_trace(&resolve_trace(text, &idx));
        assert_eq!(a, b);
        let _ = std::fs::remove_dir_all(&root);
    }
}
