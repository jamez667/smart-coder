//! Writing the editor's buffer back to disk.
//!
//! Separate from the other `logic_*` modules because this is where the editor can destroy work,
//! and it should be readable in one screen. The rules it enforces are pure and live in
//! [`sc_win::editbuf`]; this is the glue that reads the disk, asks, and writes.
//!
//! The one invariant: **a save never silently discards someone else's bytes.** Spec 21.

use sc_win::editbuf::{self, SaveVerdict};

use super::{App, TabView};

impl App {
    /// Save the active tab, if it has unsaved edits.
    ///
    /// Refuses when the file changed on disk underneath a dirty buffer — see
    /// [`editbuf::verdict`]. `force` is the user's explicit answer to that refusal and is the
    /// ONLY way to overwrite; nothing takes it automatically.
    pub(crate) fn save_active_tab(&mut self, force: bool) {
        let Some(path) = self.selected_file.clone() else {
            return;
        };
        self.save_tab(&path, force);
    }

    /// Save the tab for `rel`. No-op if it isn't open, isn't editable, or is clean.
    pub(crate) fn save_tab(&mut self, rel: &str, force: bool) {
        let root = self.workspace_root();
        let abs = root.join(rel);

        let Some(tab) = self.tabs.iter().find(|t| t.path == rel) else {
            return;
        };
        if !tab.dirty {
            return; // nothing to write; Ctrl+S on a clean buffer is a no-op, not a touch
        }
        let Some(text) = tab.text() else {
            return; // read-only buffer — the Save affordance isn't offered for these
        };
        let (opened, ending, bom, trailing) =
            (tab.opened, tab.ending, tab.bom, tab.trailing_newline);

        // Ask the disk what happened since we opened the file. `force` skips the question
        // because the user already answered it.
        if !force {
            let current = editbuf::DiskStamp::read(&abs);
            match editbuf::verdict(&opened, current.as_ref(), true) {
                SaveVerdict::Conflict => {
                    // REFUSE. Both versions hold real work, so the editor does not pick a
                    // winner — it says so and offers the choice.
                    self.save_conflict = Some(rel.to_string());
                    return;
                }
                // Unreachable for a dirty buffer (that's the Conflict arm), but if the rule ever
                // changes, reloading over unsaved edits would be the data loss this guards.
                SaveVerdict::ReloadClean | SaveVerdict::Write => {}
            }
        }

        // Only a buffer that went MIXED gets rewritten; a consistent one is written back
        // byte-for-byte, so a one-line edit stays a one-line diff.
        let mut out = if editbuf::is_mixed(&text) {
            editbuf::normalize_to(&text, ending)
        } else {
            text
        };
        // The widget's `content()` rejoins its lines and drops any trailing newline. Restore
        // what the file had, or every save rewrites the last line of every file.
        if trailing && !out.is_empty() && !out.ends_with('\n') {
            out.push_str(ending.as_str());
        }
        let bytes = editbuf::to_bytes(&out, bom);

        // Write to a sibling temp file and rename over the target: a crash mid-write leaves the
        // original intact rather than a half-file. Same directory, so the rename is atomic.
        let tmp = abs.with_extension(format!(
            "{}sc-tmp",
            abs.extension()
                .and_then(|e| e.to_str())
                .map(|e| format!("{e}."))
                .unwrap_or_default()
        ));
        if let Some(dir) = abs.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&tmp, &bytes).is_err() {
            self.save_conflict = Some(rel.to_string());
            return;
        }
        if std::fs::rename(&tmp, &abs).is_err() {
            // Rename can fail where a plain write succeeds (a lock, a different volume). Fall
            // back rather than losing the save, and clean up the temp file either way.
            let direct = std::fs::write(&abs, &bytes);
            let _ = std::fs::remove_file(&tmp);
            if direct.is_err() {
                self.save_conflict = Some(rel.to_string());
                return;
            }
        }

        // Re-stamp from what we just wrote, so the next save compares against this version.
        let stamp = editbuf::DiskStamp::read(&abs).unwrap_or_default();
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.path == rel) {
            tab.opened = stamp;
            tab.dirty = false;
        }
        self.save_conflict = None;

        // The file just changed: refresh the review view and the git diff so the green/red
        // gutter reflects what is now on disk.
        if self.selected_file.as_deref() == Some(rel) {
            self.code = Some(sc_win::codeview::load(&root, rel));
        }
        self.refresh_changed_lines();
        self.refresh_git_view();
    }

    /// Whether any open tab has unsaved edits — for the quit prompt.
    pub(crate) fn any_dirty(&self) -> bool {
        self.tabs.iter().any(|t| t.dirty)
    }

    /// Flip the active tab between the review and edit views.
    ///
    /// A tab that cannot be edited stays in review: there is nothing to switch to, and a caret
    /// that refuses keystrokes is worse than the surface that works.
    pub(crate) fn toggle_tab_view(&mut self) {
        if let Some(tab) = self.active_tab_mut() {
            if !tab.editable() {
                return;
            }
            tab.view = match tab.view {
                TabView::Edit => TabView::Review,
                TabView::Review => TabView::Edit,
            };
        }
    }

    /// Re-read `rel` from disk into its tab, discarding the buffer.
    ///
    /// Only ever called for a CLEAN tab (see the callers in `logic_b`): reloading over unsaved
    /// edits is precisely the data loss the conflict rule exists to prevent.
    pub(crate) fn reload_tab_from_disk(&mut self, rel: &str) {
        let root = self.workspace_root();
        let abs = root.join(rel);
        let Some(i) = self.tabs.iter().position(|t| t.path == rel) else {
            return;
        };
        if self.tabs[i].dirty {
            return;
        }
        // Preserve which view the user was in — a reload shouldn't move them.
        let view = self.tabs[i].view;
        let origin = match view {
            TabView::Edit => super::Origin::Tree,
            TabView::Review => super::Origin::Review,
        };
        self.tabs[i] = super::Tab::open(rel.to_string(), &abs, origin);
    }
}
