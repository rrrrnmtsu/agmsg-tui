use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};

use crate::agents::validate_agent_name;
use crate::db::{AgentIdentitySummary, Message};
use crate::exec::CommandResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterPeriod {
    SevenDays,
    ThirtyDays,
    All,
}

impl FilterPeriod {
    pub fn label(self) -> &'static str {
        match self {
            Self::SevenDays => "7d",
            Self::ThirtyDays => "30d",
            Self::All => "all",
        }
    }

    pub fn cycle(self, step: isize) -> Self {
        let periods = [Self::SevenDays, Self::ThirtyDays, Self::All];
        let current = periods
            .iter()
            .position(|period| *period == self)
            .unwrap_or(0);
        let next = if step < 0 {
            current.checked_sub(1).unwrap_or(periods.len() - 1)
        } else {
            (current + 1) % periods.len()
        };
        periods[next]
    }

    fn cutoff(self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Self::SevenDays => Some(now - Duration::days(7)),
            Self::ThirtyDays => Some(now - Duration::days(30)),
            Self::All => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkFilterFocus {
    Agent,
    Period,
    Body,
    Results,
}

impl BulkFilterFocus {
    pub fn next(self, backwards: bool) -> Self {
        let fields = [Self::Agent, Self::Period, Self::Body, Self::Results];
        let current = fields.iter().position(|field| *field == self).unwrap_or(0);
        if backwards {
            fields[current.checked_sub(1).unwrap_or(fields.len() - 1)]
        } else {
            fields[(current + 1) % fields.len()]
        }
    }
}

#[derive(Clone, Debug)]
pub struct BulkFilterState {
    pub agent: String,
    pub period: FilterPeriod,
    pub body: String,
    pub focus: BulkFilterFocus,
    pub all_messages: Vec<Message>,
    pub results: Vec<usize>,
    pub selected: usize,
}

impl BulkFilterState {
    pub fn new(all_messages: Vec<Message>, now: DateTime<Utc>) -> Self {
        let mut state = Self {
            agent: String::new(),
            period: FilterPeriod::SevenDays,
            body: String::new(),
            focus: BulkFilterFocus::Agent,
            all_messages,
            results: Vec::new(),
            selected: 0,
        };
        state.recompute(now);
        state
    }

    pub fn recompute(&mut self, now: DateTime<Utc>) {
        let agent = self.agent.to_lowercase();
        let body = self.body.to_lowercase();
        let cutoff = self.period.cutoff(now);
        self.results = self
            .all_messages
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                (agent.is_empty()
                    || message.from_agent.to_lowercase().contains(&agent)
                    || message.to_agent.to_lowercase().contains(&agent))
                    && (body.is_empty() || message.body.to_lowercase().contains(&body))
                    && cutoff.is_none_or(|cutoff| message_is_after(message, cutoff))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = self.selected.min(self.results.len().saturating_sub(1));
    }

    pub fn messages(&self) -> impl Iterator<Item = &Message> {
        self.results
            .iter()
            .filter_map(|index| self.all_messages.get(*index))
    }

    pub fn selected_message(&self) -> Option<&Message> {
        self.results
            .get(self.selected)
            .and_then(|index| self.all_messages.get(*index))
    }
}

fn message_is_after(message: &Message, cutoff: DateTime<Utc>) -> bool {
    DateTime::parse_from_rfc3339(&message.created_at)
        .map(|timestamp| timestamp.with_timezone(&Utc) >= cutoff)
        .unwrap_or(false)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkReadTarget {
    pub team: String,
    pub recipient: String,
    pub message_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResetTarget {
    pub team: String,
    pub agent: String,
    pub agent_type: String,
    pub project: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameTarget {
    pub team: String,
    pub old: String,
    pub new: String,
    pub agent_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DespawnTarget {
    pub team: String,
    pub from: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BulkTarget {
    MarkRead(MarkReadTarget),
    Reset(ResetTarget),
    Rename(RenameTarget),
    Despawn(DespawnTarget),
}

impl BulkTarget {
    pub fn label(&self) -> String {
        match self {
            Self::MarkRead(target) => format!(
                "{}/{} ({} msgs)",
                target.team, target.recipient, target.message_count
            ),
            Self::Reset(target) => {
                format!("{}/{} ({})", target.team, target.agent, target.agent_type)
            }
            Self::Rename(target) => format!("{}/{} -> {}", target.team, target.old, target.new),
            Self::Despawn(target) => {
                format!("{}/{} (from {})", target.team, target.name, target.from)
            }
        }
    }

    pub fn units(&self) -> usize {
        match self {
            Self::MarkRead(target) => target.message_count,
            _ => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkKind {
    MarkRead,
    Reset,
    Rename,
    Despawn,
}

impl BulkKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::MarkRead => "bulk mark read",
            Self::Reset => "bulk reset",
            Self::Rename => "bulk rename",
            Self::Despawn => "despawn",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Markdown,
    Json,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Json => "json",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BulkModal {
    Preview {
        kind: BulkKind,
        targets: Vec<BulkTarget>,
        confirm: String,
        scroll: usize,
    },
    ExportFormat {
        selected: ExportFormat,
    },
    RenameEdit {
        targets: Vec<RenameTarget>,
        selected: usize,
        editing: bool,
    },
    RenameConfirm {
        targets: Vec<RenameTarget>,
        confirm: String,
        scroll: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkStepResult {
    pub target: BulkTarget,
    pub success: bool,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BulkRunState {
    Running,
    AwaitDecision { detail: String },
    AwaitForce { detail: String },
    Complete { aborted: bool },
}

#[derive(Clone, Debug)]
pub struct BulkOperation {
    pub kind: BulkKind,
    pub targets: Vec<BulkTarget>,
    pub next_index: usize,
    pub completed_units: usize,
    pub failed_units: usize,
    pub results: Vec<BulkStepResult>,
    pub results_cursor: usize,
    pub state: BulkRunState,
    pub cancel_requested: bool,
    pub force_despawn: bool,
}

impl BulkOperation {
    pub fn new(kind: BulkKind, targets: Vec<BulkTarget>) -> Self {
        Self {
            kind,
            targets,
            next_index: 0,
            completed_units: 0,
            failed_units: 0,
            results: Vec::new(),
            results_cursor: 0,
            state: BulkRunState::Running,
            cancel_requested: false,
            force_despawn: false,
        }
    }

    pub fn total_units(&self) -> usize {
        self.targets.iter().map(BulkTarget::units).sum()
    }

    pub fn current_target(&self) -> Option<&BulkTarget> {
        self.targets.get(self.next_index)
    }

    pub fn progress_label(&self) -> String {
        format!(
            "{}/{} complete",
            self.completed_units + self.failed_units,
            self.total_units()
        )
    }

    pub fn record_success(&mut self, target: BulkTarget, result: &CommandResult) {
        let units = target.units();
        self.completed_units += units;
        self.next_index += 1;
        self.results.push(BulkStepResult {
            target,
            success: true,
            detail: command_detail(result, "ok"),
        });
        self.results_cursor = self.results.len().saturating_sub(1);
        self.force_despawn = false;
    }

    pub fn record_failure(&mut self, target: BulkTarget, detail: String) {
        self.results.push(BulkStepResult {
            target,
            success: false,
            detail: detail.clone(),
        });
        self.results_cursor = self.results.len().saturating_sub(1);
        self.state = if self.kind == BulkKind::Despawn && !self.force_despawn {
            BulkRunState::AwaitForce { detail }
        } else {
            BulkRunState::AwaitDecision { detail }
        };
    }

    pub fn record_cancelled(&mut self, target: BulkTarget) {
        self.results.push(BulkStepResult {
            target,
            success: false,
            detail: "cancelled by user".to_owned(),
        });
        self.results_cursor = self.results.len().saturating_sub(1);
    }

    pub fn skip_failed_and_continue(&mut self) {
        if let Some(target) = self.current_target() {
            self.failed_units += target.units();
        }
        self.next_index += 1;
        self.force_despawn = false;
        self.state = BulkRunState::Running;
    }

    pub fn retry_force(&mut self) {
        self.force_despawn = true;
        self.state = BulkRunState::Running;
    }

    pub fn finish(&mut self, aborted: bool) {
        self.state = BulkRunState::Complete { aborted };
    }
}

fn command_detail(result: &CommandResult, fallback: &str) -> String {
    if !result.stdout.is_empty() {
        result.stdout.clone()
    } else if !result.stderr.is_empty() {
        result.stderr.clone()
    } else {
        fallback.to_owned()
    }
}

pub fn mark_read_targets(messages: impl Iterator<Item = Message>) -> Vec<BulkTarget> {
    let mut grouped = BTreeMap::<(String, String), usize>::new();
    for message in messages.filter(|message| message.read_at.is_none()) {
        *grouped.entry((message.team, message.to_agent)).or_default() += 1;
    }
    grouped
        .into_iter()
        .map(|((team, recipient), message_count)| {
            BulkTarget::MarkRead(MarkReadTarget {
                team,
                recipient,
                message_count,
            })
        })
        .collect()
}

pub fn naming_violations(teams: &[crate::db::AgentTeamSummary]) -> Vec<RenameTarget> {
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    for identity in teams.iter().flat_map(|team| &team.identities) {
        let key = (identity.team.clone(), identity.name.clone());
        if !seen.insert(key) || !is_naming_violation(identity) {
            continue;
        }
        targets.push(RenameTarget {
            team: identity.team.clone(),
            old: identity.name.clone(),
            new: suggested_name(identity, teams),
            agent_type: identity.agent_type.clone(),
        });
    }
    targets
}

fn is_naming_violation(identity: &AgentIdentitySummary) -> bool {
    if identity.agent_type == "-" {
        return false;
    }
    let name = identity.name.as_str();
    let numbered = name.rsplit_once('-').is_some_and(|(_, suffix)| {
        !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
    }) || name
        .strip_prefix("claude-sub")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()));
    name == identity.agent_type
        || name == "claude"
        || name.len() < 3
        || numbered
        || validate_agent_name(name, &identity.agent_type).is_err()
}

fn suggested_name(
    identity: &AgentIdentitySummary,
    teams: &[crate::db::AgentTeamSummary],
) -> String {
    let prefix = if identity.agent_type == "-" {
        "agent"
    } else {
        &identity.agent_type
    };
    let base = format!("{prefix}-worker");
    let used = teams
        .iter()
        .find(|team| team.name == identity.team)
        .map(|team| {
            team.identities
                .iter()
                .map(|row| row.name.as_str())
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    if !used.contains(base.as_str()) {
        return base;
    }
    (2..)
        .map(|index| format!("{base}-{index}"))
        .find(|candidate| !used.contains(candidate.as_str()))
        .unwrap_or(base)
}

pub fn export_bulk_messages(
    report_dir: &Path,
    messages: &[Message],
    format: ExportFormat,
    now: DateTime<Utc>,
) -> Result<PathBuf> {
    fs::create_dir_all(report_dir)
        .with_context(|| format!("export directory unavailable: {}", report_dir.display()))?;
    let stamp = now.format("%Y%m%d-%H%M%S");
    let path = report_dir.join(format!("agmsg-bulk-{stamp}.{}", format.extension()));
    let content = match format {
        ExportFormat::Markdown => build_markdown(messages, now),
        ExportFormat::Json => build_json(messages, now)?,
    };
    fs::write(&path, content).with_context(|| format!("export failed: {}", path.display()))?;
    Ok(path)
}

fn build_markdown(messages: &[Message], now: DateTime<Utc>) -> String {
    let mut output = format!(
        "# agmsg bulk export\n\n- Generated: {}\n- Messages: {}\n\n",
        now.to_rfc3339(),
        messages.len()
    );
    for message in messages {
        output.push_str(&format!(
            "## #{} `{}` {} -> {}\n\n- Created: {}\n- Read: {}\n\n{}\n\n",
            message.id,
            escape_markdown(&message.team),
            escape_markdown(&message.from_agent),
            escape_markdown(&message.to_agent),
            message.created_at,
            message.read_at.as_deref().unwrap_or("unread"),
            message.body
        ));
    }
    output
}

fn build_json(messages: &[Message], now: DateTime<Utc>) -> Result<String> {
    let values = messages
        .iter()
        .map(|message| {
            serde_json::json!({
                "id": message.id,
                "team": message.team,
                "from": message.from_agent,
                "to": message.to_agent,
                "body": message.body,
                "created_at": message.created_at,
                "read_at": message.read_at,
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "generated_at": now.to_rfc3339(),
        "message_count": messages.len(),
        "messages": values,
    }))?)
}

fn escape_markdown(value: &str) -> String {
    value.replace('|', "\\|").replace(['\n', '\r'], " ")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockStatus {
    Unlocked,
    Owned(String),
    ParseError,
}

pub fn actas_lock_status(run_dir: &Path, team: &str, agent: &str) -> LockStatus {
    let entries = match fs::read_dir(run_dir) {
        Ok(entries) => entries,
        Err(_) => return LockStatus::ParseError,
    };
    let mut parse_error = false;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            parse_error = true;
            continue;
        };
        if !name.starts_with("actas.") || !name.ends_with(".session") {
            continue;
        }
        let encoded = &name[6..name.len() - 8];
        let Some((encoded_team, encoded_agent)) = encoded.split_once("__") else {
            parse_error = true;
            continue;
        };
        let Some(lock_team) = percent_decode(encoded_team) else {
            parse_error = true;
            continue;
        };
        let Some(lock_agent) = percent_decode(encoded_agent) else {
            parse_error = true;
            continue;
        };
        if lock_team != team || lock_agent != agent {
            continue;
        }
        return match fs::read_to_string(entry.path()) {
            Ok(content) if !content.lines().next().unwrap_or_default().trim().is_empty() => {
                LockStatus::Owned(content.lines().next().unwrap_or_default().trim().to_owned())
            }
            _ => LockStatus::ParseError,
        };
    }
    if parse_error {
        LockStatus::ParseError
    } else {
        LockStatus::Unlocked
    }
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let encoded = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            output.push(u8::from_str_radix(encoded, 16).ok()?);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).ok()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::{TimeZone, Utc};

    use super::{
        BulkFilterState, ExportFormat, FilterPeriod, LockStatus, actas_lock_status,
        export_bulk_messages, mark_read_targets,
    };
    use crate::db::Message;

    fn message(id: i64, from: &str, to: &str, body: &str, created_at: &str) -> Message {
        Message {
            id,
            team: "ops-hub".to_owned(),
            from_agent: from.to_owned(),
            to_agent: to.to_owned(),
            body: body.to_owned(),
            created_at: created_at.to_owned(),
            read_at: None,
        }
    }

    #[test]
    fn filter_matches_agent_from_or_to_case_insensitively() {
        let now = Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0).unwrap();
        let mut state = BulkFilterState::new(
            vec![
                message(1, "Codex", "claude", "one", "2026-07-20T00:00:00Z"),
                message(2, "claude", "codex-review", "two", "2026-07-20T00:00:00Z"),
                message(3, "kimi", "claude", "three", "2026-07-20T00:00:00Z"),
            ],
            now,
        );
        state.agent = "CODEX".to_owned();
        state.recompute(now);
        assert_eq!(
            state.messages().map(|row| row.id).collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn filter_applies_period_and_all_mode() {
        let now = Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0).unwrap();
        let mut state = BulkFilterState::new(
            vec![
                message(1, "a", "b", "new", "2026-07-20T00:00:00Z"),
                message(2, "a", "b", "old", "2026-06-01T00:00:00Z"),
            ],
            now,
        );
        assert_eq!(state.results.len(), 1);
        state.period = FilterPeriod::All;
        state.recompute(now);
        assert_eq!(state.results.len(), 2);
    }

    #[test]
    fn filter_matches_body_substring() {
        let now = Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, 0).unwrap();
        let mut state = BulkFilterState::new(
            vec![
                message(1, "a", "b", "Deploy READY", "2026-07-20T00:00:00Z"),
                message(2, "a", "b", "waiting", "2026-07-20T00:00:00Z"),
            ],
            now,
        );
        state.body = "ready".to_owned();
        state.recompute(now);
        assert_eq!(state.selected_message().map(|row| row.id), Some(1));
    }

    #[test]
    fn preview_targets_group_unread_by_team_and_recipient() {
        let rows = vec![
            message(1, "a", "codex", "one", "2026-07-20T00:00:00Z"),
            message(2, "b", "codex", "two", "2026-07-20T00:00:00Z"),
            message(3, "b", "claude", "three", "2026-07-20T00:00:00Z"),
        ];
        let targets = mark_read_targets(rows.into_iter());
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets.iter().map(|target| target.units()).sum::<usize>(),
            3
        );
    }

    #[test]
    fn export_writes_selected_format_with_timestamped_name() {
        let temp = tempfile::tempdir().expect("tempdir");
        let now = Utc.with_ymd_and_hms(2026, 7, 21, 12, 34, 56).unwrap();
        let path = export_bulk_messages(
            temp.path(),
            &[message(1, "a", "b", "hello", "2026-07-20T00:00:00Z")],
            ExportFormat::Json,
            now,
        )
        .expect("export");
        assert_eq!(path.file_name().unwrap(), "agmsg-bulk-20260721-123456.json");
        assert!(
            fs::read_to_string(path)
                .unwrap()
                .contains("\"message_count\": 1")
        );
    }

    #[test]
    fn actas_lock_scan_decodes_owner_and_falls_back_on_bad_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(
            temp.path().join("actas.ops%20hub__codex-worker.session"),
            "session.123\n",
        )
        .expect("lock");
        assert_eq!(
            actas_lock_status(temp.path(), "ops hub", "codex-worker"),
            LockStatus::Owned("session.123".to_owned())
        );
        fs::write(temp.path().join("actas.bad.session"), "owner\n").expect("bad lock");
        assert_eq!(
            actas_lock_status(temp.path(), "missing", "agent"),
            LockStatus::ParseError
        );
    }
}
