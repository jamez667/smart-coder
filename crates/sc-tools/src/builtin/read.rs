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
/// [`edit_function`]: super::write::edit_function
const GIANT_FN_LINES: usize = 120;

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

/// A small search over the workspace's text files. Skips the usual noise dirs and
/// anything that isn't valid UTF-8. Caps hits so the result fits a small context window.
///
/// The directory policy is [`sc_index::walk`]'s (spec 23). It already excluded the
/// agent's own run logs under `.smart-coder/sessions/*` — which echo every prior tool
/// result, so searching them makes the agent match its own transcript (observed live:
/// a search for a function name hit the session log instead of the source, wasting
/// turns) — and the shared list keeps that, plus the dotdirs and build output the old
/// walk here was missing.
pub fn search_code(workspace: &Path, query: &str) -> String {
    const MAX_HITS: usize = 50;
    if query.is_empty() {
        return "search_code: empty query".to_string();
    }
    // Treat the query as a REGEX (the model naturally reaches for `match.*ShipRole`,
    // `fn \w+`, etc.). If it isn't valid regex, fall back to a literal substring so a plain
    // string like `ShipRole::` still works. A regex whose literal meaning differs (contains
    // regex metachars) is matched as regex; this is what makes "find the exhaustive matches"
    // actually work instead of returning "no matches" and looping.
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
