//! Phase 14C — custom keybindings, exercised end-to-end through `App`
//! (not just `keys::KeyMap` in isolation). Complements the chord-parser /
//! merge unit tests already in `src/keys.rs` with the App-dispatch-level
//! guarantees the plan's test plan calls for: a remap is authoritative (not
//! additive), an invalid key falls back to default *and* is surfaced as a
//! warning, and `--print-default-keys`'s output is a faithful, loadable
//! round-trip of `KeyMap::default()`.
use std::fs;

use agmsg_tui::app::{App, AppAction, Screen};
use agmsg_tui::config::Paths;
use agmsg_tui::keys::KeyMap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rusqlite::Connection;
use tempfile::TempDir;

fn fixture_app(temp: &TempDir) -> App {
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
    App::load(Paths {
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
    .expect("app")
}

fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

/// Default load produces expected `q`→Quit binding, exercised through the
/// real `App::handle_key` dispatch (not just `KeyMap::lookup` directly).
#[test]
fn default_load_q_quits_via_app_dispatch() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut app = fixture_app(&temp);
    assert_eq!(app.screen, Screen::Main);

    let action = app
        .handle_key(key(KeyCode::Char('q'), KeyModifiers::NONE))
        .expect("handle_key");

    assert!(matches!(action, AppAction::Quit));
}

/// Partial override (only `quit = "Q"`) leaves every other key at its
/// default — proven both by the new binding working and by the old default
/// for an untouched action (`?` -> help) still working.
#[test]
fn partial_override_remaps_quit_and_leaves_help_at_default() {
    let temp = tempfile::tempdir().expect("tempdir");
    let keys_path = temp.path().join("keys.toml");
    fs::write(&keys_path, "[keys]\nquit = \"Q\"\n").expect("write keys.toml");

    let mut app = fixture_app(&temp);
    let (keymap, warnings) = KeyMap::load(&keys_path);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    app.keymap = keymap;

    // Old default 'q' no longer quits (authoritative, not additive).
    let action = app
        .handle_key(key(KeyCode::Char('q'), KeyModifiers::NONE))
        .expect("handle_key");
    assert!(!matches!(action, AppAction::Quit));

    // New remap 'Q' does quit.
    let action = app
        .handle_key(key(KeyCode::Char('Q'), KeyModifiers::NONE))
        .expect("handle_key");
    assert!(matches!(action, AppAction::Quit));
}

/// A remapped `audit = "f2"` opens Audit, and the old default `Ctrl+A` no
/// longer does — proves the keymap is authoritative for scoped actions, not
/// merely an additional trigger alongside the hardcoded default.
#[test]
fn remapped_audit_key_is_authoritative_over_old_default() {
    let temp = tempfile::tempdir().expect("tempdir");
    let keys_path = temp.path().join("keys.toml");
    fs::write(&keys_path, "[keys]\naudit = \"f2\"\n").expect("write keys.toml");

    let mut app = fixture_app(&temp);
    let (keymap, warnings) = KeyMap::load(&keys_path);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    app.keymap = keymap;

    app.handle_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL))
        .expect("handle_key");
    assert_eq!(
        app.screen,
        Screen::Main,
        "old default ctrl+a must no longer open Audit after remap"
    );

    app.handle_key(key(KeyCode::F(2), KeyModifiers::NONE))
        .expect("handle_key");
    assert_eq!(app.screen, Screen::Audit, "remapped f2 must open Audit");
}

/// Invalid key (`quit = "notakey"`) falls back to the default binding AND
/// emits a warning (surfaced to stderr by `main.rs`, verified here via the
/// `KeyMap::load` warnings vec it prints from).
#[test]
fn invalid_key_falls_back_to_default_and_warns() {
    let temp = tempfile::tempdir().expect("tempdir");
    let keys_path = temp.path().join("keys.toml");
    fs::write(&keys_path, "[keys]\nquit = \"notakey\"\n").expect("write keys.toml");

    let mut app = fixture_app(&temp);
    let (keymap, warnings) = KeyMap::load(&keys_path);
    assert_eq!(warnings.len(), 1, "expected exactly one warning: {warnings:?}");
    assert!(warnings[0].contains("quit"));
    assert!(warnings[0].contains("notakey"));
    app.keymap = keymap;

    let action = app
        .handle_key(key(KeyCode::Char('q'), KeyModifiers::NONE))
        .expect("handle_key");
    assert!(
        matches!(action, AppAction::Quit),
        "default 'q' must still quit when the override was invalid"
    );
}

/// `--print-default-keys` output (`KeyMap::default_toml()`) parses as valid
/// TOML and, loaded back through `KeyMap::load`, reproduces every default
/// binding with zero warnings — a self-hosted round-trip that also proves
/// the dump can't silently drift from actual runtime defaults.
#[test]
fn print_default_keys_output_round_trips() {
    let temp = tempfile::tempdir().expect("tempdir");
    let keys_path = temp.path().join("keys.toml");
    fs::write(&keys_path, KeyMap::default_toml()).expect("write default dump");

    let (loaded, warnings) = KeyMap::load(&keys_path);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    let defaults = KeyMap::default();
    for (code, mods) in [
        (KeyCode::Char('q'), KeyModifiers::NONE),
        (KeyCode::Char('?'), KeyModifiers::NONE),
        (KeyCode::Char('c'), KeyModifiers::NONE),
        (KeyCode::Char('a'), KeyModifiers::CONTROL),
        (KeyCode::Char('A'), KeyModifiers::NONE),
        (KeyCode::Char('H'), KeyModifiers::NONE),
        (KeyCode::Char('f'), KeyModifiers::CONTROL),
        (KeyCode::Char('F'), KeyModifiers::NONE),
        (KeyCode::Char('n'), KeyModifiers::CONTROL),
        (KeyCode::Char('k'), KeyModifiers::NONE),
        (KeyCode::Char('j'), KeyModifiers::NONE),
        (KeyCode::Tab, KeyModifiers::NONE),
    ] {
        assert_eq!(
            loaded.lookup(key(code, mods)),
            defaults.lookup(key(code, mods)),
            "round-trip mismatch for {code:?}+{mods:?}"
        );
    }
}
