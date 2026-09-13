//! Where a feature spec lives, what it's called, and how a bare-prose plan becomes one.

use super::ProposedFile;

/// The workspace-relative path a feature spec is saved at: `specs/<slug>/spec.md` — the spec
/// lives in its own OpenSpec-style directory, so the design phases (architecture.md, layout.md,
/// breakdown.md) can sit beside it when the plan is executed.
pub fn spec_path(slug: &str) -> String {
    format!("specs/{slug}/spec.md")
}

/// Whether `path` is a feature spec: `specs/<slug>/spec.md` (the current layout), a bare
/// `specs/<name>.md` (the interim layout), or a legacy `PLAN-<slug>.md` — so existing specs and
/// plans in a project still get the Execute-plan / grounding treatment.
pub fn is_spec_path(path: &str) -> bool {
    let p = path.replace('\\', "/");
    let lower = p.to_ascii_lowercase();
    (lower.starts_with("specs/") && lower.ends_with(".md")) || {
        // Legacy PLAN-<slug>.md anywhere (back-compat with existing projects).
        let name = p.rsplit('/').next().unwrap_or(&p);
        let un = name.to_ascii_uppercase();
        un.starts_with("PLAN-") && un.ends_with(".MD")
    }
}

/// Prepend a `## Request` block quoting the user's verbatim `request` to a spec's `content`,
/// so every saved spec records exactly what was asked for (provenance / traceability). Injected
/// by the app (not the model) so it's the user's exact words, not a paraphrase. No-op if the
/// content already opens with a Request block (idempotent) or the request is blank.
pub fn prepend_request(content: &str, request: &str) -> String {
    let req = request.trim();
    if req.is_empty() || content.trim_start().starts_with("## Request") {
        return content.to_string();
    }
    // Quote each line of the request as a markdown blockquote.
    let quoted: String = req
        .lines()
        .map(|l| format!("> {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("## Request\n{quoted}\n\n{}", content.trim_start())
}

/// Wrap a bare-prose feature plan into a spec [`ProposedFile`]. Used when the model returned a
/// plan as plain prose instead of the requested ```file: block (common on small local models
/// when the prompt is large) — so a plan ALWAYS yields an Apply/verify card rather than
/// silently staying prose in the chat.
///
/// The spec is named after its OWN `# <Feature> Specification` heading when it has one (e.g. →
/// `specs/add-alternate-seat-types/spec.md`) — a far better name than the user's raw phrasing —
/// falling back to `fallback_slug` (derived from the request) only when there's no title heading.
pub fn wrap_plan_prose(prose: &str, fallback_slug: &str) -> ProposedFile {
    let slug = plan_title(prose)
        .map(|t| slugify(&t))
        .filter(|s| s != "feature")
        .unwrap_or_else(|| fallback_slug.to_string());
    ProposedFile {
        name: spec_path(&slug),
        content: prose.trim().to_string(),
        applied: false,
    }
}

/// The title from a `# <Feature> Specification` heading in `prose` (or a legacy `## Plan:
/// <title>`), if present. Used to name a wrapped plan file after its own subject.
fn plan_title(prose: &str) -> Option<String> {
    for line in prose.lines() {
        let t = line.trim_start_matches('#').trim();
        // New spec heading: `<Feature> Specification` → the feature name is the title.
        if let Some(name) = t
            .strip_suffix(" Specification")
            .or_else(|| t.strip_suffix(" specification"))
        {
            if !name.trim().is_empty() {
                return Some(name.trim().to_string());
            }
        }
        // Back-compat: the old `Plan: <title>` heading.
        if let Some(rest) = t
            .strip_prefix("Plan:")
            .or_else(|| t.strip_prefix("plan:"))
            .or_else(|| t.strip_prefix("PLAN:"))
        {
            let title = rest.trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

/// Public slugifier so the app can name a wrapped plan after the FEATURE (from the user's
/// request) rather than the open file. E.g. "add alternate seat types" → "add-alternate-seat".
pub fn slug_for(text: &str) -> String {
    slugify(text)
}

/// Kebab-case a name into a filename slug, keeping it short. E.g. "SolarPanelTracker" →
/// "solar-panel-tracker" (camelCase split), "Solar Panel" → "solar-panel".
pub(super) fn slugify(name: &str) -> String {
    // Split on non-alphanumeric AND camelCase humps, lowercase, join with '-'.
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if c.is_ascii_uppercase() && prev_lower && !cur.is_empty() {
                words.push(std::mem::take(&mut cur)); // camelCase boundary
            }
            cur.push(c.to_ascii_lowercase());
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        } else if !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
            prev_lower = false;
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let slug = words
        .into_iter()
        .filter(|w| !w.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "feature".to_string()
    } else {
        slug
    }
}
