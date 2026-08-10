//! The `ClaudeCode` run kind: drive the `claude` CLI and stream its events (spec 22).
//!
//! Unlike every other module here, this one runs **no agent loop of its own** — Claude Code
//! owns the loop, the tools and the edits. This is a subprocess reader: spawn, translate each
//! line through [`crate::claudecode`], forward. All the format knowledge lives there so it can
//! be tested without a child process; what lives here is the process handling, which cannot.

use std::io::BufRead;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use crate::bridge::Pending;
use crate::claudecode::{self, Line};
use crate::config::UiConfig;

use super::UiEvent;

/// Run Claude Code over `task` in `workspace`, streaming its events to the UI.
///
/// `pending` is accepted and deliberately unused: v1 uses **delegated** approvals, where Claude
/// Code handles its own permission prompts (spec 22). Taking the parameter keeps the signature
/// uniform with every other run kind, and is where routed approvals would arrive later.
pub(super) fn run_claude_code(
    cfg: UiConfig,
    task: String,
    workspace: PathBuf,
    tx: Sender<UiEvent>,
    _pending: Sender<Pending>,
    cancel: Arc<AtomicBool>,
) {
    // Craft mode contacts no model, and Claude Code is unambiguously a model surface — the
    // same reasoning that refuses the remote mirror. Belt and braces: the run kind is not
    // offered in the UI, but a queued Task or a stale message can arrive after a mode switch.
    if cfg.craft() {
        let _ = tx.send(UiEvent::Failed(
            "Craft mode contacts no model, so Claude Code is not available (Settings ▸ General)."
                .to_string(),
        ));
        return;
    }

    let mut child = match crate::proc::command("claude")
        .args(claudecode::args(&task, &cfg.claude))
        .current_dir(&workspace)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            // Distinguish "not installed" from every other spawn failure: the first is a thing
            // the user can fix, and saying "program not found" alone doesn't tell them how.
            let msg = if e.kind() == std::io::ErrorKind::NotFound {
                "Claude Code is not installed, or `claude` is not on PATH.".to_string()
            } else {
                format!("Could not start Claude Code: {e}")
            };
            let _ = tx.send(UiEvent::Failed(msg));
            return;
        }
    };

    let Some(stdout) = child.stdout.take() else {
        let _ = tx.send(UiEvent::Failed(
            "Claude Code started but produced no output stream.".to_string(),
        ));
        let _ = child.kill();
        return;
    };
    // stderr is drained on its own thread. Without this a chatty failure fills the pipe buffer
    // and the child blocks forever writing to it — a hang that looks like a stuck run.
    let stderr_handle = child.stderr.take().map(|err| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                if buf.len() < 4096 {
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }
            buf
        })
    });

    let mut done = None;
    let mut skipped = 0usize;
    let mut cancelled = false;

    for line in std::io::BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
    {
        // Cancel by KILLING THE CHILD. The shared flag is cooperative and every other run kind
        // checks it at a turn boundary; a subprocess has no turn boundary, and a cancel that
        // left an orphaned `claude` still editing files would be worse than no cancel button,
        // because the user would believe they had stopped it.
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            cancelled = true;
            break;
        }
        for parsed in claudecode::parse_line_in(&line, &workspace) {
            match parsed {
                Line::Event(ev) => {
                    let _ = tx.send(UiEvent::Agent(ev));
                }
                Line::Done { ok, summary } => done = Some((ok, summary)),
                // Known-and-uninteresting: not counted. Counting these would report "lines
                // skipped" on every run, and a warning that always fires is one nobody reads.
                Line::Ignored => {}
                Line::Unknown => skipped += 1,
            }
        }
    }

    let status = child.wait();
    let stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    if cancelled {
        let _ = tx.send(UiEvent::Done {
            ok: false,
            summary: "Cancelled — the Claude Code process was stopped.".to_string(),
        });
        return;
    }

    // Prefer Claude Code's OWN verdict from the result line: it knows why it stopped, and the
    // exit code alone cannot tell "ran out of turns" from "crashed".
    if let Some((ok, summary)) = done {
        // Skipped lines are reported, not swallowed — a format drift that silently halves the
        // activity feed should be visible rather than looking like a quiet run.
        let summary = if skipped > 0 {
            format!("{summary}\n\n({skipped} unrecognised output line(s) skipped.)")
        } else {
            summary
        };
        let _ = tx.send(UiEvent::Done { ok, summary });
        return;
    }

    // No result line: the process died before finishing. Say what we know.
    let detail = match status {
        Ok(s) if s.success() => "Claude Code exited without reporting a result.".to_string(),
        Ok(s) => format!(
            "Claude Code exited with {}{}",
            s.code()
                .map(|c| format!("code {c}"))
                .unwrap_or_else(|| "a signal".to_string()),
            if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", stderr.trim())
            }
        ),
        Err(e) => format!("Claude Code could not be waited on: {e}"),
    };
    let _ = tx.send(UiEvent::Failed(detail));
}
