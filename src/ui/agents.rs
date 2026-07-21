use std::collections::HashSet;

use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::agents::{
    AgentFocus, AgentModal, CLI_TYPES, SpawnStep, validate_agent_name, validate_team_name,
};
use crate::app::App;
use crate::timefmt::format_timestamp;

use super::main_screen::centered_rect;

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let restart = app
        .agent_restart_needed
        .as_deref()
        .map(|name| format!(" | restart-needed:{name}"))
        .unwrap_or_default();
    let outer = Block::default()
        .title(format!(" AGENTS{restart} "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = outer.inner(frame.area());
    frame.render_widget(outer, frame.area());
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25), Constraint::Percentage(75)])
        .split(rows[0]);
    render_teams(frame, app, columns[0]);
    render_identities(frame, app, columns[1]);

    let status_text = if app.poll_offline && !app.status.text.starts_with("poll offline") {
        format!("poll offline | {}", app.status.text)
    } else {
        app.status.text.clone()
    };
    let status_style = if app.poll_offline || app.status.is_error {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if app.status.text.starts_with('⚠') {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(status_text).style(status_style),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(
            "n:new t/Tab:focus R:rename T:team X:reset D:despawn L:leave Enter:info r:reload ?:help A:back",
        )
        .style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );

    render_modal(frame, app);
    render_identity_info(frame, app);
}

/// L-3: `Enter` on an Identities-focus row. Same popup shape as
/// `main_screen::render_member_info` — this screen just never had an
/// equivalent before.
fn render_identity_info(frame: &mut Frame<'_>, app: &App) {
    let Some(info) = app.agent_identity_info.as_deref() else {
        return;
    };
    let area = centered_rect(56, 46, frame.area());
    frame.render_widget(Clear, area);
    let lines: Vec<Line<'_>> = info.split('\n').map(Line::from).collect();
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" identity info (Esc/Enter to close) ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_teams(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let items = app
        .agent_teams
        .iter()
        .map(|team| {
            let suffix = if team.broken_config.is_some() {
                " ⚠".to_owned()
            } else {
                format!(" ({})", unique_agent_count(team))
            };
            ListItem::new(format!("{}{}", team.name, suffix))
        })
        .collect::<Vec<_>>();
    let selected = (!app.agent_teams.is_empty()).then_some(app.agent_team_index);
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(
        List::new(items)
            .block(focused_block(
                " TEAMS ",
                app.agent_focus == AgentFocus::Teams,
            ))
            .highlight_symbol("> ")
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        area,
        &mut state,
    );
}

fn render_identities(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if app.agent_teams.is_empty() {
        frame.render_widget(
            Paragraph::new("No teams. Press n to create one with an agent.").block(focused_block(
                " IDENTITIES ",
                app.agent_focus == AgentFocus::Identities,
            )),
            area,
        );
        return;
    }

    let inner_width = area.width.saturating_sub(2) as usize;
    let project_width = inner_width.saturating_sub(48).clamp(6, 9);
    let now = Utc::now();
    let mut selected = None;
    let mut items = Vec::new();
    for (team_index, team) in app.agent_teams.iter().enumerate() {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("# {} ({} agents)", team.name, unique_agent_count(team)),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))));
        if let Some(error) = team.broken_config.as_deref() {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("  ⚠ broken config: {}", compact(error, 48)),
                Style::default().fg(Color::Yellow),
            ))));
            continue;
        }
        if team.identities.is_empty() {
            items.push(ListItem::new("  (no identities)"));
            continue;
        }
        for (identity_index, identity) in team.identities.iter().enumerate() {
            if team_index == app.agent_team_index && identity_index == app.agent_identity_index {
                selected = Some(items.len());
            }
            let own = if app.current_identity.as_deref() == Some(&identity.name) {
                "▏"
            } else {
                " "
            };
            let name = if own == "▏" {
                format!("{} (me)", identity.name)
            } else {
                identity.name.clone()
            };
            let seen = identity
                .last_seen_at
                .as_deref()
                .map(|timestamp| format_timestamp(timestamp, now))
                .unwrap_or_else(|| "never".to_owned());
            items.push(ListItem::new(format!(
                "{own} {:<15} {:<11} {:<project_width$} {:<6} s{:>3}/r{:<3}",
                compact(&name, 15),
                compact(&identity.agent_type, 11),
                compact_project(&identity.project, project_width),
                compact(&seen, 6),
                identity.sent_30d,
                identity.received_30d,
            )));
        }
    }
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(
        List::new(items)
            .block(focused_block(
                " NAME          CLI-TYPE    PROJECT    LAST    S/R ",
                app.agent_focus == AgentFocus::Identities,
            ))
            .highlight_symbol(">")
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

fn render_modal(frame: &mut Frame<'_>, app: &App) {
    let Some(modal) = app.agent_modal.as_ref() else {
        return;
    };
    let area = centered_rect(86, 72, frame.area());
    frame.render_widget(Clear, area);
    let (title, lines) = match modal {
        AgentModal::Spawn(state) => {
            let lines = match state.step {
                SpawnStep::Team => {
                    let mut lines = vec![Line::from("Select team (j/k + Enter):")];
                    for (index, team) in app.agent_teams.iter().enumerate() {
                        lines.push(Line::from(format!(
                            "{} {}",
                            if index == state.team_index { ">" } else { " " },
                            team.name
                        )));
                    }
                    lines.push(Line::from(format!(
                        "{} <new team…>",
                        if state.team_index == app.agent_teams.len() {
                            ">"
                        } else {
                            " "
                        }
                    )));
                    lines
                }
                SpawnStep::NewTeam => vec![
                    Line::from(format!("team : {}█", state.team_input)),
                    validation_line(validate_team_name(&state.team_input)),
                    Line::from("hint: repo-slug exact match (example: ops-hub)"),
                    Line::from("Enter: next  Esc: back"),
                ],
                SpawnStep::CliType => vec![
                    Line::from(format!("type : {}", state.agent_type())),
                    Line::from(format!("choices: {}", CLI_TYPES.join(" / "))),
                    Line::from("←/→ or j/k: change  Enter: next  Esc: back"),
                ],
                SpawnStep::Name => vec![
                    Line::from(format!("team : {}", modal_team_name(app, state))),
                    Line::from(format!("type : {}", state.agent_type())),
                    Line::from(format!("name : {}█", state.name)),
                    validation_line(validate_agent_name(&state.name, state.agent_type())),
                    Line::from("hint: <cli-type>[-role], numbered roles forbidden"),
                    Line::from("Enter: run  Esc: back"),
                ],
            };
            (" new agent ", lines)
        }
        AgentModal::Rename {
            target,
            input,
            confirming,
            self_rename,
        } => {
            let mut lines = vec![
                Line::from(format!("team : {}", target.team)),
                Line::from(format!("old  : {}", target.name)),
                Line::from(format!("new  : {}█", input)),
                validation_line(validate_agent_name(input, &target.agent_type)),
            ];
            if *self_rename {
                lines.push(Line::from(Span::styled(
                    "⚠ current identity: bridge restart is required after rename",
                    Style::default().fg(Color::Yellow),
                )));
                lines.push(Line::from("watch subscriptions are fixed at startup"));
            }
            lines.push(Line::from(if *confirming {
                "rename now? y/n"
            } else {
                "Enter: confirm  Esc: cancel"
            }));
            (" rename agent ", lines)
        }
        AgentModal::RenameTeam {
            old,
            input,
            confirming,
        } => (
            " rename team ",
            vec![
                Line::from(format!("old : {old}")),
                Line::from(format!("new : {input}█")),
                validation_line(validate_team_name(input)),
                Line::from(Span::styled(
                    "⚠ repo-slug exact match; abbreviations/UUIDs forbidden",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(Span::styled(
                    "⚠ members in other projects must rerun whoami",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(if *confirming {
                    "rename now? y/n"
                } else {
                    "Enter: confirm  Esc: cancel"
                }),
            ],
        ),
        AgentModal::Reset {
            target,
            confirm,
            blocked,
        } => {
            let mut lines = vec![
                Line::from(format!("target : {} ({})", target.name, target.agent_type)),
                Line::from(format!("project: {}", compact(&target.project, 58))),
            ];
            if *blocked {
                lines.push(Line::from(Span::styled(
                    "✗ self-reset is refused; use the session-side drop command",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from("Esc: close"));
            } else {
                lines.push(Line::from(Span::styled(
                    "⚠ irreversible: remove this project/type registration",
                    Style::default().fg(Color::Yellow),
                )));
                lines.push(Line::from(format!("confirm : {confirm}█")));
                lines.push(Line::from("Type YES exactly, then Enter  Esc: cancel"));
            }
            (" reset agent ", lines)
        }
        AgentModal::Leave {
            team,
            agent,
            confirm,
        } => (
            " leave team ",
            vec![
                Line::from(format!("team   : {team}")),
                Line::from(format!("agent  : {agent}")),
                Line::from(Span::styled(
                    "⚠ removes every registration for this agent in the team",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(format!("confirm : {confirm}█")),
                Line::from("Type YES exactly, then Enter  Esc: cancel"),
            ],
        ),
        AgentModal::JoinForce {
            team,
            agent,
            agent_type,
            project,
        } => (
            " force join? ",
            vec![
                Line::from(format!("team   : {team}")),
                Line::from(format!("agent  : {agent}")),
                Line::from(format!("type   : {agent_type}")),
                Line::from(format!("project: {}", compact(project, 58))),
                Line::from(Span::styled(
                    "⚠ this name has a rename tombstone",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from("Retry join.sh with --force? y/n"),
            ],
        ),
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn modal_team_name(app: &App, state: &crate::agents::SpawnModalState) -> String {
    if state.new_team {
        state.team_input.clone()
    } else {
        app.agent_teams
            .get(state.team_index)
            .map(|team| team.name.clone())
            .unwrap_or_else(|| "-".to_owned())
    }
}

fn validation_line(result: Result<(), String>) -> Line<'static> {
    match result {
        Ok(()) => Line::from(Span::styled("✓ valid", Style::default().fg(Color::Green))),
        Err(error) => Line::from(Span::styled(
            format!("✗ {error}"),
            Style::default().fg(Color::Red),
        )),
    }
}

fn focused_block<'a>(title: &'a str, focused: bool) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
}

fn compact_project(project: &str, width: usize) -> String {
    if project.chars().count() <= width {
        return project.to_owned();
    }
    let basename = project
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(project);
    compact(&format!("…/{basename}"), width)
}

fn compact(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    if width <= 1 {
        return "…".to_owned();
    }
    let mut output = value.chars().take(width - 1).collect::<String>();
    output.push('…');
    output
}

fn unique_agent_count(team: &crate::db::AgentTeamSummary) -> usize {
    team.identities
        .iter()
        .map(|identity| identity.name.as_str())
        .collect::<HashSet<_>>()
        .len()
}
