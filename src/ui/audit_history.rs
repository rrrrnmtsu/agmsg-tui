use chrono::Duration;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{BarChart, Block, Borders, Paragraph, Sparkline};

use crate::app::App;
use crate::audit_history::{AuditHistorySample, detect_chatter_trend};
use crate::db::BODY_SIZE_BUCKET_LABELS;

use super::NARROW_WIDTH_THRESHOLD;

struct MetricSeries {
    values: Vec<u64>,
    first_index: usize,
    last_index: usize,
    sample_count: usize,
}

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let title = format!(
        " AUDIT | D:DASHBOARD  [H] HISTORY | {} samples ",
        app.audit_history.len()
    );
    let block = Block::default()
        .title(title)
        .title_bottom(" D:dashboard  H:history  R:refresh  E:export  Ctrl-A/Tab/Esc:main  ?:help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(frame.area());
    frame.render_widget(block, frame.area());

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(10),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(inner);
    render_time_series(frame, app, rows[0]);
    frame.render_widget(
        Paragraph::new(axis_labels(&app.audit_history, usize::from(rows[1].width)))
            .style(Style::default().fg(Color::DarkGray)),
        rows[1],
    );
    render_chatter_annotation(frame, app, rows[2]);
    render_histogram(frame, app, rows[3]);
    render_status(frame, app, rows[4]);
}

fn render_time_series(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if app.audit_history.len() < 3 {
        frame.render_widget(
            Paragraph::new(format!(
                "insufficient history (min 3 samples, currently {})",
                app.audit_history.len()
            ))
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::Yellow)),
            area,
        );
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2); 5])
        .split(area);
    render_metric(
        frame,
        "score",
        metric_values(&app.audit_history, |sample| sample.score),
        rows[0],
    );
    render_metric(
        frame,
        "total_msg",
        metric_values(&app.audit_history, |sample| sample.total_msg),
        rows[1],
    );
    render_metric(
        frame,
        "unread",
        metric_values(&app.audit_history, |sample| sample.unread),
        rows[2],
    );
    render_metric(
        frame,
        "body_p95",
        metric_values(&app.audit_history, |sample| sample.body_p95),
        rows[3],
    );
    render_metric(
        frame,
        "zombies",
        metric_values(&app.audit_history, |sample| sample.zombie_identities),
        rows[4],
    );
}

fn metric_values(
    history: &[AuditHistorySample],
    field: impl Fn(&AuditHistorySample) -> Option<usize>,
) -> MetricSeries {
    let indexed = history
        .iter()
        .enumerate()
        .filter_map(|(index, sample)| field(sample).map(|value| (index, value)))
        .collect::<Vec<_>>();
    MetricSeries {
        first_index: indexed.first().map(|(index, _)| *index).unwrap_or_default(),
        last_index: indexed.last().map(|(index, _)| *index).unwrap_or_default(),
        values: indexed
            .into_iter()
            .map(|(_, value)| u64::try_from(value).unwrap_or(u64::MAX))
            .collect(),
        sample_count: history.len(),
    }
}

fn render_metric(frame: &mut Frame<'_>, label: &str, series: MetricSeries, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);
    let Some(minimum) = series.values.iter().copied().min() else {
        frame.render_widget(Paragraph::new(format!("{label:<11} n/a")), rows[0]);
        return;
    };
    let maximum = series.values.iter().copied().max().unwrap_or(minimum);
    let latest = series.values.last().copied().unwrap_or_default();
    frame.render_widget(
        Paragraph::new(format!(
            "{label:<11} y:{minimum}..{maximum}  latest:{latest}"
        )),
        rows[0],
    );
    let normalized = if minimum == maximum {
        vec![1; series.values.len()]
    } else {
        series
            .values
            .iter()
            .map(|value| value - minimum)
            .collect()
    };
    let timeline_width = usize::from(rows[1].width);
    let denominator = series.sample_count.saturating_sub(1).max(1);
    let start = series.first_index * timeline_width.saturating_sub(1) / denominator;
    let end = series.last_index * timeline_width.saturating_sub(1) / denominator;
    let chart_width = end.saturating_sub(start).saturating_add(1).max(1);
    let visible = resample_values(&normalized, chart_width);
    let chart_area = Rect {
        x: rows[1].x.saturating_add(u16::try_from(start).unwrap_or(u16::MAX)),
        y: rows[1].y,
        width: u16::try_from(chart_width).unwrap_or(rows[1].width),
        height: rows[1].height,
    };
    frame.render_widget(
        Sparkline::default()
            .data(&visible)
            .max(maximum.saturating_sub(minimum).max(1))
            .style(Style::default().fg(Color::Cyan)),
        chart_area,
    );
}

fn render_chatter_annotation(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if let Some(trend) = detect_chatter_trend(&app.audit_history) {
        frame.render_widget(
            Paragraph::new(format!(
                "⚠ chatter loop trend: {}→{} pairs (7d)",
                trend.start, trend.end
            ))
            .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            area,
        );
    } else {
        frame.render_widget(
            Paragraph::new("chatter loop trend: none (7d)")
                .style(Style::default().fg(Color::DarkGray)),
            area,
        );
    }
}

fn render_histogram(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let distribution = &app.body_size_distribution;
    let title = format!(
        " BODY SIZE — all {} msgs | p50 {} p95 {} p99 {} ",
        distribution.total, distribution.p50, distribution.p95, distribution.p99
    );
    let block = Block::default().title(title).borders(Borders::ALL);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if area.width < NARROW_WIDTH_THRESHOLD {
        let maximum = distribution
            .buckets
            .iter()
            .copied()
            .max()
            .unwrap_or(1)
            .max(1);
        let lines = BODY_SIZE_BUCKET_LABELS
            .iter()
            .zip(distribution.buckets)
            .map(|(label, count)| {
                let width = if count == 0 {
                    0
                } else {
                    (count * 10).div_ceil(maximum).max(1)
                };
                Line::from(vec![
                    Span::raw(format!("{label:<10} {count:>4} ")),
                    Span::styled("█".repeat(width), Style::default().fg(Color::Cyan)),
                ])
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }
    let data = BODY_SIZE_BUCKET_LABELS
        .iter()
        .zip(distribution.buckets)
        .map(|(label, count)| (*label, u64::try_from(count).unwrap_or(u64::MAX)))
        .collect::<Vec<_>>();
    frame.render_widget(
        BarChart::default()
            .data(&data)
            .bar_width(9)
            .bar_gap(1)
            .bar_style(Style::default().fg(Color::Cyan))
            .value_style(Style::default().fg(Color::White)),
        inner,
    );
}

fn render_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if let Some(trend) = detect_chatter_trend(&app.audit_history) {
        frame.render_widget(
            Paragraph::new(format!(
                "⚠ chatter loop trend: {}→{} pairs (7d) | {}",
                trend.start, trend.end, app.status.text
            ))
            .style(Style::default().fg(Color::Red)),
            area,
        );
        return;
    }
    let style = if app.status.is_error || app.poll_offline {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    };
    frame.render_widget(Paragraph::new(app.status.text.as_str()).style(style), area);
}

fn resample_values(values: &[u64], width: usize) -> Vec<u64> {
    if values.is_empty() || width == 0 {
        return Vec::new();
    }
    if width == 1 {
        return vec![*values.last().unwrap_or(&0)];
    }
    (0..width)
        .map(|column| {
            let index = column * values.len().saturating_sub(1) / width.saturating_sub(1);
            values[index]
        })
        .collect()
}

fn axis_labels(history: &[AuditHistorySample], width: usize) -> String {
    if history.is_empty() || width == 0 {
        return String::new();
    }
    let first = history[0].timestamp.date_naive();
    let last = history
        .last()
        .map(|sample| sample.timestamp.date_naive())
        .unwrap_or(first);
    let step_days = if last.signed_duration_since(first).num_days() <= 7 {
        1
    } else {
        7
    };
    let mut labels = Vec::<(usize, String)>::new();
    let mut next_date = first;
    for (index, sample) in history.iter().enumerate() {
        let date = sample.timestamp.date_naive();
        if labels.is_empty() || date >= next_date {
            labels.push((index, date.format("%-m/%-d").to_string()));
            next_date = date + Duration::days(step_days);
        }
    }
    let last_index = history.len() - 1;
    if labels.last().is_none_or(|(index, _)| *index != last_index) {
        labels.push((last_index, last.format("%-m/%-d").to_string()));
    }
    let mut output = vec![b' '; width];
    let mut occupied_until = 0;
    for (index, label) in labels {
        if label.len() > width {
            continue;
        }
        let raw_position = if history.len() == 1 {
            0
        } else {
            index * width.saturating_sub(1) / (history.len() - 1)
        };
        let position = raw_position.min(width - label.len());
        if position < occupied_until && position != 0 {
            continue;
        }
        output[position..position + label.len()].copy_from_slice(label.as_bytes());
        occupied_until = position + label.len() + 1;
    }
    String::from_utf8(output).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::{axis_labels, metric_values, resample_values};
    use crate::audit_history::AuditHistorySample;

    #[test]
    fn axis_labels_use_daily_then_weekly_dates() {
        let sample = |day| AuditHistorySample {
            timestamp: Utc
                .with_ymd_and_hms(2026, 7, day, 9, 0, 0)
                .single()
                .expect("timestamp"),
            score: Some(80),
            total_msg: None,
            unread: None,
            body_p95: None,
            asymmetric_pairs: None,
            zombie_identities: None,
        };
        let daily = axis_labels(&[sample(20), sample(21)], 30);
        assert!(daily.contains("7/20"));
        assert!(daily.contains("7/21"));
        let weekly = axis_labels(&[sample(1), sample(8), sample(15), sample(22)], 50);
        assert!(weekly.contains("7/1"));
        assert!(weekly.contains("7/8"));
        assert!(weekly.contains("7/22"));
    }

    #[test]
    fn metric_series_keeps_missing_prefix_and_resamples_to_chart_width() {
        let sample = |day, score| AuditHistorySample {
            timestamp: Utc
                .with_ymd_and_hms(2026, 7, day, 9, 0, 0)
                .single()
                .expect("timestamp"),
            score,
            total_msg: None,
            unread: None,
            body_p95: None,
            asymmetric_pairs: None,
            zombie_identities: None,
        };
        let series = metric_values(
            &[sample(19, None), sample(20, Some(3)), sample(21, Some(9))],
            |row| row.score,
        );
        assert_eq!(series.first_index, 1);
        assert_eq!(series.last_index, 2);
        assert_eq!(series.sample_count, 3);
        assert_eq!(resample_values(&series.values, 5), vec![3, 3, 3, 3, 9]);
    }
}
