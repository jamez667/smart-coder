//! The single-agent runs: a plain one-shot loop, and the in-place `Iterate` flow
//! that edits real files under a git safety net.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use sc_core::{AgentEvent, FnSink};
use sc_model::ModelBackend;

use crate::bridge::{ChannelConfirmer, Pending};
use crate::config::UiConfig;

use super::UiEvent;

/// Build the backends + config from `cfg` and drive a single-agent run, forwarding
/// every [`AgentEvent`] to the UI — the mirror of `sc-cli::run_task_json` minus the
/// JSON/log sinks, plus the GUI confirmer (Part A).
pub fn run_agent(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
) {
    // `None` in Craft mode (spec 21): no model is contacted, so the run reports why rather than
    // dialling out. The type is the seam — a caller added later cannot skip this.
    let Some(backend) = cfg.backend() else {
        let _ = ev_tx.send(UiEvent::Failed(crate::chat_session::NO_MODEL.to_string()));
        return;
    };
    let advisor = cfg.advisor();
    let registry = sc_tools::default_registry();
    let strategy = sc_core::select_strategy(&backend.capabilities());
    let confirmer = Arc::new(ChannelConfirmer::new(pending_tx));
    let agent_cfg = cfg.agent_config(Some(confirmer));

    let sink = FnSink(|e: &AgentEvent| {
        let _ = ev_tx.send(UiEvent::Agent(e.clone()));
    });

    let result = sc_core::run_agent_observed(
        &backend,
        advisor.as_ref().map(|a| a as &dyn sc_model::ModelBackend),
        &registry,
        strategy.as_ref(),
        &task,
        &workspace,
        &agent_cfg,
        &sink,
    );

    match result {
        Ok(report) => {
            let summary = if report.finished {
                format!("finished in {} steps", report.steps)
            } else {
                format!("stopped after {} steps (did not finish)", report.steps)
            };
            let _ = ev_tx.send(UiEvent::Done {
                ok: report.finished,
                summary,
            });
        }
        Err(e) => {
            let _ = ev_tx.send(UiEvent::Failed(format!("run failed: {e}")));
        }
    }
}

/// Drive an ITERATE run **safely, via git**: the agent edits the real files (fast — it reuses
/// your `target/` cache for an incremental `cargo check`), but the harness tracks exactly
/// which files it touches, and if the run ends **not green**, those files are `git checkout`-
/// reverted. So a broken/truncated intermediate is never *left* on disk — either you get a
/// verified change, or your files are restored to their committed state. (This replaces a
/// full scratch copy, which would be painfully slow on a large repo.)
///
/// Verify runs on the HOST (`cargo check` needs the real toolchain); nothing is frozen.
pub fn run_iterate(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    ev_tx: Sender<UiEvent>,
    pending_tx: Sender<Pending>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    // `None` in Craft mode (spec 21): no model is contacted, so the run reports why rather than
    // dialling out. The type is the seam — a caller added later cannot skip this.
    let Some(backend) = cfg.backend() else {
        let _ = ev_tx.send(UiEvent::Failed(crate::chat_session::NO_MODEL.to_string()));
        return;
    };
    let advisor = cfg.advisor();
    let registry = sc_tools::default_registry();
    let strategy = sc_core::select_strategy(&backend.capabilities());
    let confirmer = Arc::new(ChannelConfirmer::new(pending_tx));

    let mut agent_cfg = cfg.agent_config(Some(confirmer));
    // The iterate flavor (verify-command detection, no-ceremony overrides) is shared with the
    // remote server via `sc-iterate`, so both front-ends behave identically.
    sc_iterate::apply_iterate_overrides(&mut agent_cfg, &cfg.verify_command, &workspace);
    // Wire the Cancel button's flag so the loop can stop between turns.
    agent_cfg.cancel = Some(cancel);

    let instruction = sc_iterate::iterate_instruction(&task, &workspace);

    // Files that already have uncommitted changes BEFORE this run. We must never auto-revert
    // one of these (that would wipe the user's own work) — only files that were clean.
    let dirty_at_start = sc_iterate::git_dirty_files(&workspace);

    // Track the files the agent edits (from the event stream), so on failure we revert
    // exactly those — not the whole tree — leaving unrelated uncommitted work alone.
    let edited: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>> =
        Default::default();
    let edited_sink = edited.clone();
    let ev_tx_sink = ev_tx.clone();
    let sink = FnSink(move |e: &AgentEvent| {
        if let AgentEvent::ToolCall { tool, arg } = e {
            if matches!(
                tool.as_str(),
                "write_file" | "create_file" | "edit_file" | "append_file"
            ) {
                let p = arg.trim();
                if !p.is_empty() {
                    edited_sink.lock().unwrap().insert(p.replace('\\', "/"));
                }
            }
        }
        let _ = ev_tx_sink.send(UiEvent::Agent(e.clone()));
    });
    let result = sc_core::run_agent_observed(
        &backend,
        advisor.as_ref().map(|a| a as &dyn sc_model::ModelBackend),
        &registry,
        strategy.as_ref(),
        &instruction,
        &workspace,
        &agent_cfg,
        &sink,
    );

    let touched: Vec<String> = edited.lock().unwrap().iter().cloned().collect();

    // Accept-or-revert decision + closing line — shared with the remote server via `sc-iterate`.
    match result {
        Ok(report) => {
            let outcome =
                sc_iterate::finish_summary(&report, &touched, dirty_at_start.as_ref(), &workspace);
            let _ = ev_tx.send(UiEvent::Done {
                ok: outcome.ok,
                summary: outcome.summary,
            });
        }
        Err(e) => {
            // A hard error mid-run: revert the files that were CLEAN before the run (never
            // ones the user had uncommitted work in).
            // Only what we KNOW was clean. `None` means git could not be asked, and
            // treating unknown as clean would revert the user's own uncommitted work
            // -- the exact thing the comment above promises never to do.
            let safe: Vec<String> = match dirty_at_start.as_ref() {
                Some(dirty) => touched
                    .iter()
                    .filter(|f| !dirty.contains(*f))
                    .cloned()
                    .collect(),
                None => Vec::new(),
            };
            sc_iterate::git_revert_files(&workspace, &safe);
            let _ = ev_tx.send(UiEvent::Failed(format!(
                "iterate failed: {e} (reverted {} clean file(s))",
                safe.len()
            )));
        }
    }
}

/// Answer a QUESTION about the code by actually reading it.
///
/// The chat panel had no tools at all: it saw the README, the TODO and whichever file
/// happened to be open, and nothing else. Asked "why is the star trail thin before it gets
/// thick?", it said — correctly and uselessly — "I can't see the jump screen rendering
/// code", guessed three plausible causes, and asked the user to point it at the file. The
/// guess was not the model being stupid; it was the only move available to it.
///
/// This runs the ordinary agent loop over [`sc_tools::read_only_registry`], so the model can
/// search and read its way to the answer but cannot change anything on the way. A question
/// must never edit the workspace as a side effect of being asked.
pub fn investigate(cfg: UiConfig, question: String, workspace: PathBuf, ev_tx: Sender<UiEvent>) {
    let Some(backend) = cfg.backend() else {
        let _ = ev_tx.send(UiEvent::Failed(crate::chat_session::NO_MODEL.to_string()));
        return;
    };
    // Read-only: the loop can look at anything and change nothing.
    let registry = sc_tools::read_only_registry();
    let strategy = sc_core::select_strategy(&backend.capabilities());
    // No confirmer: nothing here can mutate, so there is nothing to approve. Passing one
    // would put a permission prompt in front of reading a file to answer a question.
    let mut agent_cfg = cfg.agent_config(None);
    tune_for_investigation(&mut agent_cfg);

    let sink = FnSink(|e: &AgentEvent| {
        let _ = ev_tx.send(UiEvent::Agent(e.clone()));
    });

    // Say what the loop is FOR. Without this the agent treats a question as a task and
    // reports "finished in 4 steps" instead of answering — the steps were the means, and
    // the user asked for the conclusion.
    // HAND OVER THE MAP rather than making the model discover it.
    //
    // Measured: the first live run spent steps 1-2 on `list_dir .` and `list_dir crates`
    // just learning the repo has 22 crates, then steps 3-7 guessing keywords ("jump",
    // "hyperspace", "trail") because it had no index to consult. Five of eight steps went
    // on orientation that is IDENTICAL for every question and free to compute here.
    //
    // The harness resolves paths; the model never has to invent one. Same rule that
    // removed hallucinated paths elsewhere, and it applies doubly to a small model: a
    // filename it can SEE is a filename it cannot get wrong.
    let map = sc_tools::source_files(&workspace);
    let map_block = if map.is_empty() {
        String::new()
    } else {
        // Capped: a huge repo would otherwise spend the whole prompt budget on a listing
        // and leave no room for the file contents the answer actually needs.
        const MAX: usize = 800;
        let mut b = format!(
            "\nThe source files in this project ({} of {}):\n",
            map.len().min(MAX),
            map.len()
        );
        for f in map.iter().take(MAX) {
            b.push_str("  ");
            b.push_str(f);
            b.push('\n');
        }
        if map.len() > MAX {
            b.push_str("  ... (truncated; use search_code for anything not listed)\n");
        }
        b
    };

    // Say what the loop is FOR. Without this the agent treats a question as a task and
    // reports "finished in 4 steps" instead of answering -- the steps were the means, and
    // the user asked for the conclusion.
    let task = format!(
        "Answer this question about the code in this project:\n\n{question}\n{map_block}\n\
         Pick the likely file from the list above and read it -- do NOT spend turns listing \
         directories, and do not guess at file names or line numbers. When you know the \
         answer, call `finish` with a short explanation naming the file and line, and say \
         what the fix would be. You cannot edit anything here; the user applies changes \
         themselves."
    );

    let result = sc_core::run_agent_observed(
        &backend,
        None,
        &registry,
        strategy.as_ref(),
        &task,
        &workspace,
        &agent_cfg,
        &sink,
    );

    match result {
        Ok(report) => {
            let _ = ev_tx.send(UiEvent::Done {
                ok: report.finished,
                summary: if report.finished {
                    format!("answered after reading {} step(s)", report.steps)
                } else {
                    format!("stopped after {} steps without an answer", report.steps)
                },
            });
        }
        Err(e) => {
            let _ = ev_tx.send(UiEvent::Failed(format!("investigation failed: {e}")));
        }
    }
}

/// Shape a build-agent config for ANSWERING A QUESTION.
///
/// Split out from `investigate` so it can be asserted without a live backend: every value
/// here was set because a live run went wrong without it.
pub(crate) fn tune_for_investigation(agent_cfg: &mut sc_core::AgentConfig) {
    // A question is not a build.
    //
    // Measured on the first live run: the loop found the right file in 8 steps and then
    // spent 9 more without concluding. Two causes, both from inheriting a BUILD config:
    //
    // * 40 steps is a licence to keep looking. Reading is cheap and finishing is not
    //   forced, so the model kept opening one more file. A tighter ceiling makes the
    //   budget visible and pushes it to answer with what it has.
    // * `verify_command` tells the loop there is a build to run, and this registry has no
    //   `run_verification` to run it with — steering a model toward a tool it was not
    //   offered is the `ToolNotOffered` failure, and it burns turns.
    agent_cfg.max_steps = 14;
    agent_cfg.verify_command = None;
    agent_cfg.plan_first = false;
    // Room to answer.
    //
    // Observed live: the reply hit the 6144-token cap after 23,019 chars and raised a
    // ReplyTruncated fault. A question is answered in PROSE, and prose is far longer than
    // the `{"tool":...}` JSON the default cap was sized for -- an answer that names a file,
    // a line and a fix does not fit in a budget tuned for tool calls.
    agent_cfg.response_reserve_tokens = 12288;
    // `SC_INVESTIGATE_VERBOSE=1` emits the fully-assembled prompt each turn, so a probe run
    // can be read back as "what the model actually saw" rather than guessed at from the
    // tool calls it made. Off unless asked: the payload is large.
    if std::env::var_os("SC_INVESTIGATE_VERBOSE").is_some() {
        agent_cfg.verbose = true;
    }
    // Keep the reasoning OUT of the reply.
    //
    // Measured: one turn came back as 44,981 characters of "Let me analyze... Wait, let me
    // re-read... So the first segment goes from..." -- the model thinking in plain prose
    // until the cap cut it off, costing the turn. This is the exact quirk `system_suffix`
    // exists for, and the investigate path was not setting it.
    //
    // Only when the caller has not chosen one: a user-set suffix is a deliberate override
    // and must win.
    if agent_cfg.system_suffix.is_none() {
        agent_cfg.system_suffix = Some("/no_think".to_string());
    }
}

#[cfg(test)]
mod investigation_config {
    use super::*;

    /// **Every one of these was set because a live run went wrong without it.**
    ///
    /// The suffix is the load-bearing one: without it a turn came back as 44,981 characters
    /// of "Let me analyze... Wait, let me re-read..." -- the model reasoning in plain prose
    /// until the token cap cut it off, which costs the whole turn and raises a
    /// ReplyTruncated fault.
    #[test]
    fn a_question_is_not_configured_like_a_build() {
        let mut cfg = sc_core::AgentConfig {
            verify_command: Some("cargo test".into()),
            plan_first: true,
            ..sc_core::AgentConfig::default()
        };
        tune_for_investigation(&mut cfg);

        assert_eq!(cfg.system_suffix.as_deref(), Some("/no_think"));
        assert_eq!(
            cfg.verify_command, None,
            "no run_verification exists in a read-only registry to satisfy it"
        );
        assert!(!cfg.plan_first, "a question needs an answer, not a plan");
        assert_eq!(cfg.max_steps, 14, "40 steps is a licence to keep looking");
        assert!(
            cfg.response_reserve_tokens >= 12288,
            "a prose answer does not fit a budget tuned for tool-call JSON"
        );
    }

    /// A caller's own suffix is a deliberate override and must win.
    #[test]
    fn a_chosen_suffix_is_not_overwritten() {
        let mut cfg = sc_core::AgentConfig {
            system_suffix: Some("/think".into()),
            ..sc_core::AgentConfig::default()
        };
        tune_for_investigation(&mut cfg);
        assert_eq!(cfg.system_suffix.as_deref(), Some("/think"));
    }
}
