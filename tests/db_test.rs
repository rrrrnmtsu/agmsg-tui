use std::fs;
use std::path::PathBuf;

use agmsg_tui::db::Database;
use rusqlite::{Connection, params};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    db_path: PathBuf,
    teams_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("messages.db");
        let teams_dir = temp.path().join("teams");
        fs::create_dir_all(&teams_dir).expect("teams dir");
        let connection = Connection::open(&db_path).expect("fixture db");
        connection
            .execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    team TEXT NOT NULL,
                    from_agent TEXT NOT NULL,
                    to_agent TEXT NOT NULL,
                    body TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
                    read_at TEXT
                );
                CREATE INDEX idx_unread ON messages(team, to_agent, read_at)
                    WHERE read_at IS NULL;
                CREATE INDEX idx_history ON messages(team, created_at DESC);",
            )
            .expect("schema");
        drop(connection);
        Self {
            _temp: temp,
            db_path,
            teams_dir,
        }
    }

    fn database(&self) -> Database {
        Database::new(&self.db_path)
    }

    fn add_team(&self, team: &str, members: &[&str]) {
        let team_dir = self.teams_dir.join(team);
        fs::create_dir_all(&team_dir).expect("team dir");
        let agents = members
            .iter()
            .map(|member| format!(r#""{member}":{{}}"#))
            .collect::<Vec<_>>()
            .join(",");
        fs::write(
            team_dir.join("config.json"),
            format!(r#"{{"name":"{team}","agents":{{{agents}}}}}"#),
        )
        .expect("config");
    }

    fn insert(
        &self,
        team: &str,
        from: &str,
        to: &str,
        body: &str,
        created_at: &str,
        read_at: Option<&str>,
    ) -> i64 {
        let connection = Connection::open(&self.db_path).expect("fixture db");
        connection
            .execute(
                "INSERT INTO messages
                    (team, from_agent, to_agent, body, created_at, read_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![team, from, to, body, created_at, read_at],
            )
            .expect("insert");
        connection.last_insert_rowid()
    }
}

#[test]
fn team_names_are_union_of_directory_and_database() {
    let fixture = Fixture::new();
    fixture.add_team("config-only", &["codex"]);
    fixture.insert(
        "db-only",
        "claude",
        "codex",
        "hello",
        "2026-07-20T00:00:00Z",
        None,
    );

    let teams = fixture
        .database()
        .team_names(&fixture.teams_dir)
        .expect("teams");
    assert_eq!(teams, vec!["config-only", "db-only"]);
}

#[test]
fn team_names_are_sorted_and_deduplicated() {
    let fixture = Fixture::new();
    fixture.add_team("z-team", &["codex"]);
    fixture.add_team("a-team", &["codex"]);
    fixture.insert(
        "z-team",
        "codex",
        "codex",
        "same team",
        "2026-07-20T00:00:00Z",
        Some("2026-07-20T00:01:00Z"),
    );

    let teams = fixture
        .database()
        .team_names(&fixture.teams_dir)
        .expect("teams");
    assert_eq!(teams, vec!["a-team", "z-team"]);
}

#[test]
fn history_is_chronological_and_limited() {
    let fixture = Fixture::new();
    for index in 0..4 {
        fixture.insert(
            "ops",
            "claude",
            "codex",
            &format!("message-{index}"),
            &format!("2026-07-20T00:00:0{index}Z"),
            None,
        );
    }

    let page = fixture.database().history("ops", None, 3).expect("history");
    let bodies: Vec<&str> = page
        .messages
        .iter()
        .map(|message| message.body.as_str())
        .collect();
    assert_eq!(bodies, vec!["message-1", "message-2", "message-3"]);
    assert!(page.has_more);
}

#[test]
fn history_cursor_loads_only_older_messages() {
    let fixture = Fixture::new();
    let first = fixture.insert("ops", "a", "b", "first", "2026-07-20T00:00:00Z", None);
    let second = fixture.insert("ops", "a", "b", "second", "2026-07-20T00:00:01Z", None);
    fixture.insert("ops", "a", "b", "third", "2026-07-20T00:00:02Z", None);

    let page = fixture
        .database()
        .history("ops", Some(second + 1), 10)
        .expect("history");
    assert_eq!(
        page.messages
            .iter()
            .map(|message| message.id)
            .collect::<Vec<_>>(),
        vec![first, second]
    );
    assert!(!page.has_more);
}

#[test]
fn unread_counts_are_grouped_by_team() {
    let fixture = Fixture::new();
    fixture.insert("a", "x", "y", "1", "2026-07-20T00:00:00Z", None);
    fixture.insert("a", "x", "y", "2", "2026-07-20T00:00:01Z", None);
    fixture.insert(
        "a",
        "x",
        "y",
        "read",
        "2026-07-20T00:00:02Z",
        Some("2026-07-20T00:01:00Z"),
    );
    fixture.insert("b", "x", "y", "3", "2026-07-20T00:00:03Z", None);

    let counts = fixture.database().unread_counts().expect("unread");
    assert_eq!(counts.get("a"), Some(&2));
    assert_eq!(counts.get("b"), Some(&1));
}

#[test]
fn unread_recipients_are_unique_and_sorted() {
    let fixture = Fixture::new();
    fixture.insert("ops", "x", "z", "1", "2026-07-20T00:00:00Z", None);
    fixture.insert("ops", "x", "a", "2", "2026-07-20T00:00:01Z", None);
    fixture.insert("ops", "x", "z", "3", "2026-07-20T00:00:02Z", None);

    let recipients = fixture
        .database()
        .unread_recipients("ops")
        .expect("recipients");
    assert_eq!(recipients, vec!["a", "z"]);
}

#[test]
fn new_messages_respect_last_seen_id() {
    let fixture = Fixture::new();
    let first = fixture.insert("ops", "a", "b", "old", "2026-07-20T00:00:00Z", None);
    let second = fixture.insert("ops", "a", "b", "new-1", "2026-07-20T00:00:01Z", None);
    let third = fixture.insert("ops", "b", "a", "new-2", "2026-07-20T00:00:02Z", None);

    let messages = fixture
        .database()
        .new_messages_for_team("ops", first)
        .expect("new messages");
    assert_eq!(
        messages
            .iter()
            .map(|message| message.id)
            .collect::<Vec<_>>(),
        vec![second, third]
    );
}

#[test]
fn new_messages_do_not_cross_team_boundary() {
    let fixture = Fixture::new();
    fixture.insert("a", "x", "y", "a", "2026-07-20T00:00:00Z", None);
    fixture.insert("b", "x", "y", "b", "2026-07-20T00:00:01Z", None);

    let messages = fixture
        .database()
        .new_messages_for_team("a", 0)
        .expect("new messages");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].team, "a");
}

#[test]
fn agent_inventory_flattens_registrations_and_isolates_broken_config() {
    let fixture = Fixture::new();
    let good = fixture.teams_dir.join("ops-hub");
    let broken = fixture.teams_dir.join("broken-team");
    fs::create_dir_all(&good).expect("good team");
    fs::create_dir_all(&broken).expect("broken team");
    fs::write(
        good.join("config.json"),
        r#"{"name":"ops-hub","agents":{"codex-worker":{"registrations":[{"type":"codex","project":"/tmp/one"},{"type":"codex","project":"/tmp/two"}]}}}"#,
    )
    .expect("good config");
    fs::write(broken.join("config.json"), "{").expect("broken config");
    fixture.insert(
        "ops-hub",
        "codex-worker",
        "claude-main",
        "hello",
        "2999-01-01T00:00:00Z",
        None,
    );

    let inventory = fixture
        .database()
        .agent_inventory(&fixture.teams_dir)
        .expect("inventory");
    let broken = inventory
        .iter()
        .find(|team| team.name == "broken-team")
        .expect("broken team row");
    assert!(broken.broken_config.is_some());
    let good = inventory
        .iter()
        .find(|team| team.name == "ops-hub")
        .expect("good team row");
    assert_eq!(good.identities.len(), 2, "one row per registration");
    assert_eq!(good.identities[0].name, "codex-worker");
    assert_eq!(good.identities[0].sent_30d, 1);
    assert_eq!(good.identities[0].last_seen_at.as_deref(), Some("2999-01-01T00:00:00Z"));
}

#[test]
fn roster_and_member_summary_come_from_config_and_messages() {
    let fixture = Fixture::new();
    fixture.add_team("ops", &["claude", "codex"]);
    fixture.insert(
        "ops",
        "claude",
        "codex",
        "hello",
        "2026-07-20T10:15:00Z",
        None,
    );

    let members = fixture
        .database()
        .member_summaries(&fixture.teams_dir, "ops")
        .expect("members");
    assert_eq!(members.len(), 2);
    let codex = members
        .iter()
        .find(|member| member.name == "codex")
        .expect("codex");
    assert_eq!(codex.unread_count, 1);
    assert_eq!(
        codex.last_message_at.as_deref(),
        Some("2026-07-20T10:15:00Z")
    );
}

#[test]
fn schema_validation_rejects_missing_columns() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("bad.db");
    Connection::open(&path)
        .expect("db")
        .execute("CREATE TABLE messages (id INTEGER PRIMARY KEY)", [])
        .expect("schema");
    let error = Database::new(path)
        .validate_schema()
        .expect_err("invalid schema");
    assert!(error.to_string().contains("スキーマ"));
}

#[test]
fn pair_matrix_uses_window_and_flags_twenty_percent_asymmetry() {
    let fixture = Fixture::new();
    fixture.insert("ops", "a", "b", "1", "2999-01-01T00:00:00Z", None);
    fixture.insert("ops", "a", "b", "2", "2999-01-01T00:00:01Z", None);
    fixture.insert("ops", "b", "a", "3", "2999-01-01T00:00:02Z", None);
    fixture.insert("ops", "a", "b", "old", "2000-01-01T00:00:00Z", None);

    let matrices = fixture.database().pair_matrices(30).expect("matrices");
    assert_eq!(matrices.len(), 1);
    assert_eq!(matrices[0].count("a", "b"), 2);
    assert_eq!(matrices[0].count("b", "a"), 1);
    assert!(matrices[0].is_asymmetric("a", "b"));
}

#[test]
fn zombie_identities_are_roster_members_without_window_traffic() {
    let fixture = Fixture::new();
    fixture.add_team("ops", &["active", "zombie"]);
    fixture.insert(
        "ops",
        "active",
        "someone",
        "recent",
        "2999-01-01T00:00:00Z",
        None,
    );

    let zombies = fixture
        .database()
        .zombie_identities(&fixture.teams_dir, 30)
        .expect("zombies");
    assert_eq!(zombies.len(), 1);
    assert_eq!(zombies[0].agent, "zombie");
}

#[test]
fn stale_unreads_exclude_recent_and_read_messages() {
    let fixture = Fixture::new();
    let stale_id = fixture.insert("ops", "a", "b", "stale", "2000-01-01T00:00:00Z", None);
    fixture.insert("ops", "a", "b", "future", "2999-01-01T00:00:00Z", None);
    fixture.insert(
        "ops",
        "a",
        "b",
        "already read",
        "2000-01-01T00:00:01Z",
        Some("2000-01-02T00:00:00Z"),
    );

    let stale = fixture.database().stale_unreads(3).expect("stale");
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].id, stale_id);
}
