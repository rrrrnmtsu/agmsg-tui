use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Duration as ChronoDuration, NaiveDate, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Deserialize;
use serde_json::Value;

const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);
pub const ANALYTICS_WINDOW_DAYS: u32 = 30;
pub const STALE_UNREAD_DAYS: u32 = 3;
pub const PAIR_ASYMMETRY_TOLERANCE: f64 = 0.20;
pub const DEFAULT_BURST_THRESHOLD: usize = 150;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DailyTraffic {
    pub date: NaiveDate,
    pub count: usize,
    pub burst: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTraffic {
    pub agent: String,
    pub sent: usize,
    pub received: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: i64,
    pub team: String,
    pub from_agent: String,
    pub to_agent: String,
    pub body: String,
    pub created_at: String,
    pub read_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamSummary {
    pub name: String,
    pub unread_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberSummary {
    pub name: String,
    pub last_message_at: Option<String>,
    pub unread_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryPage {
    pub messages: Vec<Message>,
    pub has_more: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairMatrix {
    pub team: String,
    pub agents: Vec<String>,
    pub counts: BTreeMap<(String, String), usize>,
}

impl PairMatrix {
    pub fn count(&self, from: &str, to: &str) -> usize {
        self.counts
            .get(&(from.to_owned(), to.to_owned()))
            .copied()
            .unwrap_or_default()
    }

    pub fn is_asymmetric(&self, from: &str, to: &str) -> bool {
        if from == to {
            return false;
        }
        let forward = self.count(from, to);
        let reverse = self.count(to, from);
        let maximum = forward.max(reverse);
        maximum > 0 && forward.abs_diff(reverse) as f64 / maximum as f64 > PAIR_ASYMMETRY_TOLERANCE
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZombieIdentity {
    pub team: String,
    pub agent: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleUnread {
    pub id: i64,
    pub team: String,
    pub from_agent: String,
    pub to_agent: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
struct TeamConfig {
    #[serde(default)]
    agents: BTreeMap<String, Value>,
}

/// First `registrations[0]` entry for an agent in a team's `config.json`,
/// used by the MEMBER info popup (I key). We only ever show one
/// registration even if an agent has several (e.g. cursor + claude-code on
/// the same repo) — the popup is a quick-glance card, not a full dump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRegistration {
    pub agent_type: String,
    pub project: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentIdentitySummary {
    pub team: String,
    pub name: String,
    pub agent_type: String,
    pub project: String,
    pub last_seen_at: Option<String>,
    pub sent_30d: usize,
    pub received_30d: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTeamSummary {
    pub name: String,
    pub identities: Vec<AgentIdentitySummary>,
    pub broken_config: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Database {
    path: PathBuf,
}

impl Database {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn connect(&self) -> Result<Connection> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection = Connection::open_with_flags(&self.path, flags)
            .with_context(|| format!("DBを読み取り専用で開けません: {}", self.path.display()))?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        Ok(connection)
    }

    pub fn validate_schema(&self) -> Result<()> {
        let connection = self.connect()?;
        let mut statement = connection.prepare("PRAGMA table_info(messages)")?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
        let expected = [
            "id",
            "team",
            "from_agent",
            "to_agent",
            "body",
            "created_at",
            "read_at",
        ];
        if expected.iter().any(|column| !columns.contains(*column)) {
            bail!("messages テーブルのスキーマが要件と一致しません");
        }
        Ok(())
    }

    pub fn team_names(&self, teams_dir: &Path) -> Result<Vec<String>> {
        let mut names = BTreeSet::new();
        if teams_dir.is_dir() {
            for entry in fs::read_dir(teams_dir)
                .with_context(|| format!("teamディレクトリを読めません: {}", teams_dir.display()))?
            {
                let entry = entry?;
                if entry.file_type()?.is_dir()
                    && let Some(name) = entry.file_name().to_str()
                {
                    names.insert(name.to_owned());
                }
            }
        }

        let connection = self.connect()?;
        let mut statement = connection.prepare("SELECT DISTINCT team FROM messages")?;
        for name in statement.query_map([], |row| row.get::<_, String>(0))? {
            names.insert(name?);
        }
        Ok(names.into_iter().collect())
    }

    pub fn team_summaries(&self, teams_dir: &Path) -> Result<Vec<TeamSummary>> {
        let unread = self.unread_counts()?;
        Ok(self
            .team_names(teams_dir)?
            .into_iter()
            .map(|name| TeamSummary {
                unread_count: unread.get(&name).copied().unwrap_or_default(),
                name,
            })
            .collect())
    }

    pub fn load_roster(teams_dir: &Path, team: &str) -> Result<Vec<String>> {
        let path = teams_dir.join(team).join("config.json");
        if !path.is_file() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("rosterを読めません: {}", path.display()))?;
        let config: TeamConfig = serde_json::from_str(&content)
            .with_context(|| format!("roster JSONが不正です: {}", path.display()))?;
        Ok(config.agents.into_keys().collect())
    }

    /// Reads `agents.<agent>.registrations[0]` straight out of `config.json`
    /// via `serde_json::Value` instead of extending `TeamConfig` — the
    /// popup only needs two fields and doesn't care about the rest of the
    /// registration shape, so a typed struct would just be schema drift risk.
    pub fn agent_registration(
        teams_dir: &Path,
        team: &str,
        agent: &str,
    ) -> Result<Option<AgentRegistration>> {
        let path = teams_dir.join(team).join("config.json");
        if !path.is_file() {
            return Ok(None);
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("rosterを読めません: {}", path.display()))?;
        let value: Value = serde_json::from_str(&content)
            .with_context(|| format!("roster JSONが不正です: {}", path.display()))?;
        let registration = value
            .get("agents")
            .and_then(|agents| agents.get(agent))
            .and_then(|entry| entry.get("registrations"))
            .and_then(|registrations| registrations.get(0));
        Ok(registration.map(|registration| AgentRegistration {
            agent_type: registration
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_owned(),
            project: registration
                .get("project")
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_owned(),
        }))
    }

    /// Sent/received message counts for one agent within `window_days`,
    /// scoped to `team`. Feeds the "Sent (30d)" / "Received (30d)" lines
    /// in the MEMBER info popup.
    pub fn agent_traffic_counts(
        &self,
        team: &str,
        agent: &str,
        window_days: u32,
    ) -> Result<(usize, usize)> {
        let connection = self.connect()?;
        let window = format!("-{window_days} days");
        let sent: i64 = connection.query_row(
            "SELECT COUNT(*) FROM messages
             WHERE team = ?1 AND from_agent = ?2
               AND created_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?3)",
            params![team, agent, window],
            |row| row.get(0),
        )?;
        let received: i64 = connection.query_row(
            "SELECT COUNT(*) FROM messages
             WHERE team = ?1 AND to_agent = ?2
               AND created_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?3)",
            params![team, agent, window],
            |row| row.get(0),
        )?;
        Ok((
            usize::try_from(sent).unwrap_or_default(),
            usize::try_from(received).unwrap_or_default(),
        ))
    }

    /// Agents画面用の遅延読込。configはteam単位で隔離し、1件の破損で全体を止めない。
    /// 複数registrationはreset対象を一意にするため行へ展開する。
    pub fn agent_inventory(&self, teams_dir: &Path) -> Result<Vec<AgentTeamSummary>> {
        if !teams_dir.is_dir() {
            return Ok(Vec::new());
        }

        let mut team_names = fs::read_dir(teams_dir)
            .with_context(|| format!("teamディレクトリを読めません: {}", teams_dir.display()))?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(|file_type| file_type.is_dir())
                    .and_then(|_| entry.file_name().to_str().map(str::to_owned))
            })
            .collect::<Vec<_>>();
        team_names.sort();

        let connection = self.connect()?;
        let mut stats = connection.prepare(
            "SELECT MAX(created_at),
                    COALESCE(SUM(CASE
                        WHEN from_agent = ?2
                         AND created_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-30 days')
                        THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE
                        WHEN to_agent = ?2
                         AND created_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-30 days')
                        THEN 1 ELSE 0 END), 0)
             FROM messages
             WHERE team = ?1 AND (from_agent = ?2 OR to_agent = ?2)",
        )?;

        let mut teams = Vec::with_capacity(team_names.len());
        for team in team_names {
            let config_path = teams_dir.join(&team).join("config.json");
            let parsed = fs::read_to_string(&config_path)
                .with_context(|| format!("configを読めません: {}", config_path.display()))
                .and_then(|content| {
                    serde_json::from_str::<Value>(&content)
                        .with_context(|| format!("config JSONが不正です: {}", config_path.display()))
                });
            let value = match parsed {
                Ok(value) => value,
                Err(error) => {
                    teams.push(AgentTeamSummary {
                        name: team,
                        identities: Vec::new(),
                        broken_config: Some(error.to_string()),
                    });
                    continue;
                }
            };
            let Some(agents) = value.get("agents").and_then(Value::as_object) else {
                teams.push(AgentTeamSummary {
                    name: team,
                    identities: Vec::new(),
                    broken_config: Some("config.json: agents object is missing".to_owned()),
                });
                continue;
            };

            let mut agent_names = agents.keys().cloned().collect::<Vec<_>>();
            agent_names.sort();
            let mut identities = Vec::new();
            for name in agent_names {
                let (last_seen_at, sent, received): (Option<String>, i64, i64) = stats
                    .query_row(params![team, name], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?;
                let sent_30d = usize::try_from(sent).unwrap_or_default();
                let received_30d = usize::try_from(received).unwrap_or_default();
                let entry = &agents[&name];
                let mut registrations = entry
                    .get("registrations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|registration| AgentRegistration {
                        agent_type: registration
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("-")
                            .to_owned(),
                        project: registration
                            .get("project")
                            .and_then(Value::as_str)
                            .unwrap_or("-")
                            .to_owned(),
                    })
                    .collect::<Vec<_>>();
                if registrations.is_empty()
                    && (entry.get("type").is_some() || entry.get("project").is_some())
                {
                    registrations.push(AgentRegistration {
                        agent_type: entry
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("-")
                            .to_owned(),
                        project: entry
                            .get("project")
                            .and_then(Value::as_str)
                            .unwrap_or("-")
                            .to_owned(),
                    });
                }
                if registrations.is_empty() {
                    registrations.push(AgentRegistration {
                        agent_type: "-".to_owned(),
                        project: "-".to_owned(),
                    });
                }
                identities.extend(registrations.into_iter().map(|registration| {
                    AgentIdentitySummary {
                        team: team.clone(),
                        name: name.clone(),
                        agent_type: registration.agent_type,
                        project: registration.project,
                        last_seen_at: last_seen_at.clone(),
                        sent_30d,
                        received_30d,
                    }
                }));
            }
            teams.push(AgentTeamSummary {
                name: team,
                identities,
                broken_config: None,
            });
        }
        Ok(teams)
    }

    pub fn member_summaries(&self, teams_dir: &Path, team: &str) -> Result<Vec<MemberSummary>> {
        let roster = Self::load_roster(teams_dir, team)?;
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT MAX(created_at),
                    COALESCE(SUM(CASE WHEN to_agent = ?2 AND read_at IS NULL THEN 1 ELSE 0 END), 0)
             FROM messages
             WHERE team = ?1 AND (from_agent = ?2 OR to_agent = ?2)",
        )?;
        roster
            .into_iter()
            .map(|name| {
                let (last_message_at, unread_count): (Option<String>, i64) = statement
                    .query_row(params![team, name], |row| Ok((row.get(0)?, row.get(1)?)))?;
                Ok(MemberSummary {
                    name,
                    last_message_at,
                    unread_count: usize::try_from(unread_count).unwrap_or_default(),
                })
            })
            .collect()
    }

    pub fn history(&self, team: &str, before_id: Option<i64>, limit: usize) -> Result<HistoryPage> {
        let fetch_limit =
            i64::try_from(limit.saturating_add(1)).context("履歴件数が大きすぎます")?;
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "WITH cursor AS (
                 SELECT created_at, id FROM messages WHERE team = ?1 AND id = ?2
             )
             SELECT id, team, from_agent, to_agent, body, created_at, read_at
             FROM messages
             WHERE team = ?1
               AND (?2 IS NULL
                    OR created_at < (SELECT created_at FROM cursor)
                    OR (created_at = (SELECT created_at FROM cursor)
                        AND id < (SELECT id FROM cursor)))
             ORDER BY created_at DESC, id DESC
             LIMIT ?3",
        )?;
        let mut messages = statement
            .query_map(params![team, before_id, fetch_limit], map_message)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = messages.len() > limit;
        messages.truncate(limit);
        messages.reverse();
        Ok(HistoryPage { messages, has_more })
    }

    pub fn unread_counts(&self) -> Result<HashMap<String, usize>> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare("SELECT team, COUNT(*) FROM messages WHERE read_at IS NULL GROUP BY team")?;
        let rows = statement.query_map([], |row| {
            let team: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((team, usize::try_from(count).unwrap_or_default()))
        })?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    pub fn latest_message_ts_per_team(&self) -> Result<BTreeMap<String, String>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT team, MAX(created_at) FROM messages GROUP BY team ORDER BY team",
        )?;
        Ok(statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?)
    }

    pub fn daily_traffic(&self, team: Option<&str>, days: u32) -> Result<Vec<DailyTraffic>> {
        if days == 0 {
            return Ok(Vec::new());
        }
        let connection = self.connect()?;
        let window = format!("-{} days", days.saturating_sub(1));
        let mut statement = connection.prepare(
            "SELECT date(created_at), COUNT(*)
             FROM messages
             WHERE (?1 IS NULL OR team = ?1)
               AND date(created_at) BETWEEN date('now', ?2) AND date('now')
             GROUP BY date(created_at)
             ORDER BY date(created_at)",
        )?;
        let rows = statement.query_map(params![team, window], |row| {
            let date: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((date, usize::try_from(count).unwrap_or_default()))
        })?;
        let counts = rows.collect::<rusqlite::Result<HashMap<_, _>>>()?;
        let threshold = burst_threshold();
        let today = Utc::now().date_naive();
        Ok((0..days)
            .rev()
            .map(|offset| {
                let date = today - ChronoDuration::days(i64::from(offset));
                let count = counts
                    .get(&date.format("%Y-%m-%d").to_string())
                    .copied()
                    .unwrap_or_default();
                DailyTraffic {
                    date,
                    count,
                    burst: count > threshold,
                }
            })
            .collect())
    }

    pub fn agent_traffic(&self, team: &str, days: u32) -> Result<Vec<AgentTraffic>> {
        if days == 0 {
            return Ok(Vec::new());
        }
        let connection = self.connect()?;
        let window = format!("-{} days", days.saturating_sub(1));
        let mut statement = connection.prepare(
            "WITH traffic(agent, sent, received) AS (
                 SELECT from_agent, 1, 0
                 FROM messages
                 WHERE team = ?1
                   AND date(created_at) BETWEEN date('now', ?2) AND date('now')
                 UNION ALL
                 SELECT to_agent, 0, 1
                 FROM messages
                 WHERE team = ?1
                   AND date(created_at) BETWEEN date('now', ?2) AND date('now')
             )
             SELECT agent, SUM(sent), SUM(received)
             FROM traffic
             GROUP BY agent
             ORDER BY SUM(sent) + SUM(received) DESC, agent",
        )?;
        Ok(statement
            .query_map(params![team, window], |row| {
                let sent: i64 = row.get(1)?;
                let received: i64 = row.get(2)?;
                Ok(AgentTraffic {
                    agent: row.get(0)?,
                    sent: usize::try_from(sent).unwrap_or_default(),
                    received: usize::try_from(received).unwrap_or_default(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn unread_recipients(&self, team: &str) -> Result<Vec<String>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT DISTINCT to_agent FROM messages
             WHERE team = ?1 AND read_at IS NULL ORDER BY to_agent",
        )?;
        Ok(statement
            .query_map([team], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn unread_count_for_recipient(&self, team: &str, recipient: &str) -> Result<usize> {
        let connection = self.connect()?;
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM messages
             WHERE team = ?1 AND to_agent = ?2 AND read_at IS NULL",
            params![team, recipient],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(count).unwrap_or_default())
    }

    /// Room検索はactive team内の全履歴を対象にし、DB自体はREAD_ONLYを維持する。
    pub fn first_search_match_id(
        &self,
        team: &str,
        query: &str,
        member_filter: Option<&str>,
    ) -> Result<Option<i64>> {
        let connection = self.connect()?;
        let pattern = format!("%{}%", escape_like_pattern(query));
        Ok(connection
            .query_row(
            "SELECT id
             FROM messages
             WHERE team = ?1
               AND (body LIKE ?2 ESCAPE '\\' COLLATE NOCASE
                    OR from_agent LIKE ?2 ESCAPE '\\' COLLATE NOCASE
                    OR to_agent LIKE ?2 ESCAPE '\\' COLLATE NOCASE)
               AND (?3 IS NULL OR from_agent = ?3 OR to_agent = ?3)
             ORDER BY created_at ASC, id ASC
             LIMIT 1",
            params![team, pattern, member_filter],
            |row| row.get(0),
        )
            .optional()?)
    }

    pub fn new_messages_for_team(&self, team: &str, last_seen_id: i64) -> Result<Vec<Message>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            "SELECT id, team, from_agent, to_agent, body, created_at, read_at
             FROM messages
             WHERE team = ?1 AND id > ?2
             ORDER BY id ASC",
        )?;
        Ok(statement
            .query_map(params![team, last_seen_id], map_message)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn last_seen_id(&self) -> Result<i64> {
        let connection = self.connect()?;
        Ok(
            connection.query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |row| {
                row.get(0)
            })?,
        )
    }

    pub fn message_count(&self) -> Result<usize> {
        let connection = self.connect()?;
        let count: i64 =
            connection.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or_default())
    }

    pub fn message_by_id(&self, id: i64) -> Result<Option<Message>> {
        let connection = self.connect()?;
        Ok(connection
            .query_row(
                "SELECT id, team, from_agent, to_agent, body, created_at, read_at
                 FROM messages WHERE id = ?1",
                [id],
                map_message,
            )
            .optional()?)
    }

    pub fn pair_matrices(&self, window_days: u32) -> Result<Vec<PairMatrix>> {
        let connection = self.connect()?;
        let window = format!("-{window_days} days");
        let mut statement = connection.prepare(
            "SELECT team, from_agent, to_agent, COUNT(*)
             FROM messages
             WHERE created_at > strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?1)
             GROUP BY team, from_agent, to_agent
             ORDER BY team, from_agent, to_agent",
        )?;
        let rows = statement.query_map([window], |row| {
            let count: i64 = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                usize::try_from(count).unwrap_or_default(),
            ))
        })?;

        let mut matrices = BTreeMap::<String, PairMatrix>::new();
        for row in rows {
            let (team, from, to, count) = row?;
            let matrix = matrices.entry(team.clone()).or_insert_with(|| PairMatrix {
                team,
                agents: Vec::new(),
                counts: BTreeMap::new(),
            });
            matrix.agents.push(from.clone());
            matrix.agents.push(to.clone());
            matrix.counts.insert((from, to), count);
        }
        for matrix in matrices.values_mut() {
            matrix.agents.sort();
            matrix.agents.dedup();
        }
        Ok(matrices.into_values().collect())
    }

    pub fn zombie_identities(
        &self,
        teams_dir: &Path,
        window_days: u32,
    ) -> Result<Vec<ZombieIdentity>> {
        let connection = self.connect()?;
        let window = format!("-{window_days} days");
        let mut statement = connection.prepare(
            "SELECT COUNT(*) FROM messages
             WHERE team = ?1 AND (from_agent = ?2 OR to_agent = ?2)
               AND created_at > datetime('now', ?3)",
        )?;
        let mut zombies = Vec::new();
        for team in self.team_names(teams_dir)? {
            for agent in Self::load_roster(teams_dir, &team)? {
                let count: i64 =
                    statement.query_row(params![team, agent, window], |row| row.get(0))?;
                if count == 0 {
                    zombies.push(ZombieIdentity {
                        team: team.clone(),
                        agent,
                    });
                }
            }
        }
        Ok(zombies)
    }

    pub fn stale_unreads(&self, stale_days: u32) -> Result<Vec<StaleUnread>> {
        let connection = self.connect()?;
        let window = format!("-{stale_days} days");
        let mut statement = connection.prepare(
            "SELECT id, team, from_agent, to_agent, body, created_at
             FROM messages
             WHERE read_at IS NULL
               AND created_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?1)
             ORDER BY created_at ASC, id ASC
             LIMIT 200",
        )?;
        Ok(statement
            .query_map([window], |row| {
                Ok(StaleUnread {
                    id: row.get(0)?,
                    team: row.get(1)?,
                    from_agent: row.get(2)?,
                    to_agent: row.get(3)?,
                    body: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn stale_unread_teams(&self, stale_days: u32) -> Result<BTreeSet<String>> {
        let connection = self.connect()?;
        let window = format!("-{stale_days} days");
        let mut statement = connection.prepare(
            "SELECT DISTINCT team
             FROM messages
             WHERE read_at IS NULL
               AND created_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?1)
             ORDER BY team",
        )?;
        Ok(statement
            .query_map([window], |row| row.get(0))?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?)
    }
}

fn map_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(0)?,
        team: row.get(1)?,
        from_agent: row.get(2)?,
        to_agent: row.get(3)?,
        body: row.get(4)?,
        created_at: row.get(5)?,
        read_at: row.get(6)?,
    })
}

fn escape_like_pattern(query: &str) -> String {
    query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

pub fn burst_threshold() -> usize {
    std::env::var("AGMSG_BURST_THRESHOLD")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_BURST_THRESHOLD)
}

#[cfg(test)]
mod health_traffic_tests {
    use chrono::{Duration as ChronoDuration, Utc};
    use rusqlite::{Connection, params};

    use super::Database;

    fn traffic_database() -> (tempfile::TempDir, Database) {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("messages.db");
        Connection::open(&path)
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
        (temp, Database::new(path))
    }

    #[test]
    fn daily_traffic_zero_fills_and_flags_days_over_threshold() {
        let (_temp, database) = traffic_database();
        let connection = Connection::open(&database.path).expect("fixture writer");
        let burst_day = (Utc::now().date_naive() - ChronoDuration::days(2))
            .format("%Y-%m-%dT12:00:00Z")
            .to_string();
        let today = Utc::now()
            .date_naive()
            .format("%Y-%m-%dT12:00:00Z")
            .to_string();
        let transaction = connection.unchecked_transaction().expect("transaction");
        for _ in 0..151 {
            transaction
                .execute(
                    "INSERT INTO messages
                     (team, from_agent, to_agent, body, created_at)
                     VALUES ('ops', 'claude', 'codex', 'burst', ?1)",
                    [&burst_day],
                )
                .expect("burst row");
        }
        transaction
            .execute(
                "INSERT INTO messages
                 (team, from_agent, to_agent, body, created_at)
                 VALUES ('ops', 'codex', 'claude', 'today', ?1)",
                [&today],
            )
            .expect("today row");
        transaction.commit().expect("commit fixture");

        let traffic = database.daily_traffic(Some("ops"), 4).expect("traffic");
        assert_eq!(traffic.len(), 4);
        assert_eq!(
            traffic.iter().map(|day| day.count).collect::<Vec<_>>(),
            vec![0, 151, 0, 1]
        );
        assert_eq!(
            traffic.iter().map(|day| day.burst).collect::<Vec<_>>(),
            vec![false, true, false, false]
        );
    }

    #[test]
    fn agent_traffic_counts_both_directions() {
        let (_temp, database) = traffic_database();
        let connection = Connection::open(&database.path).expect("fixture writer");
        let today = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        for (from, to) in [
            ("claude", "codex"),
            ("claude", "codex"),
            ("codex", "claude"),
            ("codex", "kimi"),
        ] {
            connection
                .execute(
                    "INSERT INTO messages
                     (team, from_agent, to_agent, body, created_at)
                     VALUES ('ops', ?1, ?2, 'traffic', ?3)",
                    params![from, to, today],
                )
                .expect("traffic row");
        }

        let traffic = database.agent_traffic("ops", 7).expect("agent traffic");
        let claude = traffic
            .iter()
            .find(|row| row.agent == "claude")
            .expect("claude");
        let codex = traffic
            .iter()
            .find(|row| row.agent == "codex")
            .expect("codex");
        assert_eq!((claude.sent, claude.received), (2, 1));
        assert_eq!((codex.sent, codex.received), (2, 2));
    }
}
