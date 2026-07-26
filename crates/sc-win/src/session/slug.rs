//! Task text → the `specs/<slug>/` directory the design artifacts land in.

use std::path::PathBuf;

/// If `task` references a `specs/<slug>/spec.md`, return the absolute
/// `<workspace>/specs/<slug>/` directory so the design phases land beside the spec
/// (OpenSpec layout). `None` otherwise (the workflow then uses its default
/// `.smart-coder/plan/`).
pub fn spec_artifact_dir(task: &str, workspace: &std::path::Path) -> Option<PathBuf> {
    // 1) If the task already names a `specs/<slug>/spec.md`, use that feature dir verbatim (a Build
    //    of a plan the user already wrote / a prior Breakdown created).
    let referenced = task
        .split(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | ','))
        .map(|t| t.trim_end_matches('.').replace('\\', "/"))
        .find(|t| {
            t.to_ascii_lowercase().starts_with("specs/")
                && t.to_ascii_lowercase().ends_with("/spec.md")
        })
        .and_then(|token| {
            token
                .strip_suffix("/spec.md")
                .or_else(|| token.strip_suffix("/SPEC.MD"))
                .map(|d| d.to_string())
        });
    if let Some(dir_rel) = referenced {
        return Some(workspace.join(dir_rel));
    }

    // 2) Otherwise derive a `specs/<slug>/` folder from the task text, so EVERY run lands its
    //    design in `specs/<name>/spec.md` (+ architecture.md, layout.md, …) — the OpenSpec layout
    //    is now the default, not the old numbered `.smart-coder/plan/` fallback.
    let slug = slugify(task);
    if slug.is_empty() {
        return None; // truly empty/garbage task ⇒ let the workflow use its plan-dir fallback.
    }
    Some(workspace.join("specs").join(slug))
}

/// Turn free task text into a short kebab-case folder name for `specs/<slug>/`. Lower-cases,
/// keeps `[a-z0-9]`, collapses every other run into a single `-`, trims leading/trailing `-`, and
/// caps the length + word count so a long prompt doesn't become an unwieldy directory name. Empty
/// when the text has no alphanumerics.
pub fn slugify(task: &str) -> String {
    // Drop a leading spec-instruction boilerplate so the slug reflects the feature, not the verb.
    // `plan_task` wraps a plan name as "Design how to implement the feature plan in <X>. …"; strip
    // that lead-in so the slug is the feature, not "design-how-to-implement…". Best-effort — a
    // plain prompt has no such prefix and slugifies as-is.
    let text = task.trim();
    let text = text
        .strip_prefix("Design how to implement the feature plan in ")
        .or_else(|| text.strip_prefix("Design how to implement the feature in "))
        .unwrap_or(text);
    // Only the first sentence carries the feature name; the rest is instruction boilerplate.
    let text = text.split(['.', '\n']).next().unwrap_or(text).trim();
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    // Cap to the first few words (~40 chars) so the folder name stays readable.
    let capped: String = slug.chars().take(40).collect();
    capped.trim_matches('-').to_string()
}
