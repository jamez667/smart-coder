//! Turn [`TuiState`] into a ratatui frame (spec 06 — plan panel, tool calls,
//! metrics, honest stop line). Pure view code: it reads state and draws; it never
//! mutates anything, so the state-fold logic stays independently testable.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::state::{LineKind, TuiState};

/// Draw the whole UI for the current `state` into `frame`.
pub fn draw(frame: &mut Frame, state: &TuiState) {
    let area = frame.area();
    // Top: title. Middle: plan | log. Bottom: metrics + status.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(area);

    draw_title(frame, rows[0], state);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(rows[1]);
    draw_plan(frame, cols[0], state);
    draw_log(frame, cols[1], state);

    draw_footer(frame, rows[2], state);
}

fn draw_title(frame: &mut Frame, area: Rect, state: &TuiState) {
    let title = Line::from(vec![
        Span::styled(
            " smart-coder ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            truncate(&state.task, area.width.saturating_sub(14) as usize),
            Style::default().fg(Color::White),
        ),
    ]);
    frame.render_widget(Paragraph::new(title), area);
}

fn draw_plan(frame: &mut Frame, area: Rect, state: &TuiState) {
    let lines: Vec<Line> = if state.plan.is_empty() {
        vec![Line::from(Span::styled(
            "(no plan — running plan-free)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        state
            .plan
            .iter()
            .enumerate()
            .map(|(i, p)| {
                Line::from(vec![
                    Span::styled(
                        format!("{:>2}. ", i + 1),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(p.text.clone()),
                ])
            })
            .collect()
    };
    let block = Block::default().borders(Borders::ALL).title(" plan ");
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_log(frame: &mut Frame, area: Rect, state: &TuiState) {
    // Show the tail that fits the inner height.
    let inner = area.height.saturating_sub(2) as usize;
    let start = state.log.len().saturating_sub(inner);
    let lines: Vec<Line> = state.log[start..]
        .iter()
        .map(|l| Line::from(Span::styled(l.text.clone(), style_for(l.kind))))
        .collect();
    let block = Block::default().borders(Borders::ALL).title(" activity ");
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_footer(frame: &mut Frame, area: Rect, state: &TuiState) {
    let pct = state
        .prompt_tokens
        .checked_mul(100)
        .and_then(|n| n.checked_div(state.prompt_budget))
        .unwrap_or(0)
        .min(999);
    let metrics = Line::from(vec![
        Span::raw(format!("step {}  ", state.step)),
        Span::styled(
            format!(
                "ctx {}/{} ({pct}%)  ",
                state.prompt_tokens, state.prompt_budget
            ),
            Style::default().fg(if pct > 90 { Color::Red } else { Color::Gray }),
        ),
        Span::styled(
            format!("calls {}✓ ", state.valid_calls),
            Style::default().fg(Color::Green),
        ),
        Span::styled(
            format!("{}✗  ", state.invalid_calls),
            Style::default().fg(Color::Red),
        ),
        Span::styled(
            format!("nudges {}", state.interventions),
            Style::default().fg(Color::Magenta),
        ),
    ]);
    let status = Line::from(Span::styled(
        state.status_line(),
        Style::default()
            .fg(status_color(state))
            .add_modifier(Modifier::BOLD),
    ));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" status  (q to quit) ");
    frame.render_widget(Paragraph::new(vec![metrics, status]).block(block), area);
}

fn style_for(kind: LineKind) -> Style {
    let c = match kind {
        LineKind::Info => Color::Gray,
        LineKind::ToolCall => Color::Cyan,
        LineKind::Ok => Color::Green,
        LineKind::Error => Color::Red,
        LineKind::Advice => Color::Magenta,
        LineKind::Stall => Color::Yellow,
        LineKind::Stop => Color::White,
    };
    Style::default().fg(c)
}

fn status_color(state: &TuiState) -> Color {
    use sc_core::StopReason::*;
    match &state.stop {
        None => Color::Cyan,
        Some(Finished) => Color::Green,
        Some(BudgetExhausted) => Color::Yellow,
        Some(Stalled(_)) | Some(Escalated(_)) => Color::Yellow,
        Some(Cancelled) => Color::Yellow,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_and_clips_long() {
        assert_eq!(truncate("hi", 10), "hi");
        assert_eq!(truncate("hello world", 5), "hell…");
    }

    #[test]
    fn every_line_kind_has_a_style() {
        for k in [
            LineKind::Info,
            LineKind::ToolCall,
            LineKind::Ok,
            LineKind::Error,
            LineKind::Advice,
            LineKind::Stall,
            LineKind::Stop,
        ] {
            // Just exercising the mapping — no panic, distinct-ish colors.
            let _ = style_for(k);
        }
    }
}
