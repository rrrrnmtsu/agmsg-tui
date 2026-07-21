use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::timeout;

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(10);
const DESPAWN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug)]
pub struct ScriptRunner {
    scripts_dir: PathBuf,
    audit_script: PathBuf,
    timeout: Duration,
}

impl ScriptRunner {
    pub fn new(scripts_dir: impl Into<PathBuf>, audit_script: impl Into<PathBuf>) -> Self {
        Self {
            scripts_dir: scripts_dir.into(),
            audit_script: audit_script.into(),
            timeout: SCRIPT_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub async fn audit(&self) -> Result<CommandResult> {
        // --fix は候補提示だけで、identity reset 自体は実行しない。
        self.run_path(
            &self.audit_script,
            [OsStr::new("--json"), OsStr::new("--fix")],
        )
        .await
    }

    pub async fn delivery_status(&self, agent_type: &str, project: &str) -> Result<CommandResult> {
        self.run(
            "delivery.sh",
            [
                OsStr::new("status"),
                OsStr::new(agent_type),
                OsStr::new(project),
            ],
        )
        .await
    }

    pub async fn process_status(&self, pids: &[u32]) -> Result<CommandResult> {
        if pids.is_empty() {
            return Ok(CommandResult {
                success: true,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        let pid_list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let mut command = Command::new("/bin/ps");
        command
            .args(["-p", &pid_list, "-o", "pid="])
            .kill_on_drop(true);
        let output = match timeout(self.timeout, command.output()).await {
            Ok(output) => output.context("/bin/ps を実行できません")?,
            Err(_) => bail!("timeout: ps ({})", timeout_label(self.timeout)),
        };
        Ok(CommandResult {
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }

    pub async fn send(
        &self,
        team: &str,
        from: &str,
        to: &str,
        body: &str,
    ) -> Result<CommandResult> {
        self.run(
            "send.sh",
            [
                OsStr::new(team),
                OsStr::new(from),
                OsStr::new(to),
                OsStr::new(body),
            ],
        )
        .await
    }

    pub async fn mark_recipient_read(&self, team: &str, recipient: &str) -> Result<CommandResult> {
        // inbox.sh が既読化の正規エントリポイント。TUIはDBへUPDATEしない。
        self.run(
            "inbox.sh",
            [
                OsStr::new(team),
                OsStr::new(recipient),
                OsStr::new("--quiet"),
            ],
        )
        .await
    }

    pub async fn mark_recipient_read_cancellable(
        &self,
        team: &str,
        recipient: &str,
        cancel: watch::Receiver<bool>,
    ) -> Result<CommandResult> {
        self.run_cancellable(
            "inbox.sh",
            [
                OsStr::new(team),
                OsStr::new(recipient),
                OsStr::new("--quiet"),
            ],
            cancel,
        )
        .await
    }

    pub async fn mark_team_read(&self, team: &str, recipients: &[String]) -> Result<CommandResult> {
        let mut combined_stdout = String::new();
        let mut combined_stderr = String::new();
        for recipient in recipients {
            let result = self.mark_recipient_read(team, recipient).await?;
            combined_stdout.push_str(&result.stdout);
            combined_stderr.push_str(&result.stderr);
            if !result.success {
                return Ok(CommandResult {
                    success: false,
                    exit_code: result.exit_code,
                    stdout: combined_stdout,
                    stderr: combined_stderr,
                });
            }
        }
        Ok(CommandResult {
            success: true,
            exit_code: Some(0),
            stdout: combined_stdout,
            stderr: combined_stderr,
        })
    }

    pub async fn spawn(
        &self,
        team: &str,
        agent_type: &str,
        name: &str,
        options: &[String],
    ) -> Result<CommandResult> {
        let mut args = vec![
            OsString::from(agent_type),
            OsString::from(name),
            OsString::from("--team"),
            OsString::from(team),
        ];
        args.extend(options.iter().map(OsString::from));
        // readiness待ちでTUIを最大90秒止めないよう、runner側で常時固定する。
        args.push(OsString::from("--no-wait"));
        self.run("spawn.sh", args).await
    }

    pub async fn join(
        &self,
        team: &str,
        agent: &str,
        agent_type: &str,
        project: &str,
    ) -> Result<CommandResult> {
        self.run(
            "join.sh",
            [
                OsStr::new(team),
                OsStr::new(agent),
                OsStr::new(agent_type),
                OsStr::new(project),
            ],
        )
        .await
    }

    pub async fn join_force(
        &self,
        team: &str,
        agent: &str,
        agent_type: &str,
        project: &str,
    ) -> Result<CommandResult> {
        self.run(
            "join.sh",
            [
                OsStr::new(team),
                OsStr::new(agent),
                OsStr::new(agent_type),
                OsStr::new(project),
                OsStr::new("--force"),
            ],
        )
        .await
    }

    pub async fn rename(&self, team: &str, old: &str, new: &str) -> Result<CommandResult> {
        self.run(
            "rename.sh",
            [OsStr::new(team), OsStr::new(old), OsStr::new(new)],
        )
        .await
    }

    pub async fn rename_cancellable(
        &self,
        team: &str,
        old: &str,
        new: &str,
        cancel: watch::Receiver<bool>,
    ) -> Result<CommandResult> {
        self.run_cancellable(
            "rename.sh",
            [OsStr::new(team), OsStr::new(old), OsStr::new(new)],
            cancel,
        )
        .await
    }

    pub async fn rename_team(&self, old: &str, new: &str) -> Result<CommandResult> {
        self.run(
            "rename-team.sh",
            [OsStr::new(old), OsStr::new(new)],
        )
        .await
    }

    pub async fn reset(
        &self,
        project: &str,
        agent_type: &str,
        agent: &str,
    ) -> Result<CommandResult> {
        let args = reset_arguments(project, agent_type, agent);
        // reset対象projectをcwd由来のsession rootへ再解決させない。
        self.run_with_env(
            "reset.sh",
            &[("AGMSG_RESOLVE_PROJECT", "0")],
            args.iter(),
        )
        .await
    }

    pub async fn reset_cancellable(
        &self,
        project: &str,
        agent_type: &str,
        agent: &str,
        cancel: watch::Receiver<bool>,
    ) -> Result<CommandResult> {
        let args = reset_arguments(project, agent_type, agent);
        self.run_with_env_cancellable(
            "reset.sh",
            &[("AGMSG_RESOLVE_PROJECT", "0")],
            args.iter(),
            cancel,
        )
        .await
    }

    pub async fn leave(&self, team: &str, agent: &str) -> Result<CommandResult> {
        self.run(
            "leave.sh",
            [OsStr::new(team), OsStr::new(agent)],
        )
        .await
    }

    /// `despawn.sh` is the sole Phase 12 exception to the normal 10 second
    /// subprocess budget: graceful teardown intentionally waits for the
    /// remote watcher to release its actas lock.
    pub async fn despawn(
        &self,
        team: &str,
        from: &str,
        name: &str,
        force: bool,
    ) -> Result<CommandResult> {
        let mut runner = self.clone();
        runner.timeout = DESPAWN_TIMEOUT;
        let mut args = vec![
            OsString::from(team),
            OsString::from(from),
            OsString::from(name),
        ];
        if force {
            args.push(OsString::from("--force"));
        }
        runner.run("despawn.sh", args).await
    }

    pub async fn despawn_cancellable(
        &self,
        team: &str,
        from: &str,
        name: &str,
        force: bool,
        cancel: watch::Receiver<bool>,
    ) -> Result<CommandResult> {
        let mut runner = self.clone();
        runner.timeout = DESPAWN_TIMEOUT;
        let mut args = vec![
            OsString::from(team),
            OsString::from(from),
            OsString::from(name),
        ];
        if force {
            args.push(OsString::from("--force"));
        }
        runner.run_cancellable("despawn.sh", args, cancel).await
    }

    async fn run<I, S>(&self, script: &str, args: I) -> Result<CommandResult>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let script_path = self.scripts_dir.join(script);
        self.run_path(&script_path, args).await
    }

    async fn run_with_env<I, S>(
        &self,
        script: &str,
        envs: &[(&str, &str)],
        args: I,
    ) -> Result<CommandResult>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let script_path = self.scripts_dir.join(script);
        self.run_path_with_env(&script_path, envs, args).await
    }

    async fn run_cancellable<I, S>(
        &self,
        script: &str,
        args: I,
        cancel: watch::Receiver<bool>,
    ) -> Result<CommandResult>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_env_cancellable(script, &[], args, cancel)
            .await
    }

    async fn run_with_env_cancellable<I, S>(
        &self,
        script: &str,
        envs: &[(&str, &str)],
        args: I,
        cancel: watch::Receiver<bool>,
    ) -> Result<CommandResult>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let script_path = self.scripts_dir.join(script);
        self.run_path_with_env_cancellable(&script_path, envs, args, cancel)
            .await
    }

    async fn run_path<I, S>(&self, script_path: &Path, args: I) -> Result<CommandResult>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_path_with_env(script_path, &[], args).await
    }

    async fn run_path_with_env<I, S>(
        &self,
        script_path: &Path,
        envs: &[(&str, &str)],
        args: I,
    ) -> Result<CommandResult>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        ensure_script_exists(script_path)?;
        let mut command = Command::new("bash");
        command.arg(script_path).args(args).kill_on_drop(true);
        command.envs(envs.iter().copied());
        let output = match timeout(self.timeout, command.output()).await {
            Ok(output) => {
                output.with_context(|| format!("{} を実行できません", script_path.display()))?
            }
            Err(_) => {
                let script = script_path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or("script");
                bail!("timeout: {script} ({})", timeout_label(self.timeout));
            }
        };
        Ok(CommandResult {
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }

    async fn run_path_with_env_cancellable<I, S>(
        &self,
        script_path: &Path,
        envs: &[(&str, &str)],
        args: I,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<CommandResult>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        ensure_script_exists(script_path)?;
        let mut command = Command::new("bash");
        command.arg(script_path).args(args).kill_on_drop(true);
        command.envs(envs.iter().copied());
        let script = script_path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("script");
        let output = tokio::select! {
            biased;
            changed = cancel.changed() => {
                changed.context("bulk cancel channel closed")?;
                bail!("cancelled: {script}");
            }
            output = timeout(self.timeout, command.output()) => match output {
                Ok(output) => output.with_context(|| format!("{} を実行できません", script_path.display()))?,
                Err(_) => bail!("timeout: {script} ({})", timeout_label(self.timeout)),
            },
        };
        Ok(CommandResult {
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn timeout_label(duration: Duration) -> String {
    if duration.subsec_nanos() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

pub fn reset_arguments(project: &str, agent_type: &str, agent: &str) -> [String; 3] {
    [project.to_owned(), agent_type.to_owned(), agent.to_owned()]
}

pub fn reset_command_display(
    scripts_dir: &Path,
    project: &str,
    agent_type: &str,
    agent: &str,
) -> String {
    let script = scripts_dir.join("reset.sh");
    let args = reset_arguments(project, agent_type, agent);
    format!(
        "AGMSG_RESOLVE_PROJECT=0 {} {}",
        shell_quote(&script.to_string_lossy()),
        args.iter()
            .map(|argument| shell_quote(argument))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

pub fn despawn_command_display(
    scripts_dir: &Path,
    team: &str,
    from: &str,
    name: &str,
    force: bool,
) -> String {
    let script = scripts_dir.join("despawn.sh");
    let mut args = vec![team, from, name];
    if force {
        args.push("--force");
    }
    format!(
        "{} {}",
        shell_quote(&script.to_string_lossy()),
        args.into_iter()
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "/._-".contains(character))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn ensure_script_exists(path: &Path) -> Result<()> {
    anyhow::ensure!(path.is_file(), "scriptが見つかりません: {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, Instant};

    use super::ScriptRunner;

    #[tokio::test(flavor = "current_thread")]
    async fn audit_runs_once_with_json_and_fix_arguments() {
        let temp = tempfile::tempdir().expect("tempdir");
        let script = temp.path().join("agmsg-audit");
        let counter = temp.path().join("calls");
        fs::write(
            &script,
            format!(
                "#!/bin/bash\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s\\n' \"$*\"\n",
                counter.display()
            ),
        )
        .expect("script");
        let runner = ScriptRunner::new(temp.path().join("scripts"), script);
        let result = runner.audit().await.expect("audit");
        assert!(result.success);
        assert_eq!(result.stdout, "--json --fix");
        assert_eq!(
            fs::read_to_string(counter).expect("counter"),
            "--json --fix\n"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spawn_always_appends_no_wait_with_exact_argument_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        let scripts = temp.path().join("scripts");
        fs::create_dir_all(&scripts).expect("scripts dir");
        fs::write(
            scripts.join("spawn.sh"),
            "#!/bin/bash\nprintf '<%s>\\n' \"$@\"\n",
        )
        .expect("spawn script");
        let runner = ScriptRunner::new(&scripts, temp.path().join("agmsg-audit"));
        let result = runner
            .spawn("ops-hub", "codex", "codex-worker", &[])
            .await
            .expect("spawn");
        assert!(result.success);
        assert_eq!(
            result.stdout,
            "<codex>\n<codex-worker>\n<--team>\n<ops-hub>\n<--no-wait>"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reset_structurally_sets_resolve_project_zero() {
        let temp = tempfile::tempdir().expect("tempdir");
        let scripts = temp.path().join("scripts");
        fs::create_dir_all(&scripts).expect("scripts dir");
        fs::write(
            scripts.join("reset.sh"),
            "#!/bin/bash\n[ \"${AGMSG_RESOLVE_PROJECT:-}\" = 0 ] || exit 9\nprintf 'env=%s\\n' \"$AGMSG_RESOLVE_PROJECT\"\nprintf '<%s>\\n' \"$@\"\n",
        )
        .expect("reset script");
        let runner = ScriptRunner::new(&scripts, temp.path().join("agmsg-audit"));
        let result = runner
            .reset("/tmp/project with space", "codex", "codex-worker")
            .await
            .expect("reset");
        assert!(result.success);
        assert_eq!(
            result.stdout,
            "env=0\n</tmp/project with space>\n<codex>\n<codex-worker>"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn despawn_preserves_argument_order_and_adds_force_only_when_requested() {
        let temp = tempfile::tempdir().expect("tempdir");
        let scripts = temp.path().join("scripts");
        fs::create_dir_all(&scripts).expect("scripts dir");
        fs::write(
            scripts.join("despawn.sh"),
            "#!/bin/bash\nprintf '<%s>\\n' \"$@\"\n",
        )
        .expect("despawn script");
        let runner = ScriptRunner::new(&scripts, temp.path().join("agmsg-audit"));
        let graceful = runner
            .despawn("ops-hub", "claude-main", "codex-worker", false)
            .await
            .expect("graceful");
        assert_eq!(
            graceful.stdout,
            "<ops-hub>\n<claude-main>\n<codex-worker>"
        );
        let force = runner
            .despawn("ops-hub", "claude-main", "codex-worker", true)
            .await
            .expect("force");
        assert!(force.stdout.ends_with("<--force>"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn script_timeout_returns_exact_error_without_waiting_for_child() {
        let temp = tempfile::tempdir().expect("tempdir");
        let scripts = temp.path().join("scripts");
        fs::create_dir_all(&scripts).expect("scripts dir");
        fs::write(scripts.join("send.sh"), "#!/bin/bash\nsleep 5\n").expect("send script");
        let runner = ScriptRunner::new(&scripts, temp.path().join("agmsg-audit"))
            .with_timeout(Duration::from_millis(20));
        let started = Instant::now();
        let error = runner
            .send("ops", "codex", "claude", "hello")
            .await
            .expect_err("timeout");
        assert_eq!(error.to_string(), "timeout: send.sh (20ms)");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bulk_cancel_channel_stops_running_script() {
        let temp = tempfile::tempdir().expect("tempdir");
        let scripts = temp.path().join("scripts");
        fs::create_dir_all(&scripts).expect("scripts dir");
        fs::write(scripts.join("inbox.sh"), "#!/bin/bash\nsleep 5\n")
            .expect("inbox script");
        let runner = ScriptRunner::new(&scripts, temp.path().join("agmsg-audit"));
        let (cancel, receiver) = tokio::sync::watch::channel(false);
        let started = Instant::now();
        let pending = runner.mark_recipient_read_cancellable("ops", "codex", receiver);
        cancel.send(true).expect("cancel");
        let error = pending.await.expect_err("cancelled");
        assert_eq!(error.to_string(), "cancelled: inbox.sh");
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
