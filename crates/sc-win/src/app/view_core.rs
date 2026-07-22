//! App view: view(), explorer, sync bar, files tab, git tab.

use super::*;
use iced::widget::{column, row};

impl App {
    pub(crate) fn view(&self) -> Element<'_, Message> {
        // The IDE body: three columns — EXPLORER (file tree) · CENTER (activity stream +
        // the intent composer beneath it) · CODE (the file being edited). VS-Code-style.
        let center: Element<'_, Message> = if self.plan.started() && self.is_swarm() {
            // A swarm build in flight: the live topology is the story.
            self.view_topology()
        } else {
            // Staged build / iterate / idle: the chat thread (with inline gate controls when a
            // phase is waiting) is the center column. The old left PLAN panel is gone — the phase
            // content streams into the chat and the gating file auto-opens in CODE.
            self.view_center()
        };

        // The chat and code panels share the region right of the explorer; `chat_frac` splits
        // it, driven by dragging the divider between them. Explorer stays a fixed ~20% (200 of
        // the 1000 total portions), so chat+code get 800 to divide.
        let chat_portion = (self.chat_frac * 800.0).round() as u16;
        let code_portion = 800u16.saturating_sub(chat_portion);
        let body = row![
            self.view_explorer(),
            v_divider(),
            container(center)
                .width(Length::FillPortion(chat_portion))
                .height(Fill),
            v_divider_draggable(),
            self.view_code(code_portion),
        ]
        .spacing(GAP)
        .height(Fill);

        let gate = self.view_gatebar();

        // The body content below the (flush, full-width) menu bar — this part is padded.
        // The run outcome now lives in the BUILD panel of the bottom strip (not a top
        // banner), so it no longer shoves the three columns down.
        let mut body_col = column![].spacing(GAP);
        if self.plan.started() {
            body_col = body_col.push(self.view_step_flow());
        }
        body_col = body_col.push(body);
        if let Some(strip) = self.view_bottom_strip() {
            body_col = body_col.push(h_divider());
            body_col = body_col.push(strip);
        }
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
        layers.width(Fill).height(Fill).into()
    }

    /// The rendered height (px) of the EXPLORER column — `window_h` minus the chrome above and
    /// below the body row. Used to scale a divider drag so it tracks the cursor 1:1. The layout
    /// (see `view()`): a menu bar on top, then a padded body column that holds an optional
    /// step-flow bar, the three-panel body row (which contains the explorer), and an optional
    /// bottom strip (fixed 180px) — the explorer gets what's left. These are close estimates of
    /// the fixed chrome; exact-to-the-pixel isn't needed, but the SCALE must be right so the drag
    /// doesn't lag or race the cursor.
    pub(crate) fn explorer_region_h(&self) -> f32 {
        const MENU_BAR: f32 = 34.0; // top menu row + its padding
        const STEP_FLOW: f32 = 52.0; // the phase step-flow card, shown only while planning
        const BOTTOM_STRIP: f32 = 190.0; // the fixed 180px strip + its gap, when shown
        const BODY_PAD: f32 = 2.0 * PAD as f32; // the body container's top+bottom padding
        let mut h = self.window_h - MENU_BAR - BODY_PAD;
        if self.plan.started() {
            h -= STEP_FLOW;
        }
        if self.view_bottom_strip().is_some() {
            h -= BOTTOM_STRIP;
        }
        h.max(1.0)
    }

    /// The left EXPLORER column: a tabbed panel — **Files** (the workspace file tree) and
    /// **Git** (just the changed files, PR-style). A tab bar sits under a branch header that's
    /// shared across both tabs.
    pub(crate) fn view_explorer(&self) -> Element<'_, Message> {
        // GitHub-PR-style header: the current branch, ahead/behind vs upstream, and a count of
        // changed files. Shared by both tabs.
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
        // Top section (25%): everything git — the branch line, push/pull/fetch, and the changed
        // files. Bottom section (75%): the Files tree with its quick filter. Stacked rather than
        // tabbed so both are visible at once.
        let mut git_col = column![text(branch_line)
            .size(11)
            .color(iced::Color::from_rgb(0.55, 0.58, 0.70)),]
        .spacing(6);
        // Push / Pull / Fetch — only when the repo is on a branch (has a name). Labels show the
        // ahead/behind counts so you know what each will move.
        if self.branch.is_some() {
            git_col = git_col.push(self.view_sync_bar());
        }
        git_col = git_col.push(self.view_git_tab());

        // Each section is its own rounded card, stacked with a draggable divider between them.
        // `explorer_frac` (0.1..0.8) splits the height; the clamp guarantees both portions are
        // ≥100, so neither FillPortion is ever zero.
        let git_portion = (self.explorer_frac * 1000.0).round() as u16;
        let files_portion = 1000u16.saturating_sub(git_portion);
        let git_section = container(git_col.spacing(6))
            .height(Length::FillPortion(git_portion))
            .width(Fill)
            .padding(PAD)
            .style(card_style);
        let files_section = container(self.view_files_tab())
            .height(Length::FillPortion(files_portion))
            .width(Fill)
            .padding(PAD)
            .style(card_style);

        // 200 of the 1000-portion total (chat+code share the other 800) → a fixed ~20% width,
        // so dragging the chat|code divider never moves the explorer.
        container(column![git_section, h_divider_draggable(), files_section])
            .width(Length::FillPortion(200))
            .height(Fill)
            .into()
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
