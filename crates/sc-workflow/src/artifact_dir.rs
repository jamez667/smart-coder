//! Task text → the `specs/<slug>/` directory the design artifacts land in.
//!
//! The OpenSpec layout is the default for a gated run: every phase file
//! (`spec.md`, `architecture.md`, `layout.md`, …) lands in `specs/<slug>/` beside
//! the spec, so it's reviewable as a diff and openable in an editor. The numbered
//! `.smart-coder/plan/` layout ([`crate::plan_dir`]) is the fallback for a task
//! with no usable name.
//!
//! This lives in the engine rather than a front-end so the CLI and the desktop GUI
//! resolve the same directory for the same task — which is also what lets a **Build**
//! resume the design a prior **Breakdown** approved (same task → same dir →
//! [`crate::load_from`] finds its `state.json`).

use std::path::{Path, PathBuf};

/// If `task` references a `specs/<slug>/spec.md`, return the absolute
/// `<workspace>/specs/<slug>/` directory so the design phases land beside the spec
/// (OpenSpec layout). Otherwise derive `specs/<slugified-task>/` from the task text.
/// `None` only when the task has no alphanumerics at all (the workflow then uses its
/// default `.smart-coder/plan/`).
pub fn spec_artifact_dir(task: &str, workspace: &Path) -> Option<PathBuf> {
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
    // A referenced path is used only if it stays *inside* the workspace. The token
    // came from free task text, and task text is not always the developer's — a
    // request filed from the intake surface reaches here verbatim. Without this,
    // `specs/../../../../etc/cron.d/x/spec.md` resolves outside the tree and the
    // workflow then does `create_dir_all` + writes model-authored content there,
    // which is an arbitrary write on the machine running the daemon.
    //
    // Escaping falls back to the derived slug rather than erroring: the caller
    // asked for a spec directory, and refusing outright would turn a hostile
    // string into a denial of service for a legitimate run.
    if let Some(dir_rel) = referenced.filter(|d| is_contained(d)) {
        return Some(workspace.join(dir_rel));
    }

    // 2) Otherwise derive a `specs/<slug>/` folder from the task text, so EVERY run lands its
    //    design in `specs/<name>/spec.md` (+ architecture.md, layout.md, …) — the OpenSpec layout
    //    is the default, not the old numbered `.smart-coder/plan/` fallback.
    let slug = slugify(task);
    if slug.is_empty() {
        return None; // truly empty/garbage task ⇒ let the workflow use its plan-dir fallback.
    }
    Some(workspace.join("specs").join(slug))
}

/// Does this workspace-relative directory stay inside the workspace?
///
/// Checked **lexically**, on the relative form, before any `join` — deliberately
/// not with `canonicalize`, which requires the path to exist and would therefore
/// pass a hostile path simply because the target directory has not been created
/// yet. The escape has to be impossible to *construct*, not merely impossible to
/// resolve.
///
/// The token has already had `\` folded to `/` by the caller, so Windows
/// separators are covered by the same check.
fn is_contained(dir_rel: &str) -> bool {
    // Absolute, in either flavour: `join` on an absolute path *replaces* the base
    // rather than extending it, so this is an escape even without a `..`.
    if dir_rel.starts_with('/') {
        return false;
    }
    // A drive letter or a UNC share is absolute on Windows and would replace the
    // base the same way.
    let bytes = dir_rel.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        return false;
    }

    // No component may climb. Checked per component rather than by substring, so
    // a legitimate name that merely *contains* two dots (`specs/v1..v2/`) is kept.
    !dir_rel.split('/').any(|part| part == "..")
}

/// Turn free task text into a short kebab-case folder name for `specs/<slug>/`. Lower-cases,
/// keeps `[a-z0-9]`, collapses every other run into a single `-`, trims leading/trailing `-`, and
/// caps the length + word count so a long prompt doesn't become an unwieldy directory name. Empty
/// when the text has no alphanumerics.
pub fn slugify(task: &str) -> String {
    // Drop a leading spec-instruction boilerplate so the slug reflects the feature, not the verb.
    // A plan task wraps a plan name as "Design how to implement the feature plan in <X>. …"; strip
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

/// The artifact directory for `task` plus its workspace-relative, forward-slashed form
/// (e.g. `specs/alt-seats`) — what a UI needs to open each phase file in an editor and
/// anchor line comments to it. `(None, None)` when the task yields no slug, i.e. the
/// caller should fall back to [`crate::plan_dir`].
pub fn artifact_dirs(task: &str, workspace: &Path) -> (Option<PathBuf>, Option<String>) {
    let dir = spec_artifact_dir(task, workspace);
    let rel = dir.as_ref().and_then(|d| {
        d.strip_prefix(workspace)
            .ok()
            .map(|r| r.to_string_lossy().replace('\\', "/"))
    });
    (dir, rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_referenced_spec_path_cannot_escape_the_workspace() {
        // Task text is not always the developer's: a request filed from the
        // intake surface reaches here verbatim. Without containment, this
        // resolves outside the tree and the workflow then does `create_dir_all`
        // and writes model-authored content there — an arbitrary write on the
        // machine running the daemon.
        let ws = Path::new("/proj");
        for hostile in [
            "specs/../../../../etc/cron.d/x/spec.md",
            "specs/../.ssh/spec.md",
            "specs/a/../../../b/spec.md",
            // Windows separators are folded to `/` before the check, so they are
            // covered by the same rule rather than a second one.
            "specs\\..\\..\\Windows\\System32\\spec.md",
        ] {
            let d = spec_artifact_dir(&format!("Design {hostile}."), ws)
                .expect("falls back to a slug rather than failing");
            assert!(
                d.starts_with(ws.join("specs")),
                "{hostile} escaped to {}",
                d.display()
            );
            let shown = d.to_string_lossy().replace('\\', "/");
            assert!(!shown.contains(".."), "{hostile} left a climb in {shown}");
        }
    }

    #[test]
    fn an_absolute_referenced_path_is_refused_because_join_would_replace_the_base() {
        // `join` on an absolute path discards the workspace entirely, so this is
        // an escape with no `..` in it at all.
        let ws = Path::new("/proj");
        for hostile in ["/etc/specs/x/spec.md", "specs/x/spec.md"] {
            let d = spec_artifact_dir(&format!("Design {hostile}."), ws).unwrap();
            assert!(d.starts_with(ws), "{hostile} -> {}", d.display());
        }
    }

    #[test]
    fn containment_is_lexical_so_a_path_that_does_not_exist_yet_is_still_checked() {
        // `canonicalize` would pass a hostile path simply because the target has
        // not been created — the escape must be impossible to construct, not
        // merely impossible to resolve.
        assert!(!is_contained("specs/../x"));
        assert!(!is_contained("/absolute/x"));
        assert!(!is_contained("C:/Windows/x"));
        assert!(!is_contained("c:/windows/x"));
        assert!(is_contained("specs/a-normal-slug"));
        assert!(is_contained("specs/nested/deeper"));
    }

    #[test]
    fn a_name_that_merely_contains_dots_is_not_mistaken_for_a_climb() {
        // Checked per component, so a legitimate directory keeps working. A
        // substring check on ".." would break this.
        assert!(is_contained("specs/v1..v2"));
        assert!(is_contained("specs/a.b.c"));
        assert!(is_contained("specs/..hidden-but-not-a-climb"));
    }

    #[test]
    fn spec_artifact_dir_uses_a_referenced_spec_path_verbatim() {
        let ws = Path::new("/proj");
        // A task naming specs/<slug>/spec.md → the artifact dir is exactly that feature dir.
        let d = spec_artifact_dir("Design how to implement specs/alt-seats/spec.md.", ws);
        assert_eq!(d, Some(ws.join("specs").join("alt-seats")));
    }

    #[test]
    fn spec_artifact_dir_derives_specs_slug_from_a_plain_prompt() {
        let ws = Path::new("/proj");
        // A plain prompt now ALSO lands in specs/<slug>/ (the OpenSpec layout is the default).
        assert_eq!(
            spec_artifact_dir("Add seat types for crew roles", ws),
            Some(ws.join("specs").join("add-seat-types-for-crew-roles"))
        );
        // The plan-task boilerplate lead-in is stripped so the slug is the feature, not the verb.
        assert_eq!(
            spec_artifact_dir(
                "Design how to implement the feature plan in seat types. Read the plan…",
                ws
            ),
            Some(ws.join("specs").join("seat-types"))
        );
        // A truly empty/garbage task ⇒ None ⇒ the workflow's plan-dir fallback.
        assert_eq!(spec_artifact_dir("   ", ws), None);
        assert_eq!(spec_artifact_dir("!!! ???", ws), None);
    }

    #[test]
    fn slugify_is_kebab_case_and_capped() {
        assert_eq!(slugify("Add Seat Types"), "add-seat-types");
        assert_eq!(slugify("  spaces   and---dashes  "), "spaces-and-dashes");
        assert_eq!(slugify("weird!!chars@@here"), "weird-chars-here");
        assert_eq!(slugify(""), "");
        // First sentence only (instruction boilerplate after a period is dropped).
        assert_eq!(slugify("Seat types. Do not write code yet."), "seat-types");
        // Length cap keeps the folder name reasonable.
        let long = "a".repeat(80);
        assert!(slugify(&long).len() <= 40);
    }

    #[test]
    fn artifact_dirs_returns_the_relative_form_for_a_ui() {
        let ws = Path::new("/proj");
        let (dir, rel) = artifact_dirs("Add seat types", ws);
        assert_eq!(dir, Some(ws.join("specs").join("add-seat-types")));
        // Forward-slashed and workspace-relative, whatever the platform separator.
        assert_eq!(rel.as_deref(), Some("specs/add-seat-types"));
        // No slug ⇒ no dir and no relative form (caller falls back to the plan dir).
        assert_eq!(artifact_dirs("!!!", ws), (None, None));
    }
}
