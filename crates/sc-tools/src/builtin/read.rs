//! The read-only navigation tools: `read_file`, `read_function`, `list_dir`, `search_code`.
//!
//! All of them are shaped by one constraint — a small context window. Reads are
//! capped and windowed, search hits are capped, and every truncation tells the
//! model exactly how to ask for the next chunk.

use std::path::Path;

use super::util::safe_join;

/// Default line cap when no explicit `limit` is given, so reading a large file can't flood the
/// context window (or the MCP status tail). A model that needs more asks for a specific window.
pub(super) const READ_FILE_DEFAULT_LINES: usize = 400;

/// Read a file, optionally windowed to `[start, start+limit)` (1-based lines) — the
/// grep-then-read-a-chunk pattern. With no window it reads from the top, capped at
/// [`READ_FILE_DEFAULT_LINES`]; a truncation note tells the model how to see more.
pub fn read_file(workspace: &Path, path: &str, start: Option<i64>, limit: Option<i64>) -> String {
    let p = match safe_join(workspace, path) {
        Ok(p) => p,
        Err(e) => return format!("read_file {path} rejected: {e}"),
    };
    let content = match std::fs::read_to_string(&p) {
        Ok(c) => c,
        Err(e) => return format!("read_file {path} error: {e}"),
    };
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    // 1-based start; clamp to the file. `start=0`/absent → 1.
    let start_1 = start.filter(|&s| s > 0).map(|s| s as usize).unwrap_or(1);
    if start_1 > total {
        return format!(
            "read_file {path}: start line {start_1} is past end of file ({total} lines)"
        );
    }
    let count = limit
        .filter(|&l| l > 0)
        .map(|l| l as usize)
        .unwrap_or(READ_FILE_DEFAULT_LINES);
    let end = (start_1 - 1 + count).min(total); // exclusive, 0-based
    let body = lines[start_1 - 1..end].join("\n");
    // Label the window and, when it doesn't reach the end, tell the model exactly how to continue.
    if start_1 == 1 && end == total {
        format!("read_file {path} ({total} lines):\n{body}")
    } else {
        let more = if end < total {
            format!(
                "\n… {} more line(s). Read the next chunk with \
                 {{\"tool\":\"read_file\",\"path\":\"{path}\",\"start\":{},\"limit\":{count}}}.",
                total - end,
                end + 1,
            )
        } else {
            String::new()
        };
        format!("read_file {path} (lines {start_1}-{end} of {total}):\n{body}{more}")
    }
}

/// A function longer than this is "giant" — [`read_function`] still shows it but nudges the
/// model to make a targeted `edit_lines` change rather than rewriting the whole thing, and
/// [`edit_function`] warns that a full-rewrite of a function this size is error-prone.
///
/// Re-exported from `sc_index` rather than defined twice: the health report (spec 23)
/// flags the same functions, and a tool that advised "this is large" about a different
/// set of functions than the report lists would be two sources of truth for one word.
///
/// [`edit_function`]: super::write::edit_function
const GIANT_FN_LINES: usize = sc_index::GIANT_FN_LINES;

/// Resolve `(language, source, (start,end))` for the function `name` in `path`, or an error
/// string. Shared by [`read_function`] and [`edit_function`].
///
/// [`edit_function`]: super::write::edit_function
pub(super) fn locate_function(
    workspace: &Path,
    path: &str,
    name: &str,
) -> std::result::Result<(String, usize, usize, usize), String> {
    let p = safe_join(workspace, path).map_err(|e| format!("{path} rejected: {e}"))?;
    let Some(lang) = sc_index::Language::from_path(path) else {
        return Err(format!(
            "{path}: function tools support Rust/Python/C# only. Use read_file/edit_lines here."
        ));
    };
    let src = std::fs::read_to_string(&p).map_err(|e| format!("{path} error: {e}"))?;
    let src = src.replace("\r\n", "\n").replace('\r', "\n");
    let Some((start, end)) = sc_index::function_span(lang, &src, name) else {
        return Err(format!(
            "{path}: no function named `{name}` found. Check the name (or use search_code / \
             read_file to locate it)."
        ));
    };
    let count = sc_index::count_functions_named(lang, &src, name);
    Ok((src, start, end, count))
}

/// Read one function/method by name — its whole body, line-numbered. The model gets exactly the
/// function it asked for instead of paging through a large file.
pub fn read_function(workspace: &Path, path: &str, name: &str) -> String {
    let (src, start, end, count) = match locate_function(workspace, path, name) {
        Ok(v) => v,
        Err(e) => return format!("read_function {e}"),
    };
    let lines: Vec<&str> = src.lines().collect();
    let body: String = lines[start - 1..end]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>5}  {l}", start + i))
        .collect::<Vec<_>>()
        .join("\n");
    let span_len = end - start + 1;
    let mut note = String::new();
    if count > 1 {
        note.push_str(&format!(
            "\n(note: {count} functions named `{name}` — this is the FIRST; edit_function edits \
             this one.)"
        ));
    }
    if span_len > GIANT_FN_LINES {
        note.push_str(&format!(
            "\n(this function is large — {span_len} lines. For a small change, prefer edit_lines \
             on the specific lines above rather than rewriting the whole function.)"
        ));
    }
    format!("read_function {path}:{name} (lines {start}-{end}):\n{body}{note}")
}

pub fn list_dir(workspace: &Path, path: &str) -> String {
    let joined = match safe_join(workspace, path) {
        Ok(p) => p,
        Err(e) => return format!("list_dir {path} rejected: {e}"),
    };
    let mut entries = match std::fs::read_dir(&joined) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect::<Vec<_>>(),
        Err(e) => return format!("list_dir {path} error: {e}"),
    };
    entries.sort();
    if entries.is_empty() {
        format!("list_dir {path}: (empty)")
    } else {
        format!("list_dir {path}:\n{}", entries.join("\n"))
    }
}

/// A line matcher for [`search_code`]: a compiled regex when the query is valid regex, else a
/// literal-substring fallback (so a plain string with no metachars, or an invalid pattern, still
/// searches sensibly). Kept private to this module.
enum Matcher {
    Regex(regex::Regex),
    Literal(String),
}

impl Matcher {
    fn new(query: &str) -> Self {
        match regex::Regex::new(query) {
            Ok(re) => Matcher::Regex(re),
            Err(_) => Matcher::Literal(query.to_string()),
        }
    }
    fn is_match(&self, line: &str) -> bool {
        match self {
            Matcher::Regex(re) => re.is_match(line),
            Matcher::Literal(q) => line.contains(q.as_str()),
        }
    }
}

/// Whether a query is asking for a **pattern** rather than describing a problem.
///
/// The model reaches for regex naturally (`match.*ShipRole`, `fn \w+`, `Foo::`), and
/// those queries want the literal grep: they are precise by construction, and ranking
/// them by relevance would be answering a question nobody asked. A question in
/// English -- "why is the trail thin before it gets thick" -- has no metacharacters
/// and wants the index.
///
/// The test is the presence of regex syntax, not whether the string happens to
/// compile: almost any prose compiles as a regex that matches itself literally, so
/// "does it parse" would send every question down the grep path and change nothing.
fn looks_like_a_pattern(query: &str) -> bool {
    const META: &[char] = &[
        '\\', '[', ']', '(', ')', '{', '}', '*', '+', '?', '|', '^', '$',
    ];
    if query.chars().any(|c| META.contains(&c)) {
        return true;
    }
    // `::`, a dot or an underscore is how code is spelled, not how questions are
    // asked -- but only when the query is short enough to be a name rather than a
    // sentence.
    let words = query.split_whitespace().count();
    words <= 3 && (query.contains("::") || query.contains('.') || query.contains('_'))
}

/// Search the workspace for `query`.
///
/// Two paths behind one name (spec 23). A query that **looks like a pattern** gets
/// the literal regex grep it is asking for. A query that reads like a **question**
/// gets indexed search: identifier-split, comment-weighted, ranked, and answered with
/// functions rather than lines.
///
/// The tool's name, description and one-parameter schema are unchanged, deliberately.
/// The six-tool menu is one of the few things in this project with a measurement
/// behind it (12/12 versus 3/12), so this spends its improvement *behind* the name the
/// model already knows: it asks the same vague question it always asked, and the
/// answers get better.
pub fn search_code(workspace: &Path, query: &str) -> String {
    if query.trim().is_empty() {
        return "search_code: empty query".to_string();
    }
    if looks_like_a_pattern(query) {
        return grep(workspace, query);
    }
    let index = sc_index::RepoIndex::open(workspace);
    let hits = sc_index::search(&index, query);
    if hits.is_empty() {
        // The index found nothing, but a literal match might still exist -- a rare
        // word the tokenizer dropped, or text in a file no grammar parses. Falling
        // back costs one walk and never returns worse than "no matches".
        return grep(workspace, query);
    }
    sc_index::render(query, &hits)
}

/// The original flat grep: every line matching a regex (or a literal, when the
/// pattern will not compile), capped and sorted.
fn grep(workspace: &Path, query: &str) -> String {
    const MAX_HITS: usize = 50;
    let matcher = Matcher::new(query);
    let mut hits = Vec::new();
    for file in sc_index::walk(workspace, &sc_index::WalkOptions::default()) {
        let Ok(content) = std::fs::read_to_string(&file.abs) else {
            continue;
        };
        for (i, line) in content.lines().enumerate() {
            if matcher.is_match(line) {
                hits.push(format!("{}:{}: {}", file.rel, i + 1, line.trim()));
                if hits.len() >= MAX_HITS {
                    hits.sort();
                    return format!(
                        "search_code {query:?}: {MAX_HITS}+ hits (truncated):\n{}",
                        hits.join("\n")
                    );
                }
            }
        }
    }
    if hits.is_empty() {
        format!("search_code {query:?}: no matches")
    } else {
        hits.sort();
        format!(
            "search_code {query:?}: {} hit(s):\n{}",
            hits.len(),
            hits.join("\n")
        )
    }
}

#[cfg(test)]
mod search_routing {
    use super::*;

    fn temp_repo(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sc-tools-search-{tag}-{}-{}",
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

    #[test]
    fn a_pattern_goes_to_grep_and_a_question_goes_to_the_index() {
        assert!(looks_like_a_pattern("match.*ShipRole"));
        assert!(looks_like_a_pattern("fn \\w+"));
        assert!(looks_like_a_pattern("ShipRole::"));
        assert!(looks_like_a_pattern("draw_trails"));
        assert!(!looks_like_a_pattern(
            "why is the trail behind the stars thin before it gets thick"
        ));
        assert!(!looks_like_a_pattern("where is the hull bar drawn"));
    }

    /// A precise query keeps its precision: the grep path still returns raw lines.
    #[test]
    fn a_regex_query_still_returns_matching_lines() {
        let root = temp_repo("regex");
        write(&root, "a.rs", "fn alpha() {}\nfn beta() {}\n");
        let out = search_code(&root, "fn (alpha|beta)");
        assert!(out.contains("a.rs:1: fn alpha() {}"), "{out}");
        assert!(out.contains("a.rs:2: fn beta() {}"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The point of the whole spec.** A question whose words appear only in a
    /// comment used to return "no matches"; now it returns the function.
    #[test]
    fn a_vague_question_finds_the_function_the_old_grep_could_not() {
        let root = temp_repo("vague");
        write(
            &root,
            "src/fx.rs",
            "// the trail is thin at the head and thick at the tail\npub fn draw_trails() {}\n",
        );
        write(&root, "src/other.rs", "pub fn unrelated() {}\n");

        let out = search_code(&root, "why is the trail thin before it gets thick");
        assert!(out.contains("src/fx.rs"), "{out}");
        assert!(out.contains("draw_trails"), "{out}");
        // Indexed results name the matched terms and never quote source lines.
        assert!(out.contains("matched:"), "{out}");
        assert!(!out.contains("pub fn draw_trails() {}"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_query_is_still_rejected() {
        let root = temp_repo("empty");
        write(&root, "a.rs", "fn a() {}\n");
        assert_eq!(search_code(&root, ""), "search_code: empty query");
        assert_eq!(search_code(&root, "   "), "search_code: empty query");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A question the index cannot answer falls back to grep rather than dead-ending.
    #[test]
    fn a_question_with_no_indexed_match_falls_back_to_grep() {
        let root = temp_repo("fallback");
        write(&root, "a.rs", "fn a() {}\n");
        let out = search_code(&root, "nothing here matches this question at all");
        assert!(out.contains("no matches"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The index is a cache, and a tool call must never leave one where a user's
    /// `git status` would trip over it -- except under `.smart-coder`, which every
    /// walk already skips and `.gitignore` already covers.
    #[test]
    fn searching_writes_only_under_the_hidden_cache_dir() {
        let root = temp_repo("cachedir");
        write(&root, "a.rs", "// widget\npub fn alpha() {}\n");
        search_code(&root, "where is the widget");
        let stray: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "a.rs" && n != ".smart-coder")
            .collect();
        assert!(stray.is_empty(), "unexpected files: {stray:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
