//! `profile_hotspots` — a recorded profile, as an observation.
//!
//! A thin adapter, like [`super::cargo`]: `sc-flame` parses the folded stacks and ranks the
//! frames, and this turns a validated call into the right view.
//!
//! # Why this reads a file rather than running a profiler
//!
//! The obvious tool is "profile this project". It would be the wrong one. Recording needs a
//! sampler that may not be installed, takes minutes, and on Windows often needs an elevated
//! shell — a tool that fails for environmental reasons teaches a small model to distrust the
//! harness (spec 04).
//!
//! Reading an existing profile has none of those properties: it is deterministic, instant, and
//! works wherever the file does. The human records — from the Profiler panel, or by hand — and
//! the agent gets to answer *which function is slow*, which is the part it is actually good for.

use std::path::Path;

/// How many frames to report when the model does not say.
///
/// Enough to see a shape, few enough that the answer is still an answer. A profile's tail is
/// thousands of frames at 0.01% and pasting it would bury the three that matter.
const DEFAULT_LIMIT: usize = 15;

/// The most that can be asked for, whatever the model passes.
const MAX_LIMIT: usize = 100;

/// Answer a `profile_hotspots` call.
///
/// `limit` is clamped rather than rejected: a model asking for 5000 rows has made a judgement
/// error, not a syntax error, and an observation that quietly gives it 100 is more useful than
/// a validation failure it has to spend a turn recovering from.
pub fn profile_hotspots(workspace: &Path, path: &str, limit: Option<i64>) -> String {
    let limit = limit
        .map(|n| n.clamp(1, MAX_LIMIT as i64) as usize)
        .unwrap_or(DEFAULT_LIMIT);

    // Resolved against the workspace, never trusted from the model — the same rule every other
    // path-taking tool follows.
    let full = workspace.join(path);
    let text = match std::fs::read_to_string(&full) {
        Ok(t) => t,
        Err(e) => {
            return format!(
                "Could not read the profile at `{path}`: {e}\n\
                 Expected a folded-stack file (one line of `main;work 42` per unique stack), \
                 such as the one the Profiler panel writes to target/sc-profile.folded."
            )
        }
    };

    let profile = sc_flame::parse_folded(&text);
    if profile.is_empty() {
        return format!(
            "`{path}` holds no readable stacks ({} unreadable lines). A folded-stack file has \
             one line per stack: a semicolon-separated call path, a space, then a sample count.",
            profile.skipped
        );
    }

    render(&profile, path, limit)
}

/// The observation the model reads.
///
/// Percentages, not raw sample counts, are what carry meaning across profiles — "4200 samples"
/// answers nothing without the total, so both are given and the percentage leads.
fn render(profile: &sc_flame::Profile, path: &str, limit: usize) -> String {
    let total = profile.total();
    let hot = sc_flame::hot_frames(&profile.root, limit);

    let mut out = format!(
        "{path}: {total} samples across {} frames.\n\
         Hottest frames by SELF time (time in the function itself, not its callees):\n",
        profile.root.count()
    );
    for (i, (name, own)) in hot.iter().enumerate() {
        let pct = *own as f32 / total.max(1) as f32 * 100.0;
        out.push_str(&format!("{:>3}. {pct:>6.2}%  {own:>9}  {name}\n", i + 1));
    }

    let shown: u64 = hot.iter().map(|(_, n)| *n).sum();
    let rest = total.saturating_sub(shown);
    if rest > 0 {
        out.push_str(&format!(
            "     {:>6.2}%  {rest:>9}  (everything else)\n",
            rest as f32 / total.max(1) as f32 * 100.0
        ));
    }
    if profile.skipped > 0 {
        out.push_str(&format!(
            "{} unreadable lines were skipped.\n",
            profile.skipped
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sc-flame-tool-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn it_ranks_by_self_time_and_gives_percentages() {
        let w = workspace();
        write(
            &w,
            "a.folded",
            "main;work;alloc 70\nmain;work 20\nmain;idle 10\n",
        );
        let out = profile_hotspots(&w, "a.folded", None);
        assert!(out.contains("100 samples"), "{out}");
        // `alloc` is 70% and leads the table.
        assert!(out.contains("70.00%"), "{out}");
        let alloc = out.find("alloc").expect("alloc listed");
        let idle = out.find("idle").expect("idle listed");
        assert!(alloc < idle, "hottest first:\n{out}");
        // `main` has no self time at all and must not be reported as a hotspot.
        assert!(
            !out.lines().any(|l| l.trim_end().ends_with(" main")),
            "a frame with no self time is not a hotspot:\n{out}"
        );
    }

    #[test]
    fn a_missing_file_explains_the_format_rather_than_just_failing() {
        let out = profile_hotspots(&workspace(), "nope.folded", None);
        assert!(out.contains("Could not read"), "{out}");
        // An error the model can act on names the shape it wanted.
        assert!(out.contains("main;work 42"), "{out}");
    }

    #[test]
    fn an_unparseable_file_says_so_instead_of_reporting_an_empty_profile() {
        let w = workspace();
        write(&w, "junk.folded", "this is not a profile\nnor is this\n");
        let out = profile_hotspots(&w, "junk.folded", None);
        assert!(out.contains("no readable stacks"), "{out}");
        assert!(out.contains("2 unreadable"), "{out}");
    }

    #[test]
    fn the_limit_is_clamped_rather_than_rejected() {
        let w = workspace();
        let body: String = (0..50).map(|i| format!("main;f{i} 1\n")).collect();
        write(&w, "many.folded", &body);

        // A silly request still returns an answer, capped rather than refused.
        let huge = profile_hotspots(&w, "many.folded", Some(9_999));
        assert!(huge.lines().count() <= MAX_LIMIT + 4, "capped:\n{huge}");
        // Zero or negative still yields at least one row.
        let zero = profile_hotspots(&w, "many.folded", Some(0));
        assert!(zero.contains("  1. "), "{zero}");
        let neg = profile_hotspots(&w, "many.folded", Some(-5));
        assert!(neg.contains("  1. "), "{neg}");
    }

    #[test]
    fn the_unlisted_remainder_is_accounted_for() {
        let w = workspace();
        let body: String = (0..40).map(|i| format!("main;f{i} 10\n")).collect();
        write(&w, "tail.folded", &body);
        let out = profile_hotspots(&w, "tail.folded", Some(5));
        // Without this line the percentages do not add up, and the model cannot tell whether
        // it has seen the whole story.
        assert!(out.contains("(everything else)"), "{out}");
    }
}
