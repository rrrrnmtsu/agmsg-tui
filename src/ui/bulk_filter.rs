use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::app::App;
use crate::bulk::BulkFilterFocus;

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let outer = Block::default()
        .title(" BULK FILTER — all teams ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = outer.inner(frame.area());
    frame.render_widget(outer, frame.area());
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    render_fields(frame, app, rows[0]);
    render_results(frame, app, rows[1]);
    let status_style = if app.status.is_error {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(app.status.text.clone()).style(status_style),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(
            "Ctrl-F/Esc:back Tab:field ←/→:period j/k:result M:mark-read E:export ?:help",
        )
        .style(Style::default().fg(Color::DarkGray)),
        rows[3],
    );
}

fn render_fields(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(filter) = app.bulk_filter.as_ref() else {
        frame.render_widget(Paragraph::new("loading messages..."), area);
        return;
    };
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(if area.width < 60 {
            [
                Constraint::Percentage(34),
                Constraint::Percentage(25),
                Constraint::Percentage(41),
            ]
        } else {
            [
                Constraint::Percentage(30),
                Constraint::Percentage(20),
                Constraint::Percentage(50),
            ]
        })
        .split(area);
    render_field(
        frame,
        columns[0],
        " agent (from/to) ",
        &filter.agent,
        filter.focus == BulkFilterFocus::Agent,
    );
    render_field(
        frame,
        columns[1],
        " period ",
        &format!("[{}] 7d/30d/all", filter.period.label()),
        filter.focus == BulkFilterFocus::Period,
    );
    render_field(
        frame,
        columns[2],
        " body contains ",
        &filter.body,
        filter.focus == BulkFilterFocus::Body,
    );
}

fn render_field(frame: &mut Frame<'_>, area: Rect, title: &str, value: &str, focused: bool) {
    frame.render_widget(
        Paragraph::new(if value.is_empty() { " " } else { value }).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if focused {
                    Color::Cyan
                } else {
                    Color::DarkGray
                })),
        ),
        area,
    );
}

fn render_results(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(filter) = app.bulk_filter.as_ref() else {
        return;
    };
    let wide = area.width >= 70;
    let items = filter
        .messages()
        .map(|message| {
            let unread = if message.read_at.is_none() { "!" } else { " " };
            let body = single_line(&message.body, if wide { 30 } else { 14 });
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{unread}#{:<5} ", message.id),
                    Style::default().fg(if message.read_at.is_none() {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    }),
                ),
                Span::raw(if wide {
                    format!(
                        "{:<14} {:<13} -> {:<13} {}  {}",
                        compact(&message.team, 14),
                        compact(&message.from_agent, 13),
                        compact(&message.to_agent, 13),
                        compact(&message.created_at, 10),
                        body
                    )
                } else {
                    format!(
                        "{:<10} {}>{} {}",
                        compact(&message.team, 10),
                        compact(&message.from_agent, 8),
                        compact(&message.to_agent, 8),
                        body
                    )
                }),
            ]))
        })
        .collect::<Vec<_>>();
    let selected = (!items.is_empty()).then_some(filter.selected);
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(format!(" RESULTS ({}) — ! unread ", filter.results.len()))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(
                        if filter.focus == BulkFilterFocus::Results {
                            Color::Cyan
                        } else {
                            Color::DarkGray
                        },
                    )),
            )
            .highlight_symbol("> ")
            .highlight_style(
                Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
        area,
        &mut state,
    );
}

fn compact(value: &str, width: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(width).collect::<String>();
    if chars.next().is_some() && width > 1 {
        format!("{}…", prefix.chars().take(width - 1).collect::<String>())
    } else {
        prefix
    }
}

fn single_line(value: &str, width: usize) -> String {
    compact(&value.replace(['\n', '\r', '\t'], " "), width)
}
