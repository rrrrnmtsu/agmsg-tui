use chrono::{Duration as ChronoDuration, Utc};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Focus, SPLIT_WIDE_THRESHOLD, fold_preview};
use crate::color::agent_color;
use crate::highlight::highlight_body;
use crate::pane::{PaneView, SplitMode};
use crate::timefmt::{format_timestamp, is_within};

use super::{NARROW_WIDTH_THRESHOLD, ScreenLayout, compute_layout};

/// MEMBER row is considered "active" for the ● indicator within this window.
const MEMBER_ACTIVITY_WINDOW: ChronoDuration = ChronoDuration::hours(1);

pub fn render(frame: &mut Frame<'_>, app: &App) {
    // Phase 14E: replay steals one row off the top for the
    // `▶ REPLAY @ ...` banner; outside replay `body_area` is exactly
    // `frame.area()`, so this is a no-op for the Off-mode golden regression
    // (see `render_single`'s doc comment) and every existing split/narrow
    // snapshot.
    let full_area = frame.area();
    let body_area = match &app.replay {
        Some(replay) if full_area.height > 1 => {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(0)])
                .split(full_area);
            render_replay_banner(frame, replay, rows[0]);
            rows[1]
        }
        _ => full_area,
    };
    match &app.split {
        SplitMode::Off => render_single(frame, app, body_area),
        SplitMode::Split { active, .. } => render_split(frame, app, *active, body_area),
    }
    render_status_line(frame, app, body_area, body_area.width < NARROW_WIDTH_THRESHOLD);
}

/// `▶ REPLAY @ 2026-07-21 09:00:00 UTC — Esc to exit`, styled so it can't be
/// mistaken for the regular status line (yellow-on-black rather than the
/// status line's fg-only colors) — this is the one always-visible cue that
/// every message/member count on screen is historical, not live.
fn render_replay_banner(frame: &mut Frame<'_>, replay: &crate::state::ReplayState, area: Rect) {
    frame.render_widget(
        Paragraph::new(format!("▶ REPLAY @ {} — Esc to exit", replay.label)).style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        area,
    );
}

/// Pre-14A rendering path, unmodified in shape (still the sole path when
/// `app.split` is `Off`) — this is what the Off-mode golden regression test
/// pins byte-for-byte. `app.pane_view_active()` is exactly `app`'s own
/// fields (see `App::swap_active_pane`'s doc comment), so this produces
/// identical output to reading `app.*` directly, pre-14A.
fn render_single(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let narrow = area.width < NARROW_WIDTH_THRESHOLD;
    let layout = compute_layout(area, app.sidebar_pct, app.focus);
    let pane = app.pane_view_active();
    // In narrow mode `compute_layout` already zeroed out the two panes that
    // aren't `app.focus` (see `compute_narrow_layout`), so these three calls
    // don't need a narrow/wide branch of their own — each just draws into
    // whatever rect it was handed, empty or not.
    render_teams(frame, &pane, layout.teams);
    render_members(frame, &pane, layout.members);
    render_room(frame, &pane, layout.room);
    if !narrow {
        render_resize_handle(frame, &layout);
    }
}

/// Phase 14A: two-pane layout. Side-by-side when the terminal is at least
/// `SPLIT_WIDE_THRESHOLD` columns wide, stacked (top/bottom) otherwise —
/// covers the 80x24 reference terminal from the phase constraints. Pane
/// *slot* (left/top vs. right/bottom) is fixed by `active`: whichever pane
/// is active is always drawn in the same slot it was in in the previous
/// frame (see `App::swap_active_pane`), so `Tab` only moves the highlight,
/// never the panes themselves.
fn render_split(frame: &mut Frame<'_>, app: &App, active: crate::pane::PaneIdx, area: Rect) {
    let wide = area.width >= SPLIT_WIDE_THRESHOLD;
    let direction = if wide {
        Direction::Horizontal
    } else {
        Direction::Vertical
    };
    let halves = Layout::default()
        .direction(direction)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let active_view = app.pane_view_active();
    let inactive_view = app
        .pane_view_inactive()
        .expect("render_split only called while app.split is SplitMode::Split");
    let (first_view, first_active, second_view, second_active) = match active {
        crate::pane::PaneIdx::First => (active_view, true, inactive_view, false),
        crate::pane::PaneIdx::Second => (inactive_view, false, active_view, true),
    };
    render_pane(frame, app, &first_view, halves[0], first_active);
    render_pane(frame, app, &second_view, halves[1], second_active);
}

/// One split pane: an outer border (highlighted iff `is_active`) wrapping
/// the same TEAMS/MEMBERS/ROOM trio a single-pane view draws, so both panes
/// keep the header/body/footer parity the phase spec calls for.
fn render_pane(frame: &mut Frame<'_>, app: &App, pane: &PaneView<'_>, area: Rect, is_active: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let outer_style = if is_active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)
    };
    let outer = Block::default().borders(Borders::ALL).border_style(outer_style);
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    // Reuses the single-pane 3-region split (sidebar/teams/members/room) —
    // its own reserved status row is simply left blank per-pane, since the
    // real status line is one shared bar under both panes (see `render`).
    let layout = compute_layout(inner, app.sidebar_pct, pane.focus);
    render_teams(frame, pane, layout.teams);
    render_members(frame, pane, layout.members);
    render_room(frame, pane, layout.room);
}

fn render_status_line(frame: &mut Frame<'_>, app: &App, area: Rect, narrow: bool) {
    let status_area = Rect::new(area.x, area.y + area.height.saturating_sub(1), area.width, 1);
    // The burst banner outranks the regular status line for its 3s window —
    // it's the one alert that must stay visible even if a mark-read or send
    // completes underneath it, so callers don't miss a message flood.
    if let Some((text, until)) = &app.burst_alert
        && std::time::Instant::now() < *until
    {
        frame.render_widget(
            Paragraph::new(text.as_str())
                .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            status_area,
        );
        return;
    }

    let mut status_text = if app.poll_offline && !app.status.text.starts_with("poll offline") {
        format!("poll offline | {}", app.status.text)
    } else {
        app.status.text.clone()
    };
    if narrow {
        // The H-3 startup warning (and any other message set to run past
        // ~40 cols) used to push this suffix clean off the visible line —
        // status.text was always short enough to coexist before, so nothing
        // truncated it. Trim the message itself rather than let the narrow
        // indicator disappear silently.
        const NARROW_SUFFIX: &str = " [<60cols: 1-pane mode]";
        let available = (status_area.width as usize).saturating_sub(NARROW_SUFFIX.chars().count());
        if status_text.chars().count() > available {
            status_text = status_text
                .chars()
                .take(available.saturating_sub(1))
                .collect::<String>();
            status_text.push('…');
        }
        status_text.push_str(NARROW_SUFFIX);
    }
    let status_style = if app.poll_offline || app.status.is_error {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if app.status.text.starts_with('⚠') {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(status_text).style(status_style),
        status_area,
    );
}

/// Dims the seam between sidebar and room so it reads as a draggable handle
/// rather than just another border line. Drawn last so it wins over both
/// panes' own border cells at that column.
fn render_resize_handle(frame: &mut Frame<'_>, layout: &ScreenLayout) {
    let column = layout.sidebar.x + layout.sidebar.width.saturating_sub(1);
    let style = Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM);
    for row in layout.sidebar.y..layout.sidebar.y + layout.sidebar.height {
        if let Some(cell) = frame.buffer_mut().cell_mut((column, row)) {
            cell.set_symbol("│").set_style(style);
        }
    }
}

/// Team list pane, drawn into `area` as-is — `area` is either the top half
/// of the sidebar column (wide 3-pane mode) or the full terminal width when
/// `Focus::Teams` is active in narrow 1-pane mode (S10-3); either way this
/// function doesn't need to know which, since ratatui widgets no-op on a
/// zero-size area (the other two panes in narrow mode).
fn render_teams(frame: &mut Frame<'_>, pane: &PaneView<'_>, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Phase 14B: only prefix with `[host]` when at least one remote host is
    // configured — with zero hosts (the pre-14B / default case) every team
    // is `LOCAL_HOST` and this branch never fires, so the rendered line is
    // byte-identical to before 14B (no visual change for existing users).
    let show_host = pane.teams.iter().any(|team| team.host != crate::db::LOCAL_HOST);
    let team_items: Vec<ListItem<'_>> = pane
        .teams
        .iter()
        .map(|team| {
            let badge = if team.unread_count > 0 {
                format!(" ●{}", team.unread_count)
            } else {
                String::new()
            };
            let host_prefix = if show_host && team.host != crate::db::LOCAL_HOST {
                format!("[{}] ", team.host)
            } else {
                String::new()
            };
            ListItem::new(format!("{host_prefix}{}{badge}", team.name))
        })
        .collect();
    let mut team_state = ListState::default().with_selected(Some(pane.selected_team));
    let team_block = focused_block("TEAMS", pane.focus == Focus::Teams);
    frame.render_stateful_widget(
        List::new(team_items)
            .block(team_block)
            .highlight_symbol("> ")
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        area,
        &mut team_state,
    );
}

/// Member list pane — same "draws into whatever `area` it's given" contract
/// as [`render_teams`].
fn render_members(frame: &mut Frame<'_>, pane: &PaneView<'_>, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let now = Utc::now();
    let member_items: Vec<ListItem<'_>> = pane
        .members
        .iter()
        .enumerate()
        .map(|(index, member)| {
            let unread = if member.unread_count > 0 {
                format!(" ●{}", member.unread_count)
            } else {
                String::new()
            };
            let last = member
                .last_message_at
                .as_deref()
                .and_then(|value| value.get(11..16))
                .unwrap_or("--:--");
            // Activity dot is only shown on the selected row (per spec: "選択中
            // member 名の右に") rather than every row, so it reads as detail on
            // demand instead of one more always-on badge competing with unread counts.
            let mut suffix = String::new();
            if index == pane.selected_member {
                let active = member
                    .last_message_at
                    .as_deref()
                    .is_some_and(|created_at| is_within(created_at, now, MEMBER_ACTIVITY_WINDOW));
                suffix.push(' ');
                suffix.push(if active { '●' } else { '○' });
            }
            if pane.member_filter == Some(member.name.as_str()) {
                suffix.push_str(" [F]");
            }
            ListItem::new(format!("{} {}{}{}", member.name, last, unread, suffix))
        })
        .collect();
    let selected_member = (!pane.members.is_empty()).then_some(pane.selected_member);
    let mut member_state = ListState::default().with_selected(selected_member);
    frame.render_stateful_widget(
        List::new(member_items)
            .block(focused_block("MEMBERS", pane.focus == Focus::Members))
            .highlight_symbol("> ")
            .highlight_style(
                Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
        area,
        &mut member_state,
    );
}

fn render_room(frame: &mut Frame<'_>, pane: &PaneView<'_>, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let filter_note = pane
        .member_filter
        .map(|name| format!(" | filter:{name}"))
        .unwrap_or_default();
    let search_note = pane
        .search_query
        .map(|query| format!(" | /:{query}"))
        .unwrap_or_default();
    let title = match pane.selected_team_name() {
        Some(team) => format!(
            " agmsg-tui | # {team} | {} members | {} unread{filter_note}{search_note} ",
            pane.members.len(),
            pane.teams.iter().map(|team| team.unread_count).sum::<usize>()
        ),
        None => " agmsg-tui | no teams ".to_owned(),
    };
    let block = focused_block(&title, pane.focus == Focus::Room);

    // Member and search filters compose with AND; with neither set this is
    // simply `0..len`, so normal room rendering is unchanged.
    let visible: Vec<usize> = (0..pane.messages.len())
        .filter(|&index| pane.message_matches_filters(&pane.messages[index]))
        .collect();

    let mut lines = Vec::new();
    if visible.is_empty() {
        lines.push(Line::from(
            if pane.member_filter.is_some() || pane.search_query.is_some() {
                "No messages match the active filter."
            } else {
                "No messages."
            },
        ));
    } else {
        let selected_pos = visible
            .iter()
            .position(|&index| index == pane.selected_message)
            .unwrap_or(visible.len() - 1);
        // Each message now spans a header line, a blank separator, and N
        // body lines, so the old "2 lines per message" heuristic no longer
        // holds; ~3 lines/message keeps recent history in view without
        // needing exact wrap-aware math (Paragraph handles overflow anyway).
        let inner_height = area.height.saturating_sub(2) as usize;
        let visible_count = (inner_height / 3).max(1);
        let start_pos = selected_pos.saturating_sub(visible_count.saturating_sub(1));
        let now = Utc::now();
        let divider = "─".repeat(area.width.saturating_sub(2).max(1) as usize);

        for (rendered, &index) in visible.iter().enumerate().skip(start_pos) {
            if rendered != start_pos {
                lines.push(Line::from(Span::styled(
                    divider.clone(),
                    Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
                )));
            }
            let message = &pane.messages[index];
            let selected = index == pane.selected_message;
            push_message_header(&mut lines, pane, message, selected, now);
            lines.push(Line::from(""));
            push_message_body(&mut lines, pane, message);
        }
    }

    let footer = Line::from(vec![
        Span::raw("  [c][H:health][Ctrl-F:bulk][r:read-recipient][R][/][?:help]  "),
        Span::styled("details: ?", Style::default().fg(Color::DarkGray)),
    ]);
    lines.push(footer);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// `[time] from → to [●unread]`, left-margined with `>` when this is the
/// selected message or `▏` when it's the current identity's own message —
/// the two markers never compete for the same row since a message can't be
/// both the cursor position's neighbor and "not selected" at once.
fn push_message_header(
    lines: &mut Vec<Line<'static>>,
    pane: &PaneView<'_>,
    message: &crate::db::Message,
    selected: bool,
    now: chrono::DateTime<Utc>,
) {
    let lead = if selected {
        "> "
    } else if pane.is_own_message(message) {
        "▏ "
    } else {
        "  "
    };
    let lead_style = if selected {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let mut spans = vec![
        Span::styled(lead, lead_style),
        Span::styled(
            format!("[{}]", format_timestamp(&message.created_at, now)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(" "),
        Span::styled(
            message.from_agent.clone(),
            Style::default()
                .fg(agent_color(&message.from_agent, crate::palette::current().mode))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" → ", Style::default().fg(Color::DarkGray)),
        Span::styled(message.to_agent.clone(), Style::default().fg(Color::DarkGray)),
    ];
    if message.read_at.is_none() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            "●",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(spans));
}

/// Full body when short or already expanded; otherwise the first
/// `FOLD_PREVIEW_LINES` lines plus a trim note (`X` toggles between the two,
/// tracked per message id in `App::expanded_messages`).
fn push_message_body(lines: &mut Vec<Line<'static>>, pane: &PaneView<'_>, message: &crate::db::Message) {
    if pane.body_is_folded(message) {
        let (preview, trimmed_chars) = fold_preview(&message.body);
        for line in highlight_body(&preview) {
            lines.push(indent_line(line));
        }
        lines.push(Line::from(Span::styled(
            format!("  [... {trimmed_chars} chars trimmed, press X to expand]"),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        )));
    } else {
        for line in highlight_body(&message.body) {
            lines.push(indent_line(line));
        }
    }
}

/// One `KEY  description` row in the help popup.
type HelpEntry = (&'static str, &'static str);

/// A titled group of [`HelpEntry`] rows, rendered with a bold header and a
/// dim rule under it so the popup reads as distinct blocks instead of one
/// undifferentiated wall of bindings.
struct HelpSection {
    title: &'static str,
    entries: &'static [HelpEntry],
}

const HELP_LEFT: &[HelpSection] = &[
    HelpSection {
        title: "NAVIGATION",
        entries: &[
            ("Tab / S-Tab", "Focus next / previous pane"),
            ("Ctrl+A", "Toggle audit dashboard"),
            ("A", "Agents screen"),
            ("H", "Health & trends screen"),
            ("Ctrl+F", "Cross-team bulk filter"),
            ("Ctrl+N", "Notification settings"),
            ("Ctrl+B", "Mute / unmute terminal bell"),
            ("?", "Toggle this help"),
        ],
    },
    HelpSection {
        title: "MAIN / ROOM",
        entries: &[
            ("j/k  ↑/↓", "Move selection"),
            ("g/G  Ctrl-D/U", "Filtered edges / half page"),
            ("u  [ / ]", "Next unread / previous-next team"),
            ("Enter  x/X", "Open / toggle message fold"),
            ("f", "Fold / unfold all in team"),
            ("s", "Jump to nearest same-sender msg"),
            ("c", "Compose message"),
            ("r / R", "Recipient-all / team-all read"),
            ("a", "Open audit dashboard"),
            ("/  n / N", "Search, next / prev match"),
            ("y", "Yank body to clipboard (any focus)"),
        ],
    },
    HelpSection {
        title: "MEMBERS",
        entries: &[
            ("Enter", "Compose to member"),
            ("I", "Member info"),
            ("F", "Filter by member"),
            ("M", "Mark member unread"),
            ("n", "New agent (team default)"),
            ("R", "Rename selected member"),
            ("Esc", "Close info popup"),
        ],
    },
    HelpSection {
        title: "HEALTH",
        entries: &[
            ("H / Esc", "Back to main"),
            ("j/k  ↑/↓", "Select team"),
            ("t", "Toggle 7d / 30d window"),
            ("R", "Refresh health"),
            ("?", "Open this help"),
            ("q", "Quit agmsg-tui"),
        ],
    },
    HelpSection {
        title: "BULK FILTER",
        entries: &[
            ("Ctrl+F / Esc", "Toggle bulk filter / main"),
            ("Tab / S-Tab", "Cycle agent / period / body / results"),
            ("←/→  7/3/a", "Select 7d / 30d / all"),
            ("j/k  g/G", "Select result / edges"),
            ("M", "Preview filtered unread marking"),
            ("E", "Export Markdown / JSON"),
        ],
    },
];

const HELP_RIGHT: &[HelpSection] = &[
    HelpSection {
        title: "COMPOSER",
        entries: &[
            // L-5: this used to say "switch from / to field", which reads
            // as focus-moves-between-fields — it doesn't. Both keys stay on
            // the same field and cycle which roster entry it holds.
            ("Tab / S-Tab", "Cycle from / to value"),
            ("Arrows Home/End", "Move cursor"),
            ("Ctrl+A / E", "Cursor to start / end"),
            ("Ctrl+W", "Delete previous word"),
            ("Ctrl+K", "Clear draft"),
            ("Ctrl+S", "Send message"),
            ("Esc", "Save draft & close"),
        ],
    },
    HelpSection {
        title: "AUDIT",
        entries: &[
            ("D / H", "Dashboard/history"),
            ("t", "Pair matrix 7d / 30d / 90d"),
            ("h/l  ←/→", "Switch team"),
            ("j/k  ↑/↓", "Select item"),
            ("g", "Jump to top"),
            ("R / a", "Refresh audit"),
            ("Enter", "Item detail"),
            ("D", "Show reset command"),
            ("B", "Bulk reset stale / zombie identities"),
            ("W", "Bulk rename naming violations"),
            ("M", "Mark stale unread"),
            ("E / x", "Export report"),
            ("Tab / Ctrl+A", "Back to main"),
        ],
    },
    HelpSection {
        title: "AGENTS",
        entries: &[
            ("Tab / t", "Switch team / identity focus"),
            ("j/k  g/G", "Move selection"),
            ("r", "Reload agents"),
            ("n", "New agent"),
            ("R", "Rename identity (any focus)"),
            ("T", "Rename team (any focus)"),
            ("X / Del", "Reset identity (identity focus)"),
            ("D", "Despawn identity (identity focus)"),
            ("L", "Leave (any focus)"),
            ("Enter", "Identity info (identity focus)"),
        ],
    },
    HelpSection {
        title: "NOTIFICATIONS",
        entries: &[
            ("Ctrl+N", "Open settings popup"),
            ("j/k  ↑/↓", "Move selection"),
            ("Enter / Space", "Toggle setting"),
            ("Esc", "Close popup"),
        ],
    },
    HelpSection {
        title: "GENERAL",
        entries: &[
            ("q", "Quit agmsg-tui"),
            ("Esc", "Close popup / clear filter"),
            ("?", "Close this help"),
            ("AGMSG_IDENTITY", "Env: own identity (marker/guard)"),
        ],
    },
];

/// Key label column width; descriptions start here regardless of label
/// length, so the cyan/white boundary lines up across every row.
const HELP_KEY_COL: usize = 17;

fn flatten_help_sections(sections: &[HelpSection]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, section) in sections.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            section.title,
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "─".repeat(section.title.len().max(6)),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        )));
        for (key, desc) in section.entries {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{key:<HELP_KEY_COL$}"),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled(*desc, Style::default().fg(Color::White)),
            ]));
        }
    }
    lines
}

pub fn render_help(frame: &mut Frame<'_>, app: &App) {
    let area = centered_rect(88, 90, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" help — Main Esc=clear only | q=quit ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(3), Constraint::Length(1)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "agmsg-tui — Keybindings",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center),
        rows[0],
    );

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    let left_lines = flatten_help_sections(HELP_LEFT);
    let right_lines = flatten_help_sections(HELP_RIGHT);
    let content_len = left_lines.len().max(right_lines.len()) as u16;

    // Render-time clamp only — `help_scroll` itself is unbounded above so
    // key handling never needs to know popup geometry, only the paint step
    // does.
    let max_scroll = content_len.saturating_sub(columns[0].height);
    let scroll = app.help_scroll.min(max_scroll);

    frame.render_widget(Paragraph::new(left_lines).scroll((scroll, 0)), columns[0]);
    frame.render_widget(Paragraph::new(right_lines).scroll((scroll, 0)), columns[1]);

    let footer_text = if content_len > rows[1].height {
        "Esc/? close  ·  j/k or ↑/↓ to scroll"
    } else {
        "Esc/? close"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer_text,
            Style::default().fg(Color::DarkGray),
        )))
        .alignment(Alignment::Center),
        rows[2],
    );
}

fn focused_block<'a>(title: &'a str, focused: bool) -> Block<'a> {
    let style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(style)
}

/// `I` on the MEMBER column: agent/registration/traffic card, centered over
/// whatever screen is behind it. No-ops when `app.member_info` is `None` so
/// callers can invoke it unconditionally every frame.
pub fn render_member_info(frame: &mut Frame<'_>, app: &App) {
    let Some(info) = app.member_info.as_deref() else {
        return;
    };
    let area = centered_rect(56, 46, frame.area());
    frame.render_widget(Clear, area);
    let lines: Vec<Line<'_>> = info.split('\n').map(Line::from).collect();
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" member info (Esc/I to close) ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Prepends a 2-space indent span without collapsing the syntax-highlighted
/// spans that follow it into one flat string.
fn indent_line(line: Line<'static>) -> Line<'static> {
    // Preserve alignment/style: the code-block language badge (highlight.rs)
    // relies on `Line::right_aligned()` surviving this wrap, otherwise it'd
    // silently fall back to left-aligned once indented.
    let Line { spans, style, alignment } = line;
    let mut new_spans = vec![Span::raw("  ")];
    new_spans.extend(spans);
    let mut out = Line::from(new_spans).style(style);
    if let Some(alignment) = alignment {
        out = out.alignment(alignment);
    }
    out
}

pub(super) fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}
