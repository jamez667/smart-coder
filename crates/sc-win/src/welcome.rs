//! The project-open "welcome" flow: when a folder is opened, greet the user with a
//! readout of the project's README — surfacing its TODO / roadmap section if it has one —
//! and an invitation to say what they want to work on. Pure text logic (no iced, no I/O
//! beyond a passed-in string), so it's host-testable; the app reads the README file and
//! renders the returned lines into the Activity stream.

/// A prepared welcome: a short project title line, the highlighted TODO/roadmap excerpt
/// (may be empty if the project has none), and the closing prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    /// The project name (README's first `# ` heading, else the folder name).
    pub title: String,
    /// The lines to show in Activity, each flagged as a "highlight" (a TODO/roadmap item)
    /// or plain context. Rendering decides the colour.
    pub lines: Vec<WelcomeLine>,
    /// The closing call-to-action shown under the excerpt.
    pub prompt: String,
    /// True when the project has NO TODO source at all — no TODO.md and no TODO/roadmap
    /// section in the README. The app uses this to nudge the user to create one (a TODO is
    /// how the agent knows what to work on), rather than silently showing nothing.
    pub no_todo: bool,
}

/// One rendered welcome line and whether it's a highlighted TODO item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeLine {
    pub text: String,
    /// True for a roadmap/TODO bullet worth calling out (rendered in the accent colour).
    pub highlight: bool,
}

/// Headings that mark specifically the *not-yet-done* part of a README — preferred over a
/// generic "roadmap" so we jump straight past completed work to what's actually left.
const FUTURE_HEADINGS: &[&str] = &[
    "todo",
    "to do",
    "to-do",
    "later phases",
    "later",
    "next steps",
    "next",
    "planned",
    "upcoming",
    "backlog",
    "future",
    "not yet",
    "unreleased",
];

/// The broader "here's the plan" headings — used only if no future-specific heading exists.
/// A generic roadmap often mixes done + todo, so we filter completed items out of it.
const ROADMAP_HEADINGS: &[&str] = &["roadmap", "milestones", "status"];

/// Build the welcome from a project's README text and the contents of a dedicated TODO
/// file (`TODO.md`) if the project has one — both already read from disk (empty string =
/// absent) — plus the opened folder name (the title fallback).
///
/// TODO source preference: a dedicated `TODO.md` wins (it's unambiguous), else a
/// TODO/roadmap section in the README. If neither exists, `no_todo` is set and the prompt
/// nudges the user to create a TODO so the agent has a backlog to work from.
pub fn build(readme: &str, todo_md: &str, folder_name: &str) -> Welcome {
    let title = first_heading(readme).unwrap_or_else(|| folder_name.to_string());

    // A dedicated TODO.md is the strongest signal — every non-blank line is a candidate item.
    let (excerpt, from_todo_file) = if !todo_md.trim().is_empty() {
        (todo_file_lines(todo_md), true)
    } else {
        (todo_excerpt(readme), false)
    };
    let has_todo = !excerpt.is_empty();

    let mut lines = Vec::new();
    if has_todo {
        for l in &excerpt {
            let highlight = is_bullet(l);
            lines.push(WelcomeLine {
                text: l.clone(),
                highlight,
            });
        }
    } else {
        // No TODO anywhere — still show a little README context if there is any.
        for l in intro_lines(readme) {
            lines.push(WelcomeLine {
                text: l,
                highlight: false,
            });
        }
    }
    let _ = from_todo_file;

    let prompt = if has_todo {
        "What do you want to work on today?".to_string()
    } else {
        "No TODO list found. Create a TODO.md (or add a ## TODO section to your README) so \
         the agent has a backlog — then tell me what to work on. What would you like to do first?"
            .to_string()
    };

    Welcome {
        title,
        lines,
        prompt,
        no_todo: !has_todo,
    }
}

/// Turn a dedicated TODO.md into display lines: keep its heading (if any) and non-blank
/// lines, capped. Every bullet is a candidate item (highlighted by the caller).
fn todo_file_lines(todo_md: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in todo_md.lines() {
        let t = raw.trim_end();
        if t.trim().is_empty() {
            continue;
        }
        // Drop lines that are clearly already-done checkboxes.
        if mentions_done(t) {
            continue;
        }
        out.push(t.to_string());
        if out.len() >= 30 {
            out.push("…".to_string());
            break;
        }
    }
    out
}

/// The README's first `# ` heading text, if present.
fn first_heading(readme: &str) -> Option<String> {
    for line in readme.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("# ") {
            let name = rest.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Extract the lines under the first TODO/roadmap-style heading, up to (but not including)
/// the next heading of the same-or-higher level. Empty if the README has no such section.
/// Blank lines are dropped; the section heading itself is kept as the first line.
fn todo_excerpt(readme: &str) -> Vec<String> {
    let lines: Vec<&str> = readme.lines().collect();

    // Prefer a *future-specific* heading (Later / TODO / Next / …) so we skip completed
    // work entirely. Only fall back to a generic Roadmap/Status section if none exists —
    // and there we filter out anything marked "(done)".
    let (start, level, filter_done) = match find_heading(&lines, FUTURE_HEADINGS) {
        Some((i, lvl)) => (i, lvl, false),
        None => match find_heading(&lines, ROADMAP_HEADINGS) {
            Some((i, lvl)) => (i, lvl, true),
            None => return Vec::new(),
        },
    };

    let mut out = Vec::new();
    if let Some((_, title)) = heading(lines[start]) {
        out.push(title.to_string());
    }
    // Collect until the next heading at the same or higher level. Skip "(done)" bullets
    // (and their wrapped continuation lines) when reading a generic roadmap.
    let mut skipping_done_item = false;
    for raw in &lines[start + 1..] {
        if let Some((lvl, _)) = heading(raw) {
            if lvl <= level {
                break;
            }
            skipping_done_item = false; // a sub-heading resets item-skip state
        }
        let t = raw.trim_end();
        if t.trim().is_empty() {
            continue;
        }
        if filter_done {
            // An "item" starts at a bullet, a sub-heading, OR a bold-led paragraph
            // (`**Phase 1 (done):** …` — the common roadmap style). Its continuation
            // (wrapped) lines inherit the skip until the next item starts.
            let starts_item = is_bullet(t) || heading(raw).is_some() || starts_bold(t);
            if starts_item {
                skipping_done_item = mentions_done(t);
            }
            if skipping_done_item {
                continue; // drop the done item and its continuation lines
            }
        }
        out.push(t.to_string());
        if out.len() >= 30 {
            out.push("…".to_string());
            break;
        }
    }
    // If filtering left only the heading (everything was done), treat as "no excerpt".
    if out.len() <= 1 {
        return Vec::new();
    }
    out
}

/// Find the first heading whose title matches any of `needles`; returns `(line_index, level)`.
fn find_heading(lines: &[&str], needles: &[&str]) -> Option<(usize, usize)> {
    for (i, raw) in lines.iter().enumerate() {
        if let Some((lvl, title)) = heading(raw) {
            let lower = title.to_ascii_lowercase();
            if needles.iter().any(|n| lower.contains(n)) {
                return Some((i, lvl));
            }
        }
    }
    None
}

/// Whether a line marks completed work (e.g. "**Phase 1 (done):** …").
fn mentions_done(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("(done)") || l.contains("✅") || l.contains("[x]")
}

/// Whether a line begins a bold-led item (e.g. `**Phase 1 (done):** …`) — the paragraph
/// style many roadmaps use instead of bullets.
fn starts_bold(line: &str) -> bool {
    line.trim_start().starts_with("**")
}

/// The first non-empty prose lines of the README (skipping the title heading and badges),
/// as light context when there's no roadmap section. Capped short.
fn intro_lines(readme: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in readme.lines() {
        let t = raw.trim();
        if t.is_empty() {
            if out.is_empty() {
                continue;
            } else {
                break; // stop at the first blank line after we've gathered a paragraph
            }
        }
        if heading(raw).is_some() {
            continue; // skip headings (incl. the title)
        }
        out.push(t.to_string());
        if out.len() >= 5 {
            break;
        }
    }
    out
}

/// Parse a markdown ATX heading: returns `(level, trimmed_title)` for `#`..`######` lines.
fn heading(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_start();
    if !t.starts_with('#') {
        return None;
    }
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = t[hashes..].trim();
    // A `#` immediately followed by non-space (e.g. `#foo`) isn't a heading.
    if t.as_bytes().get(hashes).is_some_and(|b| *b != b' ') {
        return None;
    }
    Some((hashes, rest))
}

/// Whether a line is a list bullet / numbered item (worth highlighting as a TODO item).
fn is_bullet(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("+ ")
        || t.chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() && t.contains(". "))
}

#[cfg(test)]
mod tests {
    use super::*;

    const README: &str = "\
# idle-city-sim — Procedural City Generator

A top-down procedural city sim in Rust.

## Roadmap

**Phase 1 (done):** zoned rings.

**Later phases**, grouped by theme:
- Lakes in addition to the ocean.
- Highways that follow the terrain.
- Rail + stations; subway overlay.

## Architecture
- crates/city — the generator.
";

    #[test]
    fn picks_title_from_first_heading() {
        let w = build(README, "", "idle-city-sim");
        assert!(w.title.contains("idle-city-sim"), "{}", w.title);
    }

    #[test]
    fn extracts_the_roadmap_section_and_highlights_bullets() {
        let w = build(README, "", "fallback");
        let joined: Vec<&str> = w.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(joined.iter().any(|l| l.contains("Roadmap")), "{joined:?}");
        assert!(
            joined.iter().any(|l| l.contains("Lakes")),
            "roadmap bullet present: {joined:?}"
        );
        // It must STOP at the next same-level heading (## Architecture).
        assert!(
            !joined.iter().any(|l| l.contains("the generator")),
            "must not bleed into the next section: {joined:?}"
        );
        // The bullets are highlighted; the heading line isn't.
        let lake = w.lines.iter().find(|l| l.text.contains("Lakes")).unwrap();
        assert!(lake.highlight, "a roadmap bullet is highlighted");
        let head = w.lines.iter().find(|l| l.text.contains("Roadmap")).unwrap();
        assert!(!head.highlight, "the section heading is not a highlight");
    }

    #[test]
    fn done_items_are_filtered_from_a_generic_roadmap() {
        // The "(done)" line must be dropped so the readout is the TODO, not a changelog.
        let w = build(README, "", "x");
        let joined: Vec<&str> = w.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(
            !joined.iter().any(|l| l.contains("Phase 1")),
            "completed '(done)' items are filtered out: {joined:?}"
        );
        assert!(
            joined.iter().any(|l| l.contains("Rail")),
            "pending items remain: {joined:?}"
        );
    }

    #[test]
    fn a_future_specific_heading_is_preferred_over_roadmap() {
        // With an explicit TODO/Later heading, jump straight there (past any done section).
        let r = "\
# P

## Roadmap
- old done thing (done)

## TODO
- add lakes
- add rail
";
        let w = build(r, "", "p");
        let joined: Vec<&str> = w.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(
            joined.iter().any(|l| l.to_lowercase().contains("todo")),
            "used the TODO heading: {joined:?}"
        );
        assert!(
            joined.iter().any(|l| l.contains("add lakes")),
            "todo items present: {joined:?}"
        );
    }

    #[test]
    fn always_asks_what_to_work_on() {
        let w = build(README, "", "x");
        assert!(w.prompt.to_lowercase().contains("work on"), "{}", w.prompt);
    }

    #[test]
    fn no_readme_yields_title_from_folder_and_empty_excerpt() {
        let w = build("", "", "my-game");
        assert_eq!(w.title, "my-game");
        assert!(w.lines.is_empty());
        assert!(!w.prompt.is_empty());
    }

    #[test]
    fn a_dedicated_todo_md_is_preferred_and_wins() {
        // A TODO.md beats a README roadmap — it's the unambiguous backlog.
        let todo = "# TODO\n- wire up the airport\n- add rail lines\n- [x] already done thing\n";
        let w = build(README, todo, "x");
        let joined: Vec<&str> = w.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(!w.no_todo, "a TODO.md means we have a TODO");
        assert!(
            joined.iter().any(|l| l.contains("airport")),
            "TODO.md items shown: {joined:?}"
        );
        // A checked-off item is filtered.
        assert!(
            !joined.iter().any(|l| l.contains("already done")),
            "done items filtered from TODO.md: {joined:?}"
        );
        // The README roadmap is NOT used when a TODO.md exists.
        assert!(
            !joined.iter().any(|l| l.contains("Lakes")),
            "README roadmap ignored when TODO.md present: {joined:?}"
        );
    }

    #[test]
    fn no_todo_anywhere_sets_the_flag_and_prompts_to_create_one() {
        // A README with no TODO/roadmap section and no TODO.md → nudge the user.
        let r = "# Cool Project\n\nDoes a cool thing.\n\n## Install\n- cargo build\n";
        let w = build(r, "", "cool");
        assert!(w.no_todo, "no TODO source anywhere");
        let p = w.prompt.to_lowercase();
        assert!(
            p.contains("todo") && (p.contains("create") || p.contains("add")),
            "prompt nudges creating a TODO: {}",
            w.prompt
        );
    }

    #[test]
    fn readme_without_a_roadmap_shows_intro_context() {
        let r = "# Cool Project\n\nDoes a cool thing with widgets.\n\n## Install\n- cargo build\n";
        let w = build(r, "", "cool");
        assert_eq!(w.title, "Cool Project");
        assert!(
            w.lines.iter().any(|l| l.text.contains("cool thing")),
            "intro paragraph shown when no roadmap: {:?}",
            w.lines
        );
    }
}
