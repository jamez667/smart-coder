//! App view: view(), explorer, sync bar, files tab, git tab.

use super::*;
use iced::widget::{column, row};

impl App {
    pub(crate) fn view(&self) -> Element<'_, Message> {
        // The IDE body is now a PANEL TREE (spec 21): the arrangement is data, so which panels
        // are on screen and how they're split is the user's choice rather than three hardcoded
        // columns. Craft mode simply gets a tree without a Chat leaf in it.
        let body = self.view_layout();

        let gate = self.view_gatebar();

        // The body content below the (flush, full-width) menu bar — this part is padded.
        // The run outcome now lives in the BUILD panel of the bottom strip (not a top
        // banner), so it no longer shoves the three columns down.
        let mut body_col = column![].spacing(GAP);
        // The phase strip tracks an agent workflow, so it has no meaning in Craft mode. Guarded
        // rather than assumed impossible: a plan from before the switch would otherwise linger.
        if self.plan.started() && !self.cfg.craft() {
            body_col = body_col.push(self.view_step_flow());
        }
        // The bottom strip is a PANEL now (`PanelKind::Bottom`), sized by the tree's
        // `body|bottom` split — so it's resizable and hideable rather than a fixed 180px band
        // appended here.
        body_col = body_col.push(body);
        if let Some(g) = gate {
            body_col = body_col.push(g);
        }

        // Base layer: the menu bar flush at the very top (no padding around it, full width),
        // then the padded body beneath it.
        let base = column![
            self.view_menu_bar(),
            container(body_col).width(Fill).height(Fill),
        ]
        .width(Fill)
        .height(Fill);

        // Overlays float ABOVE the base (a Stack), so an open dropdown or the settings modal
        // never shifts the layout. Only one shows at a time.
        let mut layers = iced::widget::stack![base];
        if let Some(dd) = self.view_menu_dropdown() {
            layers = layers.push(dd);
        }
        if let Some(gm) = self.view_git_menu() {
            layers = layers.push(gm);
        }
        if self.settings_open {
            layers = layers.push(self.view_settings_modal());
        }
        if self.comply_open {
            layers = layers.push(self.view_comply_modal());
        }
        if let Some(prompt) = self.view_close_confirm() {
            layers = layers.push(prompt);
        }
        // LAST, so nothing renders over it: until a mode is chosen, the app is not usable and
        // no other overlay should be reachable (spec 21).
        if let Some(prompt) = self.view_first_run() {
            layers = layers.push(prompt);
        }
        layers.width(Fill).height(Fill).into()
    }

    /// The GIT panel: branch header, sync bar, and the changed files.
    ///
    /// A [`sc_win::layout::PanelKind`] leaf, so it returns `Fill × Fill` with no padding or card
    /// of its own — the tree walker applies those (see [`Self::view_panel`]). That uniformity is
    /// what lets it be placed anywhere in the layout.
    pub(crate) fn view_git_panel(&self) -> Element<'_, Message> {
        // GitHub-PR-style header: the current branch, ahead/behind vs upstream, and a count of
        // changed files.
        let n_changed = self.file_status.len();
        let up = &self.upstream;
        let branch_line = match &self.branch {
            Some(b) => {
                let mut s = format!("⎇ {b}");
                if up.ahead > 0 {
                    s.push_str(&format!("  ↑{}", up.ahead));
                }
                if up.behind > 0 {
                    s.push_str(&format!("  ↓{}", up.behind));
                }
                s.push_str(&format!("  ·  {n_changed} changed"));
                s
            }
            None => "not a git repo".to_string(),
        };
        let mut col = column![text(branch_line)
            .size(11)
            .color(iced::Color::from_rgb(0.55, 0.58, 0.70)),]
        .spacing(6);
        // Push / Pull / Fetch — only when the repo is on a branch (has a name). Labels show the
        // ahead/behind counts so you know what each will move.
        if self.branch.is_some() {
            col = col.push(self.view_sync_bar());
        }
        col = col.push(self.view_git_tab());
        col.width(Fill).height(Fill).into()
    }

    /// The CODE panel — editor or review, per the active tab.
    pub(crate) fn view_code_panel(&self) -> Element<'_, Message> {
        self.view_code()
    }

    /// The bottom panel: Problems / Terminal (+ Verification / Build in Assistant mode).
    pub(crate) fn view_bottom_panel(&self) -> Element<'_, Message> {
        self.view_bottom_strip()
            .unwrap_or_else(|| Space::new().into())
    }

    /// The push / pull / fetch bar shown under the branch line. Push shows the ahead count, Pull
    /// the behind count. When the branch has no upstream, Push offers to publish it.
    pub(crate) fn view_sync_bar(&self) -> Element<'_, Message> {
        let up = &self.upstream;
        let push_label = if up.upstream.is_none() {
            "↑ Publish".to_string()
        } else if up.ahead > 0 {
            format!("↑ Push {}", up.ahead)
        } else {
            "↑ Push".to_string()
        };
        let pull_label = if up.behind > 0 {
            format!("↓ Pull {}", up.behind)
        } else {
            "↓ Pull".to_string()
        };
        let btn = |label: String, msg: Message| {
            button(text(label).size(11).color(FG))
                .on_press(msg)
                .padding([1, 8])
                .style(stage_toggle_button)
        };
        row![
            btn(push_label, Message::GitPush),
            btn(pull_label, Message::GitPull),
            button(text("⟳").size(12).color(FG))
                .on_press(Message::GitFetch)
                .padding([1, 8])
                .style(stage_toggle_button),
        ]
        .spacing(4)
        .into()
    }

    /// The **Files** tab: the workspace file tree, dirs-first, click a file to pin it in the
    /// code panel, click a dir to collapse/expand. Empty-state hint before a project folder is
    /// picked.
    pub(crate) fn view_files_tab(&self) -> Element<'_, Message> {
        use sc_win::gitdiff::FileStatus;
        let filtering = !self.file_filter.trim().is_empty();
        // Derive the display from the cached full tree in memory — no filesystem walk per frame.
        let rows = if filtering {
            sc_win::filetree::filter_view(&self.tree_cache, &self.file_filter)
        } else {
            sc_win::filetree::collapse_view(&self.tree_cache, &self.collapsed_dirs)
        };

        // A quick-filter box at the top of the tree — type to narrow to matching files/folders.
        let filter_box = text_input("Filter files…", &self.file_filter)
            .on_input(Message::FileFilterChanged)
            .padding(4)
            .size(12)
            .style(input_style)
            .width(Fill);

        let mut col = column![].spacing(2);
        if rows.is_empty() {
            let hint = if filtering {
                "no files match"
            } else {
                "File ▸ Open folder to work in"
            };
            col = col.push(text(hint).size(11).color(FG_MUTED));
        }
        for r in rows.iter().take(600) {
            let indent = 8.0 + (r.depth as f32) * 12.0;
            let is_selected = !r.is_dir && self.selected_file.as_deref() == Some(r.rel.as_str());
            let glyph = if r.is_dir {
                // While filtering the tree is force-expanded, so every shown dir reads as open.
                if !filtering && self.collapsed_dirs.contains(&r.rel) {
                    "▸"
                } else {
                    "▾"
                }
            } else {
                " "
            };
            // PR-style file status badge (M/A/D) + colouring for changed files.
            let status = (!r.is_dir).then(|| self.file_status.get(&r.rel)).flatten();
            let (badge, badge_color) = match status {
                Some(FileStatus::Added) => ("A", GOOD),
                Some(FileStatus::Modified) => ("M", AMBER),
                Some(FileStatus::Deleted) => ("D", BAD),
                None => (" ", FG_MUTED),
            };
            let name_color = if is_selected {
                ACCENT
            } else if let Some(s) = status {
                match s {
                    FileStatus::Added => GOOD,
                    FileStatus::Modified => AMBER,
                    FileStatus::Deleted => BAD,
                }
            } else if r.is_dir {
                FG
            } else {
                FG_MUTED
            };
            let msg = if r.is_dir {
                Message::ToggleDir(r.rel.clone())
            } else {
                Message::SelectFile(r.rel.clone())
            };
            let btn = button(
                row![
                    text(badge.to_string())
                        .size(11)
                        .font(iced::Font::MONOSPACE)
                        .color(badge_color),
                    text(format!("{glyph} {}", r.name))
                        .size(12)
                        .color(name_color),
                ]
                .spacing(4),
            )
            .on_press(msg)
            .padding([1, 4])
            .style(tree_button)
            .width(Fill);
            col = col.push(row![Space::new().width(Length::Fixed(indent)), btn]);
        }

        column![filter_box, scrollable(col).height(Fill)]
            .spacing(6)
            .into()
    }

    /// The **Git** tab: a VS-Code-style Source Control panel — a commit-message box + Commit
    /// button on top, then a **Staged Changes** section and an unstaged **Changes** section
    /// (grouped Added / Modified / Deleted). Right-click any file for stage / unstage / discard.
    /// The git files in the exact order the git tab renders them: the staged section first (keys
    /// filtered to those `stage_states` marks staged), then the unstaged section (everything
    /// unstaged/untracked). `view_git_tab` and the Shift-range selection both derive from this, so
    /// the "displayed order" the range spans can never drift from what's on screen.
    pub(crate) fn git_display_order(&self) -> Vec<String> {
        let staged = self
            .file_status
            .keys()
            .filter(|p| self.stage_states.get(*p).map(|s| s.staged).unwrap_or(false))
            .cloned();
        let unstaged = self
            .file_status
            .keys()
            .filter(|p| {
                self.stage_states
                    .get(*p)
                    .map(|s| s.unstaged)
                    .unwrap_or(true)
            })
            .cloned();
        staged.chain(unstaged).collect()
    }

    /// The files a stage/unstage/discard action should apply to, given the row it was invoked on.
    /// When `path` is part of a multi-selection (the user Ctrl/Shift-picked a set), the action
    /// fans out to every selected file — in displayed order, so the git call is deterministic.
    /// Otherwise it's just `path` (a lone click, or acting on a row outside the current selection).
    pub(crate) fn git_action_targets(&self, path: &str) -> Vec<String> {
        if self.git_selection.len() > 1 && self.git_selection.contains(path) {
            self.git_display_order()
                .into_iter()
                .filter(|p| self.git_selection.contains(p))
                .collect()
        } else {
            vec![path.to_string()]
        }
    }

    pub(crate) fn view_git_tab(&self) -> Element<'_, Message> {
        use sc_win::gitdiff::FileStatus;
        if self.branch.is_none() {
            return text("not a git repository").size(11).color(FG_MUTED).into();
        }

        // The commit box: a message input + a Commit button, enabled only when something is
        // staged and the message is non-empty (like VS Code's checkmark).
        let has_staged = self.stage_states.values().any(|s| s.staged);
        let can_commit = has_staged && !self.commit_msg.trim().is_empty();
        let input = text_input("Message (commit staged changes)", &self.commit_msg)
            .on_input(Message::CommitMsgChanged)
            .on_submit(Message::GitCommit)
            .padding(6)
            .size(12)
            .style(input_style)
            .width(Fill);
        let mut commit_btn = button(text("✓ Commit").size(12));
        if can_commit {
            commit_btn = commit_btn
                .on_press(Message::GitCommit)
                .style(primary_button);
        } else {
            commit_btn = commit_btn.style(menu_item_style);
        }
        let commit_box = column![input, commit_btn.padding([4, 12]).width(Fill)].spacing(4);

        // Partition the changed files into staged and unstaged. A file can be in BOTH (staged
        // plus further working-tree edits) — VS Code shows it in each, and so do we. The
        // staged/unstaged filters here MUST match `git_display_order` (which the Shift-range
        // selection uses), so the on-screen order and the selectable order stay in lock-step.
        let staged: Vec<&String> = self
            .file_status
            .keys()
            .filter(|p| self.stage_states.get(*p).map(|s| s.staged).unwrap_or(false))
            .collect();
        let unstaged: Vec<(&String, FileStatus)> = self
            .file_status
            .iter()
            .filter(|(p, _)| {
                // Unstaged, or untracked. If we have no stage info for it, treat it as unstaged.
                self.stage_states
                    .get(*p)
                    .map(|s| s.unstaged)
                    .unwrap_or(true)
            })
            .map(|(p, s)| (p, *s))
            .collect();

        let mut col = column![].spacing(2);

        // Staged Changes header — with a "− All" (unstage all) — then rows.
        if !staged.is_empty() {
            col = col.push(self.git_section_header(
                "Staged Changes",
                staged.len(),
                Some(("− All", Message::GitUnstageAll)),
            ));
            for path in &staged {
                let status = self
                    .file_status
                    .get(*path)
                    .copied()
                    .unwrap_or(FileStatus::Modified);
                col = col.push(self.git_file_row(path, status, true));
            }
        }

        // Changes (unstaged) header — with a "＋ All" (stage all) — then rows.
        if !unstaged.is_empty() {
            col = col.push(self.git_section_header(
                "Changes",
                unstaged.len(),
                Some(("＋ All", Message::GitStageAll)),
            ));
            for (path, status) in &unstaged {
                col = col.push(self.git_file_row(path, *status, false));
            }
        }

        if staged.is_empty() && unstaged.is_empty() {
            col = col.push(
                text("working tree clean — no changes vs HEAD")
                    .size(11)
                    .color(FG_MUTED),
            );
        }

        column![commit_box, scrollable(col).height(Fill)]
            .spacing(8)
            .into()
    }

    /// A git-tab section header (e.g. "Staged Changes (3)") with an optional stage/unstage-all
    /// action button on the right — `action` is `(button_label, message)`, e.g. `("＋ All", …)` on
    /// the unstaged section or `("− All", …)` on the staged one.
    pub(crate) fn git_section_header(
        &self,
        label: &str,
        count: usize,
        action: Option<(&str, Message)>,
    ) -> Element<'_, Message> {
        let mut r = row![
            text(format!("{label} ({count})")).size(11).color(FG_MUTED),
            Space::new().width(Fill),
        ]
        .align_y(iced::Alignment::Center);
        if let Some((btn_label, msg)) = action {
            r = r.push(
                button(text(btn_label.to_string()).size(11).color(FG))
                    .on_press(msg)
                    .padding([0, 8])
                    .style(stage_toggle_button),
            );
        }
        container(r).padding([2, 0]).into()
    }

    /// One file row in the git tab: a status badge + path, left-click opens it in the code
    /// panel, right-click pops the stage/unstage/discard menu. `staged` tints staged rows and is
    /// carried into the menu so it offers the right action. Deleted files aren't click-to-open.
    pub(crate) fn git_file_row(
        &self,
        path: &str,
        status: sc_win::gitdiff::FileStatus,
        staged: bool,
    ) -> Element<'_, Message> {
        use sc_win::gitdiff::FileStatus;
        let color = match status {
            FileStatus::Added => GOOD,
            FileStatus::Modified => AMBER,
            FileStatus::Deleted => BAD,
        };
        // Highlight the row when it's the previewed file OR part of the multi-selection, so every
        // Ctrl/Shift-selected row reads as selected (not just the last-clicked previewed one).
        let is_selected =
            self.selected_file.as_deref() == Some(path) || self.git_selection.contains(path);
        let name_color = if is_selected { ACCENT } else { color };
        let mut inner = row![
            text(status.badge().to_string())
                .size(11)
                .font(iced::Font::MONOSPACE)
                .color(color),
            text(path.to_string()).size(12).color(name_color),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center);
        // Right-aligned +added / −removed line counts for this file (staged rows read the
        // staged diff, unstaged rows the working-tree diff). Only shown when non-zero.
        let deltas = if staged {
            &self.staged_deltas
        } else {
            &self.unstaged_deltas
        };
        if let Some(d) = deltas.get(path) {
            if d.added > 0 || d.removed > 0 {
                inner = inner.push(Space::new().width(Fill));
                if d.added > 0 {
                    inner = inner.push(
                        text(format!("+{}", d.added))
                            .size(11)
                            .font(iced::Font::MONOSPACE)
                            .color(GOOD),
                    );
                }
                if d.removed > 0 {
                    inner = inner.push(
                        text(format!("−{}", d.removed))
                            .size(11)
                            .font(iced::Font::MONOSPACE)
                            .color(BAD),
                    );
                }
            }
        }
        let row_el: Element<'_, Message> = if status == FileStatus::Deleted {
            container(inner).padding([1, 4]).width(Fill).into()
        } else {
            button(inner)
                .on_press(Message::SelectGitFile(path.to_string()))
                .padding([1, 4])
                .style(tree_button)
                .width(Fill)
                .into()
        };
        // A quick stage (＋) / unstage (−) button beside the row — staged files get −, unstaged get
        // ＋. It's a SIBLING of the row button (buttons can't nest), so clicking it stages/unstages
        // without selecting the file.
        let (glyph, action) = if staged {
            ("−", Message::GitUnstage(path.to_string()))
        } else {
            ("＋", Message::GitStage(path.to_string()))
        };
        let toggle = button(text(glyph).size(13).font(iced::Font::MONOSPACE).color(FG))
            .on_press(action)
            .padding([0, 8])
            .style(stage_toggle_button);
        let full = row![row_el, toggle]
            .align_y(iced::Alignment::Center)
            .spacing(4);
        // A right-click opens the menu; which Stage/Unstage action it offers is decided from
        // `stage_states` when the menu opens. Cursor position comes from the window sub.
        iced::widget::mouse_area(full)
            .on_right_press(Message::GitRowMenu(path.to_string(), status))
            .into()
    }

    /// The git-row context menu overlay: a small floating card of actions (stage / unstage /
    /// discard) for the right-clicked file, positioned at the cursor. A transparent full-window
    /// backdrop closes it on any outside click. `None` when no menu is open.
    pub(crate) fn view_git_menu(&self) -> Option<Element<'_, Message>> {
        use sc_win::gitdiff::FileStatus;
        let (path, status) = self.git_menu.clone()?;

        // Show Stage only if the file has unstaged content, Unstage only if it has staged
        // content — never both when there's nothing to do. Discard's label reflects the status.
        let stage = self.stage_states.get(&path).copied();
        let has_unstaged = stage.map(|s| s.unstaged).unwrap_or(true);
        let has_staged = stage.map(|s| s.staged).unwrap_or(false);
        // When the right-clicked file is part of a multi-selection, Stage/Unstage/Discard fan out
        // to the whole set (see `git_action_targets`) — reflect that in the labels so it's clear
        // the action isn't just this one file, and the count warns before a batch discard.
        let batch = if self.git_selection.len() > 1 && self.git_selection.contains(&path) {
            self.git_selection.len()
        } else {
            1
        };
        let discard_label = if batch > 1 {
            format!("🗑  Discard {batch} files")
        } else {
            match status {
                FileStatus::Added => "🗑  Delete untracked file",
                FileStatus::Deleted => "↩  Restore deleted file",
                FileStatus::Modified => "↩  Discard changes",
            }
            .to_string()
        };
        let stage_label = if batch > 1 {
            format!("＋  Stage {batch} files")
        } else {
            "＋  Stage".to_string()
        };
        let unstage_label = if batch > 1 {
            format!("－  Unstage {batch} files")
        } else {
            "－  Unstage".to_string()
        };
        let mut items: Vec<(String, Message)> = Vec::new();
        if has_unstaged {
            items.push((stage_label, Message::GitStage(path.clone())));
        }
        if has_staged {
            items.push((unstage_label, Message::GitUnstage(path.clone())));
        }
        // Discard acts on the working tree — only meaningful when there are unstaged changes.
        if has_unstaged {
            items.push((discard_label, Message::GitDiscard(path.clone())));
        }
        let mut col = column![text(path.clone())
            .size(11)
            .color(FG_MUTED)
            .wrapping(iced::widget::text::Wrapping::None),]
        .spacing(0);
        for (label, msg) in items {
            col = col.push(
                button(text(label.to_string()).size(13).color(FG))
                    .on_press(msg)
                    .padding([6, 14])
                    .width(Length::Fixed(230.0))
                    .style(menu_item_style),
            );
        }
        let card = container(col).padding(3).style(dropdown_style);

        // Position the card at the click point; clamp a little off the edges isn't needed for a
        // narrow panel, but keep it from riding the very top.
        let x = self.git_menu_at.x.max(0.0);
        let y = self.git_menu_at.y.max(0.0);
        let positioned = column![
            Space::new().height(Length::Fixed(y)),
            row![Space::new().width(Length::Fixed(x)), card],
        ];
        let backdrop = iced::widget::mouse_area(container(Space::new()).width(Fill).height(Fill))
            .on_press(Message::CloseGitMenu)
            .on_right_press(Message::CloseGitMenu);

        Some(
            iced::widget::stack![backdrop, positioned]
                .width(Fill)
                .height(Fill)
                .into(),
        )
    }

    /// The line range currently highlighted in the code view: the active drag (normalized so
    /// lo ≤ hi) if dragging, else the committed comment range. `None` when neither.
    pub(crate) fn selected_line_range(&self) -> Option<(usize, usize)> {
        if let Some((a, b)) = self.drag {
            Some(if a <= b { (a, b) } else { (b, a) })
        } else {
            self.comment_range
        }
    }
}
