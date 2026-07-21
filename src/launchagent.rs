//! Phase 14D — LaunchAgent toggle (HEALTH screen "Automation" section).
//!
//! Scope: exactly one hardcoded LaunchAgent
//! (`com.remma.agmsg-audit-daily`), not a general LaunchAgent manager.
//!
//! `probe()`/`probe_with_os()` gate macOS-only behavior with a **runtime**
//! `cfg!(target_os = "macos")` check rather than `#[cfg(target_os =
//! "macos")]` on the function itself, per the phase spec: the module (and
//! its tests) must still compile and run on Linux CI, only the returned
//! `LaState` differs there (`Unsupported`).
//!
//! All subprocess calls (`launchctl`, `plutil`) go through the injectable
//! [`CommandRunner`] trait — mirrors `exec::CommandResult`'s plain-fields
//! shape rather than `std::process::Output` specifically so tests can
//! construct results directly without spawning a real child process to get
//! a valid `ExitStatus`.

use std::path::{Path, PathBuf};
use std::process::Command;

use chrono::{DateTime, Datelike, Local, TimeZone};
use serde_json::Value;

/// The single LaunchAgent this screen controls (see module docs — v1 is
/// deliberately not a general-purpose manager).
pub const AUDIT_DAILY_LABEL: &str = "com.remma.agmsg-audit-daily";

/// Loaded/unloaded/missing/unsupported state of a LaunchAgent, as observed
/// via `launchctl list <label>` (or skipped entirely off-macOS).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaState {
    Loaded { pid: Option<i32> },
    Unloaded,
    Missing,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchAgentStatus {
    pub label: String,
    pub plist: Option<PathBuf>,
    pub state: LaState,
    pub next_run: Option<DateTime<Local>>,
}

impl LaunchAgentStatus {
    pub fn is_loaded(&self) -> bool {
        matches!(self.state, LaState::Loaded { .. })
    }

    /// `07-22 09:15` (empty when unknown) — used by `ui/health.rs`.
    pub fn next_run_label(&self) -> String {
        self.next_run
            .map(|next_run| next_run.format("%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "-".to_owned())
    }
}

/// Plain-fields subprocess result — see module docs for why this isn't
/// `std::process::Output`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Injectable process runner. Production uses [`RealCommandRunner`]; tests
/// substitute a fake so no real `launchctl`/`plutil` ever runs in CI.
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> RunOutput;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> RunOutput {
        match Command::new(program).args(args).output() {
            Ok(output) => RunOutput {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            },
            Err(error) => RunOutput {
                success: false,
                stdout: String::new(),
                stderr: error.to_string(),
            },
        }
    }
}

/// `~/Library/LaunchAgents/<label>.plist`.
pub fn default_plist_path(label: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_owned());
    PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"))
}

/// Probes `label`'s LaunchAgent state. Runtime-gated on macOS (see module
/// docs); use [`probe_with_os`] directly in tests to force either branch
/// regardless of the host OS actually running the test.
pub fn probe(runner: &dyn CommandRunner, label: &str, plist_path: &Path) -> LaunchAgentStatus {
    probe_with_os(cfg!(target_os = "macos"), runner, label, plist_path)
}

pub fn probe_with_os(
    is_macos: bool,
    runner: &dyn CommandRunner,
    label: &str,
    plist_path: &Path,
) -> LaunchAgentStatus {
    if !is_macos {
        return LaunchAgentStatus {
            label: label.to_owned(),
            plist: None,
            state: LaState::Unsupported,
            next_run: None,
        };
    }
    if !plist_path.is_file() {
        return LaunchAgentStatus {
            label: label.to_owned(),
            plist: None,
            state: LaState::Missing,
            next_run: None,
        };
    }

    let state = probe_loaded_state(runner, label);
    let next_run = probe_next_run(runner, plist_path);
    LaunchAgentStatus {
        label: label.to_owned(),
        plist: Some(plist_path.to_owned()),
        state,
        next_run,
    }
}

fn probe_loaded_state(runner: &dyn CommandRunner, label: &str) -> LaState {
    let output = runner.run("launchctl", &["list", label]);
    if !output.success {
        return LaState::Unloaded;
    }
    LaState::Loaded {
        pid: parse_launchctl_pid(&output.stdout),
    }
}

/// Extracts `"PID" = 12345;` from `launchctl list <label>` output. A loaded
/// but currently-idle scheduled job has no `PID` line at all — that's still
/// `Loaded { pid: None }`, not `Unloaded` (exit code, not PID presence, is
/// the loaded/unloaded signal).
pub fn parse_launchctl_pid(stdout: &str) -> Option<i32> {
    stdout.lines().find_map(|line| {
        let line = line.trim().trim_end_matches(';').trim();
        let rest = line.strip_prefix("\"PID\"")?;
        rest.trim().trim_start_matches('=').trim().parse().ok()
    })
}

fn probe_next_run(runner: &dyn CommandRunner, plist_path: &Path) -> Option<DateTime<Local>> {
    let path_str = plist_path.to_str()?;
    let output = runner.run("plutil", &["-convert", "json", "-o", "-", path_str]);
    if !output.success {
        return None;
    }
    let json: Value = serde_json::from_str(&output.stdout).ok()?;
    next_run_from_plist_json(&json, Local::now())
}

/// `StartCalendarInterval` is either a single dict or an array of dicts
/// (launchd runs at the earliest match across all of them); this returns
/// the soonest future occurrence across every interval, or `None` if the
/// key is absent/malformed (the row still renders — see `ui/health.rs`).
pub fn next_run_from_plist_json(json: &Value, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let interval = json.get("StartCalendarInterval")?;
    let intervals: Vec<&Value> = match interval {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![interval],
        _ => return None,
    };
    intervals
        .into_iter()
        .filter_map(|item| next_occurrence(item, now))
        .min()
}

/// Linear day-by-day scan (bounded to a year) rather than closed-form
/// cron-style "next" math: launchd's own `StartCalendarInterval` semantics
/// treat an unset field as a wildcard exactly like a plain filter would, so
/// scanning candidate days and checking each field independently is both
/// simpler and harder to get subtly wrong at month/DST boundaries than
/// reimplementing next-occurrence arithmetic per field.
fn next_occurrence(interval: &Value, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let obj = interval.as_object()?;
    let hour = field(obj, "Hour").unwrap_or(0);
    let minute = field(obj, "Minute").unwrap_or(0);
    let weekday = field(obj, "Weekday");
    let day = field(obj, "Day");
    let month = field(obj, "Month");
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }

    for offset in 0..366i64 {
        let date = now.date_naive().checked_add_signed(chrono::Duration::days(offset))?;
        let naive = date.and_hms_opt(hour as u32, minute as u32, 0)?;
        let Some(candidate) = now.timezone().from_local_datetime(&naive).single() else {
            continue;
        };
        if candidate <= now {
            continue;
        }
        let matches_weekday = weekday
            .map(|weekday| (weekday.rem_euclid(7)) as u32 == candidate.weekday().num_days_from_sunday())
            .unwrap_or(true);
        let matches_day = day.map(|day| i64::from(candidate.day()) == day).unwrap_or(true);
        let matches_month = month
            .map(|month| i64::from(candidate.month()) == month)
            .unwrap_or(true);
        if matches_weekday && matches_day && matches_month {
            return Some(candidate);
        }
    }
    None
}

fn field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    obj.get(key).and_then(Value::as_i64)
}

/// Issues `launchctl load -w <plist>` (if `currently_loaded` is false) or
/// `launchctl unload -w <plist>` (if true) and returns the raw outcome —
/// callers (`app.rs`) re-probe afterward rather than trusting the toggle's
/// own exit code to reflect the new state.
pub fn toggle(
    runner: &dyn CommandRunner,
    plist_path: &Path,
    currently_loaded: bool,
) -> Result<RunOutput, String> {
    let path_str = plist_path
        .to_str()
        .ok_or_else(|| "invalid plist path (not valid UTF-8)".to_owned())?;
    let verb = if currently_loaded { "unload" } else { "load" };
    Ok(runner.run("launchctl", &[verb, "-w", path_str]))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;

    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    /// Records every call it receives and returns a preprogrammed response
    /// keyed by the program name — no real `launchctl`/`plutil` ever runs.
    #[derive(Default)]
    struct FakeRunner {
        calls: RefCell<Vec<(String, Vec<String>)>>,
        launchctl_list: RunOutput,
        plutil: RunOutput,
        toggle_result: RunOutput,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> RunOutput {
            self.calls.borrow_mut().push((
                program.to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect(),
            ));
            match program {
                "plutil" => self.plutil.clone(),
                "launchctl" if args.first() == Some(&"list") => self.launchctl_list.clone(),
                "launchctl" => self.toggle_result.clone(),
                _ => RunOutput::default(),
            }
        }
    }

    fn plist_fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("{AUDIT_DAILY_LABEL}.plist"));
        fs::write(&path, "not read directly by probe(); plutil is faked").expect("plist fixture");
        (dir, path)
    }

    // --- launchctl list parsing -----------------------------------------

    #[test]
    fn parses_pid_from_loaded_with_pid_output() {
        let stdout = "{\n\t\"PID\" = 12345;\n\t\"Label\" = \"com.remma.agmsg-audit-daily\";\n}";
        assert_eq!(parse_launchctl_pid(stdout), Some(12345));
    }

    #[test]
    fn loaded_idle_has_no_pid_line() {
        let stdout = "{\n\t\"Label\" = \"com.remma.agmsg-audit-daily\";\n\t\"LastExitStatus\" = 0;\n}";
        assert_eq!(parse_launchctl_pid(stdout), None);
    }

    // --- StartCalendarInterval -> next-run -------------------------------

    #[test]
    fn single_interval_returns_next_matching_time_today_or_tomorrow() {
        let now = Local.with_ymd_and_hms(2026, 7, 20, 8, 0, 0).unwrap();
        let json = json!({ "StartCalendarInterval": { "Hour": 9, "Minute": 15 } });
        let next = next_run_from_plist_json(&json, now).expect("next run");
        assert_eq!(next.format("%Y-%m-%d %H:%M").to_string(), "2026-07-20 09:15");
    }

    #[test]
    fn single_interval_rolls_to_tomorrow_when_todays_slot_already_passed() {
        let now = Local.with_ymd_and_hms(2026, 7, 20, 10, 0, 0).unwrap();
        let json = json!({ "StartCalendarInterval": { "Hour": 9, "Minute": 15 } });
        let next = next_run_from_plist_json(&json, now).expect("next run");
        assert_eq!(next.format("%Y-%m-%d %H:%M").to_string(), "2026-07-21 09:15");
    }

    #[test]
    fn array_of_intervals_returns_soonest_across_all_of_them() {
        let now = Local.with_ymd_and_hms(2026, 7, 20, 8, 0, 0).unwrap();
        let json = json!({
            "StartCalendarInterval": [
                { "Hour": 22, "Minute": 0 },
                { "Hour": 9, "Minute": 15 }
            ]
        });
        let next = next_run_from_plist_json(&json, now).expect("next run");
        assert_eq!(next.format("%Y-%m-%d %H:%M").to_string(), "2026-07-20 09:15");
    }

    #[test]
    fn weekday_field_crossing_the_week_boundary() {
        // 2026-07-20 is a Monday; Weekday 0 (=Sunday per launchd) is 6 days out.
        let now = Local.with_ymd_and_hms(2026, 7, 20, 8, 0, 0).unwrap();
        let json = json!({ "StartCalendarInterval": { "Hour": 9, "Minute": 0, "Weekday": 0 } });
        let next = next_run_from_plist_json(&json, now).expect("next run");
        assert_eq!(next.weekday(), chrono::Weekday::Sun);
        assert!(next > now);
    }

    #[test]
    fn malformed_interval_returns_none() {
        assert_eq!(
            next_run_from_plist_json(&json!({ "StartCalendarInterval": "not-a-dict" }), Local::now()),
            None
        );
        assert_eq!(
            next_run_from_plist_json(&json!({ "StartCalendarInterval": { "Hour": 99 } }), Local::now()),
            None
        );
        assert_eq!(next_run_from_plist_json(&json!({}), Local::now()), None);
    }

    // --- probe(): full status assembly via the injected fake runner ------

    #[test]
    fn probe_reports_loaded_true_with_correctly_formatted_next_run() {
        let (_dir, plist_path) = plist_fixture();
        let runner = FakeRunner {
            launchctl_list: RunOutput {
                success: true,
                stdout: "{\n\t\"PID\" = 555;\n}".to_owned(),
                stderr: String::new(),
            },
            plutil: RunOutput {
                success: true,
                stdout: json!({ "StartCalendarInterval": { "Hour": 9, "Minute": 15 } })
                    .to_string(),
                stderr: String::new(),
            },
            ..Default::default()
        };

        let status = probe_with_os(true, &runner, AUDIT_DAILY_LABEL, &plist_path);

        assert!(status.is_loaded());
        assert_eq!(status.state, LaState::Loaded { pid: Some(555) });
        // Format contract: "MM-DD HH:MM" (see `LaunchAgentStatus::next_run_label`).
        assert_eq!(status.next_run_label().len(), 11);
        assert!(status.next_run.is_some());
    }

    #[test]
    fn probe_reports_missing_when_plist_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.plist");
        let runner = FakeRunner::default();
        let status = probe_with_os(true, &runner, AUDIT_DAILY_LABEL, &missing);
        assert_eq!(status.state, LaState::Missing);
        assert_eq!(status.next_run_label(), "-");
    }

    #[test]
    fn probe_reports_unloaded_on_nonzero_launchctl_list_exit() {
        let (_dir, plist_path) = plist_fixture();
        let runner = FakeRunner {
            launchctl_list: RunOutput {
                success: false,
                stdout: String::new(),
                stderr: "Could not find service".to_owned(),
            },
            ..Default::default()
        };
        let status = probe_with_os(true, &runner, AUDIT_DAILY_LABEL, &plist_path);
        assert_eq!(status.state, LaState::Unloaded);
    }

    /// Linux build path: forced via the `is_macos` flag exactly like a real
    /// non-macOS host would resolve through `probe()`'s own
    /// `cfg!(target_os = "macos")` check — no `#[cfg]` on the function, so
    /// this branch (and this test) compiles and runs on every OS.
    #[test]
    fn probe_reports_unsupported_off_macos_regardless_of_plist_or_runner() {
        let (_dir, plist_path) = plist_fixture();
        let runner = FakeRunner::default();
        let status = probe_with_os(false, &runner, AUDIT_DAILY_LABEL, &plist_path);
        assert_eq!(status.state, LaState::Unsupported);
        assert!(status.plist.is_none());
        assert!(runner.calls.borrow().is_empty(), "must not shell out when unsupported");
    }

    // --- toggle() ----------------------------------------------------------

    #[test]
    fn toggle_from_loaded_state_issues_launchctl_unload() {
        let (_dir, plist_path) = plist_fixture();
        let runner = FakeRunner {
            toggle_result: RunOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            },
            ..Default::default()
        };

        let result = toggle(&runner, &plist_path, true).expect("toggle");

        assert!(result.success);
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        let (program, args) = &calls[0];
        assert_eq!(program, "launchctl");
        assert_eq!(args[0], "unload");
        assert_eq!(args[1], "-w");
        assert_eq!(args[2], plist_path.to_str().unwrap());
    }

    #[test]
    fn toggle_from_unloaded_state_issues_launchctl_load() {
        let (_dir, plist_path) = plist_fixture();
        let runner = FakeRunner::default();

        toggle(&runner, &plist_path, false).expect("toggle");

        let calls = runner.calls.borrow();
        assert_eq!(calls[0].1[0], "load");
    }
}
