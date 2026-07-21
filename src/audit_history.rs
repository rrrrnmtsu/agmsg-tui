use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

pub const AUDIT_HISTORY_MAX_SAMPLES: usize = 30;
pub const AUDIT_HISTORY_DAYS: i64 = 30;
pub const CHATTER_TREND_DAYS: i64 = 7;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditHistorySample {
    pub timestamp: DateTime<Utc>,
    pub score: Option<usize>,
    pub total_msg: Option<usize>,
    pub unread: Option<usize>,
    pub body_p95: Option<usize>,
    pub asymmetric_pairs: Option<usize>,
    pub zombie_identities: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChatterTrend {
    pub start: usize,
    pub end: usize,
}

/// audit history は旧形式を含むため、現在の厳格な `AuditReport` とは分けて読む。
/// 壊れた1行は他の日次サンプルを巻き込まずに読み飛ばす。
pub fn read_audit_history(path: &Path) -> Result<Vec<AuditHistorySample>> {
    read_audit_history_at(path, Utc::now())
}

fn read_audit_history_at(path: &Path, now: DateTime<Utc>) -> Result<Vec<AuditHistorySample>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("audit historyを読めません: {}", path.display()));
        }
    };
    let mut samples = content
        .lines()
        .filter_map(parse_history_line)
        .collect::<Vec<_>>();
    samples.sort_by_key(|sample| sample.timestamp);
    let cutoff = now - Duration::days(AUDIT_HISTORY_DAYS);
    samples.retain(|sample| sample.timestamp >= cutoff && sample.timestamp <= now);
    if samples.len() > AUDIT_HISTORY_MAX_SAMPLES {
        samples.drain(..samples.len() - AUDIT_HISTORY_MAX_SAMPLES);
    }
    Ok(samples)
}

pub fn parse_history_line(line: &str) -> Option<AuditHistorySample> {
    let value: Value = serde_json::from_str(line).ok()?;
    let timestamp = string_field(&value, &["ts", "timestamp"])?;
    let timestamp = DateTime::parse_from_rfc3339(timestamp)
        .ok()?
        .with_timezone(&Utc);
    Some(AuditHistorySample {
        timestamp,
        score: number_field(&value, &["score", "total"]),
        total_msg: number_field(&value, &["total_msg", "msg"]),
        unread: number_field(&value, &["unread"]),
        body_p95: number_field(&value, &["body_p95"]),
        asymmetric_pairs: number_field(&value, &["asymmetric_pairs", "asym"]),
        zombie_identities: number_field(&value, &["zombie_identities", "zombies"]),
    })
}

/// 現在時刻を基準に7日を切り出し、単調増加または前後半の平均が
/// 50%超増えた場合だけwarningを返す。横ばいの0件列はtrendにしない。
pub fn detect_chatter_trend(samples: &[AuditHistorySample]) -> Option<ChatterTrend> {
    detect_chatter_trend_at(samples, Utc::now())
}

fn detect_chatter_trend_at(
    samples: &[AuditHistorySample],
    now: DateTime<Utc>,
) -> Option<ChatterTrend> {
    let cutoff = now - Duration::days(CHATTER_TREND_DAYS);
    let values = samples
        .iter()
        .filter(|sample| sample.timestamp >= cutoff && sample.timestamp <= now)
        .filter_map(|sample| sample.asymmetric_pairs)
        .collect::<Vec<_>>();
    if values.len() < 3 {
        return None;
    }
    let start = values[0];
    let end = *values.last()?;
    let monotonic = end > start && values.windows(2).all(|pair| pair[0] <= pair[1]);
    let middle = values.len() / 2;
    let (early, late) = values.split_at(middle);
    let early_sum = early.iter().map(|value| *value as u128).sum::<u128>();
    let late_sum = late.iter().map(|value| *value as u128).sum::<u128>();
    let ratio_increased = if early_sum == 0 {
        late_sum > 0
    } else {
        // late_avg > early_avg * 1.5 を整数演算で比較する。
        late_sum * early.len() as u128 * 2 > early_sum * late.len() as u128 * 3
    };
    if monotonic {
        return Some(ChatterTrend { start, end });
    }
    ratio_increased.then(|| ChatterTrend {
        start: rounded_average(early),
        end: rounded_average(late),
    })
}

fn rounded_average(values: &[usize]) -> usize {
    let sum = values.iter().map(|value| *value as u128).sum::<u128>();
    usize::try_from((sum + values.len() as u128 / 2) / values.len() as u128)
        .unwrap_or(usize::MAX)
}

fn string_field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
}

fn number_field(value: &Value, names: &[&str]) -> Option<usize> {
    names.iter().find_map(|name| {
        value
            .get(*name)
            .and_then(Value::as_u64)
            .and_then(|number| usize::try_from(number).ok())
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::{TimeZone, Utc};

    use super::{
        AUDIT_HISTORY_MAX_SAMPLES, AuditHistorySample, ChatterTrend, detect_chatter_trend_at,
        parse_history_line, read_audit_history_at,
    };

    fn sample(day: u32, asymmetry: usize) -> AuditHistorySample {
        AuditHistorySample {
            timestamp: Utc
                .with_ymd_and_hms(2026, 7, day, 9, 0, 0)
                .single()
                .expect("timestamp"),
            score: Some(80),
            total_msg: Some(700),
            unread: Some(10),
            body_p95: Some(2_000),
            asymmetric_pairs: Some(asymmetry),
            zombie_identities: Some(0),
        }
    }

    #[test]
    fn parses_legacy_aliases_and_partial_metrics() {
        let old = parse_history_line(r#"{"timestamp":"2026-07-20T07:32:54Z","total":48}"#)
            .expect("old sample");
        assert_eq!(old.score, Some(48));
        assert_eq!(old.total_msg, None);

        let compact = parse_history_line(
            r#"{"ts":"2026-07-20T09:34:30Z","score":74,"msg":739,"zombies":4,"asym":2}"#,
        )
        .expect("compact sample");
        assert_eq!(compact.total_msg, Some(739));
        assert_eq!(compact.zombie_identities, Some(4));
        assert_eq!(compact.asymmetric_pairs, Some(2));
        assert!(parse_history_line("not json").is_none());
    }

    #[test]
    fn reader_skips_invalid_lines_and_caps_tail_to_thirty_samples() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("audit.jsonl");
        let mut lines = vec!["broken".to_owned()];
        for day in 1..=31 {
            lines.push(format!(
                r#"{{"ts":"2026-07-{day:02}T09:00:00Z","score":{day}}}"#
            ));
        }
        fs::write(&path, lines.join("\n")).expect("fixture");
        let now = Utc
            .with_ymd_and_hms(2026, 7, 31, 9, 0, 0)
            .single()
            .expect("now");
        let history = read_audit_history_at(&path, now).expect("history");
        assert_eq!(history.len(), AUDIT_HISTORY_MAX_SAMPLES);
        assert_eq!(history[0].score, Some(2));
        assert_eq!(history.last().and_then(|row| row.score), Some(31));
    }

    #[test]
    fn detects_monotonic_and_average_ratio_chatter_trends() {
        let now = Utc
            .with_ymd_and_hms(2026, 7, 21, 9, 0, 0)
            .single()
            .expect("now");
        assert_eq!(
            detect_chatter_trend_at(&[sample(15, 1), sample(17, 2), sample(20, 4)], now),
            Some(ChatterTrend { start: 1, end: 4 })
        );
        assert_eq!(
            detect_chatter_trend_at(
                &[sample(15, 2), sample(16, 1), sample(17, 5), sample(20, 6)],
                now,
            ),
            Some(ChatterTrend { start: 2, end: 6 })
        );
        assert_eq!(
            detect_chatter_trend_at(&[sample(15, 0), sample(17, 0), sample(20, 0)], now),
            None
        );
        assert_eq!(
            detect_chatter_trend_at(
                &[
                    sample(15, 1),
                    sample(16, 1),
                    sample(17, 10),
                    sample(18, 10),
                    sample(20, 1),
                ],
                now,
            ),
            Some(ChatterTrend { start: 1, end: 7 })
        );
    }
}
