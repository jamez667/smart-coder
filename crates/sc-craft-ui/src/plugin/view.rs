//! Flattening a plugin's [`Content`] into rows the app can draw.
//!
//! **Pure, and deliberately not iced.** This module turns the nested view model into a
//! flat `Vec<Row>`; the app walks that and builds widgets. The split is the same one
//! `markdown.rs` already makes — it parses to `Block`s and the app renders them — and it
//! exists for the same reason: the interesting logic (nesting, limits, what a click
//! means) is then testable without a renderer.
//!
//! It is also what keeps the four-kind promise honest. A renderer here would be a
//! place for `iced` types to leak into the content model one convenience at a time.

use sc_plugin_proto::{Content, FormField};

/// The most elements a panel may render.
///
/// A plugin that pushes 200,000 rows would otherwise freeze the UI thread building a
/// widget tree from them, and a frozen editor is indistinguishable from a crashed one.
/// The excess is dropped with a note rather than refused outright: showing the first
/// 2,000 rows of a runaway feed is more useful than showing an error.
pub const MAX_ELEMENTS: usize = 2_000;

/// One drawable row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A list row. `command` is `None` when it is not clickable.
    Item {
        text: String,
        detail: Option<String>,
        command: Option<String>,
        args: Vec<String>,
        /// Nesting depth, for indentation. 0 at the top level.
        depth: usize,
    },
    /// Markdown, to be parsed and drawn by the app.
    Markdown { source: String, depth: usize },
    /// A form input.
    Field { field: FormField, depth: usize },
    /// A form's submit button.
    Submit { label: String, depth: usize },
    /// A short note in the app's muted style: the truncation warning, or an
    /// unrecognised content kind.
    Note { text: String },
}

/// Flatten `content` into rows, stopping at [`MAX_ELEMENTS`].
///
/// The depth counter exists so a `Stack` inside a `Stack` indents rather than flattening
/// invisibly — without it, composition would be silently lossy.
pub fn flatten(content: &Content) -> Vec<Row> {
    let mut rows = Vec::new();
    let truncated = walk(content, 0, &mut rows);
    if truncated {
        rows.push(Row::Note {
            text: format!("… truncated at {MAX_ELEMENTS} items"),
        });
    }
    rows
}

/// Append `content`'s rows. Returns true when the cap was hit.
fn walk(content: &Content, depth: usize, out: &mut Vec<Row>) -> bool {
    // Depth is bounded as well as width: a `Stack` nested inside itself a thousand deep
    // would otherwise recurse until the stack overflows, which a plugin can do by
    // accident with a recursive builder. Ten levels is far past any honest layout.
    const MAX_DEPTH: usize = 10;
    if depth > MAX_DEPTH || out.len() >= MAX_ELEMENTS {
        return true;
    }
    match content {
        Content::List { items } => {
            for item in items {
                if out.len() >= MAX_ELEMENTS {
                    return true;
                }
                out.push(Row::Item {
                    text: item.text.clone(),
                    detail: item.detail.clone(),
                    command: item.command.clone(),
                    args: item.args.clone(),
                    depth,
                });
            }
            false
        }
        Content::Text { markdown } => {
            out.push(Row::Markdown {
                source: markdown.clone(),
                depth,
            });
            false
        }
        Content::Form { fields, submit } => {
            for f in fields {
                if out.len() >= MAX_ELEMENTS {
                    return true;
                }
                out.push(Row::Field {
                    field: f.clone(),
                    depth,
                });
            }
            if let Some(label) = submit {
                out.push(Row::Submit {
                    label: label.clone(),
                    depth,
                });
            }
            false
        }
        Content::Stack { children } => {
            for child in children {
                if walk(child, depth + 1, out) {
                    return true;
                }
            }
            false
        }
        // Named rather than blank, so a user seeing an odd panel can tell it is a
        // version mismatch and not a broken plugin.
        Content::Unsupported => {
            out.push(Row::Note {
                text: "This panel uses a feature this version doesn't support.".to_string(),
            });
            false
        }
    }
}

/// Encode a form's values for [`sc_plugin_proto::HostMessage::PanelEvent`]: one
/// `field=value` per line.
///
/// Flat text rather than JSON because the plugin already knows what fields it declared,
/// and a typed payload would be a second schema to version. A value containing a newline
/// would break the encoding, so newlines become spaces — a form field is a single-line
/// input, and silently mangling one line is better than producing a payload the plugin
/// will mis-parse.
pub fn encode_form(fields: &[(String, String)]) -> String {
    fields
        .iter()
        .map(|(id, value)| format!("{id}={}", value.replace(['\n', '\r'], " ")))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_plugin_proto::ListItem;

    #[test]
    fn a_list_flattens_to_rows_at_depth_zero() {
        let c = Content::List {
            items: vec![ListItem::text("a").with_detail("1"), ListItem::text("b")],
        };
        let rows = flatten(&c);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            Row::Item {
                text: "a".to_string(),
                detail: Some("1".to_string()),
                command: None,
                args: Vec::new(),
                depth: 0,
            }
        );
    }

    /// Nesting indents rather than flattening invisibly — otherwise composition would be
    /// silently lossy and a plugin could not tell why its layout looked wrong.
    #[test]
    fn a_stack_increases_depth() {
        let c = Content::Stack {
            children: vec![Content::Text {
                markdown: "hi".to_string(),
            }],
        };
        let rows = flatten(&c);
        assert_eq!(
            rows[0],
            Row::Markdown {
                source: "hi".to_string(),
                depth: 1
            }
        );
    }

    /// A runaway plugin cannot freeze the UI thread. It is truncated with a note, not
    /// refused: the first 2,000 rows of a broken feed still tell you what is happening.
    #[test]
    fn an_enormous_list_is_truncated_with_a_note() {
        let c = Content::List {
            items: (0..MAX_ELEMENTS + 500)
                .map(|i| ListItem::text(i.to_string()))
                .collect(),
        };
        let rows = flatten(&c);
        assert_eq!(rows.len(), MAX_ELEMENTS + 1, "the cap plus the note");
        assert!(matches!(rows.last(), Some(Row::Note { .. })));
    }

    /// A plugin can nest a `Stack` inside itself by accident with a recursive builder.
    /// Bounded depth turns a stack overflow into a truncated panel.
    #[test]
    fn runaway_nesting_is_bounded_rather_than_overflowing_the_stack() {
        let mut c = Content::Text {
            markdown: "deep".to_string(),
        };
        for _ in 0..500 {
            c = Content::Stack { children: vec![c] };
        }
        let rows = flatten(&c);
        assert!(matches!(rows.last(), Some(Row::Note { .. })), "{rows:?}");
    }

    /// An unknown content kind says so, so a version mismatch does not look like a
    /// broken plugin.
    #[test]
    fn an_unsupported_kind_renders_an_explanation() {
        let rows = flatten(&Content::Unsupported);
        assert!(matches!(&rows[0], Row::Note { text } if text.contains("support")));
    }

    #[test]
    fn a_form_yields_its_fields_then_its_submit() {
        let c = Content::Form {
            fields: vec![FormField {
                id: "q".to_string(),
                label: "Query".to_string(),
                value: String::new(),
                placeholder: None,
                secret: false,
            }],
            submit: Some("Go".to_string()),
        };
        let rows = flatten(&c);
        assert!(matches!(rows[0], Row::Field { .. }));
        assert!(matches!(&rows[1], Row::Submit { label, .. } if label == "Go"));
    }

    /// No submit means a read-only display of current values, not a form with an
    /// invisible button.
    #[test]
    fn a_form_without_a_submit_has_no_button() {
        let c = Content::Form {
            fields: Vec::new(),
            submit: None,
        };
        assert!(flatten(&c).is_empty());
    }

    #[test]
    fn a_form_encodes_as_field_equals_value_lines() {
        let got = encode_form(&[
            ("a".to_string(), "one".to_string()),
            ("b".to_string(), "two".to_string()),
        ]);
        assert_eq!(got, "a=one\nb=two");
    }

    /// A newline in a value would split one field into two and hand the plugin a payload
    /// it mis-parses. Mangling the value is the lesser harm, and a form field is a
    /// single-line input anyway.
    #[test]
    fn a_newline_in_a_value_cannot_forge_a_second_field() {
        let got = encode_form(&[("a".to_string(), "one\nb=evil".to_string())]);
        assert_eq!(got, "a=one b=evil");
        assert_eq!(got.lines().count(), 1);
    }
}
