//! Mouse focus-switching and resize-drag logic, exercised without a real
//! terminal (App::handle_mouse only needs a Rect and a synthetic MouseEvent).
use std::fs;

use agmsg_tui::app::{App, Focus};
use agmsg_tui::config::Paths;
use agmsg_tui::ui::{SIDEBAR_MAX_PCT, SIDEBAR_MIN_PCT, compute_layout, resize_pct_from_column};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use rusqlite::Connection;
use tempfile::TempDir;

fn fixture_app() -> (TempDir, App) {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("messages.db");
    Connection::open(&db)
        .expect("db")
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
    fs::create_dir_all(teams_dir.join("ops")).expect("team dir");
    fs::write(
        teams_dir.join("ops/config.json"),
        r#"{"agents":{"claude-main":{},"codex-worker":{}}}"#,
    )
    .expect("config");
    let app = App::load(Paths {
        db,
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
    (temp, app)
}

fn left_down(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn left_drag(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn left_click_on_room_pane_switches_focus_from_teams() {
    let (_temp, mut app) = fixture_app();
    assert_eq!(app.focus, Focus::Teams);
    let area = Rect::new(0, 0, 100, 30);
    let layout = compute_layout(area, app.sidebar_pct, app.focus);
    // A point comfortably inside the room pane (well clear of the sidebar
    // boundary, so this can't accidentally land on the resize handle).
    let click_column = layout.room.x + layout.room.width / 2;
    let click_row = layout.room.y + 1;

    app.handle_mouse(left_down(click_column, click_row), area);

    assert_eq!(app.focus, Focus::Room);
}

#[test]
fn left_click_on_members_pane_switches_focus() {
    let (_temp, mut app) = fixture_app();
    let area = Rect::new(0, 0, 100, 30);
    let layout = compute_layout(area, app.sidebar_pct, app.focus);
    let click_column = layout.members.x + 1;
    let click_row = layout.members.y + 1;

    app.handle_mouse(left_down(click_column, click_row), area);

    assert_eq!(app.focus, Focus::Members);
}

#[test]
fn dragging_the_resize_handle_updates_sidebar_pct_within_bounds() {
    let (_temp, mut app) = fixture_app();
    let area = Rect::new(0, 0, 100, 30);
    let layout = compute_layout(area, app.sidebar_pct, app.focus);
    let handle_column = layout.sidebar.x + layout.sidebar.width - 1;
    let handle_row = layout.sidebar.y + 1;

    app.handle_mouse(left_down(handle_column, handle_row), area);
    assert!(app.resize_dragging, "down on the handle should start a drag");

    // Drag far to the right — must clamp at SIDEBAR_MAX_PCT, never runaway.
    app.handle_mouse(left_drag(90, handle_row), area);
    assert_eq!(app.sidebar_pct, SIDEBAR_MAX_PCT);

    // Drag far to the left — must clamp at SIDEBAR_MIN_PCT.
    app.handle_mouse(left_drag(2, handle_row), area);
    assert_eq!(app.sidebar_pct, SIDEBAR_MIN_PCT);
}

#[test]
fn resize_pct_from_column_clamps_at_both_bounds() {
    let area = Rect::new(0, 0, 100, 30);
    assert_eq!(resize_pct_from_column(area, 0), SIDEBAR_MIN_PCT);
    assert_eq!(resize_pct_from_column(area, 200), SIDEBAR_MAX_PCT);
    assert_eq!(resize_pct_from_column(area, 45), 45);
}

#[test]
fn drag_without_a_prior_down_on_the_handle_is_ignored() {
    let (_temp, mut app) = fixture_app();
    let area = Rect::new(0, 0, 100, 30);
    let starting_pct = app.sidebar_pct;

    // No Down(Left) preceded this, so resize_dragging is still false and the
    // drag must be a no-op — otherwise any mouse-move-while-held elsewhere
    // on screen would silently resize the sidebar.
    app.handle_mouse(left_drag(90, 5), area);

    assert_eq!(app.sidebar_pct, starting_pct);
    assert!(!app.resize_dragging);
}
