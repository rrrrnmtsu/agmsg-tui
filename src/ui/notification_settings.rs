use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::notify::NOTIFY_SETTING_LABELS;

use super::main_screen::centered_rect;

/// `Ctrl+N` settings popup: one checkbox row per `NotifySettings` flag, with
/// the highlighted row driven by `App::notify_popup`. No-ops when the popup
/// is closed so callers can invoke it unconditionally every frame, same
/// pattern as `main_screen::render_member_info`.
pub fn render(frame: &mut Frame<'_>, app: &App) {
    let Some(selected) = app.notify_popup else {
        return;
    };
    let area = centered_rect(50, 34, frame.area());
    frame.render_widget(Clear, area);

    let settings = &app.notify_settings;
    let mut lines: Vec<Line<'_>> = NOTIFY_SETTING_LABELS
        .iter()
        .enumerate()
        .map(|(index, label)| {
            let checked = settings.is_enabled(index);
            let mark = if checked { "x" } else { " " };
            let row_style = if index == selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if checked {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(Span::styled(format!("[{mark}] {label}"), row_style))
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter/Space toggle · j/k move · Esc close",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Left).block(
            Block::default()
                .title(" notifications (Ctrl+N) ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        area,
    );
}
