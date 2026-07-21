use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::db::{PairMatrix, StaleUnread, ZombieIdentity};

pub const AXIS_ORDER: [&str; 10] = [
    "team_naming",
    "agent_naming",
    "body_size",
    "burst_control",
    "loop_prevention",
    "unread_hygiene",
    "zombie_cleanup",
    "state_hygiene",
    "traffic_spread",
    "activity",
];

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AuditAxis {
    pub score: u16,
    pub note: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AuditReport {
    pub ts: String,
    pub window_days: u32,
    pub score: u16,
    pub total_msg: usize,
    pub total_teams: usize,
    pub total_agents: usize,
    pub unread: usize,
    pub unread_stale: usize,
    pub body_p95: usize,
    pub burst_days: usize,
    pub asymmetric_pairs: usize,
    pub zombie_identities: usize,
    pub stale_run_files: usize,
    pub max_team_pct: usize,
    pub axes: BTreeMap<String, AuditAxis>,
}

pub fn parse_audit_stdout(stdout: &str) -> Result<AuditReport> {
    let json = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with('{') && line.ends_with('}'))
        .context("agmsg-audit のJSON出力が見つかりません")?;
    let report: AuditReport =
        serde_json::from_str(json).context("agmsg-audit のJSONを解析できません")?;
    anyhow::ensure!(
        report.axes.len() == AXIS_ORDER.len()
            && AXIS_ORDER
                .iter()
                .all(|name| report.axes.contains_key(*name)),
        "agmsg-audit の10軸が揃っていません"
    );
    Ok(report)
}

pub fn report_stamp(timestamp: &str) -> Result<String> {
    let digits: String = timestamp
        .chars()
        .filter(char::is_ascii_digit)
        .take(12)
        .collect();
    anyhow::ensure!(
        digits.len() == 12,
        "audit timestamp が不正です: {timestamp}"
    );
    Ok(format!("{}-{}", &digits[..8], &digits[8..]))
}

pub fn build_markdown(
    report: &AuditReport,
    matrices: &[PairMatrix],
    zombies: &[ZombieIdentity],
    stale_unreads: &[StaleUnread],
) -> String {
    let mut output = format!(
        "# agmsg audit report\n\n- Generated: {}\n- Window: {} days\n- Total score: **{}/100**\n\n",
        report.ts, report.window_days, report.score
    );
    output.push_str("## Audit axes\n\n| Axis | Score | Note |\n|---|---:|---|\n");
    for name in AXIS_ORDER {
        if let Some(axis) = report.axes.get(name) {
            output.push_str(&format!(
                "| {} | {}/10 | {} |\n",
                name,
                axis.score,
                escape_markdown(&axis.note)
            ));
        }
    }
    output.push_str(&format!(
        "\n## Summary stats\n\n- Messages: {}\n- Teams: {}\n- Agents: {}\n- Unread: {} (stale: {})\n- Body p95: {}B\n- Burst days: {}\n- Asymmetric pairs: {}\n- Zombie identities: {}\n- Stale run files: {}\n- Largest team share: {}%\n",
        report.total_msg,
        report.total_teams,
        report.total_agents,
        report.unread,
        report.unread_stale,
        report.body_p95,
        report.burst_days,
        report.asymmetric_pairs,
        report.zombie_identities,
        report.stale_run_files,
        report.max_team_pct
    ));
    output.push_str("\n## Pair matrices (30 days)\n");
    if matrices.is_empty() {
        output.push_str("\nNo pair traffic.\n");
    }
    for matrix in matrices {
        output.push_str(&format!(
            "\n### {}\n\n| from \\ to |",
            escape_markdown(&matrix.team)
        ));
        for agent in &matrix.agents {
            output.push_str(&format!(" {} |", escape_markdown(agent)));
        }
        output.push_str("\n|---|");
        for _ in &matrix.agents {
            output.push_str("---:|");
        }
        output.push('\n');
        for from in &matrix.agents {
            output.push_str(&format!("| {} |", escape_markdown(from)));
            for to in &matrix.agents {
                output.push_str(&format!(" {} |", matrix.count(from, to)));
            }
            output.push('\n');
        }
    }
    output.push_str("\n## Zombie identities\n");
    if zombies.is_empty() {
        output.push_str("\nNone.\n");
    } else {
        for zombie in zombies {
            output.push_str(&format!(
                "\n- `{}/{}` (no traffic in 30 days)",
                zombie.team, zombie.agent
            ));
        }
        output.push('\n');
    }
    output.push_str("\n## Stale unread messages\n");
    if stale_unreads.is_empty() {
        output.push_str("\nNone.\n");
    } else {
        for stale in stale_unreads {
            output.push_str(&format!(
                "\n- #{} `{}` {} -> {} at {}: {}",
                stale.id,
                stale.team,
                stale.from_agent,
                stale.to_agent,
                stale.created_at,
                escape_markdown(&single_line(&stale.body, 120))
            ));
        }
        output.push('\n');
    }
    output
}

fn single_line(value: &str, max_chars: usize) -> String {
    let flattened = value.replace(['\n', '\r'], " ");
    let mut chars = flattened.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

fn escape_markdown(value: &str) -> String {
    value.replace('|', "\\|").replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::{build_markdown, parse_audit_stdout, report_stamp};

    const SAMPLE: &str = r#"{"ts":"2026-07-20T11:29:58Z","window_days":30,"score":83,"total_msg":739,"total_teams":5,"total_agents":26,"unread":23,"unread_stale":13,"body_p95":2396,"burst_days":0,"asymmetric_pairs":0,"zombie_identities":0,"stale_run_files":0,"max_team_pct":46,"axes":{"team_naming":{"score":10,"note":"OK"},"agent_naming":{"score":4,"note":"3 anonymous"},"body_size":{"score":7,"note":"soft-over"},"burst_control":{"score":10,"note":"OK"},"loop_prevention":{"score":10,"note":"OK"},"unread_hygiene":{"score":2,"note":"13 stale"},"zombie_cleanup":{"score":10,"note":"OK"},"state_hygiene":{"score":10,"note":"OK"},"traffic_spread":{"score":10,"note":"max 46%"},"activity":{"score":10,"note":"739 msgs"}}}"#;

    #[test]
    fn parses_json_from_last_stdout_line() {
        let report = parse_audit_stdout(&format!("diagnostic\n{SAMPLE}\n")).expect("report");
        assert_eq!(report.score, 83);
        assert_eq!(report.axes.len(), 10);
    }

    #[test]
    fn report_stamp_uses_required_filename_shape() {
        assert_eq!(
            report_stamp("2026-07-20T11:29:58Z").expect("stamp"),
            "20260720-1129"
        );
    }

    #[test]
    fn markdown_contains_audit_and_stats_sections() {
        let report = parse_audit_stdout(SAMPLE).expect("report");
        let markdown = build_markdown(&report, &[], &[], &[]);
        assert!(markdown.contains("Total score: **83/100**"));
        assert!(markdown.contains("## Summary stats"));
        assert!(markdown.contains("## Pair matrices (30 days)"));
    }
}
