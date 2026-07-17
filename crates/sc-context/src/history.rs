//! Rolling history compaction (spec 05 — history compaction).
//!
//! Older turns are compressed into a short running summary — "decisions made,
//! files changed, what's verified" — rather than kept verbatim. Recent turns stay
//! verbatim; distant ones become summary.
//!
//! The summary here is **extractive and model-free**: it reads the harness's own
//! record of what each turn *did* (which tool, with what key argument, and whether
//! the observation was an error) and renders compact bullets. That keeps it
//! deterministic and cheap — a small model shouldn't spend a model call, or
//! trust another model's prose, to know what it already did.

/// One past turn, as the harness recorded it: the tool the model called, a short
/// argument label, and whether the resulting observation looked like a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRecord {
    pub tool: String,
    /// A short label for the key argument (e.g. a path or query). May be empty.
    pub arg: String,
    /// Whether the observation indicated an error the model had to react to.
    pub was_error: bool,
}

impl TurnRecord {
    pub fn new(tool: impl Into<String>, arg: impl Into<String>, was_error: bool) -> Self {
        Self {
            tool: tool.into(),
            arg: arg.into(),
            was_error,
        }
    }
}

/// Split a turn history into the older turns to compact and the recent turns to
/// keep verbatim. `keep_recent` most-recent turns are kept; the rest summarized.
pub fn split_for_compaction(
    turns: &[TurnRecord],
    keep_recent: usize,
) -> (&[TurnRecord], &[TurnRecord]) {
    if turns.len() <= keep_recent {
        return (&[], turns);
    }
    let cut = turns.len() - keep_recent;
    (&turns[..cut], &turns[cut..])
}

/// Build a compact, structured summary of older turns (spec 05 — structured state
/// instead of prose). Groups by action so repeated reads/searches collapse, and
/// always surfaces the count of errors hit. Returns an empty string for no turns.
pub fn summarize_history(older: &[TurnRecord]) -> String {
    if older.is_empty() {
        return String::new();
    }

    let mut wrote: Vec<&str> = Vec::new();
    let mut read: Vec<&str> = Vec::new();
    let mut searched: Vec<&str> = Vec::new();
    let mut other: Vec<&str> = Vec::new();
    let mut errors = 0usize;

    for t in older {
        if t.was_error {
            errors += 1;
        }
        match t.tool.as_str() {
            "write_file" => push_unique(&mut wrote, &t.arg),
            "read_file" => push_unique(&mut read, &t.arg),
            "search_code" | "find_symbol" => push_unique(&mut searched, &t.arg),
            _ => push_unique(&mut other, &t.tool),
        }
    }

    let mut lines = vec![format!("Earlier ({} turns) summary:", older.len())];
    if !wrote.is_empty() {
        lines.push(format!("- wrote: {}", wrote.join(", ")));
    }
    if !read.is_empty() {
        lines.push(format!("- read: {}", read.join(", ")));
    }
    if !searched.is_empty() {
        lines.push(format!("- searched: {}", searched.join(", ")));
    }
    if !other.is_empty() {
        lines.push(format!("- other: {}", other.join(", ")));
    }
    if errors > 0 {
        lines.push(format!("- {errors} error(s) encountered and handled"));
    }
    lines.join("\n")
}

fn push_unique<'a>(v: &mut Vec<&'a str>, s: &'a str) {
    if !s.is_empty() && !v.contains(&s) {
        v.push(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turns() -> Vec<TurnRecord> {
        vec![
            TurnRecord::new("read_file", "src/a.rs", false),
            TurnRecord::new("read_file", "src/a.rs", false), // dup collapses
            TurnRecord::new("search_code", "fn main", false),
            TurnRecord::new("write_file", "src/a.rs", false),
            TurnRecord::new("write_file", "src/b.rs", true), // an error turn
            TurnRecord::new("read_file", "missing.rs", true),
        ]
    }

    #[test]
    fn split_keeps_recent_and_compacts_the_rest() {
        let t = turns();
        let (older, recent) = split_for_compaction(&t, 2);
        assert_eq!(older.len(), 4);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[1].arg, "missing.rs");
    }

    #[test]
    fn split_compacts_nothing_when_short() {
        let t = turns();
        let (older, recent) = split_for_compaction(&t, 10);
        assert!(older.is_empty());
        assert_eq!(recent.len(), t.len());
    }

    #[test]
    fn summary_groups_actions_dedups_and_counts_errors() {
        let s = summarize_history(&turns());
        assert!(s.contains("wrote: src/a.rs, src/b.rs"), "{s}");
        // a.rs read twice -> listed once in the read line.
        assert!(s.contains("read: src/a.rs, missing.rs"), "{s}");
        assert!(s.contains("searched: fn main"), "{s}");
        assert!(s.contains("2 error(s)"), "{s}");
        assert!(s.contains("6 turns"), "{s}");
    }

    #[test]
    fn empty_history_summarizes_to_empty() {
        assert_eq!(summarize_history(&[]), "");
    }
}
