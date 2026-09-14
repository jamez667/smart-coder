//! The agent's panels, as protocol content.
//!
//! Three panels, because the content model has no tabs and no overlay:
//!
//! * **`run`** — the activity stream plus the composer. The feed rows come from
//!   [`crate::view`], which already maps every `AgentEvent` and `SwarmEvent` to a glyph,
//!   a line and an is-error flag — written years before there was a wire to send them
//!   over, and unchanged by this.
//! * **`approvals`** — shell confirmations and workflow gates. Its own panel rather than
//!   an inline card, because **the worker thread is blocked** while one is outstanding:
//!   burying that in a scrolling feed is how a run appears to hang.
//! * **`plan`** — the staged workflow's artifacts, when a run produces them.
//! * **`board`** — one row per swarm subtask, showing its *current* status.
//!
//! The board is the one panel that shows something the run feed structurally cannot.
//! A swarm runs several workers at once and they all emit into one stream, so a
//! subtask's updates arrive scattered between other subtasks'. [`crate::board`] folds
//! that stream by subtask id; this renders the fold. "What is each coder doing right
//! now" is a question the interleaved log cannot answer at a glance.
//!
//! The swarm *topology* — the same events as a diagram, with edges that glow as messages
//! flow — did not survive the port, and has now been deleted rather than left parked. It
//! is a drawing, and the content model is `List`/`Text`/`Form`/`Stack` on purpose: adding
//! a canvas kind to carry one panel is the "widget tree by degrees" spec 25 warns against.
//!
//! Its fold (`topology.rs`) and its renderer (`canvas.rs`) are in the history if the
//! feature is ever wanted back. Keeping them compiled but unreachable would have meant
//! carrying ~680 lines that nothing called and no test could reach through the plugin.
//! The board above covers the question they were really for — which subtask is where.

use sc_plugin_proto::{Content, FormField, ListItem, Severity};

use crate::board::BoardRow;
use crate::view::Row;

/// A decision the run is blocked on, waiting for an answer.
pub struct Ask {
    /// Index into the plugin's pending list — the answer says which one it is for.
    pub id: usize,
    /// What is being asked.
    pub kind: AskKind,
}

/// The two things a run can stop and ask.
pub enum AskKind {
    /// A shell command wants to run. `reason` is the static policy's denial text.
    Confirm { command: String, reason: String },
    /// A workflow phase's artifact wants a checkpoint decision.
    Gate { phase: String, excerpt: String },
}

/// The run panel: what happened, and the box to say what to do next.
pub fn run_panel(rows: &[Row], running: bool, has_workspace: bool, kind_label: &str) -> Content {
    let mut children = Vec::new();

    if rows.is_empty() {
        // Two different empties: "nothing yet" and "working" are different states, and a
        // blank panel is indistinguishable from a broken one.
        children.push(Content::Text {
            markdown: if running {
                "Working…".to_string()
            } else if has_workspace {
                "Describe what you want and press Run.".to_string()
            } else {
                "Open a project first — the agent works in it.".to_string()
            },
        });
    } else {
        // Adjacent rows batch into one list so the host draws them as a block rather than
        // a stack of one-row lists.
        let items: Vec<ListItem> = rows
            .iter()
            .map(|r| {
                let item = ListItem::text(format!("{} {}", r.icon, r.text));
                if r.is_error {
                    item.with_severity(Severity::Error)
                } else {
                    item
                }
            })
            .collect();
        children.push(Content::List { items });
    }

    children.push(Content::Form {
        fields: vec![FormField {
            id: "task".to_string(),
            label: if running {
                "Working…".to_string()
            } else {
                kind_label.to_string()
            },
            value: String::new(),
            placeholder: Some("What should the agent do?".to_string()),
            secret: false,
            // A composer where Enter does not send is materially worse than one that does
            // — the reason the protocol grew this field at all.
            submit_on_enter: true,
        }],
        submit: Some(if running { "Stop" } else { "✦ Run" }.to_string()),
    });

    Content::Stack { children }
}

/// The approvals panel.
///
/// **This is the panel that must never be silently empty while a run is blocked.** The
/// worker is sitting on `reply_rx.recv()`; if nobody answers, the run does not fail, it
/// waits forever — which reads as a hang rather than a question.
pub fn approvals_panel(asks: &[Ask]) -> Content {
    if asks.is_empty() {
        return Content::Text {
            markdown: "Nothing waiting.\n\nShell commands the agent is not already allowed \
                       to run, and workflow checkpoints, appear here."
                .to_string(),
        };
    }

    let mut children = Vec::new();
    for ask in asks {
        match &ask.kind {
            AskKind::Confirm { command, reason } => {
                children.push(Content::Text {
                    markdown: format!("**The agent wants to run:**\n\n```\n{command}\n```"),
                });
                if !reason.trim().is_empty() {
                    children.push(Content::Text {
                        markdown: reason.clone(),
                    });
                }
                children.push(Content::List {
                    items: vec![
                        ListItem::text("✓  Allow once").with_command(
                            "agent.answer",
                            vec![ask.id.to_string(), "allow-once".to_string()],
                        ),
                        // The prefix is the command up to its first space, which is what
                        // "remember `git `" means — the same rule the host's own
                        // approval prompt used.
                        ListItem::text(format!("✓✓  Allow and remember `{}`", prefix_of(command)))
                            .with_command(
                                "agent.answer",
                                vec![ask.id.to_string(), "allow-remember".to_string()],
                            ),
                        ListItem::text("✕  Deny")
                            .with_command(
                                "agent.answer",
                                vec![ask.id.to_string(), "deny".to_string()],
                            )
                            .with_severity(Severity::Error),
                    ],
                });
            }
            AskKind::Gate { phase, excerpt } => {
                children.push(Content::Text {
                    markdown: format!("**Checkpoint: {phase}**"),
                });
                children.push(Content::Text {
                    markdown: excerpt.clone(),
                });
                children.push(Content::List {
                    items: vec![
                        ListItem::text("✓  Approve").with_command(
                            "agent.answer",
                            vec![ask.id.to_string(), "approve".to_string()],
                        ),
                        // Revise means "I edited the file on disk; re-read it" — which is
                        // only meaningful because the runner already wrote the artifact.
                        ListItem::text("✎  I edited it — re-read").with_command(
                            "agent.answer",
                            vec![ask.id.to_string(), "revise".to_string()],
                        ),
                        ListItem::text("✕  Abort the workflow")
                            .with_command(
                                "agent.answer",
                                vec![ask.id.to_string(), "abort".to_string()],
                            )
                            .with_severity(Severity::Error),
                    ],
                });
            }
        }
    }
    Content::Stack { children }
}

/// The plan panel: the staged workflow's artifacts as they land.
pub fn plan_panel(steps: &[(String, bool)]) -> Content {
    if steps.is_empty() {
        return Content::Text {
            markdown: "No plan yet.\n\nThe staged run kinds (Plan, TDD, Staged build) \
                       produce reviewable artifacts here."
                .to_string(),
        };
    }
    Content::List {
        items: steps
            .iter()
            .map(|(name, done)| ListItem::text(format!("{} {name}", if *done { "✓" } else { "·" })))
            .collect(),
    }
}

/// The swarm board: one row per subtask, in first-seen order.
///
/// Every part of a row comes from [`crate::board`], which already decided all of it:
/// the glyph from [`SubtaskStatus::icon`], the one-line description from
/// [`BoardRow::detail`] (retry counts, integrated file lists), and whether it reads as
/// a problem from [`SubtaskStatus::is_bad`]. This function chooses nothing — it maps a
/// fold that is already tested onto `ListItem`, which is why it is this short.
pub fn board_panel(rows: &[BoardRow], running: bool) -> Content {
    if rows.is_empty() {
        return Content::Text {
            markdown: if running {
                "No subtasks yet.\n\nThe orchestrator is still decomposing the task.".to_string()
            } else {
                "No swarm run.\n\nThe Swarm, TDD and Sequential build run kinds \
                 decompose a task across workers, and each one appears here."
                    .to_string()
            },
        };
    }

    Content::List {
        items: rows
            .iter()
            .map(|r| {
                // The goal can be empty: `set_status` creates a bare row when a status
                // event arrives before its `WorkerStarted`, so the event is not lost.
                // Such a row would otherwise render as a lone glyph. The subtask id is
                // what it does have, and it is what the later events key on.
                let label = if r.goal.trim().is_empty() {
                    r.subtask.clone()
                } else {
                    r.goal.clone()
                };
                let item =
                    ListItem::text(format!("{} {label}", r.status.icon())).with_detail(r.detail());
                if r.status.is_bad() {
                    item.with_severity(Severity::Error)
                } else {
                    item
                }
            })
            .collect(),
    }
}

/// The command prefix an "allow and remember" covers: up to and including the first
/// space, or the whole command when it has none.
fn prefix_of(command: &str) -> String {
    match command.find(' ') {
        Some(i) => command[..=i].to_string(),
        None => command.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_of(c: &Content) -> Vec<ListItem> {
        match c {
            Content::List { items } => items.clone(),
            Content::Stack { children } => children.iter().flat_map(rows_of).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn an_empty_approvals_panel_explains_itself() {
        // It must never read as broken: this panel being blank is the normal state.
        let c = approvals_panel(&[]);
        let Content::Text { markdown } = &c else {
            panic!("expected prose");
        };
        assert!(markdown.contains("Nothing waiting"));
    }

    #[test]
    fn a_confirm_offers_all_three_answers_and_names_the_command() {
        let asks = [Ask {
            id: 0,
            kind: AskKind::Confirm {
                command: "git push --force".to_string(),
                reason: "not on the allowlist".to_string(),
            },
        }];
        let c = approvals_panel(&asks);
        let items = rows_of(&c);
        assert_eq!(items.len(), 3, "allow-once, allow-remember, deny");
        assert!(items
            .iter()
            .all(|i| i.command.as_deref() == Some("agent.answer")));
        // Every answer carries the id, or it could answer the wrong blocked worker.
        assert!(items
            .iter()
            .all(|i| i.args.first().map(|s| s.as_str()) == Some("0")));
    }

    #[test]
    fn allow_and_remember_names_the_prefix_it_would_grant() {
        // Granting "git " is a materially different decision from granting this one
        // command, so the row has to say which it is.
        let asks = [Ask {
            id: 3,
            kind: AskKind::Confirm {
                command: "cargo test --workspace".to_string(),
                reason: String::new(),
            },
        }];
        let items = rows_of(&approvals_panel(&asks));
        assert!(
            items.iter().any(|i| i.text.contains("`cargo `")),
            "{:?}",
            items.iter().map(|i| &i.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn prefix_of_handles_a_bare_command() {
        assert_eq!(prefix_of("ls"), "ls");
        assert_eq!(prefix_of("git push"), "git ");
    }

    #[test]
    fn a_gate_offers_approve_revise_abort() {
        let asks = [Ask {
            id: 1,
            kind: AskKind::Gate {
                phase: "Specs".to_string(),
                excerpt: "# Goals".to_string(),
            },
        }];
        let items = rows_of(&approvals_panel(&asks));
        let texts: Vec<_> = items.iter().map(|i| i.text.clone()).collect();
        assert_eq!(texts.len(), 3, "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("Approve")));
        assert!(texts.iter().any(|t| t.contains("re-read")));
        assert!(texts.iter().any(|t| t.contains("Abort")));
    }

    #[test]
    fn two_blocked_asks_get_distinct_ids() {
        // Both workers are blocked; answering one must not answer the other.
        let asks = [
            Ask {
                id: 0,
                kind: AskKind::Confirm {
                    command: "a".to_string(),
                    reason: String::new(),
                },
            },
            Ask {
                id: 1,
                kind: AskKind::Confirm {
                    command: "b".to_string(),
                    reason: String::new(),
                },
            },
        ];
        let items = rows_of(&approvals_panel(&asks));
        let ids: std::collections::BTreeSet<_> = items
            .iter()
            .filter_map(|i| i.args.first().cloned())
            .collect();
        assert_eq!(ids.len(), 2, "each ask answers only itself");
    }

    #[test]
    fn the_run_panel_always_carries_a_composer() {
        let c = run_panel(&[], false, true, "Task");
        let Content::Stack { children } = &c else {
            panic!("expected a stack");
        };
        assert!(
            matches!(children.last(), Some(Content::Form { .. })),
            "the composer is always last"
        );
    }

    #[test]
    fn an_error_row_is_marked_as_one() {
        // A failed step rendering identically to a successful one is a feed that hides
        // its own failures.
        let rows = vec![Row::ok("·", "fine"), Row::err("✗", "broke")];
        let items = rows_of(&run_panel(&rows, false, true, "Task"));
        assert_eq!(items.len(), 2);
        assert!(items[0].severity.is_none());
        assert_eq!(items[1].severity, Some(Severity::Error));
    }

    fn board_row(goal: &str, status: crate::board::SubtaskStatus) -> BoardRow {
        BoardRow {
            subtask: goal.to_string(),
            goal: goal.to_string(),
            status,
            proposal: None,
        }
    }

    /// The empty board explains which run kinds fill it, rather than sitting blank —
    /// an empty panel is indistinguishable from a broken one.
    #[test]
    fn an_empty_board_says_what_would_fill_it() {
        let Content::Text { markdown } = board_panel(&[], false) else {
            panic!("expected prose");
        };
        assert!(markdown.contains("decompose"));
    }

    /// Mid-run emptiness is a different state from no-run emptiness: the orchestrator
    /// has not produced subtasks yet, which is progress, not absence.
    #[test]
    fn an_empty_board_mid_run_says_it_is_still_decomposing() {
        let Content::Text { markdown } = board_panel(&[], true) else {
            panic!("expected prose");
        };
        assert!(markdown.contains("decomposing"));
    }

    /// **The reason this panel exists.** Concurrent workers interleave in the feed;
    /// here each subtask is one row carrying its own current status.
    #[test]
    fn each_subtask_is_one_row_carrying_its_own_status() {
        use crate::board::SubtaskStatus;
        let rows = [
            board_row("build the parser", SubtaskStatus::Running),
            board_row("wire the CLI", SubtaskStatus::Finished),
        ];
        let items = rows_of(&board_panel(&rows, true));
        assert_eq!(items.len(), 2);
        assert!(items[0].text.contains("build the parser"));
        assert!(items[1].text.contains("wire the CLI"));
        assert!(items.iter().all(|i| i.severity.is_none()));
    }

    /// A retry or a revert reads as a problem — the same rule the feed follows, and
    /// the whole point of scanning a board rather than a log.
    #[test]
    fn a_retrying_subtask_reads_as_a_problem_and_shows_why() {
        use crate::board::SubtaskStatus;
        let rows = [board_row(
            "build the parser",
            SubtaskStatus::Retrying {
                attempt: 1,
                max: 2,
                red: 3,
            },
        )];
        let items = rows_of(&board_panel(&rows, true));
        assert_eq!(items[0].severity, Some(Severity::Error));
        assert_eq!(
            items[0].detail.as_deref(),
            Some("retry 1/2 — 3 tests red"),
            "the detail comes from BoardRow::detail, not from this module"
        );
    }

    /// A status event can arrive before its `WorkerStarted` — the board keeps the
    /// event rather than dropping it, leaving a row with no goal text. It must not
    /// render as a bare glyph with nothing beside it.
    #[test]
    fn a_row_with_no_goal_yet_falls_back_to_the_subtask_id() {
        use crate::board::SubtaskStatus;
        let mut row = board_row("", SubtaskStatus::Running);
        row.subtask = "t7".to_string();
        let items = rows_of(&board_panel(&[row], true));
        assert!(items[0].text.contains("t7"), "{}", items[0].text);
    }

    #[test]
    fn an_integrated_subtask_names_the_files_it_changed() {
        use crate::board::SubtaskStatus;
        let rows = [board_row(
            "build the parser",
            SubtaskStatus::Integrated {
                files: vec!["src/parser.rs".to_string()],
            },
        )];
        let items = rows_of(&board_panel(&rows, false));
        assert!(items[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("src/parser.rs"));
        assert!(items[0].severity.is_none(), "integrated is not a problem");
    }

    #[test]
    fn without_a_project_the_empty_run_panel_says_so() {
        let c = run_panel(&[], false, false, "Task");
        let Content::Stack { children } = &c else {
            panic!("expected a stack");
        };
        let Content::Text { markdown } = &children[0] else {
            panic!("expected prose");
        };
        assert!(markdown.contains("Open a project first"));
    }
}
