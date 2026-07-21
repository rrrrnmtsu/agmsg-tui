mod audit;
mod agents;
mod composer;
mod health;
mod main_screen;
mod notification_settings;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::app::{App, Focus, Screen};

/// Sidebar width as a percentage of the terminal, drag-resizable between
/// these bounds (requirement: 20%-60%).
pub const SIDEBAR_MIN_PCT: u16 = 20;
pub const SIDEBAR_MAX_PCT: u16 = 60;
pub const SIDEBAR_DEFAULT_PCT: u16 = 30;

/// Below this terminal width, the 3-pane layout collapses to whichever pane
/// currently has `Focus`, at full width (S10-3). `Tab`/`Shift-Tab` already
/// cycle Teams → Members → Room → Teams for the wide-mode mouse/keyboard
/// focus model, so narrow mode reuses that exact cycle instead of adding a
/// parallel keybinding.
pub const NARROW_WIDTH_THRESHOLD: u16 = 60;

/// The regions computed by [`main_screen::render`], recomputed here so mouse
/// hit-testing in `app.rs` and drawing in `main_screen.rs` never drift apart
/// (single source of truth instead of two independent Layout calls).
#[derive(Clone, Copy, Debug)]
pub struct ScreenLayout {
    pub sidebar: Rect,
    pub teams: Rect,
    pub members: Rect,
    pub room: Rect,
    pub status: Rect,
}

pub fn compute_layout(area: Rect, sidebar_pct: u16, focus: Focus) -> ScreenLayout {
    if area.width < NARROW_WIDTH_THRESHOLD {
        return compute_narrow_layout(area, focus);
    }
    let sidebar_pct = sidebar_pct.clamp(SIDEBAR_MIN_PCT, SIDEBAR_MAX_PCT);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(sidebar_pct),
            Constraint::Percentage(100 - sidebar_pct),
        ])
        .split(rows[0]);
    let sidebar_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(columns[0]);
    ScreenLayout {
        sidebar: columns[0],
        teams: sidebar_rows[0],
        members: sidebar_rows[1],
        room: columns[1],
        status: rows[1],
    }
}

/// One pane at full width — whichever `focus` currently points at — with the
/// other two collapsed to a zero-size `Rect` at the same origin. Zero-size
/// rects are deliberate, not a placeholder to special-case away: every
/// ratatui widget already treats a 0-width/0-height area as "draw nothing"
/// (`main_screen::render_teams`/`render_members`/`render_room` early-return
/// on it too, defensively), and `rect_contains`/hit-testing naturally never
/// matches an empty rect, so the wide-mode call sites don't need a narrow/
/// wide branch of their own.
fn compute_narrow_layout(area: Rect, focus: Focus) -> ScreenLayout {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);
    let main = rows[0];
    let empty = Rect::new(main.x, main.y, 0, 0);
    let (teams, members, room) = match focus {
        Focus::Teams => (main, empty, empty),
        Focus::Members => (empty, main, empty),
        Focus::Room => (empty, empty, main),
    };
    ScreenLayout { sidebar: main, teams, members, room, status: rows[1] }
}

/// Which pane (if any) a mouse event landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseTarget {
    Teams,
    Members,
    Room,
    /// The 1-cell column between sidebar and room; dragging it resizes.
    ResizeHandle,
    None,
}

pub fn hit_test(layout: &ScreenLayout, column: u16, row: u16) -> MouseTarget {
    let boundary = layout.sidebar.x + layout.sidebar.width;
    // boundary-1 covers the sidebar's own right border cell too, so a click
    // right on either side of the seam still grabs the resize handle.
    if row >= layout.sidebar.y
        && row < layout.sidebar.y + layout.sidebar.height
        && (column == boundary || column + 1 == boundary)
    {
        return MouseTarget::ResizeHandle;
    }
    if rect_contains(layout.teams, column, row) {
        return MouseTarget::Teams;
    }
    if rect_contains(layout.members, column, row) {
        return MouseTarget::Members;
    }
    if rect_contains(layout.room, column, row) {
        return MouseTarget::Room;
    }
    MouseTarget::None
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Converts a drag's absolute column back into a sidebar percentage, clamped
/// to the same [`SIDEBAR_MIN_PCT`, `SIDEBAR_MAX_PCT`] bounds as rendering.
pub fn resize_pct_from_column(area: Rect, column: u16) -> u16 {
    if area.width == 0 {
        return SIDEBAR_DEFAULT_PCT;
    }
    let ratio = (column.saturating_sub(area.x) as u32 * 100) / area.width as u32;
    (ratio as u16).clamp(SIDEBAR_MIN_PCT, SIDEBAR_MAX_PCT)
}

pub fn render(frame: &mut Frame<'_>, app: &App) {
    match app.screen {
        Screen::Audit => audit::render(frame, app),
        Screen::Agents => agents::render(frame, app),
        Screen::Health => health::render(frame, app),
        Screen::Composer => {
            main_screen::render(frame, app);
            composer::render(frame, app);
        }
        Screen::Help => {
            match app.help_return_screen {
                Screen::Agents => agents::render(frame, app),
                Screen::Health => health::render(frame, app),
                _ => main_screen::render(frame, app),
            }
            main_screen::render_help(frame, app);
        }
        Screen::Main => {
            main_screen::render(frame, app);
            main_screen::render_member_info(frame, app);
            notification_settings::render(frame, app);
        }
    }
    // Single choke point for every screen (S10-1/S10-2): applied after all
    // widgets have drawn, so it sees the final color every cell actually
    // ended up with instead of needing to intercept each `Style` at the ~90
    // call sites that build one. See `palette::apply_to_buffer` for why.
    crate::palette::apply_to_buffer(frame.buffer_mut());
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use chrono::NaiveDate;
    use ratatui::{
        Terminal,
        backend::{Backend, TestBackend},
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use rusqlite::Connection;
    use tempfile::{TempDir, tempdir};

    use super::render;
    use crate::app::{App, AppAction, Screen};
    use crate::config::Paths;
    use crate::db::{AgentTraffic, DailyTraffic};
    use crate::exec::CommandResult;
    use crate::health::{
        BridgeStatus, DeliveryMode, HealthSnapshot, TeamHealth,
    };

    fn agents_fixture() -> (TempDir, App) {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("messages.db");
        Connection::open(&db_path)
            .expect("fixture db")
            .execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    team TEXT NOT NULL,
                    from_agent TEXT NOT NULL,
                    to_agent TEXT NOT NULL,
                    body TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    read_at TEXT
                );
                INSERT INTO messages (team, from_agent, to_agent, body, created_at)
                VALUES ('ops-hub', 'claude-main', 'codex-worker', 'hello', '2999-01-01T00:00:00Z');",
            )
            .expect("schema");
        let teams_dir = temp.path().join("teams");
        fs::create_dir_all(teams_dir.join("ops-hub")).expect("ops team");
        fs::create_dir_all(teams_dir.join("sakura-project")).expect("sakura team");
        fs::write(
            teams_dir.join("ops-hub/config.json"),
            r#"{"name":"ops-hub","agents":{"claude-main":{"registrations":[{"type":"claude-code","project":"/Users/remma/dev/ops-hub"}]},"codex-worker":{"registrations":[{"type":"codex","project":"/Users/remma/dev/ops-hub"}]}}}"#,
        )
        .expect("ops config");
        fs::write(
            teams_dir.join("sakura-project/config.json"),
            r#"{"name":"sakura-project","agents":{"opencode-review":{"registrations":[{"type":"opencode","project":"/Users/remma/dev/sakura/sakura-project"}]}}}"#,
        )
        .expect("sakura config");
        let app = App::load(Paths {
            db: db_path,
            teams_dir,
            scripts_dir: temp.path().join("scripts"),
            audit_script: temp.path().join("agmsg-audit"),
            report_dir: temp.path().join("reports"),
            state_file: temp.path().join("state.json"),
        })
        .expect("app");
        (temp, app)
    }

    fn health_snapshot() -> HealthSnapshot {
        let date = NaiveDate::from_ymd_opt(2026, 7, 21).expect("fixture date");
        let traffic = vec![
            DailyTraffic {
                date,
                count: 3,
                burst: false,
            },
            DailyTraffic {
                date,
                count: 8,
                burst: false,
            },
            DailyTraffic {
                date,
                count: 21,
                burst: false,
            },
        ];
        let ops = TeamHealth {
            name: "ops-hub".to_owned(),
            orphan: false,
            delivery: DeliveryMode::Monitor,
            bridges: vec![
                BridgeStatus {
                    label: "claude-code".to_owned(),
                    pid: 101,
                    alive: true,
                },
                BridgeStatus {
                    label: "codex".to_owned(),
                    pid: 102,
                    alive: true,
                },
            ],
            last_msg_age: Some(Duration::from_secs(180)),
            unread: 0,
            stale_unread: false,
            traffic_7d: traffic.clone(),
            traffic_30d: traffic.clone(),
            agents_7d: vec![
                AgentTraffic {
                    agent: "claude".to_owned(),
                    sent: 58,
                    received: 41,
                },
                AgentTraffic {
                    agent: "codex".to_owned(),
                    sent: 40,
                    received: 52,
                },
            ],
            agents_30d: Vec::new(),
        };
        let orphan = TeamHealth {
            name: "old-team".to_owned(),
            orphan: true,
            delivery: DeliveryMode::Unknown,
            bridges: Vec::new(),
            last_msg_age: Some(Duration::from_secs(45 * 86_400)),
            unread: 6,
            stale_unread: true,
            traffic_7d: vec![DailyTraffic {
                date,
                count: 0,
                burst: false,
            }],
            traffic_30d: Vec::new(),
            agents_7d: Vec::new(),
            agents_30d: Vec::new(),
        };
        HealthSnapshot {
            teams: vec![ops, orphan],
            daily_total_7d: traffic.clone(),
            daily_total_30d: traffic,
            refreshed_at: "12:04:31".to_owned(),
            burst_threshold: 150,
        }
    }

    #[test]
    fn renders_at_80_by_24() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("messages.db");
        let connection = Connection::open(&db_path).expect("fixture db");
        connection
            .execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    team TEXT NOT NULL,
                    from_agent TEXT NOT NULL,
                    to_agent TEXT NOT NULL,
                    body TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    read_at TEXT
                );",
            )
            .expect("schema");
        let teams_dir = temp.path().join("teams");
        fs::create_dir_all(teams_dir.join("ops-hub")).expect("team dir");
        fs::write(
            teams_dir.join("ops-hub/config.json"),
            r#"{"agents":{"codex":{},"claude":{}}}"#,
        )
        .expect("config");
        let app = App::load(Paths {
            db: db_path,
            teams_dir,
            scripts_dir: temp.path().join("scripts"),
            audit_script: temp.path().join("agmsg-audit"),
            report_dir: temp.path().join("reports"),
            state_file: temp.path().join("state.json"),
        })
        .expect("app");
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw");
        assert_eq!(terminal.backend().size().expect("size").width, 80);
        assert_eq!(terminal.backend().size().expect("size").height, 24);
    }

    /// S10-3: below `NARROW_WIDTH_THRESHOLD` the layout collapses to
    /// whichever pane is focused, so this exercises the full render pipeline
    /// (not just `compute_layout`) at the narrowest width the spec commits
    /// to ("80x24 崩れなし... S10-3 の 1-pane mode で新幅 40x24 でも動く").
    #[test]
    fn renders_at_40_by_24_in_one_pane_mode_without_panicking() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("messages.db");
        let connection = Connection::open(&db_path).expect("fixture db");
        connection
            .execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    team TEXT NOT NULL,
                    from_agent TEXT NOT NULL,
                    to_agent TEXT NOT NULL,
                    body TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    read_at TEXT
                );
                INSERT INTO messages (team, from_agent, to_agent, body, created_at)
                VALUES ('ops-hub', 'claude-main', 'codex-worker', 'hello', '2999-01-01T00:00:00Z');",
            )
            .expect("schema");
        let teams_dir = temp.path().join("teams");
        fs::create_dir_all(teams_dir.join("ops-hub")).expect("team dir");
        fs::write(
            teams_dir.join("ops-hub/config.json"),
            r#"{"agents":{"codex":{},"claude":{}}}"#,
        )
        .expect("config");
        let mut app = App::load(Paths {
            db: db_path,
            teams_dir,
            scripts_dir: temp.path().join("scripts"),
            audit_script: temp.path().join("agmsg-audit"),
            report_dir: temp.path().join("reports"),
            state_file: temp.path().join("state.json"),
        })
        .expect("app");
        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        for focus in [
            crate::app::Focus::Teams,
            crate::app::Focus::Members,
            crate::app::Focus::Room,
        ] {
            app.focus = focus;
            terminal.draw(|frame| render(frame, &app)).expect("draw did not panic");
        }
        let snapshot = terminal.backend().to_string();
        assert!(snapshot.contains("1-pane mode"));
    }

    #[test]
    fn help_snapshot_covers_phase_six_keys_and_draft_at_80_by_24() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("messages.db");
        Connection::open(&db_path)
            .expect("fixture db")
            .execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    team TEXT NOT NULL,
                    from_agent TEXT NOT NULL,
                    to_agent TEXT NOT NULL,
                    body TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    read_at TEXT
                );",
            )
            .expect("schema");
        let teams_dir = temp.path().join("teams");
        fs::create_dir_all(teams_dir.join("ops-hub")).expect("team dir");
        fs::write(
            teams_dir.join("ops-hub/config.json"),
            r#"{"agents":{"codex":{},"claude":{}}}"#,
        )
        .expect("config");
        let mut app = App::load(Paths {
            db: db_path,
            teams_dir,
            scripts_dir: temp.path().join("scripts"),
            audit_script: temp.path().join("agmsg-audit"),
            report_dir: temp.path().join("reports"),
            state_file: temp.path().join("state.json"),
        })
        .expect("app");
        app.screen = Screen::Help;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw");
        let snapshot = terminal.backend().to_string();
        for needle in ["/", "X", "F", "M", "draft", "NAVIGATION", "COMPOSER"] {
            assert!(
                snapshot.contains(needle),
                "help snapshot missing {needle:?}"
            );
        }
        assert!(snapshot.contains("Main Esc=clear only | q=quit"));
        assert!(snapshot.contains("agmsg-tui — Keybindings"));
        app.help_scroll = u16::MAX;
        terminal
            .draw(|frame| render(frame, &app))
            .expect("draw scrolled help");
        assert!(terminal.backend().to_string().contains("HEALTH"));
    }

    #[test]
    fn help_snapshot_covers_phase_seven_agent_keys_at_80_by_24() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("messages.db");
        Connection::open(&db_path)
            .expect("fixture db")
            .execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    team TEXT NOT NULL,
                    from_agent TEXT NOT NULL,
                    to_agent TEXT NOT NULL,
                    body TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    read_at TEXT
                );",
            )
            .expect("schema");
        let teams_dir = temp.path().join("teams");
        fs::create_dir_all(teams_dir.join("ops-hub")).expect("team dir");
        fs::write(
            teams_dir.join("ops-hub/config.json"),
            r#"{"agents":{"codex":{},"claude":{}}}"#,
        )
        .expect("config");
        let mut app = App::load(Paths {
            db: db_path,
            teams_dir,
            scripts_dir: temp.path().join("scripts"),
            audit_script: temp.path().join("agmsg-audit"),
            report_dir: temp.path().join("reports"),
            state_file: temp.path().join("state.json"),
        })
        .expect("app");
        // help_return_screen mirrors how `?` is reached from the Agents
        // screen (see app.rs handle_agents_key) — the popup content itself
        // is static, but this exercises the same render path as real usage.
        app.help_return_screen = Screen::Agents;
        app.screen = Screen::Help;
        // The AGENTS section now lives further down the right column than
        // fits in an 80x24 popup at rest (Phase 8 redesign spreads keys one
        // per line instead of pipe-compressing them), so scroll down first —
        // the same `j`/PageDown handling a real user would reach for.
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
            .expect("scroll help");
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
            .expect("scroll help");
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw");
        assert_eq!(terminal.backend().size().expect("size").width, 80);
        assert_eq!(terminal.backend().size().expect("size").height, 24);
        let snapshot = terminal.backend().to_string();
        for needle in [
            "AGENTS",
            "New agent",
            "Rename identity",
            "Rename team",
            "Reset identity",
            "Leave (any focus)",
        ] {
            assert!(
                snapshot.contains(needle),
                "help snapshot missing {needle:?}"
            );
        }
    }

    #[test]
    fn audit_renders_at_80_by_24() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("messages.db");
        Connection::open(&db_path)
            .expect("fixture db")
            .execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    team TEXT NOT NULL,
                    from_agent TEXT NOT NULL,
                    to_agent TEXT NOT NULL,
                    body TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    read_at TEXT
                );
                INSERT INTO messages (team, from_agent, to_agent, body, created_at)
                VALUES ('ops-hub', 'claude-main', 'codex-worker', 'hello', '2999-01-01T00:00:00Z');",
            )
            .expect("schema");
        let teams_dir = temp.path().join("teams");
        fs::create_dir_all(teams_dir.join("ops-hub")).expect("team dir");
        fs::write(
            teams_dir.join("ops-hub/config.json"),
            r#"{"agents":{"codex-worker":{},"claude-main":{}}}"#,
        )
        .expect("config");
        let mut app = App::load(Paths {
            db: db_path,
            teams_dir,
            scripts_dir: temp.path().join("scripts"),
            audit_script: temp.path().join("agmsg-audit"),
            report_dir: temp.path().join("reports"),
            state_file: temp.path().join("state.json"),
        })
        .expect("app");
        app.screen = Screen::Audit;
        app.complete_audit(&CommandResult {
            success: true,
            exit_code: Some(0),
            stdout: r#"{"ts":"2026-07-20T11:29:58Z","window_days":30,"score":83,"total_msg":1,"total_teams":1,"total_agents":2,"unread":1,"unread_stale":0,"body_p95":5,"burst_days":0,"asymmetric_pairs":1,"zombie_identities":0,"stale_run_files":0,"max_team_pct":100,"axes":{"team_naming":{"score":10,"note":"OK"},"agent_naming":{"score":10,"note":"OK"},"body_size":{"score":10,"note":"OK"},"burst_control":{"score":10,"note":"OK"},"loop_prevention":{"score":5,"note":"asym"},"unread_hygiene":{"score":10,"note":"OK"},"zombie_cleanup":{"score":10,"note":"OK"},"state_hygiene":{"score":10,"note":"OK"},"traffic_spread":{"score":3,"note":"100%"},"activity":{"score":5,"note":"1 msg"}}}"#.to_owned(),
            stderr: String::new(),
        })
        .expect("audit");
        app.export_audit_report().expect("export");
        assert!(
            temp.path()
                .join("reports/agmsg-report-20260720-1129.md")
                .is_file()
        );
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw");
        assert_eq!(terminal.backend().size().expect("size").width, 80);
        assert_eq!(terminal.backend().size().expect("size").height, 24);
        let snapshot = terminal.backend().to_string();
        assert!(snapshot.contains("/100"));
        assert!(snapshot.contains("10 AXES"));
        assert!(snapshot.contains("PAIR MATRIX 30d"));
        assert!(snapshot.contains("ACTIONS Z:"));
    }

    #[test]
    fn agents_snapshot_contains_two_teams_and_three_identities_at_80_by_24() {
        let (_temp, mut app) = agents_fixture();
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        app.agent_teams[0].identities[0].sent_30d = 123;
        app.agent_teams[0].identities[0].received_30d = 456;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw");
        let snapshot = terminal.backend().to_string();
        for needle in [
            "ops-hub",
            "sakura-project",
            "claude-main",
            "codex-worker",
            "opencode-review",
            "s123/r456",
        ] {
            assert!(snapshot.contains(needle), "agents snapshot missing {needle}");
        }

        assert_eq!(terminal.backend().size().expect("size").width, 80);
        assert_eq!(terminal.backend().size().expect("size").height, 24);
    }

    #[test]
    fn self_rename_modal_shows_bridge_warning_only_for_self() {
        let (_temp, mut app) = agents_fixture();
        app.current_identity = Some("claude-main".to_owned());
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
            .expect("rename self");
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw self");
        assert!(terminal.backend().to_string().contains("bridge restart"));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close modal");
        app.agent_identity_index = 1;
        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
            .expect("rename peer");
        terminal.draw(|frame| render(frame, &app)).expect("draw peer");
        assert!(!terminal.backend().to_string().contains("bridge restart"));
    }

    #[test]
    fn team_rename_and_self_reset_modals_fit_at_80_by_24() {
        let (_temp, mut app) = agents_fixture();
        app.current_identity = Some("claude-main".to_owned());
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("team focus");
        app.handle_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::NONE))
            .expect("rename team");
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw team modal");
        let snapshot = terminal.backend().to_string();
        assert!(snapshot.contains("repo-slug"));
        assert!(snapshot.contains("whoami"));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close team modal");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .expect("identity focus");
        app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE))
            .expect("self reset");
        terminal.draw(|frame| render(frame, &app)).expect("draw self reset");
        let snapshot = terminal.backend().to_string();
        assert!(snapshot.contains("self-reset is refused"));
        assert!(!snapshot.contains("confirm :"));
    }

    #[test]
    fn health_snapshot_matches_wide_operational_layout_at_80_by_24() {
        let (_temp, mut app) = agents_fixture();
        let action = app
            .handle_key(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::NONE))
            .expect("open health");
        assert!(matches!(action, AppAction::RefreshHealth));
        app.complete_health(health_snapshot());
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &app)).expect("draw health");
        let snapshot = terminal.backend().to_string();
        for needle in [
            "HEALTH",
            "[7d]",
            "ops-hub",
            "monitor",
            "● 2/2 up",
            "(orphan) old",
            "6 !",
            "ops-hub agents (7d)",
            "daily total (7d)",
            "burst>150: none",
        ] {
            assert!(snapshot.contains(needle), "health snapshot missing {needle}");
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("toggle health window");
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("select next health team");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("draw 30d health");
        let snapshot = terminal.backend().to_string();
        assert!(snapshot.contains("[30d]"));
        assert!(snapshot.contains("old-team agents (30d)"));
        let refresh = app
            .handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
            .expect("refresh health");
        assert!(matches!(refresh, AppAction::RefreshHealth));
    }

    #[test]
    fn health_snapshot_drops_traffic_column_below_60_columns() {
        let (_temp, mut app) = agents_fixture();
        app.screen = Screen::Health;
        app.health_snapshot = Some(health_snapshot());
        let backend = TestBackend::new(40, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render(frame, &app))
            .expect("draw narrow health");
        let snapshot = terminal.backend().to_string();
        for needle in ["TEAM", "MODE", "BRIDGE", "UNREAD", "ops-hub"] {
            assert!(snapshot.contains(needle), "narrow snapshot missing {needle}");
        }
        assert!(!snapshot.contains("LAST MSG"));
        assert!(!snapshot.contains("TRAFFIC"));
    }
}
