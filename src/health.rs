use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::task::JoinSet;

use crate::db::{
    AgentTraffic, DailyTraffic, Database, STALE_UNREAD_DAYS, SilentIdentity, burst_threshold,
    silent_identity_days,
};
use crate::exec::ScriptRunner;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeliveryMode {
    Monitor,
    Both,
    Turn,
    Off,
    Mixed,
    Unknown,
}

impl DeliveryMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::Both => "both",
            Self::Turn => "turn",
            Self::Off => "off",
            Self::Mixed => "mixed",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeStatus {
    pub label: String,
    pub pid: u32,
    pub alive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamHealth {
    pub name: String,
    pub orphan: bool,
    pub delivery: DeliveryMode,
    pub bridges: Vec<BridgeStatus>,
    pub last_msg_age: Option<Duration>,
    pub unread: usize,
    pub stale_unread: bool,
    pub traffic_7d: Vec<DailyTraffic>,
    pub traffic_30d: Vec<DailyTraffic>,
    pub agents_7d: Vec<AgentTraffic>,
    pub agents_30d: Vec<AgentTraffic>,
    pub silent_identities: Vec<SilentIdentity>,
}

impl TeamHealth {
    pub fn traffic(&self, days: u32) -> &[DailyTraffic] {
        if days == 30 {
            &self.traffic_30d
        } else {
            &self.traffic_7d
        }
    }

    pub fn agent_traffic(&self, days: u32) -> &[AgentTraffic] {
        if days == 30 {
            &self.agents_30d
        } else {
            &self.agents_7d
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthSnapshot {
    pub teams: Vec<TeamHealth>,
    pub daily_total_7d: Vec<DailyTraffic>,
    pub daily_total_30d: Vec<DailyTraffic>,
    pub refreshed_at: String,
    pub burst_threshold: usize,
    pub silent_days: u32,
}

impl HealthSnapshot {
    pub fn daily_total(&self, days: u32) -> &[DailyTraffic] {
        if days == 30 {
            &self.daily_total_30d
        } else {
            &self.daily_total_7d
        }
    }
}

#[derive(Clone, Debug)]
struct Registration {
    team: String,
    agent: String,
    agent_type: String,
    project: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RawBridge {
    label: String,
    pid: u32,
    team_hint: Option<String>,
    agent_hint: Option<String>,
    project_hint: Option<String>,
}

pub fn parse_delivery_mode(stdout: &str) -> DeliveryMode {
    let mode = stdout.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("mode=")
            .or_else(|| line.strip_prefix("mode:"))
            .map(str::trim)
    });
    match mode {
        Some("monitor") => DeliveryMode::Monitor,
        Some("both") => DeliveryMode::Both,
        Some("turn") => DeliveryMode::Turn,
        Some("off") => DeliveryMode::Off,
        _ => DeliveryMode::Unknown,
    }
}

pub fn parse_alive_pids(stdout: &str) -> BTreeSet<u32> {
    stdout
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

pub async fn collect_health_snapshot(
    database: &Database,
    teams_dir: &Path,
    run_dir: &Path,
    scripts: &ScriptRunner,
) -> Result<HealthSnapshot> {
    let team_names = database.team_names(teams_dir)?;
    let registrations = load_registrations(teams_dir, &team_names);
    let delivery_modes = collect_delivery_modes(&registrations, scripts).await;
    let latest = database.latest_message_ts_per_team().unwrap_or_default();
    let unread = database.unread_counts().unwrap_or_default();
    let stale_teams = database
        .stale_unread_teams(STALE_UNREAD_DAYS)
        .unwrap_or_default();

    let raw_bridges = discover_bridges(run_dir);
    let pids = raw_bridges
        .iter()
        .map(|bridge| bridge.pid)
        .collect::<Vec<_>>();
    let alive = scripts
        .process_status(&pids)
        .await
        .ok()
        .filter(|result| result.success)
        .map(|result| parse_alive_pids(&result.stdout))
        .unwrap_or_default();
    let mut bridges_by_team =
        map_bridges_to_teams(&raw_bridges, &registrations, &team_names, &alive);
    let alive_by_identity = bridge_alive_map(&raw_bridges, &registrations, &alive);
    let silent_days = silent_identity_days();
    let silent_candidates = database
        .silent_identities(teams_dir, silent_days)
        .unwrap_or_default();

    let now = Utc::now();
    let mut teams = Vec::with_capacity(team_names.len());
    for name in team_names {
        let last_msg_age = latest.get(&name).and_then(|timestamp| {
            DateTime::parse_from_rfc3339(timestamp)
                .ok()
                .and_then(|timestamp| {
                    now.signed_duration_since(timestamp.with_timezone(&Utc))
                        .to_std()
                        .ok()
                })
        });
        let traffic_7d = database.daily_traffic(Some(&name), 7).unwrap_or_default();
        let traffic_30d = database.daily_traffic(Some(&name), 30).unwrap_or_default();
        let agents_7d = database.agent_traffic(&name, 7).unwrap_or_default();
        let agents_30d = database.agent_traffic(&name, 30).unwrap_or_default();
        let silent_identities = silent_candidates
            .iter()
            .filter(|identity| {
                identity.team == name
                    && alive_by_identity
                        .get(&(identity.team.clone(), identity.agent.clone()))
                        .copied()
                        .unwrap_or(false)
            })
            .cloned()
            .collect();
        teams.push(TeamHealth {
            orphan: !teams_dir.join(&name).is_dir(),
            delivery: delivery_modes
                .get(&name)
                .copied()
                .unwrap_or(DeliveryMode::Unknown),
            bridges: bridges_by_team.remove(&name).unwrap_or_default(),
            last_msg_age,
            unread: unread.get(&name).copied().unwrap_or_default(),
            stale_unread: stale_teams.contains(&name),
            traffic_7d,
            traffic_30d,
            agents_7d,
            agents_30d,
            silent_identities,
            name,
        });
    }

    Ok(HealthSnapshot {
        teams,
        daily_total_7d: database.daily_traffic(None, 7).unwrap_or_default(),
        daily_total_30d: database.daily_traffic(None, 30).unwrap_or_default(),
        refreshed_at: now.format("%H:%M:%S").to_string(),
        burst_threshold: burst_threshold(),
        silent_days,
    })
}

async fn collect_delivery_modes(
    registrations: &[Registration],
    scripts: &ScriptRunner,
) -> BTreeMap<String, DeliveryMode> {
    let mut queries = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for registration in registrations {
        queries
            .entry((
                registration.agent_type.clone(),
                registration.project.clone(),
            ))
            .or_default()
            .insert(registration.team.clone());
    }

    let mut tasks = JoinSet::new();
    for ((agent_type, project), teams) in queries {
        let scripts = scripts.clone();
        tasks.spawn(async move {
            let mode = match scripts.delivery_status(&agent_type, &project).await {
                Ok(result) if result.success => parse_delivery_mode(&result.stdout),
                _ => DeliveryMode::Unknown,
            };
            (teams, mode)
        });
    }

    let mut modes_by_team = BTreeMap::<String, BTreeSet<DeliveryMode>>::new();
    while let Some(result) = tasks.join_next().await {
        let Ok((teams, mode)) = result else {
            continue;
        };
        for team in teams {
            modes_by_team.entry(team).or_default().insert(mode);
        }
    }
    modes_by_team
        .into_iter()
        .map(|(team, modes)| (team, aggregate_delivery_modes(&modes)))
        .collect()
}

fn aggregate_delivery_modes(modes: &BTreeSet<DeliveryMode>) -> DeliveryMode {
    let known = modes
        .iter()
        .copied()
        .filter(|mode| *mode != DeliveryMode::Unknown)
        .collect::<Vec<_>>();
    match known.as_slice() {
        [] => DeliveryMode::Unknown,
        [mode] => *mode,
        _ => DeliveryMode::Mixed,
    }
}

fn load_registrations(teams_dir: &Path, team_names: &[String]) -> Vec<Registration> {
    let mut registrations = Vec::new();
    for team in team_names {
        let Ok(content) = fs::read_to_string(teams_dir.join(team).join("config.json")) else {
            continue;
        };
        let Ok(config) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        let Some(agents) = config.get("agents").and_then(Value::as_object) else {
            continue;
        };
        for (name, agent) in agents {
            let Some(items) = agent.get("registrations").and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                let Some(agent_type) = item.get("type").and_then(Value::as_str) else {
                    continue;
                };
                let Some(project) = item.get("project").and_then(Value::as_str) else {
                    continue;
                };
                registrations.push(Registration {
                    team: team.clone(),
                    agent: name.clone(),
                    agent_type: agent_type.to_owned(),
                    project: project.to_owned(),
                });
            }
        }
    }
    registrations
}

fn discover_bridges(run_dir: &Path) -> Vec<RawBridge> {
    let Ok(entries) = fs::read_dir(run_dir) else {
        return Vec::new();
    };
    let mut paths = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect::<Vec<_>>();
    paths.sort();
    let mut bridges = BTreeMap::<u32, RawBridge>::new();
    for path in paths {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let raw = if let Some(pid) = name
            .strip_prefix("cc-instance.")
            .and_then(|value| value.parse::<u32>().ok())
        {
            Some(RawBridge {
                label: "claude-code".to_owned(),
                pid,
                team_hint: None,
                agent_hint: None,
                project_hint: read_project_marker(run_dir, pid),
            })
        } else if let Some(stem) = name.strip_suffix(".pid") {
            let pid = fs::read_to_string(&path)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok());
            pid.map(|pid| raw_bridge_from_pidfile(run_dir, stem, pid))
        } else {
            None
        };
        let Some(raw) = raw else {
            continue;
        };
        bridges
            .entry(raw.pid)
            .and_modify(|existing| merge_bridge_hints(existing, &raw))
            .or_insert(raw);
    }
    bridges.into_values().collect()
}

fn raw_bridge_from_pidfile(run_dir: &Path, stem: &str, pid: u32) -> RawBridge {
    let mut team_hint = None;
    let mut agent_hint = None;
    let mut project_hint = None;
    let mut label = stem.split('.').next().unwrap_or("bridge").to_owned();

    if let Some(identity) = stem.strip_prefix("codex-bridge.")
        && let Some((team, agent)) = identity.rsplit_once('.')
    {
        team_hint = Some(team.to_owned());
        agent_hint = Some(agent.to_owned());
        label = format!("codex/{agent}");
    }
    if let Some(owner_pid) = stem
        .strip_prefix("watch.")
        .and_then(|value| value.rsplit_once('.').map(|(_, pid)| pid))
        .and_then(|pid| pid.parse::<u32>().ok())
    {
        project_hint = read_project_marker(run_dir, owner_pid);
    }

    let metadata = read_key_values(&run_dir.join(format!("{stem}.meta")));
    team_hint = metadata.get("team").cloned().or(team_hint);
    agent_hint = metadata.get("agent").cloned().or(agent_hint);
    project_hint = metadata.get("project").cloned().or(project_hint);
    if let Some(name) = metadata.get("name") {
        label = format!("{label}/{name}");
    }
    if project_hint.is_none() {
        project_hint = read_project_from_log(&run_dir.join(format!("{stem}.log")));
    }
    if let Some(role_agent) = role_session_agent_hint(
        run_dir,
        team_hint.as_deref(),
        bridge_agent_type(&label),
        project_hint.as_deref(),
    ) {
        agent_hint = Some(role_agent);
    }

    RawBridge {
        label,
        pid,
        team_hint,
        agent_hint,
        project_hint,
    }
}

/// codex bridgeのpid名は汎用名になることがあるため、role-session markerから
/// 実際の登録identityを一意に復元する。
fn role_session_agent_hint(
    run_dir: &Path,
    team_hint: Option<&str>,
    agent_type: Option<&str>,
    project_hint: Option<&str>,
) -> Option<String> {
    let team_hint = team_hint?;
    let agent_type = agent_type?;
    let mut agents = BTreeSet::new();
    let entries = fs::read_dir(run_dir).ok()?;
    for path in entries.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
        let is_role_session = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("role-session."));
        if !is_role_session {
            continue;
        }
        let metadata = read_key_values(&path);
        if metadata.get("team").map(String::as_str) != Some(team_hint)
            || metadata.get("type").map(String::as_str) != Some(agent_type)
            || project_hint.is_some_and(|project| {
                metadata.get("project").map(String::as_str) != Some(project)
            })
        {
            continue;
        }
        if let Some(agent) = metadata.get("agent") {
            agents.insert(agent.clone());
        }
    }
    (agents.len() == 1).then(|| agents.into_iter().next()).flatten()
}

fn bridge_agent_type(label: &str) -> Option<&'static str> {
    if label.starts_with("claude-code") {
        Some("claude-code")
    } else if label.starts_with("cursor-bridge") {
        Some("cursor")
    } else if label.starts_with("kimi-bridge") {
        Some("kimi")
    } else if label.starts_with("codex") {
        Some("codex")
    } else {
        None
    }
}

fn read_project_marker(run_dir: &Path, pid: u32) -> Option<String> {
    fs::read_to_string(run_dir.join(format!("proj.{pid}.project")))
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn read_key_values(path: &Path) -> BTreeMap<String, String> {
    fs::read_to_string(path)
        .ok()
        .into_iter()
        .flat_map(|content| {
            content
                .lines()
                .filter_map(|line| line.split_once('='))
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn read_project_from_log(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    content.lines().take(20).find_map(|line| {
        let project = line.split_once("project=")?.1;
        let project = project
            .split(" session_id=")
            .next()
            .unwrap_or(project)
            .trim();
        (!project.is_empty()).then(|| project.to_owned())
    })
}

fn merge_bridge_hints(existing: &mut RawBridge, candidate: &RawBridge) {
    if existing.team_hint.is_none() {
        existing.team_hint.clone_from(&candidate.team_hint);
    }
    if existing.project_hint.is_none() {
        existing.project_hint.clone_from(&candidate.project_hint);
    }
    if existing.agent_hint.is_none() {
        existing.agent_hint.clone_from(&candidate.agent_hint);
    }
    if existing.label == "bridge" {
        existing.label.clone_from(&candidate.label);
    }
}

fn map_bridges_to_teams(
    raw_bridges: &[RawBridge],
    registrations: &[Registration],
    team_names: &[String],
    alive: &BTreeSet<u32>,
) -> BTreeMap<String, Vec<BridgeStatus>> {
    let known_teams = team_names.iter().cloned().collect::<BTreeSet<_>>();
    let mut mapped = BTreeMap::<String, Vec<BridgeStatus>>::new();
    for bridge in raw_bridges {
        let mut targets = BTreeSet::new();
        if let Some(team) = bridge
            .team_hint
            .as_ref()
            .filter(|team| known_teams.contains(*team))
        {
            targets.insert(team.clone());
        } else if let Some(project) = &bridge.project_hint {
            for registration in registrations.iter().filter(|registration| {
                registration.project == *project
                    && bridge_kind_matches(&bridge.label, &registration.agent_type)
            }) {
                targets.insert(registration.team.clone());
            }
        }
        for team in targets {
            mapped.entry(team).or_default().push(BridgeStatus {
                label: bridge.label.clone(),
                pid: bridge.pid,
                alive: alive.contains(&bridge.pid),
            });
        }
    }
    for bridges in mapped.values_mut() {
        bridges.sort_by_key(|bridge| bridge.pid);
    }
    mapped
}

/// P11のbridge解決結果をidentity単位へ投影する。identity名を持つbridgeは
/// exact matchし、名前を持たないbridgeは一意なtype/projectだけ照合する。
fn bridge_alive_map(
    raw_bridges: &[RawBridge],
    registrations: &[Registration],
    alive: &BTreeSet<u32>,
) -> BTreeMap<(String, String), bool> {
    let mut identities_by_registration =
        BTreeMap::<(String, String), BTreeSet<(String, String)>>::new();
    for registration in registrations {
        identities_by_registration
            .entry((
                registration.agent_type.clone(),
                registration.project.clone(),
            ))
            .or_default()
            .insert((registration.team.clone(), registration.agent.clone()));
    }
    let mut mapped = BTreeMap::new();
    for registration in registrations {
        let is_alive = raw_bridges.iter().any(|bridge| {
            if !alive.contains(&bridge.pid)
                || !bridge_kind_matches(&bridge.label, &registration.agent_type)
            {
                return false;
            }
            if let Some(team) = &bridge.team_hint
                && team != &registration.team
            {
                return false;
            }
            if let Some(agent_hint) = &bridge.agent_hint {
                return agent_hint == &registration.agent;
            }
            let identity = (registration.team.clone(), registration.agent.clone());
            bridge.project_hint.as_ref().is_some_and(|project| {
                project == &registration.project
                    && identities_by_registration
                        .get(&(registration.agent_type.clone(), project.clone()))
                        .is_some_and(|identities| {
                            identities.len() == 1 && identities.contains(&identity)
                        })
            })
        });
        mapped
            .entry((registration.team.clone(), registration.agent.clone()))
            .and_modify(|alive| *alive |= is_alive)
            .or_insert(is_alive);
    }
    mapped
}

fn bridge_kind_matches(label: &str, agent_type: &str) -> bool {
    if label.starts_with("claude-code") {
        agent_type == "claude-code"
    } else if label.starts_with("cursor-bridge") {
        agent_type == "cursor"
    } else if label.starts_with("kimi-bridge") {
        agent_type == "kimi"
    } else if label.starts_with("codex") {
        agent_type == "codex"
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use super::{
        DeliveryMode, RawBridge, Registration, bridge_alive_map, discover_bridges,
        parse_alive_pids, parse_delivery_mode,
    };
    use crate::exec::ScriptRunner;

    #[tokio::test(flavor = "current_thread")]
    async fn delivery_status_parser_handles_known_unknown_and_empty_output() {
        let temp = tempfile::tempdir().expect("fixture dir");
        let scripts = temp.path().join("scripts");
        fs::create_dir_all(&scripts).expect("scripts fixture");
        fs::write(
            scripts.join("delivery.sh"),
            "#!/bin/bash\nprintf 'args=%s\\n' \"$*\"\nprintf 'mode=monitor\\n'\n",
        )
        .expect("fake delivery script");
        let runner = ScriptRunner::new(&scripts, temp.path().join("agmsg-audit"));
        let result = runner
            .delivery_status("codex", "/tmp/fixture project")
            .await
            .expect("delivery status");
        assert!(
            result
                .stdout
                .contains("args=status codex /tmp/fixture project")
        );
        assert_eq!(parse_delivery_mode(&result.stdout), DeliveryMode::Monitor);
        assert_eq!(parse_delivery_mode("mode: both\n"), DeliveryMode::Both);
        assert_eq!(parse_delivery_mode("mode=surprise"), DeliveryMode::Unknown);
        assert_eq!(parse_delivery_mode(""), DeliveryMode::Unknown);
    }

    #[test]
    fn run_fixture_extracts_cc_instance_and_pidfile_pids() {
        let temp = tempfile::tempdir().expect("fixture dir");
        fs::write(temp.path().join("cc-instance.123"), "session.123\n").expect("cc fixture");
        fs::write(temp.path().join("cursor-bridge.hash.pid"), "456\n").expect("pid fixture");
        fs::write(temp.path().join("broken.pid"), "not-a-pid\n").expect("broken fixture");
        fs::write(temp.path().join("cc-instance.nope"), "ignored\n").expect("ignored fixture");

        let bridges = discover_bridges(temp.path());
        assert_eq!(
            bridges.iter().map(|bridge| bridge.pid).collect::<Vec<_>>(),
            vec![123, 456]
        );
    }

    #[test]
    fn ps_output_maps_only_numeric_alive_pids() {
        let alive = parse_alive_pids("  PID\n  123\n456\nnot-a-pid\n");
        assert!(alive.contains(&123));
        assert!(alive.contains(&456));
        assert!(!alive.contains(&789));
    }

    #[test]
    fn bridge_alive_map_prefers_identity_hint_and_falls_back_to_project_type() {
        let registrations = vec![
            Registration {
                team: "ops".to_owned(),
                agent: "codex-worker".to_owned(),
                agent_type: "codex".to_owned(),
                project: "/repo/ops".to_owned(),
            },
            Registration {
                team: "ops".to_owned(),
                agent: "claude-main".to_owned(),
                agent_type: "claude-code".to_owned(),
                project: "/repo/ops".to_owned(),
            },
        ];
        let bridges = vec![
            RawBridge {
                label: "codex/codex-worker".to_owned(),
                pid: 10,
                team_hint: Some("ops".to_owned()),
                agent_hint: Some("codex-worker".to_owned()),
                project_hint: None,
            },
            RawBridge {
                label: "claude-code".to_owned(),
                pid: 20,
                team_hint: None,
                agent_hint: None,
                project_hint: Some("/repo/ops".to_owned()),
            },
        ];
        let alive = BTreeSet::from([10, 20]);
        let map = bridge_alive_map(&bridges, &registrations, &alive);
        assert_eq!(
            map.get(&("ops".to_owned(), "codex-worker".to_owned())),
            Some(&true)
        );
        assert_eq!(
            map.get(&("ops".to_owned(), "claude-main".to_owned())),
            Some(&true)
        );
    }

    #[test]
    fn project_fallback_requires_a_unique_registered_identity() {
        let registrations = vec![
            Registration {
                team: "one".to_owned(),
                agent: "claude-one".to_owned(),
                agent_type: "claude-code".to_owned(),
                project: "/repo/shared".to_owned(),
            },
            Registration {
                team: "two".to_owned(),
                agent: "claude-two".to_owned(),
                agent_type: "claude-code".to_owned(),
                project: "/repo/shared".to_owned(),
            },
        ];
        let bridges = vec![RawBridge {
            label: "claude-code".to_owned(),
            pid: 20,
            team_hint: None,
            agent_hint: None,
            project_hint: Some("/repo/shared".to_owned()),
        }];
        let map = bridge_alive_map(&bridges, &registrations, &BTreeSet::from([20]));
        assert!(map.values().all(|alive| !alive));
    }

    #[test]
    fn codex_role_session_marker_overrides_generic_pid_identity() {
        let temp = tempfile::tempdir().expect("fixture dir");
        fs::write(
            temp.path().join("codex-bridge.ops.codex.pid"),
            "789\n",
        )
        .expect("pid fixture");
        fs::write(
            temp.path().join("codex-bridge.ops.codex.meta"),
            "team=ops\nproject=/repo/ops\n",
        )
        .expect("meta fixture");
        fs::write(
            temp.path().join("role-session.ops__codex-worker"),
            "team=ops\nagent=codex-worker\ntype=codex\nproject=/repo/ops\n",
        )
        .expect("role fixture");

        let bridge = discover_bridges(temp.path())
            .into_iter()
            .find(|bridge| bridge.pid == 789)
            .expect("bridge");
        assert_eq!(bridge.agent_hint.as_deref(), Some("codex-worker"));
    }
}
