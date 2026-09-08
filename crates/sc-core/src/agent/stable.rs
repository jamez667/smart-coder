//! The retrieved-zone material that must NOT change between turns.
//!
//! llama.cpp reuses its KV cache for the longest byte-identical prefix of the previous
//! request. Every turn used to re-read the focus files, the imported files, the progress
//! ledger and the feature plan from disk, and re-walk the workspace for the signature map --
//! so nothing before the newest observation was guaranteed stable, and even an unchanged
//! workspace could re-prefill the whole prompt. [`StableContext`] renders that material
//! ONCE at run start and hands out the same strings until the loop reports a workspace
//! change; even then only the parts whose SOURCE changed (by content hash) are re-rendered.
//! With no edit, every segment before the recent window is byte-identical turn to turn.

use std::hash::{Hash, Hasher};
use std::path::Path;

use sc_context::{Segment, Zone};
use sc_tools::ToolRegistry;

use super::prompt::{
    imported_files, render_context_files, render_focus_files, render_other_files_map,
    render_progress_ledger,
};
use super::AgentConfig;

/// A feature-plan doc (`PLAN-<slug>.md`) pinned in full, keyed by the hash of its body.
#[derive(Debug, Clone)]
struct PinnedPlan {
    name: String,
    text: String,
    hash: u64,
}

/// The cached, rendered retrieved-zone blocks plus the focus-file render. See the module doc.
#[derive(Debug)]
pub(super) struct StableContext {
    /// The `PLAN-<slug>.md` token named in the instruction, if any. Scanned once; the file
    /// itself is re-read on a change so a plan written mid-run still gets pinned.
    plan_token: Option<String>,
    plan_doc: Option<PinnedPlan>,
    /// Whole-task only: the ranked repo map, computed once for the run.
    repo_map: String,
    /// Whole-task only: the ledger of files that exist. Re-rendered on any change (a walk).
    ledger: String,
    /// Focus mode: the files the focus files import, their rendered bodies, and the hash of
    /// those bodies.
    imports: Vec<String>,
    imports_text: String,
    imports_hash: u64,
    /// Focus mode: the signature map of everything else. Re-rendered on any change (a walk
    /// plus PageRank -- the expensive one).
    others_map: String,
    /// The line-numbered focus files, keyed by the hash of their bodies.
    focus_text: String,
    focus_hash: u64,
}

impl StableContext {
    /// Render everything once. `repo_map` is the run's ranked map (already computed for
    /// planning); it is used as-is in whole-task mode and ignored in focus mode.
    pub(super) fn new(
        workspace: &Path,
        cfg: &AgentConfig,
        registry: &ToolRegistry,
        instruction: &str,
        repo_map: String,
    ) -> Self {
        let plan_token = plan_token(instruction).map(str::to_owned);
        let mut this = Self {
            plan_doc: plan_token.as_deref().and_then(|t| read_plan(workspace, t)),
            plan_token,
            repo_map,
            ledger: String::new(),
            imports: Vec::new(),
            imports_text: String::new(),
            imports_hash: 0,
            others_map: String::new(),
            focus_text: String::new(),
            focus_hash: content_hash(workspace, &cfg.focus_files),
        };
        this.render_focus(workspace, cfg, registry);
        this.render_walks(workspace, cfg);
        this
    }

    /// Re-render only what the loop's `changed` flag and the content hashes say moved.
    /// A turn with no workspace change touches nothing, so every cached string is returned
    /// byte-for-byte and the prefix cache holds.
    pub(super) fn refresh_if_changed(
        &mut self,
        workspace: &Path,
        cfg: &AgentConfig,
        registry: &ToolRegistry,
        changed: bool,
    ) {
        if !changed {
            return;
        }
        // The plan doc: re-pinned only if its bytes moved (or it appeared).
        if let Some(token) = &self.plan_token {
            let fresh = read_plan(workspace, token);
            if fresh.as_ref().map(|p| p.hash) != self.plan_doc.as_ref().map(|p| p.hash) {
                self.plan_doc = fresh;
            }
        }
        // The focus files: an edit invalidates the prefix from the FocusFile zone anyway,
        // so re-render them and re-derive their imports; an unchanged focus file keeps its
        // render even though something else in the workspace moved.
        let focus_hash = content_hash(workspace, &cfg.focus_files);
        if focus_hash != self.focus_hash {
            self.focus_hash = focus_hash;
            self.render_focus(workspace, cfg, registry);
        } else if !cfg.focus_files.is_empty() {
            // The focus file is untouched but an imported file may have been (a
            // multi-file task edits helpers too): re-render the imports on THEIR hash.
            let imports_hash = content_hash(workspace, &self.imports);
            if imports_hash != self.imports_hash {
                self.imports_hash = imports_hash;
                self.imports_text = render_context_files(workspace, &self.imports);
            }
        }
        // The two workspace walks: a change anywhere can add a file (ledger) or a symbol
        // (signature map), and they are cheap to compare -- unchanged output stays
        // byte-identical.
        self.render_walks(workspace, cfg);
    }

    /// Render the focus files and, from them, the imported files (focus mode only).
    fn render_focus(&mut self, workspace: &Path, cfg: &AgentConfig, registry: &ToolRegistry) {
        self.focus_text = render_focus_files(workspace, &cfg.focus_files, registry);
        if cfg.focus_files.is_empty() {
            return;
        }
        // The model needs the CODE of the files its file IMPORTS FROM — signatures alone
        // weren't enough (it re-read them to see args / behavior). Pin their full bodies
        // (read-only context), bounded to the few it actually imports.
        self.imports = imported_files(workspace, &cfg.focus_files);
        self.imports_hash = content_hash(workspace, &self.imports);
        self.imports_text = render_context_files(workspace, &self.imports);
    }

    /// The workspace walks: the progress ledger (whole-task) or the signature map of the
    /// distant files (focus mode).
    fn render_walks(&mut self, workspace: &Path, cfg: &AgentConfig) {
        if cfg.focus_files.is_empty() {
            self.ledger = render_progress_ledger(workspace);
        } else {
            let mut exclude = cfg.focus_files.clone();
            exclude.extend(self.imports.iter().cloned());
            self.others_map = render_other_files_map(workspace, &exclude, cfg.repo_map_top_k);
        }
    }

    /// Push the retrieved-zone segments in their stable order: the plan doc, then the repo
    /// map + ledger (whole-task) or the imported bodies + signature map (focus mode).
    pub(super) fn push_retrieved(&self, cfg: &AgentConfig, segments: &mut Vec<Segment>) {
        if let Some(plan) = &self.plan_doc {
            segments.push(Segment::user(Zone::Retrieved, plan.text.clone()));
        }
        if cfg.focus_files.is_empty() {
            // WHOLE-TASK path: the repo map helps navigation; the progress ledger lists the
            // files that exist (so it doesn't re-create/forget them).
            if !self.repo_map.is_empty() {
                segments.push(Segment::user(Zone::Retrieved, self.repo_map.clone()));
            }
            if !self.ledger.is_empty() {
                segments.push(Segment::user(Zone::Retrieved, self.ledger.clone()));
            }
        } else {
            // FOCUSED path (per-file step): the imported bodies, then the cheap signature
            // map of the DISTANT rest. The focused file's own body is a separate, sacred
            // segment (see [`Self::focus_segment`]).
            if !self.imports_text.is_empty() {
                segments.push(Segment::user(Zone::Retrieved, self.imports_text.clone()));
            }
            if !self.others_map.is_empty() {
                segments.push(Segment::user(Zone::Retrieved, self.others_map.clone()));
            }
        }
    }

    /// The line-numbered focus files as a SACRED segment (Zone::FocusFile): the file being
    /// edited must never be evicted/clipped, or the model edits a truncated view and can't
    /// anchor its `old_str`. `None` when there is nothing to pin.
    pub(super) fn focus_segment(&self) -> Option<Segment> {
        (!self.focus_text.is_empty())
            .then(|| Segment::user(Zone::FocusFile, self.focus_text.clone()))
    }

    /// The files whose CURRENT contents are pinned IN FULL this turn: the plan doc, and in
    /// focus mode the focus + imported files. A `read_file` of any of these is pure waste —
    /// the content is already shown — so the dispatch short-circuits it (the model re-reads
    /// pinned files reflexively, even its own focus file).
    pub(super) fn pinned_full_files(&self, cfg: &AgentConfig) -> Vec<String> {
        let mut pinned: Vec<String> = self.plan_doc.iter().map(|p| p.name.clone()).collect();
        if !cfg.focus_files.is_empty() {
            pinned.extend(cfg.focus_files.iter().cloned());
            pinned.extend(self.imports.iter().cloned());
        }
        pinned
    }
}

/// The `PLAN-<slug>.md` token named in `instruction`, if any: a feature-plan doc is the
/// SPEC for this run, but it's neither a focus file nor a Python import, so nothing else pins
/// it. Left unpinned, the model reads it every few turns to remember what it's building, then
/// re-reads it once the read scrolls out of the window — the "re-reading the plan over and
/// over" stall. Case-insensitive on the token; returns it as written.
fn plan_token(instruction: &str) -> Option<&str> {
    // Scan whitespace/punctuation-delimited tokens for one shaped like PLAN-<...>.md.
    instruction
        .split(|c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | ','))
        .map(|t| t.trim_end_matches('.')) // a trailing sentence period
        .find(|t| {
            let up = t.to_ascii_uppercase();
            up.starts_with("PLAN-") && up.ends_with(".MD")
        })
}

/// Read and render the plan doc named by `token`, if it exists and is non-empty.
fn read_plan(workspace: &Path, token: &str) -> Option<PinnedPlan> {
    let body = std::fs::read_to_string(workspace.join(token)).ok()?;
    if body.trim().is_empty() {
        return None;
    }
    let mut h = std::hash::DefaultHasher::new();
    body.hash(&mut h);
    Some(PinnedPlan {
        name: token.to_string(),
        text: format!(
            "The feature plan you are implementing (do NOT re-read this — its full contents \
             are here; follow its Approach, Files to touch, and Steps):\n\n=== {token} ===\n\
             {body}\n=== end {token} ===",
        ),
        hash: h.finish(),
    })
}

/// A hash of the bytes of every listed file (an unreadable one hashes as absent), so a
/// re-render can be skipped when nothing in the set moved.
fn content_hash(workspace: &Path, files: &[String]) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    for f in files {
        f.hash(&mut h);
        std::fs::read(workspace.join(f)).ok().hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::super::test_util::temp_dir;
    use super::*;

    fn text_of(segments: &[Segment]) -> String {
        segments.iter().map(|s| s.text.clone()).collect()
    }

    #[test]
    fn referenced_plan_pins_a_named_plan_that_exists() {
        let ws = temp_dir("refplan");
        std::fs::write(
            ws.join("PLAN-lakes.md"),
            "## Plan: lakes\nflood-fill basins",
        )
        .unwrap();
        // Referenced with a trailing period, as the iterate instruction phrases it.
        let token = plan_token("Implement the feature plan in PLAN-lakes.md. Follow it.")
            .expect("plan named");
        assert_eq!(token, "PLAN-lakes.md");
        let plan = read_plan(&ws, token).expect("plan found");
        assert_eq!(plan.name, "PLAN-lakes.md");
        assert!(plan.text.contains("flood-fill"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn referenced_plan_is_none_when_absent_or_unmentioned() {
        let ws = temp_dir("refplan-none");
        std::fs::write(ws.join("PLAN-lakes.md"), "x").unwrap();
        // No plan token in the instruction.
        assert!(plan_token("add error handling to the parser").is_none());
        // Token present but the file doesn't exist.
        assert!(read_plan(&ws, "PLAN-rivers.md").is_none());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn nothing_re_renders_without_a_change_and_only_the_moved_part_with_one() {
        let ws = temp_dir("stable-focus");
        std::fs::write(
            ws.join("app.py"),
            "from store import add\n\ndef main():\n    pass\n",
        )
        .unwrap();
        std::fs::write(ws.join("store.py"), "def add(a, b):\n    return a + b\n").unwrap();
        std::fs::write(ws.join("other.py"), "def other():\n    pass\n").unwrap();
        let cfg = AgentConfig {
            focus_files: vec!["app.py".to_string()],
            ..AgentConfig::default()
        };
        let registry = sc_tools::default_registry();
        let mut stable = StableContext::new(&ws, &cfg, &registry, "edit app.py", String::new());
        let focus0 = stable.focus_segment().expect("focus pinned").text;
        let mut retrieved0 = Vec::new();
        stable.push_retrieved(&cfg, &mut retrieved0);
        assert!(
            text_of(&retrieved0).contains("return a + b"),
            "import pinned"
        );

        // A change is reported but nothing pinned moved (a distant file's BODY changed, no
        // new symbol): byte-identical renders, signature map included.
        std::fs::write(ws.join("other.py"), "def other():\n    return 1\n").unwrap();
        stable.refresh_if_changed(&ws, &cfg, &registry, true);
        assert_eq!(stable.focus_segment().unwrap().text, focus0);
        let mut retrieved1 = Vec::new();
        stable.push_retrieved(&cfg, &mut retrieved1);
        assert_eq!(text_of(&retrieved1)[..], text_of(&retrieved0)[..]);

        // The focus file moved on disk, but the loop reported no change: the cache holds
        // (the loop's flag is the gate), so the render is still the old one...
        std::fs::write(
            ws.join("app.py"),
            "from store import add\n\ndef main():\n    add(1, 2)\n",
        )
        .unwrap();
        stable.refresh_if_changed(&ws, &cfg, &registry, false);
        assert_eq!(stable.focus_segment().unwrap().text, focus0);
        // ...and with the flag, only the focus render moves; the imports are untouched.
        stable.refresh_if_changed(&ws, &cfg, &registry, true);
        let focus2 = stable.focus_segment().unwrap().text;
        assert!(focus2.contains("add(1, 2)"), "{focus2}");
        let mut retrieved2 = Vec::new();
        stable.push_retrieved(&cfg, &mut retrieved2);
        assert_eq!(text_of(&retrieved2)[..], text_of(&retrieved0)[..]);

        // An imported file moved: its body re-renders on its own hash, the focus stays.
        std::fs::write(ws.join("store.py"), "def add(a, b):\n    return b + a\n").unwrap();
        stable.refresh_if_changed(&ws, &cfg, &registry, true);
        assert_eq!(stable.focus_segment().unwrap().text, focus2);
        let mut retrieved3 = Vec::new();
        stable.push_retrieved(&cfg, &mut retrieved3);
        assert!(text_of(&retrieved3).contains("return b + a"));
        assert_eq!(
            stable.pinned_full_files(&cfg),
            vec!["app.py".to_string(), "store.py".to_string()]
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn whole_task_ledger_only_moves_when_a_file_appears() {
        let ws = temp_dir("stable-ledger");
        std::fs::write(ws.join("a.py"), "x = 1\n").unwrap();
        let cfg = AgentConfig::default();
        let registry = sc_tools::default_registry();
        let mut stable = StableContext::new(&ws, &cfg, &registry, "build it", "map".to_string());
        let mut r0 = Vec::new();
        stable.push_retrieved(&cfg, &mut r0);
        let t0 = text_of(&r0);
        assert!(t0.starts_with("map"), "the repo map leads: {t0}");
        assert!(t0.contains("a.py"));

        // An edit to an existing file: the ledger lists the same files, byte-identical.
        std::fs::write(ws.join("a.py"), "x = 2\n").unwrap();
        stable.refresh_if_changed(&ws, &cfg, &registry, true);
        let mut r1 = Vec::new();
        stable.push_retrieved(&cfg, &mut r1);
        assert_eq!(text_of(&r1), t0);

        // A new file: the ledger grows.
        std::fs::write(ws.join("b.py"), "y = 1\n").unwrap();
        stable.refresh_if_changed(&ws, &cfg, &registry, true);
        let mut r2 = Vec::new();
        stable.push_retrieved(&cfg, &mut r2);
        assert!(text_of(&r2).contains("b.py"));
        assert!(
            stable.focus_segment().is_none(),
            "no focus file in whole-task mode"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }
}
