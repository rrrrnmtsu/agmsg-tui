//! Phase 8 notification primitives: BEL, OSC 9 desktop notifications, tmux
//! terminal title, and burst detection. Follows `clipboard.rs`'s split — pure
//! logic (throttle windows, burst counting, sequence formatting) is kept
//! separate from the `crossterm::execute!` calls so it can be unit tested
//! without a real terminal, and `App` stays IO-free (it only *decides* what
//! to notify; `main.rs` is the one place that writes to stdout).

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::execute;
use crossterm::style::Print;
use serde::{Deserialize, Serialize};

/// Rolling window used to detect a message burst.
pub const BURST_WINDOW: Duration = Duration::from_secs(60);
/// More than this many messages inside `BURST_WINDOW` counts as a burst.
pub const BURST_THRESHOLD: usize = 20;
/// How long the burst banner stays on screen once triggered.
pub const BURST_ALERT_DURATION: Duration = Duration::from_secs(3);
/// Minimum gap between two OSC 9 desktop notifications — new-message floods
/// (e.g. a reconnect replay) would otherwise spam the OS notification center.
pub const OSC9_THROTTLE: Duration = Duration::from_secs(5);
/// OSC 9 body preview is capped here so long messages don't blow past what
/// terminal notification centers render sanely.
const OSC9_PREVIEW_CHARS: usize = 60;

pub const NOTIFY_SETTING_COUNT: usize = 4;
pub const NOTIFY_SETTING_LABELS: [&str; NOTIFY_SETTING_COUNT] = [
    "Terminal bell on unread",
    "Desktop notification (OSC 9)",
    "Terminal title unread count",
    "Burst alert",
];

/// Toggle state for the `Ctrl+N` settings popup. `bell` doubles as the
/// `Ctrl+B` mute target — one flag, two entry points, so the popup checkbox
/// and the quick-mute key can never disagree about whether the bell is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifySettings {
    pub bell: bool,
    pub desktop: bool,
    pub title: bool,
    pub burst_alert: bool,
}

impl Default for NotifySettings {
    fn default() -> Self {
        Self {
            bell: true,
            desktop: true,
            title: true,
            burst_alert: true,
        }
    }
}

impl NotifySettings {
    pub fn is_enabled(&self, index: usize) -> bool {
        match index {
            0 => self.bell,
            1 => self.desktop,
            2 => self.title,
            3 => self.burst_alert,
            _ => false,
        }
    }

    pub fn toggle(&mut self, index: usize) {
        match index {
            0 => self.bell = !self.bell,
            1 => self.desktop = !self.desktop,
            2 => self.title = !self.title,
            3 => self.burst_alert = !self.burst_alert,
            _ => {}
        }
    }
}

/// A notification `App::receive_new_messages` decided to fire, deferred to
/// `main.rs` so `App` itself performs no IO (mirrors `AppAction::Yank`, which
/// defers the `pbcopy`/OSC 52 write the same way).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingNotification {
    Bell,
    Desktop { from: String, body: String },
}

/// Counts message arrivals in a rolling 60s window to detect a burst.
#[derive(Debug, Default)]
pub struct BurstTracker {
    events: VecDeque<Instant>,
}

impl BurstTracker {
    pub fn new() -> Self {
        Self {
            events: VecDeque::new(),
        }
    }

    /// Records `count` arrivals at `now` and prunes anything older than
    /// `BURST_WINDOW`. Returns `Some(total_in_window)` once the window holds
    /// more than `BURST_THRESHOLD` events, else `None`.
    pub fn record(&mut self, count: usize, now: Instant) -> Option<usize> {
        for _ in 0..count {
            self.events.push_back(now);
        }
        self.prune(now);
        if self.events.len() > BURST_THRESHOLD {
            Some(self.events.len())
        } else {
            None
        }
    }

    fn prune(&mut self, now: Instant) {
        while let Some(&front) = self.events.front() {
            if now.saturating_duration_since(front) > BURST_WINDOW {
                self.events.pop_front();
            } else {
                break;
            }
        }
    }
}

/// Owns the throttle/dedup state for raw-sequence writes. Instantiated once
/// in `main.rs`'s event loop (not per-App, since it wraps a stdout handle
/// concern, not application state that snapshot tests need to assert on).
#[derive(Debug, Default)]
pub struct NotificationSink {
    last_osc9: Option<Instant>,
    last_title: Option<usize>,
}

impl NotificationSink {
    pub fn new() -> Self {
        Self {
            last_osc9: None,
            last_title: None,
        }
    }

    /// `true` when enough time has passed since the last OSC 9 write to emit
    /// another one. Split out from `emit_osc9` so the throttle window itself
    /// is testable without spawning a real clock/terminal.
    pub fn should_emit_osc9(&self, now: Instant) -> bool {
        match self.last_osc9 {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= OSC9_THROTTLE,
        }
    }

    pub fn emit_osc9(&mut self, from: &str, body: &str) -> Result<bool> {
        self.emit_osc9_at(from, body, Instant::now())
    }

    fn emit_osc9_at(&mut self, from: &str, body: &str, now: Instant) -> Result<bool> {
        if !self.should_emit_osc9(now) {
            return Ok(false);
        }
        self.last_osc9 = Some(now);
        execute!(io::stdout(), Print(osc9_sequence(from, body)))
            .context("OSC 9通知を書き込めません")?;
        Ok(true)
    }

    /// Writes the tmux/terminal title only when the unread count actually
    /// changed since the last write — every redraw would otherwise hammer
    /// stdout with an identical escape sequence.
    pub fn set_title_if_changed(&mut self, unread: usize) -> Result<bool> {
        if self.last_title == Some(unread) {
            return Ok(false);
        }
        self.last_title = Some(unread);
        execute!(io::stdout(), Print(title_sequence(unread))).context("タイトルを書き込めません")?;
        Ok(true)
    }
}

pub fn emit_bell() -> Result<()> {
    execute!(io::stdout(), Print("\x07")).context("ベルを書き込めません")?;
    Ok(())
}

/// `\x1b]0;agmsg-tui [N]\x07`, or the bare title when `unread == 0` — no
/// badge means no unread, not a "[0]" the user has to parse.
pub fn title_sequence(unread: usize) -> String {
    if unread == 0 {
        "\x1b]0;agmsg-tui\x07".to_owned()
    } else {
        format!("\x1b]0;agmsg-tui [{unread}]\x07")
    }
}

/// `\x1b]9;<from>: <preview>\x07`. `from`/`body` come from message content —
/// another agent's `from_agent`/body — so control characters are stripped
/// before they land inside our own escape sequence; otherwise a message
/// containing `\x1b]0;pwned\x07` would let a peer agent rewrite our terminal
/// title or worse via the "trusted" notification path.
pub fn osc9_sequence(from: &str, body: &str) -> String {
    let from = sanitize(from);
    let preview = sanitize(&truncate_preview(body, OSC9_PREVIEW_CHARS));
    format!("\x1b]9;{from}: {preview}\x07")
}

fn truncate_preview(body: &str, max_chars: usize) -> String {
    let flattened: String = body.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    let char_count = flattened.chars().count();
    if char_count <= max_chars {
        return flattened;
    }
    let mut truncated: String = flattened.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

fn sanitize(input: &str) -> String {
    input.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_tracker_stays_quiet_under_threshold() {
        let mut tracker = BurstTracker::new();
        let now = Instant::now();
        assert_eq!(tracker.record(20, now), None);
    }

    #[test]
    fn burst_tracker_fires_once_threshold_exceeded() {
        let mut tracker = BurstTracker::new();
        let now = Instant::now();
        assert_eq!(tracker.record(21, now), Some(21));
    }

    #[test]
    fn burst_tracker_prunes_events_outside_the_window() {
        let mut tracker = BurstTracker::new();
        let start = Instant::now();
        assert_eq!(tracker.record(21, start), Some(21));
        let later = start + BURST_WINDOW + Duration::from_secs(1);
        // The first 21 events are now outside the 60s window, so a single
        // fresh arrival should not still read as a burst.
        assert_eq!(tracker.record(1, later), None);
    }

    #[test]
    fn burst_tracker_accumulates_across_multiple_batches() {
        let mut tracker = BurstTracker::new();
        let now = Instant::now();
        assert_eq!(tracker.record(15, now), None);
        assert_eq!(tracker.record(6, now + Duration::from_secs(1)), Some(21));
    }

    #[test]
    fn osc9_throttle_blocks_within_the_window_and_allows_after() {
        let mut sink = NotificationSink::new();
        let start = Instant::now();
        assert!(sink.should_emit_osc9(start));
        sink.last_osc9 = Some(start);
        assert!(!sink.should_emit_osc9(start + Duration::from_secs(4)));
        assert!(sink.should_emit_osc9(start + OSC9_THROTTLE));
    }

    #[test]
    fn title_sequence_hides_badge_at_zero_unread() {
        assert_eq!(title_sequence(0), "\x1b]0;agmsg-tui\x07");
        assert_eq!(title_sequence(3), "\x1b]0;agmsg-tui [3]\x07");
    }

    #[test]
    fn title_sequence_updates_for_large_counts() {
        assert_eq!(title_sequence(142), "\x1b]0;agmsg-tui [142]\x07");
    }

    #[test]
    fn osc9_sequence_strips_control_characters_to_block_injection() {
        // The literal text may survive (it's inert without its ESC/BEL
        // delimiters) — what matters is that no *new* escape or bell byte
        // makes it into our own sequence, which would let a message body
        // terminate our OSC 9 early and inject an attacker-controlled one.
        let seq = osc9_sequence("codex\x1b]0;pwned\x07", "hello\x07world");
        assert_eq!(seq, "\x1b]9;codex]0;pwned: helloworld\x07");
        assert_eq!(seq.matches('\x1b').count(), 1);
        assert_eq!(seq.matches('\x07').count(), 1);
    }

    #[test]
    fn osc9_sequence_truncates_long_bodies_with_ellipsis() {
        let body = "a".repeat(200);
        let seq = osc9_sequence("claude-main", &body);
        assert!(seq.contains('…'));
        assert!(seq.chars().count() < 200);
    }

    #[test]
    fn osc9_sequence_flattens_newlines_in_preview() {
        let seq = osc9_sequence("claude-main", "line one\nline two");
        assert_eq!(seq, "\x1b]9;claude-main: line one line two\x07");
    }

    #[test]
    fn notify_settings_default_all_on() {
        let settings = NotifySettings::default();
        for index in 0..NOTIFY_SETTING_COUNT {
            assert!(settings.is_enabled(index));
        }
    }

    #[test]
    fn notify_settings_toggle_flips_only_the_targeted_flag() {
        let mut settings = NotifySettings::default();
        settings.toggle(1);
        assert!(!settings.desktop);
        assert!(settings.bell);
        assert!(settings.title);
        assert!(settings.burst_alert);
    }
}
