//! Small standalone helper functions shared across the app modules.

use super::*;

/// A two-button segmented control choosing a [`Provider`] for one stage; the selected side is
/// highlighted. `on_pick` turns a chosen provider into the stage's routing message.
pub(crate) fn provider_toggle(
    selected: sc_win::config::Provider,
    on_pick: fn(sc_win::config::Provider) -> Message,
) -> Element<'static, Message> {
    use sc_win::config::Provider;
    let seg = |p: Provider| {
        let active = selected == p;
        button(
            text(p.label().to_string())
                .size(12)
                .color(if active { FG } else { FG_MUTED }),
        )
        .on_press(on_pick(p))
        .padding([4, 12])
        .style(if active {
            primary_button
        } else {
            stage_toggle_button
        })
    };
    row![seg(Provider::Local), seg(Provider::Gemini)]
        .spacing(6)
        .into()
}

/// Map a live `AgentEvent` to a concise chat line for the line-comment fix feed, or `None`
/// for events too noisy to surface (plain reads, model turns, plan chatter). Keeps the feed
/// to the steps a human cares about: editing a file, and the verify result.
pub(crate) fn fix_feed_line(e: &sc_core::AgentEvent) -> Option<String> {
    use sc_core::AgentEvent::*;
    match e {
        // The model's own account of what it's seeing / about to do — so the execute feed
        // reads as a running narration, not just a list of file touches.
        ModelTurn { raw, .. } => sc_win::view::narration(raw).map(|n| format!("💭 {n}")),
        // Surface EVERY tool action, not just edits — the coder model spends most turns
        // searching/reading (and often emits a bare tool call with no prose), so without these
        // the execute feed sat silent and the run "felt dead". A line per action makes the work
        // visible turn by turn.
        ToolCall { tool, arg } => {
            let arg = arg.trim();
            Some(match tool.as_str() {
                "edit_file" | "edit_lines" | "edit_function" => format!("✎ editing {arg}"),
                "write_file" | "create_file" => format!("✎ writing {arg}"),
                "append_file" => format!("✎ appending to {arg}"),
                "read_file" => format!("· reading {arg}"),
                "search_code" => format!("🔍 searching for {arg}"),
                "find_symbol" => format!("🔍 locating {arg}"),
                "list_dir" => format!("· listing {arg}"),
                "run_verification" | "run_command" => "· checking it compiles…".to_string(),
                "finish" => "✓ done with this step".to_string(),
                // An unknown tool still gets a line so nothing runs invisibly.
                other => format!("· {other} {arg}").trim_end().to_string(),
            })
        }
        Verification { green, .. } => Some(if *green {
            "✓ compiles".to_string()
        } else {
            "✗ didn't compile — trying again".to_string()
        }),
        _ => None,
    }
}

/// Find a README in `dir`, case-insensitively (`README.md`, `readme.md`, `Readme.md`, or a
/// bare `README`). Returns the first match, or `None`.
pub(crate) fn find_readme(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let lower = name.to_ascii_lowercase();
            if lower == "readme.md" || lower == "readme" || lower == "readme.txt" {
                return Some(path);
            }
        }
    }
    None
}

/// Find a dedicated TODO file in `dir`, case-insensitively (`TODO.md`, `todo.md`, `TODO`,
/// `TODO.txt`). Returns the first match, or `None`.
pub(crate) fn find_todo_file(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let lower = name.to_ascii_lowercase();
            if lower == "todo.md" || lower == "todo" || lower == "todo.txt" {
                return Some(path);
            }
        }
    }
    None
}

/// "s" when `n != 1`, for plain pluralization.
pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// A computed snapshot of the workspace's tree + git state. Produced by [`compute_snapshot`]
/// (the expensive filesystem walk + git subprocess calls) so that work can run OFF the UI thread;
/// [`App::apply_snapshot`] then applies it with cheap assignments.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceSnapshot {
    pub(crate) tree: Vec<sc_win::filetree::TreeRow>,
    pub(crate) file_status: std::collections::BTreeMap<String, sc_win::gitdiff::FileStatus>,
    pub(crate) stage_states: std::collections::BTreeMap<String, sc_win::gitdiff::StageState>,
    pub(crate) unstaged_deltas: std::collections::BTreeMap<String, sc_win::gitdiff::LineDelta>,
    pub(crate) staged_deltas: std::collections::BTreeMap<String, sc_win::gitdiff::LineDelta>,
    pub(crate) branch: Option<String>,
    pub(crate) upstream: sc_win::gitdiff::UpstreamStatus,
}

/// Compute the full workspace snapshot: walk the tree and run the git status/diff/branch queries.
/// This is the BLOCKING work (filesystem + `git` subprocesses); it takes no `&self` so it can run
/// on a background thread (see `Message::SyncWorkspace`). Pure — reads the workspace, mutates
/// nothing.
pub(crate) fn compute_snapshot(root: std::path::PathBuf) -> WorkspaceSnapshot {
    let tree = sc_win::filetree::full_rows(&root);
    let file_status = sc_win::gitdiff::statuses(&root);
    let stage_states = sc_win::gitdiff::stage_states(&root);
    let mut unstaged_deltas = sc_win::gitdiff::line_deltas(&root, false);
    let staged_deltas = sc_win::gitdiff::line_deltas(&root, true);
    // Untracked files don't show in `git diff --numstat`; count their lines directly as all-added
    // so the Changes row still shows a +N.
    for (path, status) in &file_status {
        if *status == sc_win::gitdiff::FileStatus::Added && !unstaged_deltas.contains_key(path) {
            if let Ok(text) = std::fs::read_to_string(root.join(path)) {
                unstaged_deltas.insert(
                    path.clone(),
                    sc_win::gitdiff::LineDelta {
                        added: text.lines().count(),
                        removed: 0,
                    },
                );
            }
        }
    }
    let branch = sc_win::gitdiff::current_branch(&root);
    let upstream = sc_win::gitdiff::upstream_status(&root);
    WorkspaceSnapshot {
        tree,
        file_status,
        stage_states,
        unstaged_deltas,
        staged_deltas,
        branch,
        upstream,
    }
}

/// `None` for empty/whitespace, else the trimmed value — for optional config inputs.
pub(crate) fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// The workflow task for executing a feature plan. Names the plan file so the workflow pins
/// its contents every phase (via `referenced_plan`) and grounds the design on it — and frames
/// the run as designing (not implementing), since plan-only stops at the breakdown for review.
pub(crate) fn plan_task(plan_name: &str) -> String {
    format!(
        "Design how to implement the feature plan in {plan_name}. Read the plan, look at the \
         relevant existing files, and produce a spec, an architecture, a file layout, and an \
         ordered implementation breakdown that follows the plan's Approach and Files-to-touch. \
         This is a DESIGN pass — do not write source code yet."
    )
}

/// Whether a proposed file is a feature spec (a `specs/<slug>.md`, or a legacy `PLAN-*.md`), as
/// opposed to a README/TODO plan-file edit. Only specs get the "Execute plan" build button —
/// README/TODO aren't things you "build".
pub(crate) fn is_feature_plan(name: &str) -> bool {
    sc_win::chat::is_spec_path(name.trim())
}

/// Normalize any artifact of a `specs/<slug>/` feature to that feature's canonical `spec.md`, so
/// Breakdown/Build act on the FEATURE (targeting `specs/<slug>/`, reusing its approved design)
/// regardless of which phase file — `architecture.md`, `breakdown.md`, `decomposition.md`, … — the
/// user happens to have open. A `specs/<slug>.md` (flat) or legacy `PLAN-*.md` has no feature
/// folder, so it's returned unchanged.
pub(crate) fn feature_spec_of(rel: &str) -> String {
    let p = rel.replace('\\', "/");
    // A `specs/<slug>/<artifact>.md` → `specs/<slug>/spec.md`. Requires a path segment between
    // `specs/` and the filename (i.e. a real feature folder).
    let lower = p.to_ascii_lowercase();
    if lower.starts_with("specs/") && lower.ends_with(".md") {
        if let Some(dir) = p.rsplit_once('/').map(|(d, _)| d) {
            // `dir` must be `specs/<slug>` (exactly one slug segment) — not bare `specs`.
            if !dir.eq_ignore_ascii_case("specs") && dir.matches('/').count() == 1 {
                return format!("{dir}/spec.md");
            }
        }
    }
    p
}

/// The prefix to remember when a user clicks "Allow & remember": the command up to
/// and including the first space (so `git push` remembers `git `), or the whole
/// command if it has no space.
pub(crate) fn remember_prefix(command: &str) -> String {
    match command.find(' ') {
        Some(i) => command[..=i].to_string(),
        None => command.to_string(),
    }
}

// Keep `ToolCalling` referenced so the settings surface can grow into it without an
// unused-import churn; the v0 settings panel exposes the common knobs first.
#[allow(dead_code)]
const _: Option<ToolCalling> = None;
