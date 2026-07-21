use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Gauge, List, ListItem, ListState, Paragraph, Row, Table, Tabs,
    Wrap,
};

use crate::app::{App, AuditTab};
use crate::audit::AXIS_ORDER;
use crate::audit_history::detect_chatter_trend;
use crate::db::PairMatrix;
use crate::palette::{axis_score_color, overall_score_color};

use super::main_screen::centered_rect;

pub fn render(frame: &mut Frame<'_>, app: &App) {
    if app.audit_tab == AuditTab::History {
        super::audit_history::render(frame, app);
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(7),
            Constraint::Min(6),
            Constraint::Length(3),
        ])
        .split(frame.area());
    render_header(frame, app, rows[0]);
    render_axes(frame, app, rows[1]);
    let lower = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(rows[2]);
    render_pair_matrix(frame, app, lower[0]);
    render_items(frame, app, lower[1]);
    render_footer(frame, app, rows[3]);
    if let Some(detail) = &app.audit_detail {
        render_detail(frame, detail);
    }
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let title = app.audit_report.as_ref().map_or_else(
        || " agmsg audit | [D] DASHBOARD  H:HISTORY ".to_owned(),
        |report| {
            format!(
                " agmsg audit | [D] DASHBOARD  H:HISTORY | last {} | {}d / {} msgs / {} unread ",
                compact_timestamp(&report.ts),
                report.window_days,
                report.total_msg,
                report.unread
            )
        },
    );
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if let Some(report) = &app.audit_report {
        let color = overall_score_color(report.score, crate::palette::current().mode);
        frame.render_widget(
            Paragraph::new(large_score_lines(report.score, color, app.audit_loading))
                .alignment(Alignment::Center),
            inner,
        );
    } else {
        frame.render_widget(
            Paragraph::new("loading audit...")
                .style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Center),
            inner,
        );
    }
}

fn render_axes(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = Block::default().title(" 10 AXES ").borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);
    render_axis_column(frame, app, columns[0], &AXIS_ORDER[..5]);
    render_axis_column(frame, app, columns[1], &AXIS_ORDER[5..]);
}

fn render_axis_column(frame: &mut Frame<'_>, app: &App, area: Rect, names: &[&str]) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(names.iter().map(|_| Constraint::Length(1)))
        .split(area);
    for (row, name) in rows.iter().zip(names) {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(14),
                Constraint::Length(9),
                Constraint::Length(5),
                Constraint::Min(1),
            ])
            .split(*row);
        frame.render_widget(Paragraph::new(axis_label(name)), columns[0]);
        if let Some(axis) = app
            .audit_report
            .as_ref()
            .and_then(|report| report.axes.get(*name))
        {
            let color = axis_score_color(axis.score, crate::palette::current().mode);
            frame.render_widget(
                Gauge::default()
                    .ratio(f64::from(axis.score.min(10)) / 10.0)
                    .label("")
                    .gauge_style(Style::default().fg(color)),
                columns[1],
            );
            frame.render_widget(Paragraph::new(format!("{:>2}/10", axis.score)), columns[2]);
            frame.render_widget(Paragraph::new(axis.note.as_str()), columns[3]);
        }
    }
}

fn render_pair_matrix(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(matrix) = app.current_pair_matrix() else {
        frame.render_widget(
            Paragraph::new(format!(
                "No pair traffic in {} days",
                app.audit_pair_window_days
            ))
            .block(
                Block::default()
                    .title(" PAIR MATRIX ")
                    .borders(Borders::ALL),
            ),
            area,
        );
        return;
    };
    let block = Block::default()
        .title(format!(
            " PAIR MATRIX {}d [h/l] [t:window] ",
            app.audit_pair_window_days
        ))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let matrix_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(2)])
        .split(inner);
    let tabs = Tabs::new(
        app.pair_matrices
            .iter()
            .map(|item| Line::from(item.team.clone()))
            .collect::<Vec<_>>(),
    )
    .select(app.audit_team_index)
    .highlight_style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
    .divider("|");
    frame.render_widget(tabs, matrix_rows[0]);

    let table_area = matrix_rows[1];
    let column_count = usize::from(table_area.width.saturating_sub(12) / 6).max(1);
    let agents: Vec<&String> = matrix.agents.iter().take(column_count).collect();
    let mut widths = vec![Constraint::Length(9)];
    widths.extend(agents.iter().map(|_| Constraint::Length(5)));
    let header = Row::new(
        std::iter::once(Cell::from("from\\to"))
            .chain(agents.iter().map(|agent| Cell::from(short_name(agent))))
            .collect::<Vec<_>>(),
    )
    .style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    let visible_rows = usize::from(table_area.height.saturating_sub(1));
    let rows = matrix
        .agents
        .iter()
        .take(visible_rows)
        .map(|from| {
            let mut cells = vec![Cell::from(short_name(from))];
            cells.extend(agents.iter().map(|to| {
                if from == *to {
                    Cell::from("  -  ").style(Style::default().fg(Color::DarkGray))
                } else {
                    let count = matrix.count(from, to);
                    Cell::from(format!("{count:>5}")).style(pair_style(matrix, from, to, count))
                }
            }));
            Row::new(cells)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Table::new(rows, widths).header(header), table_area);
}

fn pair_style(matrix: &PairMatrix, from: &str, to: &str, count: usize) -> Style {
    if matrix.is_asymmetric(from, to) {
        return Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD);
    }
    let color = match count {
        0 => Color::DarkGray,
        1..=9 => Color::Blue,
        10..=49 => Color::Cyan,
        _ => Color::Yellow,
    };
    Style::default().fg(color)
}

fn render_items(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut items = Vec::with_capacity(app.audit_item_count());
    for zombie in &app.zombies {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("Z ", Style::default().fg(Color::Red)),
            Span::raw(format!("{}/{}", zombie.team, zombie.agent)),
        ])));
    }
    for stale in &app.stale_unreads {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("S ", Style::default().fg(Color::Yellow)),
            Span::raw(format!(
                "{} {}<-{} #{}",
                stale.team, stale.to_agent, stale.from_agent, stale.id
            )),
        ])));
    }
    if items.is_empty() {
        items.push(ListItem::new("No zombie or stale unread items"));
    }
    let title = format!(
        " ACTIONS Z:{} S:{} [D/M/Enter] ",
        app.zombies.len(),
        app.stale_unreads.len()
    );
    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL))
        .highlight_symbol("> ")
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = ListState::default();
    if app.audit_item_count() > 0 {
        state.select(Some(app.audit_selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut status_text = if app.poll_offline && !app.status.text.starts_with("poll offline") {
        format!("poll offline | {}", app.status.text)
    } else {
        app.status.text.clone()
    };
    let chatter_trend = detect_chatter_trend(&app.audit_history);
    if let Some(trend) = chatter_trend {
        status_text = format!(
            "⚠ chatter loop trend: {}→{} pairs (7d) | {status_text}",
            trend.start, trend.end
        );
    }
    let style = if chatter_trend.is_some() || app.poll_offline || app.status.is_error {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    };
    if app.status.text.starts_with("AGMSG_RESOLVE_PROJECT=") {
        frame.render_widget(
            Paragraph::new(status_text)
                .style(style)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(status_text, style),
            Line::raw(
                "Ctrl-A/Tab:main D:dashboard/cmd H:history t:7/30/90d R:refresh j/k:item B/W/M/E",
            ),
        ]),
        area,
    );
}

fn render_detail(frame: &mut Frame<'_>, detail: &str) {
    let area = centered_rect(78, 60, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(detail)
            .block(
                Block::default()
                    .title(" detail [Enter/Esc] ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn axis_label(name: &str) -> String {
    name.replace("_control", "").replace("_prevention", "")
}

fn compact_timestamp(timestamp: &str) -> &str {
    timestamp.get(..16).unwrap_or(timestamp)
}

fn short_name(name: &str) -> String {
    name.chars().take(5).collect()
}

fn large_score_lines(score: u16, color: Color, loading: bool) -> Vec<Line<'static>> {
    const DIGITS: [[&str; 3]; 10] = [
        ["███", "█ █", "███"],
        [" ██", "  █", "  █"],
        ["███", " ██", "███"],
        ["███", " ██", "███"],
        ["█ █", "███", "  █"],
        ["███", "██ ", "███"],
        ["█  ", "███", "███"],
        ["███", "  █", "  █"],
        ["███", "███", "███"],
        ["███", "███", "  █"],
    ];
    let digits = score.min(100).to_string();
    let mut rendered = [String::new(), String::new(), String::new()];
    for digit in digits.bytes() {
        let glyph = &DIGITS[usize::from(digit - b'0')];
        for (line, part) in rendered.iter_mut().zip(glyph) {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(part);
        }
    }
    rendered[1].push_str("  /100");
    if loading {
        rendered[2].push_str("  refreshing...");
    }
    rendered
        .into_iter()
        .map(|line| {
            Line::styled(
                line,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}
