use std::io::{self, Stdout};
use std::panic;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::cursor::Show;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::layout::Rect;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::time::{Instant, MissedTickBehavior, interval_at, timeout};

use agmsg_tui::agents::AgentOperation;
use agmsg_tui::app::{App, AppAction, InFlightOperation, Screen};
use agmsg_tui::bulk::BulkTarget;
use agmsg_tui::clipboard;
use agmsg_tui::config::{Cli, Paths};
use agmsg_tui::exec::CommandResult;
use agmsg_tui::exec::ScriptRunner;
use agmsg_tui::health::{HealthSnapshot, collect_health_snapshot};
use agmsg_tui::notify::{NotificationSink, PendingNotification, emit_bell};
use agmsg_tui::poll::LivePoller;
use agmsg_tui::ui;

const AUDIT_REFRESH_PERIOD: Duration = Duration::from_secs(60);
const HEALTH_REFRESH_PERIOD: Duration = Duration::from_secs(60);
const SPINNER_PERIOD: Duration = Duration::from_millis(100);

enum AsyncCommandResult {
    Send(Result<CommandResult, String>),
    MarkRead {
        result: Result<CommandResult, String>,
        label: String,
        unread_count: usize,
    },
    // M-1: agent-management scripts (spawn/join/rename/reset/leave) used to
    // run with a direct `.await` inside `execute_action`, blocking the whole
    // event loop — key input, poll, and rendering — for up to the 10s
    // script timeout with no spinner. Carrying the result back through this
    // channel like Send/MarkRead already do lets the loop keep ticking.
    Agent {
        operation: AgentOperation,
        result: Result<CommandResult, String>,
    },
    Bulk {
        target: BulkTarget,
        result: Result<CommandResult, String>,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.print_default_keys {
        print!("{}", agmsg_tui::keys::KeyMap::default_toml());
        return Ok(());
    }
    let (no_color, palette_mode) = agmsg_tui::config::resolve_palette(&cli);
    agmsg_tui::palette::init(no_color, palette_mode);
    let paths = Paths::from_cli(&cli)?;
    // Loaded (and any warnings printed) before terminal setup/raw-mode below
    // so a malformed keys.toml's warning is actually visible to the user
    // instead of getting swallowed by the alternate screen.
    let (keymap, key_warnings) = agmsg_tui::keys::KeyMap::load(&paths.keys_file);
    for warning in &key_warnings {
        eprintln!("{warning}");
    }
    let mut app = App::load(paths.clone())?;
    app.keymap = keymap;

    // Phase 14B: hosts.toml is read the same way as keys.toml (before
    // raw-mode, warnings to stderr, absent file = silent local-only). Any
    // snapshot already on disk from a previous run is opened immediately so
    // a restart doesn't forget the last good remote data while the first
    // scheduled fetch is still pending.
    let (hosts_file, host_warnings) = agmsg_tui::remote::HostsFile::load(&paths.hosts_file);
    for warning in &host_warnings {
        eprintln!("{warning}");
    }
    let host_configs = hosts_file.hosts.clone();
    let hosts = host_configs
        .iter()
        .cloned()
        .map(|config| agmsg_tui::remote::HostRuntime::from_existing_snapshot(config, &paths.remote_dir))
        .collect();
    app.set_hosts(hosts)?;

    if cli.diagnose {
        println!(
            "team_count={} message_count={}",
            app.teams.len(),
            app.database.message_count()?
        );
        return Ok(());
    }
    if cli.startup_probe {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| ui::render(frame, &app))?;
        return Ok(());
    }
    if cli.bulk_preview_probe {
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))?;
        app.handle_key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::NONE))?;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| ui::render(frame, &app))?;
        let (loaded, filtered) = app
            .bulk_filter
            .as_ref()
            .map(|filter| (filter.all_messages.len(), filter.results.len()))
            .unwrap_or_default();
        let preview_targets = match app.bulk_modal.as_ref() {
            Some(agmsg_tui::bulk::BulkModal::Preview { targets, .. }) => targets.len(),
            _ => 0,
        };
        println!(
            "bulk_loaded={loaded} filtered_7d={filtered} preview_targets={preview_targets}"
        );
        return Ok(());
    }

    install_panic_hook();
    let mut terminal = setup_terminal()?;
    let guard = TerminalGuard;
    let result = run_tui(
        &mut terminal,
        &mut app,
        ScriptRunner::new(paths.scripts_dir, paths.audit_script),
        host_configs,
        hosts_file.refresh_secs,
        paths.remote_dir.clone(),
    )
    .await;
    let state_result = app.save_state();
    drop(terminal);
    drop(guard);
    match result {
        Ok(()) => state_result,
        Err(error) => Err(error),
    }
}

async fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    scripts: ScriptRunner,
    hosts: Vec<agmsg_tui::remote::HostConfig>,
    hosts_refresh_secs: u64,
    remote_dir: std::path::PathBuf,
) -> Result<()> {
    let last_seen_id = app.database.last_seen_id()?;
    let mut poller = LivePoller::new(last_seen_id);
    let (audit_tx, mut audit_rx) = unbounded_channel::<Result<CommandResult, String>>();
    let (health_tx, mut health_rx) = unbounded_channel::<Result<HealthSnapshot, String>>();
    let (command_tx, mut command_rx) = unbounded_channel::<AsyncCommandResult>();
    let (host_tx, mut host_rx) = unbounded_channel::<agmsg_tui::remote::HostFetchOutcome>();
    let mut audit_refresh =
        interval_at(Instant::now() + AUDIT_REFRESH_PERIOD, AUDIT_REFRESH_PERIOD);
    audit_refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut health_refresh =
        interval_at(Instant::now() + HEALTH_REFRESH_PERIOD, HEALTH_REFRESH_PERIOD);
    health_refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut spinner = interval_at(Instant::now() + SPINNER_PERIOD, SPINNER_PERIOD);
    spinner.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut needs_draw = true;
    let mut notify_sink = NotificationSink::new();

    // Phase 14B: one background fetch loop per configured host, entirely
    // off the render/input path (invariant #2) — the loop below only ever
    // drains `host_rx`, mirroring the audit/health task pattern already in
    // use here. `_host_fetch_handles` just keeps the tasks alive for the
    // process lifetime; nothing joins them (the process exit tears them
    // down, same as any other `tokio::spawn`'d background task in main.rs).
    let ssh_fetcher: std::sync::Arc<dyn agmsg_tui::remote::SnapshotFetcher + Send + Sync> =
        std::sync::Arc::new(agmsg_tui::remote::SshFetcher);
    let _host_fetch_handles: Vec<_> = hosts
        .into_iter()
        .map(|host| {
            agmsg_tui::remote::spawn_fetch_loop(
                ssh_fetcher.clone(),
                host,
                hosts_refresh_secs,
                remote_dir.clone(),
                host_tx.clone(),
            )
        })
        .collect();

    loop {
        if needs_draw {
            terminal.draw(|frame| ui::render(frame, app))?;
            needs_draw = false;
            if app.notify_settings.title && notify_sink.set_title_if_changed(app.total_unread()).is_err() {
                app.warn_notify_failure_once();
                needs_draw = true;
            }
        }

        while let Ok(result) = audit_rx.try_recv() {
            match result {
                Ok(result) => {
                    if let Err(error) = app.complete_audit(&result) {
                        app.complete_audit_error(error.to_string());
                    }
                }
                Err(error) => app.complete_audit_error(error),
            }
            needs_draw = true;
        }

        while let Ok(result) = health_rx.try_recv() {
            match result {
                Ok(snapshot) => app.complete_health(snapshot),
                Err(error) => app.complete_health_error(error),
            }
            needs_draw = true;
        }

        while let Ok(outcome) = host_rx.try_recv() {
            if let Err(error) = app.complete_host_fetch(outcome) {
                app.set_error(&error);
            }
            needs_draw = true;
        }

        while let Ok(completion) = command_rx.try_recv() {
            app.finish_operation();
            let mut followup = None;
            match completion {
                AsyncCommandResult::Send(result) => match result {
                    Ok(result) => {
                        if let Err(error) = app.complete_send(&result) {
                            app.set_error(&error);
                        }
                    }
                    Err(error) => app.set_error(&anyhow::anyhow!(error)),
                },
                AsyncCommandResult::MarkRead {
                    result,
                    label,
                    unread_count,
                } => match result {
                    Ok(result) => {
                        if let Err(error) = app.complete_mark_read(&result, &label, unread_count) {
                            app.set_error(&error);
                        }
                    }
                    Err(error) => app.set_error(&anyhow::anyhow!(error)),
                },
                AsyncCommandResult::Agent { operation, result } => match result {
                    Ok(result) => {
                        if let Err(error) = app.complete_agent_operation(&operation, &result) {
                            app.complete_agent_error(&error);
                        }
                    }
                    Err(error) => app.complete_agent_error(&anyhow::anyhow!(error)),
                },
                AsyncCommandResult::Bulk { target, result } => {
                    match app.complete_bulk_target(target, result) {
                        Ok(action) => followup = Some(action),
                        Err(error) => app.set_error(&error),
                    }
                }
            }
            if let Some(action) = followup {
                execute_action(
                    app,
                    &scripts,
                    &audit_tx,
                    &health_tx,
                    &command_tx,
                    action,
                )
                .await;
            }
            needs_draw = true;
        }

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    // Phase 14A: keep `app.term_area` current so the split-
                    // entry min-height guard (`Ctrl+S`) sees the real
                    // terminal size even when the last event was a key, not
                    // a mouse move/click (which is `handle_mouse`'s own
                    // update path).
                    let size = terminal.size()?;
                    app.term_area = Rect::new(0, 0, size.width, size.height);
                    let action = app.handle_key(key)?;
                    if matches!(action, AppAction::Quit) {
                        return Ok(());
                    }
                    if matches!(action, AppAction::RefreshAudit) {
                        audit_refresh.reset();
                    }
                    if matches!(action, AppAction::RefreshHealth) {
                        health_refresh.reset();
                    }
                    execute_action(
                        app,
                        &scripts,
                        &audit_tx,
                        &health_tx,
                        &command_tx,
                        action,
                    )
                    .await;
                    needs_draw = true;
                }
                Event::Mouse(mouse) => {
                    let size = terminal.size()?;
                    app.handle_mouse(mouse, Rect::new(0, 0, size.width, size.height));
                    needs_draw = true;
                }
                Event::Resize(_, _) => needs_draw = true,
                _ => {}
            }
        }

        match poller
            .poll_if_due(&app.database, &app.paths.teams_dir)
            .await
        {
            Ok(Some(messages)) if !messages.is_empty() => {
                if let Err(error) = app.receive_new_messages(messages) {
                    app.set_error(&error);
                }
                for note in app.drain_pending_notifications() {
                    // L-4: both writes used to be dropped outright
                    // (`let _ = ...`) — a broken terminal/tmux passthrough
                    // meant bell/desktop notifications silently stopped
                    // firing with no indication why.
                    let failed = match note {
                        PendingNotification::Bell => emit_bell().is_err(),
                        PendingNotification::Desktop { from, body } => {
                            notify_sink.emit_osc9(&from, &body).is_err()
                        }
                    };
                    if failed {
                        app.warn_notify_failure_once();
                    }
                }
                needs_draw = true;
            }
            Ok(_) => {}
            Err(error) => {
                app.set_poll_error(&error);
                needs_draw = true;
            }
        }
        if poller.take_recovered() {
            app.set_poll_recovered();
            needs_draw = true;
        }

        // The burst banner has no user-driven close key, so it needs its own
        // expiry check here — otherwise it'd sit on screen past its 3s
        // window until some unrelated key/message happened to force a redraw.
        if app
            .burst_alert
            .as_ref()
            .is_some_and(|(_, until)| std::time::Instant::now() >= *until)
        {
            app.burst_alert = None;
            needs_draw = true;
        }

        if app.in_flight.is_some()
            && timeout(Duration::from_millis(1), spinner.tick())
                .await
                .is_ok()
            && app.advance_spinner()
        {
            needs_draw = true;
        }

        if let Err(error) = app.persist_state_if_due(std::time::Instant::now()) {
            app.set_error(&error);
            needs_draw = true;
        }

        if timeout(Duration::from_millis(1), audit_refresh.tick())
            .await
            .is_ok()
            && app.screen == Screen::Audit
        {
            let action = app.request_audit_refresh();
            execute_action(
                app,
                &scripts,
                &audit_tx,
                &health_tx,
                &command_tx,
                action,
            )
            .await;
            needs_draw = true;
        }

        if timeout(Duration::from_millis(1), health_refresh.tick())
            .await
            .is_ok()
            && app.screen == Screen::Health
        {
            let action = app.request_health_refresh();
            execute_action(
                app,
                &scripts,
                &audit_tx,
                &health_tx,
                &command_tx,
                action,
            )
            .await;
            needs_draw = true;
        }
    }
}

async fn execute_action(
    app: &mut App,
    scripts: &ScriptRunner,
    audit_tx: &UnboundedSender<Result<CommandResult, String>>,
    health_tx: &UnboundedSender<Result<HealthSnapshot, String>>,
    command_tx: &UnboundedSender<AsyncCommandResult>,
    action: AppAction,
) {
    match action {
        AppAction::None | AppAction::Quit => {}
        AppAction::Send(request) => {
            if !app.start_operation(InFlightOperation::Send) {
                return;
            }
            let scripts = scripts.clone();
            let command_tx = command_tx.clone();
            tokio::spawn(async move {
                let result = scripts
                    .send(&request.team, &request.from, &request.to, &request.body)
                    .await
                    .map_err(|error| error.to_string());
                let _ = command_tx.send(AsyncCommandResult::Send(result));
            });
        }
        AppAction::MarkRecipient {
            team,
            recipient,
            unread_count,
        } => {
            if !app.start_operation(InFlightOperation::MarkRead) {
                return;
            }
            let scripts = scripts.clone();
            let command_tx = command_tx.clone();
            tokio::spawn(async move {
                let result = scripts
                    .mark_recipient_read(&team, &recipient)
                    .await
                    .map_err(|error| error.to_string());
                let _ = command_tx.send(AsyncCommandResult::MarkRead {
                    result,
                    label: recipient,
                    unread_count,
                });
            });
        }
        AppAction::MarkTeam {
            team,
            recipients,
            unread_count,
        } => {
            if !app.start_operation(InFlightOperation::MarkRead) {
                return;
            }
            let scripts = scripts.clone();
            let command_tx = command_tx.clone();
            tokio::spawn(async move {
                let result = scripts
                    .mark_team_read(&team, &recipients)
                    .await
                    .map_err(|error| error.to_string());
                let _ = command_tx.send(AsyncCommandResult::MarkRead {
                    result,
                    label: team,
                    unread_count,
                });
            });
        }
        AppAction::RefreshAudit => {
            let scripts = scripts.clone();
            let audit_tx = audit_tx.clone();
            tokio::spawn(async move {
                let result = scripts.audit().await.map_err(|error| error.to_string());
                let _ = audit_tx.send(result);
            });
        }
        AppAction::RefreshHealth => {
            let database = app.database.clone();
            let teams_dir = app.paths.teams_dir.clone();
            let run_dir = app
                .paths
                .scripts_dir
                .parent()
                .map(|path| path.join("run"))
                .unwrap_or_else(|| app.paths.scripts_dir.join("../run"));
            let scripts = scripts.clone();
            let health_tx = health_tx.clone();
            tokio::spawn(async move {
                let result = collect_health_snapshot(&database, &teams_dir, &run_dir, &scripts)
                    .await
                    .map_err(|error| error.to_string());
                let _ = health_tx.send(result);
            });
        }
        AppAction::ExportReport => {
            if let Err(error) = app.export_audit_report() {
                app.set_error(&error);
            }
        }
        AppAction::ExportBulk(format) => {
            if let Err(error) = app.export_bulk_filter(format) {
                app.set_error(&error);
            }
        }
        AppAction::RunBulk {
            target,
            force_despawn,
        } => {
            if !app.start_operation(InFlightOperation::Bulk) {
                return;
            }
            let Some(cancel) = app.bulk_cancel_receiver() else {
                app.finish_operation();
                app.set_error(&anyhow::anyhow!("bulk cancel channel is unavailable"));
                return;
            };
            let scripts = scripts.clone();
            let command_tx = command_tx.clone();
            tokio::spawn(async move {
                let result = match &target {
                    BulkTarget::MarkRead(target) => {
                        scripts
                            .mark_recipient_read_cancellable(
                                &target.team,
                                &target.recipient,
                                cancel,
                            )
                            .await
                    }
                    BulkTarget::Reset(target) => {
                        scripts
                            .reset_cancellable(
                                &target.project,
                                &target.agent_type,
                                &target.agent,
                                cancel,
                            )
                            .await
                    }
                    BulkTarget::Rename(target) => {
                        scripts
                            .rename_cancellable(&target.team, &target.old, &target.new, cancel)
                            .await
                    }
                    BulkTarget::Despawn(target) => {
                        scripts
                            .despawn_cancellable(
                                &target.team,
                                &target.from,
                                &target.name,
                                force_despawn,
                                cancel,
                            )
                            .await
                    }
                }
                .map_err(|error| error.to_string());
                let _ = command_tx.send(AsyncCommandResult::Bulk { target, result });
            });
        }
        AppAction::Yank(body) => match clipboard::yank(&body) {
            Ok(true) => {
                app.status = agmsg_tui::app::StatusLine {
                    text: format!("yanked {} chars", body.chars().count()),
                    is_error: false,
                }
            }
            // L-4: OSC 52 wrote fine but `pbcopy` failed — don't claim
            // "yanked" when the clipboard bridge is actually broken.
            Ok(false) => app.complete_yank_fallback(&body),
            Err(error) => app.set_error(&error),
        },
        AppAction::ManageAgent(operation) => {
            // M-1: was `.await`ed directly here, freezing the whole event
            // loop for up to SCRIPT_TIMEOUT (10s) with no feedback. Moved
            // onto the same tokio::spawn + in_flight/spinner rail as Send
            // and MarkRead above — `start_operation` also gives it the same
            // re-entrancy guard (a second confirm while one is running is
            // dropped, not queued or double-fired).
            if !app.start_operation(InFlightOperation::Agent) {
                return;
            }
            let scripts = scripts.clone();
            let command_tx = command_tx.clone();
            tokio::spawn(async move {
                let result = match &operation {
                    AgentOperation::Spawn {
                        team,
                        agent_type,
                        name,
                    } => scripts.spawn(team, agent_type, name, &[]).await,
                    AgentOperation::Join {
                        team,
                        agent,
                        agent_type,
                        project,
                    } => scripts.join(team, agent, agent_type, project).await,
                    AgentOperation::JoinForce {
                        team,
                        agent,
                        agent_type,
                        project,
                    } => scripts.join_force(team, agent, agent_type, project).await,
                    AgentOperation::Rename {
                        team, old, new, ..
                    } => scripts.rename(team, old, new).await,
                    AgentOperation::RenameTeam { old, new } => {
                        scripts.rename_team(old, new).await
                    }
                    AgentOperation::Reset {
                        project,
                        agent_type,
                        agent,
                    } => scripts.reset(project, agent_type, agent).await,
                    AgentOperation::Leave { team, agent } => scripts.leave(team, agent).await,
                }
                .map_err(|error| error.to_string());
                let _ = command_tx.send(AsyncCommandResult::Agent { operation, result });
            });
        }
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
        let _ = disable_raw_mode();
        return Err(error.into());
    }
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture, Show);
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}
