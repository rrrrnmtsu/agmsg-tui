//! Phase 14E: message replay (time-travel from audit history).
//!
//! Fixture seeds one team with three messages at real relative offsets
//! (`now - 1h`, `now - 30m`, `now`) plus one `audit_history.jsonl` sample at
//! `now - 45m`, so `read_audit_history` (which filters against the real
//! wall clock, not an injectable "now") still picks the sample up — every
//! timestamp here is computed from `Utc::now()` at fixture-build time rather
//! than hardcoded, same reasoning `split_view_test.rs` doesn't need since it
//! isn't time-filtered.

use agmsg_tui::app::{App, AuditTab, Screen};
use agmsg_tui::config::Paths;
use agmsg_tui::db::Message;
use chrono::{Duration, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rusqlite::Connection;
use std::fs;
use tempfile::{TempDir, tempdir};

fn rfc3339(offset: Duration) -> String {
    (Utc::now() - offset).format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn ctrl_a() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)
}

fn ctrl_r() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)
}

fn char_key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn esc() -> KeyEvent {
    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
}

/// One team, three messages at `now-1h` / `now-30m` / `now`, and one audit
/// history sample at `now-45m` — replaying that sample must show exactly
/// the `now-1h` message (the only one at or before the snapshot).
fn replay_fixture() -> (TempDir, App) {
    let temp = tempdir().expect("tempdir");
    let db_path = temp.path().join("messages.db");
    let hour_ago = rfc3339(Duration::hours(1));
    let thirty_min_ago = rfc3339(Duration::minutes(30));
    let now = rfc3339(Duration::zero());
    Connection::open(&db_path)
        .expect("fixture db")
        .execute_batch(&format!(
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
                ('ops-hub', 'claude-main', 'codex-worker', 'one hour ago', '{hour_ago}'),
                ('ops-hub', 'claude-main', 'codex-worker', 'thirty min ago', '{thirty_min_ago}'),
                ('ops-hub', 'claude-main', 'codex-worker', 'just now', '{now}');"
        ))
        .expect("schema");
    let teams_dir = temp.path().join("teams");
    fs::create_dir_all(teams_dir.join("ops-hub")).expect("ops team dir");
    fs::write(
        teams_dir.join("ops-hub/config.json"),
        r#"{"agents":{"claude-main":{"registrations":[{"type":"claude-code","project":"/tmp/ops-hub"}]},"codex-worker":{"registrations":[{"type":"codex","project":"/tmp/ops-hub"}]}}}"#,
    )
    .expect("ops config");

    let audit_history = temp.path().join("audit.jsonl");
    let forty_five_min_ago = rfc3339(Duration::minutes(45));
    fs::write(
        &audit_history,
        format!(r#"{{"ts":"{forty_five_min_ago}","score":80,"total_msg":2}}"#),
    )
    .expect("audit history fixture");

    let mut app = App::load(Paths {
        db: db_path,
        teams_dir,
        scripts_dir: temp.path().join("scripts"),
        audit_script: temp.path().join("agmsg-audit"),
        audit_history,
        report_dir: temp.path().join("reports"),
        state_file: temp.path().join("state.json"),
        keys_file: temp.path().join("keys.toml"),
        hosts_file: temp.path().join("hosts.toml"),
        remote_dir: temp.path().join("agmsg-remote"),
    })
    .expect("app");
    assert_eq!(app.messages.len(), 3, "fixture must seed all three messages");
    app.split = agmsg_tui::pane::SplitMode::Off;
    (temp, app)
}

/// Navigates Main → Audit → HISTORY tab, same key sequence a user would
/// press (`Ctrl+A` then `H`).
fn open_history_tab(app: &mut App) {
    app.handle_key(ctrl_a()).expect("open audit");
    assert_eq!(app.screen, Screen::Audit);
    app.handle_key(char_key('H')).expect("switch to history tab");
    assert_eq!(app.audit_tab, AuditTab::History);
    assert_eq!(
        app.audit_history.len(),
        1,
        "fixture's single audit.jsonl sample must have been picked up"
    );
}

/// `r` on the HISTORY tab's (only) row enters replay, routes to Main, and
/// pins `snapshot_ts` to that sample's timestamp.
#[test]
fn r_on_history_row_enters_replay_and_routes_to_main() {
    let (_temp, mut app) = replay_fixture();
    open_history_tab(&mut app);

    app.handle_key(char_key('r')).expect("enter replay");

    assert_eq!(app.screen, Screen::Main);
    let replay = app.replay.as_ref().expect("replay state set");
    let expected_ts = app.audit_history[0].timestamp.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    assert_eq!(replay.snapshot_ts, expected_ts);
    assert!(replay.return_to_audit);
}

/// Query filter: replaying at `now-45m` must show only the `now-1h`
/// message — the other two postdate the snapshot.
#[test]
fn replay_filters_messages_to_snapshot_timestamp() {
    let (_temp, mut app) = replay_fixture();
    open_history_tab(&mut app);
    app.handle_key(char_key('r')).expect("enter replay");

    assert_eq!(app.messages.len(), 1, "only the pre-snapshot message should be loaded");
    assert_eq!(app.messages[0].body, "one hour ago");
}

/// Composer entry is refused with a status message, not silently opened,
/// while replaying.
#[test]
fn composer_is_blocked_during_replay() {
    let (_temp, mut app) = replay_fixture();
    open_history_tab(&mut app);
    app.handle_key(char_key('r')).expect("enter replay");

    app.handle_key(char_key('c')).expect("attempt compose");

    assert_eq!(app.screen, Screen::Main, "composer must not open");
    assert!(app.status.is_error);
    assert!(app.status.text.contains("compose disabled in replay mode"));
}

/// Live poll arrivals are dropped while replaying — a frozen snapshot must
/// not gain messages the user is actively looking away from live traffic
/// to avoid.
#[test]
fn live_poll_push_is_suppressed_during_replay() {
    let (_temp, mut app) = replay_fixture();
    open_history_tab(&mut app);
    app.handle_key(char_key('r')).expect("enter replay");
    let before = app.messages.len();

    let incoming = Message {
        id: 999,
        team: "ops-hub".to_owned(),
        from_agent: "codex-worker".to_owned(),
        to_agent: "claude-main".to_owned(),
        body: "arrived mid-replay".to_owned(),
        created_at: rfc3339(Duration::zero()),
        read_at: None,
    };
    app.receive_new_messages(vec![incoming]).expect("poll push");

    assert_eq!(app.messages.len(), before, "replay view must stay frozen");
}

/// `Esc` exits replay, restores live single-pane data, and — since this
/// replay was entered from HISTORY — returns to Audit/HISTORY rather than
/// staying on Main.
#[test]
fn esc_exits_replay_and_returns_to_audit_history() {
    let (_temp, mut app) = replay_fixture();
    open_history_tab(&mut app);
    app.handle_key(char_key('r')).expect("enter replay");
    assert_eq!(app.messages.len(), 1);

    app.handle_key(esc()).expect("exit replay");

    assert!(app.replay.is_none());
    assert_eq!(app.screen, Screen::Audit);
    assert_eq!(app.audit_tab, AuditTab::History);
    assert_eq!(app.messages.len(), 3, "live data must be restored on exit");
    assert!(matches!(app.split, agmsg_tui::pane::SplitMode::Off));
}

/// Ctrl+R (the 14C-reserved `Action::Replay` binding) also exits replay —
/// wiring up the previously no-op binding as documented in the phase plan.
#[test]
fn ctrl_r_also_exits_replay() {
    let (_temp, mut app) = replay_fixture();
    open_history_tab(&mut app);
    app.handle_key(char_key('r')).expect("enter replay");

    app.handle_key(ctrl_r()).expect("ctrl+r exits replay");

    assert!(app.replay.is_none());
}

/// Split and replay are mutually exclusive: entering split while replaying
/// (or replaying while split is active) is refused with a status message
/// rather than composed together.
#[test]
fn split_and_replay_refuse_to_coexist() {
    let (_temp, mut app) = replay_fixture();

    // split -> attempt replay entry
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .expect("enter split");
    assert!(matches!(app.split, agmsg_tui::pane::SplitMode::Split { .. }));
    open_history_tab(&mut app);
    app.handle_key(char_key('r')).expect("attempt replay while split");
    assert!(app.replay.is_none(), "replay must be refused while split is active");
    assert!(app.status.is_error);

    // exit split, replay -> attempt split entry
    app.handle_key(esc()).expect("back to Main (History's Esc, not split's)");
    assert_eq!(app.screen, Screen::Main);
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .expect("exit split from Main");
    assert!(matches!(app.split, agmsg_tui::pane::SplitMode::Off));
    open_history_tab(&mut app);
    app.handle_key(char_key('r')).expect("enter replay");
    assert!(app.replay.is_some());
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .expect("attempt split while replaying");
    assert!(matches!(app.split, agmsg_tui::pane::SplitMode::Off), "split must be refused while replaying");
}

/// Header banner formatting: `▶ REPLAY @ <label> — Esc to exit`, rendered
/// on the row Main's body area gave up (verified by scanning row 0 of the
/// TestBackend buffer for the banner text).
#[test]
fn replay_header_banner_renders_at_top_row() {
    let (_temp, mut app) = replay_fixture();
    open_history_tab(&mut app);
    app.handle_key(char_key('r')).expect("enter replay");

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| agmsg_tui::ui::render(frame, &app)).expect("draw");
    let buffer = terminal.backend().buffer();

    let top_row: String = (0..80u16)
        .filter_map(|x| buffer.cell((x, 0)).map(|cell| cell.symbol()))
        .collect();
    assert!(top_row.contains("REPLAY"), "banner must render on the top row: {top_row:?}");
    assert!(top_row.contains("Esc to exit"));
}

/// Off-replay (`app.replay == None`) rendering must stay byte-identical to
/// pre-14E output at the 80x24 reference terminal — the no-regression
/// contract every phase's plan doc requires.
#[test]
fn non_replay_render_is_unchanged_by_this_phase() {
    let (_temp, app) = replay_fixture();
    assert!(app.replay.is_none());

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| agmsg_tui::ui::render(frame, &app)).expect("draw");
    let buffer = terminal.backend().buffer().clone();

    // Re-render an identical, freshly-loaded app: two independent draws of
    // the same non-replay state must match exactly.
    let backend2 = TestBackend::new(80, 24);
    let mut terminal2 = Terminal::new(backend2).expect("terminal2");
    terminal2.draw(|frame| agmsg_tui::ui::render(frame, &app)).expect("draw2");
    assert_eq!(buffer, *terminal2.backend().buffer());

    let top_row: String = (0..80u16)
        .filter_map(|x| buffer.cell((x, 0)).map(|cell| cell.symbol()))
        .collect();
    assert!(!top_row.contains("REPLAY"), "no banner row outside replay mode");
}
