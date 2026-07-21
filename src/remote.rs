//! Phase 14B — multi-host DB read-only federation.
//!
//! One dashboard shows local + configured remote hosts' agmsg traffic. This
//! module owns three concerns kept deliberately separate:
//!
//! 1. `HostsFile` — parses `~/.config/agmsg-tui/hosts.toml`.
//! 2. `SnapshotFetcher` / `fetch_snapshot` — the snapshot-copy pipeline
//!    (`ssh <host> "cat <db_path>"` → tmp file → `PRAGMA quick_check` →
//!    atomic rename). Testable without real ssh via `FakeFetcher`, which
//!    swaps in an arbitrary `sh -c` command.
//! 3. `HostRuntime` / `HostStatus` — the merged config+status the rest of
//!    the app renders and routes queries through. `db.rs` stays unaware of
//!    hosts entirely (see `crate::db::Database::team_summaries_for_host`);
//!    this module tags rows with the host label instead.
//!
//! Fetch tasks run on their own tokio interval per host and never touch the
//! render/input path (invariant #2 in the phase-14 plan doc) — `app.rs`
//! only ever consumes already-written snapshot files and `HostFetchOutcome`
//! events, matching the existing audit/health background-task pattern in
//! `main.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::timeout;

use crate::db::Database;

/// Reserved host label for the local `~/.agents/agmsg.db` connection.
/// `hosts.toml` entries using this name are rejected at parse time so a
/// team's `host` field unambiguously distinguishes local from remote.
pub const LOCAL_HOST_ID: &str = "local";

const DEFAULT_REFRESH_SECS: u64 = 300;
const MIN_REFRESH_SECS: u64 = 60;
/// `-o ConnectTimeout=5` on the ssh side; see plan doc "ssh timeout hard-capped".
const SSH_CONNECT_TIMEOUT_SECS: u64 = 5;
/// Outer tokio timeout wrapping the whole fetch so a wedged ssh (past its
/// own ConnectTimeout, e.g. hung post-auth) can't stall the fetch task.
const FETCH_TIMEOUT_SECS: u64 = 30;
/// N=3 consecutive failures before a host is reported `Offline` (health
/// alert threshold); 1 failure is `Degraded` (team-list badge dims but
/// stays labeled with the last-good snapshot).
const OFFLINE_FAILURE_THRESHOLD: u32 = 3;

/// One `[[hosts]]` entry from `hosts.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct HostConfig {
    pub name: String,
    /// Anything `ssh <ssh>` accepts (host alias from `~/.ssh/config`, or
    /// `user@host`).
    pub ssh: String,
    #[serde(default = "default_db_path")]
    pub db_path: String,
}

fn default_db_path() -> String {
    "~/.agents/agmsg.db".to_owned()
}

#[derive(Debug, Default, Deserialize)]
struct HostsFileRaw {
    #[serde(default)]
    refresh_secs: Option<u64>,
    #[serde(default)]
    hosts: Vec<HostConfig>,
}

/// Parsed + validated `hosts.toml`. Missing file ⇒ empty `hosts` (local-only
/// mode, zero behavior change) with no warning; malformed file or malformed
/// entries ⇒ warn and skip just the offending pieces, never abort startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostsFile {
    pub refresh_secs: u64,
    pub hosts: Vec<HostConfig>,
}

impl HostsFile {
    fn empty() -> Self {
        Self {
            refresh_secs: DEFAULT_REFRESH_SECS,
            hosts: Vec::new(),
        }
    }

    /// Reads `path` and returns the parsed file plus any warnings (print to
    /// stderr before raw-mode, same convention as `keys.rs::KeyMap::load`).
    pub fn load(path: &Path) -> (HostsFile, Vec<String>) {
        let mut warnings = Vec::new();

        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => return (Self::empty(), warnings), // absent file: silent, local-only
        };

        let raw: HostsFileRaw = match toml::from_str(&content) {
            Ok(raw) => raw,
            Err(error) => {
                warnings.push(format!(
                    "hosts.toml: could not parse {} ({error}); local-only mode",
                    path.display()
                ));
                return (Self::empty(), warnings);
            }
        };

        let refresh_secs = raw.refresh_secs.unwrap_or(DEFAULT_REFRESH_SECS).max(MIN_REFRESH_SECS);
        if let Some(requested) = raw.refresh_secs
            && requested < MIN_REFRESH_SECS
        {
            warnings.push(format!(
                "hosts.toml: refresh_secs={requested} below minimum {MIN_REFRESH_SECS}, clamped"
            ));
        }

        let mut hosts = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for host in raw.hosts {
            let name = host.name.trim();
            if name.is_empty() || host.ssh.trim().is_empty() {
                warnings.push("hosts.toml: skipping host entry with empty name/ssh".to_owned());
                continue;
            }
            if name == LOCAL_HOST_ID {
                warnings.push(format!(
                    "hosts.toml: host name '{LOCAL_HOST_ID}' is reserved for the local DB, skipping"
                ));
                continue;
            }
            if !seen.insert(name.to_owned()) {
                warnings.push(format!("hosts.toml: duplicate host '{name}', keeping first"));
                continue;
            }
            hosts.push(host);
        }

        (HostsFile { refresh_secs, hosts }, warnings)
    }
}

/// Connectivity state for one configured host, derived from consecutive
/// fetch outcomes (see `OFFLINE_FAILURE_THRESHOLD`).
#[derive(Clone, Debug, PartialEq)]
pub enum HostStatus {
    Ok { fetched_at: DateTime<Utc> },
    Degraded { reason: String },
    Offline { since: DateTime<Utc>, last_err: String },
}

impl HostStatus {
    pub fn is_offline(&self) -> bool {
        matches!(self, HostStatus::Offline { .. })
    }
}

/// Config + live status + (once a snapshot exists) an opened read-only
/// `Database` for one remote host. `db` is populated lazily: `None` until
/// the first successful fetch, so the team list simply omits the host
/// until then rather than erroring.
pub struct HostRuntime {
    pub config: HostConfig,
    pub status: HostStatus,
    pub db: Option<Database>,
    consecutive_failures: u32,
}

impl HostRuntime {
    pub fn new(config: HostConfig) -> Self {
        Self {
            config,
            status: HostStatus::Offline {
                since: Utc::now(),
                last_err: "not fetched yet".to_owned(),
            },
            db: None,
            consecutive_failures: 0,
        }
    }

    /// Builds runtime state from config, opening any snapshot file already
    /// on disk from a previous run (so a restart doesn't forget the last
    /// good remote data while waiting for the next scheduled fetch).
    pub fn from_existing_snapshot(config: HostConfig, remote_dir: &Path) -> Self {
        let mut runtime = Self::new(config);
        let path = snapshot_path(remote_dir, &runtime.config.name);
        if path.is_file() && quick_check(&path).is_ok() {
            runtime.status = HostStatus::Degraded {
                reason: "snapshot from previous session, awaiting refresh".to_owned(),
            };
            runtime.db = Some(Database::new(path));
        }
        runtime
    }

    /// Folds a fetch outcome into status + (re)opens the snapshot DB on
    /// success. Call this from the event-loop's outcome channel drain, same
    /// shape as `App::complete_audit`/`complete_health`.
    pub fn apply_outcome(&mut self, outcome: &HostFetchOutcome, remote_dir: &Path) {
        if outcome.ok {
            self.consecutive_failures = 0;
            self.status = HostStatus::Ok { fetched_at: outcome.ts };
            self.db = Some(Database::new(snapshot_path(remote_dir, &self.config.name)));
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            let err = outcome.err.clone().unwrap_or_else(|| "unknown error".to_owned());
            self.status = if self.consecutive_failures >= OFFLINE_FAILURE_THRESHOLD {
                HostStatus::Offline { since: outcome.ts, last_err: err }
            } else {
                HostStatus::Degraded { reason: err }
            };
            // db (if any) is deliberately left in place: a failed fetch
            // keeps serving the last good snapshot per the plan's "quick_check
            // fails → keep the previous good copy" rule — fetch_snapshot
            // already refused to promote a torn/corrupt file before this
            // outcome was ever produced.
        }
    }
}

/// Result of one fetch attempt, delivered through the outcome channel.
#[derive(Clone, Debug)]
pub struct HostFetchOutcome {
    pub name: String,
    pub ok: bool,
    pub err: Option<String>,
    pub ts: DateTime<Utc>,
}

fn snapshot_path(remote_dir: &Path, name: &str) -> PathBuf {
    remote_dir.join(format!("{name}.db"))
}

fn snapshot_tmp_path(remote_dir: &Path, name: &str) -> PathBuf {
    remote_dir.join(format!("{name}.db.tmp"))
}

/// A path guaranteed not to exist, passed as the `teams_dir` argument when
/// querying a remote host's `Database` — `db.rs::team_names` only uses that
/// argument to also scan a local roster directory, which has no remote
/// mirror; the DB-side `SELECT DISTINCT team` half of that function still
/// runs normally. Using the *local* teams_dir here would incorrectly source
/// remote team names from local team configs.
pub fn empty_teams_dir() -> PathBuf {
    PathBuf::from("/nonexistent/agmsg-tui-remote-teams-dir")
}

/// Builds the `Command` that produces a snapshot's raw bytes on stdout.
/// Kept separate from execution so tests can swap in a fake without any
/// real network access (per plan doc "no real ssh in CI").
pub trait SnapshotFetcher: Send + Sync {
    fn build_command(&self, host: &HostConfig) -> Command;
}

/// Production fetcher: `ssh <host.ssh> "cat <host.db_path>"`.
pub struct SshFetcher;

impl SnapshotFetcher for SshFetcher {
    fn build_command(&self, host: &HostConfig) -> Command {
        let mut command = Command::new("ssh");
        command
            .arg("-o")
            .arg(format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECS}"))
            .arg("-o")
            .arg("BatchMode=yes")
            .arg(&host.ssh)
            .arg(format!("cat {}", shell_quote(&host.db_path)))
            .kill_on_drop(true);
        command
    }
}

/// Test fetcher: runs an arbitrary `sh -c <script>` in place of ssh, e.g.
/// `"cat fixture.db"` or `"exit 255"`.
pub struct FakeFetcher {
    pub script: String,
}

impl FakeFetcher {
    pub fn new(script: impl Into<String>) -> Self {
        Self { script: script.into() }
    }
}

impl SnapshotFetcher for FakeFetcher {
    fn build_command(&self, _host: &HostConfig) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(&self.script).kill_on_drop(true);
        command
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Runs one fetch attempt end-to-end: build command → capture stdout → tmp
/// file → `PRAGMA quick_check` → atomic rename to the promoted snapshot.
/// Never panics or propagates an `Err` to the caller — failures fold into
/// `HostFetchOutcome::ok = false` so the event-loop drain can treat every
/// outcome uniformly (same shape as `AsyncCommandResult` variants).
pub async fn fetch_snapshot(
    fetcher: &dyn SnapshotFetcher,
    host: &HostConfig,
    remote_dir: &Path,
) -> HostFetchOutcome {
    let ts = Utc::now();
    match fetch_snapshot_inner(fetcher, host, remote_dir).await {
        Ok(()) => HostFetchOutcome { name: host.name.clone(), ok: true, err: None, ts },
        Err(error) => HostFetchOutcome {
            name: host.name.clone(),
            ok: false,
            err: Some(error.to_string()),
            ts,
        },
    }
}

async fn fetch_snapshot_inner(
    fetcher: &dyn SnapshotFetcher,
    host: &HostConfig,
    remote_dir: &Path,
) -> Result<()> {
    fs::create_dir_all(remote_dir)
        .with_context(|| format!("remote snapshot dir を作成できません: {}", remote_dir.display()))?;
    let dest = snapshot_path(remote_dir, &host.name);
    let tmp = snapshot_tmp_path(remote_dir, &host.name);

    let mut command = fetcher.build_command(host);
    let output = match timeout(Duration::from_secs(FETCH_TIMEOUT_SECS), command.output()).await {
        Ok(result) => result.with_context(|| format!("host '{}' の fetch を実行できません", host.name))?,
        Err(_) => bail!("timeout: host '{}' snapshot fetch ({FETCH_TIMEOUT_SECS}s)", host.name),
    };
    if !output.status.success() {
        bail!(
            "ssh/fetch exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if output.stdout.is_empty() {
        bail!("empty snapshot for host '{}' (remote db missing or unreadable)", host.name);
    }

    fs::write(&tmp, &output.stdout)
        .with_context(|| format!("snapshot tmp を書き込めません: {}", tmp.display()))?;
    if let Err(error) = quick_check(&tmp) {
        let _ = fs::remove_file(&tmp);
        bail!("quick_check failed for host '{}': {error}; keeping previous snapshot", host.name);
    }
    fs::rename(&tmp, &dest)
        .with_context(|| format!("snapshot を昇格できません: {} -> {}", tmp.display(), dest.display()))?;
    Ok(())
}

fn quick_check(path: &Path) -> Result<()> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("quick_check 用に開けません: {}", path.display()))?;
    let result: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if result != "ok" {
        bail!("quick_check returned '{result}'");
    }
    Ok(())
}

/// Spawns the per-host background fetch loop: an immediate fetch (Tokio
/// `interval`'s first tick fires without delay) followed by one fetch every
/// `refresh_secs`, forever, sending each outcome down `tx`. Runs entirely
/// off the render/input path — the event loop in `main.rs` only ever drains
/// `tx`'s receiver, mirroring the existing audit/health task pattern.
pub fn spawn_fetch_loop(
    fetcher: std::sync::Arc<dyn SnapshotFetcher + Send + Sync>,
    host: HostConfig,
    refresh_secs: u64,
    remote_dir: PathBuf,
    tx: UnboundedSender<HostFetchOutcome>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(refresh_secs.max(MIN_REFRESH_SECS)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let outcome = fetch_snapshot(fetcher.as_ref(), &host, &remote_dir).await;
            if tx.send(outcome).is_err() {
                return; // receiver (event loop) gone: app is shutting down
            }
        }
    })
}

/// One-line label for a host's status, shared by whatever surface renders
/// it (team-list badge and, eventually, Health).
pub fn status_label(status: &HostStatus) -> String {
    match status {
        HostStatus::Ok { fetched_at } => format!("online (last fetch {})", fetched_at.to_rfc3339()),
        HostStatus::Degraded { reason } => format!("degraded: {reason}"),
        HostStatus::Offline { since, last_err } => {
            format!("offline since {} ({last_err})", since.to_rfc3339())
        }
    }
}

/// Per-host `(name, status label)` rows for a health/connectivity screen.
///
// TODO(14D-merge): Health (src/health.rs + src/ui/health.rs) should call
// this to render a per-host connectivity section — insert its rows above or
// below the LaunchAgent automation row Phase 14D owns, without touching
// that row. Not wired up here: 14B is instructed not to modify health.rs /
// ui/health.rs while 14D is in flight on those files.
pub fn host_status_rows(hosts: &[HostRuntime]) -> Vec<(String, String)> {
    hosts
        .iter()
        .map(|host| (host.config.name.clone(), status_label(&host.status)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_fixture_db(path: &Path) {
        let connection = Connection::open(path).expect("fixture db");
        connection
            .execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    team TEXT NOT NULL,
                    from_agent TEXT NOT NULL,
                    to_agent TEXT NOT NULL,
                    body TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    read_at TEXT
                );
                INSERT INTO messages (team, from_agent, to_agent, body, created_at)
                VALUES ('ops', 'claude', 'codex', 'hi', '2026-07-20T00:00:00Z');",
            )
            .expect("fixture schema+row");
    }

    #[test]
    fn local_host_id_matches_db_local_host_tag() {
        assert_eq!(LOCAL_HOST_ID, crate::db::LOCAL_HOST);
    }

    // --- hosts.toml parse -----------------------------------------------

    #[test]
    fn missing_file_yields_local_only_no_warnings() {
        let (file, warnings) = HostsFile::load(Path::new("/nonexistent/agmsg-tui/hosts.toml"));
        assert!(warnings.is_empty());
        assert!(file.hosts.is_empty());
        assert_eq!(file.refresh_secs, DEFAULT_REFRESH_SECS);
    }

    #[test]
    fn happy_path_parses_hosts_and_refresh_secs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.toml");
        fs::write(
            &path,
            "refresh_secs = 120\n\n[[hosts]]\nname = \"vps\"\nssh = \"root@162.43.15.67\"\n",
        )
        .unwrap();

        let (file, warnings) = HostsFile::load(&path);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(file.refresh_secs, 120);
        assert_eq!(file.hosts.len(), 1);
        assert_eq!(file.hosts[0].name, "vps");
        assert_eq!(file.hosts[0].ssh, "root@162.43.15.67");
        assert_eq!(file.hosts[0].db_path, "~/.agents/agmsg.db"); // default
    }

    #[test]
    fn malformed_toml_falls_back_to_local_only_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.toml");
        fs::write(&path, "this is [ not valid").unwrap();

        let (file, warnings) = HostsFile::load(&path);
        assert!(file.hosts.is_empty());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn refresh_secs_below_minimum_is_clamped_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.toml");
        fs::write(&path, "refresh_secs = 5\n").unwrap();

        let (file, warnings) = HostsFile::load(&path);
        assert_eq!(file.refresh_secs, MIN_REFRESH_SECS);
        assert!(warnings.iter().any(|w| w.contains("clamped")));
    }

    #[test]
    fn reserved_local_name_is_skipped_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.toml");
        fs::write(&path, "[[hosts]]\nname = \"local\"\nssh = \"whatever\"\n").unwrap();

        let (file, warnings) = HostsFile::load(&path);
        assert!(file.hosts.is_empty());
        assert!(warnings.iter().any(|w| w.contains("reserved")));
    }

    #[test]
    fn duplicate_host_name_keeps_first_and_warns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.toml");
        fs::write(
            &path,
            "[[hosts]]\nname = \"vps\"\nssh = \"a\"\n[[hosts]]\nname = \"vps\"\nssh = \"b\"\n",
        )
        .unwrap();

        let (file, warnings) = HostsFile::load(&path);
        assert_eq!(file.hosts.len(), 1);
        assert_eq!(file.hosts[0].ssh, "a");
        assert!(warnings.iter().any(|w| w.contains("duplicate")));
    }

    // --- SnapshotFetcher / fetch_snapshot --------------------------------

    #[tokio::test(flavor = "current_thread")]
    async fn successful_fetch_writes_file_and_passes_quick_check() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("fixture.db");
        write_fixture_db(&fixture);
        let remote_dir = temp.path().join("remote");

        let host = HostConfig {
            name: "vps".to_owned(),
            ssh: "unused".to_owned(),
            db_path: "unused".to_owned(),
        };
        let fetcher = FakeFetcher::new(format!("cat {}", fixture.display()));
        let outcome = fetch_snapshot(&fetcher, &host, &remote_dir).await;

        assert!(outcome.ok, "expected success, got {outcome:?}");
        assert!(outcome.err.is_none());
        let promoted = snapshot_path(&remote_dir, "vps");
        assert!(promoted.is_file());
        assert!(quick_check(&promoted).is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_command_yields_error_outcome_and_writes_no_file() {
        let temp = tempfile::tempdir().unwrap();
        let remote_dir = temp.path().join("remote");
        let host = HostConfig {
            name: "vps".to_owned(),
            ssh: "unused".to_owned(),
            db_path: "unused".to_owned(),
        };
        let fetcher = FakeFetcher::new("exit 255".to_owned());
        let outcome = fetch_snapshot(&fetcher, &host, &remote_dir).await;

        assert!(!outcome.ok);
        assert!(outcome.err.is_some());
        assert!(!snapshot_path(&remote_dir, "vps").exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn corrupt_snapshot_fails_quick_check_and_keeps_old_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let remote_dir = temp.path().join("remote");
        fs::create_dir_all(&remote_dir).unwrap();
        let host = HostConfig {
            name: "vps".to_owned(),
            ssh: "unused".to_owned(),
            db_path: "unused".to_owned(),
        };

        // Seed a previously-good snapshot.
        let good = snapshot_path(&remote_dir, "vps");
        write_fixture_db(&good);
        let good_bytes = fs::read(&good).unwrap();

        // A fetch that returns garbage bytes (not a sqlite file at all).
        let mut garbage_file = tempfile::NamedTempFile::new().unwrap();
        garbage_file.write_all(b"not a sqlite database").unwrap();
        let fetcher = FakeFetcher::new(format!("cat {}", garbage_file.path().display()));

        let outcome = fetch_snapshot(&fetcher, &host, &remote_dir).await;

        assert!(!outcome.ok);
        assert!(outcome.err.as_deref().unwrap_or_default().contains("quick_check"));
        // old snapshot untouched
        assert_eq!(fs::read(&good).unwrap(), good_bytes);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_stdout_is_treated_as_failure() {
        let temp = tempfile::tempdir().unwrap();
        let remote_dir = temp.path().join("remote");
        let host = HostConfig {
            name: "vps".to_owned(),
            ssh: "unused".to_owned(),
            db_path: "unused".to_owned(),
        };
        let fetcher = FakeFetcher::new("true".to_owned()); // succeeds, no stdout
        let outcome = fetch_snapshot(&fetcher, &host, &remote_dir).await;
        assert!(!outcome.ok);
        assert!(!snapshot_path(&remote_dir, "vps").exists());
    }

    // --- HostStatus transitions -------------------------------------------

    #[test]
    fn status_transitions_ok_degraded_offline_recovers() {
        let temp = tempfile::tempdir().unwrap();
        let remote_dir = temp.path().join("remote");
        let config = HostConfig {
            name: "vps".to_owned(),
            ssh: "unused".to_owned(),
            db_path: "unused".to_owned(),
        };
        let mut runtime = HostRuntime::new(config);
        assert!(matches!(runtime.status, HostStatus::Offline { .. }));

        let fail = |ts: DateTime<Utc>| HostFetchOutcome {
            name: "vps".to_owned(),
            ok: false,
            err: Some("boom".to_owned()),
            ts,
        };
        runtime.apply_outcome(&fail(Utc::now()), &remote_dir);
        assert!(matches!(runtime.status, HostStatus::Degraded { .. }), "1st failure -> degraded");
        runtime.apply_outcome(&fail(Utc::now()), &remote_dir);
        assert!(matches!(runtime.status, HostStatus::Degraded { .. }), "2nd failure -> still degraded");
        runtime.apply_outcome(&fail(Utc::now()), &remote_dir);
        assert!(matches!(runtime.status, HostStatus::Offline { .. }), "3rd failure -> offline");

        let ok = HostFetchOutcome {
            name: "vps".to_owned(),
            ok: true,
            err: None,
            ts: Utc::now(),
        };
        runtime.apply_outcome(&ok, &remote_dir);
        assert!(matches!(runtime.status, HostStatus::Ok { .. }), "success recovers immediately");
        assert!(runtime.db.is_some());
    }

    // --- scheduler wiring (smoke, not timing-precise) ----------------------

    #[tokio::test(flavor = "current_thread")]
    async fn spawn_fetch_loop_delivers_immediate_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("fixture.db");
        write_fixture_db(&fixture);
        let remote_dir = temp.path().join("remote");
        let host = HostConfig {
            name: "vps".to_owned(),
            ssh: "unused".to_owned(),
            db_path: "unused".to_owned(),
        };
        let fetcher: std::sync::Arc<dyn SnapshotFetcher + Send + Sync> =
            std::sync::Arc::new(FakeFetcher::new(format!("cat {}", fixture.display())));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = spawn_fetch_loop(fetcher, host, MIN_REFRESH_SECS, remote_dir, tx);

        let outcome = rx.recv().await.expect("first tick outcome");
        assert!(outcome.ok);
        handle.abort();
    }
}
