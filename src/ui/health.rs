use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table};

use crate::app::App;
use crate::health::{BridgeStatus, HealthSnapshot};
use crate::launchagent::{LaState, LaunchAgentStatus};

use super::NARROW_WIDTH_THRESHOLD;

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let snapshot = app.health_snapshot.as_ref();
    let refreshed_at = snapshot
        .map(|snapshot| snapshot.refreshed_at.as_str())
        .unwrap_or("--:--:--");
    let title = Line::from(vec![
        Span::styled(" HEALTH ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("── window: "),
        window_span(7, app.health_window_days),
        Span::raw(" "),
        window_span(30, app.health_window_days),
        Span::raw(format!(" ── refreshed {refreshed_at} ")),
        if app.health_loading {
            Span::styled("refreshing... ", Style::default().fg(Color::Yellow))
        } else {
            Span::raw("")
        },
    ]);
    let block = Block::default()
        .title(title)
        .title_bottom(" H/Esc:back  j/k:team  t:7d/30d  R:refresh  L:toggle-agent  ?:help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(frame.area());
    frame.render_widget(block, frame.area());

    let Some(snapshot) = snapshot else {
        frame.render_widget(
            Paragraph::new("Health data is loading...").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };

    let team_height = u16::try_from(snapshot.teams.len().saturating_add(1))
        .unwrap_or(u16::MAX)
        .clamp(3, 8);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(team_height),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let narrow = frame.area().width < NARROW_WIDTH_THRESHOLD;
    render_teams(frame, app, snapshot, rows[0], narrow);
    render_separator(frame, rows[1]);
    render_agents(frame, app, rows[2]);
    render_separator(frame, rows[3]);
    render_daily_total(frame, app, snapshot, rows[4]);
    render_separator(frame, rows[5]);
    render_automation(frame, app, rows[6]);
}

/// Phase 14D: single-row "Automation" section for the
/// `com.remma.agmsg-audit-daily` LaunchAgent — label, loaded/unloaded state,
/// and next scheduled run.
fn render_automation(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let text = match app.launchagent.as_ref() {
        None => Line::from(Span::styled(
            "Automation: loading...",
            Style::default().fg(Color::DarkGray),
        )),
        Some(status) => automation_line(status),
    };
    frame.render_widget(Paragraph::new(text), area);
}

fn automation_line(status: &LaunchAgentStatus) -> Line<'static> {
    match &status.state {
        LaState::Loaded { .. } => Line::from(vec![
            Span::styled("● ", Style::default().fg(Color::Green)),
            Span::raw(format!("{}  loaded  next: {}", status.label, status.next_run_label())),
        ]),
        LaState::Unloaded => Line::from(vec![
            Span::styled("○ ", Style::default().fg(Color::Yellow)),
            Span::raw(format!("{}  unloaded  [L] to load", status.label)),
        ]),
        LaState::Missing => Line::from(vec![
            Span::styled("– ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}  plist missing", status.label),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        LaState::Unsupported => Line::from(Span::styled(
            "– LaunchAgent control: unsupported on this OS",
            Style::default().fg(Color::DarkGray),
        )),
    }
}

fn window_span(days: u32, selected: u32) -> Span<'static> {
    let label = format!("{days}d");
    if days == selected {
        Span::styled(
            format!("[{label}]"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw(label)
    }
}

fn render_teams(
    frame: &mut Frame<'_>,
    app: &App,
    snapshot: &HealthSnapshot,
    area: Rect,
    narrow: bool,
) {
    let visible_rows = usize::from(area.height.saturating_sub(1));
    let start = app
        .health_team_index
        .saturating_sub(visible_rows.saturating_sub(1));
    let teams = snapshot.teams.iter().skip(start).take(visible_rows);
    let rows = teams
        .clone()
        .enumerate()
        .map(|(offset, team)| {
            let index = start + offset;
            let marker = if index == app.health_team_index {
                "▸ "
            } else {
                "  "
            };
            let name = if team.orphan {
                format!("{marker}(orphan) {}", team.name)
            } else {
                format!("{marker}{}", team.name)
            };
            let unread = format!(
                "{}{}",
                team.unread,
                if team.stale_unread { " !" } else { "" }
            );
            let mut cells = vec![
                Cell::from(name),
                Cell::from(team.delivery.label()),
                Cell::from(bridge_summary(&team.bridges)).style(bridge_style(&team.bridges)),
            ];
            if narrow {
                cells.push(Cell::from(unread));
            } else {
                cells.push(Cell::from(format_age(team.last_msg_age)));
                cells.push(Cell::from(unread));
                cells.push(Cell::from(""));
            }
            Row::new(cells).style(if index == app.health_team_index {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            })
        })
        .collect::<Vec<_>>();
    let header = if narrow {
        Row::new(["TEAM", "MODE", "BRIDGE", "UNREAD"])
    } else {
        Row::new(["TEAM", "MODE", "BRIDGE", "LAST MSG", "UNREAD", "TRAFFIC"])
    }
    .style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    let widths = if narrow {
        vec![
            Constraint::Min(12),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Length(6),
        ]
    } else {
        vec![
            Constraint::Length(17),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Min(12),
        ]
    };
    frame.render_widget(
        Table::new(rows, widths).header(header).column_spacing(1),
        area,
    );

    if narrow {
        return;
    }
    let traffic_x = area.x.saturating_add(60);
    let traffic_width = area.right().saturating_sub(traffic_x);
    if traffic_width <= 5 {
        return;
    }
    for (offset, team) in snapshot
        .teams
        .iter()
        .skip(start)
        .take(visible_rows)
        .enumerate()
    {
        let values = team
            .traffic(app.health_window_days)
            .iter()
            .map(|day| u64::try_from(day.count).unwrap_or(u64::MAX))
            .collect::<Vec<_>>();
        let spark_width = traffic_width.saturating_sub(5);
        let values = tail_values(&values, usize::from(spark_width));
        let max = values.iter().copied().max().unwrap_or(1).max(1);
        let spark_area = Rect::new(
            traffic_x,
            area.y.saturating_add(1 + offset as u16),
            spark_width,
            1,
        );
        let color = if team
            .traffic(app.health_window_days)
            .iter()
            .any(|day| day.burst)
        {
            Color::Yellow
        } else {
            Color::Cyan
        };
        frame.render_widget(
            Sparkline::default().data(&values).max(max).style(color),
            spark_area,
        );
        let total = team
            .traffic(app.health_window_days)
            .iter()
            .map(|day| day.count)
            .sum::<usize>();
        frame.render_widget(
            Paragraph::new(format!("{total:>4}")),
            Rect::new(traffic_x + spark_width, spark_area.y, 5, 1),
        );
    }
}

fn render_agents(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(team) = app.current_health_team() else {
        frame.render_widget(Paragraph::new("No team selected"), area);
        return;
    };
    let mut traffic = team.agent_traffic(app.health_window_days).to_vec();
    for identity in &team.silent_identities {
        if !traffic.iter().any(|row| row.agent == identity.agent) {
            traffic.push(crate::db::AgentTraffic {
                agent: identity.agent.clone(),
                sent: 0,
                received: 0,
            });
        }
    }
    traffic.sort_by_key(|row| {
        !team
            .silent_identities
            .iter()
            .any(|identity| identity.agent == row.agent)
    });
    let max_total = traffic
        .iter()
        .map(|row| row.sent + row.received)
        .max()
        .unwrap_or(1)
        .max(1);
    let visible_rows = usize::from(area.height.saturating_sub(1));
    if area.width < NARROW_WIDTH_THRESHOLD {
        let rows = traffic.iter().take(visible_rows).map(|row| {
            let silent = team
                .silent_identities
                .iter()
                .any(|identity| identity.agent == row.agent);
            let label = if silent {
                format!(
                    "{} ⚠ silent {} days",
                    row.agent,
                    app.health_snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.silent_days)
                        .unwrap_or_default()
                )
            } else {
                format!("{}  s{}/r{}", row.agent, row.sent, row.received)
            };
            Row::new([Cell::from(label).style(if silent {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            })])
        });
        frame.render_widget(
            Table::new(rows, [Constraint::Min(1)]).header(
                Row::new([format!(
                    "{} agents ({}d)",
                    team.name, app.health_window_days
                )])
                .style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ),
            area,
        );
        return;
    }
    let rows = traffic.iter().take(visible_rows).map(|row| {
        let total = row.sent + row.received;
        let bar_len = if total == 0 {
            0
        } else {
            (total * 12).div_ceil(max_total).max(1)
        };
        let silent = team
            .silent_identities
            .iter()
            .any(|identity| identity.agent == row.agent);
        Row::new(vec![
            Cell::from(format!("  {}", row.agent)),
            Cell::from(format!("{:>5}", row.sent)),
            Cell::from(format!("{:>5}", row.received)),
            if silent {
                Cell::from(format!(
                    "⚠ silent {} days",
                    app.health_snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.silent_days)
                        .unwrap_or_default()
                ))
                .style(Style::default().fg(Color::Yellow))
            } else {
                Cell::from("▇".repeat(bar_len))
            },
        ])
    });
    let header = Row::new(vec![
        Cell::from(format!(
            "{} agents ({}d)",
            team.name, app.health_window_days
        )),
        Cell::from("sent"),
        Cell::from("recv"),
        Cell::from(""),
    ])
    .style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(28),
                Constraint::Length(6),
                Constraint::Length(6),
                Constraint::Min(1),
            ],
        )
        .header(header)
        .column_spacing(1),
        area,
    );
}

fn render_daily_total(frame: &mut Frame<'_>, app: &App, snapshot: &HealthSnapshot, area: Rect) {
    let traffic = snapshot.daily_total(app.health_window_days);
    let values = traffic
        .iter()
        .map(|day| u64::try_from(day.count).unwrap_or(u64::MAX))
        .collect::<Vec<_>>();
    let max = values.iter().copied().max().unwrap_or(1).max(1);
    let burst_days = traffic
        .iter()
        .filter(|day| day.burst)
        .map(|day| day.date.format("%m-%d").to_string())
        .collect::<Vec<_>>();
    let burst = if burst_days.is_empty() {
        "none".to_owned()
    } else {
        burst_days.join(",")
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);
    frame.render_widget(
        Paragraph::new(format!(
            "daily total ({}d)   max {max}   burst>{}: {burst}",
            app.health_window_days, snapshot.burst_threshold
        )),
        rows[0],
    );
    let style = if burst_days.is_empty() {
        Color::Cyan
    } else {
        Color::Yellow
    };
    frame.render_widget(
        Sparkline::default().data(&values).max(max).style(style),
        rows[1],
    );
}

fn render_separator(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(area.width)))
            .style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn tail_values(values: &[u64], width: usize) -> Vec<u64> {
    values[values.len().saturating_sub(width)..].to_vec()
}

fn bridge_marker(bridges: &[BridgeStatus]) -> &'static str {
    if bridges.is_empty() {
        return "-";
    }
    let alive = bridges.iter().filter(|bridge| bridge.alive).count();
    if alive == bridges.len() {
        "●"
    } else if alive == 0 {
        "○"
    } else {
        "◐"
    }
}

fn bridge_summary(bridges: &[BridgeStatus]) -> String {
    if bridges.is_empty() {
        return "-".to_owned();
    }
    let alive = bridges.iter().filter(|bridge| bridge.alive).count();
    format!("{} {alive}/{} up", bridge_marker(bridges), bridges.len())
}

fn bridge_style(bridges: &[BridgeStatus]) -> Style {
    let color = match bridge_marker(bridges) {
        "●" => Color::Green,
        "◐" => Color::Yellow,
        "○" => Color::Red,
        _ => Color::DarkGray,
    };
    Style::default().fg(color)
}

fn format_age(age: Option<Duration>) -> String {
    let Some(age) = age else {
        return "-".to_owned();
    };
    let seconds = age.as_secs();
    if seconds < 60 {
        "now".to_owned()
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::{automation_line, bridge_marker};
    use crate::health::BridgeStatus;
    use crate::launchagent::{LaState, LaunchAgentStatus};

    fn bridge(pid: u32, alive: bool) -> BridgeStatus {
        BridgeStatus {
            label: "fixture".to_owned(),
            pid,
            alive,
        }
    }

    #[test]
    fn bridge_marker_maps_all_partial_down_and_empty_states() {
        assert_eq!(bridge_marker(&[]), "-");
        assert_eq!(bridge_marker(&[bridge(1, true), bridge(2, true)]), "●");
        assert_eq!(bridge_marker(&[bridge(1, true), bridge(2, false)]), "◐");
        assert_eq!(bridge_marker(&[bridge(1, false), bridge(2, false)]), "○");
    }

    fn status(state: LaState) -> LaunchAgentStatus {
        LaunchAgentStatus {
            label: "com.remma.agmsg-audit-daily".to_owned(),
            plist: None,
            state,
            next_run: None,
        }
    }

    /// Phase 14D — row text for the Automation section's 4 states (stands
    /// in for a full TestBackend snapshot, which lives in `ui/mod.rs`,
    /// outside this phase's file scope).
    #[test]
    fn automation_line_covers_loaded_unloaded_missing_unsupported_variants() {
        let loaded = automation_line(&status(LaState::Loaded { pid: Some(123) }))
            .to_string();
        assert!(loaded.contains("loaded"));
        assert!(loaded.contains("com.remma.agmsg-audit-daily"));

        let unloaded = automation_line(&status(LaState::Unloaded)).to_string();
        assert!(unloaded.contains("unloaded"));
        assert!(unloaded.contains("[L] to load"));

        let missing = automation_line(&status(LaState::Missing)).to_string();
        assert!(missing.contains("plist missing"));

        let unsupported = automation_line(&status(LaState::Unsupported)).to_string();
        assert!(unsupported.contains("unsupported on this OS"));
    }
}
