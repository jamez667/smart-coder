//! Answering a plugin's requests (spec 25).
//!
//! Every request here is **synchronous and cheap**: read a buffer already in memory,
//! apply an edit, open a file, list paths. None of them blocks, because this runs on the
//! UI thread during the tick drain — a request that needed real work would have to
//! become a task, and none of v1's do.
//!
//! # The rules that are not negotiable
//!
//! * **Every path goes through `sc_fsutil::safe_join`.** A plugin is a subprocess with
//!   the user's full privileges and can read any file it likes on its own; what it must
//!   not do is get the *host* to read one outside the workspace on its behalf, because
//!   then the refusal the user thinks they have is not there. The rule is the same one
//!   the agent's tools use on a model's path arguments.
//! * **Every edit is version-checked.** A plugin reads at version N and sends N back; if
//!   the user has typed since, the edit is refused. Without it a slow plugin's edit lands
//!   on text that has moved, which corrupts the file in a way nothing reconstructs.
//! * **Every reply carries the request's id**, including failures, so a plugin waiting on
//!   an answer always gets one. A dropped reply is a plugin hung forever.

use super::*;

use sc_plugin_proto::{
    Edit, ErrorCode, HostMessage, PluginMessage, Position, RequestId, ResponsePayload,
};

impl App {
    /// Handle one request, returning the reply to send.
    ///
    /// Returns `None` for messages that are notifications rather than requests — they
    /// are handled by the caller and owe no answer.
    pub(crate) fn answer_plugin(&mut self, msg: &PluginMessage) -> Option<HostMessage> {
        match msg {
            PluginMessage::BufferRead { id, path } => Some(self.reply_buffer_read(*id, path)),
            PluginMessage::BufferList { id } => Some(HostMessage::Response {
                id: *id,
                payload: ResponsePayload::Buffers {
                    paths: self.open_buffer_paths(),
                },
            }),
            PluginMessage::FileRead { id, path } => Some(self.reply_file_read(*id, path)),
            PluginMessage::BufferEdit {
                id,
                path,
                version,
                edits,
            } => Some(self.reply_buffer_edit(*id, path, *version, edits)),
            PluginMessage::EditorOpen { id, path, line } => {
                Some(self.reply_editor_open(*id, path, *line))
            }
            // `RunCommand` lets a plugin invoke another's command. Answered as
            // unsupported rather than silently ignored: a plugin waiting on the reply
            // would otherwise hang, and "not yet" is a fact it can act on.
            PluginMessage::RunCommand { id, .. } => Some(HostMessage::ErrorResponse {
                id: *id,
                code: ErrorCode::Unsupported,
                message: "running commands across plugins is not implemented yet".to_string(),
            }),
            _ => None,
        }
    }

    /// The open buffers, in pane and tab order.
    fn open_buffer_paths(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (_, pane) in self.panes.iter() {
            for tab in &pane.tabs {
                if !out.contains(&tab.path) {
                    out.push(tab.path.clone());
                }
            }
        }
        out
    }

    /// Find a tab by workspace-relative path, in any pane.
    ///
    /// A path lives in exactly one pane — `Panes` enforces that, because two copies would
    /// mean two dirty flags over one file — so the first match is the only match.
    fn tab_for(&self, path: &str) -> Option<&super::tabs::Tab> {
        self.panes
            .iter()
            .flat_map(|(_, p)| p.tabs.iter())
            .find(|t| t.path == path)
    }

    fn reply_buffer_read(&self, id: RequestId, path: &str) -> HostMessage {
        let Some(tab) = self.tab_for(path) else {
            return err(id, ErrorCode::NotFound, format!("{path} is not open"));
        };
        match tab.text() {
            Some(text) => HostMessage::Response {
                id,
                payload: ResponsePayload::Text {
                    text,
                    version: tab.version,
                },
            },
            // Open, but not as an editable buffer — a binary or oversized file opens
            // read-only with a reason rather than failing to open at all.
            None => err(
                id,
                ErrorCode::NotEditable,
                format!("{path} is open but has no editable buffer"),
            ),
        }
    }

    fn reply_file_read(&self, id: RequestId, path: &str) -> HostMessage {
        let root = self.workspace_root();
        let Some(abs) = sc_fsutil::safe_join(&root, path) else {
            return err(
                id,
                ErrorCode::Forbidden,
                format!("{path} is outside the workspace"),
            );
        };
        let Ok(bytes) = std::fs::read(&abs) else {
            return err(id, ErrorCode::NotFound, format!("{path} could not be read"));
        };
        // The editor's OWN rules decide what is handable — binary, non-UTF-8, or too
        // large. Reusing them means a plugin and the editor agree on what a text file is,
        // rather than the plugin discovering a different limit.
        match sc_win::editbuf::classify(&bytes) {
            sc_win::editbuf::Classified::Editable { text, .. } => HostMessage::Response {
                id,
                payload: ResponsePayload::Text {
                    text,
                    // A file read from disk has no buffer version. 0 is honest — a plugin
                    // cannot edit against it, because `buffer.edit` looks up the open
                    // tab's version and would reject a stale 0.
                    version: 0,
                },
            },
            sc_win::editbuf::Classified::Refused(why) => {
                err(id, ErrorCode::NotEditable, format!("{path}: {why:?}"))
            }
        }
    }

    fn reply_editor_open(&mut self, id: RequestId, path: &str, line: Option<usize>) -> HostMessage {
        let root = self.workspace_root();
        if sc_fsutil::safe_join(&root, path).is_none() {
            return err(
                id,
                ErrorCode::Forbidden,
                format!("{path} is outside the workspace"),
            );
        }
        self.select_file(path.to_string());
        if let Some(n) = line {
            self.panes.focused_mut().pending_scroll_line = Some(n);
        }
        HostMessage::Response {
            id,
            payload: ResponsePayload::Ok,
        }
    }

    fn reply_buffer_edit(
        &mut self,
        id: RequestId,
        path: &str,
        version: u64,
        edits: &[Edit],
    ) -> HostMessage {
        let Some(tab) = self.tab_for(path) else {
            return err(id, ErrorCode::NotFound, format!("{path} is not open"));
        };
        if tab.version != version {
            // The check this whole mechanism exists for. Refuse; do not guess.
            return err(
                id,
                ErrorCode::VersionConflict,
                format!(
                    "{path} is at version {} but the edit was computed against {version}",
                    tab.version
                ),
            );
        }
        let Some(text) = tab.text() else {
            return err(
                id,
                ErrorCode::NotEditable,
                format!("{path} has no editable buffer"),
            );
        };

        // Validated against a COPY first: if any position is out of range, or two edits
        // overlap, nothing is applied at all. That is what "all or nothing" means, and a
        // half-applied multi-edit is unreconstructable.
        if apply_edits(&text, edits).is_none() {
            return err(
                id,
                ErrorCode::BadRequest,
                "an edit position is out of range, or two edits overlap".to_string(),
            );
        }

        // `pane_holding` rather than a scan: a path lives in exactly one pane, and that
        // invariant is the reason `Panes` has the lookup at all.
        let Some(pane_id) = self.panes.pane_holding(path) else {
            return err(id, ErrorCode::NotFound, format!("{path} is not open"));
        };
        let Some(tab) = self
            .panes
            .get_mut(pane_id)
            .and_then(|p| p.tabs.iter_mut().find(|t| t.path == path))
        else {
            return err(id, ErrorCode::NotFound, format!("{path} is not open"));
        };
        let Some(editor) = tab.editor_mut() else {
            return err(
                id,
                ErrorCode::NotEditable,
                format!("{path} has no editable buffer"),
            );
        };

        // Applied BACK TO FRONT through the widget's own edit message, so every position
        // is interpreted against the text as the plugin read it — applying front to back
        // would shift each later edit by the length of every earlier one.
        //
        // `Message::ApplyEdit` is this project's addition to the editor widget (see
        // crates/sc-editor/FORK.md): it routes through the command history, so the whole
        // request lands as ONE undo entry rather than as an unreachable mutation.
        let mut ordered: Vec<&Edit> = edits.iter().collect();
        ordered.sort_by_key(|e| std::cmp::Reverse(e.start));
        for e in ordered {
            let _ = editor.update(&sc_editor::Message::ApplyEdit {
                start: (e.start.line, e.start.column),
                end: (e.end.line, e.end.column),
                text: e.text.clone(),
            });
        }
        tab.dirty = true;
        tab.version = tab.version.wrapping_add(1);
        let new_version = tab.version;

        HostMessage::Response {
            id,
            payload: ResponsePayload::Edited {
                version: new_version,
            },
        }
    }
}

/// Build an error reply.
fn err(id: RequestId, code: ErrorCode, message: String) -> HostMessage {
    HostMessage::ErrorResponse { id, code, message }
}

/// Apply `edits` to `text`, or `None` if any position is out of range.
///
/// **Every position is interpreted against the ORIGINAL text**, which is why the edits are
/// sorted and applied back to front: applying front to back would shift every later
/// position by the length of every earlier edit, and a plugin computing positions against
/// what it read has no way to anticipate that.
///
/// Pure, so the whole coordinate contract is testable without an editor widget.
pub(crate) fn apply_edits(text: &str, edits: &[Edit]) -> Option<String> {
    let mut offsets: Vec<(usize, usize, &str)> = Vec::with_capacity(edits.len());
    for e in edits {
        let start = offset_of(text, e.start)?;
        let end = offset_of(text, e.end)?;
        if start > end {
            return None;
        }
        offsets.push((start, end, e.text.as_str()));
    }
    // Back to front, so earlier offsets stay valid as later edits are spliced in.
    offsets.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    // Overlapping edits would corrupt each other silently; refuse the whole request.
    for w in offsets.windows(2) {
        if w[0].0 < w[1].1 {
            return None;
        }
    }
    let mut out = text.to_string();
    for (start, end, replacement) in offsets {
        out.replace_range(start..end, replacement);
    }
    Some(out)
}

/// Byte offset of a 0-based line/column position, or `None` when it is past the end.
///
/// Columns are **character** counts, not bytes: a plugin counting columns in a line
/// containing `é` would otherwise address the middle of a UTF-8 sequence, and splitting
/// one panics.
fn offset_of(text: &str, pos: Position) -> Option<usize> {
    let mut line_start = 0usize;
    for (n, line) in text.split_inclusive('\n').enumerate() {
        if n == pos.line {
            let mut chars = line.char_indices();
            for _ in 0..pos.column {
                // A column past the line's end is not an error when it lands exactly at
                // the end — that is how "insert at end of line" is spelled.
                match chars.next() {
                    Some(_) => {}
                    None => return None,
                }
            }
            return Some(line_start + chars.next().map(|(i, _)| i).unwrap_or(line.len()));
        }
        line_start += line.len();
    }
    // One past the last line addresses the very end, which is how a plugin appends.
    (pos.line == text.split_inclusive('\n').count() && pos.column == 0).then_some(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(sl: usize, sc: usize, el: usize, ec: usize, text: &str) -> Edit {
        Edit {
            start: Position {
                line: sl,
                column: sc,
            },
            end: Position {
                line: el,
                column: ec,
            },
            text: text.to_string(),
        }
    }

    #[test]
    fn a_single_insert_lands_where_it_was_asked_to() {
        let got = apply_edits("abc\ndef\n", &[edit(1, 0, 1, 0, "X")]).unwrap();
        assert_eq!(got, "abc\nXdef\n");
    }

    #[test]
    fn a_range_is_replaced() {
        let got = apply_edits("abc\ndef\n", &[edit(0, 1, 0, 3, "ZZ")]).unwrap();
        assert_eq!(got, "aZZ\ndef\n");
    }

    #[test]
    fn an_empty_replacement_deletes() {
        let got = apply_edits("abc\ndef\n", &[edit(0, 0, 1, 0, "")]).unwrap();
        assert_eq!(got, "def\n");
    }

    /// **The contract that makes multi-edit requests usable.** Every position is against
    /// the ORIGINAL text, so a plugin computing two edits from one read does not have to
    /// predict how the first shifts the second.
    #[test]
    fn every_position_is_against_the_original_text() {
        let got = apply_edits(
            "one\ntwo\nthree\n",
            &[edit(0, 0, 0, 3, "1"), edit(2, 0, 2, 5, "3")],
        )
        .unwrap();
        assert_eq!(got, "1\ntwo\n3\n", "neither edit shifted the other");
    }

    /// Order in the request must not matter, or a plugin's array ordering would silently
    /// change the result.
    #[test]
    fn edit_order_in_the_request_is_irrelevant() {
        let forwards = apply_edits(
            "one\ntwo\nthree\n",
            &[edit(0, 0, 0, 3, "1"), edit(2, 0, 2, 5, "3")],
        );
        let backwards = apply_edits(
            "one\ntwo\nthree\n",
            &[edit(2, 0, 2, 5, "3"), edit(0, 0, 0, 3, "1")],
        );
        assert_eq!(forwards, backwards);
    }

    /// Overlapping edits would corrupt each other silently, so the whole request is
    /// refused — all or nothing, and nothing is the safe half.
    #[test]
    fn overlapping_edits_are_refused_entirely() {
        assert_eq!(
            apply_edits("abcdef\n", &[edit(0, 0, 0, 4, "X"), edit(0, 2, 0, 6, "Y")]),
            None
        );
    }

    /// An out-of-range position refuses the WHOLE request, so a good edit beside a bad
    /// one never lands alone.
    #[test]
    fn one_bad_position_refuses_every_edit() {
        assert_eq!(
            apply_edits("abc\n", &[edit(0, 0, 0, 1, "X"), edit(99, 0, 99, 0, "Y")]),
            None
        );
    }

    #[test]
    fn a_backwards_range_is_refused() {
        assert_eq!(apply_edits("abcdef\n", &[edit(0, 4, 0, 1, "X")]), None);
    }

    /// Columns count CHARACTERS. Counting bytes would let a plugin address the middle of
    /// a UTF-8 sequence, and splicing there panics.
    #[test]
    fn columns_are_characters_not_bytes() {
        let got = apply_edits("héllo\n", &[edit(0, 2, 0, 3, "L")]).unwrap();
        assert_eq!(got, "héLlo\n", "column 2 is the first l, not a byte offset");
    }

    /// Appending at the very end is spelled as one line past the last — the position a
    /// plugin naturally computes from a line count.
    #[test]
    fn a_position_one_line_past_the_end_appends() {
        let got = apply_edits("abc\n", &[edit(1, 0, 1, 0, "def\n")]).unwrap();
        assert_eq!(got, "abc\ndef\n");
    }

    #[test]
    fn an_empty_edit_list_changes_nothing() {
        assert_eq!(apply_edits("abc\n", &[]).unwrap(), "abc\n");
    }

    /// Inserting at end-of-line, which is column == the line's length.
    #[test]
    fn a_column_at_the_line_end_is_valid() {
        let got = apply_edits("abc\ndef\n", &[edit(0, 3, 0, 3, "!")]).unwrap();
        assert_eq!(got, "abc!\ndef\n");
    }

    // -----------------------------------------------------------------------
    // The request handler, against a real App
    // -----------------------------------------------------------------------

    /// An `App` with one file open, in a directory of its own.
    ///
    /// Keyed by THREAD id: `std::process::id()` is the same for every test in a binary,
    /// so two tests using it share a directory and delete each other's files.
    fn app_with_open_file(name: &str, body: &str) -> App {
        let dir = std::env::temp_dir().join(format!(
            "sc-plugreq-{name}-{:?}",
            std::thread::current().id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join(name), body).unwrap();
        let mut app = App::default();
        app.picked_workspace = Some(dir);
        app.select_file(name.to_string());
        app
    }

    /// **The rule this handler exists to enforce.** A plugin can read any file it likes
    /// in its own process; what it must not do is get the HOST to read one outside the
    /// workspace on its behalf, because then the containment the user believes in is not
    /// there.
    #[test]
    fn a_path_outside_the_workspace_is_refused() {
        let mut app = app_with_open_file(
            "a.rs",
            "fn main() {}
",
        );
        for escape in ["../secrets.txt", "/etc/passwd", "src/../../outside"] {
            let reply = app.answer_plugin(&PluginMessage::FileRead {
                id: 1,
                path: escape.to_string(),
            });
            assert!(
                matches!(
                    reply,
                    Some(HostMessage::ErrorResponse {
                        code: ErrorCode::Forbidden,
                        ..
                    })
                ),
                "{escape} must be refused, got {reply:?}"
            );
        }
    }

    /// Opening a file is gated by the same rule — a plugin must not be able to make the
    /// editor open something outside the project.
    #[test]
    fn editor_open_is_gated_by_the_same_rule() {
        let mut app = app_with_open_file(
            "a.rs", "x
",
        );
        let reply = app.answer_plugin(&PluginMessage::EditorOpen {
            id: 2,
            path: "../elsewhere.rs".to_string(),
            line: None,
        });
        assert!(matches!(
            reply,
            Some(HostMessage::ErrorResponse {
                code: ErrorCode::Forbidden,
                ..
            })
        ));
    }

    /// Reading a buffer returns its text and the version to edit against.
    #[test]
    fn a_buffer_read_returns_text_and_version() {
        let mut app = app_with_open_file(
            "a.rs", "hello
",
        );
        let reply = app.answer_plugin(&PluginMessage::BufferRead {
            id: 3,
            path: "a.rs".to_string(),
        });
        let Some(HostMessage::Response {
            id,
            payload: ResponsePayload::Text { text, .. },
        }) = reply
        else {
            panic!("expected text, got {reply:?}");
        };
        assert_eq!(id, 3);
        assert!(text.contains("hello"), "{text:?}");
    }

    /// A path that is not open is `NotFound`, distinct from one that is refused — a
    /// plugin should be able to tell "ask differently" from "never allowed".
    #[test]
    fn reading_a_closed_buffer_is_not_found() {
        let mut app = app_with_open_file(
            "a.rs", "x
",
        );
        let reply = app.answer_plugin(&PluginMessage::BufferRead {
            id: 4,
            path: "never-opened.rs".to_string(),
        });
        assert!(matches!(
            reply,
            Some(HostMessage::ErrorResponse {
                code: ErrorCode::NotFound,
                ..
            })
        ));
    }

    /// **The version check.** A plugin that computed an edit against text the user has
    /// since changed must be refused, not applied — that is the difference between a
    /// rejected request and a corrupted file.
    #[test]
    fn an_edit_against_a_stale_version_is_refused() {
        let mut app = app_with_open_file(
            "a.rs", "hello
",
        );
        let reply = app.answer_plugin(&PluginMessage::BufferEdit {
            id: 5,
            path: "a.rs".to_string(),
            // The buffer is at 0; this claims to have read it at 99.
            version: 99,
            edits: vec![edit(0, 0, 0, 0, "X")],
        });
        assert!(
            matches!(
                reply,
                Some(HostMessage::ErrorResponse {
                    code: ErrorCode::VersionConflict,
                    ..
                })
            ),
            "{reply:?}"
        );
    }

    /// Every request gets a reply, including the ones this version cannot serve. A
    /// dropped reply is a plugin waiting forever.
    #[test]
    fn an_unimplemented_request_still_gets_an_answer() {
        let mut app = app_with_open_file(
            "a.rs", "x
",
        );
        let reply = app.answer_plugin(&PluginMessage::RunCommand {
            id: 6,
            command: "other.thing".to_string(),
            args: Vec::new(),
        });
        let Some(HostMessage::ErrorResponse { id, code, .. }) = reply else {
            panic!("expected an error reply, got {reply:?}");
        };
        assert_eq!(id, 6, "the reply carries the request's id");
        assert_eq!(code, ErrorCode::Unsupported);
    }

    /// A notification is not a request and owes no answer — replying to one would leave
    /// the plugin correlating an id it never sent.
    #[test]
    fn a_notification_gets_no_reply() {
        let mut app = app_with_open_file(
            "a.rs", "x
",
        );
        assert!(app
            .answer_plugin(&PluginMessage::Log {
                message: "hi".to_string()
            })
            .is_none());
    }

    /// Listing buffers reports what is open.
    #[test]
    fn buffer_list_reports_the_open_files() {
        let mut app = app_with_open_file(
            "a.rs", "x
",
        );
        let reply = app.answer_plugin(&PluginMessage::BufferList { id: 7 });
        let Some(HostMessage::Response {
            payload: ResponsePayload::Buffers { paths },
            ..
        }) = reply
        else {
            panic!("expected a buffer list, got {reply:?}");
        };
        assert!(paths.contains(&"a.rs".to_string()), "{paths:?}");
    }
}
