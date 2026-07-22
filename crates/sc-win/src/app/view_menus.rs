//! App view: center layout, chat, composer, topology, settings tabs.

use super::*;
use iced::widget::{column, row};

impl App {
    /// The center column: the planning CHAT thread on top (you ⟷ agent), with the composer
    /// docked at the bottom. When the assistant proposes plan-file changes, they render as
    /// Apply cards under its message. Before a project is opened, a hint to open a folder.
    pub(crate) fn view_center(&self) -> Element<'_, Message> {
        let mut thread = column![section("CHAT  ·  PLAN")].spacing(8);
        if self.conversation.is_none() {
            thread = thread.push(
                text("Open a project folder (File ▸ Open folder) to start planning.")
                    .size(12)
                    .color(FG_MUTED),
            );
        }
        for (i, turn) in self.chat_turns.iter().enumerate() {
            thread = thread.push(self.view_chat_turn(i, turn));
        }
        // Proposed plan-file Apply cards, after the latest assistant message.
        for (i, pf) in self.proposed_files.iter().enumerate() {
            thread = thread.push(self.view_proposed_file(i, pf));
        }
        // A proposed command (the Command intent) → a Run card that pipes it into the terminal.
        if let Some(cmd) = &self.proposed_command {
            thread = thread.push(self.view_proposed_command(cmd));
        }
        // The live "typing" bubble while a reply streams in: show the growing text (with any
        // <think> block hidden), or a thinking cue before the first token arrives. Shown for a
        // chat/plan turn (a ChatSession) AND for a staged run streaming phases through `streaming`
        // (no ChatSession there — the session emits ChatDelta over UiEvent::Agent).
        if self.chat_session.is_some() || self.streaming.is_some() {
            let live = self
                .streaming
                .as_deref()
                .map(sc_win::chat::visible_so_far)
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
        // A pending workflow gate: the Approve / Send-back / Abort controls live at the bottom of
        // the chat, right under the phase content that just streamed in (they used to sit in a
        // separate left PLAN panel). Only shown while a gate is actually waiting.
        if let Some(controls) = self.view_gate_controls() {
            thread = thread.push(controls);
        }
        // The thread scrolls inside its own padding; the composer below spans the panel edge
        // to edge (its divider + input reach the left/right/bottom), so there's no gutter
        // around the input. Hence the panel container itself is unpadded.
        let thread = container(
            scrollable(thread)
                .id(chat_scroll_id())
                .on_scroll(Message::ChatScrolled)
                .height(Fill),
        )
        .padding(PAD);

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

    /// The workflow-gate controls, rendered inline at the bottom of the chat when a phase is
    /// waiting for a human decision. `None` when no gate is pending. Approve / Send back / Abort
    /// emit the same messages the old left-panel copy did (the send-back note is the fallback when
    /// no line comments were left on the phase's file in CODE — which auto-opens on the gate).
    pub(crate) fn view_gate_controls(&self) -> Option<Element<'_, Message>> {
        let phase = self.gating_phase()?;
        let buttons = row![
            button(text("Approve").size(13))
                .on_press(Message::GateApprove)
                .padding([4, 14])
                .style(primary_button),
            button(text("Send back").size(13))
                .on_press(Message::GateSendBack)
                .padding([4, 14])
                .style(stage_toggle_button),
            button(text("Abort").size(13))
                .on_press(Message::GateAbort)
                .padding([4, 14])
                .style(stage_toggle_button),
        ]
        .spacing(6);
        let notes = text_input("or a general send-back note…", &self.sendback_notes)
            .on_input(Message::NotesChanged)
            .padding(6)
            .size(12)
            .style(input_style);
        let card = column![
            text(format!("⛳ Review the {} above.", phase.title()))
                .size(13)
                .color(AMBER),
            text("Comment lines in the CODE view to change, then Send back — or Approve to continue.")
                .size(10)
                .color(FG_MUTED),
            buttons,
            notes,
        ]
        .spacing(6);
        Some(
            container(card)
                .width(Fill)
                .padding(10)
                .style(card_style)
                .into(),
        )
    }

    /// One chat bubble: a small role label + the text, coloured by speaker. A Debug turn (the
    /// raw prompt echo) renders dimmed + monospace so it reads as diagnostic output, not chat.
    /// Rebuild the per-turn selectable editor buffers when the chat thread changes. Cheap
    /// change-detection via a `(len, last_text_len)` signature so this is a no-op on the vast
    /// majority of update calls; only a new/edited turn triggers a full rebuild. Rebuilding
    /// wholesale (rather than diffing) keeps it simple and correct — chat threads are short.
    pub(crate) fn sync_chat_editors(&mut self) {
        use iced::widget::text_editor::Content;
        let last_len = self.chat_turns.last().map_or(0, |t| t.text.len());
        let sig = (self.chat_turns.len(), last_len);
        if sig == self.chat_sig && self.chat_editors.len() == self.chat_turns.len() {
            return;
        }
        self.chat_sig = sig;
        self.chat_editors = self
            .chat_turns
            .iter()
            .map(|t| Content::with_text(&t.text))
            .collect();
    }

    pub(crate) fn view_chat_turn<'a>(
        &'a self,
        i: usize,
        turn: &'a sc_win::chat::Turn,
    ) -> Element<'a, Message> {
        if turn.role == sc_win::chat::Speaker::Debug {
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
            sc_win::chat::Speaker::You => ("you", ACCENT),
            _ => ("agent", GOOD),
        };
        // A small copy button beside the speaker label — copies the whole message in one click
        // (alongside drag-select below).
        let copy = button(text("⧉ copy").size(10).color(FG_MUTED))
            .on_press(Message::CopyTurn(turn.text.clone()))
            .padding([1, 6])
            .style(menu_item_style);
        let header = row![
            text(who).size(11).color(who_color),
            Space::new().width(Fill),
            copy,
        ]
        .align_y(iced::Alignment::Center);

        // The message body: a read-only text_editor so it's drag-selectable + Ctrl+C-copyable.
        // It's styled transparent/borderless to read as plain chat text, not an input box. The
        // `on_action` handler drops edits (see `Message::ChatEditorAction`), so it's immutable.
        // Falls back to plain text if the editor buffer isn't synced yet (shouldn't happen).
        let body: Element<'a, Message> = match self.chat_editors.get(i) {
            Some(content) => iced::widget::text_editor(content)
                .size(13)
                .padding(0)
                .on_action(move |a| Message::ChatEditorAction(i, a))
                .style(|_t: &Theme, _s| iced::widget::text_editor::Style {
                    background: Background::Color(Color::TRANSPARENT),
                    border: Border::default(),
                    placeholder: FG_MUTED,
                    value: FG,
                    selection: Color { a: 0.35, ..ACCENT },
                })
                .into(),
            None => text(turn.text.clone()).size(13).color(FG).into(),
        };
        column![header, body].spacing(2).into()
    }

    /// If debug mode is on, echo `prompt` into the chat as a Debug turn (the raw text the
    /// model received). `label` names the call (e.g. "triage", "fix", "chat").
    pub(crate) fn debug_prompt(&mut self, label: &str, prompt: &str) {
        if self.debug {
            self.chat_turns.push(sc_win::chat::Turn {
                role: sc_win::chat::Speaker::Debug,
                text: format!("[{label}]\n{prompt}"),
            });
        }
    }

    /// An Apply card for a proposed plan-file: the filename + an Apply button (writes it to
    /// disk). The file's contents show in the code view when it's the current proposal.
    pub(crate) fn view_proposed_file(
        &self,
        i: usize,
        pf: &sc_win::chat::ProposedFile,
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
        // The Apply button: once applied, it reads "✓ Applied" and is inert (no on_press), so the
        // card records the write while its Breakdown/Build actions stay live.
        let apply = if pf.applied {
            button(text("✓ Applied").size(13).color(FG_MUTED))
                .padding([5, 12])
                .style(menu_item_style)
        } else {
            button(text("✓ Apply to disk").size(13))
                .on_press(Message::ApplyFile(i))
                .padding([5, 12])
                .style(primary_button)
        };
        // A feature plan (PLAN-<slug>.md) gets two one-click actions (both apply it to disk
        // first): Breakdown runs the staged DESIGN pipeline and stops for review; Build runs the
        // whole thing through to a green compile. These STAY available after applying (the card is
        // kept, marked applied), so you can review then act. README/TODO edits aren't buildable →
        // Apply only, and their card is removed on apply.
        let actions: Element<'_, Message> = if is_feature_plan(&pf.name) {
            let breakdown = button(text("☷ Breakdown").size(13))
                .on_press(Message::BreakdownPlan(i))
                .padding([5, 12])
                .style(menu_item_style);
            let build = button(text("⚒ Build").size(13))
                .on_press(Message::ExecutePlan(i))
                .padding([5, 12])
                .style(primary_button);
            row![apply, breakdown, build].spacing(8).into()
        } else {
            apply.into()
        };
        container(column![head, actions].spacing(6))
            .width(Fill)
            .padding(10)
            .style(dropdown_style)
            .into()
    }

    /// A Run card for a command the chat proposed (the `Command` intent). Shows the exact
    /// command and a Run button that pipes it into the integrated terminal (strict sandbox
    /// applies), plus a Dismiss. Nothing runs until you click — commands execute, so the chat
    /// proposes rather than auto-runs.
    pub(crate) fn view_proposed_command(&self, cmd: &str) -> Element<'_, Message> {
        let head = row![
            text("▶ run command").size(13).color(ACCENT),
            Space::new().width(Fill),
        ]
        .align_y(iced::Alignment::Center);
        let cmd_line = text(cmd.to_string())
            .size(13)
            .font(iced::Font::MONOSPACE)
            .color(FG);
        let run = button(text("▶ Run in terminal").size(13))
            .on_press(Message::RunProposedCommand)
            .padding([5, 12])
            .style(primary_button);
        let dismiss = button(text("Dismiss").size(13).color(FG_MUTED))
            .on_press(Message::DismissProposedCommand)
            .padding([5, 12])
            .style(menu_item_style);
        container(column![head, cmd_line, row![run, dismiss].spacing(8)].spacing(6))
            .width(Fill)
            .padding(10)
            .style(dropdown_style)
            .into()
    }

    /// The composer: the text input + send. When a project (conversation) is open it sends a
    /// CHAT turn; with no project it falls back to the from-scratch build action.
    pub(crate) fn view_composer(&self) -> Element<'_, Message> {
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
            // A chat/plan turn is running → a Cancel button that interrupts it (the streaming
            // backend stops at its next SSE line). Darker-orange idle, brightening on hover.
            button(text("Cancel").size(15).width(Fill).height(Fill).center())
                .on_press(Message::CancelChat)
                .width(Length::Fixed(90.0))
                .height(Fill)
                .padding(0)
                .style(|_t: &Theme, status| {
                    let hov = matches!(status, button::Status::Hovered);
                    button::Style {
                        background: Some(Background::Color(if hov {
                            Color::from_rgb(0.85, 0.47, 0.18)
                        } else {
                            Color::from_rgb(0.72, 0.40, 0.16)
                        })),
                        text_color: Color::from_rgb(0.12, 0.06, 0.02),
                        border: Border {
                            radius: RADIUS.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                })
        } else {
            button(text(label).size(15).width(Fill).height(Fill).center())
                .on_press(send_msg)
                .width(Length::Fixed(90.0))
                .height(Fill)
                .padding(0)
                .style(primary_button)
        };
        // Send button is full composer height, sitting flush against the input.
        let mut bar = row![input, btn].spacing(0);
        // The think/debug toggles stack vertically to the right of the send button. They're kept
        // small (14px box, 11px label, tight gap) so both fit within the one-input-tall composer.
        let mut toggles = column![]
            .spacing(2)
            .padding([0, 8])
            .align_x(iced::Alignment::Start);
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
        // The input bar fills the composer edge-to-edge: no horizontal padding (so it reaches the
        // panel's left/right) and no top divider (so it reaches the top). One flush block.
        bar.align_y(iced::Alignment::Center)
            .height(Length::Fixed(INPUT_H))
            .width(Fill)
            .into()
    }

    pub(crate) fn view_topology(&self) -> Element<'_, Message> {
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

    /// The bottom panel: the prompt sent to the selected coder and the output it
    /// proposed back. Click a coder box on the topology to select one; otherwise it
    /// hints how, or shows the latest verification result.
    pub(crate) fn view_coder_io(&self) -> Element<'_, Message> {
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

    /// The bottom decision card — now ONLY for shell-command confirms. Workflow-phase gate
    /// approval moved into the PLAN master list (Change A): each phase's Approve/Send-back/Abort
    /// buttons sit inline on its row, beside the file you review in CODE. A `Gate` at the front of
    /// the queue therefore renders nothing here (the master list owns it).
    pub(crate) fn view_gatebar(&self) -> Option<Element<'_, Message>> {
        match self.gatebar.first()? {
            // Workflow gate → handled by the master list, not this bottom card.
            Gatebar::Gate { .. } => None,
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
        }
    }

    /// The settings form body (no outer card — the modal wraps it). A tab strip (Connections /
    /// Routing) over a scrollable body, so endpoints+keys are set once and stages are routed
    /// separately. The active tab is [`Self::settings_tab`].
    pub(crate) fn view_settings_body(&self) -> Element<'_, Message> {
        // Tab strip: two toggle buttons, the active one highlighted (reuses the stage-toggle look).
        let tab = |label: &str, which: SettingsTab| {
            let active = self.settings_tab == which;
            let color = if active { FG } else { FG_MUTED };
            button(text(label.to_string()).size(13).color(color))
                .on_press(Message::SettingsTabChanged(which))
                .padding([6, 14])
                .style(if active {
                    primary_button
                } else {
                    stage_toggle_button
                })
        };
        let tabs = row![
            tab("Connections", SettingsTab::Connections),
            tab("Routing", SettingsTab::Routing),
        ]
        .spacing(8);

        let body = match self.settings_tab {
            SettingsTab::Connections => self.view_connections_tab(),
            SettingsTab::Routing => self.view_routing_tab(),
        };

        column![tabs, scrollable(body).height(Length::Fixed(400.0))]
            .spacing(12)
            .into()
    }

    /// The CONNECTIONS tab: the two endpoints (Local + Gemini), each an url + secure key. This is
    /// where the Gemini key lives — on the Gemini connection ONLY, so it never bleeds onto the
    /// local coder endpoint.
    pub(crate) fn view_connections_tab(&self) -> Element<'_, Message> {
        let local_url = text_input(
            "local url (e.g. http://localhost:11435/v1)",
            &self.local_url_input,
        )
        .on_input(Message::LocalUrlChanged)
        .padding(6)
        .style(input_style);
        let local_key = text_input("local api key (usually blank)", &self.local_key_input)
            .on_input(Message::LocalKeyChanged)
            .secure(true)
            .padding(6)
            .style(input_style);
        let gemini_url = text_input("gemini url", &self.gemini_url_input)
            .on_input(Message::GeminiUrlChanged)
            .padding(6)
            .style(input_style);
        let gemini_key = text_input("gemini api key", &self.gemini_key_input)
            .on_input(Message::GeminiKeyChanged)
            .secure(true)
            .padding(6)
            .style(input_style);

        column![
            text("LOCAL  (your llama.cpp / Ollama server)")
                .size(11)
                .color(FG_MUTED),
            local_url,
            local_key,
            text("GEMINI  (Google's OpenAI-compatible endpoint)")
                .size(11)
                .color(FG_MUTED),
            gemini_url,
            gemini_key,
            text("The Gemini key is read from .env (GEMINI_API_KEY) if present.")
                .size(10)
                .color(FG_MUTED),
        ]
        .spacing(8)
        .into()
    }

    /// The ROUTING tab: for each stage, pick which connection it uses + its model. Plus the
    /// verify command and posture toggles (endpoint-agnostic behaviour).
    pub(crate) fn view_routing_tab(&self) -> Element<'_, Message> {
        let model = text_input("coder model (e.g. qwen3-coder-30b)", &self.model_input)
            .on_input(Message::ModelChanged)
            .padding(6)
            .style(input_style);
        let orch_model = text_input(
            "planner model (e.g. gemini-2.5-flash-lite)",
            &self.orch_model_input,
        )
        .on_input(Message::OrchModelChanged)
        .padding(6)
        .style(input_style);
        let advisor = text_input("advisor model (optional)", &self.advisor_input)
            .on_input(Message::AdvisorChanged)
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

        column![
            text("CODER  (does the file writing)")
                .size(11)
                .color(FG_MUTED),
            provider_toggle(self.cfg.coder_provider, Message::CoderProviderChanged),
            model,
            text("PLANNER  (does the breakdown)")
                .size(11)
                .color(FG_MUTED),
            provider_toggle(self.cfg.planner_provider, Message::PlannerProviderChanged),
            orch_model,
            text("ADVISOR  (junior asks senior on a stall)")
                .size(11)
                .color(FG_MUTED),
            provider_toggle(self.cfg.advisor_provider, Message::AdvisorProviderChanged),
            advisor,
            text("VERIFY & BEHAVIOUR").size(11).color(FG_MUTED),
            verify,
            suffix,
            yolo,
            dry,
        ]
        .spacing(8)
        .into()
    }
}
