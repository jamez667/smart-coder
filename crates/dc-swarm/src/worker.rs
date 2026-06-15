//! A swarm worker (spec 08 — "each worker IS a `dumb-coder` agent loop").
//!
//! A worker runs the unchanged `dc_core` agent loop against a **scratch copy** of
//! the workspace, scoped to one subtask. It never touches the real workspace;
//! instead it returns the set of file changes it *proposes* (a [`ProposedChange`]
//! per file). The orchestrator later applies accepted proposals to the real
//! workspace one at a time (serialized writes, spec 08).

use std::path::Path;

use dc_core::AgentConfig;
use dc_model::ModelBackend;

use crate::board::Subtask;

/// One file the worker proposes to change. `after == None` means delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedChange {
    pub path: String,
    pub after: Option<String>,
}

/// The outcome of one worker running one subtask.
///
/// Division of labour (spec 08): a tiny worker is good at *reasoning* about the
/// fix but bad at the mechanical exactness of applying it (exact `old_str`
/// anchors, whitespace, merging). So the worker hands back its fix **as text** —
/// the corrected file and/or a description — and the smarter orchestrator turns
/// that into the actual file change. `proposal` is that text answer.
#[derive(Debug, Clone)]
pub struct WorkerResult {
    pub subtask_id: String,
    /// The files this subtask was scoped to (so the orchestrator knows what the
    /// proposal applies to).
    pub files: Vec<String>,
    /// The worker's fix, in its own words/code — handed to the orchestrator to
    /// merge. Empty if the worker produced nothing usable.
    pub proposal: String,
    pub report_summary: String,
}

impl WorkerResult {
    pub fn has_proposal(&self) -> bool {
        !self.proposal.trim().is_empty()
    }
}

/// Run `subtask` on a worker `backend`: a SINGLE model call that returns the fix
/// as text (the corrected file). The worker never touches the filesystem — it
/// reasons, the orchestrator applies (spec 08). `advisor`/`cfg` are accepted for
/// signature stability but a one-shot proposer doesn't use them.
pub fn run_worker(
    backend: &dyn ModelBackend,
    advisor: Option<&dyn ModelBackend>,
    subtask: &Subtask,
    workspace: &Path,
    cfg: &AgentConfig,
) -> WorkerResult {
    run_worker_with_feedback(backend, advisor, subtask, workspace, cfg, None)
}

/// Like [`run_worker`], but for a **retry** (spec 08 — subtask retry): the prompt is
/// augmented with `feedback` — the still-failing test names + assertion messages and
/// the current (already-merged) file contents — so the worker fixes *what's still
/// wrong* rather than re-deriving from scratch. `feedback == None` is the first
/// attempt (identical to [`run_worker`]).
pub fn run_worker_with_feedback(
    backend: &dyn ModelBackend,
    _advisor: Option<&dyn ModelBackend>,
    subtask: &Subtask,
    workspace: &Path,
    _cfg: &AgentConfig,
    feedback: Option<&str>,
) -> WorkerResult {
    let prompt = propose_prompt_with_feedback(subtask, workspace, feedback);
    let req = dc_model::GenerateRequest::new(vec![
        dc_model::Message::system(PROPOSER_SYSTEM),
        dc_model::Message::user(prompt),
    ]);

    let (proposal, summary) = match backend.generate(&req) {
        Ok(resp) => {
            let p = strip_code_fence(&resp.content);
            let words = p.split_whitespace().count();
            (p, format!("proposed a fix ({words} words)"))
        }
        Err(e) => (String::new(), format!("worker errored: {e}")),
    };

    WorkerResult {
        subtask_id: subtask.id.clone(),
        files: subtask.files.clone(),
        proposal,
        report_summary: summary,
    }
}

/// System prompt for a worker: it's a reasoner, not an editor.
// The trailing `/no_think` is load-bearing on Qwen3-class workers: without it the
// model emits its chain-of-thought *as the reply*, which then gets written into the
// source file as garbage (observed live 2026-06-14 — roman.py filled with "Okay,
// let's see…" reasoning, breaking collection). A proposer only ever wants the final
// file, never the thinking, so suppression belongs in the prompt itself. (Decompose,
// by contrast, *needs* to think — see decompose.rs — so it omits the suffix.)
const PROPOSER_SYSTEM: &str = "You fix code. You are shown a file and a task. \
Reply with the COMPLETE corrected file and nothing else — no explanation, no \
markdown fences, just the full file contents with your fix applied. Do not change \
anything the task doesn't require. /no_think";

/// Build the worker's single-shot prompt: the task plus the current contents of
/// the files it must fix. Only the named files are shown (the subtask is scoped).
#[cfg(test)]
fn propose_prompt(subtask: &Subtask, workspace: &Path) -> String {
    propose_prompt_with_feedback(subtask, workspace, None)
}

/// Build the worker's single-shot prompt. On a retry, `feedback` (still-failing test
/// names + assertion messages) is woven in *before* the file contents so the worker
/// sees what's still wrong against the current — already-merged — code (spec 08).
pub(crate) fn propose_prompt_with_feedback(
    subtask: &Subtask,
    workspace: &Path,
    feedback: Option<&str>,
) -> String {
    let mut s = format!("Task: {}\n", subtask.goal);
    if let Some(fb) = feedback {
        s.push_str(&format!(
            "\nA previous attempt was incomplete — these tests are STILL failing:\n{fb}\n\
             \nThe file below already contains that previous attempt. Fix what's still \
             wrong so every test passes.\n"
        ));
    }
    for f in &subtask.files {
        if let Ok(content) = std::fs::read_to_string(workspace.join(f)) {
            let content = content.replace("\r\n", "\n");
            s.push_str(&format!("\nFile {f}:\n{content}\n"));
        }
    }
    s.push_str("\nReply with the complete corrected file.");
    s
}

/// Strip a leading/trailing ``` fence (with optional language tag) a model often
/// wraps code in, so the proposal is the raw file body.
fn strip_code_fence(s: &str) -> String {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t.to_string();
    };
    // Drop the first line (``` or ```lang) and a trailing ``` if present.
    let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
    rest.trim_end()
        .strip_suffix("```")
        .unwrap_or(rest)
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dc_model::MockBackend;

    fn temp(tag: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!("dc-swarm-wt-{tag}-{n}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn worker_returns_the_corrected_file_as_text() {
        let ws = temp("propose");
        std::fs::write(ws.join("m.py"), "def double(n):\n    return n\n").unwrap();
        // The proposer returns the whole corrected file (wrapped in a fence, which
        // we strip) — no tools, no filesystem writes.
        let backend = MockBackend::new(["```python\ndef double(n):\n    return n * 2\n```"]);
        let subtask = Subtask::new("t1", "fix double").with_files(vec!["m.py".into()]);
        let result = run_worker(&backend, None, &subtask, &ws, &AgentConfig::default());

        assert!(result.has_proposal());
        assert_eq!(result.proposal, "def double(n):\n    return n * 2");
        assert_eq!(result.files, vec!["m.py"]);
        // The REAL workspace is untouched — the worker only proposes.
        assert_eq!(
            std::fs::read_to_string(ws.join("m.py")).unwrap(),
            "def double(n):\n    return n\n"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn strip_code_fence_handles_plain_and_fenced() {
        assert_eq!(strip_code_fence("just text"), "just text");
        assert_eq!(strip_code_fence("```\ncode\n```"), "code");
        assert_eq!(strip_code_fence("```py\na\nb\n```"), "a\nb");
    }

    #[test]
    fn propose_prompt_inlines_the_named_file() {
        let ws = temp("prompt");
        std::fs::write(ws.join("a.py"), "x = 1\n").unwrap();
        let subtask = Subtask::new("t", "do it").with_files(vec!["a.py".into()]);
        let p = propose_prompt(&subtask, &ws);
        assert!(p.contains("Task: do it"));
        assert!(p.contains("File a.py:"));
        assert!(p.contains("x = 1"));
        let _ = std::fs::remove_dir_all(&ws);
    }
}
