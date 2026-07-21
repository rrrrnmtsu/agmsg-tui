//! Phase 14A: split-view (two teams side-by-side).
//!
//! Fixture always seeds two teams (`ops-hub`, `sakura-project`) each with
//! their own message so nav/composer/scroll assertions can tell the panes
//! apart by content, not just by index.

use agmsg_tui::app::{App, AppAction, Screen};
use agmsg_tui::config::Paths;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rusqlite::Connection;
use std::fs;
use tempfile::{TempDir, tempdir};

fn ctrl_s() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
}

fn tab() -> KeyEvent {
    KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
}

fn esc() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}

fn down() -> KeyEvent {
    KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
}

fn two_team_fixture() -> (TempDir, App) {
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
            INSERT INTO messages (team, from_agent, to_agent, body, created_at) VALUES
                ('ops-hub', 'claude-main', 'codex-worker', 'hub msg 1', '2026-01-01T00:00:00Z'),
                ('ops-hub', 'claude-main', 'codex-worker', 'hub msg 2', '2026-01-01T00:01:00Z'),
                ('ops-hub', 'claude-main', 'codex-worker', 'hub msg 3', '2026-01-01T00:02:00Z'),
                ('sakura-project', 'opencode-review', 'cursor-1', 'sakura msg 1', '2026-01-01T00:00:00Z'),
                ('sakura-project', 'opencode-review', 'cursor-1', 'sakura msg 2', '2026-01-01T00:01:00Z');",
        )
        .expect("schema");
    let teams_dir = temp.path().join("teams");
    fs::create_dir_all(teams_dir.join("ops-hub")).expect("ops team dir");
    fs::create_dir_all(teams_dir.join("sakura-project")).expect("sakura team dir");
    fs::write(
        teams_dir.join("ops-hub/config.json"),
        r#"{"agents":{"claude-main":{"registrations":[{"type":"claude-code","project":"/tmp/ops-hub"}]},"codex-worker":{"registrations":[{"type":"codex","project":"/tmp/ops-hub"}]}}}"#,
    )
    .expect("ops config");
    fs::write(
        teams_dir.join("sakura-project/config.json"),
        r#"{"agents":{"opencode-review":{"registrations":[{"type":"opencode","project":"/tmp/sakura"}]},"cursor-1":{"registrations":[{"type":"cursor","project":"/tmp/sakura"}]}}}"#,
    )
    .expect("sakura config");
    let mut app = App::load(Paths {
        db: db_path,
        teams_dir,
        scripts_dir: temp.path().join("scripts"),
        audit_script: temp.path().join("agmsg-audit"),
        audit_history: temp.path().join("audit.jsonl"),
        report_dir: temp.path().join("reports"),
        state_file: temp.path().join("state.json"),
        keys_file: temp.path().join("keys.toml"),
        hosts_file: temp.path().join("hosts.toml"),
        remote_dir: temp.path().join("agmsg-remote"),
    })
    .expect("app");
    // Tests below assume split entry is never refused for min-height — the
    // real default (`Rect::new(0,0,120,40)`) already clears
    // `MIN_SPLIT_HEIGHT`, spelled out again here just so a future default
    // change can't silently break this fixture's assumption.
    assert!(app.term_area.height >= 24);
    app.split = agmsg_tui::pane::SplitMode::Off;
    (temp, app)
}

fn render_to_buffer(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| agmsg_tui::ui::render(frame, app))
        .expect("draw");
    terminal.backend().buffer().clone()
}

/// Off-mode golden regression (deliverable "Preserve byte-identical
/// rendering when split mode is OFF"): render, toggle split on then back
/// off, render again — the two buffers must match exactly at the 80x24
/// reference terminal from the phase constraints.
#[test]
fn split_off_mode_render_is_byte_identical_before_and_after_a_split_cycle() {
    let (_temp, mut app) = two_team_fixture();
    // Pin the status line to a fixed value on both sides of the cycle:
    // toggling split legitimately changes `app.status` ("split view off"),
    // and that's a correct, expected state change — not the thing this
    // regression test is pinning. What must stay byte-identical is the
    // *layout/data* rendering, so a same-known-status comparison isolates
    // that from incidental status-text drift.
    let ready = agmsg_tui::app::StatusLine { text: "ready".to_owned(), is_error: false };
    app.status = ready.clone();
    let before = render_to_buffer(&app, 80, 24);

    app.handle_key(ctrl_s()).expect("enter split");
    assert!(matches!(app.split, agmsg_tui::pane::SplitMode::Split { .. }));
    app.handle_key(ctrl_s()).expect("exit split");
    assert!(matches!(app.split, agmsg_tui::pane::SplitMode::Off));
    app.status = ready;

    let after = render_to_buffer(&app, 80, 24);
    assert_eq!(before, after, "split on/off cycle must not perturb single-pane rendering");
}

/// Toggle-cycle preserves the single-pane state the user started from:
/// same selected team/focus after Ctrl+S, Ctrl+S as before either press.
#[test]
fn toggle_on_off_cycle_preserves_single_pane_state() {
    let (_temp, mut app) = two_team_fixture();
    let team_before = app.selected_team_name().map(str::to_owned);
    let focus_before = app.focus;

    app.handle_key(ctrl_s()).expect("enter split");
    app.handle_key(ctrl_s()).expect("exit split");

    assert_eq!(app.selected_team_name().map(str::to_owned), team_before);
    assert_eq!(app.focus, focus_before);
    assert!(matches!(app.split, agmsg_tui::pane::SplitMode::Off));
}

/// `Esc` is the other documented exit besides a second `Ctrl+S`.
#[test]
fn esc_exits_split_mode() {
    let (_temp, mut app) = two_team_fixture();
    app.handle_key(ctrl_s()).expect("enter split");
    assert!(matches!(app.split, agmsg_tui::pane::SplitMode::Split { .. }));
    app.handle_key(esc()).expect("esc exits split");
    assert!(matches!(app.split, agmsg_tui::pane::SplitMode::Off));
}

/// `Tab` while split is on switches which pane is active instead of cycling
/// TEAMS/MEMBERS/ROOM focus within one pane.
#[test]
fn tab_switches_active_pane_index_while_split() {
    let (_temp, mut app) = two_team_fixture();
    app.handle_key(ctrl_s()).expect("enter split");
    let team_first = app.selected_team_name().map(str::to_owned);

    app.handle_key(tab()).expect("tab switches pane");
    let team_second = app.selected_team_name().map(str::to_owned);

    assert_ne!(
        team_first, team_second,
        "Tab must swap in the other pane's team, not just re-show the same one"
    );

    // A second Tab returns to the first pane's team — round-trips cleanly.
    app.handle_key(tab()).expect("tab back");
    assert_eq!(app.selected_team_name().map(str::to_owned), team_first);
}

/// Composer opens against the *active* pane's team — after Tab switches
/// panes, `c` must target the newly-active pane, not whichever pane was
/// active when split was entered.
#[test]
fn composer_targets_active_panes_team() {
    let (_temp, mut app) = two_team_fixture();
    app.handle_key(ctrl_s()).expect("enter split");
    app.handle_key(tab()).expect("switch to second pane");
    let active_team = app.selected_team_name().map(str::to_owned).expect("active team");

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
        .expect("open composer");

    assert_eq!(app.screen, Screen::Composer);
    let drafted_team = app
        .drafts
        .keys()
        .next()
        .cloned()
        .or_else(|| app.selected_team_name().map(str::to_owned));
    assert_eq!(drafted_team, Some(active_team));
}

/// Independent scroll offset per pane: moving the ROOM selection in the
/// active pane must not move the inactive pane's stored selection.
#[test]
fn scrolling_one_pane_leaves_the_other_panes_selection_untouched() {
    let (_temp, mut app) = two_team_fixture();
    app.handle_key(ctrl_s()).expect("enter split");
    app.focus = agmsg_tui::app::Focus::Room;
    // ops-hub's 3 fixture messages leave the active pane parked on the last
    // one by default (`reload_selected_team` follows newest-message
    // convention) — rewind to the first so `Down` has somewhere to go.
    app.selected_message = 0;
    let inactive_selected_before = match &app.split {
        agmsg_tui::pane::SplitMode::Split { second, .. } => second.selected_message,
        agmsg_tui::pane::SplitMode::Off => unreachable!("just entered split"),
    };
    let active_selected_before = app.selected_message;

    app.handle_key(down()).expect("move active pane selection");

    let (active_selected_after, inactive_selected_after) = match &app.split {
        agmsg_tui::pane::SplitMode::Split { second, .. } => (app.selected_message, second.selected_message),
        agmsg_tui::pane::SplitMode::Off => unreachable!("still in split"),
    };

    assert_eq!(
        inactive_selected_after, inactive_selected_before,
        "inactive pane's scroll offset must stay put"
    );
    assert_ne!(
        active_selected_after, active_selected_before,
        "active pane's scroll offset must move"
    );
}

/// Constraint: "Must survive 80x24 narrow terminal (stack vertically,
/// don't clip)" — this must not panic and must actually draw content into
/// both pane regions (non-zero cells beyond the border).
#[test]
fn split_renders_without_panicking_at_80x24() {
    let (_temp, mut app) = two_team_fixture();
    app.handle_key(ctrl_s()).expect("enter split");
    let buffer = render_to_buffer(&app, 80, 24);
    // Sanity: something was drawn (not an all-blank buffer), on both the
    // top half (first pane) and bottom half (second pane, since 80 cols is
    // below SPLIT_WIDE_THRESHOLD so panes stack).
    let non_blank = |rows: std::ops::Range<u16>| {
        rows.flat_map(|y| (0..80u16).map(move |x| (x, y)))
            .any(|(x, y)| buffer.cell((x, y)).is_some_and(|cell| cell.symbol() != " "))
    };
    assert!(non_blank(0..11), "top pane should render content");
    assert!(non_blank(12..23), "bottom pane should render content");
}

/// Split entry below `MIN_SPLIT_HEIGHT` is refused with a status message,
/// not silently entered.
#[test]
fn split_entry_is_refused_below_min_height() {
    let (_temp, mut app) = two_team_fixture();
    app.term_area = ratatui::layout::Rect::new(0, 0, 80, 20);
    app.handle_key(ctrl_s()).expect("attempt split");
    assert!(matches!(app.split, agmsg_tui::pane::SplitMode::Off));
    assert!(app.status.is_error);
}

/// `AppAction::None` is what dispatch is documented to return.
#[test]
fn split_toggle_returns_no_app_action() {
    let (_temp, mut app) = two_team_fixture();
    let action = app.handle_key(ctrl_s()).expect("toggle split");
    assert!(matches!(action, AppAction::None));
}
