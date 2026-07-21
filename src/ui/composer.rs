use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{App, BODY_BLOCK_BYTES, BODY_WARN_BYTES, BodySizeLevel};

use super::main_screen::centered_rect;

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let Some(composer) = app.composer.as_ref() else {
        return;
    };
    let area = centered_rect(74, 72, frame.area());
    frame.render_widget(Clear, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .margin(1)
        .split(area);
    frame.render_widget(
        Block::default()
            .title(" compose ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
        area,
    );
    frame.render_widget(
        Paragraph::new(format!(
            "team : {}",
            app.selected_team_name().unwrap_or("-")
        )),
        rows[0],
    );
    // M-3: bold the from-name so it reads at a glance instead of blending
    // into "team : / to : ", since a misdirected send here is the one typo
    // in this modal that actually leaves the room (unlike a wrong `to`,
    // which just reaches the wrong person within the same team).
    let from_name = composer.from_agent().unwrap_or("-");
    let is_current_identity = app.current_identity.as_deref() == Some(from_name);
    let from_note = if app.current_identity.is_none() {
        " (AGMSG_IDENTITY not set)"
    } else if !is_current_identity {
        " (not your identity)"
    } else {
        ""
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("from : "),
            Span::styled(
                from_name.to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  (Tab){from_note}")),
        ])),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(format!(
            "to   : {}  (Shift-Tab)",
            composer.to_agent().unwrap_or("-")
        )),
        rows[2],
    );
    let mut body = composer.body.clone();
    let byte_index = body
        .char_indices()
        .nth(composer.cursor)
        .map(|(index, _)| index)
        .unwrap_or(body.len());
    body.insert(byte_index, '█');
    frame.render_widget(
        Paragraph::new(body)
            .block(Block::default().title(" body ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        rows[3],
    );
    let (size_color, hint) = match composer.body_size_level() {
        BodySizeLevel::Normal => (Color::Green, "Ctrl-S: send".to_owned()),
        BodySizeLevel::Warning => (Color::Yellow, "warn: share a file path".to_owned()),
        BodySizeLevel::Blocked => (
            Color::Red,
            format!("BLOCK >{BODY_BLOCK_BYTES}B: share a file path"),
        ),
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("[{}/{}B]", composer.body_bytes(), BODY_WARN_BYTES),
                Style::default().fg(size_color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  {hint}  Esc: save draft  Ctrl-K: clear")),
        ])),
        rows[4],
    );
}
