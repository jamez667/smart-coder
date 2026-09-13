//! The compliance panel, as protocol content.
//!
//! Ported from the iced dialog this crate carries as `view_comply.rs.pending`. The
//! widgets are gone; every *decision* in it survives, because each one was made for a
//! reason the content model does not change:
//!
//! * **The model picker is disabled mid-run.** Changing it would not affect the audit
//!   already in flight, so offering it would be a lie.
//! * **The caveat is the reason the panel exists.** Firing the audit straight off a menu
//!   item would have been less code, but there would be nowhere honest to say what a
//!   model does and does not do to a compliance document.
//! * **Counts, never a headline percentage.** The reports themselves follow this rule:
//!   "78% compliant" is precisely the misreading the whole feature exists to avoid.
//! * **An unnarrated report says so.** If the user asked for a summary and the model did
//!   not return one, the panel states it rather than letting them assume the page has one.

use sc_plugin_proto::{Content, ListItem, Severity};

use crate::comply::{ComplyModel, ComplyReport};

/// What the panel is showing right now.
pub struct View<'a> {
    pub workspace: Option<&'a std::path::Path>,
    pub model: ComplyModel,
    pub running: bool,
    pub result: Option<&'a Result<ComplyReport, String>>,
}

/// The whole panel.
pub fn panel(v: &View<'_>) -> Content {
    let mut children = vec![Content::Text {
        markdown: format!(
            "Audits this project against all {} shipped frameworks and writes a redacted \
             HTML report to `docs/compliance`.",
            sc_comply::registry::SHIPPED.len()
        ),
    }];

    children.push(picker(v));
    children.push(caveat(v.model));
    children.push(Content::Text {
        markdown: "Control results never use a model. A model only writes the executive \
                   summary and the guidance for controls a code scan cannot settle."
            .to_string(),
    });

    children.push(run_control(v));
    if let Some(result) = v.result {
        children.push(outcome(result, v.model));
    }

    Content::Stack { children }
}

/// The model choice, as one row per option.
///
/// A list rather than a segmented button row: the content model has no segmented control,
/// and inventing one would mean asking for a widget by degrees. Each row carries its own
/// command, and the active one is marked in its text rather than by colour — the host
/// decides what things look like.
fn picker(v: &View<'_>) -> Content {
    let items = ComplyModel::ALL
        .iter()
        .map(|m| {
            let active = *m == v.model;
            ListItem {
                text: format!("{} {}", if active { "●" } else { "○" }, m.label()),
                detail: None,
                // Mid-run every row is inert: the audit in flight cannot change model.
                command: (!v.running).then(|| "comply.set-model".to_string()),
                args: vec![m.label().to_string()],
                severity: None,
            }
        })
        .collect();

    Content::Stack {
        children: vec![
            Content::Text {
                markdown: "**Summary written by**".to_string(),
            },
            Content::List { items },
        ],
    }
}

/// What the choice actually means. This is the reason the panel exists.
fn caveat(model: ComplyModel) -> Content {
    match model.caveat() {
        Some(c) => Content::List {
            items: vec![ListItem {
                text: c.to_string(),
                detail: None,
                command: None,
                args: Vec::new(),
                severity: Some(Severity::Warning),
            }],
        },
        None => Content::Text {
            markdown: "Deterministic only — no executive summary or auditor guidance. \
                       Every control result is still produced."
                .to_string(),
        },
    }
}

/// The Run button, and the in-flight note.
fn run_control(v: &View<'_>) -> Content {
    if v.running {
        let narrating = v.model != ComplyModel::None;
        return Content::Text {
            markdown: if narrating {
                "**Auditing…** Scanning the workspace, then writing the summary. \
                 The model calls take a while."
                    .to_string()
            } else {
                "**Auditing…** Scanning the workspace.".to_string()
            },
        };
    }

    // No project ⇒ say so instead of offering a button that can only fail.
    if v.workspace.is_none() {
        return Content::List {
            items: vec![ListItem {
                text: "Open a project folder first — the audit reads the workspace.".to_string(),
                detail: None,
                command: None,
                args: Vec::new(),
                severity: Some(Severity::Warning),
            }],
        };
    }

    Content::Form {
        fields: Vec::new(),
        submit: Some("🛡  Generate report".to_string()),
    }
}

/// The outcome: totals and an Open row, or the reason it failed.
fn outcome(result: &Result<ComplyReport, String>, model: ComplyModel) -> Content {
    match result {
        Err(e) => Content::List {
            items: vec![
                ListItem {
                    text: "Could not generate the report".to_string(),
                    detail: None,
                    command: None,
                    args: Vec::new(),
                    severity: Some(Severity::Error),
                },
                ListItem {
                    text: e.clone(),
                    detail: None,
                    command: None,
                    args: Vec::new(),
                    severity: None,
                },
            ],
        },
        Ok(r) => {
            let mut items = vec![
                ListItem {
                    text: format!("{} controls across {} frameworks", r.controls, r.frameworks),
                    detail: None,
                    command: None,
                    args: Vec::new(),
                    severity: None,
                },
                // Counts, never a single headline percentage — the same rule the reports
                // themselves follow.
                ListItem {
                    text: format!("{} verified", r.passed),
                    detail: None,
                    command: None,
                    args: Vec::new(),
                    severity: None,
                },
                ListItem {
                    text: format!("{} gaps", r.gaps),
                    detail: None,
                    command: None,
                    args: Vec::new(),
                    severity: Some(Severity::Error),
                },
                ListItem {
                    text: format!("{} need manual evidence", r.unknown),
                    detail: None,
                    command: None,
                    args: Vec::new(),
                    severity: Some(Severity::Warning),
                },
            ];

            // The user asked for a summary and did not get one. Say so rather than
            // letting them assume the page has one.
            if !r.narrated && model != ComplyModel::None {
                items.push(ListItem {
                    text: "The model did not return a usable summary — the report is \
                           published without one."
                        .to_string(),
                    detail: None,
                    command: None,
                    args: Vec::new(),
                    severity: Some(Severity::Warning),
                });
            }

            items.push(ListItem {
                text: "📂  Open report".to_string(),
                detail: Some(r.index.display().to_string()),
                command: Some("comply.open-report".to_string()),
                args: Vec::new(),
                severity: None,
            });

            Content::List { items }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view<'a>(model: ComplyModel, running: bool) -> View<'a> {
        View {
            workspace: None,
            model,
            running,
            result: None,
        }
    }

    fn rows(c: &Content) -> Vec<&ListItem> {
        match c {
            Content::List { items } => items.iter().collect(),
            Content::Stack { children } => children.iter().flat_map(rows).collect(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn the_picker_is_inert_while_the_audit_runs() {
        // Changing the model mid-run would not affect the audit in flight, so every row
        // must be unclickable rather than misleadingly live.
        let c = picker(&view(ComplyModel::None, true));
        assert!(
            rows(&c).iter().all(|i| i.command.is_none()),
            "a running audit must not offer a model change"
        );
    }

    #[test]
    fn the_picker_is_live_when_idle() {
        let c = picker(&view(ComplyModel::None, false));
        assert!(rows(&c).iter().all(|i| i.command.is_some()));
    }

    #[test]
    fn the_active_model_is_marked_without_relying_on_colour() {
        let c = picker(&view(ComplyModel::Gemini, false));
        let marked: Vec<_> = rows(&c)
            .iter()
            .filter(|i| i.text.starts_with('●'))
            .map(|i| i.text.clone())
            .collect();
        assert_eq!(marked.len(), 1, "exactly one row is active");
        assert!(marked[0].contains("Gemini"));
    }

    #[test]
    fn choosing_no_model_still_explains_what_that_means() {
        // The caveat is the reason this panel exists rather than a menu item, so the
        // deterministic path must say something too.
        let c = caveat(ComplyModel::None);
        let Content::Text { markdown } = &c else {
            panic!("expected prose for the deterministic path");
        };
        assert!(markdown.contains("Every control result is still produced"));
    }

    #[test]
    fn an_unnarrated_report_says_so_when_a_model_was_asked_for() {
        let r = ComplyReport {
            index: std::path::PathBuf::from("docs/compliance/index.html"),
            frameworks: 2,
            controls: 40,
            passed: 10,
            gaps: 5,
            unknown: 25,
            narrated: false,
        };
        let c = outcome(&Ok(r), ComplyModel::Gemini);
        assert!(
            rows(&c)
                .iter()
                .any(|i| i.text.contains("did not return a usable summary")),
            "silence about a missing summary lets the user assume it is there"
        );
    }

    #[test]
    fn a_deterministic_report_does_not_apologise_for_a_summary_nobody_asked_for() {
        let r = ComplyReport {
            index: std::path::PathBuf::from("docs/compliance/index.html"),
            frameworks: 2,
            controls: 40,
            passed: 10,
            gaps: 5,
            unknown: 25,
            narrated: false,
        };
        let c = outcome(&Ok(r), ComplyModel::None);
        assert!(!rows(&c)
            .iter()
            .any(|i| i.text.contains("did not return a usable summary")));
    }

    #[test]
    fn the_outcome_reports_counts_and_never_a_percentage() {
        // "78% compliant" is the misreading this whole feature exists to avoid.
        let r = ComplyReport {
            index: std::path::PathBuf::from("docs/compliance/index.html"),
            frameworks: 3,
            controls: 100,
            passed: 78,
            gaps: 2,
            unknown: 20,
            narrated: true,
        };
        let c = outcome(&Ok(r), ComplyModel::Local);
        let all = rows(&c)
            .iter()
            .map(|i| i.text.clone())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("78 verified"));
        assert!(!all.contains('%'), "no headline percentage: {all}");
    }

    #[test]
    fn a_failure_carries_its_reason() {
        let c = outcome(
            &Err("gemini has no API key".to_string()),
            ComplyModel::Gemini,
        );
        let all = rows(&c)
            .iter()
            .map(|i| i.text.clone())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("gemini has no API key"));
    }

    #[test]
    fn without_a_project_the_run_button_is_replaced_by_the_reason() {
        let c = run_control(&view(ComplyModel::None, false));
        assert!(
            !matches!(c, Content::Form { .. }),
            "a button that can only fail is worse than none"
        );
    }
}
