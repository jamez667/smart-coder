//! Building the two panels' content.
//!
//! Pure: state in, [`Content`] out, no I/O and no process. So what the panel looks like in
//! each state is testable without running `claude` at all — which is the same split
//! [`crate::stream`] makes, for the same reason.
//!
//! # What the port lost, and what it did not
//!
//! The host's panel painted three row shapes: an accent bar and a lifted background for
//! the user's own turn, flat text for tool calls, and indented markdown for prose. The
//! content model has no colours and no backgrounds, so the user's turn is marked with a
//! `❯` prefix inside the markdown instead. That is a real, small loss.
//!
//! What did *not* get lost is the prose rendering, which is what people actually read: a
//! `Text` block goes through the host's own markdown renderer — the very same function
//! the old panel called. Headings, bullets and code fences look exactly as they did.

use sc_plugin_proto::{Content, FormField, ListItem, Severity};

use crate::options::{Options, Permission};
use crate::stream::Event;

/// One line of the feed, in the plugin's own terms.
///
/// Kept as a row rather than as raw [`Event`]s because two of them collapse: a tool call
/// and its result are one row that gains an outcome, not two rows. The old panel did the
/// same thing in `logic_c.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// What the user asked for.
    You(String),
    /// What the model said. Markdown.
    Said(String),
    /// A tool call.
    Called { tool: String, arg: String },
    /// A tool's answer.
    Result { summary: String, is_error: bool },
    /// The run ended.
    Finished { ok: bool, summary: String },
    /// The host talking, not the model: a cancel, a format warning.
    Note(String),
    /// Something went wrong.
    Error(String),
}

impl Row {
    pub fn you(task: &str) -> Self {
        Row::You(task.to_string())
    }
    pub fn note(text: &str) -> Self {
        Row::Note(text.to_string())
    }
    pub fn error(text: &str) -> Self {
        Row::Error(text.to_string())
    }
    pub fn finished(ok: bool, summary: &str) -> Self {
        Row::Finished {
            ok,
            summary: summary.to_string(),
        }
    }

    /// Turn a translated stream event into a row.
    pub fn from_event(e: Event) -> Self {
        match e {
            // `Started` is not shown. The user pressed Run; telling them it started is a
            // row that says nothing, and the old panel dropped it for the same reason.
            Event::Started => Row::Note(String::new()),
            Event::Said(raw) => Row::Said(raw),
            Event::Called { tool, arg } => Row::Called { tool, arg },
            Event::Result {
                summary, is_error, ..
            } => Row::Result { summary, is_error },
        }
    }
}

/// The feed panel: the run, then the composer.
pub fn feed(rows: &[Row], running: bool, has_workspace: bool) -> Content {
    let mut children = Vec::new();

    if rows
        .iter()
        .all(|r| matches!(r, Row::Note(n) if n.is_empty()))
    {
        // An empty panel should say what it is for rather than sit blank. Two different
        // empties, because "nothing yet" and "working" are different states.
        children.push(Content::Text {
            markdown: if running {
                "Working…".to_string()
            } else if has_workspace {
                "Describe a task below and press Run.\n\nClaude Code works in this project folder."
                    .to_string()
            } else {
                "Open a project first — Claude Code runs in it.".to_string()
            },
        });
    } else {
        // Prose becomes markdown; everything else becomes list rows. Runs of adjacent
        // list rows are batched into one `List` so the host draws them as a block rather
        // than as a stack of one-row lists.
        let mut pending: Vec<ListItem> = Vec::new();
        for r in rows {
            match r {
                Row::Said(raw) => {
                    flush(&mut pending, &mut children);
                    children.push(Content::Text {
                        markdown: raw.clone(),
                    });
                }
                Row::You(task) => {
                    flush(&mut pending, &mut children);
                    // The one place the port shows: an accent bar is not expressible, so
                    // the turn is marked in the markdown itself.
                    children.push(Content::Text {
                        markdown: format!("**❯ {task}**"),
                    });
                }
                Row::Called { tool, arg } => {
                    pending.push(ListItem::text(arg.clone()).with_detail(tool.clone()));
                }
                Row::Result { summary, is_error } => {
                    if summary.trim().is_empty() && !*is_error {
                        continue;
                    }
                    let mut item = ListItem::text(summary.clone());
                    if *is_error {
                        item = item.with_severity(Severity::Error);
                    }
                    pending.push(item);
                }
                Row::Finished { ok, summary } => {
                    flush(&mut pending, &mut children);
                    children.push(Content::Text {
                        markdown: format!("{} {summary}", if *ok { "✓" } else { "✗" }),
                    });
                }
                Row::Note(n) if n.is_empty() => {}
                Row::Note(n) => pending.push(ListItem::text(n.clone())),
                Row::Error(e) => {
                    pending.push(ListItem::text(e.clone()).with_severity(Severity::Error));
                }
            }
        }
        flush(&mut pending, &mut children);
    }

    // The composer, last. `submit_on_enter` is why the protocol needed a v2: a composer
    // where Enter does not send is materially worse than the one it replaced.
    children.push(Content::Form {
        fields: vec![FormField {
            id: "task".to_string(),
            label: if running {
                "Working…".to_string()
            } else {
                "Task".to_string()
            },
            value: String::new(),
            placeholder: Some("What should Claude Code do?".to_string()),
            secret: false,
            submit_on_enter: true,
        }],
        submit: Some(if running {
            "Stop".to_string()
        } else {
            "✦ Run".to_string()
        }),
    });

    Content::Stack { children }
}

/// Move any accumulated rows into the output as one list.
fn flush(pending: &mut Vec<ListItem>, out: &mut Vec<Content>) {
    if !pending.is_empty() {
        out.push(Content::List {
            items: std::mem::take(pending),
        });
    }
}

/// The options panel.
///
/// Every setting is a clickable row that cycles, rather than a dropdown: the content model
/// has no dropdown, and a row whose `detail` shows the current value and whose click
/// advances it is both expressible and — for three or four options — faster to use.
pub fn options(opts: &Options, workspace: Option<&std::path::Path>) -> Content {
    let mut items = vec![
        ListItem::text("Model")
            .with_detail(opts.model.label())
            .with_command("claude.cycle-model", Vec::new()),
        ListItem::text("Permission")
            .with_detail(opts.permission.label())
            .with_command("claude.cycle-permission", Vec::new()),
        ListItem::text("Continue last session")
            .with_detail(if opts.continue_session { "on" } else { "off" })
            .with_command("claude.toggle-continue", Vec::new()),
    ];

    if let Some(id) = &opts.resume_session {
        // A resumed session overrides `--continue`, so say which one rather than leaving
        // the user to wonder why "continue" looks off.
        items.push(ListItem::text("Resuming").with_detail(short_id(id)));
    }

    for (i, d) in opts.add_dirs.iter().enumerate() {
        items.push(
            ListItem::text(format!("✕  {d}"))
                .with_command("claude.remove-dir", vec![i.to_string()]),
        );
    }

    let mut children = vec![Content::List { items }];

    // Past sessions to resume, newest first. Only when there is a project: the CLI keys
    // its logs by workspace path.
    if let Some(ws) = workspace {
        let sessions = crate::options::sessions(ws);
        if !sessions.is_empty() {
            children.push(Content::Text {
                markdown: "**Resume a session**".to_string(),
            });
            children.push(Content::List {
                items: sessions
                    .into_iter()
                    .take(12)
                    .map(|s| {
                        ListItem::text(s.summary)
                            .with_detail(s.age)
                            .with_command("claude.resume", vec![s.id])
                    })
                    .collect(),
            });
        }
    }

    // Tool restrictions are free text, because the patterns are (`Bash(git *)`).
    children.push(Content::Form {
        fields: vec![
            FormField {
                id: "allowed".to_string(),
                label: "Allowed tools".to_string(),
                value: opts.allowed_tools.join(" "),
                placeholder: Some("empty = no restriction".to_string()),
                secret: false,
                submit_on_enter: false,
            },
            FormField {
                id: "disallowed".to_string(),
                label: "Disallowed tools".to_string(),
                value: opts.disallowed_tools.join(" "),
                placeholder: Some("empty = nothing forbidden".to_string()),
                secret: false,
                submit_on_enter: false,
            },
        ],
        submit: Some("Save".to_string()),
    });

    // Stated, not hidden: `bypassPermissions` is deliberately absent from the cycle, and
    // someone looking for it deserves to know that rather than assume it is a bug.
    if opts.permission == Permission::Default {
        children.push(Content::Text {
            markdown: "_Claude Code asks about its own permissions. There is no bypass here._"
                .to_string(),
        });
    }

    Content::Stack { children }
}

/// A session id, shortened for display.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stack(c: &Content) -> &Vec<Content> {
        match c {
            Content::Stack { children } => children,
            other => panic!("expected a stack, got {other:?}"),
        }
    }

    /// An empty feed says what the panel is for. A blank panel is indistinguishable from
    /// a broken one.
    #[test]
    fn an_empty_feed_explains_itself() {
        let c = feed(&[], false, true);
        let Content::Text { markdown } = &stack(&c)[0] else {
            panic!("expected text");
        };
        assert!(markdown.contains("Describe a task"), "{markdown}");
    }

    /// With no project open the panel says so, rather than inviting a task it cannot run.
    #[test]
    fn with_no_project_the_feed_says_to_open_one() {
        let c = feed(&[], false, false);
        let Content::Text { markdown } = &stack(&c)[0] else {
            panic!("expected text");
        };
        assert!(markdown.contains("Open a project"), "{markdown}");
    }

    /// **The composer always exists**, whatever the feed is doing — it is the only way to
    /// start a run.
    #[test]
    fn the_composer_is_always_present_and_submits_on_enter() {
        for rows in [vec![], vec![Row::you("do a thing")]] {
            let c = feed(&rows, false, true);
            let last = stack(&c).last().expect("a composer");
            let Content::Form { fields, submit } = last else {
                panic!("expected a form, got {last:?}");
            };
            assert!(fields[0].submit_on_enter, "Enter must send");
            assert_eq!(submit.as_deref(), Some("✦ Run"));
        }
    }

    /// Adjacent tool rows batch into ONE list, so the host draws them as a block rather
    /// than as a stack of single-row lists.
    #[test]
    fn adjacent_tool_rows_batch_into_one_list() {
        let rows = vec![
            Row::Called {
                tool: "Read".into(),
                arg: "a.rs".into(),
            },
            Row::Called {
                tool: "Edit".into(),
                arg: "b.rs".into(),
            },
        ];
        let c = feed(&rows, false, true);
        let lists: Vec<_> = stack(&c)
            .iter()
            .filter(|c| matches!(c, Content::List { .. }))
            .collect();
        assert_eq!(lists.len(), 1, "one list, not two");
        let Content::List { items } = lists[0] else {
            unreachable!()
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].detail.as_deref(), Some("Read"));
    }

    /// Prose breaks the batch, because markdown is its own block.
    #[test]
    fn prose_splits_the_batches_around_it() {
        let rows = vec![
            Row::Called {
                tool: "Read".into(),
                arg: "a.rs".into(),
            },
            Row::Said("Now I will edit it.".into()),
            Row::Called {
                tool: "Edit".into(),
                arg: "a.rs".into(),
            },
        ];
        let c = feed(&rows, false, true);
        let kinds: Vec<&str> = stack(&c)
            .iter()
            .map(|c| match c {
                Content::List { .. } => "list",
                Content::Text { .. } => "text",
                Content::Form { .. } => "form",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["list", "text", "list", "form"]);
    }

    /// **A failed tool call must not read like a successful one.** This is what
    /// `severity` was added to v2 for.
    #[test]
    fn a_failed_tool_result_carries_error_severity() {
        let rows = vec![Row::Result {
            summary: "No such file".into(),
            is_error: true,
        }];
        let c = feed(&rows, false, true);
        let Content::List { items } = &stack(&c)[0] else {
            panic!("expected a list");
        };
        assert_eq!(items[0].severity, Some(Severity::Error));
    }

    /// A successful result carries no severity, so the common case stays plain.
    #[test]
    fn a_successful_result_is_plain() {
        let rows = vec![Row::Result {
            summary: "ok".into(),
            is_error: false,
        }];
        let c = feed(&rows, false, true);
        let Content::List { items } = &stack(&c)[0] else {
            panic!("expected a list");
        };
        assert_eq!(items[0].severity, None);
    }

    /// The user's own turn is marked, since an accent bar is not expressible.
    #[test]
    fn the_users_turn_is_marked_in_the_markdown() {
        let c = feed(&[Row::you("fix the parser")], false, true);
        let Content::Text { markdown } = &stack(&c)[0] else {
            panic!("expected text");
        };
        assert!(markdown.contains('❯'), "{markdown}");
        assert!(markdown.contains("fix the parser"), "{markdown}");
    }

    /// `Started` is not a row: the user pressed Run, and telling them it started says
    /// nothing.
    #[test]
    fn the_started_event_produces_no_visible_row() {
        assert_eq!(Row::from_event(Event::Started), Row::Note(String::new()));
        let c = feed(&[Row::from_event(Event::Started)], false, true);
        // Still the empty state, because the only row is invisible.
        let Content::Text { markdown } = &stack(&c)[0] else {
            panic!("expected the empty-state text");
        };
        assert!(markdown.contains("Describe a task"));
    }

    /// While running, the button stops rather than starting a second run.
    #[test]
    fn a_running_feed_offers_stop() {
        let c = feed(&[Row::you("x")], true, true);
        let Content::Form { submit, .. } = stack(&c).last().unwrap() else {
            panic!("expected a form");
        };
        assert_eq!(submit.as_deref(), Some("Stop"));
    }

    /// Every option is a clickable row that cycles — the content model has no dropdown,
    /// and for three or four values a cycling row is faster anyway.
    #[test]
    fn the_options_panel_offers_cycling_rows() {
        let c = options(&Options::default(), None);
        let Content::List { items } = &stack(&c)[0] else {
            panic!("expected a list");
        };
        let commands: Vec<_> = items.iter().filter_map(|i| i.command.as_deref()).collect();
        assert!(commands.contains(&"claude.cycle-model"));
        assert!(commands.contains(&"claude.cycle-permission"));
        assert!(commands.contains(&"claude.toggle-continue"));
    }

    /// The absence of a bypass is STATED, not hidden — someone looking for it deserves to
    /// know it is a decision rather than a gap.
    #[test]
    fn the_missing_bypass_is_stated() {
        let c = options(&Options::default(), None);
        let said = stack(&c)
            .iter()
            .any(|c| matches!(c, Content::Text { markdown } if markdown.contains("no bypass")));
        assert!(said, "the options panel must say there is no bypass");
    }
}
