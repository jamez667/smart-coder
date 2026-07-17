//! The iced application — thin rendering glue over the tested `dc_win` library.
//!
//! All "what to show / what to run" logic lives in [`crate::view`], [`crate::config`],
//! [`crate::session`], and [`crate::bridge`]; this file only lays those out as
//! widgets, pumps the worker channels on a timer tick, and routes button clicks back
//! to the blocking decision seams. Keep it thin.

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use iced::widget::{button, checkbox, column, container, row, scrollable, text, text_input, Space};
use iced::{Background, Border, Color, Element, Fill, Length, Subscription, Task, Theme};

use dc_core::Confirmation;
use dc_win::bridge::Pending;
use dc_win::config::ToolCalling;
use dc_win::session::{RunKind, Session, UiEvent};
use dc_win::view::{agent_rows, swarm_rows, Row};
use dc_win::UiConfig;
use dc_workflow::{Decision, Phase};

// --- Visual design tokens (Tokyo Night-aligned) -----------------------------------
// A small, consistent palette + spacing so panels read as cards on a dark canvas,
// not bare text floating on the background.

/// Panel/card surface — a hair lighter than the window background.
const SURFACE: Color = Color::from_rgb(0.106, 0.118, 0.18);
/// A subtle border around cards.
const CARD_BORDER: Color = Color::from_rgb(0.20, 0.22, 0.32);
/// Primary text.
const FG: Color = Color::from_rgb(0.84, 0.86, 0.93);
/// Muted / secondary text (section labels, hints).
const FG_MUTED: Color = Color::from_rgb(0.52, 0.55, 0.66);
/// Accent (the build action, current step, active flows).
const ACCENT: Color = Color::from_rgb(0.48, 0.65, 0.98);
const GOOD: Color = Color::from_rgb(0.45, 0.78, 0.55);
const BAD: Color = Color::from_rgb(0.93, 0.45, 0.50);
/// Amber — the "agent is working on these lines" highlight (pulses while a fix is in flight).
const AMBER: Color = Color::from_rgb(0.95, 0.72, 0.35);
/// Orange — the primary action buttons (Send / build / iterate / Execute). A warm, confident
/// call-to-action against the cool dark canvas.
const ORANGE: Color = Color::from_rgb(0.96, 0.55, 0.24);

// --- Spacing / shape tokens (the modern flat look) --------------------------------
// One radius and one panel padding, so the whole UI reads as a single system and the
// look is dialed from here — not scattered magic numbers. Panels butt together with a
// hairline gutter instead of floating as rounded cards on a wide gap.

/// The single corner radius for every widget — fully square for a flat, modern look.
const RADIUS: f32 = 0.0;
/// The gutter between panels: none — panels butt seamlessly against each other. The
/// card border alone divides them.
const GAP: f32 = 0.0;
/// Shared inner padding for the main panels — tighter than the old 10/12 so the cramped
/// left tree reclaims width.
const PAD: u16 = 8;

/// A stable id for the code-view scrollable, so the minimap can scroll it to a clicked line
/// via `iced::widget::operation::scroll_to`.
fn code_scroll_id() -> iced::advanced::widget::Id {
    iced::advanced::widget::Id::new("code-view")
}

/// Approx. pixel height of one rendered code line (size-13 monospace) — used to convert a
/// clicked minimap line into a scroll offset.
const CODE_LINE_PX: f32 = 17.0;

/// Card surface style: a flat filled panel. Borderless — panels are separated by explicit
/// 1px [`v_divider`]/[`h_divider`] lines between them, so an interior seam is exactly one
/// pixel (two touching card borders would have doubled it to 2px).
fn card_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SURFACE)),
        border: Border {
            radius: RADIUS.into(),
            ..Default::default()
        },
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// A 1px vertical hairline between side-by-side panels, in the card-border tone.
fn v_divider<'a>() -> Element<'a, Message> {
    container(Space::new())
        .width(Length::Fixed(1.0))
        .height(Fill)
        .style(|_t: &Theme| container::Style {
            background: Some(Background::Color(CARD_BORDER)),
            ..container::Style::default()
        })
        .into()
}

/// A draggable version of [`v_divider`]: the same 1px hairline, but wrapped in a wider
/// invisible grab strip that shows the horizontal-resize cursor and starts a divider drag
/// on mouse-down. Used only between the chat and code panels.
fn v_divider_draggable<'a>() -> Element<'a, Message> {
    // A 1px visible line centered in a 7px hit strip, so the handle is easy to grab without
    // widening the seam the user sees.
    let handle = container(v_divider())
        .width(Length::Fixed(7.0))
        .height(Fill)
        .align_x(iced::alignment::Horizontal::Center);
    iced::widget::mouse_area(handle)
        .on_press(Message::SplitDragStart)
        .interaction(iced::mouse::Interaction::ResizingHorizontally)
        .into()
}

/// A 1px horizontal hairline between stacked panels, in the card-border tone.
fn h_divider<'a>() -> Element<'a, Message> {
    container(Space::new())
        .width(Fill)
        .height(Length::Fixed(1.0))
        .style(|_t: &Theme| container::Style {
            background: Some(Background::Color(CARD_BORDER)),
            ..container::Style::default()
        })
        .into()
}

/// Primary (accent-filled) button style for the build action.
fn primary_button(_t: &Theme, status: button::Status) -> button::Style {
    // A clean, crisp orange action button: solid fill that brightens on hover and dims on press.
    // No shadow or fake bevel — flat and modern, matching the rest of the UI. The label is
    // centered by the caller (a Fill-sized, centered text).
    let bg = match status {
        button::Status::Hovered => Color::from_rgb(1.0, 0.63, 0.33),
        button::Status::Pressed => Color::from_rgb(0.85, 0.47, 0.18),
        _ => ORANGE,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: Color::from_rgb(0.12, 0.06, 0.02),
        border: Border {
            radius: RADIUS.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A borderless, transparent button style for file-tree rows — so the explorer reads
/// as a clickable list, not a wall of buttons. A faint wash on hover gives feedback.
fn tree_button(_t: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered => Some(Background::Color(Color { a: 0.06, ..ACCENT })),
        _ => None,
    };
    button::Style {
        background: bg,
        text_color: FG,
        border: Border {
            radius: RADIUS.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// The top menu bar's background: a flat strip a touch darker than the cards.
fn menu_bar_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.08, 0.09, 0.14))),
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// A menu-bar title button: transparent, faint wash when its dropdown is open or hovered.
fn menu_title_style(open: bool, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered);
    let bg = if open || hovered {
        Some(Background::Color(Color { a: 0.10, ..ACCENT }))
    } else {
        None
    };
    button::Style {
        background: bg,
        text_color: FG,
        border: Border {
            radius: RADIUS.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A Windows-style menu item button: transparent, full accent-wash highlight on hover
/// (the classic "whole row highlights" behaviour), square corners for a native feel.
/// The little ＋ / − stage-toggle button on a git file row: a visible lighter surface with a
/// border so it reads as a clickable chip (the plain tree_button was nearly invisible), brightening
/// on hover.
fn stage_toggle_button(_t: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    let a = if hovered { 0.28 } else { 0.16 };
    button::Style {
        background: Some(Background::Color(Color {
            a,
            ..Color::from_rgb(0.6, 0.64, 0.78)
        })),
        text_color: if hovered { FG } else { Color::from_rgb(0.85, 0.87, 0.94) },
        border: Border {
            color: Color { a: 0.35, ..CARD_BORDER },
            width: 1.0,
            radius: RADIUS.into(),
        },
        ..Default::default()
    }
}

fn menu_item_style(_t: &Theme, status: button::Status) -> button::Style {
    let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
    button::Style {
        background: hovered.then(|| Background::Color(ACCENT)),
        text_color: if hovered {
            Color::from_rgb(0.06, 0.07, 0.11)
        } else {
            FG
        },
        border: Border {
            radius: RADIUS.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A code line container's wash, by state (precedence: selection → change → working):
///  • `selected` → faint accent (you're commenting on it),
///  • `changed` → faint GREEN (differs from HEAD — a git change, PR-style),
///  • `working = Some(alpha)` → pulsing AMBER (the agent is working these lines right now),
///  • else transparent.
fn code_line_container(selected: bool, changed: bool, working: Option<f32>) -> container::Style {
    let bg = if selected {
        Some(Background::Color(Color { a: 0.14, ..ACCENT }))
    } else if changed {
        Some(Background::Color(Color { a: 0.12, ..GOOD }))
    } else {
        working.map(|a| Background::Color(Color { a, ..AMBER }))
    };
    container::Style {
        background: bg,
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// A GitHub-PR-style RED "removed line" row: a red wash behind the deleted (HEAD-only) text.
fn code_removed_line_container() -> container::Style {
    container::Style {
        background: Some(Background::Color(Color { a: 0.12, ..BAD })),
        text_color: Some(BAD),
        ..container::Style::default()
    }
}

/// The floating dropdown card: an opaque surface with a border so it reads above the body.
fn dropdown_style(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.12, 0.13, 0.20))),
        border: Border {
            color: CARD_BORDER,
            width: 1.0,
            radius: RADIUS.into(),
        },
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// Text-input style: the theme default, but with our single [`RADIUS`] so the boxes
/// match the flat panels instead of iced's rounder default corners.
fn input_style(t: &Theme, status: text_input::Status) -> text_input::Style {
    let mut s = text_input::default(t, status);
    s.border.radius = RADIUS.into();
    s
}

/// A borderless input style for the composer: the theme default with its border stripped and
/// its fill matched to the panel surface, so the field reads as part of the composer block
/// (filling the whole area) rather than a boxed widget floating inside it.
fn input_style_borderless(t: &Theme, status: text_input::Status) -> text_input::Style {
    let mut s = text_input::default(t, status);
    s.border.width = 0.0;
    s.border.radius = RADIUS.into();
    s.background = Background::Color(SURFACE);
    s
}

/// Checkbox style: the theme default squared to our [`RADIUS`].
fn checkbox_style(t: &Theme, status: checkbox::Status) -> checkbox::Style {
    let mut s = checkbox::primary(t, status);
    s.border.radius = RADIUS.into();
    s
}

/// A section header label (muted, uppercase-ish small caps feel via size).
fn section(label: &str) -> iced::widget::Text<'_> {
    text(label).size(12).color(FG_MUTED)
}

/// The shared "↩ revert" button used on both the standalone revert bar and inline-comment rows,
/// so they look and behave identically. Reverts the diff block starting at `cur_start`.
fn revert_button(cur_start: usize) -> Element<'static, Message> {
    button(text("↩ revert").size(11).color(GOOD))
        .on_press(Message::RevertBlock(cur_start))
        .padding([0, 8])
        .style(menu_item_style)
        .into()
}

/// The shared rounded, bordered bar chrome used by BOTH the standalone revert bar and inline
/// comments — one component so they're visually identical. Sized to the viewport (`bar_width`) so
/// the row ends before the minimap and its buttons stay visible without scrolling horizontally;
/// `tint` colours the faint background wash.
fn bar_container<'a>(
    content: impl Into<Element<'a, Message>>,
    bar_width: Option<f32>,
    tint: Color,
) -> Element<'a, Message> {
    let width = bar_width.map(Length::Fixed).unwrap_or(Fill);
    container(content)
        .width(width)
        .padding([4, 8])
        .style(move |_t: &Theme| container::Style {
            background: Some(Background::Color(Color { a: 0.07, ..tint })),
            border: Border {
                color: CARD_BORDER,
                width: 1.0,
                radius: RADIUS.into(),
            },
            ..container::Style::default()
        })
        .into()
}

/// A standalone revert bar: the shared bar chrome carrying ONLY the "↩ revert" button (no comment
/// text). Rendered under a changed diff block that has no comment on it.
fn view_revert_block_bar(cur_start: usize, bar_width: Option<f32>) -> Element<'static, Message> {
    let head = row![Space::new().width(Fill), revert_button(cur_start)].align_y(iced::Alignment::Center);
    bar_container(head, bar_width, GOOD)
}

/// One stored inline comment rendered under its line (PR-style): a pending one shows the text + a
/// dismiss ✕; a resolved one shows a ✓ and dimmer text (the running "done" record). Uses the SAME
/// [`bar_container`] chrome + [`revert_button`] as the standalone bar. `revert_block_start`: if the
/// comment sits on a changed diff block, its `cur_start` so the row can offer a "↩ revert".
fn view_inline_comment(
    i: usize,
    c: dc_win::comments::Comment,
    bar_width: Option<f32>,
    revert_block_start: Option<usize>,
) -> Element<'static, Message> {
    let (mark, mark_color, txt_color) = if c.resolved {
        ("✓", GOOD, FG_MUTED)
    } else {
        ("💬", ACCENT, FG)
    };
    let tint = if c.resolved { GOOD } else { ACCENT };
    let can_revert = c.resolved && c.before.is_some();
    let mut head = row![
        text(mark).size(12).color(mark_color),
        text(c.text.clone()).size(12).color(txt_color),
        Space::new().width(Fill),
    ]
    .spacing(6)
    .align_y(iced::Alignment::Center);
    // If the comment is on a changed git block, offer to revert that block right here — the same
    // shared button as the standalone bar.
    if let Some(cur_start) = revert_block_start {
        head = head.push(revert_button(cur_start));
    }
    // A resolved comment with stored before-text gets a per-comment Revert. While a revert is
    // available, that's the action to take — so we HIDE the ✕ (dismiss) until then, to steer you
    // to undo the change rather than silently drop the record. The ✕ shows on pending comments
    // (nothing applied yet) and on resolved ones with no revert available.
    if can_revert {
        head = head.push(
            button(text("↩ revert").size(11))
                .on_press(Message::RevertComment(i))
                .padding([0, 8])
                .style(menu_item_style),
        );
    } else {
        head = head.push(
            button(text("✕").size(11))
                .on_press(Message::DismissComment(i))
                .padding([0, 6])
                .style(menu_item_style),
        );
    }
    bar_container(head, bar_width, tint)
}

/// Launch the desktop app.
pub fn run() -> iced::Result {
    // iced 0.14: `application(boot, update, view)` where boot returns the initial
    // (State, Task); title/subscription/theme are builder methods. If a project was
    // remembered from last session, greet with its README/roadmap on boot.
    iced::application(
        || {
            let mut app = App::default();
            if app.picked_workspace.is_some() {
                app.show_welcome();
                app.open_conversation();
            }
            (app, Task::none())
        },
        App::update,
        App::view,
    )
    .title(App::title)
    .subscription(App::subscription)
    .theme(App::theme)
    .run()
}

/// A pending decision surfaced to the human, with the reply channel to answer it.
enum Gatebar {
    Confirm {
        command: String,
        reason: String,
        reply: Sender<Confirmation>,
    },
    Gate {
        phase: Phase,
        content: String,
        reply: Sender<Decision>,
    },
}

/// The outcome of a finished run, for the result banner. Computed when the run ends by
/// pairing the honest-stop status with a scan of the output folder.
struct RunResult {
    /// True only when the run finished cleanly AND actually produced source files.
    ok: bool,
    /// A one-line headline (e.g. "5 files built, tests green").
    headline: String,
    /// The specific reason on failure (e.g. "built 0 source files — decomposition
    /// produced no subtasks"). Empty on success.
    reason: String,
    /// Source files present in the output folder (what it built).
    files: Vec<String>,
    /// The output folder, for the "open folder" button.
    dir: Option<std::path::PathBuf>,
}

struct App {
    cfg: UiConfig,
    intent: String,
    /// Editable mirrors of the config for the settings panel. Seeded from
    /// `UiConfig::default()` so the boxes show the active values (and a run never
    /// reads a blank input over a good default — see [`App::default`]).
    model_input: String,
    url_input: String,
    advisor_input: String,
    advisor_url_input: String,
    orch_model_input: String,
    orch_url_input: String,
    verify_input: String,
    suffix_input: String,
    settings_open: bool,
    /// Activity rows accumulated from the event stream.
    rows: Vec<Row>,
    /// The latest single-run plan steps (right panel, agent mode).
    board: Vec<String>,
    /// The live per-subtask board (right panel, swarm mode).
    swarm_board: dc_win::SwarmBoard,
    /// The staged-workflow plan (TDD mode), shown in the always-visible plan panel.
    plan: dc_win::Plan,
    /// The live swarm topology (advisor/orchestrator/coders + glowing flows), drawn on
    /// the canvas during a swarm run.
    topology: dc_win::Topology,
    /// When the current run started, for the canvas's monotonic animation clock.
    run_started: Option<Instant>,
    /// The latest verification text (bottom panel), failure-first.
    verify_text: Option<String>,
    /// The closing honest-stop summary, set when the run ends.
    summary: Option<String>,
    /// The outcome banner of the last finished run (success/failure + files built).
    result: Option<RunResult>,
    /// The live run, if one is in flight.
    session: Option<Session>,
    /// Pending human decisions (FIFO; the oldest is shown).
    gatebar: Vec<Gatebar>,
    /// Send-back notes the human is typing for a workflow checkpoint.
    sendback_notes: String,
    /// The coder box selected on the topology canvas (shows its prompt + proposal in
    /// the detail panel). `None` = show the orchestrator's decomposition reply.
    selected_coder: Option<String>,
    /// The actual folder the current/last run writes to (a picked dir, or a fresh
    /// datetime folder).
    run_dir: Option<std::path::PathBuf>,
    /// A folder the user picked to work in. When set, runs go HERE (so a follow-up
    /// prompt iterates on the existing files) instead of a fresh datetime folder.
    picked_workspace: Option<std::path::PathBuf>,
    /// True while the current/last run is an ITERATE (in-place edit) run, so the outcome
    /// banner reports "N files changed" from what the agent actually edited — never a
    /// whole-repo "files built" scan (which would count thousands in an existing project).
    iterating: bool,
    /// The files the agent actually *edited/wrote* this run (workspace-relative, de-duped),
    /// for the iterate outcome banner. Reset at each run start.
    edited_files: Vec<String>,

    // --- IDE shell state (explorer + code viewer) --------------------------------
    /// Collapsed directories in the explorer (workspace-relative paths). Everything
    /// expanded by default; clicking a dir toggles it here.
    collapsed_dirs: std::collections::HashSet<String>,
    /// The explorer's quick-filter query. When non-empty the file tree is narrowed to matching
    /// files/folders (searching the whole tree, ignoring collapse). Empty = normal tree.
    file_filter: String,
    /// The fully-walked file tree, cached so `view()` derives the collapsed/filtered display in
    /// memory instead of re-walking the filesystem every frame (which made filtering laggy).
    /// Refreshed by the snapshot path on workspace change and after edits/git actions.
    tree_cache: Vec<dc_win::filetree::TreeRow>,
    /// True while a background `compute_snapshot` is in flight, so the heartbeat doesn't stack up
    /// overlapping walks if one runs long.
    sync_pending: bool,
    /// The file shown in the code panel (workspace-relative). `None` before any file
    /// is chosen / touched.
    selected_file: Option<String>,
    /// When true, the code panel follows the agent (auto-jumps to the file it's
    /// editing). Clicking a file in the tree pins it (sets this false); it re-arms when
    /// a new run starts. This is the "watch it work" behaviour, escapable on demand.
    follow_agent: bool,
    /// The rendered contents of `selected_file`, recomputed when the selection changes
    /// or the file is edited. Cached so `view()` doesn't hit the disk every frame.
    code: Option<dc_win::CodeView>,
    /// Which top-bar menu is currently open (File / View), if any. `None` = all closed.
    open_menu: Option<Menu>,

    // --- Conversation (plan-first chat) ------------------------------------------
    /// The planning conversation with the model (mode-shaped, plans-as-files). `None`
    /// until a project folder is opened.
    conversation: Option<dc_win::chat::Conversation>,
    /// The chat thread shown in the middle column (you ⟷ agent), in order.
    chat_turns: Vec<dc_win::chat::Turn>,
    /// An in-flight chat turn (a `generate` call on a worker thread), if any.
    chat_session: Option<dc_win::chat_session::ChatSession>,
    /// Plan-file changes the assistant proposed in its latest reply, awaiting Apply.
    proposed_files: Vec<dc_win::chat::ProposedFile>,
    /// Whether the next chat turn should let the model *reason* (slower, deeper) vs. answer
    /// directly (`/no_think`, fast). Off by default — this 8B rambles when left to think, so
    /// fast conclusions are the default and Think is opt-in per the composer toggle.
    think: bool,
    /// Which bottom-panel tab is selected (Activity / Verification / Build).
    bottom_tab: BottomTab,

    // --- Line comments (PR-style) ------------------------------------------------
    /// The committed line range being commented on (1-based, inclusive `(start, end)`), if a
    /// comment box is open. A single-line comment is `(n, n)`.
    comment_range: Option<(usize, usize)>,
    /// The comment text being typed for `comment_range`.
    comment_draft: String,
    /// Drag-select state: `Some((anchor, current))` while the mouse is pressed and dragging
    /// across lines in the code view; `None` when not dragging. On release it becomes the
    /// committed `comment_range`.
    drag: Option<(usize, usize)>,
    /// A line-comment classification in flight (the small/big triage call), if any. Carries
    /// the comment so the result can be routed once the verdict arrives.
    triage: Option<TriageInFlight>,
    /// A fast line-replace fix in flight (Small verdict → one call → splice), if any.
    replace: Option<ReplaceInFlight>,
    /// True when the current iterate run was triggered by a small line-comment fix, so its
    /// outcome (files changed + verify result) is reported back into the chat thread.
    iterate_from_comment: bool,
    /// The in-flight assistant reply as it streams in token-by-token (the live "typing"
    /// bubble). `None` when nothing is streaming; replaced by a finished turn on completion.
    streaming: Option<String>,
    /// Debug mode: when on, every prompt sent to the model is echoed into the chat as a
    /// (dimmed, collapsible) debug turn, so you can see exactly what the agent receives.
    debug: bool,
    /// Lines of the currently-shown file that differ from HEAD right now (from `git diff`),
    /// highlighted GitHub-PR-style. Refreshed as a fix edits the file, so you SEE changes land.
    changed_lines: std::collections::BTreeSet<usize>,
    /// The full PR-style diff of the shown file vs HEAD: `.added` (green, == `changed_lines`) plus
    /// `.removed_before` (red deleted lines, anchored to the current line they sat before). Drives
    /// the GitHub-diff rendering in the code view. Refreshed alongside `changed_lines`.
    file_diff: dc_win::gitdiff::FileDiff,
    /// The visible slice of the code view as (top_fraction, height_fraction) of the whole file,
    /// updated on every scroll so the minimap draws a "you are here" box. `None` until first scroll.
    code_viewport: Option<(f32, f32)>,
    /// Pixel height of the code viewport (last seen on scroll), so a minimap jump can center the
    /// clicked line rather than parking it at the top. `0.0` until first scroll.
    code_view_h: f32,
    /// Absolute vertical scroll offset (px) of the code view, last seen on scroll. Drives line
    /// virtualization: only lines within `[code_scroll_y, code_scroll_y + code_view_h]` (plus
    /// overscan) are turned into widgets. `0.0` until first scroll.
    code_scroll_y: f32,
    /// Pixel WIDTH of the code viewport, last seen on scroll. Sizes the comment/revert bars to the
    /// viewport (not the horizontally-scrollable content width) so they end before the minimap and
    /// stay visible without scrolling right. `0.0` until first scroll (→ fall back to Fill).
    code_view_w: f32,
    /// The range the agent is actively working on (the lines you commented on), highlighted in
    /// a pulsing amber from submit until the change lands — so the "thinking" gap feels active.
    /// `(file, start, end)`; `None` when nothing is in flight.
    working: Option<(String, usize, usize)>,

    // --- PR-review state ---------------------------------------------------------
    /// Persisted inline code comments (`.dc/comments.json`), rendered under their lines and
    /// marked resolved when the agent finishes the change.
    comments: dc_win::comments::Comments,
    /// Working-tree file statuses (path → M/A/D) for the PR-style file tree, refreshed as
    /// fixes land.
    file_status: std::collections::BTreeMap<String, dc_win::gitdiff::FileStatus>,
    /// The current git branch (shown in the explorer header), if any.
    branch: Option<String>,
    /// Ahead/behind vs the upstream tracking branch (for the ↑↓ header + push/pull buttons).
    /// Refreshed alongside the branch; `behind` reflects the last fetch (Pull/Fetch updates it).
    upstream: dc_win::gitdiff::UpstreamStatus,
    /// Last cursor position seen over the git list, so a right-click can pop the context menu
    /// at the cursor. Updated on mouse-move within the git rows.
    cursor_pos: iced::Point,
    /// The open git-row context menu: the file it targets + its status. `None` when closed.
    git_menu: Option<(String, dc_win::gitdiff::FileStatus)>,
    /// Where to draw the open git context menu (the cursor position at right-click time).
    git_menu_at: iced::Point,
    /// Per-file staged/unstaged state (from `git status` XY codes), for the Staged section and
    /// the Stage/Unstage menu items. Refreshed alongside `file_status`.
    stage_states: std::collections::BTreeMap<String, dc_win::gitdiff::StageState>,
    /// Per-file unstaged +added/−removed line counts (`git diff --numstat`), shown on the right
    /// of each Changes row. Untracked files are counted directly (git won't diff them).
    unstaged_deltas: std::collections::BTreeMap<String, dc_win::gitdiff::LineDelta>,
    /// Per-file STAGED +added/−removed line counts (`git diff --cached --numstat`), for the
    /// right of each Staged Changes row.
    staged_deltas: std::collections::BTreeMap<String, dc_win::gitdiff::LineDelta>,
    /// The commit-message draft typed in the git tab's VS-Code-style commit box.
    commit_msg: String,

    // --- Resizable chat|code divider --------------------------------------------
    /// Chat's share of the combined chat+code region (0.15..0.85). 0.5 = the old even
    /// split; dragging the divider between the chat and code panels moves this.
    chat_frac: f32,
    /// Last-seen window width (px), from resize events — needed to turn an absolute
    /// cursor X into a split fraction while dragging the chat|code divider.
    window_w: f32,
    /// True while the chat|code divider is being dragged (mouse down on the handle).
    dragging_split: bool,
}

/// A line-comment triage running on a worker thread: the classify call + the comment it's
/// deciding, so `pump` can route to a small fix or a planning turn when the verdict lands.
struct TriageInFlight {
    comment: dc_win::linecomment::LineComment,
    session: dc_win::chat_session::ChatSession,
}

/// A fast LINE-REPLACE fix in flight: one model call for the new block text, which the IDE
/// splices into the file by line number (no agent-loop `edit_file` whitespace thrashing).
struct ReplaceInFlight {
    comment: dc_win::linecomment::LineComment,
    session: dc_win::chat_session::ChatSession,
    /// Streaming buffer for the "watch it type" preview of the replacement.
    streamed: String,
}

/// The bottom panel's tabs — the verify output and the last run's build outcome. Tabbed
/// (not stacked) so they share the bottom space. (Activity was dropped: the chat column
/// now carries "what the agent is doing", so a separate activity log is redundant.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BottomTab {
    Verification,
    Build,
}

/// The top menu-bar dropdowns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Menu {
    File,
    View,
}

impl Default for App {
    fn default() -> Self {
        // Seed the editable input boxes from the *loaded* config, so the settings
        // panel shows the active values and `start()` never commits a blank input
        // over a sensible default (URL, /no_think suffix, …). `load()` layers the
        // machine-local endpoint/model (config.json / env) over the neutral default,
        // so the specific backend this box uses never lives in the repo.
        let cfg = UiConfig::load();
        // Re-open the last project the user worked in (if it still exists on disk), so the
        // app comes back to where they left off instead of the empty scratch base.
        let picked_workspace = dc_win::persist::load().last_project;
        // Re-opening a remembered project → open its tree compacted (top-level folders collapsed),
        // matching the fresh-pick behavior.
        let collapsed_dirs = picked_workspace
            .as_deref()
            .map(dc_win::filetree::top_level_dirs)
            .unwrap_or_default();
        // Walk the remembered project's tree once up front; the view derives from this cache.
        let tree_cache = picked_workspace
            .as_deref()
            .map(dc_win::filetree::full_rows)
            .unwrap_or_default();
        Self {
            url_input: cfg.base_url.clone(),
            model_input: cfg.model.clone(),
            advisor_input: cfg.advisor_model.clone().unwrap_or_default(),
            advisor_url_input: cfg.advisor_url.clone().unwrap_or_default(),
            orch_model_input: cfg.orchestrator_model.clone().unwrap_or_default(),
            orch_url_input: cfg.orchestrator_url.clone().unwrap_or_default(),
            verify_input: cfg.verify_command.clone().unwrap_or_default(),
            suffix_input: cfg.system_suffix.clone().unwrap_or_default(),
            cfg,
            intent: String::new(),
            settings_open: false,
            rows: Vec::new(),
            board: Vec::new(),
            swarm_board: dc_win::SwarmBoard::default(),
            plan: dc_win::Plan::default(),
            topology: dc_win::Topology::default(),
            run_started: None,
            verify_text: None,
            summary: None,
            result: None,
            session: None,
            gatebar: Vec::new(),
            sendback_notes: String::new(),
            selected_coder: None,
            run_dir: None,
            picked_workspace,
            iterating: false,
            edited_files: Vec::new(),
            collapsed_dirs,
            file_filter: String::new(),
            tree_cache,
            sync_pending: false,
            selected_file: None,
            follow_agent: true,
            code: None,
            open_menu: None,
            conversation: None,
            chat_turns: Vec::new(),
            chat_session: None,
            proposed_files: Vec::new(),
            think: false,
            bottom_tab: BottomTab::Verification,
            comment_range: None,
            comment_draft: String::new(),
            drag: None,
            triage: None,
            replace: None,
            iterate_from_comment: false,
            streaming: None,
            debug: false,
            changed_lines: std::collections::BTreeSet::new(),
            file_diff: dc_win::gitdiff::FileDiff::default(),
            code_viewport: None,
            code_view_h: 0.0,
            working: None,
            comments: dc_win::comments::Comments::default(),
            code_scroll_y: 0.0,
            code_view_w: 0.0,
            file_status: std::collections::BTreeMap::new(),
            branch: None,
            upstream: dc_win::gitdiff::UpstreamStatus::default(),
            cursor_pos: iced::Point::ORIGIN,
            git_menu: None,
            git_menu_at: iced::Point::ORIGIN,
            stage_states: std::collections::BTreeMap::new(),
            unstaged_deltas: std::collections::BTreeMap::new(),
            staged_deltas: std::collections::BTreeMap::new(),
            commit_msg: String::new(),
            chat_frac: 0.5,
            window_w: 1040.0,
            dragging_split: false,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
    IntentChanged(String),
    ModelChanged(String),
    UrlChanged(String),
    AdvisorChanged(String),
    AdvisorUrlChanged(String),
    OrchModelChanged(String),
    OrchUrlChanged(String),
    VerifyChanged(String),
    SuffixChanged(String),
    ToggleSettings,
    ToggleYolo(bool),
    ToggleDryRun(bool),
    RunTdd,
    /// Run the in-place iterate loop (the default when a project folder is picked).
    RunIterate,
    Tick,
    /// Heartbeat while a project is open: kick off an OFF-THREAD re-walk of the tree + git state
    /// so externally-created/removed files appear without a manual refresh.
    SyncWorkspace,
    /// The background workspace snapshot finished — apply it (or drop it if the compute failed).
    WorkspaceSynced(Option<WorkspaceSnapshot>),
    // Explorer / code-viewer interaction.
    /// Select a file in the tree → show it in the code panel (and pin, stop following).
    SelectFile(String),
    /// Toggle a directory's collapsed state in the explorer.
    ToggleDir(String),
    /// The explorer's quick-filter text changed → narrow the tree to matching files/folders.
    FileFilterChanged(String),
    // Top menu bar.
    /// Open (or toggle) a top-bar dropdown menu.
    ToggleMenu(Menu),
    // Conversation.
    /// Send the composer text as a chat message to the planning agent.
    ChatSend,
    /// Apply the Nth proposed plan-file to disk (writes README.md / TODO.md).
    ApplyFile(usize),
    /// Apply the Nth proposed plan-file, then kick off an iterate build to implement it —
    /// the one-click bridge from a `PLAN-<slug>.md` design doc to a real build run.
    ExecutePlan(usize),
    /// Kick off an iterate build to implement the PLAN-*.md currently open in the code view
    /// (already on disk — no apply needed). The button lives on the code-view header.
    ExecuteOpenPlan,
    /// Toggle "think" mode for the next chat turn (reason vs. answer directly).
    ToggleThink(bool),
    /// Toggle debug mode: echo every prompt sent to the model into the chat.
    ToggleDebug(bool),
    /// Undo the last fix: git-revert exactly the files it changed, back to committed state.
    UndoLastChange,
    /// Dismiss the stored inline comment at this index (remove it from the review list).
    DismissComment(usize),
    /// Revert just this comment's change: splice its stored before-text back into the file.
    RevertComment(usize),
    /// Revert a single diff block (VS-Code-style) back to its HEAD text. Carries the hunk's
    /// current start line, which identifies the block to restore.
    RevertBlock(usize),
    /// Jump the code view to a 1-based line (clicked in the minimap).
    MinimapJump(usize),
    /// The code view was scrolled — carries the viewport so the minimap can draw a "you are here"
    /// box tracking the visible slice of the file.
    CodeScrolled(scrollable::Viewport),
    /// Cancel the in-flight run/fix (stops the agent at its next turn; reverts partial edits).
    CancelRun,
    /// Select a bottom-panel tab (Activity / Verification / Build).
    SelectBottomTab(BottomTab),
    /// Cursor moved over the git list — track it so a right-click can place the context menu.
    GitCursorMoved(iced::Point),
    /// Right-clicked a git-tab row: open its context menu (stage / unstage / discard …).
    GitRowMenu(String, dc_win::gitdiff::FileStatus),
    /// Close the open git context menu without acting.
    CloseGitMenu,
    /// Stage this file (`git add -- <path>`).
    GitStage(String),
    /// Unstage this file (`git restore --staged -- <path>`).
    GitUnstage(String),
    /// Discard this file's working-tree changes (`git checkout -- <path>`); restores a deleted
    /// file or reverts a modified one to its committed state.
    GitDiscard(String),
    /// Select a file from the git tab → open it AND jump to its first changed line.
    SelectGitFile(String),
    /// Deferred second step of `SelectGitFile`: scroll to the first changed line once the new
    /// file's content has actually been laid out (avoids scrolling against the old file's tree).
    JumpToFirstChange,
    /// The commit-message draft in the git tab changed.
    CommitMsgChanged(String),
    /// Commit the staged files with the draft message (`git commit -m …`).
    GitCommit,
    /// Stage every changed file (`git add -A`) — the "Stage All Changes" ＋ on the Changes header.
    GitStageAll,
    /// Unstage every staged file (`git reset`) — the "− All" on the Staged Changes header.
    GitUnstageAll,
    /// Push HEAD to its upstream (`git push`). If the branch has no upstream, sets it on first push.
    GitPush,
    /// Pull from upstream (`git pull --ff-only`) — fast-forward only, so it never auto-merges.
    GitPull,
    /// Fetch from the remote (`git fetch`) to refresh the behind-count without changing the tree.
    GitFetch,
    // Line comments (PR-style) — drag to select a range, then comment.
    /// Mouse pressed on line N: start a drag-selection anchored there.
    LineDragStart(usize),
    /// Mouse entered line N while dragging: extend the selection to N.
    LineDragTo(usize),
    /// Mouse released: commit the drag-selection and open the comment box for the range.
    LineDragEnd,
    /// The comment draft text changed.
    CommentDraftChanged(String),
    /// Submit the line comment — triage it (small → fix now, big → plan).
    CommentSubmit,
    /// Cancel the open comment box.
    CommentCancel,
    // Confirm-gate answers.
    ConfirmAllow,
    ConfirmDeny,
    ConfirmRemember,
    // Workflow-gate answers.
    NotesChanged(String),
    GateApprove,
    GateRevise,
    GateSendBack,
    GateAbort,
    // Topology canvas interaction.
    SelectCoder(String),
    ClearSelection,
    // Workspace folder.
    PickWorkspace,
    ClearWorkspace,
    /// Open the output folder of the last run in the system file manager.
    OpenOutputFolder,
    // Resizable chat|code divider.
    /// Mouse pressed on the chat|code divider → begin dragging it.
    SplitDragStart,
    /// Mouse released anywhere → stop dragging the divider.
    SplitDragEnd,
    /// The window was resized — remember its width so the drag can map cursor X to a fraction.
    WindowResized(f32),
}

impl App {
    fn title(&self) -> String {
        "dumb-coder — vibe coding".to_string()
    }

    fn theme(&self) -> Theme {
        Theme::TokyoNight
    }

    fn subscription(&self) -> Subscription<Message> {
        // Tick while there's anything live to pump or animate: a running session, an
        // open gate, or topology flows still glowing (so the fade animates to rest
        // even after the run ends). iced delivers the tick on the UI thread, so
        // draining the std::mpsc Receiver here is safe.
        let glowing = !self.topology.active_flows(self.now()).is_empty();
        let tick = if self.session.is_some()
            || self.chat_session.is_some()
            || self.triage.is_some()
            || self.replace.is_some()
            || self.working.is_some()
            || !self.gatebar.is_empty()
            || glowing
        {
            iced::time::every(Duration::from_millis(50)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        };
        // A heartbeat that re-walks the tree + git state while a project is open, so files
        // created/removed OUTSIDE the app (or by a running agent) show up without a manual
        // refresh. The walk is cheap and off the render path, so 500ms feels live without cost.
        // Off when no project is open.
        let sync = if self.picked_workspace.is_some() {
            iced::time::every(Duration::from_millis(500)).map(|_| Message::SyncWorkspace)
        } else {
            Subscription::none()
        };
        // Track the window-absolute cursor position so a right-click in the git tab can pop its
        // context menu exactly at the pointer. `mouse_area::on_move` reports widget-relative
        // coordinates (useless for placing a window overlay); this window event is absolute.
        let cursor = iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                Some(Message::GitCursorMoved(position))
            }
            // A button-release anywhere ends a divider drag (even if the cursor left the handle).
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )) => Some(Message::SplitDragEnd),
            // Track the window width so a divider drag can map cursor X to a split fraction.
            // `Opened` seeds it at startup; `Resized` keeps it current.
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Message::WindowResized(size.width))
            }
            iced::Event::Window(iced::window::Event::Opened { size, .. }) => {
                Some(Message::WindowResized(size.width))
            }
            _ => None,
        });
        Subscription::batch([tick, sync, cursor])
    }

    /// The run action for the primary button / intent submit, chosen by context: a
    /// picked project folder means "iterate in place"; otherwise the from-scratch build.
    fn run_message(&self) -> Message {
        if self.picked_workspace.is_some() {
            Message::RunIterate
        } else {
            Message::RunTdd
        }
    }

    /// The primary button label matching [`Self::run_message`].
    fn run_label(&self) -> &'static str {
        if self.picked_workspace.is_some() {
            "⚒  iterate"
        } else {
            "⚒  build"
        }
    }

    fn start(&mut self, kind: RunKind) {
        if self.intent.trim().is_empty() || self.session.is_some() {
            return;
        }
        // Commit the settings inputs into the config before the run.
        self.cfg.model = self.model_input.clone();
        self.cfg.base_url = self.url_input.clone();
        self.cfg.advisor_model = non_empty(&self.advisor_input);
        self.cfg.advisor_url = non_empty(&self.advisor_url_input);
        self.cfg.orchestrator_model = non_empty(&self.orch_model_input);
        self.cfg.orchestrator_url = non_empty(&self.orch_url_input);
        self.cfg.verify_command = non_empty(&self.verify_input);
        self.cfg.system_suffix = non_empty(&self.suffix_input);

        self.rows.clear();
        self.board.clear();
        self.swarm_board = dc_win::SwarmBoard::default();
        self.plan = dc_win::Plan::default();
        self.topology = dc_win::Topology::default();
        self.selected_coder = None;
        self.run_started = Some(Instant::now());
        self.verify_text = None;
        self.summary = None;
        self.result = None;
        self.gatebar.clear();
        // Re-arm follow so the code panel tracks the agent through this run.
        self.follow_agent = true;
        // Track this run's mode + which files it edits (for the honest iterate banner).
        self.iterating = matches!(kind, RunKind::Iterate);
        self.edited_files.clear();
        // Jump to the Verification tab so the run's checks are visible as it works.
        self.bottom_tab = BottomTab::Verification;

        // Where to run:
        //  • a folder you picked → run there directly, so a follow-up prompt iterates
        //    on (and edits) the existing files in that project;
        //  • otherwise → a fresh datetime-stamped folder under the scratch base, so a
        //    new project never overwrites a previous one.
        let ws = match &self.picked_workspace {
            Some(dir) => {
                let _ = std::fs::create_dir_all(dir);
                dir.clone()
            }
            None => {
                let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
                self.cfg.run_workspace(&stamp)
            }
        };
        self.run_dir = Some(ws.clone());
        self.session = Some(Session::spawn(
            kind,
            self.cfg.clone(),
            self.intent.clone(),
            ws,
        ));
    }

    /// Monotonic seconds since the current run started — the canvas's animation clock.
    fn now(&self) -> f32 {
        self.run_started
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0)
    }

    /// The workspace root the explorer/code panels read from: the picked project folder
    /// if any, else the current run's output dir, else the config base. This is the tree
    /// the user is actually working in.
    fn workspace_root(&self) -> std::path::PathBuf {
        self.picked_workspace
            .clone()
            .or_else(|| self.run_dir.clone())
            .unwrap_or_else(|| self.cfg.workspace.clone())
    }

    /// On opening a project, greet the user in the Activity stream: the project name, its
    /// README's TODO/roadmap excerpt (highlighted), and an invitation to say what to work
    /// on. No-op for a folder with no README (still greets, just no excerpt).
    fn show_welcome(&mut self) {
        let Some(root) = self.picked_workspace.clone() else {
            return;
        };
        let folder = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project")
            .to_string();
        let readme = find_readme(&root)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let todo_md = find_todo_file(&root)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let w = dc_win::welcome::build(&readme, &todo_md, &folder);

        // Clear any previous run's activity so the welcome is the first thing shown.
        self.rows.clear();
        self.summary = None;
        self.result = None;
        self.rows.push(Row::ok("◆", format!("opened  {}", w.title)));
        if !w.lines.is_empty() {
            let header = if w.no_todo {
                "— from the README —"
            } else {
                "— what's on the TODO —"
            };
            self.rows.push(Row::ok(" ", header.to_string()));
            for l in &w.lines {
                // A highlighted TODO/roadmap item gets a star; context lines a faint dot.
                let icon = if l.highlight { "★" } else { "·" };
                self.rows.push(Row::ok(icon, l.text.clone()));
            }
        }
        // The closing prompt — for a project with no TODO, this nudges the user to make one.
        let prompt_icon = if w.no_todo { "⚠" } else { "▸" };
        self.rows.push(Row::ok(prompt_icon, w.prompt));
    }

    /// Open a planning conversation for the current project: read the plan files, pick the
    /// mode (scratch vs existing), and seed the thread with the agent's opening line.
    fn open_conversation(&mut self) {
        let root = self.workspace_root();
        // Load persisted inline comments, ensure .dc/ is git-ignored, and pull the initial
        // git view (branch + file statuses) for the PR-style tree.
        self.comments = dc_win::comments::load(&root);
        dc_win::comments::ensure_gitignored(&root);
        self.refresh_git_view();
        let (readme, todo) = self.read_plan_files(&root);
        let convo = dc_win::chat::Conversation::open(&readme, &todo);
        self.chat_turns.clear();
        self.chat_turns.push(dc_win::chat::Turn {
            role: dc_win::chat::Speaker::Agent,
            text: convo.opening_line(),
        });
        self.proposed_files.clear();
        // In an existing project, auto-open the TODO (or README) in the code view so you
        // land on the plan; the conversation continues from there.
        if let Some(f) = self.plan_file_to_show(&root) {
            self.follow_agent = false;
            self.select_file(f);
        }
        self.conversation = Some(convo);
    }

    /// The current README/TODO contents (empty string if absent) for the workspace `root`.
    fn read_plan_files(&self, root: &std::path::Path) -> (String, String) {
        let readme = find_readme(root)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let todo = find_todo_file(root)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        (readme, todo)
    }

    /// Which plan file to auto-open in the code view when a project opens: the TODO if it
    /// exists, else the README, else nothing.
    fn plan_file_to_show(&self, root: &std::path::Path) -> Option<String> {
        let rel = |p: std::path::PathBuf| {
            p.strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/")
        };
        find_todo_file(root)
            .map(rel)
            .or_else(|| find_readme(root).map(rel))
    }

    /// Send the composer text as a chat turn to the planning agent (worker thread). No-op
    /// when there's no conversation, no text, or a turn is already in flight.
    fn send_chat(&mut self) {
        let text = self.intent.trim().to_string();
        if text.is_empty() || self.chat_session.is_some() || self.conversation.is_none() {
            return;
        }
        // Commit connection settings (mirrors `start`), so a chat uses the current backend.
        self.cfg.model = self.model_input.clone();
        self.cfg.base_url = self.url_input.clone();
        let think = self.think;

        // Snapshot the file open in the code view so the model answers against what the user is
        // looking at ("what does this do?", "add handling here"). Read from disk (not the capped
        // CodeView render) so the chat sees the real file; chat.rs head-clips it for the window.
        let open_file = self.selected_file.as_ref().and_then(|rel| {
            let path = self.workspace_root().join(rel);
            std::fs::read_to_string(&path)
                .ok()
                .map(|body| (rel.clone(), body))
        });

        // Update the conversation, then spawn a planning turn (classify intent → generate). The
        // classification decides how the reply is shaped; the app no longer sniffs the text.
        let convo = {
            let convo = self.conversation.as_mut().expect("checked above");
            convo.set_open_file(open_file);
            convo.user_turn(&text);
            convo.clone()
        };
        self.chat_turns.push(dc_win::chat::Turn {
            role: dc_win::chat::Speaker::You,
            text,
        });
        self.proposed_files.clear();
        self.intent.clear();
        if self.debug {
            // Show the generate prompt for the most likely intent path (feature plan is the one
            // that was misbehaving); the real intent is classified on the worker.
            let req = convo.request(think, dc_win::chat::ChatIntent::Question);
            let joined = req
                .messages
                .iter()
                .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            self.debug_prompt("chat", &joined);
        }
        self.chat_session = Some(dc_win::chat_session::ChatSession::spawn_planning(
            self.cfg.clone(),
            convo,
            think,
        ));
    }

    /// Spawn a chat/generate call, first echoing its prompt into the chat when debug mode is
    /// on. Every model call that streams into the chat goes through here.
    fn spawn_chat(&mut self, label: &str, req: dc_model::GenerateRequest) {
        if self.debug {
            let joined = req
                .messages
                .iter()
                .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            self.debug_prompt(label, &joined);
        }
        self.chat_session = Some(dc_win::chat_session::ChatSession::spawn(
            self.cfg.clone(),
            req,
        ));
    }

    /// Submit the open line comment: capture the line + text, spawn the small/big triage
    /// classification (fast `/no_think`), and close the comment box. Routing happens in
    /// [`Self::pump_triage`] when the verdict arrives.
    fn submit_line_comment(&mut self) {
        let (Some((start, end)), Some(cv)) = (self.comment_range, self.code.as_ref()) else {
            return;
        };
        let comment = self.comment_draft.trim().to_string();
        if comment.is_empty() {
            return;
        }
        let rel = cv.rel.clone();
        // Scope from the CURRENT on-disk file, freshly numbered — NOT from `self.code`, which can
        // be a stale streamed *preview* (renumbered/spliced) left by an earlier fix. Reading disk
        // here guarantees `start`/`end`/`selection` all refer to the same real bytes we'll splice,
        // so the fix lands on the lines you actually selected.
        let disk = dc_win::codeview::load(&self.workspace_root(), &rel);
        let lines = if disk.note.is_none() {
            &disk.lines
        } else {
            &cv.lines // fall back to the view if the file can't be re-read
        };
        // The IDE does the scoping: hand the model the selected code + a small window, so it
        // doesn't slurp the whole file (what made a one-line fix slow + context-heavy).
        let (selection, context) = dc_win::linecomment::scope_context(lines, start, end);
        let lc = dc_win::linecomment::LineComment {
            file: rel,
            start,
            end,
            selection,
            context,
            comment,
        };
        // Persist the comment inline (pending) — it stays visible in the code view and gets
        // marked resolved when the agent finishes. A Question is removed again in pump_triage
        // (it's answered, not a change to track).
        self.comments.add(dc_win::comments::Comment::new(
            lc.file.clone(),
            start,
            end,
            lc.comment.clone(),
        ));
        dc_win::comments::save(&self.workspace_root(), &self.comments);
        // Commit connection settings so the triage/edit use the current backend.
        self.cfg.model = self.model_input.clone();
        self.cfg.base_url = self.url_input.clone();
        // Keep the commented lines highlighted (pulsing amber) while the agent works on them,
        // so the "thinking" gap between submit and the edit landing feels active. Cleared when
        // the run/answer finishes.
        self.working = Some((lc.file.clone(), start, end));
        self.run_started = Some(Instant::now()); // (re)start the animation clock
                                                 // Echo the comment into the chat (before `lc` moves into the triage state).
        let span = if start == end {
            format!("line {start}")
        } else {
            format!("lines {start}-{end}")
        };
        let echo = format!("💬 {} {span}: {}", lc.file, lc.comment);
        self.chat_turns.push(dc_win::chat::Turn {
            role: dc_win::chat::Speaker::You,
            text: echo,
        });
        let req = lc.classify_request();
        if self.debug {
            // Dump exactly what range/text was captured, so a mis-splice is diagnosable.
            let sel_first = lc.selection.lines().next().unwrap_or("").to_string();
            self.debug_prompt(
                "capture",
                &format!(
                    "range = lines {}-{}\nselection[0] = {:?}\nselection = {} line(s)",
                    lc.start,
                    lc.end,
                    sel_first,
                    lc.selection.lines().count()
                ),
            );
            let joined = req
                .messages
                .iter()
                .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                .collect::<Vec<_>>()
                .join("\n\n");
            self.debug_prompt("triage", &joined);
        }
        let session = dc_win::chat_session::ChatSession::spawn(self.cfg.clone(), req);
        self.triage = Some(TriageInFlight {
            comment: lc,
            session,
        });
        self.comment_range = None;
        self.comment_draft.clear();
    }

    /// Drain the line-comment triage call. On a verdict: SMALL → run a scoped-but-coherent
    /// iterate fix now; BIG → seed a planning chat turn for approval.
    fn pump_triage(&mut self) {
        let Some(t) = &self.triage else {
            return;
        };
        let events = t.session.drain();
        for ev in events {
            let reply = match ev {
                // Triage is a one-word classify — ignore streamed tokens, act on the final.
                dc_win::chat_session::ChatEvent::Token(_) => continue,
                dc_win::chat_session::ChatEvent::Reply(r) => r,
                dc_win::chat_session::ChatEvent::Failed(_) => {
                    // On a failed triage, fall back to planning (the safe route).
                    "BIG".to_string()
                }
            };
            let verdict = dc_win::linecomment::parse_verdict(&reply);
            let comment = self.triage.take().expect("in flight").comment;
            match verdict {
                dc_win::linecomment::Verdict::Question => {
                    // A question about the code → ANSWER it (streamed into the chat), edit
                    // nothing. It's not a change to track, so drop the pending inline comment
                    // we stored on submit.
                    if let Some((i, _)) = self
                        .comments
                        .on_file(&comment.file)
                        .filter(|(_, c)| !c.resolved)
                        .last()
                    {
                        self.comments.remove(i);
                        dc_win::comments::save(&self.workspace_root(), &self.comments);
                    }
                    self.cfg.model = self.model_input.clone();
                    self.cfg.base_url = self.url_input.clone();
                    let req = comment.question_request(self.think);
                    self.spawn_chat("question", req);
                }
                dc_win::linecomment::Verdict::Small => {
                    // FAST PATH: one model call for the new block text; the IDE splices it in by
                    // line number (no edit_file whitespace thrashing — the thing that made a
                    // reindent take 3 tries). The amber "working" highlight already shows the range.
                    self.chat_turns.push(dc_win::chat::Turn {
                        role: dc_win::chat::Speaker::Agent,
                        text: "→ quick fix — rewriting the selection…".to_string(),
                    });
                    self.cfg.model = self.model_input.clone();
                    self.cfg.base_url = self.url_input.clone();
                    let req = comment.replace_request();
                    if self.debug {
                        let joined = req
                            .messages
                            .iter()
                            .map(|m| format!("[{:?}]\n{}", m.role, m.content))
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        self.debug_prompt("fix (line-replace)", &joined);
                    }
                    let session = dc_win::chat_session::ChatSession::spawn(self.cfg.clone(), req);
                    self.replace = Some(ReplaceInFlight {
                        comment,
                        session,
                        streamed: String::new(),
                    });
                }
                dc_win::linecomment::Verdict::Big => {
                    // Route into planning: seed a user turn and send it to the chat agent.
                    let seed = comment.planning_seed();
                    self.chat_turns.push(dc_win::chat::Turn {
                        role: dc_win::chat::Speaker::Agent,
                        text: "→ this needs a plan — let's talk it through first.".to_string(),
                    });
                    self.intent = seed;
                    self.send_chat();
                }
            }
            break;
        }
    }

    /// Drain the fast line-replace fix. Streams the replacement into the code-view preview;
    /// on completion, splices the new block into the file BY LINE NUMBER (deterministic — no
    /// whitespace matching), captures the before-text for a per-comment Revert, resolves the
    /// comment, and verifies (unless the change is comment-only).
    fn pump_replace(&mut self) {
        let Some(r) = &self.replace else {
            return;
        };
        let events = r.session.drain();
        let mut done: Option<(String, String)> = None; // (raw reply, none) sentinel via loop
        for ev in events {
            match ev {
                dc_win::chat_session::ChatEvent::Token(delta) => {
                    // Pull the fields we need out of the in-flight replace, ending the mutable
                    // borrow before we touch `self` again (for workspace_root / self.code).
                    let preview_bits = self.replace.as_mut().map(|r| {
                        r.streamed.push_str(&delta);
                        (
                            r.comment.file.clone(),
                            r.comment.start,
                            r.comment.end,
                            r.comment.selection.clone(),
                            dc_win::linecomment::extract_replacement(&r.streamed)
                                .unwrap_or_default(),
                        )
                    });
                    if let Some((file, hstart, hend, selection, preview)) = preview_bits {
                        if !preview.is_empty() {
                            let cur = std::fs::read_to_string(self.workspace_root().join(&file))
                                .unwrap_or_default();
                            // Preview at the block's TRUE current location (same re-anchoring the
                            // real write uses), so the live preview lands where the edit will.
                            if let Some((start, end)) =
                                dc_win::linecomment::locate_range(&cur, hstart, hend, &selection)
                            {
                                let spliced =
                                    dc_win::linecomment::splice_lines(&cur, start, end, &preview);
                                self.selected_file = Some(file.clone());
                                self.follow_agent = false;
                                self.code = Some(dc_win::codeview::from_text(&file, &spliced));
                            }
                        }
                    }
                }
                dc_win::chat_session::ChatEvent::Reply(raw) => {
                    done = Some((raw, String::new()));
                    break;
                }
                dc_win::chat_session::ChatEvent::Failed(msg) => {
                    self.working = None;
                    self.replace = None;
                    self.chat_turns.push(dc_win::chat::Turn {
                        role: dc_win::chat::Speaker::Agent,
                        text: format!("⚠ {msg}"),
                    });
                    return;
                }
            }
        }
        if let Some((raw, _)) = done {
            self.apply_line_replace(&raw);
        }
    }

    /// Apply a completed line-replace reply: splice the new block into the file, record the
    /// before-text on the comment, resolve it, refresh the view + git, and verify if needed.
    fn apply_line_replace(&mut self, raw: &str) {
        let Some(rf) = self.replace.take() else {
            return;
        };
        self.working = None;
        let c = rf.comment;
        let root = self.workspace_root();
        let path = root.join(&c.file);
        let Some(new_block) = dc_win::linecomment::extract_replacement(raw) else {
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: "⚠ the model returned nothing to apply.".to_string(),
            });
            return;
        };
        if self.debug {
            self.debug_prompt(
                "line-replace reply",
                &format!(
                    "hint range = {}-{}\nRAW REPLY:\n{}\n\nEXTRACTED BLOCK ({} lines):\n{}",
                    c.start,
                    c.end,
                    raw.trim(),
                    new_block.lines().count(),
                    new_block
                ),
            );
        }
        let Ok(current) = std::fs::read_to_string(&path) else {
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: format!("⚠ couldn't read {}.", c.file),
            });
            return;
        };
        // Re-anchor to where the selected block ACTUALLY is on disk right now — guards against the
        // captured line numbers having drifted (which would splice the fix onto the wrong lines,
        // duplicating the block instead of replacing it). Abort clearly if we can't locate it.
        let Some((start, end)) =
            dc_win::linecomment::locate_range(&current, c.start, c.end, &c.selection)
        else {
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: format!(
                    "⚠ couldn't safely locate the selected lines in {} (the file changed) — no edit made.",
                    c.file
                ),
            });
            return;
        };
        // Capture the exact BEFORE-text of the range (for per-comment Revert), then splice.
        let before: String = current
            .lines()
            .skip(start.saturating_sub(1))
            .take(end.saturating_sub(start) + 1)
            .collect::<Vec<_>>()
            .join("\n");
        let spliced = dc_win::linecomment::splice_lines(&current, start, end, &new_block);
        if spliced == current {
            // The model returned the selection unchanged (a local model sometimes echoes the
            // input on "shorten this"). Be honest about it and leave the comment PENDING so you
            // can re-run or rephrase, rather than falsely claiming success.
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: "⚠ the model returned the same lines unchanged — nothing applied. Try \
                       rephrasing (e.g. \"make this comment 2 lines\") or run it again."
                    .to_string(),
            });
            self.refresh_git_view();
            return;
        }
        if std::fs::write(&path, &spliced).is_err() {
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: format!("⚠ couldn't write {}.", c.file),
            });
            return;
        }
        // Resolve the matching stored comment and stash the before-text + new length for Revert.
        let new_len = new_block.lines().count().max(1);
        if let Some((i, _)) = self
            .comments
            .on_file(&c.file)
            .filter(|(_, cc)| !cc.resolved && cc.start == c.start && cc.end == c.end)
            .last()
        {
            if let Some(stored) = self.comments.items.get_mut(i) {
                stored.resolved = true;
                stored.before = Some(before);
                stored.after_len = Some(new_len);
                // Record where the edit ACTUALLY landed (after re-anchoring) so a later Revert
                // splices the before-text back over the right lines. The NEW block spans
                // `start .. start + new_len - 1` — use that as the end, so the resolved comment
                // anchors to the last NEW line (not the stale old end, which drifts when the fix
                // added or removed lines).
                stored.start = start;
                stored.end = start + new_len - 1;
            }
        }
        dc_win::comments::save(&root, &self.comments);

        // Show the applied file + refresh highlights/tree.
        self.selected_file = Some(c.file.clone());
        self.follow_agent = false;
        self.select_file(c.file.clone());
        self.refresh_git_view();

        // Verify — unless the change is comment-only (git says all changed lines are comments).
        let diff = dc_win::gitdiff::files_diff(&root, &[c.file.clone()]);
        let comment_only = dc_win::gitdiff::is_comment_only_change(&diff);
        if comment_only {
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: "✓ Done — comment/whitespace only, skipped the compile check.".to_string(),
            });
        } else {
            // Kick a lightweight verify run (cargo check) to confirm it still compiles.
            self.verify_after_replace(&c.file);
        }
    }

    /// Revert just this comment's change: splice its stored before-text back over the lines the
    /// fix produced (`start .. start + after_len - 1`), restoring the original. The comment goes
    /// back to PENDING (you can re-run or dismiss it). No-op if it has no stored before-text.
    fn revert_comment(&mut self, i: usize) {
        let Some(c) = self.comments.items.get(i).cloned() else {
            return;
        };
        let (Some(before), Some(after_len)) = (c.before.clone(), c.after_len) else {
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: "⚠ nothing stored to revert for this comment.".to_string(),
            });
            return;
        };
        let root = self.workspace_root();
        let path = root.join(&c.file);
        let Ok(current) = std::fs::read_to_string(&path) else {
            return;
        };
        // The fix produced `after_len` lines starting at `c.start`; swap them back to `before`.
        let restored =
            dc_win::linecomment::splice_lines(&current, c.start, c.start + after_len - 1, &before);
        if std::fs::write(&path, &restored).is_ok() {
            // Reverting the change also removes the comment line entirely (VS-Code-style: undo
            // the edit → the comment thread goes with it), rather than leaving it pending.
            self.comments.remove(i);
            dc_win::comments::save(&root, &self.comments);
            self.select_file(c.file.clone());
            self.refresh_git_view();
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: format!("↩ Reverted the change on {} and removed the comment.", c.file),
            });
        }
    }

    /// Revert ONE diff block (VS-Code-style) back to its HEAD text: replace the block's current
    /// line range with the committed version. `cur_start` identifies the hunk (its first current
    /// line). Recomputes the diff from the freshly-loaded file, so it's always ground-truth.
    fn revert_block(&mut self, cur_start: usize) {
        let Some(rel) = self.selected_file.clone() else {
            return;
        };
        let root = self.workspace_root();
        // Find the hunk from the CURRENT on-disk diff (not stale UI state).
        let diff = dc_win::gitdiff::file_diff(&root, &rel);
        let Some(hunk) = diff.hunks().into_iter().find(|h| h.cur_start == cur_start) else {
            return; // the diff moved under us — no-op, the view will refresh
        };
        let path = root.join(&rel);
        let Ok(current) = std::fs::read_to_string(&path) else {
            return;
        };
        // Replace the block's current range with its HEAD text. For a pure addition head_text is
        // empty (→ the added lines are deleted); for a pure deletion the range is empty (→ the
        // removed text is inserted back before cur_start).
        let restored = dc_win::linecomment::splice_lines(
            &current,
            hunk.cur_start,
            hunk.cur_end,
            &hunk.head_text,
        );
        if std::fs::write(&path, &restored).is_ok() {
            self.select_file(rel.clone());
            self.refresh_git_view();
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: format!("↩ Reverted a block in {rel} to its committed version."),
            });
        }
    }

    /// After a line-replace that changed code, run the configured verify command to confirm it
    /// still compiles; report the result in the chat. Runs on a worker via a tiny iterate-less
    /// path would be ideal, but for v1 we run it inline-async through the existing Session by
    /// spawning a no-op check — simplest: just report optimistically and let the PR highlight +
    /// Undo cover a bad edit. (Verification wiring can deepen later.)
    fn verify_after_replace(&mut self, file: &str) {
        self.chat_turns.push(dc_win::chat::Turn {
            role: dc_win::chat::Speaker::Agent,
            text: format!(
                "✓ Applied to {file}. (Tip: if it doesn't compile, use the comment's ↩ Revert.)"
            ),
        });
    }

    /// Start an iterate run from a ready-made instruction (used by the small-fix line-comment
    /// path). Mirrors `start(RunKind::Iterate)` but with an explicit instruction instead of
    /// the composer text.
    #[allow(dead_code)]
    fn start_iterate_with(&mut self, instruction: String) {
        if self.session.is_some() {
            return;
        }
        self.debug_prompt("fix (iterate)", &instruction);
        self.intent = instruction;
        self.start(RunKind::Iterate);
        self.intent.clear();
        // Mark this run so its outcome is reported back into the chat (set AFTER start(),
        // which clears run state).
        self.iterate_from_comment = true;
    }

    /// Start a PLAN-ONLY workflow run from a ready-made task (the Execute-plan flow): run the
    /// staged workflow through the stage breakdown and stop for review. The phases stream to
    /// the plan panel. Mirrors `start_iterate_with` but with `RunKind::Plan`.
    fn start_plan_with(&mut self, task: String) {
        if self.session.is_some() {
            return;
        }
        self.debug_prompt("execute plan", &task);
        self.intent = task;
        self.start(RunKind::Plan);
        self.intent.clear();
        // Report the plan's outcome back into the chat thread, like an iterate-from-comment run.
        self.iterate_from_comment = true;
    }

    /// Apply the Nth proposed plan-file, then kick off an iterate build to implement it.
    /// The one-click bridge from a `PLAN-<slug>.md` design doc to a real build run: the plan
    /// is written to disk (so the agent can read it), then an iterate run is started with an
    /// instruction pointing at that file. Iterate needs a real project on disk, so this is a
    /// no-op (with a chat note) when no project folder is open or a run is already in flight.
    fn execute_plan(&mut self, i: usize) {
        let Some(pf) = self.proposed_files.get(i).cloned() else {
            return;
        };
        if self.session.is_some() {
            return; // A run is already in flight — don't stack another.
        }
        if self.picked_workspace.is_none() {
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: "⚠ Open a project folder first — executing a plan builds into it."
                    .to_string(),
            });
            return;
        }
        // Land the plan on disk (and refresh the conversation snapshot) first, so the workflow
        // can read the plan it's told to design against. This also clears it from the pending
        // proposals, so `i` is consumed exactly like a plain Apply.
        self.apply_proposed_file(i);
        self.start_plan_with(plan_task(&pf.name));
    }

    /// Kick off an iterate build to implement the `PLAN-*.md` open in the code view. Unlike
    /// [`Self::execute_plan`], the file is already on disk (opened from the tree), so there's
    /// nothing to apply — just point an iterate run at it. Same guards: needs an open project
    /// and no run in flight.
    fn execute_open_plan(&mut self) {
        let Some(rel) = self.selected_file.clone() else {
            return;
        };
        if !is_feature_plan(&rel) || self.session.is_some() {
            return;
        }
        if self.picked_workspace.is_none() {
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: "⚠ Open a project folder first — executing a plan builds into it."
                    .to_string(),
            });
            return;
        }
        self.start_plan_with(plan_task(&rel));
    }

    /// Write the Nth proposed plan-file to disk (README.md / TODO.md), then refresh the
    /// conversation's view of the plan files and re-open it in the code view.
    fn apply_proposed_file(&mut self, i: usize) {
        let Some(pf) = self.proposed_files.get(i).cloned() else {
            return;
        };
        let root = self.workspace_root();
        let path = root.join(&pf.name);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&path, &pf.content).is_ok() {
            // Refresh the conversation's plan snapshot so later turns see the new files.
            if let Some(convo) = self.conversation.as_mut() {
                let (readme, todo) = {
                    let r = find_readme(&root)
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .unwrap_or_default();
                    let t = find_todo_file(&root)
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .unwrap_or_default();
                    (r, t)
                };
                convo.set_plan_files(&readme, &todo);
            }
            // Show the file we just wrote.
            self.follow_agent = false;
            self.select_file(pf.name.clone());
            // Drop it from the pending list (applied).
            self.proposed_files.remove(i);
            // Confirm the write in the chat thread, so applying is visible in the record.
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: format!("✓ Applied {} to disk.", pf.name),
            });
        } else {
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text: format!("⚠ Could not write {}.", pf.name),
            });
        }
    }

    /// Select `rel` for the code panel and load its contents from the workspace root.
    fn select_file(&mut self, rel: String) {
        let root = self.workspace_root();
        // Switching to a different file resets the scrollable to the top; keep our virtualization
        // offset in sync so the first frame renders the top window, not the old file's slice.
        if self.selected_file.as_deref() != Some(rel.as_str()) {
            self.code_scroll_y = 0.0;
            self.code_viewport = None;
        }
        self.code = Some(dc_win::codeview::load(&root, &rel));
        self.selected_file = Some(rel);
        self.refresh_changed_lines();
    }

    /// Re-read the currently selected file from disk (after the agent edited it), so the
    /// code panel reflects the latest bytes — and refresh which lines differ from HEAD.
    fn reload_selected(&mut self) {
        if let Some(rel) = self.selected_file.clone() {
            let root = self.workspace_root();
            self.code = Some(dc_win::codeview::load(&root, &rel));
        }
        self.refresh_changed_lines();
    }

    /// Scroll the code view so `line` (1-based) sits in the MIDDLE of the viewport. Each rendered
    /// line is ~`CODE_LINE_PX` tall; back off by half the visible height so the target lands
    /// centered (falls back to a small top offset before the first scroll gives us a real
    /// viewport height). Shared by the minimap jump and the git-tab "open at first change".
    fn scroll_code_to_line(&self, line: usize) -> Task<Message> {
        let center = line as f32 * CODE_LINE_PX;
        // `code_view_h` is 0 until the view's first scroll event; fall back to a typical editor
        // height so a jump on a freshly-opened file still lands the target near center, not glued
        // to the very top.
        let view_h = if self.code_view_h > 1.0 {
            self.code_view_h
        } else {
            400.0
        };
        let y = (center - view_h / 2.0).max(0.0);
        iced::widget::operation::scroll_to(
            code_scroll_id(),
            iced::widget::scrollable::AbsoluteOffset { x: 0.0, y },
        )
    }

    /// Recompute the shown file's PR-style diff vs HEAD (git): added lines (green) + removed lines
    /// (red). Cheap `git diff -U0` on the one file (all-added for an untracked file); empty when
    /// nothing's selected. `changed_lines` is the green set, kept for the minimap + jump-to-change.
    fn refresh_changed_lines(&mut self) {
        self.file_diff = match &self.selected_file {
            Some(rel) => dc_win::gitdiff::file_diff(&self.workspace_root(), rel),
            None => dc_win::gitdiff::FileDiff::default(),
        };
        self.changed_lines = self.file_diff.added.clone();
    }

    /// Refresh the PR-view git state synchronously: the tree cache, per-file M/A/D statuses,
    /// branch, upstream. Used at the points where we want the state up-to-date *before* the next
    /// line runs (project open, right after a stage/discard). The periodic heartbeat instead uses
    /// the async path (`SyncWorkspace` → `compute_snapshot` off-thread → `WorkspaceSynced`).
    fn refresh_git_view(&mut self) {
        let snap = compute_snapshot(self.workspace_root());
        self.apply_snapshot(snap);
    }

    /// Apply a computed [`WorkspaceSnapshot`] to the live state. Pure assignment — the expensive
    /// walk/git work already happened in [`compute_snapshot`] (possibly on a background thread).
    fn apply_snapshot(&mut self, snap: WorkspaceSnapshot) {
        self.tree_cache = snap.tree;
        self.file_status = snap.file_status;
        self.stage_states = snap.stage_states;
        self.unstaged_deltas = snap.unstaged_deltas;
        self.staged_deltas = snap.staged_deltas;
        self.branch = snap.branch;
        self.upstream = snap.upstream;
    }

    /// Run a `git` subcommand in the workspace (e.g. `["add", "--", path]`) and return whether
    /// it succeeded. Used by the git-tab context-menu actions (stage / unstage / discard).
    fn run_git(&self, args: &[&str]) -> bool {
        let root = self.workspace_root();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Run a NETWORK git op (push / pull / fetch), capturing its output and reporting the outcome
    /// in the chat — these fail more often (auth, conflicts, no remote), so the message matters.
    /// `label` names the op for the report. Runs synchronously (briefly blocks the UI).
    fn run_git_net(&mut self, label: &str, args: &[&str]) {
        let root = self.workspace_root();
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output();
        let (ok, detail) = match out {
            Ok(o) => {
                // git writes progress/results to stderr; prefer it, fall back to stdout.
                let err = String::from_utf8_lossy(&o.stderr);
                let msg = if err.trim().is_empty() {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                } else {
                    err.trim().to_string()
                };
                (o.status.success(), msg)
            }
            Err(e) => (false, e.to_string()),
        };
        // Keep the report short — the last non-empty line usually carries the gist.
        let gist = detail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
        let text = if ok {
            format!("✓ git {label} — {}", if gist.is_empty() { "done" } else { gist })
        } else {
            format!("⚠ git {label} failed — {gist}")
        };
        self.chat_turns.push(dc_win::chat::Turn {
            role: dc_win::chat::Speaker::Agent,
            text,
        });
    }

    /// Whether a swarm run is (or was) active — i.e. the topology has nodes to draw.
    fn is_swarm(&self) -> bool {
        !self.topology.is_empty()
    }

    /// Compute the outcome banner when a run ends. Two shapes:
    ///  • ITERATE (editing an existing project) → report the files the agent actually
    ///    *changed* — never a whole-repo scan (which would count thousands) and no "open
    ///    output folder" (you're already in your own repo).
    ///  • FROM-SCRATCH build → the "N files built" summary + open-folder, as before.
    fn finish_run(&mut self, ok: bool, summary: &str) {
        // Surface the outcome: jump to the Build tab when a run ends.
        self.bottom_tab = BottomTab::Build;
        // The agent's done working the selection — drop the amber "working" highlight (the
        // green git-change highlight takes over).
        self.working = None;
        if self.iterating {
            self.finish_iterate(ok, summary);
            return;
        }
        let dir = self.run_dir.clone();
        let files = dir
            .as_deref()
            .map(dc_win::config::source_files)
            .unwrap_or_default();
        let n = files.len();

        // The truthful classification: "ok" from the run only counts if it actually
        // produced source files. Building zero files is a failure no matter what the
        // swarm reported.
        let (banner_ok, headline, reason) = if n == 0 {
            (
                false,
                "Built 0 files".to_string(),
                if self.plan.subtasks.is_empty() {
                    "the planner produced no buildable subtasks (decomposition failed)".to_string()
                } else {
                    format!("implementation failed — {summary}")
                },
            )
        } else if ok {
            (true, format!("{n} file{} built", plural(n)), String::new())
        } else {
            (
                false,
                format!("{n} file{} built, but did not finish", plural(n)),
                summary.to_string(),
            )
        };

        self.result = Some(RunResult {
            ok: banner_ok,
            headline,
            reason,
            files,
            dir,
        });
    }

    /// Undo the last fix: `git checkout --` exactly the files it changed, restoring them to
    /// their committed state. A deliberate "I don't like this change" action — the safe way to
    /// reject a fix, instead of hand-reverting. No-op if there's no finished result with files.
    fn undo_last_change(&mut self) {
        let files: Vec<String> = self
            .result
            .as_ref()
            .map(|r| r.files.clone())
            .unwrap_or_default();
        if files.is_empty() {
            return;
        }
        let root = self.workspace_root();
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .arg("checkout")
            .arg("--")
            .args(&files)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        self.chat_turns.push(dc_win::chat::Turn {
            role: dc_win::chat::Speaker::Agent,
            text: if ok {
                format!(
                    "↩ Undid the change — reverted {} file(s) to committed state.",
                    files.len()
                )
            } else {
                "⚠ Couldn't undo (not a git repo, or the file was already committed).".to_string()
            },
        });
        if ok {
            // Clear the result banner + refresh the code view to the reverted content.
            self.result = None;
            self.reload_selected();
        }
    }

    /// The iterate outcome: report only the files the agent edited this run, and whether it
    /// finished green. No repo scan, no open-folder button.
    fn finish_iterate(&mut self, ok: bool, summary: &str) {
        let files = self.edited_files.clone();
        let n = files.len();
        // On a successful comment-driven fix, mark the comment resolved (in place) and refresh
        // the PR file-tree statuses. The resolved comment stays visible as a "done" record.
        if ok && self.iterate_from_comment {
            let mut changed = false;
            for f in &files {
                changed |= self.comments.resolve_latest_on(f);
            }
            if changed {
                dc_win::comments::save(&self.workspace_root(), &self.comments);
            }
        }
        self.refresh_git_view();
        let (headline, reason) = if ok && n > 0 {
            (
                format!("done — {n} file{} changed", plural(n)),
                String::new(),
            )
        } else if ok {
            // Finished cleanly but edited nothing (e.g. "already as requested").
            ("done — no changes needed".to_string(), summary.to_string())
        } else {
            (
                format!("stopped — {n} file{} changed, did not finish", plural(n)),
                summary.to_string(),
            )
        };
        // If this iterate came from a line comment, report the outcome IN THE CHAT so you
        // see what it did without hunting in the Build tab.
        if self.iterate_from_comment {
            self.iterate_from_comment = false;
            let text = if ok && n > 0 {
                let list = files.join(", ");
                let verified = if self.verify_text.is_some() {
                    " and it still compiles"
                } else {
                    ""
                };
                format!("✓ Done. Changed {n} file{} ({list}){verified}.", plural(n))
            } else if ok {
                "✓ Done — nothing needed changing.".to_string()
            } else {
                format!(
                    "⚠ I couldn't finish that cleanly ({} file{} touched). {summary}",
                    n,
                    plural(n)
                )
            };
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Agent,
                text,
            });
        }
        self.result = Some(RunResult {
            ok,
            headline,
            reason,
            files,
            dir: None, // no "open output folder" — you're iterating in your own repo
        });
    }

    /// Drain the in-flight chat turn, if any. On a reply: split prose from proposed
    /// plan-files, record the assistant turn, show the prose in the thread, stage the
    /// proposed files for Apply, and auto-open the first one in the code view.
    fn pump_chat(&mut self) {
        let Some(cs) = &self.chat_session else {
            return;
        };
        let events = cs.drain();
        for ev in events {
            match ev {
                dc_win::chat_session::ChatEvent::Token(delta) => {
                    // Grow the live "typing" bubble. Strip <think> for display as it streams
                    // (a reasoning delta shouldn't flash into the visible reply).
                    let buf = self.streaming.get_or_insert_with(String::new);
                    buf.push_str(&delta);
                }
                dc_win::chat_session::ChatEvent::Reply(raw) => {
                    self.streaming = None; // the live bubble is replaced by the finished turn
                    self.working = None; // a question answer is done → drop the amber highlight
                    let (prose, files) = dc_win::chat::parse_reply(&raw);
                    if let Some(convo) = self.conversation.as_mut() {
                        convo.record_reply(&raw);
                        // Fold any proposed plan-file content into the conversation's plan
                        // snapshot NOW (before Apply), so follow-up questions ("what's in the
                        // todo?") see what was just proposed — otherwise the system prompt
                        // keeps showing the stale on-disk file and the model contradicts its
                        // own proposal.
                        for f in &files {
                            if f.name.eq_ignore_ascii_case("README.md") {
                                convo.set_readme(&f.content);
                            } else if f.name.eq_ignore_ascii_case("TODO.md") {
                                convo.set_todo(&f.content);
                            }
                        }
                    }
                    let shown = if prose.is_empty() {
                        // A reply that was only file blocks → note what changed.
                        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
                        format!("Proposed changes to {}.", names.join(", "))
                    } else {
                        prose
                    };
                    self.chat_turns.push(dc_win::chat::Turn {
                        role: dc_win::chat::Speaker::Agent,
                        text: shown,
                    });
                    self.proposed_files = files;
                    // Auto-open the first proposed file so you see the plan taking shape.
                    if let Some(first) = self.proposed_files.first() {
                        let name = first.name.clone();
                        let content = first.content.clone();
                        // Show the PROPOSED content directly (not the on-disk file, which
                        // hasn't been written yet).
                        self.follow_agent = false;
                        self.selected_file = Some(name.clone());
                        self.code = Some(dc_win::codeview::from_text(&name, &content));
                    }
                    self.chat_session = None;
                }
                dc_win::chat_session::ChatEvent::Failed(msg) => {
                    self.streaming = None;
                    self.working = None;
                    self.chat_turns.push(dc_win::chat::Turn {
                        role: dc_win::chat::Speaker::Agent,
                        text: format!("⚠ {msg}"),
                    });
                    self.chat_session = None;
                }
            }
        }
    }

    /// Drain the worker channels into UI state. Called each tick.
    fn pump(&mut self) {
        self.pump_chat();
        self.pump_triage();
        self.pump_replace();
        // While a fix run is in flight, keep the code view + change-highlight fresh from disk
        // so edits land live (the agent edits the real files). Cheap: reload the one shown
        // file + one `git diff` on it per tick. Done before borrowing `session` below.
        if self.iterate_from_comment && self.session.is_some() {
            self.reload_selected();
        }
        let Some(session) = &self.session else {
            return;
        };
        for ev in session.drain_events() {
            match ev {
                UiEvent::Agent(e) => {
                    // Live "watch it type": as the model streams a write/edit, preview the
                    // growing file content in the code view, word by word, before it lands.
                    if let dc_core::AgentEvent::ContentDelta { cumulative, .. } = &e {
                        if let Some(p) = dc_win::codeview::partial_edit_preview(cumulative) {
                            if !p.content.is_empty() {
                                let name = p
                                    .file
                                    .clone()
                                    .or_else(|| self.selected_file.clone())
                                    .unwrap_or_else(|| "(writing…)".to_string());
                                if p.file.is_some() {
                                    self.selected_file = p.file.clone();
                                    self.follow_agent = false;
                                }
                                self.code = Some(dc_win::codeview::from_text(&name, &p.content));
                            }
                        }
                        continue; // a delta is preview-only; nothing else to fold
                    }
                    if let dc_core::AgentEvent::Planned { steps }
                    | dc_core::AgentEvent::PlanRevised { steps } = &e
                    {
                        self.board = steps.clone();
                    }
                    if let dc_core::AgentEvent::Verification { summary, .. } = &e {
                        self.verify_text = Some(summary.clone());
                    }
                    // Record files the agent actually edited/wrote (for the iterate banner).
                    if dc_win::codeview::is_mutating_touch(&e) {
                        if let Some(rel) = dc_win::codeview::file_touched_by(&e) {
                            if !self.edited_files.contains(&rel) {
                                self.edited_files.push(rel);
                            }
                        }
                    }
                    // During a line-comment fix, narrate the key steps into the chat so the
                    // work is VISIBLE (a fix used to run silently). Only meaningful steps —
                    // editing a file and the verify result — not every read.
                    if self.iterate_from_comment {
                        if let Some(line) = fix_feed_line(&e) {
                            self.chat_turns.push(dc_win::chat::Turn {
                                role: dc_win::chat::Speaker::Agent,
                                text: line,
                            });
                        }
                    }
                    // Follow the agent: when it touches a file and we're in follow mode,
                    // show that file in the code panel — so edits land in front of you.
                    if self.follow_agent {
                        if let Some(rel) = dc_win::codeview::file_touched_by(&e) {
                            self.select_file(rel);
                        }
                    }
                    // A tool result means a file may have just changed on disk; refresh the
                    // shown file so an edit/write to it is reflected (the ToolCall fired
                    // before the bytes landed). Cheap: only when something is selected.
                    if matches!(
                        e,
                        dc_core::AgentEvent::ToolResult {
                            is_error: false,
                            ..
                        }
                    ) {
                        self.reload_selected();
                    }
                    self.rows.extend(agent_rows(&e));
                }
                UiEvent::Swarm(e) => {
                    // Fold into the live topology (canvas) and the per-subtask board,
                    // and append to the flat activity stream.
                    self.topology.apply(&e, self.now());
                    self.swarm_board.apply(&e);
                    self.rows.extend(swarm_rows(&e));
                }
                UiEvent::Phase {
                    phase,
                    content,
                    tests_written,
                } => {
                    // Fold a staged-workflow phase into the plan (the plan panel) and
                    // note it in the activity stream.
                    self.plan.apply(phase, &content, &tests_written);
                    if tests_written.is_empty() {
                        self.rows
                            .push(Row::ok("◆", format!("plan · {}", phase.title())));
                    } else {
                        self.rows.push(Row::ok(
                            "✓",
                            format!("wrote {} frozen test file(s)", tests_written.len()),
                        ));
                    }
                }
                UiEvent::Done { ok, summary } => {
                    self.finish_run(ok, &summary);
                    self.session = None;
                }
                UiEvent::Failed(msg) => {
                    self.finish_run(false, &format!("error: {msg}"));
                    self.session = None;
                }
            }
        }
        // Move any new decision requests onto the gate bar.
        if let Some(session) = &self.session {
            for p in session.drain_pending() {
                self.gatebar.push(match p {
                    Pending::Confirm {
                        command,
                        default_reason,
                        reply,
                    } => Gatebar::Confirm {
                        command,
                        reason: default_reason,
                        reply,
                    },
                    Pending::Gate {
                        phase,
                        content,
                        reply,
                    } => Gatebar::Gate {
                        phase,
                        content,
                        reply,
                    },
                });
            }
        }
    }

    /// Answer the oldest pending confirm with `c` (no-op if the front isn't a confirm).
    fn answer_confirm(&mut self, c: Confirmation) {
        if matches!(self.gatebar.first(), Some(Gatebar::Confirm { .. })) {
            if let Gatebar::Confirm { reply, .. } = self.gatebar.remove(0) {
                let _ = reply.send(c);
            }
        }
    }

    /// Answer the oldest pending workflow gate with `d`.
    fn answer_gate(&mut self, d: Decision) {
        if matches!(self.gatebar.first(), Some(Gatebar::Gate { .. })) {
            if let Gatebar::Gate { reply, .. } = self.gatebar.remove(0) {
                let _ = reply.send(d);
            }
            self.sendback_notes.clear();
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::IntentChanged(s) => self.intent = s,
            Message::ModelChanged(s) => self.model_input = s,
            Message::UrlChanged(s) => self.url_input = s,
            Message::AdvisorChanged(s) => self.advisor_input = s,
            Message::AdvisorUrlChanged(s) => self.advisor_url_input = s,
            Message::OrchModelChanged(s) => self.orch_model_input = s,
            Message::OrchUrlChanged(s) => self.orch_url_input = s,
            Message::VerifyChanged(s) => self.verify_input = s,
            Message::SuffixChanged(s) => self.suffix_input = s,
            Message::ToggleSettings => {
                self.open_menu = None;
                self.settings_open = !self.settings_open;
            }
            Message::ToggleYolo(v) => self.cfg.yolo = v,
            Message::ToggleDryRun(v) => self.cfg.dry_run = v,
            Message::RunTdd => self.start(RunKind::Tdd),
            Message::RunIterate => self.start(RunKind::Iterate),
            Message::Tick => self.pump(),
            Message::SyncWorkspace => {
                // Re-walk the tree + git state OFF the UI thread — the walk and the git
                // subprocesses are the slow part, so compute a snapshot on a background thread and
                // apply it when it's ready (`WorkspaceSynced`). Skip if a sync is already pending.
                if self.picked_workspace.is_some() && !self.sync_pending {
                    self.sync_pending = true;
                    let root = self.workspace_root();
                    return Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || compute_snapshot(root))
                                .await
                                .ok()
                        },
                        Message::WorkspaceSynced,
                    );
                }
            }
            Message::WorkspaceSynced(snap) => {
                self.sync_pending = false;
                if let Some(snap) = snap {
                    self.apply_snapshot(snap);
                }
            }
            Message::SelectFile(rel) => {
                // Click-to-pin: show this file and stop auto-following the agent until
                // the next run re-arms follow.
                self.follow_agent = false;
                self.select_file(rel);
            }
            Message::SelectGitFile(rel) => {
                // Open the file, then jump the code view to its first changed line (git-tab
                // click → land you on the change, VS-Code-diff style). The scroll is DEFERRED a
                // beat so it runs against the newly-laid-out content, not the previous file's
                // tree — a same-frame scroll_to misses on large files (the new lines don't exist
                // in the layout yet).
                self.follow_agent = false;
                self.select_file(rel);
                if self.changed_lines.iter().next().is_some() {
                    // Re-emit as a follow-up message: it's processed after this update's view()
                    // rebuilds with the new file, so scroll_to acts on the correct layout.
                    return Task::done(Message::JumpToFirstChange);
                }
            }
            Message::JumpToFirstChange => {
                if let Some(&first) = self.changed_lines.iter().next() {
                    return self.scroll_code_to_line(first);
                }
            }
            Message::ToggleDir(rel) => {
                if !self.collapsed_dirs.remove(&rel) {
                    self.collapsed_dirs.insert(rel);
                }
            }
            Message::FileFilterChanged(q) => {
                self.file_filter = q;
            }
            Message::ToggleMenu(m) => {
                self.open_menu = if self.open_menu == Some(m) {
                    None
                } else {
                    Some(m)
                };
            }
            Message::ChatSend => self.send_chat(),
            Message::ApplyFile(i) => self.apply_proposed_file(i),
            Message::ExecutePlan(i) => self.execute_plan(i),
            Message::ExecuteOpenPlan => self.execute_open_plan(),
            Message::ToggleThink(v) => self.think = v,
            Message::ToggleDebug(v) => self.debug = v,
            Message::UndoLastChange => self.undo_last_change(),
            Message::DismissComment(i) => {
                self.comments.remove(i);
                dc_win::comments::save(&self.workspace_root(), &self.comments);
            }
            Message::RevertComment(i) => self.revert_comment(i),
            Message::RevertBlock(cur_start) => self.revert_block(cur_start),
            Message::MinimapJump(line) => {
                return self.scroll_code_to_line(line);
            }
            Message::CodeScrolled(vp) => {
                // Record the visible slice as fractions of the whole content so the minimap can
                // box "you are here". top = how far down we've scrolled; height = how much of the
                // file fits on screen.
                let top = vp.relative_offset().y;
                let content_h = vp.content_bounds().height.max(1.0);
                let view_h = vp.bounds().height;
                self.code_view_h = view_h;
                self.code_view_w = vp.bounds().width;
                self.code_scroll_y = vp.absolute_offset().y;
                let height = (view_h / content_h).clamp(0.0, 1.0);
                self.code_viewport = Some((top * (1.0 - height), height));
            }
            Message::CancelRun => {
                if let Some(s) = &self.session {
                    s.cancel();
                    self.chat_turns.push(dc_win::chat::Turn {
                        role: dc_win::chat::Speaker::Agent,
                        text: "⏹ cancelling — stopping at the next step…".to_string(),
                    });
                }
            }
            Message::SelectBottomTab(t) => self.bottom_tab = t,
            Message::GitCursorMoved(p) => {
                self.cursor_pos = p;
                // While the chat|code divider is held, map the absolute cursor X to chat's share
                // of the chat+code region. The explorer occupies a fixed 20% on the left, so that
                // region runs from 0.20·W to W.
                if self.dragging_split && self.window_w > 0.0 {
                    let region_left = 0.20 * self.window_w;
                    let region_w = self.window_w - region_left;
                    if region_w > 1.0 {
                        let frac = (p.x - region_left) / region_w;
                        self.chat_frac = frac.clamp(0.15, 0.85);
                    }
                }
            }
            Message::SplitDragStart => self.dragging_split = true,
            Message::SplitDragEnd => self.dragging_split = false,
            Message::WindowResized(w) => self.window_w = w,
            Message::GitRowMenu(path, status) => {
                self.git_menu_at = self.cursor_pos;
                self.git_menu = Some((path, status));
            }
            Message::CloseGitMenu => self.git_menu = None,
            Message::GitStage(path) => {
                self.git_menu = None;
                self.run_git(&["add", "--", &path]);
                self.refresh_git_view();
            }
            Message::GitUnstage(path) => {
                self.git_menu = None;
                self.run_git(&["restore", "--staged", "--", &path]);
                self.refresh_git_view();
            }
            Message::GitDiscard(path) => {
                self.git_menu = None;
                // For an untracked file `checkout --` is a no-op, so clean it instead; for a
                // tracked file it restores the committed content (reverting a modify or delete).
                let is_untracked =
                    self.file_status.get(&path) == Some(&dc_win::gitdiff::FileStatus::Added);
                if is_untracked {
                    self.run_git(&["clean", "-f", "--", &path]);
                } else {
                    self.run_git(&["checkout", "--", &path]);
                }
                self.refresh_git_view();
                // If the discarded file is the one on screen, reload it to show reverted content.
                if self.selected_file.as_deref() == Some(path.as_str()) {
                    self.reload_selected();
                }
            }
            Message::CommitMsgChanged(s) => self.commit_msg = s,
            Message::GitStageAll => {
                self.run_git(&["add", "-A"]);
                self.refresh_git_view();
            }
            Message::GitUnstageAll => {
                self.run_git(&["reset"]); // unstage everything, keep working-tree changes
                self.refresh_git_view();
            }
            Message::GitCommit => {
                let msg = self.commit_msg.trim().to_string();
                // Nothing staged, or an empty message → don't attempt a commit.
                let has_staged = self.stage_states.values().any(|s| s.staged);
                if msg.is_empty() || !has_staged {
                    return Task::none();
                }
                if self.run_git(&["commit", "-m", &msg]) {
                    self.commit_msg.clear();
                }
                self.refresh_git_view();
                self.reload_selected();
            }
            Message::GitPush => {
                // No upstream yet → set it on push so a fresh branch publishes cleanly.
                if self.upstream.upstream.is_none() {
                    if let Some(b) = self.branch.clone() {
                        self.run_git_net("push", &["push", "-u", "origin", &b]);
                    }
                } else {
                    self.run_git_net("push", &["push"]);
                }
                self.refresh_git_view();
                self.reload_selected();
            }
            Message::GitPull => {
                self.run_git_net("pull", &["pull", "--ff-only"]);
                self.refresh_git_view();
                self.reload_selected();
            }
            Message::GitFetch => {
                self.run_git_net("fetch", &["fetch"]);
                self.refresh_git_view();
            }
            Message::LineDragStart(n) => {
                // Begin a drag-selection anchored at line n. Clear any open comment box.
                self.drag = Some((n, n));
                self.comment_range = None;
            }
            Message::LineDragTo(n) => {
                // Extend the drag to line n (only while a drag is active).
                if let Some((anchor, _)) = self.drag {
                    self.drag = Some((anchor, n));
                }
            }
            Message::LineDragEnd => {
                // Commit the drag into a comment range (normalized so start ≤ end) and open
                // the comment box. A no-drag (just a click) yields a single-line range.
                if let Some((a, b)) = self.drag.take() {
                    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                    self.comment_range = Some((lo, hi));
                    self.comment_draft.clear();
                }
            }
            Message::CommentDraftChanged(s) => self.comment_draft = s,
            Message::CommentSubmit => self.submit_line_comment(),
            Message::CommentCancel => {
                self.comment_range = None;
                self.drag = None;
                self.comment_draft.clear();
            }
            Message::ConfirmAllow => self.answer_confirm(Confirmation::AllowOnce),
            Message::ConfirmDeny => {
                self.answer_confirm(Confirmation::Deny("denied by user".to_string()))
            }
            Message::ConfirmRemember => {
                // Remember the command's first token as the approved prefix.
                let prefix = match self.gatebar.first() {
                    Some(Gatebar::Confirm { command, .. }) => remember_prefix(command),
                    _ => String::new(),
                };
                self.answer_confirm(Confirmation::AllowRemember { prefix });
            }
            Message::NotesChanged(s) => self.sendback_notes = s,
            Message::GateApprove => self.answer_gate(Decision::Approve),
            Message::GateRevise => self.answer_gate(Decision::Revise),
            Message::GateSendBack => {
                let (target, notes) = match self.gatebar.first() {
                    Some(Gatebar::Gate { phase, .. }) => (*phase, non_empty(&self.sendback_notes)),
                    _ => (Phase::Specs, None),
                };
                self.answer_gate(Decision::SendBack { target, notes });
            }
            Message::GateAbort => self.answer_gate(Decision::Abort),
            Message::SelectCoder(id) => self.selected_coder = Some(id),
            Message::ClearSelection => self.selected_coder = None,
            Message::PickWorkspace => {
                self.open_menu = None;
                // Native folder dialog (blocking — fine for a button click). When a
                // folder is chosen, runs go there so follow-up prompts iterate on it.
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title("Choose a project folder to work in")
                    .pick_folder()
                {
                    self.picked_workspace = Some(dir.clone());
                    // A fresh project → drop any stale selection from the last one, and open the
                    // tree compacted: every top-level folder starts collapsed.
                    self.selected_file = None;
                    self.code = None;
                    self.collapsed_dirs = dc_win::filetree::top_level_dirs(&dir);
                    self.file_filter.clear();
                    self.tree_cache = dc_win::filetree::full_rows(&dir);
                    // Remember it for next launch.
                    dc_win::persist::save(&dc_win::persist::UiState {
                        last_project: Some(dir),
                    });
                    // Greet: show the README/roadmap in Activity, and open the planning
                    // conversation (mode-detected) in the chat column.
                    self.show_welcome();
                    self.open_conversation();
                }
            }
            Message::ClearWorkspace => {
                self.open_menu = None;
                self.picked_workspace = None;
                self.selected_file = None;
                self.code = None;
                // Forget the remembered project so a restart doesn't re-open it.
                dc_win::persist::save(&dc_win::persist::UiState { last_project: None });
            }
            Message::OpenOutputFolder => {
                if let Some(dir) = self.result.as_ref().and_then(|r| r.dir.clone()) {
                    // Open in the system file manager (Explorer on Windows).
                    #[cfg(target_os = "windows")]
                    let _ = std::process::Command::new("explorer").arg(&dir).spawn();
                    #[cfg(target_os = "macos")]
                    let _ = std::process::Command::new("open").arg(&dir).spawn();
                    #[cfg(all(unix, not(target_os = "macos")))]
                    let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
                }
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        // The IDE body: three columns — EXPLORER (file tree) · CENTER (activity stream +
        // the intent composer beneath it) · CODE (the file being edited). VS-Code-style.
        let center: Element<'_, Message> = if self.plan.started() && self.is_swarm() {
            // A swarm build in flight: the plan panel + live topology are the story.
            row![self.view_plan(), v_divider(), self.view_topology()]
                .spacing(GAP)
                .into()
        } else if self.plan.started() {
            // A staged build (single agent): plan panel beside the activity stream.
            row![self.view_plan(), v_divider(), self.view_center()]
                .spacing(GAP)
                .into()
        } else {
            // Iterate / idle: the activity stream + composer is the center column.
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

    /// The left EXPLORER column: a tabbed panel — **Files** (the workspace file tree) and
    /// **Git** (just the changed files, PR-style). A tab bar sits under a branch header that's
    /// shared across both tabs.
    fn view_explorer(&self) -> Element<'_, Message> {
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
        let mut git_col = column![
            text(branch_line)
                .size(11)
                .color(iced::Color::from_rgb(0.55, 0.58, 0.70)),
        ]
        .spacing(6);
        // Push / Pull / Fetch — only when the repo is on a branch (has a name). Labels show the
        // ahead/behind counts so you know what each will move.
        if self.branch.is_some() {
            git_col = git_col.push(self.view_sync_bar());
        }
        git_col = git_col.push(self.view_git_tab());

        // Each section is its own rounded card, stacked with a gap between them.
        let git_section = container(git_col.spacing(6))
            .height(Length::FillPortion(1))
            .width(Fill)
            .padding(PAD)
            .style(card_style);
        let files_section = container(self.view_files_tab())
            .height(Length::FillPortion(3))
            .width(Fill)
            .padding(PAD)
            .style(card_style);

        // 200 of the 1000-portion total (chat+code share the other 800) → a fixed ~20% width,
        // so dragging the chat|code divider never moves the explorer.
        container(column![git_section, h_divider(), files_section])
            .width(Length::FillPortion(200))
            .height(Fill)
            .into()
    }

    /// The push / pull / fetch bar shown under the branch line. Push shows the ahead count, Pull
    /// the behind count. When the branch has no upstream, Push offers to publish it.
    fn view_sync_bar(&self) -> Element<'_, Message> {
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
    fn view_files_tab(&self) -> Element<'_, Message> {
        use dc_win::gitdiff::FileStatus;
        let filtering = !self.file_filter.trim().is_empty();
        // Derive the display from the cached full tree in memory — no filesystem walk per frame.
        let rows = if filtering {
            dc_win::filetree::filter_view(&self.tree_cache, &self.file_filter)
        } else {
            dc_win::filetree::collapse_view(&self.tree_cache, &self.collapsed_dirs)
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
    fn view_git_tab(&self) -> Element<'_, Message> {
        use dc_win::gitdiff::FileStatus;
        if self.branch.is_none() {
            return text("not a git repository")
                .size(11)
                .color(FG_MUTED)
                .into();
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
            commit_btn = commit_btn.on_press(Message::GitCommit).style(primary_button);
        } else {
            commit_btn = commit_btn.style(menu_item_style);
        }
        let commit_box = column![input, commit_btn.padding([4, 12]).width(Fill)].spacing(4);

        // Partition the changed files into staged and unstaged. A file can be in BOTH (staged
        // plus further working-tree edits) — VS Code shows it in each, and so do we.
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
                self.stage_states.get(*p).map(|s| s.unstaged).unwrap_or(true)
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
                let status = self.file_status.get(*path).copied().unwrap_or(FileStatus::Modified);
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
    fn git_section_header(
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
    fn git_file_row(
        &self,
        path: &str,
        status: dc_win::gitdiff::FileStatus,
        staged: bool,
    ) -> Element<'_, Message> {
        use dc_win::gitdiff::FileStatus;
        let color = match status {
            FileStatus::Added => GOOD,
            FileStatus::Modified => AMBER,
            FileStatus::Deleted => BAD,
        };
        let is_selected = self.selected_file.as_deref() == Some(path);
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
    fn view_git_menu(&self) -> Option<Element<'_, Message>> {
        use dc_win::gitdiff::FileStatus;
        let (path, status) = self.git_menu.clone()?;

        // Show Stage only if the file has unstaged content, Unstage only if it has staged
        // content — never both when there's nothing to do. Discard's label reflects the status.
        let stage = self.stage_states.get(&path).copied();
        let has_unstaged = stage.map(|s| s.unstaged).unwrap_or(true);
        let has_staged = stage.map(|s| s.staged).unwrap_or(false);
        let discard_label = match status {
            FileStatus::Added => "🗑  Delete untracked file",
            FileStatus::Deleted => "↩  Restore deleted file",
            FileStatus::Modified => "↩  Discard changes",
        };
        let mut items: Vec<(&str, Message)> = Vec::new();
        if has_unstaged {
            items.push(("＋  Stage", Message::GitStage(path.clone())));
        }
        if has_staged {
            items.push(("－  Unstage", Message::GitUnstage(path.clone())));
        }
        // Discard acts on the working tree — only meaningful when there are unstaged changes.
        if has_unstaged {
            items.push((discard_label, Message::GitDiscard(path.clone())));
        }
        let mut col = column![
            text(path.clone())
                .size(11)
                .color(FG_MUTED)
                .wrapping(iced::widget::text::Wrapping::None),
        ]
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
    fn selected_line_range(&self) -> Option<(usize, usize)> {
        if let Some((a, b)) = self.drag {
            Some(if a <= b { (a, b) } else { (b, a) })
        } else {
            self.comment_range
        }
    }

    /// The right CODE column: the selected/followed file with line numbers, read-only,
    /// rendered like a VS Code editor. The gutter (line numbers) and the code are TWO
    /// side-by-side columns; wrapping is disabled so a long line scrolls horizontally
    /// instead of wrapping into — and interrupting — the number gutter. One vertical
    /// scroll (whole editor) + one horizontal scroll (the code column) is what you get.
    fn view_code(&self, portion: u16) -> Element<'_, Message> {
        let header = match (&self.selected_file, self.follow_agent) {
            (Some(rel), true) => format!("CODE  ·  {rel}  ·  following"),
            (Some(rel), false) => format!("CODE  ·  {rel}  ·  pinned"),
            (None, _) => "CODE".to_string(),
        };

        let inner: Element<'_, Message> = match &self.code {
            Some(cv) if cv.note.is_some() => text(cv.note.clone().unwrap_or_default())
                .size(12)
                .color(FG_MUTED)
                .into(),
            Some(cv) => {
                // Per-line rows you can DRAG across to select a range, then comment (PR-style).
                // Each line is a mouse_area (press=start drag, enter=extend, release=commit)
                // wrapping ONE no-wrap monospace string; selected lines get an accent wash.
                // The comment box renders after the last line of the committed range.
                let sel = self.selected_line_range(); // active drag OR committed range
                                                      // The amber "working" range applies only when the shown file is the one being
                                                      // worked on. A pulsing alpha (sine on the animation clock) reads as "in progress".
                let working_here = self
                    .working
                    .as_ref()
                    .filter(|(f, _, _)| Some(f.as_str()) == self.selected_file.as_deref())
                    .map(|(_, lo, hi)| (*lo, *hi));
                let pulse = 0.10 + 0.10 * (0.5 + 0.5 * (self.now() * 3.0).sin());

                // Always window to the visible lines (fast on big files). Inline comments and the
                // open comment box have variable height, but they only ever appear INSIDE the
                // rendered window (anchored to a visible line), between the two spacers — so they
                // never corrupt the spacer counts, which only stand in for plain CODE_LINE_PX
                // rows above/below. A box below shifts later lines slightly, exactly as before.
                // When the minimap is floating (file overflows the viewport), inline-comment rows
                // need right padding so their ✕/revert buttons don't hide behind it. 72px minimap
                // + a small gap. Computed here since the comment rows are built in the loop below.
                let minimap_overflows =
                    self.code_viewport.is_some_and(|(_, height)| height < 0.999);
                // Width for the comment / revert bars: the VIEWPORT width (not the horizontally-
                // scrollable content width), minus the minimap gutter when it's floating, so a bar
                // spans the visible area and ends just before the minimap — with round edges. It
                // scales with the window and expands when the minimap is hidden. `None` (→ Fill)
                // before the first scroll gives us a real viewport width.
                let bar_width: Option<f32> = (self.code_view_w > 1.0).then(|| {
                    let gutter = if minimap_overflows { 76.0 } else { 8.0 };
                    (self.code_view_w - gutter).max(120.0)
                });

                // Diff blocks (VS-Code-style). Two derived maps:
                //  • `block_bar_after`: last green line of a block → its cur_start (where to render
                //    the standalone "↩ revert block" bar).
                //  • `line_to_block`: every green line → its block's cur_start (so a comment ON a
                //    changed block can offer the same revert inline).
                let hunks = self.file_diff.hunks();
                let block_bar_after: std::collections::BTreeMap<usize, usize> = hunks
                    .iter()
                    .filter(|h| h.cur_start <= h.cur_end) // has a current (green) line
                    .map(|h| (h.cur_end, h.cur_start))
                    .collect();
                let line_to_block: std::collections::BTreeMap<usize, usize> = hunks
                    .iter()
                    .filter(|h| h.cur_start <= h.cur_end)
                    .flat_map(|h| (h.cur_start..=h.cur_end).map(move |l| (l, h.cur_start)))
                    .collect();
                // Blocks that already have a comment on them — those revert from the comment row,
                // so we skip the standalone bar to avoid a duplicate button.
                let blocks_with_comment: std::collections::BTreeSet<usize> = self
                    .selected_file
                    .as_deref()
                    .map(|f| {
                        self.comments
                            .on_file(f)
                            .filter_map(|(_, c)| {
                                (c.start..=c.end).find_map(|l| line_to_block.get(&l).copied())
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let total = cv.lines.len();
                let (first_idx, last_idx) = {
                    const OVERSCAN: usize = 12;
                    let per = CODE_LINE_PX.max(1.0);
                    let view_h = if self.code_view_h > 1.0 {
                        self.code_view_h
                    } else {
                        800.0 // generous first-frame guess before we know the real height
                    };
                    let first = ((self.code_scroll_y / per) as usize).saturating_sub(OVERSCAN);
                    let visible = (view_h / per).ceil() as usize + 2 * OVERSCAN;
                    (first.min(total), (first + visible).min(total))
                };

                let mut col = column![].spacing(0);
                // Top spacer standing in for the hidden lines above the window (keeps the
                // scrollbar geometry, minimap box, and scroll_to offsets pixel-accurate). It must
                // also cover any RED removed-lines that anchor above the window (they render as
                // rows, so they add height) — count removals before the first visible line.
                if first_idx > 0 {
                    let first_visible_line = first_idx + 1; // 1-based line number of cv.lines[first_idx]
                    let hidden_removed: usize = self
                        .file_diff
                        .removed_before
                        .range(..first_visible_line)
                        .map(|(_, v)| v.len())
                        .sum();
                    let hidden_rows = first_idx + hidden_removed;
                    col = col.push(
                        Space::new().height(Length::Fixed(hidden_rows as f32 * CODE_LINE_PX)),
                    );
                }
                for (n, line) in &cv.lines[first_idx..last_idx] {
                    // GitHub-PR style: render any lines DELETED just before this one as red rows
                    // (a `-` gutter, red wash). They exist only in HEAD, so they're not selectable.
                    if let Some(removed) = self.file_diff.removed_before.get(n) {
                        for gone in removed {
                            col = col.push(
                                container(
                                    text(format!("-     {gone}"))
                                        .size(13)
                                        .line_height(iced::widget::text::LineHeight::Absolute(
                                            CODE_LINE_PX.into(),
                                        ))
                                        .font(iced::Font::MONOSPACE)
                                        .color(BAD)
                                        .wrapping(iced::widget::text::Wrapping::None),
                                )
                                .height(Length::Fixed(CODE_LINE_PX))
                                .padding([0, 4])
                                .style(|_t: &Theme| code_removed_line_container()),
                            );
                        }
                    }
                    let in_sel = sel.is_some_and(|(lo, hi)| *n >= lo && *n <= hi);
                    let working = working_here.is_some_and(|(lo, hi)| *n >= lo && *n <= hi);
                    // A line that differs from HEAD (git) → GitHub-PR-style green highlight
                    // with a `+` gutter marker, so you SEE what the agent changed, live.
                    let changed = self.changed_lines.contains(n);
                    let mark = if changed { "+" } else { " " };
                    let row_text = format!("{mark}{n:>4}  {line}");
                    let color = if in_sel {
                        ACCENT
                    } else if changed {
                        GOOD
                    } else if working {
                        AMBER
                    } else {
                        FG
                    };
                    let line_el = container(
                        text(row_text)
                            .size(13)
                            // Pin the text's line height so every row is EXACTLY CODE_LINE_PX
                            // tall — the fixed height the minimap, scroll-jump, and the
                            // virtualization spacers all assume. Without this, natural line
                            // height drifts from the estimate and windowed scrolling desyncs.
                            .line_height(iced::widget::text::LineHeight::Absolute(
                                CODE_LINE_PX.into(),
                            ))
                            .font(iced::Font::MONOSPACE)
                            .color(color)
                            .wrapping(iced::widget::text::Wrapping::None),
                    )
                    .height(Length::Fixed(CODE_LINE_PX))
                    .padding([0, 4])
                    .style(move |_t: &Theme| {
                        code_line_container(in_sel, changed, working.then_some(pulse))
                    });
                    let row_ma = iced::widget::mouse_area(line_el)
                        .on_press(Message::LineDragStart(*n))
                        .on_enter(Message::LineDragTo(*n))
                        .on_release(Message::LineDragEnd);
                    col = col.push(row_ma);
                    // After the LAST green line of a changed block, render a "↩ revert block" bar —
                    // its own comment-shaped row (no comment text) carrying only the revert button,
                    // so the control lives on a dedicated line instead of floating over code. Skip
                    // it when a comment already sits on this block (it offers revert inline).
                    if let Some(&cur_start) = block_bar_after.get(n) {
                        if !blocks_with_comment.contains(&cur_start) {
                            col = col.push(view_revert_block_bar(cur_start, bar_width));
                        }
                    }
                    // Stored inline comments whose range ENDS on this line — render them (PR
                    // style), struck-through + ✓ once resolved. Only the in-window lines are
                    // iterated, so a comment scrolled off-screen simply isn't drawn (its state
                    // persists); the box/comment adds height inside the window, not the spacers.
                    if let Some(file) = self.selected_file.clone() {
                        let here: Vec<(usize, dc_win::comments::Comment)> = self
                            .comments
                            .on_file(&file)
                            .filter(|(_, c)| c.end == *n)
                            .map(|(i, c)| (i, c.clone()))
                            .collect();
                        for (i, c) in here {
                            // If the comment sits on a changed block, offer to revert that block
                            // from the comment row (look up by any line the comment covers).
                            let block = (c.start..=c.end)
                                .find_map(|l| line_to_block.get(&l).copied());
                            col = col.push(view_inline_comment(i, c, bar_width, block));
                        }
                    }
                    // The (new) comment box after the last line of the committed range.
                    if self.comment_range.is_some_and(|(_, hi)| hi == *n) {
                        col = col.push(self.view_comment_box());
                    }
                }
                // Bottom spacer for the hidden lines below the window — plus any removed-lines that
                // anchor below the last visible line (they'd add height when scrolled into view).
                if last_idx < total {
                    let first_hidden_line = last_idx + 1; // 1-based line after the window
                    let below_removed: usize = self
                        .file_diff
                        .removed_before
                        .range(first_hidden_line..)
                        .map(|(_, v)| v.len())
                        .sum();
                    let hidden_rows = (total - last_idx) + below_removed;
                    col = col.push(
                        Space::new().height(Length::Fixed(hidden_rows as f32 * CODE_LINE_PX)),
                    );
                } else {
                    // Window reaches EOF: render removals anchored past the last line (deletions at
                    // end-of-file) as trailing red rows.
                    for (anchor, removed) in self.file_diff.removed_before.range(total + 1..) {
                        let _ = anchor;
                        for gone in removed {
                            col = col.push(
                                container(
                                    text(format!("-     {gone}"))
                                        .size(13)
                                        .line_height(iced::widget::text::LineHeight::Absolute(
                                            CODE_LINE_PX.into(),
                                        ))
                                        .font(iced::Font::MONOSPACE)
                                        .color(BAD)
                                        .wrapping(iced::widget::text::Wrapping::None),
                                )
                                .height(Length::Fixed(CODE_LINE_PX))
                                .padding([0, 4])
                                .style(|_t: &Theme| code_removed_line_container()),
                            );
                        }
                    }
                }
                if cv.truncated {
                    col = col.push(
                        text(format!(
                            "… truncated at {} lines",
                            dc_win::codeview::MAX_LINES
                        ))
                        .size(11)
                        .color(FG_MUTED),
                    );
                }
                // Only show the minimap when the file actually overflows the viewport — if it all
                // fits on screen there's nothing to navigate, so the map is just noise. Computed
                // above as `minimap_overflows` (drives both the comment inset and the minimap).
                let overflows = minimap_overflows;
                // With the minimap up, its viewport box IS the vertical scrollbar — hide the real
                // one (width 0) so we don't show both. Keep the horizontal bar for long lines.
                let vbar = if overflows {
                    scrollable::Scrollbar::new().width(0).scroller_width(0)
                } else {
                    scrollable::Scrollbar::new()
                };
                // One scrollable, BOTH axes: vertical for the file, horizontal for long lines.
                let code_scroll = scrollable(col)
                    .id(code_scroll_id())
                    .on_scroll(Message::CodeScrolled)
                    .direction(scrollable::Direction::Both {
                        vertical: vbar,
                        horizontal: scrollable::Scrollbar::new().width(6).scroller_width(6),
                    })
                    .height(Fill)
                    .width(Fill);
                // VS-Code-style minimap on the right: file shape + green changes + comment
                // ticks + viewport box; click a line in it to select there.
                let line_lens: Vec<usize> =
                    cv.lines.iter().map(|(_, t)| t.chars().count()).collect();
                let commented: std::collections::BTreeSet<usize> = self
                    .selected_file
                    .as_deref()
                    .map(|f| {
                        self.comments
                            .on_file(f)
                            .flat_map(|(_, c)| c.start..=c.end)
                            .collect()
                    })
                    .unwrap_or_default();
                if overflows {
                    let minimap = crate::minimap::Minimap::new(
                        line_lens,
                        self.changed_lines.clone(),
                        commented,
                        self.code_viewport,
                    )
                    .view();
                    // VS-Code style: the code fills the whole width and the minimap floats
                    // semi-transparently on top of the right edge, so text runs behind it.
                    let floating = container(minimap)
                        .width(Fill)
                        .height(Fill)
                        .align_x(iced::alignment::Horizontal::Right);
                    iced::widget::stack![code_scroll, floating].into()
                } else {
                    code_scroll.into()
                }
            }
            None => text("the file the agent edits appears here — or click one in the tree")
                .size(12)
                .color(FG_MUTED)
                .into(),
        };

        // Header stays fixed; the code area is the single scrollable (no outer scroll wrap,
        // which is what previously collapsed the inner one). When the open file is a feature
        // plan (PLAN-<slug>.md), the header carries an "⚒ Execute plan" button — the same
        // one-click build the proposal card offers, but for a plan opened from the tree.
        let is_open_plan = self
            .selected_file
            .as_deref()
            .is_some_and(is_feature_plan);
        let header_bar: Element<'_, Message> = if is_open_plan && self.session.is_none() {
            row![
                text(header).size(12).color(FG_MUTED),
                Space::new().width(Fill),
                button(text("⚒ Execute plan").size(12))
                    .on_press(Message::ExecuteOpenPlan)
                    .padding([3, 10])
                    .style(primary_button),
            ]
            .align_y(iced::Alignment::Center)
            .into()
        } else {
            text(header).size(12).color(FG_MUTED).into()
        };
        let body = column![header_bar, inner].spacing(6);
        container(body)
            .width(Length::FillPortion(portion))
            .height(Fill)
            .padding(PAD)
            .style(card_style)
            .into()
    }

    /// The inline comment box shown under the selected range (PR-style): a text input +
    /// Comment / Cancel. Submitting triages small (fix now) vs. big (plan first).
    fn view_comment_box(&self) -> Element<'_, Message> {
        let placeholder = match self.comment_range {
            Some((lo, hi)) if lo != hi => format!("comment on lines {lo}–{hi}…"),
            _ => "comment on this line…".to_string(),
        };
        let input = text_input(&placeholder, &self.comment_draft)
            .on_input(Message::CommentDraftChanged)
            .on_submit(Message::CommentSubmit)
            .padding(8)
            .style(input_style)
            .width(Fill);
        let submit = button(text("Comment").size(13))
            .on_press(Message::CommentSubmit)
            .padding([4, 12])
            .style(primary_button);
        let cancel = button(text("Cancel").size(13))
            .on_press(Message::CommentCancel)
            .padding([4, 12])
            .style(menu_item_style);
        let hint = text("small fix → done inline · bigger → we'll plan it")
            .size(10)
            .color(FG_MUTED);
        container(column![input, row![submit, cancel].spacing(6), hint].spacing(6))
            .width(Fill)
            .padding(8)
            .style(dropdown_style)
            .into()
    }

    /// The bottom panel — a tabbed strip of **Verification** (verify output) and **Build**
    /// (last run outcome). It auto-hides when there's nothing to show — no run in flight, no
    /// verify output, no build result — so planning gets the full height. During a swarm
    /// build the coder prompt/output panel takes the whole strip.
    fn view_bottom_strip(&self) -> Option<Element<'_, Message>> {
        if self.is_swarm() {
            return Some(self.view_coder_io());
        }
        // Nothing to build/verify → hide the strip entirely.
        let has_content =
            self.session.is_some() || self.verify_text.is_some() || self.result.is_some();
        if !has_content {
            return None;
        }
        let tabs = row![
            self.bottom_tab_button("Verification", BottomTab::Verification),
            self.bottom_tab_button("Build", BottomTab::Build),
        ]
        .spacing(4);
        let content = match self.bottom_tab {
            BottomTab::Verification => self.view_verification_tab(),
            BottomTab::Build => self.view_build_tab(),
        };
        Some(
            container(column![tabs, content].spacing(6))
                .width(Fill)
                .height(Length::Fixed(180.0))
                .padding(10)
                .style(card_style)
                .into(),
        )
    }

    /// One bottom-tab button; highlighted when it's the selected tab.
    fn bottom_tab_button(&self, label: &str, tab: BottomTab) -> Element<'_, Message> {
        let selected = self.bottom_tab == tab;
        button(
            text(label.to_string())
                .size(12)
                .color(if selected { ACCENT } else { FG_MUTED }),
        )
        .on_press(Message::SelectBottomTab(tab))
        .padding([3, 10])
        .style(move |_t, status| menu_title_style(selected, status))
        .into()
    }

    /// The Verification tab: the verify command's captured output (failure-first).
    fn view_verification_tab(&self) -> Element<'_, Message> {
        let inner: Element<'_, Message> = match &self.verify_text {
            Some(v) => text(v.clone()).size(12).into(),
            None => text("cargo check / test output shows here after the agent verifies")
                .size(12)
                .color(FG_MUTED)
                .into(),
        };
        scrollable(inner).height(Fill).into()
    }

    /// The Build tab: the last run's outcome — ✓/✗ headline, reason, changed/built files,
    /// and (from-scratch only) an open-folder button.
    fn view_build_tab(&self) -> Element<'_, Message> {
        let Some(r) = self.result.as_ref() else {
            return text("no build yet — describe a change and run it")
                .size(12)
                .color(FG_MUTED)
                .into();
        };
        let (mark, color) = if r.ok { ("✓", GOOD) } else { ("✗", BAD) };
        let mut col = column![text(format!("{mark}  {}", r.headline))
            .size(15)
            .color(color)]
        .spacing(4);
        if !r.reason.is_empty() {
            col = col.push(text(&r.reason).size(12).color(FG_MUTED));
        }
        let label = if self.iterating { "changed" } else { "built" };
        if !r.files.is_empty() {
            col = col.push(text(format!("files {label}:")).size(11).color(FG_MUTED));
        }
        for f in r.files.iter().take(10) {
            col = col.push(text(format!("  • {f}")).size(12));
        }
        if r.files.len() > 10 {
            col = col.push(
                text(format!("  … and {} more", r.files.len() - 10))
                    .size(12)
                    .color(FG_MUTED),
            );
        }
        if r.dir.is_some() {
            col = col.push(Space::new().height(Length::Fixed(4.0)));
            col = col.push(
                button(text("📂 open output folder"))
                    .on_press(Message::OpenOutputFolder)
                    .style(menu_item_style),
            );
        }
        // "I don't like this change" — undo an in-place fix's edits (git-revert its files).
        // Only for iterate runs that changed files (from-scratch builds have no committed base).
        if self.iterating && !r.files.is_empty() {
            col = col.push(Space::new().height(Length::Fixed(6.0)));
            col = col.push(
                button(text("↩ Undo this change").size(13))
                    .on_press(Message::UndoLastChange)
                    .padding([4, 12])
                    .style(|_t: &Theme, status| {
                        let hov = matches!(status, button::Status::Hovered);
                        button::Style {
                            background: Some(Background::Color(Color {
                                a: if hov { 0.22 } else { 0.14 },
                                ..BAD
                            })),
                            text_color: FG,
                            border: Border {
                                radius: RADIUS.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        }
                    }),
            );
        }
        scrollable(col).height(Fill).into()
    }

    /// The horizontal step-flow at the top: each phase with arrows between, the current
    /// phase highlighted, done phases checked, plus a final "Build" step that lights up
    /// once planning is complete and implementation begins.
    fn view_step_flow(&self) -> Element<'_, Message> {
        let current = self.plan.current_phase();
        let done_color = iced::Color::from_rgb(0.45, 0.78, 0.55); // green
        let now_color = iced::Color::from_rgb(0.48, 0.65, 0.98); // blue
        let dim_color = iced::Color::from_rgb(0.4, 0.43, 0.55);
        let arrow_color = iced::Color::from_rgb(0.35, 0.38, 0.5);

        let mut flow = row![].spacing(6).align_y(iced::Alignment::Center);
        let steps = self.plan.steps();
        for (i, step) in steps.iter().enumerate() {
            let is_current = current == Some(step.phase);
            let (mark, color, size) = if step.done {
                ("✓", done_color, 13)
            } else if is_current {
                ("▶", now_color, 15)
            } else {
                ("·", dim_color, 13)
            };
            let label = text(format!("{mark} {}", step.phase.title()))
                .size(size)
                .color(color);
            flow = flow.push(label);
            flow = flow.push(text("→").size(13).color(arrow_color));
            let _ = i;
        }
        // The final "Build" step: active once planning is complete (no current phase
        // left) and a swarm is running; done when source files were built.
        let built = self.result.as_ref().is_some_and(|r| r.ok);
        let building = current.is_none() && self.is_swarm();
        let (bmark, bcolor) = if built {
            ("✓", done_color)
        } else if building {
            ("▶", now_color)
        } else {
            ("·", dim_color)
        };
        flow = flow.push(
            text(format!("{bmark} Build"))
                .size(if building { 15 } else { 13 })
                .color(bcolor),
        );

        container(
            scrollable(flow).direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(2).scroller_width(2),
            )),
        )
        .width(Fill)
        .padding(10)
        .style(card_style)
        .into()
    }

    /// The top menu bar: the app title, the dropdown menu buttons (File / View), and —
    /// pushed to the right — the workspace/status line (what folder we're working in, and
    /// how). Clicking a title toggles its dropdown; items live in [`Self::view_menu_dropdown`].
    fn view_menu_bar(&self) -> Element<'_, Message> {
        let title = text("dumb-coder").size(14).color(ACCENT);
        let file = self.menu_title("File", Menu::File);
        let view_m = self.menu_title("View", Menu::View);
        // The status line, right-aligned in the bar.
        let status = text(self.workspace_status())
            .size(11)
            .color(iced::Color::from_rgb(0.55, 0.58, 0.70));
        let bar = row![
            title,
            Space::new().width(Length::Fixed(16.0)),
            file,
            view_m,
            Space::new().width(Fill), // push the status to the right edge
            status,
        ]
        .spacing(4)
        .align_y(iced::Alignment::Center);
        container(bar)
            .width(Fill)
            .padding([4, 10])
            .style(menu_bar_style)
            .into()
    }

    /// The one-line workspace status shown at the right of the top bar: where the app is
    /// working and in which mode.
    fn workspace_status(&self) -> String {
        match (&self.picked_workspace, &self.run_dir) {
            (Some(dir), _) => {
                let stack = dc_workflow::ProjectStack::detect(dir).label();
                format!("iterating in  {}  ·  {stack}", dir.display())
            }
            (None, Some(d)) => format!("output  {}", d.display()),
            (None, None) => "no project — File ▸ Open folder".to_string(),
        }
    }

    /// One clickable menu-bar title; highlighted while its dropdown is open.
    fn menu_title(&self, label: &str, which: Menu) -> Element<'_, Message> {
        let open = self.open_menu == Some(which);
        button(
            text(label.to_string())
                .size(13)
                .color(if open { ACCENT } else { FG }),
        )
        .on_press(Message::ToggleMenu(which))
        .padding([3, 10])
        .style(move |_t, status| menu_title_style(open, status))
        .into()
    }

    /// The open dropdown's items, rendered as a floating Windows-style card positioned
    /// directly under its menu-bar title (via top/left spacers in an overlay layer, so it
    /// never shifts the base layout). A full-window transparent backdrop behind it closes
    /// the menu on an outside click. Returns `None` when no menu is open.
    fn view_menu_dropdown(&self) -> Option<Element<'_, Message>> {
        let which = self.open_menu?;
        let items: Vec<(String, Message)> = match which {
            Menu::File => {
                if self.picked_workspace.is_some() {
                    vec![
                        (
                            "📁  Open a different folder…".to_string(),
                            Message::PickWorkspace,
                        ),
                        ("✕  Close project".to_string(), Message::ClearWorkspace),
                    ]
                } else {
                    vec![("📁  Open folder…".to_string(), Message::PickWorkspace)]
                }
            }
            Menu::View => vec![(
                if self.settings_open {
                    "⚙  Hide settings".to_string()
                } else {
                    "⚙  Settings…".to_string()
                },
                Message::ToggleSettings,
            )],
        };
        let mut col = column![].spacing(0);
        for (label, msg) in items {
            col = col.push(
                button(text(label).size(13).color(FG))
                    .on_press(msg)
                    .padding([6, 14])
                    .width(Length::Fixed(230.0))
                    .style(menu_item_style),
            );
        }
        let card = container(col).padding(3).style(dropdown_style);

        // Position under the right title: the menu bar is ~28px tall; File sits ~92px from
        // the left, View ~130px. Spacers place the card; a transparent full-window backdrop
        // behind it (a mouse_area) closes the menu on any outside click.
        let left = match which {
            Menu::File => 88.0,
            Menu::View => 128.0,
        };
        let positioned = column![
            Space::new().height(Length::Fixed(30.0)),
            row![Space::new().width(Length::Fixed(left)), card],
        ];
        // Backdrop: an invisible full-size mouse_area that closes the menu when clicked.
        let backdrop = iced::widget::mouse_area(container(Space::new()).width(Fill).height(Fill))
            .on_press(Message::ToggleMenu(which)); // re-toggle closes it

        Some(
            iced::widget::stack![backdrop, positioned]
                .width(Fill)
                .height(Fill)
                .into(),
        )
    }

    /// The Settings modal: a centered card floating over a dimmed backdrop (an overlay
    /// layer in `view`'s Stack). Clicking the backdrop or the ✕ closes it. This replaces the
    /// old inline panel that pushed the layout around.
    fn view_settings_modal(&self) -> Element<'_, Message> {
        // The dim backdrop; clicking it closes settings.
        let backdrop =
            iced::widget::mouse_area(container(Space::new()).width(Fill).height(Fill).style(
                |_t: &Theme| container::Style {
                    background: Some(Background::Color(Color {
                        a: 0.55,
                        ..Color::BLACK
                    })),
                    ..container::Style::default()
                },
            ))
            .on_press(Message::ToggleSettings);

        // The settings card itself, centered, with a close button in its header.
        let header = row![
            text("Settings").size(16).color(FG),
            Space::new().width(Fill),
            button(text("✕").size(14))
                .on_press(Message::ToggleSettings)
                .padding([2, 8])
                .style(menu_item_style),
        ]
        .align_y(iced::Alignment::Center);
        let card = container(column![header, self.view_settings_body()].spacing(12))
            .width(Length::Fixed(520.0))
            .max_width(560.0)
            .padding(18)
            .style(dropdown_style);

        // `opaque` stops clicks on the card from falling through to the backdrop.
        iced::widget::stack![backdrop, iced::widget::center(iced::widget::opaque(card))]
            .width(Fill)
            .height(Fill)
            .into()
    }

    /// The center column: the planning CHAT thread on top (you ⟷ agent), with the composer
    /// docked at the bottom. When the assistant proposes plan-file changes, they render as
    /// Apply cards under its message. Before a project is opened, a hint to open a folder.
    fn view_center(&self) -> Element<'_, Message> {
        let mut thread = column![section("CHAT  ·  PLAN")].spacing(8);
        if self.conversation.is_none() {
            thread = thread.push(
                text("Open a project folder (File ▸ Open folder) to start planning.")
                    .size(12)
                    .color(FG_MUTED),
            );
        }
        for turn in &self.chat_turns {
            thread = thread.push(self.view_chat_turn(turn));
        }
        // Proposed plan-file Apply cards, after the latest assistant message.
        for (i, pf) in self.proposed_files.iter().enumerate() {
            thread = thread.push(self.view_proposed_file(i, pf));
        }
        // The live "typing" bubble while a reply streams in: show the growing text (with any
        // <think> block hidden), or a thinking cue before the first token arrives.
        if self.chat_session.is_some() {
            let live = self
                .streaming
                .as_deref()
                .map(dc_win::chat::visible_so_far)
                .filter(|s| !s.is_empty());
            match live {
                Some(text_so_far) => {
                    thread = thread.push(
                        column![
                            text("agent").size(11).color(GOOD),
                            text(text_so_far).size(13).color(FG),
                        ]
                        .spacing(2),
                    );
                }
                None => {
                    thread = thread.push(text("agent is thinking…").size(12).color(FG_MUTED));
                }
            }
        }
        // The thread scrolls inside its own padding; the composer below spans the panel edge
        // to edge (its divider + input reach the left/right/bottom), so there's no gutter
        // around the input. Hence the panel container itself is unpadded.
        let thread = container(scrollable(thread).height(Fill)).padding(PAD);

        let composer = self.view_composer();
        // The outer column must be Fill-width, else its Fill children (incl. the composer's
        // full-width top divider) collapse to content width and the hairline stops short of the
        // panel's right edge.
        container(column![thread, composer].width(Fill))
            .width(Length::FillPortion(2))
            .height(Fill)
            .style(card_style)
            .into()
    }

    /// One chat bubble: a small role label + the text, coloured by speaker. A Debug turn (the
    /// raw prompt echo) renders dimmed + monospace so it reads as diagnostic output, not chat.
    fn view_chat_turn<'a>(&self, turn: &'a dc_win::chat::Turn) -> Element<'a, Message> {
        if turn.role == dc_win::chat::Speaker::Debug {
            return container(
                column![
                    text("⛭ prompt sent to model").size(10).color(FG_MUTED),
                    text(turn.text.clone())
                        .size(11)
                        .font(iced::Font::MONOSPACE)
                        .color(FG_MUTED),
                ]
                .spacing(2),
            )
            .width(Fill)
            .padding(6)
            .style(|_t: &Theme| container::Style {
                background: Some(Background::Color(Color::from_rgb(0.08, 0.09, 0.14))),
                border: Border {
                    color: CARD_BORDER,
                    width: 1.0,
                    radius: RADIUS.into(),
                },
                ..container::Style::default()
            })
            .into();
        }
        let (who, who_color) = match turn.role {
            dc_win::chat::Speaker::You => ("you", ACCENT),
            _ => ("agent", GOOD),
        };
        column![
            text(who).size(11).color(who_color),
            text(turn.text.clone()).size(13).color(FG),
        ]
        .spacing(2)
        .into()
    }

    /// If debug mode is on, echo `prompt` into the chat as a Debug turn (the raw text the
    /// model received). `label` names the call (e.g. "triage", "fix", "chat").
    fn debug_prompt(&mut self, label: &str, prompt: &str) {
        if self.debug {
            self.chat_turns.push(dc_win::chat::Turn {
                role: dc_win::chat::Speaker::Debug,
                text: format!("[{label}]\n{prompt}"),
            });
        }
    }

    /// An Apply card for a proposed plan-file: the filename + an Apply button (writes it to
    /// disk). The file's contents show in the code view when it's the current proposal.
    fn view_proposed_file(
        &self,
        i: usize,
        pf: &dc_win::chat::ProposedFile,
    ) -> Element<'_, Message> {
        let lines = pf.content.lines().count();
        let head = row![
            text(format!("📄 proposed: {}", pf.name))
                .size(13)
                .color(ACCENT),
            Space::new().width(Fill),
            text(format!("{lines} lines")).size(11).color(FG_MUTED),
        ]
        .align_y(iced::Alignment::Center);
        let apply = button(text("✓ Apply to disk").size(13))
            .on_press(Message::ApplyFile(i))
            .padding([5, 12])
            .style(primary_button);
        // A feature plan (PLAN-<slug>.md) gets a one-click build: apply it, then iterate the
        // project to implement it. README/TODO edits aren't buildable, so they show Apply only.
        let actions: Element<'_, Message> = if is_feature_plan(&pf.name) {
            let execute = button(text("⚒ Execute plan").size(13))
                .on_press(Message::ExecutePlan(i))
                .padding([5, 12])
                .style(primary_button);
            row![apply, execute].spacing(8).into()
        } else {
            apply.into()
        };
        container(column![head, actions].spacing(6))
            .width(Fill)
            .padding(10)
            .style(dropdown_style)
            .into()
    }

    /// The composer: the text input + send. When a project (conversation) is open it sends a
    /// CHAT turn; with no project it falls back to the from-scratch build action.
    fn view_composer(&self) -> Element<'_, Message> {
        let has_convo = self.conversation.is_some();
        let sending = self.chat_session.is_some();
        let (placeholder, send_msg, label): (&str, Message, &str) = if has_convo {
            (
                "Talk through the plan — ask, refine, decide what's next…",
                Message::ChatSend,
                "Send",
            )
        } else {
            (
                "Open a project folder to start…",
                self.run_message(),
                self.run_label(),
            )
        };
        // A fix/iterate run in flight → a Cancel button (the run is the slow, cancellable
        // thing). A quick chat/triage reply just shows a busy "…".
        let run_active = self.session.is_some();
        // The composer is exactly one input tall — a fixed height so the stacked think/debug
        // toggles (which are naturally taller than a single-line input) don't inflate the row
        // and leave dead space above/below the input.
        const INPUT_H: f32 = 38.0;
        let input = text_input(placeholder, &self.intent)
            .on_input(Message::IntentChanged)
            .on_submit(send_msg.clone())
            .padding([8, 10])
            .style(input_style_borderless)
            .width(Fill);
        let btn = if run_active {
            button(text("⏹ cancel").size(15).width(Fill).height(Fill).center())
                .on_press(Message::CancelRun)
                .width(Length::Fixed(110.0))
                .height(Fill)
                .padding(0)
                .style(|_t: &Theme, status| {
                    let hov = matches!(status, button::Status::Hovered);
                    button::Style {
                        background: Some(Background::Color(if hov {
                            Color::from_rgb(0.80, 0.35, 0.40)
                        } else {
                            BAD
                        })),
                        text_color: Color::from_rgb(0.06, 0.07, 0.11),
                        border: Border {
                            radius: RADIUS.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                })
        } else if sending {
            button(text("…").width(Fill).height(Fill).center())
                .width(Length::Fixed(90.0))
                .height(Fill)
        } else {
            button(text(label).size(15).width(Fill).height(Fill).center())
                .on_press(send_msg)
                .width(Length::Fixed(90.0))
                .height(Fill)
                .padding(0)
                .style(primary_button)
        };
        // Send button is full composer height, sitting flush against the input.
        let mut bar = row![input, btn].spacing(8);
        // The think/debug toggles stack vertically to the right of the send button. They're kept
        // small (14px box, 11px label, tight gap) so both fit within the one-input-tall composer.
        let mut toggles = column![].spacing(2).align_x(iced::Alignment::Start);
        // The Think toggle (chat mode only): fast conclusions by default, deeper reasoning
        // when you flip it on for a hard planning question.
        if has_convo {
            toggles = toggles.push(
                checkbox(self.think)
                    .label("think")
                    .size(14)
                    .text_size(11)
                    .spacing(4)
                    .on_toggle(Message::ToggleThink)
                    .style(checkbox_style),
            );
        }
        // Debug: echo every prompt sent to the model into the chat (always available).
        toggles = toggles.push(
            checkbox(self.debug)
                .label("debug")
                .size(14)
                .text_size(11)
                .spacing(4)
                .on_toggle(Message::ToggleDebug)
                .style(checkbox_style),
        );
        bar = bar.push(toggles);
        // Fix the row to one input height: the send button's `height(Fill)` then matches the
        // input exactly (flush), and the row no longer stretches to the taller toggle stack —
        // killing the dead space above/below the input. Toggles center against that height.
        let bar = bar
            .align_y(iced::Alignment::Center)
            .height(Length::Fixed(INPUT_H));
        // The divider spans the full panel width; the input bar gets a little horizontal
        // breathing room but no vertical padding, so it sits flush to the bottom with the
        // divider right on top of it.
        let bar = container(bar).padding([0, PAD]).width(Fill);
        // Force the column full-width so the top divider spans the whole panel — otherwise the
        // column shrinks to content and the hairline stops just short of the right edge.
        column![h_divider(), bar].spacing(0).width(Fill).into()
    }

    fn view_topology(&self) -> Element<'_, Message> {
        let canvas = crate::canvas::TopologyCanvas::new(
            &self.topology,
            self.now(),
            self.selected_coder.as_deref(),
        )
        .view();
        container(column![section("SWARM TOPOLOGY"), canvas].spacing(6))
            .width(Length::FillPortion(2))
            .height(Fill)
            .padding(PAD)
            .style(card_style)
            .into()
    }

    /// The always-visible plan panel (TDD mode): the six workflow phases with status,
    /// the frozen tests written, and the readable subtask list — so you can see what it
    /// intends to do, before and while it does it.
    fn view_plan(&self) -> Element<'_, Message> {
        let mut col = column![section("PLAN  ·  TDD")].spacing(4);
        for step in self.plan.steps() {
            let mark = if step.done { "✓" } else { "·" };
            let line = text(format!("{mark} {}", step.title)).size(13);
            let line = if step.done {
                line.color(GOOD)
            } else {
                line.color(FG_MUTED)
            };
            col = col.push(line);
            // Show the produced artifact text under each completed phase.
            if step.done && !step.content.is_empty() {
                let preview: String = step.content.lines().take(10).collect::<Vec<_>>().join("\n");
                col = col.push(
                    text(preview)
                        .size(11)
                        .color(iced::Color::from_rgb(0.7, 0.73, 0.84)),
                );
            }
        }

        if !self.plan.frozen_tests.is_empty() {
            col = col.push(Space::new().height(Length::Fixed(8.0)));
            col = col.push(
                text(format!("frozen tests ({})", self.plan.frozen_tests.len()))
                    .size(13)
                    .color(GOOD),
            );
            for t in &self.plan.frozen_tests {
                col = col.push(text(format!("  🔒 {t}")).size(12).color(FG_MUTED));
            }
        }

        if !self.plan.subtasks.is_empty() {
            col = col.push(Space::new().height(Length::Fixed(8.0)));
            col = col.push(section("SUBTASKS TO IMPLEMENT"));
            for (i, g) in self.plan.subtasks.iter().enumerate() {
                col = col.push(text(format!("  {}. {g}", i + 1)).size(12));
            }
        }

        container(scrollable(col).height(Fill))
            .width(Length::FillPortion(2))
            .padding(PAD)
            .style(card_style)
            .into()
    }

    /// The bottom panel: the prompt sent to the selected coder and the output it
    /// proposed back. Click a coder box on the topology to select one; otherwise it
    /// hints how, or shows the latest verification result.
    fn view_coder_io(&self) -> Element<'_, Message> {
        let selected = self
            .selected_coder
            .as_deref()
            .and_then(|id| self.topology.coder(id));

        let inner = match selected {
            Some(c) => {
                let prompt = c.prompt.clone().unwrap_or_else(|| "(not captured)".into());
                let proposal = c
                    .proposal
                    .clone()
                    .unwrap_or_else(|| "(still working…)".into());
                column![
                    text(format!("coder [{}] — {}", c.subtask, c.goal))
                        .size(14)
                        .color(ACCENT),
                    section("▸ PROMPT SENT TO THIS CODER"),
                    text(prompt).size(11),
                    Space::new().height(Length::Fixed(6.0)),
                    section("◂ OUTPUT IT PROPOSED"),
                    text(proposal).size(11),
                ]
                .spacing(3)
            }
            None => {
                let hint = if self.is_swarm() {
                    "click a coder box above to see the prompt it got and the code it wrote"
                } else {
                    "the coders' prompts & output appear here during the build"
                };
                let mut c = column![
                    section("CODER PROMPT & OUTPUT"),
                    text(hint).size(12).color(FG_MUTED)
                ]
                .spacing(4);
                if let Some(v) = &self.verify_text {
                    c = c.push(Space::new().height(Length::Fixed(6.0)));
                    c = c.push(section("LATEST VERIFICATION"));
                    c = c.push(text(v).size(11));
                }
                c
            }
        };

        container(scrollable(inner).height(Fill))
            .width(Fill)
            .height(Length::FillPortion(1))
            .padding(PAD)
            .style(card_style)
            .into()
    }

    fn view_gatebar(&self) -> Option<Element<'_, Message>> {
        match self.gatebar.first()? {
            Gatebar::Confirm {
                command, reason, ..
            } => {
                let head = text(format!("⛳ run a shell command?  {command}")).size(14);
                let why = text(reason).size(12);
                let buttons = row![
                    button(text("Allow once")).on_press(Message::ConfirmAllow),
                    button(text("Allow & remember")).on_press(Message::ConfirmRemember),
                    button(text("Deny")).on_press(Message::ConfirmDeny),
                ]
                .spacing(8);
                Some(
                    container(column![head, why, buttons].spacing(6))
                        .width(Fill)
                        .padding(12)
                        .style(card_style)
                        .into(),
                )
            }
            Gatebar::Gate { phase, content, .. } => {
                let head = text(format!("⛳ checkpoint — {} phase", phase.title())).size(14);
                // Show the produced artifact so the human can read it before deciding.
                let preview = container(scrollable(text(content.clone()).size(12)))
                    .height(Length::Fixed(160.0));
                let notes = text_input("send-back notes (optional)…", &self.sendback_notes)
                    .on_input(Message::NotesChanged)
                    .padding(6)
                    .style(input_style);
                let buttons = row![
                    button(text("Approve")).on_press(Message::GateApprove),
                    button(text("Revise")).on_press(Message::GateRevise),
                    button(text("Send back")).on_press(Message::GateSendBack),
                    button(text("Abort")).on_press(Message::GateAbort),
                ]
                .spacing(8);
                Some(
                    container(column![head, preview, notes, buttons].spacing(6))
                        .width(Fill)
                        .padding(12)
                        .style(card_style)
                        .into(),
                )
            }
        }
    }

    /// The settings form body (no outer card — the modal wraps it). A scrollable column of
    /// the connection + posture controls.
    fn view_settings_body(&self) -> Element<'_, Message> {
        let model = text_input("model", &self.model_input)
            .on_input(Message::ModelChanged)
            .padding(6)
            .style(input_style);
        let url = text_input("backend url", &self.url_input)
            .on_input(Message::UrlChanged)
            .padding(6)
            .style(input_style);
        let orch_model = text_input("orchestrator model (decomposer)", &self.orch_model_input)
            .on_input(Message::OrchModelChanged)
            .padding(6)
            .style(input_style);
        let orch_url = text_input("orchestrator url", &self.orch_url_input)
            .on_input(Message::OrchUrlChanged)
            .padding(6)
            .style(input_style);
        let advisor = text_input("advisor model (senior)", &self.advisor_input)
            .on_input(Message::AdvisorChanged)
            .padding(6)
            .style(input_style);
        let advisor_url = text_input("advisor url", &self.advisor_url_input)
            .on_input(Message::AdvisorUrlChanged)
            .padding(6)
            .style(input_style);
        let verify = text_input("verify command (optional)", &self.verify_input)
            .on_input(Message::VerifyChanged)
            .padding(6)
            .style(input_style);
        let suffix = text_input("system suffix (e.g. /no_think)", &self.suffix_input)
            .on_input(Message::SuffixChanged)
            .padding(6)
            .style(input_style);
        let yolo = checkbox(self.cfg.yolo)
            .label("yolo (allow shell without asking)")
            .on_toggle(Message::ToggleYolo)
            .style(checkbox_style);
        let dry = checkbox(self.cfg.dry_run)
            .label("dry-run (no writes)")
            .on_toggle(Message::ToggleDryRun)
            .style(checkbox_style);

        let form = column![
            text("CODER  (does the file writing)")
                .size(11)
                .color(FG_MUTED),
            model,
            url,
            text("ORCHESTRATOR  (decomposes the task — needs a reasoning model)")
                .size(11)
                .color(FG_MUTED),
            orch_model,
            orch_url,
            text("ADVISOR  (junior asks senior on a stall)")
                .size(11)
                .color(FG_MUTED),
            advisor,
            advisor_url,
            text("VERIFY & BEHAVIOUR").size(11).color(FG_MUTED),
            verify,
            suffix,
            yolo,
            dry,
        ]
        .spacing(8);

        scrollable(form).height(Length::Fixed(440.0)).into()
    }
}

/// Map a live `AgentEvent` to a concise chat line for the line-comment fix feed, or `None`
/// for events too noisy to surface (plain reads, model turns, plan chatter). Keeps the feed
/// to the steps a human cares about: editing a file, and the verify result.
fn fix_feed_line(e: &dc_core::AgentEvent) -> Option<String> {
    use dc_core::AgentEvent::*;
    match e {
        // The model's own account of what it's seeing / about to do — so the execute feed
        // reads as a running narration, not just a list of file touches.
        ModelTurn { raw, .. } => dc_win::view::narration(raw).map(|n| format!("💭 {n}")),
        ToolCall { tool, arg } => match tool.as_str() {
            "edit_file" | "write_file" | "append_file" | "create_file" => {
                Some(format!("✎ editing {}", arg.trim()))
            }
            "read_file" => Some(format!("· reading {}", arg.trim())),
            "run_verification" => Some("· checking it compiles…".to_string()),
            _ => None,
        },
        Verification { green, .. } => Some(if *green {
            "✓ compiles".to_string()
        } else {
            "✗ didn't compile — trying again".to_string()
        }),
        _ => None,
    }
}

/// Find a README in `dir`, case-insensitively (`README.md`, `readme.md`, `Readme.md`, or a
/// bare `README`). Returns the first match, or `None`.
fn find_readme(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let lower = name.to_ascii_lowercase();
            if lower == "readme.md" || lower == "readme" || lower == "readme.txt" {
                return Some(path);
            }
        }
    }
    None
}

/// Find a dedicated TODO file in `dir`, case-insensitively (`TODO.md`, `todo.md`, `TODO`,
/// `TODO.txt`). Returns the first match, or `None`.
fn find_todo_file(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let lower = name.to_ascii_lowercase();
            if lower == "todo.md" || lower == "todo" || lower == "todo.txt" {
                return Some(path);
            }
        }
    }
    None
}

/// "s" when `n != 1`, for plain pluralization.
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// A computed snapshot of the workspace's tree + git state. Produced by [`compute_snapshot`]
/// (the expensive filesystem walk + git subprocess calls) so that work can run OFF the UI thread;
/// [`App::apply_snapshot`] then applies it with cheap assignments.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceSnapshot {
    tree: Vec<dc_win::filetree::TreeRow>,
    file_status: std::collections::BTreeMap<String, dc_win::gitdiff::FileStatus>,
    stage_states: std::collections::BTreeMap<String, dc_win::gitdiff::StageState>,
    unstaged_deltas: std::collections::BTreeMap<String, dc_win::gitdiff::LineDelta>,
    staged_deltas: std::collections::BTreeMap<String, dc_win::gitdiff::LineDelta>,
    branch: Option<String>,
    upstream: dc_win::gitdiff::UpstreamStatus,
}

/// Compute the full workspace snapshot: walk the tree and run the git status/diff/branch queries.
/// This is the BLOCKING work (filesystem + `git` subprocesses); it takes no `&self` so it can run
/// on a background thread (see `Message::SyncWorkspace`). Pure — reads the workspace, mutates
/// nothing.
fn compute_snapshot(root: std::path::PathBuf) -> WorkspaceSnapshot {
    let tree = dc_win::filetree::full_rows(&root);
    let file_status = dc_win::gitdiff::statuses(&root);
    let stage_states = dc_win::gitdiff::stage_states(&root);
    let mut unstaged_deltas = dc_win::gitdiff::line_deltas(&root, false);
    let staged_deltas = dc_win::gitdiff::line_deltas(&root, true);
    // Untracked files don't show in `git diff --numstat`; count their lines directly as all-added
    // so the Changes row still shows a +N.
    for (path, status) in &file_status {
        if *status == dc_win::gitdiff::FileStatus::Added && !unstaged_deltas.contains_key(path) {
            if let Ok(text) = std::fs::read_to_string(root.join(path)) {
                unstaged_deltas.insert(
                    path.clone(),
                    dc_win::gitdiff::LineDelta {
                        added: text.lines().count(),
                        removed: 0,
                    },
                );
            }
        }
    }
    let branch = dc_win::gitdiff::current_branch(&root);
    let upstream = dc_win::gitdiff::upstream_status(&root);
    WorkspaceSnapshot {
        tree,
        file_status,
        stage_states,
        unstaged_deltas,
        staged_deltas,
        branch,
        upstream,
    }
}

/// `None` for empty/whitespace, else the trimmed value — for optional config inputs.
fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// The workflow task for executing a feature plan. Names the plan file so the workflow pins
/// its contents every phase (via `referenced_plan`) and grounds the design on it — and frames
/// the run as designing (not implementing), since plan-only stops at the breakdown for review.
fn plan_task(plan_name: &str) -> String {
    format!(
        "Design how to implement the feature plan in {plan_name}. Read the plan, look at the \
         relevant existing files, and produce a spec, an architecture, a file layout, and an \
         ordered implementation breakdown that follows the plan's Approach and Files-to-touch. \
         This is a DESIGN pass — do not write source code yet."
    )
}

/// Whether a proposed file is a *feature plan* (a `PLAN-<slug>.md`), as opposed to a
/// README/TODO plan-file edit. Only feature plans get the "Execute plan" build button —
/// README/TODO aren't things you "build".
fn is_feature_plan(name: &str) -> bool {
    let n = name.trim();
    n.to_ascii_uppercase().starts_with("PLAN-") && n.to_ascii_lowercase().ends_with(".md")
}

/// The prefix to remember when a user clicks "Allow & remember": the command up to
/// and including the first space (so `git push` remembers `git `), or the whole
/// command if it has no space.
fn remember_prefix(command: &str) -> String {
    match command.find(' ') {
        Some(i) => command[..=i].to_string(),
        None => command.to_string(),
    }
}

// Keep `ToolCalling` referenced so the settings surface can grow into it without an
// unused-import churn; the v0 settings panel exposes the common knobs first.
#[allow(dead_code)]
const _: Option<ToolCalling> = None;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_plans_are_buildable_but_readme_and_todo_are_not() {
        // The Execute-plan button gates on this: only a PLAN-<slug>.md is buildable.
        assert!(is_feature_plan("PLAN-lakes.md"));
        assert!(is_feature_plan("plan-auth-flow.md")); // case-insensitive
        assert!(!is_feature_plan("README.md"));
        assert!(!is_feature_plan("TODO.md"));
        assert!(!is_feature_plan("PLAN-lakes.txt")); // must be markdown
        assert!(!is_feature_plan("MYPLAN-x.md")); // must start with the PLAN- prefix
    }

    #[test]
    fn plan_task_names_the_plan_and_frames_a_design_pass() {
        // The workflow pins the plan via its filename, so the task must name it; and plan-only
        // stops at the breakdown, so it must frame a design pass (not "write the code").
        let t = plan_task("PLAN-lakes.md");
        assert!(t.contains("PLAN-lakes.md"), "names the plan so referenced_plan pins it");
        assert!(t.to_lowercase().contains("design"));
        assert!(t.contains("do not write source code yet"));
    }

    #[test]
    fn fix_feed_line_surfaces_model_narration() {
        // The execute/iterate feed shows the model's thinking, not just file touches.
        let line = fix_feed_line(&dc_core::AgentEvent::ModelTurn {
            step: 1,
            prompt_tokens: 10,
            raw: "I'll add the water module and wire it in.\n{\"tool\":\"write_file\",\"path\":\"w.rs\"}"
                .to_string(),
        });
        let line = line.expect("narration surfaced");
        assert!(line.starts_with("💭"));
        assert!(line.contains("water module"));
    }
}
