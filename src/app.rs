use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::agents::{
    AgentFocus, AgentModal, AgentOperation, CLI_TYPES, SpawnModalState, SpawnStep,
    validate_agent_name, validate_team_name,
};
use crate::audit::{AuditReport, build_markdown_for_window, parse_audit_stdout, report_stamp};
use crate::audit_history::{AuditHistorySample, read_audit_history};
use crate::bulk::{
    BulkFilterFocus, BulkFilterState, BulkKind, BulkModal, BulkOperation, BulkRunState, BulkTarget,
    DespawnTarget, ExportFormat, LockStatus, RenameTarget, ResetTarget, actas_lock_status,
    export_bulk_messages, mark_read_targets, naming_violations,
};
use crate::config::Paths;
use crate::db::{
    ANALYTICS_WINDOW_DAYS, AgentIdentitySummary, AgentTeamSummary, BodySizeDistribution, Database,
    MemberSummary, Message, PairMatrix, STALE_UNREAD_DAYS, StaleUnread, TeamSummary,
    ZombieIdentity,
};
use crate::exec::{CommandResult, reset_command_display};
use crate::health::{HealthSnapshot, TeamHealth};
use crate::notify::{
    BURST_ALERT_DURATION, BurstTracker, NOTIFY_SETTING_COUNT, NotifySettings, PendingNotification,
};
use crate::state::PersistentState;
use crate::ui::{
    MouseTarget, SIDEBAR_MAX_PCT, SIDEBAR_MIN_PCT, compute_layout, hit_test, resize_pct_from_column,
};

const HISTORY_PAGE_SIZE: usize = 200;
const HALF_PAGE_MESSAGES: usize = 10;
const STATE_DEBOUNCE: Duration = Duration::from_secs(3);
/// Body length (in chars, not bytes — folding is about visual line count,
/// not wire size) past which the room pane shows a bounded preview instead
/// of the full message. Separate from `BODY_WARN_BYTES`/`BODY_BLOCK_BYTES`,
/// which gate what the *composer* is willing to send.
pub const FOLD_CHAR_THRESHOLD: usize = 500;
pub const FOLD_PREVIEW_LINES: usize = 20;
pub const BODY_WARN_BYTES: usize = 2_048;
pub const BODY_BLOCK_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Teams,
    Members,
    Room,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Main,
    Composer,
    Audit,
    Agents,
    Health,
    BulkFilter,
    Help,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditTab {
    Dashboard,
    History,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Search,
}

#[derive(Clone, Debug, Default)]
pub struct StatusLine {
    pub text: String,
    pub is_error: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerState {
    pub roster: Vec<String>,
    pub from_index: usize,
    pub to_index: usize,
    pub body: String,
    pub cursor: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodySizeLevel {
    Normal,
    Warning,
    Blocked,
}

impl ComposerState {
    pub fn from_agent(&self) -> Option<&str> {
        self.roster.get(self.from_index).map(String::as_str)
    }

    pub fn to_agent(&self) -> Option<&str> {
        self.roster.get(self.to_index).map(String::as_str)
    }

    pub fn body_bytes(&self) -> usize {
        self.body.len()
    }

    pub fn body_size_level(&self) -> BodySizeLevel {
        if self.body_bytes() <= BODY_WARN_BYTES {
            BodySizeLevel::Normal
        } else if self.body_bytes() <= BODY_BLOCK_BYTES {
            BodySizeLevel::Warning
        } else {
            BodySizeLevel::Blocked
        }
    }

    fn insert_char(&mut self, character: char) {
        let mut characters: Vec<char> = self.body.chars().collect();
        characters.insert(self.cursor, character);
        self.cursor += 1;
        self.body = characters.into_iter().collect();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut characters: Vec<char> = self.body.chars().collect();
        self.cursor -= 1;
        characters.remove(self.cursor);
        self.body = characters.into_iter().collect();
    }

    fn delete(&mut self) {
        let mut characters: Vec<char> = self.body.chars().collect();
        if self.cursor < characters.len() {
            characters.remove(self.cursor);
            self.body = characters.into_iter().collect();
        }
    }

    fn delete_previous_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut characters: Vec<char> = self.body.chars().collect();
        let mut start = self.cursor;
        while start > 0 && characters[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !characters[start - 1].is_whitespace() {
            start -= 1;
        }
        characters.drain(start..self.cursor);
        self.cursor = start;
        self.body = characters.into_iter().collect();
    }
}

#[derive(Clone, Debug)]
pub struct SendRequest {
    pub team: String,
    pub from: String,
    pub to: String,
    pub body: String,
}

#[derive(Clone, Debug)]
pub enum AppAction {
    None,
    Quit,
    Send(SendRequest),
    MarkRecipient {
        team: String,
        recipient: String,
        unread_count: usize,
    },
    MarkTeam {
        team: String,
        recipients: Vec<String>,
        unread_count: usize,
    },
    RefreshAudit,
    RefreshHealth,
    ExportReport,
    ExportBulk(ExportFormat),
    RunBulk {
        target: BulkTarget,
        force_despawn: bool,
    },
    Yank(String),
    ManageAgent(AgentOperation),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InFlightOperation {
    Send,
    MarkRead,
    Agent,
    Bulk,
}

impl InFlightOperation {
    fn status_label(self) -> &'static str {
        match self {
            Self::Send => "sending",
            Self::MarkRead => "marking read",
            Self::Agent => "running agent op",
            Self::Bulk => "running bulk op",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum AuditSelection<'a> {
    Zombie(&'a ZombieIdentity),
    Stale(&'a StaleUnread),
}

pub struct App {
    pub paths: Paths,
    pub database: Database,
    pub teams: Vec<TeamSummary>,
    pub selected_team: usize,
    pub active_team: Option<String>,
    pub members: Vec<MemberSummary>,
    pub selected_member: usize,
    pub messages: Vec<Message>,
    pub selected_message: usize,
    pub focus: Focus,
    pub screen: Screen,
    pub help_return_screen: Screen,
    /// Vertical scroll offset (lines) into the help popup; render-time
    /// clamps it against actual content length, so no upper bound needs
    /// tracking here.
    pub help_scroll: u16,
    pub input_mode: InputMode,
    pub composer: Option<ComposerState>,
    pub drafts: HashMap<String, ComposerState>,
    pub status: StatusLine,
    pub has_more_history: bool,
    pub expanded_messages: HashMap<String, HashSet<i64>>,
    pub audit_report: Option<AuditReport>,
    pub audit_loading: bool,
    pub audit_tab: AuditTab,
    pub audit_pair_window_days: u32,
    pub audit_history: Vec<AuditHistorySample>,
    pub body_size_distribution: BodySizeDistribution,
    pub pair_matrices: Vec<PairMatrix>,
    pub zombies: Vec<ZombieIdentity>,
    pub stale_unreads: Vec<StaleUnread>,
    pub audit_team_index: usize,
    pub audit_selected: usize,
    pub audit_detail: Option<String>,
    pub health_snapshot: Option<HealthSnapshot>,
    pub health_loading: bool,
    pub health_window_days: u32,
    pub health_team_index: usize,
    pub bulk_filter: Option<BulkFilterState>,
    pub bulk_modal: Option<BulkModal>,
    pub bulk_operation: Option<BulkOperation>,
    bulk_cancel: Option<watch::Sender<bool>>,
    /// Sidebar width as a percentage, drag-resizable (`ui::SIDEBAR_MIN_PCT`..=`ui::SIDEBAR_MAX_PCT`).
    pub sidebar_pct: u16,
    /// True while a left-button drag started on the resize handle is still held.
    pub resize_dragging: bool,
    /// This session's own agent name, sourced from `AGMSG_IDENTITY` — used
    /// only for the self-message `▏` marker and as the composer's default
    /// `from` when opened via the MEMBER column's Enter action. `None` just
    /// means neither of those get a default; nothing else depends on it.
    pub current_identity: Option<String>,
    /// When set, the room pane shows only messages where this agent is
    /// sender or recipient. Toggled by `F` on the selected MEMBER row.
    pub member_filter: Option<String>,
    /// Case-insensitive in-memory search over the loaded message fields.
    /// `Some("")` is used only while the search prompt is initially empty.
    pub search_query: Option<String>,
    /// Rendered text for the MEMBER info popup (`I` key); `None` = closed.
    pub member_info: Option<String>,
    /// Rendered text for the Agents-screen identity info popup (`Enter` on
    /// an Identities-focus row); `None` = closed. Same shape as
    /// `member_info`, scoped to the Agents screen instead of Main.
    pub agent_identity_info: Option<String>,
    /// A押下時だけ読み込むagent registry。起動パスには載せない。
    pub agent_teams: Vec<AgentTeamSummary>,
    pub agent_team_index: usize,
    pub agent_identity_index: usize,
    pub agent_focus: AgentFocus,
    pub agent_modal: Option<AgentModal>,
    /// self rename後にbridge再起動が必要なことを画面ヘッダへ残す。
    pub agent_restart_needed: Option<String>,
    /// Bell/desktop/title/burst toggles. `bell` is also the `Ctrl+B` mute
    /// target — see `notify::NotifySettings` doc comment.
    pub notify_settings: NotifySettings,
    /// Rolling 60s arrival counter feeding the burst banner; lives on `App`
    /// (not `NotificationSink`) because burst state gates whether BEL/OSC 9
    /// fire at all, which `receive_new_messages` needs to decide.
    pub burst_tracker: BurstTracker,
    /// `(banner text, hide-after)`; render-time compares `hide-after` against
    /// `Instant::now()` so nothing has to explicitly clear this on expiry.
    pub burst_alert: Option<(String, Instant)>,
    /// BEL/OSC 9 events `receive_new_messages` decided to fire, drained by
    /// `main.rs` each loop tick — keeps `App` itself IO-free (see
    /// `notify::PendingNotification` doc comment).
    pub pending_notifications: Vec<PendingNotification>,
    /// `Ctrl+N` settings popup; `Some(index)` is the highlighted row, `None`
    /// closed.
    pub notify_popup: Option<usize>,
    pub in_flight: Option<InFlightOperation>,
    spinner_frame: usize,
    pub poll_offline: bool,
    state_dirty_since: Option<Instant>,
    /// L-4: OSC 9 / bell / title notification writes are best-effort IO —
    /// `main.rs` used to swallow their errors outright. This gates a
    /// one-shot status warning so a broken terminal doesn't spam the status
    /// line on every subsequent message arrival.
    notify_delivery_warned: bool,
}

impl App {
    pub fn load(paths: Paths) -> Result<Self> {
        let database = Database::new(paths.db.clone());
        database.validate_schema()?;
        let teams = database.team_summaries(&paths.teams_dir)?;
        let persisted = PersistentState::load(&paths.state_file);
        let selected_team = persisted
            .last_team
            .as_ref()
            .and_then(|last| teams.iter().position(|team| &team.name == last))
            .unwrap_or_default();
        let active_team = teams.get(selected_team).map(|team| team.name.clone());
        let mut app = Self {
            paths,
            database,
            teams,
            selected_team,
            active_team,
            members: Vec::new(),
            selected_member: 0,
            messages: Vec::new(),
            selected_message: 0,
            focus: Focus::Teams,
            screen: Screen::Main,
            help_return_screen: Screen::Main,
            help_scroll: 0,
            input_mode: InputMode::Normal,
            composer: None,
            drafts: persisted.drafts,
            status: StatusLine {
                text: "ready".to_owned(),
                is_error: false,
            },
            has_more_history: false,
            expanded_messages: HashMap::new(),
            audit_report: None,
            audit_loading: false,
            audit_tab: AuditTab::Dashboard,
            audit_pair_window_days: ANALYTICS_WINDOW_DAYS,
            audit_history: Vec::new(),
            body_size_distribution: BodySizeDistribution::default(),
            pair_matrices: Vec::new(),
            zombies: Vec::new(),
            stale_unreads: Vec::new(),
            audit_team_index: 0,
            audit_selected: 0,
            audit_detail: None,
            health_snapshot: None,
            health_loading: false,
            health_window_days: 7,
            health_team_index: 0,
            bulk_filter: None,
            bulk_modal: None,
            bulk_operation: None,
            bulk_cancel: None,
            sidebar_pct: persisted
                .sidebar_pct
                .clamp(SIDEBAR_MIN_PCT, SIDEBAR_MAX_PCT),
            resize_dragging: false,
            current_identity: env::var("AGMSG_IDENTITY").ok(),
            member_filter: None,
            search_query: None,
            member_info: None,
            agent_identity_info: None,
            agent_teams: Vec::new(),
            agent_team_index: 0,
            agent_identity_index: 0,
            agent_focus: AgentFocus::Identities,
            agent_modal: None,
            agent_restart_needed: None,
            notify_settings: persisted.notify_settings,
            burst_tracker: BurstTracker::new(),
            burst_alert: None,
            pending_notifications: Vec::new(),
            notify_popup: None,
            in_flight: None,
            spinner_frame: 0,
            poll_offline: false,
            state_dirty_since: None,
            notify_delivery_warned: false,
        };
        app.reload_selected_team()?;
        app.state_dirty_since = None;
        // S10-4: AGMSG_TUI_THEME naming a syntect theme that doesn't exist
        // would otherwise fail silently (highlight.rs falls back to the
        // default theme either way) — surface it once at startup instead of
        // leaving the user to notice the code blocks look unexpectedly
        // unchanged.
        if let Some(requested) = crate::highlight::requested_theme_missing() {
            app.status = StatusLine {
                text: format!(
                    "⚠ AGMSG_TUI_THEME={requested} not found in syntect themes, using default"
                ),
                is_error: false,
            };
        } else if app.current_identity.is_none() {
            // H-3: without AGMSG_IDENTITY the self-reset guard, the own-message
            // `▏` marker, and the composer `from` default all quietly fall back
            // to "no protection" / "roster[0]" instead of erroring — surface it
            // once at startup rather than let each of those degrade silently.
            app.status = StatusLine {
                text: "⚠ AGMSG_IDENTITY unset: self-guard / own-marker / from-default disabled"
                    .to_owned(),
                is_error: false,
            };
        }
        Ok(app)
    }

    fn persistence_snapshot(&self) -> PersistentState {
        let mut drafts = self.drafts.clone();
        if let (Some(team), Some(composer)) = (self.selected_team_name(), self.composer.as_ref()) {
            drafts.insert(team.to_owned(), composer.clone());
        }
        PersistentState {
            sidebar_pct: self.sidebar_pct,
            last_team: self.active_team.clone(),
            drafts,
            notify_settings: self.notify_settings,
            ..PersistentState::default()
        }
    }

    pub fn persist_state_if_due(&mut self, now: Instant) -> Result<bool> {
        if !self
            .state_dirty_since
            .is_some_and(|changed| now.saturating_duration_since(changed) >= STATE_DEBOUNCE)
        {
            return Ok(false);
        }
        match self.save_state() {
            Ok(()) => Ok(true),
            Err(error) => {
                self.state_dirty_since = Some(now);
                Err(error)
            }
        }
    }

    pub fn save_state(&mut self) -> Result<()> {
        self.persistence_snapshot().save(&self.paths.state_file)?;
        self.state_dirty_since = None;
        Ok(())
    }

    fn mark_state_changed(&mut self) {
        self.state_dirty_since = Some(Instant::now());
    }

    pub fn start_operation(&mut self, operation: InFlightOperation) -> bool {
        if self.in_flight.is_some() {
            return false;
        }
        self.in_flight = Some(operation);
        self.spinner_frame = 0;
        self.update_spinner_status();
        true
    }

    pub fn finish_operation(&mut self) {
        self.in_flight = None;
        self.spinner_frame = 0;
    }

    pub fn advance_spinner(&mut self) -> bool {
        if self.in_flight.is_none() {
            return false;
        }
        self.spinner_frame = (self.spinner_frame + 1) % 4;
        self.update_spinner_status();
        true
    }

    fn update_spinner_status(&mut self) {
        const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
        if let Some(operation) = self.in_flight {
            self.status = StatusLine {
                text: format!(
                    "{}... {}",
                    operation.status_label(),
                    FRAMES[self.spinner_frame]
                ),
                is_error: false,
            };
        }
    }

    pub fn selected_team_name(&self) -> Option<&str> {
        self.active_team.as_deref()
    }

    pub fn team_names(&self) -> Vec<String> {
        self.teams.iter().map(|team| team.name.clone()).collect()
    }

    pub fn total_unread(&self) -> usize {
        self.teams.iter().map(|team| team.unread_count).sum()
    }

    pub fn selected_message(&self) -> Option<&Message> {
        self.messages.get(self.selected_message)
    }

    pub fn selected_agent_team(&self) -> Option<&AgentTeamSummary> {
        self.agent_teams.get(self.agent_team_index)
    }

    pub fn selected_agent_identity(&self) -> Option<&AgentIdentitySummary> {
        self.selected_agent_team()
            .and_then(|team| team.identities.get(self.agent_identity_index))
    }

    /// The yank source is the model's full body, never the folded rendering.
    pub fn selected_message_body(&self) -> Option<&str> {
        self.selected_message().map(|message| message.body.as_str())
    }

    pub fn body_is_folded(&self, message: &Message) -> bool {
        // Display columns, not char count (S10-5): a CJK-heavy body can pack
        // twice the rendered width into the same char count as an ASCII one,
        // so gating the fold on chars alone let long Japanese messages
        // render fully unfolded well past where an equivalent ASCII message
        // would already have folded.
        crate::width::display_width(&message.body) > FOLD_CHAR_THRESHOLD
            && !self
                .expanded_messages
                .get(&message.team)
                .is_some_and(|ids| ids.contains(&message.id))
    }

    /// `true` when `message` should count as "mine" for the self-message
    /// `▏` marker — only meaningful once `current_identity` is set.
    pub fn is_own_message(&self, message: &Message) -> bool {
        self.current_identity.as_deref() == Some(message.from_agent.as_str())
    }

    /// `true` when `message` should be visible under the active MEMBER
    /// filter (`F` key); always `true` when no filter is set.
    pub fn message_matches_filter(&self, message: &Message) -> bool {
        match &self.member_filter {
            None => true,
            Some(name) => message.from_agent == *name || message.to_agent == *name,
        }
    }

    pub fn message_matches_search(&self, message: &Message) -> bool {
        let Some(query) = self.search_query.as_deref() else {
            return true;
        };
        if query.is_empty() {
            return true;
        }
        let query = query.to_lowercase();
        message.body.to_lowercase().contains(&query)
            || message.from_agent.to_lowercase().contains(&query)
            || message.to_agent.to_lowercase().contains(&query)
    }

    pub fn message_matches_filters(&self, message: &Message) -> bool {
        self.message_matches_filter(message) && self.message_matches_search(message)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<AppAction> {
        if key.kind != KeyEventKind::Press {
            return Ok(AppAction::None);
        }
        let before = self.persistence_snapshot();
        let result = match self.screen {
            Screen::Composer => self.handle_composer_key(key),
            Screen::Help => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                        self.screen = self.help_return_screen;
                        self.help_scroll = 0;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.help_scroll = self.help_scroll.saturating_add(1);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.help_scroll = self.help_scroll.saturating_sub(1);
                    }
                    KeyCode::PageDown => {
                        self.help_scroll = self.help_scroll.saturating_add(10);
                    }
                    KeyCode::PageUp => {
                        self.help_scroll = self.help_scroll.saturating_sub(10);
                    }
                    _ => {}
                }
                Ok(AppAction::None)
            }
            Screen::Audit => self.handle_audit_key(key),
            Screen::Agents => self.handle_agents_key(key),
            Screen::Health => self.handle_health_key(key),
            Screen::BulkFilter => self.handle_bulk_filter_key(key),
            Screen::Main => self.handle_main_key(key),
        };
        if self.persistence_snapshot() != before {
            self.mark_state_changed();
        }
        result
    }

    /// Mouse handling only touches focus/resize state, so it never needs to
    /// return an `AppAction` — unlike key handling, no mouse gesture here
    /// triggers a send/mark-read/audit subprocess.
    pub fn handle_mouse(&mut self, event: MouseEvent, terminal_area: Rect) {
        // Composer/Audit/Help screens keep their own keyboard-only nav for now;
        // mouse focus/resize is scoped to the 3-pane main screen (§1 of the ask).
        if self.screen != Screen::Main {
            return;
        }
        let previous_sidebar_pct = self.sidebar_pct;
        let layout = compute_layout(terminal_area, self.sidebar_pct, self.focus);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                match hit_test(&layout, event.column, event.row) {
                    MouseTarget::ResizeHandle => self.resize_dragging = true,
                    MouseTarget::Teams => self.focus = Focus::Teams,
                    MouseTarget::Members => self.focus = Focus::Members,
                    MouseTarget::Room => self.focus = Focus::Room,
                    MouseTarget::None => {}
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.resize_dragging => {
                self.sidebar_pct = resize_pct_from_column(terminal_area, event.column);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.resize_dragging = false;
            }
            _ => {}
        }
        if self.sidebar_pct != previous_sidebar_pct {
            self.mark_state_changed();
        }
    }

    pub fn receive_new_messages(&mut self, messages: Vec<Message>) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let was_following = self.messages.is_empty()
            || self.selected_message == self.messages.len().saturating_sub(1);
        let selected_team = self.selected_team_name().map(str::to_owned);
        let mut relevant_count = 0usize;
        let mut latest_relevant: Option<Message> = None;
        for message in &messages {
            let matches_selected_team = selected_team.as_deref() == Some(message.team.as_str());
            if matches_selected_team
                && !self
                    .messages
                    .iter()
                    .any(|existing| existing.id == message.id)
            {
                self.messages.push(message.clone());
            }
            // Only the currently selected team's unread traffic should ring/
            // pop a notification — otherwise every background team's chatter
            // would fire alerts the user isn't looking at.
            if matches_selected_team && message.read_at.is_none() {
                relevant_count += 1;
                latest_relevant = Some(message.clone());
            }
        }
        self.messages.sort_by_key(|message| message.id);
        if was_following {
            self.selected_message = self.messages.len().saturating_sub(1);
        }
        self.refresh_team_summaries()?;
        self.refresh_members()?;
        self.status = StatusLine {
            text: format!("{} new message(s)", messages.len()),
            is_error: false,
        };
        self.notify_for_arrivals(relevant_count, latest_relevant, Instant::now());
        Ok(())
    }

    /// Splits burst-detection and BEL/OSC 9 dispatch out of
    /// `receive_new_messages` so the notification decision (pure, testable
    /// via `pending_notifications`/`burst_alert` assertions) isn't tangled
    /// with the DB refresh calls above it.
    fn notify_for_arrivals(
        &mut self,
        relevant_count: usize,
        latest_relevant: Option<Message>,
        now: Instant,
    ) {
        if relevant_count == 0 {
            return;
        }
        if self.notify_settings.burst_alert
            && let Some(total) = self.burst_tracker.record(relevant_count, now)
        {
            self.burst_alert = Some((
                format!("⚠ burst: {total} msgs/1min"),
                now + BURST_ALERT_DURATION,
            ));
        }
        // Spam suppression: while the burst banner is showing, individual
        // BEL/OSC 9 pops would just pile on top of the alert that already
        // told the user "a lot just happened".
        if self.is_burst_active(now) {
            return;
        }
        if self.notify_settings.bell {
            self.pending_notifications.push(PendingNotification::Bell);
        }
        if self.notify_settings.desktop
            && let Some(message) = latest_relevant
        {
            self.pending_notifications.push(PendingNotification::Desktop {
                from: message.from_agent,
                body: message.body,
            });
        }
    }

    /// `true` while the burst banner set by `notify_for_arrivals` is still
    /// within its display window.
    pub fn is_burst_active(&self, now: Instant) -> bool {
        self.burst_alert
            .as_ref()
            .is_some_and(|(_, until)| now < *until)
    }

    /// Drains BEL/OSC 9 events queued by `receive_new_messages` for `main.rs`
    /// to actually write to stdout.
    pub fn drain_pending_notifications(&mut self) -> Vec<PendingNotification> {
        std::mem::take(&mut self.pending_notifications)
    }

    pub fn complete_send(&mut self, result: &CommandResult) -> Result<()> {
        if result.success {
            if let Some(team) = self.selected_team_name().map(str::to_owned) {
                self.drafts.remove(&team);
            }
            self.screen = Screen::Main;
            self.composer = None;
            self.mark_state_changed();
            self.refresh_team_summaries()?;
            self.reload_selected_team()?;
            self.status = StatusLine {
                text: if result.stdout.is_empty() {
                    "sent".to_owned()
                } else {
                    result.stdout.clone()
                },
                is_error: false,
            };
        } else {
            let detail = if result.stderr.is_empty() {
                format!("send.sh failed (exit {:?})", result.exit_code)
            } else {
                result.stderr.clone()
            };
            self.status = StatusLine {
                text: detail,
                is_error: true,
            };
        }
        Ok(())
    }

    pub fn complete_mark_read(
        &mut self,
        result: &CommandResult,
        label: &str,
        unread_count: usize,
    ) -> Result<()> {
        self.refresh_team_summaries()?;
        self.reload_selected_team()?;
        if self.screen == Screen::Audit {
            self.refresh_audit_analytics()?;
        }
        if result.success {
            self.status = StatusLine {
                text: format!("marked read: all for {label} ({unread_count} msgs)"),
                is_error: false,
            };
        } else {
            self.status = StatusLine {
                text: if result.stderr.is_empty() {
                    format!("inbox.sh failed (exit {:?})", result.exit_code)
                } else {
                    result.stderr.clone()
                },
                is_error: true,
            };
        }
        Ok(())
    }

    pub fn complete_audit(&mut self, result: &CommandResult) -> Result<()> {
        self.audit_loading = false;
        if !result.success {
            self.status = StatusLine {
                text: if result.stderr.is_empty() {
                    format!("agmsg-audit failed (exit {:?})", result.exit_code)
                } else {
                    result.stderr.clone()
                },
                is_error: true,
            };
            return Ok(());
        }
        self.audit_report = Some(parse_audit_stdout(&result.stdout)?);
        self.refresh_audit_insights()?;
        self.refresh_audit_analytics()?;
        self.audit_detail = None;
        self.status = StatusLine {
            text: if result.stderr.is_empty() {
                "audit refreshed".to_owned()
            } else {
                format!("audit refreshed; warning: {}", result.stderr)
            },
            is_error: false,
        };
        Ok(())
    }

    pub fn complete_audit_error(&mut self, message: String) {
        self.audit_loading = false;
        self.status = StatusLine {
            text: message,
            is_error: true,
        };
    }

    pub fn request_audit_refresh(&mut self) -> AppAction {
        if self.audit_loading {
            return AppAction::None;
        }
        self.audit_loading = true;
        self.status = StatusLine {
            text: "loading audit...".to_owned(),
            is_error: false,
        };
        AppAction::RefreshAudit
    }

    pub fn complete_health(&mut self, snapshot: HealthSnapshot) {
        self.health_loading = false;
        self.health_team_index = self
            .health_team_index
            .min(snapshot.teams.len().saturating_sub(1));
        self.health_snapshot = Some(snapshot);
        self.status = StatusLine {
            text: "health refreshed".to_owned(),
            is_error: false,
        };
    }

    pub fn complete_health_error(&mut self, message: String) {
        self.health_loading = false;
        self.status = StatusLine {
            text: message,
            is_error: true,
        };
    }

    pub fn request_health_refresh(&mut self) -> AppAction {
        if self.health_loading {
            return AppAction::None;
        }
        self.health_loading = true;
        self.status = StatusLine {
            text: "loading health...".to_owned(),
            is_error: false,
        };
        AppAction::RefreshHealth
    }

    pub fn current_health_team(&self) -> Option<&TeamHealth> {
        self.health_snapshot
            .as_ref()?
            .teams
            .get(self.health_team_index)
    }

    pub fn export_audit_report(&mut self) -> Result<()> {
        let report = self
            .audit_report
            .as_ref()
            .context("audit未取得です。Rでrefreshしてください")?;
        let stamp = report_stamp(&report.ts)?;
        let path = self
            .paths
            .report_dir
            .join(format!("agmsg-report-{stamp}.md"));
        fs::create_dir_all(&self.paths.report_dir).with_context(|| {
            format!(
                "reportディレクトリを作成できません: {}",
                self.paths.report_dir.display()
            )
        })?;
        let markdown = build_markdown_for_window(
            report,
            &self.pair_matrices,
            &self.zombies,
            &self.stale_unreads,
            self.audit_pair_window_days,
        );
        fs::write(&path, markdown)
            .with_context(|| format!("reportを書き込めません: {}", path.display()))?;
        self.status = StatusLine {
            text: format!("exported: {}", path.display()),
            is_error: false,
        };
        Ok(())
    }

    pub fn current_pair_matrix(&self) -> Option<&PairMatrix> {
        self.pair_matrices.get(self.audit_team_index)
    }

    pub fn audit_item_count(&self) -> usize {
        self.zombies.len() + self.stale_unreads.len()
    }

    pub fn selected_audit_item(&self) -> Option<AuditSelection<'_>> {
        if self.audit_selected < self.zombies.len() {
            return self
                .zombies
                .get(self.audit_selected)
                .map(AuditSelection::Zombie);
        }
        self.stale_unreads
            .get(self.audit_selected.saturating_sub(self.zombies.len()))
            .map(AuditSelection::Stale)
    }

    pub fn set_error(&mut self, error: &anyhow::Error) {
        self.status = StatusLine {
            text: error.to_string(),
            is_error: true,
        };
    }

    pub fn set_poll_error(&mut self, error: &anyhow::Error) {
        self.poll_offline = true;
        self.status = StatusLine {
            text: format!("poll offline: {error}"),
            is_error: true,
        };
    }

    pub fn set_poll_recovered(&mut self) {
        self.poll_offline = false;
        self.status = StatusLine {
            text: "poll recovered".to_owned(),
            is_error: false,
        };
    }

    /// L-4: bell/OSC 9/title notification IO used to be dropped with
    /// `let _ = ...` in `main.rs`, so a broken terminal (e.g. no tmux
    /// passthrough) never told the user why notifications stopped. Surface
    /// it once — repeating it on every message arrival would just replace
    /// one silent failure mode with a noisy one.
    pub fn warn_notify_failure_once(&mut self) {
        if self.notify_delivery_warned {
            return;
        }
        self.notify_delivery_warned = true;
        self.status = StatusLine {
            text: "⚠ notification unavailable".to_owned(),
            is_error: false,
        };
    }

    /// L-4: `pbcopy` failing used to be swallowed inside `clipboard::yank`
    /// (OSC 52 alone was treated as success), so a broken clipboard bridge
    /// left the user thinking the copy worked. When `pbcopy` fails we still
    /// keep the OSC 52 escape sequence (some terminals honor it), but also
    /// persist the body to disk and say so, instead of claiming "yanked".
    pub fn complete_yank_fallback(&mut self, body: &str) {
        let path = self.paths.report_dir.join("agmsg-tui-clipboard-fallback.txt");
        match fs::create_dir_all(&self.paths.report_dir).and_then(|()| fs::write(&path, body)) {
            Ok(()) => {
                self.status = StatusLine {
                    text: format!("clipboard unavailable, message logged to {}", path.display()),
                    is_error: false,
                };
            }
            Err(error) => {
                self.status = StatusLine {
                    text: format!("clipboard unavailable and fallback log failed: {error}"),
                    is_error: true,
                };
            }
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent) -> Result<AppAction> {
        // Modal: the notification settings popup swallows everything except
        // its own nav/close keys, same shape as the MEMBER info popup below.
        if self.notify_popup.is_some() {
            self.handle_notify_popup_key(key);
            return Ok(AppAction::None);
        }
        // Modal: the MEMBER info popup swallows everything except its own
        // close keys, same as the audit detail popup does in handle_audit_key.
        if self.member_info.is_some() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('I')) {
                self.member_info = None;
            }
            return Ok(AppAction::None);
        }
        if self.input_mode == InputMode::Search {
            return self.handle_search_key(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f') {
            self.open_bulk_filter()?;
            return Ok(AppAction::None);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            return self.open_audit();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b') {
            self.notify_settings.bell = !self.notify_settings.bell;
            self.status = StatusLine {
                text: if self.notify_settings.bell {
                    "terminal bell unmuted".to_owned()
                } else {
                    "terminal bell muted".to_owned()
                },
                is_error: false,
            };
            return Ok(AppAction::None);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
            self.notify_popup = Some(0);
            return Ok(AppAction::None);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
            self.move_half_page(1)?;
            return Ok(AppAction::None);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            self.move_half_page(-1)?;
            return Ok(AppAction::None);
        }
        if key.code == KeyCode::Esc {
            if self.search_query.is_some() {
                self.clear_search();
                return Ok(AppAction::None);
            }
            if self.member_filter.is_some() {
                self.member_filter = None;
                self.status = StatusLine {
                    text: "filter cleared".to_owned(),
                    is_error: false,
                };
                return Ok(AppAction::None);
            }
            return Ok(AppAction::None);
        }
        match key.code {
            KeyCode::Char('q') => return Ok(AppAction::Quit),
            KeyCode::Tab => self.focus = self.focus_next(),
            KeyCode::BackTab => self.focus = self.focus_previous(),
            KeyCode::Char('j') | KeyCode::Down => self.move_down(),
            KeyCode::Char('k') | KeyCode::Up => self.move_up()?,
            KeyCode::Char('g') => self.move_first(),
            KeyCode::Char('G') => self.move_last(),
            KeyCode::Char('u') => self.move_to_next_unread(),
            KeyCode::Char('[') => self.cycle_team(-1)?,
            KeyCode::Char(']') => self.cycle_team(1)?,
            KeyCode::Enter => self.activate_selection()?,
            KeyCode::Char('c') => self.open_composer()?,
            KeyCode::Char('r') => return self.mark_selected_action(),
            // Guarded arms must precede the plain 'n'/'R' ones below: with
            // MEMBER pane focus these are agents-adjacent actions (Phase
            // 8.5), everywhere else they keep their Phase 1-7 meaning
            // (search-next / mark-team-read).
            KeyCode::Char('n') if self.focus == Focus::Members => {
                self.open_agents_spawn_from_members();
            }
            KeyCode::Char('R') if self.focus == Focus::Members => {
                self.open_agents_rename_from_members();
            }
            KeyCode::Char('R') => return self.mark_team_action(),
            KeyCode::Char('a') => return self.open_audit(),
            KeyCode::Char('A') => self.open_agents(),
            KeyCode::Char('H') => return Ok(self.open_health()),
            KeyCode::Char('x') | KeyCode::Char('X') => self.toggle_message_fold(),
            KeyCode::Char('f') => self.toggle_fold_all(),
            KeyCode::Char('s') => self.jump_to_sender(),
            KeyCode::Char('I') => self.toggle_member_info()?,
            KeyCode::Char('F') => self.toggle_member_filter(),
            KeyCode::Char('M') => return self.mark_member_action(),
            KeyCode::Char('/') => self.begin_search(),
            KeyCode::Char('n') => self.cycle_search_match(1),
            KeyCode::Char('N') => self.cycle_search_match(-1),
            KeyCode::Char('y') => {
                if let Some(body) = self.selected_message_body() {
                    return Ok(AppAction::Yank(body.to_owned()));
                }
                self.status = validation_status("no message selected".to_owned());
            }
            KeyCode::Char('?') => {
                self.help_return_screen = Screen::Main;
                self.help_scroll = 0;
                self.screen = Screen::Help;
            }
            _ => {}
        }
        Ok(AppAction::None)
    }

    /// `Ctrl+N` popup: `j/k` move the highlighted row, `Enter`/`Space` toggle
    /// it, `Esc` closes. No `AppAction` ever comes out of here — like mouse
    /// handling, this only flips in-memory flags, no subprocess involved.
    fn handle_notify_popup_key(&mut self, key: KeyEvent) {
        let Some(selected) = self.notify_popup else {
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.notify_popup = None,
            KeyCode::Char('j') | KeyCode::Down => {
                self.notify_popup = Some((selected + 1).min(NOTIFY_SETTING_COUNT - 1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.notify_popup = Some(selected.saturating_sub(1));
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.notify_settings.toggle(selected),
            _ => {}
        }
    }

    fn open_agents(&mut self) {
        self.screen = Screen::Agents;
        self.agent_modal = None;
        if let Err(error) = self.reload_agents() {
            self.set_error(&error);
        }
    }

    /// Shared by the Agents screen `n` handler and the Phase 8.5 MEMBER-pane
    /// `n` shortcut — both just need a default team index to seed the wizard.
    fn open_spawn_modal(&mut self, team_index: usize) {
        self.agent_modal = Some(AgentModal::Spawn(SpawnModalState::new(team_index)));
    }

    /// Shared by the Agents screen `R` handler and the Phase 8.5 MEMBER-pane
    /// `R` shortcut — both just need a resolved rename target.
    fn open_rename_modal(&mut self, target: AgentIdentitySummary) {
        let self_rename = self.current_identity.as_deref() == Some(&target.name);
        self.agent_modal = Some(AgentModal::Rename {
            target,
            input: String::new(),
            confirming: false,
            self_rename,
        });
    }

    /// Phase 8.5: `n` from the MEMBER pane opens the same spawn wizard as
    /// the Agents screen, defaulting the team to whatever team the MEMBER
    /// pane is currently showing (falls back to the prior agents-screen team
    /// if the MEMBER-pane team has no agent-inventory entry yet, e.g. a
    /// brand new team dir). No member needs to be selected — only the team
    /// default is required.
    fn open_agents_spawn_from_members(&mut self) {
        self.screen = Screen::Agents;
        self.agent_modal = None;
        if let Err(error) = self.reload_agents() {
            self.set_error(&error);
            return;
        }
        self.agent_focus = AgentFocus::Teams;
        let team_index = self
            .selected_team_name()
            .and_then(|name| self.agent_teams.iter().position(|team| team.name == name))
            .unwrap_or(self.agent_team_index);
        self.agent_team_index = team_index;
        self.open_spawn_modal(team_index);
    }

    /// Phase 8.5: `R` from the MEMBER pane opens the same rename modal as
    /// the Agents screen `R` handler, pre-resolved to the currently
    /// selected member (matched by name within the MEMBER pane's team).
    /// No-op if no member is selected or the member isn't a known agent
    /// identity (e.g. a human/legacy sender with no team/config.json entry).
    fn open_agents_rename_from_members(&mut self) {
        let Some(member) = self.members.get(self.selected_member).cloned() else {
            self.status = validation_status("no member selected".to_owned());
            return;
        };
        self.screen = Screen::Agents;
        self.agent_modal = None;
        if let Err(error) = self.reload_agents() {
            self.set_error(&error);
            return;
        }
        let Some(team_name) = self.selected_team_name().map(str::to_owned) else {
            return;
        };
        let Some(team_index) = self.agent_teams.iter().position(|team| team.name == team_name)
        else {
            self.status = StatusLine {
                text: "team情報が見つかりません".to_owned(),
                is_error: true,
            };
            return;
        };
        self.agent_team_index = team_index;
        self.agent_focus = AgentFocus::Identities;
        let Some(identity_index) = self.agent_teams[team_index]
            .identities
            .iter()
            .position(|identity| identity.name == member.name)
        else {
            self.status = StatusLine {
                text: format!("{} はagent identityとして未登録です", member.name),
                is_error: true,
            };
            return;
        };
        self.agent_identity_index = identity_index;
        let target = self.agent_teams[team_index].identities[identity_index].clone();
        self.open_rename_modal(target);
    }

    fn handle_agents_key(&mut self, key: KeyEvent) -> Result<AppAction> {
        if let Some(action) = self.handle_bulk_overlay_key(key)? {
            return Ok(action);
        }
        // M-1: agent management scripts now run off the render thread
        // (`InFlightOperation::Agent`), same shape as Send. While one is in
        // flight we swallow input here too — same as `handle_composer_key`
        // does for `InFlightOperation::Send` — so a second modal can't fire
        // a second script against the same identity/team mid-flight.
        if self.in_flight == Some(InFlightOperation::Agent) {
            self.update_spinner_status();
            return Ok(AppAction::None);
        }
        if self.agent_modal.is_some() {
            return self.handle_agent_modal_key(key);
        }
        // L-3: the identity info popup swallows everything except its own
        // close keys, same shape as `member_info` on the Main screen.
        if self.agent_identity_info.is_some() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                self.agent_identity_info = None;
            }
            return Ok(AppAction::None);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('A') => {
                self.screen = Screen::Main;
            }
            KeyCode::Tab => {
                self.agent_focus = match self.agent_focus {
                    AgentFocus::Teams => AgentFocus::Identities,
                    AgentFocus::Identities => AgentFocus::Teams,
                };
            }
            KeyCode::Char('t') => self.agent_focus = AgentFocus::Teams,
            KeyCode::Enter if self.agent_focus == AgentFocus::Teams => {
                self.agent_focus = AgentFocus::Identities;
            }
            // L-3: Enter on an Identities-focus row used to do nothing —
            // there was no equivalent of the MEMBER pane's `I` info popup
            // on this screen at all. Reuses the same registration + traffic
            // lookups `toggle_member_info` already does for Main.
            KeyCode::Enter if self.agent_focus == AgentFocus::Identities => {
                self.toggle_agent_identity_info()?;
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_agent_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_agent_cursor(-1),
            KeyCode::Char('g') => self.move_agent_edge(false),
            KeyCode::Char('G') => self.move_agent_edge(true),
            KeyCode::Char('r') => {
                if let Err(error) = self.reload_agents() {
                    self.set_error(&error);
                } else {
                    self.status = StatusLine {
                        text: "agents reloaded".to_owned(),
                        is_error: false,
                    };
                }
            }
            KeyCode::Char('n') => {
                self.open_spawn_modal(self.agent_team_index);
            }
            // Both `R` and `T` used to guard on `agent_focus` matching their
            // "natural" pane (Identities for R, Teams for T), silently
            // no-op'ing from the other pane even though the target row is
            // visibly selected there too — same silent-guard bug L had
            // before Phase 10.5. Rename doesn't destroy anything, so both
            // now resolve their target from whichever cursor is live
            // instead of requiring a Tab first.
            KeyCode::Char('R') => match self.agent_focus {
                AgentFocus::Identities => {
                    if let Some(target) = self.selected_agent_identity().cloned() {
                        self.open_rename_modal(target);
                    } else {
                        self.status = validation_status("no identity selected".to_owned());
                    }
                }
                AgentFocus::Teams => {
                    let target = self.selected_agent_team().and_then(|team| {
                        team.identities
                            .get(self.agent_identity_index)
                            .or_else(|| team.identities.first())
                            .cloned()
                    });
                    match target {
                        Some(target) => self.open_rename_modal(target),
                        None => {
                            let team_name = self
                                .selected_agent_team()
                                .map(|team| team.name.clone())
                                .unwrap_or_default();
                            self.status = validation_status(format!(
                                "no identities to rename in {team_name}"
                            ));
                        }
                    }
                }
            },
            KeyCode::Char('T') => {
                if let Some(team) = self.selected_agent_team() {
                    self.agent_modal = Some(AgentModal::RenameTeam {
                        old: team.name.clone(),
                        input: String::new(),
                        confirming: false,
                    });
                } else {
                    self.status = validation_status("no team selected".to_owned());
                }
            }
            // Reset is destructive, so unlike R/T it does not fall back to
            // an implicit target from Teams focus — it just tells the user
            // how to get to a selectable one, matching the audit's H-1
            // guidance (no silent fallback for `X`).
            KeyCode::Char('X') | KeyCode::Delete => {
                if self.agent_focus == AgentFocus::Teams {
                    self.status = validation_status(
                        "X: switch to identity focus (Tab) and select a target".to_owned(),
                    );
                } else if let Some(target) = self.selected_agent_identity().cloned() {
                    let blocked = self.current_identity.as_deref() == Some(&target.name);
                    if blocked {
                        self.status = StatusLine {
                            text: "self-reset refused; use the session-side drop command".to_owned(),
                            is_error: true,
                        };
                    }
                    self.agent_modal = Some(AgentModal::Reset {
                        target,
                        confirm: String::new(),
                        blocked,
                    });
                }
            }
            KeyCode::Char('D') => self.open_despawn_preview(),
            KeyCode::Char('L') if self.agent_focus == AgentFocus::Teams => {
                self.open_leave_modal_from_team_focus();
            }
            KeyCode::Char('L') if self.agent_focus == AgentFocus::Identities => {
                self.open_leave_modal_from_identity_focus();
            }
            KeyCode::Char('?') => {
                self.help_return_screen = Screen::Agents;
                self.help_scroll = 0;
                self.screen = Screen::Help;
            }
            _ => {}
        }
        Ok(AppAction::None)
    }

    fn move_agent_cursor(&mut self, step: isize) {
        match self.agent_focus {
            AgentFocus::Teams => {
                self.agent_team_index = move_bounded(
                    self.agent_team_index,
                    self.agent_teams.len(),
                    step,
                );
                self.agent_identity_index = 0;
            }
            AgentFocus::Identities => {
                let length = self
                    .selected_agent_team()
                    .map(|team| team.identities.len())
                    .unwrap_or_default();
                self.agent_identity_index =
                    move_bounded(self.agent_identity_index, length, step);
            }
        }
    }

    fn move_agent_edge(&mut self, last: bool) {
        match self.agent_focus {
            AgentFocus::Teams => {
                self.agent_team_index = if last {
                    self.agent_teams.len().saturating_sub(1)
                } else {
                    0
                };
                self.agent_identity_index = 0;
            }
            AgentFocus::Identities => {
                self.agent_identity_index = if last {
                    self.selected_agent_team()
                        .map(|team| team.identities.len().saturating_sub(1))
                        .unwrap_or_default()
                } else {
                    0
                };
            }
        }
    }

    /// `L` from Teams focus. `leave.sh` only needs `<team> <agent>`, so
    /// `AGMSG_IDENTITY` is a convenience default, not a requirement: when set
    /// and registered in the selected team it's used as before, otherwise we
    /// fall back to the team's first identity so the modal always opens.
    fn open_leave_modal_from_team_focus(&mut self) {
        let Some(team) = self.selected_agent_team() else {
            return;
        };
        let team_name = team.name.clone();
        if let Some(identity) = self.current_identity.clone() {
            let registered = team.identities.iter().any(|row| row.name == identity);
            if !registered {
                self.status = StatusLine {
                    text: format!("{identity} is not registered in {team_name}"),
                    is_error: true,
                };
                return;
            }
            self.open_leave_modal(team_name, identity);
            return;
        }
        let Some(agent) = team.identities.first().map(|row| row.name.clone()) else {
            self.status = StatusLine {
                text: format!("{team_name} has no identities to leave"),
                is_error: true,
            };
            return;
        };
        self.open_leave_modal(team_name, agent);
    }

    /// `L` from Identities focus: leave whichever identity row is under the
    /// cursor. This is the common case (no `AGMSG_IDENTITY` juggling needed)
    /// and previously did nothing at all.
    fn open_leave_modal_from_identity_focus(&mut self) {
        let Some(team) = self.selected_agent_team().map(|team| team.name.clone()) else {
            return;
        };
        let Some(agent) = self.selected_agent_identity().map(|row| row.name.clone()) else {
            self.status = StatusLine {
                text: "select an identity row to leave, or focus a team".to_owned(),
                is_error: true,
            };
            return;
        };
        self.open_leave_modal(team, agent);
    }

    fn open_leave_modal(&mut self, team: String, agent: String) {
        self.agent_modal = Some(AgentModal::Leave {
            team,
            agent,
            confirm: String::new(),
        });
    }

    fn handle_agent_modal_key(&mut self, key: KeyEvent) -> Result<AppAction> {
        let Some(mut modal) = self.agent_modal.take() else {
            return Ok(AppAction::None);
        };
        let mut keep_open = true;
        let mut action = AppAction::None;
        match &mut modal {
            AgentModal::Spawn(state) => match state.step {
                SpawnStep::Team => match key.code {
                    KeyCode::Esc => keep_open = false,
                    KeyCode::Char('j') | KeyCode::Down => {
                        state.team_index = move_bounded(
                            state.team_index,
                            self.agent_teams.len().saturating_add(1),
                            1,
                        );
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        state.team_index = move_bounded(
                            state.team_index,
                            self.agent_teams.len().saturating_add(1),
                            -1,
                        );
                    }
                    KeyCode::Enter => {
                        state.new_team = state.team_index == self.agent_teams.len();
                        state.step = if state.new_team {
                            SpawnStep::NewTeam
                        } else {
                            SpawnStep::CliType
                        };
                    }
                    _ => {}
                },
                SpawnStep::NewTeam => match key.code {
                    KeyCode::Esc => {
                        state.step = SpawnStep::Team;
                        state.new_team = false;
                    }
                    KeyCode::Backspace => {
                        state.team_input.pop();
                    }
                    KeyCode::Enter => match validate_team_name(&state.team_input) {
                        Ok(()) => state.step = SpawnStep::CliType,
                        Err(error) => self.status = validation_status(error),
                    },
                    KeyCode::Char(character) if plain_text_key(key) => {
                        state.team_input.push(character);
                    }
                    _ => {}
                },
                SpawnStep::CliType => match key.code {
                    KeyCode::Esc => {
                        state.step = if state.new_team {
                            SpawnStep::NewTeam
                        } else {
                            SpawnStep::Team
                        };
                    }
                    KeyCode::Char('j') | KeyCode::Right | KeyCode::Down => {
                        state.type_index = (state.type_index + 1) % CLI_TYPES.len();
                    }
                    KeyCode::Char('k') | KeyCode::Left | KeyCode::Up => {
                        state.type_index = if state.type_index == 0 {
                            CLI_TYPES.len() - 1
                        } else {
                            state.type_index - 1
                        };
                    }
                    KeyCode::Enter => state.step = SpawnStep::Name,
                    _ => {}
                },
                SpawnStep::Name => match key.code {
                    KeyCode::Esc => state.step = SpawnStep::CliType,
                    KeyCode::Backspace => {
                        state.name.pop();
                    }
                    KeyCode::Enter => {
                        let agent_type = state.agent_type();
                        match validate_agent_name(&state.name, agent_type) {
                            Err(error) => self.status = validation_status(error),
                            Ok(()) => {
                                let team = if state.new_team {
                                    state.team_input.clone()
                                } else {
                                    self.agent_teams
                                        .get(state.team_index)
                                        .map(|team| team.name.clone())
                                        .unwrap_or_default()
                                };
                                let exists = self.agent_teams.iter().any(|team| {
                                    team.identities.iter().any(|row| row.name == state.name)
                                });
                                if exists && !state.new_team {
                                    self.status = validation_status(
                                        "existing agent can only join a new team".to_owned(),
                                    );
                                } else if exists {
                                    let project = env::current_dir()
                                        .context("cwdを取得できません")?
                                        .to_string_lossy()
                                        .into_owned();
                                    action = AppAction::ManageAgent(AgentOperation::Join {
                                        team,
                                        agent: state.name.clone(),
                                        agent_type: agent_type.to_owned(),
                                        project,
                                    });
                                    keep_open = false;
                                } else {
                                    action = AppAction::ManageAgent(AgentOperation::Spawn {
                                        team,
                                        agent_type: agent_type.to_owned(),
                                        name: state.name.clone(),
                                    });
                                    keep_open = false;
                                }
                            }
                        }
                    }
                    KeyCode::Char(character) if plain_text_key(key) => state.name.push(character),
                    _ => {}
                },
            },
            AgentModal::Rename {
                target,
                input,
                confirming,
                self_rename,
            } => {
                if *confirming {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            action = AppAction::ManageAgent(AgentOperation::Rename {
                                team: target.team.clone(),
                                old: target.name.clone(),
                                new: input.clone(),
                                self_rename: *self_rename,
                            });
                            keep_open = false;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') => *confirming = false,
                        KeyCode::Esc => keep_open = false,
                        _ => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Esc => keep_open = false,
                        KeyCode::Backspace => {
                            input.pop();
                        }
                        KeyCode::Enter => match validate_agent_name(input, &target.agent_type) {
                            Ok(()) => *confirming = true,
                            Err(error) => self.status = validation_status(error),
                        },
                        KeyCode::Char(character) if plain_text_key(key) => input.push(character),
                        _ => {}
                    }
                }
            }
            AgentModal::RenameTeam {
                old,
                input,
                confirming,
            } => {
                if *confirming {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            action = AppAction::ManageAgent(AgentOperation::RenameTeam {
                                old: old.clone(),
                                new: input.clone(),
                            });
                            keep_open = false;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') => *confirming = false,
                        KeyCode::Esc => keep_open = false,
                        _ => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Esc => keep_open = false,
                        KeyCode::Backspace => {
                            input.pop();
                        }
                        KeyCode::Enter => match validate_team_name(input) {
                            Ok(()) => *confirming = true,
                            Err(error) => self.status = validation_status(error),
                        },
                        KeyCode::Char(character) if plain_text_key(key) => input.push(character),
                        _ => {}
                    }
                }
            }
            AgentModal::Reset {
                target,
                confirm,
                blocked,
            } => {
                if *blocked {
                    if key.code == KeyCode::Esc {
                        keep_open = false;
                    }
                } else {
                    match key.code {
                        KeyCode::Esc => keep_open = false,
                        KeyCode::Backspace => {
                            confirm.pop();
                        }
                        KeyCode::Enter if confirm == "YES" => {
                            action = AppAction::ManageAgent(AgentOperation::Reset {
                                project: target.project.clone(),
                                agent_type: target.agent_type.clone(),
                                agent: target.name.clone(),
                            });
                            keep_open = false;
                        }
                        KeyCode::Enter => {
                            self.status = validation_status("type YES exactly".to_owned());
                        }
                        KeyCode::Char(character) if plain_text_key(key) => confirm.push(character),
                        _ => {}
                    }
                }
            }
            AgentModal::Leave {
                team,
                agent,
                confirm,
            } => match key.code {
                KeyCode::Esc => keep_open = false,
                KeyCode::Backspace => {
                    confirm.pop();
                }
                KeyCode::Enter if confirm == "YES" => {
                    action = AppAction::ManageAgent(AgentOperation::Leave {
                        team: team.clone(),
                        agent: agent.clone(),
                    });
                    keep_open = false;
                }
                KeyCode::Enter => {
                    self.status = validation_status("type YES exactly".to_owned());
                }
                KeyCode::Char(character) if plain_text_key(key) => confirm.push(character),
                _ => {}
            },
            AgentModal::JoinForce {
                team,
                agent,
                agent_type,
                project,
            } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    action = AppAction::ManageAgent(AgentOperation::JoinForce {
                        team: team.clone(),
                        agent: agent.clone(),
                        agent_type: agent_type.clone(),
                        project: project.clone(),
                    });
                    keep_open = false;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => keep_open = false,
                _ => {}
            },
        }
        if keep_open {
            self.agent_modal = Some(modal);
        }
        Ok(action)
    }

    pub fn complete_agent_operation(
        &mut self,
        operation: &AgentOperation,
        result: &CommandResult,
    ) -> Result<()> {
        self.refresh_after_agent_operation()?;
        if !result.success {
            let detail = command_failure_detail(result);
            if let AgentOperation::Join {
                team,
                agent,
                agent_type,
                project,
            } = operation
                && detail.contains("was renamed to")
            {
                self.agent_modal = Some(AgentModal::JoinForce {
                    team: team.clone(),
                    agent: agent.clone(),
                    agent_type: agent_type.clone(),
                    project: project.clone(),
                });
            }
            self.status = StatusLine {
                text: detail,
                is_error: true,
            };
            return Ok(());
        }

        if matches!(operation, AgentOperation::Reset { .. })
            && result.stdout.contains("No registrations removed.")
        {
            self.status = StatusLine {
                text: "⚠ No registrations removed.".to_owned(),
                is_error: false,
            };
            return Ok(());
        }

        self.status = match operation {
            AgentOperation::Spawn { .. } => StatusLine {
                text: "spawned --no-wait; agent is starting in another pane/window".to_owned(),
                is_error: false,
            },
            AgentOperation::Rename {
                new,
                self_rename: true,
                ..
            } => {
                self.agent_restart_needed = Some(new.clone());
                StatusLine {
                    text: "renamed; bridge restart required".to_owned(),
                    is_error: false,
                }
            }
            AgentOperation::RenameTeam { .. } => StatusLine {
                text: "⚠ renamed team; members in other projects must rerun whoami".to_owned(),
                is_error: false,
            },
            _ => StatusLine {
                text: if result.stdout.is_empty() {
                    "agent operation completed".to_owned()
                } else {
                    result.stdout.clone()
                },
                is_error: false,
            },
        };
        Ok(())
    }

    pub fn complete_agent_error(&mut self, error: &anyhow::Error) {
        let refresh_error = self.refresh_after_agent_operation().err();
        self.status = StatusLine {
            text: refresh_error
                .map(|refresh| format!("{error}; reload failed: {refresh}"))
                .unwrap_or_else(|| error.to_string()),
            is_error: true,
        };
    }

    fn refresh_after_agent_operation(&mut self) -> Result<()> {
        self.refresh_team_summaries()?;
        self.reload_selected_team()?;
        self.reload_agents()?;
        Ok(())
    }

    fn reload_agents(&mut self) -> Result<()> {
        let selected_team = self
            .selected_agent_team()
            .map(|team| team.name.clone());
        let selected_identity = self.selected_agent_identity().map(|identity| {
            (
                identity.name.clone(),
                identity.agent_type.clone(),
                identity.project.clone(),
            )
        });
        self.agent_teams = self.database.agent_inventory(&self.paths.teams_dir)?;
        self.agent_team_index = selected_team
            .as_deref()
            .and_then(|name| self.agent_teams.iter().position(|team| team.name == name))
            .unwrap_or_else(|| {
                self.agent_team_index
                    .min(self.agent_teams.len().saturating_sub(1))
            });
        self.agent_identity_index = selected_identity
            .as_ref()
            .and_then(|(name, agent_type, project)| {
                self.selected_agent_team().and_then(|team| {
                    team.identities.iter().position(|identity| {
                        &identity.name == name
                            && &identity.agent_type == agent_type
                            && &identity.project == project
                    })
                })
            })
            .unwrap_or_else(|| {
                self.agent_identity_index.min(
                    self.selected_agent_team()
                        .map(|team| team.identities.len().saturating_sub(1))
                        .unwrap_or_default(),
                )
            });
        Ok(())
    }

    fn handle_composer_key(&mut self, key: KeyEvent) -> Result<AppAction> {
        if self.in_flight == Some(InFlightOperation::Send) {
            return Ok(AppAction::None);
        }
        if key.code == KeyCode::Esc {
            let team = self.selected_team_name().map(str::to_owned);
            if let (Some(team), Some(composer)) = (team, self.composer.take()) {
                self.drafts.insert(team, composer);
            }
            self.screen = Screen::Main;
            self.status = StatusLine {
                text: "draft saved".to_owned(),
                is_error: false,
            };
            return Ok(AppAction::None);
        }

        let Some(composer) = self.composer.as_mut() else {
            self.screen = Screen::Main;
            return Ok(AppAction::None);
        };

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('s') => {
                    if self.in_flight.is_some() {
                        self.update_spinner_status();
                        return Ok(AppAction::None);
                    }
                    let Some(team) = self.selected_team_name().map(str::to_owned) else {
                        return Ok(AppAction::None);
                    };
                    let Some(composer) = self.composer.as_ref() else {
                        return Ok(AppAction::None);
                    };
                    if composer.body.trim().is_empty() {
                        self.status = StatusLine {
                            text: "body is empty".to_owned(),
                            is_error: true,
                        };
                        return Ok(AppAction::None);
                    }
                    if composer.body_size_level() == BodySizeLevel::Blocked {
                        self.status = StatusLine {
                            text: format!(
                                "body {}B exceeds {}B; share a file path instead",
                                composer.body_bytes(),
                                BODY_BLOCK_BYTES
                            ),
                            is_error: true,
                        };
                        return Ok(AppAction::None);
                    }
                    return Ok(AppAction::Send(SendRequest {
                        team,
                        from: composer.from_agent().unwrap_or_default().to_owned(),
                        to: composer.to_agent().unwrap_or_default().to_owned(),
                        body: composer.body.clone(),
                    }));
                }
                KeyCode::Char('a') => composer.cursor = 0,
                KeyCode::Char('e') => composer.cursor = composer.body.chars().count(),
                KeyCode::Char('w') => composer.delete_previous_word(),
                KeyCode::Char('k') => {
                    composer.body.clear();
                    composer.cursor = 0;
                    if let Some(team) = self.selected_team_name().map(str::to_owned) {
                        self.drafts.remove(&team);
                    }
                    self.status = StatusLine {
                        text: "draft cleared".to_owned(),
                        is_error: false,
                    };
                }
                _ => {}
            }
            return Ok(AppAction::None);
        }

        match key.code {
            KeyCode::Tab => {
                composer.from_index = next_index(composer.from_index, composer.roster.len());
            }
            KeyCode::BackTab => {
                composer.to_index = next_index(composer.to_index, composer.roster.len());
            }
            KeyCode::Left => composer.cursor = composer.cursor.saturating_sub(1),
            KeyCode::Right => {
                composer.cursor = (composer.cursor + 1).min(composer.body.chars().count());
            }
            KeyCode::Home => composer.cursor = 0,
            KeyCode::End => composer.cursor = composer.body.chars().count(),
            KeyCode::Backspace => composer.backspace(),
            KeyCode::Delete => composer.delete(),
            KeyCode::Enter => composer.insert_char('\n'),
            KeyCode::Char(character) => composer.insert_char(character),
            _ => {}
        }
        Ok(AppAction::None)
    }

    fn handle_audit_key(&mut self, key: KeyEvent) -> Result<AppAction> {
        if let Some(action) = self.handle_bulk_overlay_key(key)? {
            return Ok(action);
        }
        if self.audit_detail.is_some()
            && matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q'))
        {
            self.audit_detail = None;
            return Ok(AppAction::None);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            self.screen = Screen::Main;
            return Ok(AppAction::None);
        }
        if key.code == KeyCode::Char('H') {
            self.audit_tab = AuditTab::History;
            return Ok(AppAction::None);
        }
        if self.audit_tab == AuditTab::History {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Tab => self.screen = Screen::Main,
                KeyCode::Char('D') => self.audit_tab = AuditTab::Dashboard,
                KeyCode::Char('R') | KeyCode::Char('a') => {
                    return Ok(self.request_audit_refresh());
                }
                KeyCode::Char('E') | KeyCode::Char('x') => return Ok(AppAction::ExportReport),
                KeyCode::Char('?') => {
                    self.help_return_screen = Screen::Audit;
                    self.help_scroll = 0;
                    self.screen = Screen::Help;
                }
                _ => {}
            }
            return Ok(AppAction::None);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Tab => self.screen = Screen::Main,
            KeyCode::Char('l') | KeyCode::Right => {
                if !self.pair_matrices.is_empty() {
                    self.audit_team_index = (self.audit_team_index + 1) % self.pair_matrices.len();
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                if !self.pair_matrices.is_empty() {
                    self.audit_team_index = if self.audit_team_index == 0 {
                        self.pair_matrices.len() - 1
                    } else {
                        self.audit_team_index - 1
                    };
                }
            }
            KeyCode::Char('R') | KeyCode::Char('a') => {
                return Ok(self.request_audit_refresh());
            }
            KeyCode::Char('t') => self.cycle_audit_pair_window()?,
            // M-2: `E` already exports to `report_dir` (default `~/tmp`);
            // `x` was in the requirements doc as a second export trigger and
            // was previously unbound. Aliasing it to the same action instead
            // of adding a second export path with a different hardcoded
            // filename avoids fragmenting "export" into two mechanisms.
            KeyCode::Char('E') | KeyCode::Char('x') => return Ok(AppAction::ExportReport),
            KeyCode::Char('j') | KeyCode::Down => {
                self.audit_selected =
                    (self.audit_selected + 1).min(self.audit_item_count().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.audit_selected = self.audit_selected.saturating_sub(1);
            }
            // M-2: jump to the top of the currently selected list.
            KeyCode::Char('g') => self.audit_selected = 0,
            KeyCode::Char('D') => self.show_reset_command(),
            KeyCode::Char('B') => self.open_bulk_reset_preview(),
            KeyCode::Char('W') => self.open_bulk_rename_wizard(),
            KeyCode::Char('M') => return self.mark_stale_action(),
            KeyCode::Enter => self.show_audit_detail(),
            KeyCode::Char('?') => {
                self.help_return_screen = Screen::Audit;
                self.help_scroll = 0;
                self.screen = Screen::Help;
            }
            _ => {}
        }
        Ok(AppAction::None)
    }

    fn handle_health_key(&mut self, key: KeyEvent) -> Result<AppAction> {
        match key.code {
            KeyCode::Char('q') => return Ok(AppAction::Quit),
            KeyCode::Esc | KeyCode::Char('H') => self.screen = Screen::Main,
            KeyCode::Char('j') | KeyCode::Down => {
                let team_count = self
                    .health_snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.teams.len())
                    .unwrap_or_default();
                self.health_team_index =
                    (self.health_team_index + 1).min(team_count.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.health_team_index = self.health_team_index.saturating_sub(1);
            }
            KeyCode::Char('t') => {
                self.health_window_days = if self.health_window_days == 7 { 30 } else { 7 };
            }
            KeyCode::Char('R') => return Ok(self.request_health_refresh()),
            KeyCode::Char('?') => {
                self.help_return_screen = Screen::Health;
                self.help_scroll = 0;
                self.screen = Screen::Help;
            }
            _ => {}
        }
        Ok(AppAction::None)
    }

    fn open_health(&mut self) -> AppAction {
        self.screen = Screen::Health;
        if self.health_snapshot.is_none() {
            self.request_health_refresh()
        } else {
            AppAction::None
        }
    }

    fn open_audit(&mut self) -> Result<AppAction> {
        self.screen = Screen::Audit;
        self.refresh_audit_insights()?;
        if self.audit_report.is_none() {
            Ok(self.request_audit_refresh())
        } else {
            Ok(AppAction::None)
        }
    }

    fn open_bulk_filter(&mut self) -> Result<()> {
        let messages = self.database.all_messages()?;
        self.bulk_filter = Some(BulkFilterState::new(messages, chrono::Utc::now()));
        self.bulk_modal = None;
        self.bulk_operation = None;
        self.bulk_cancel = None;
        self.screen = Screen::BulkFilter;
        self.status = StatusLine {
            text: "bulk filter loaded".to_owned(),
            is_error: false,
        };
        Ok(())
    }

    fn handle_bulk_filter_key(&mut self, key: KeyEvent) -> Result<AppAction> {
        if let Some(action) = self.handle_bulk_overlay_key(key)? {
            return Ok(action);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f') {
            self.screen = Screen::Main;
            return Ok(AppAction::None);
        }
        let Some(filter) = self.bulk_filter.as_mut() else {
            self.open_bulk_filter()?;
            return Ok(AppAction::None);
        };
        let mut recompute = false;
        match key.code {
            KeyCode::Esc => self.screen = Screen::Main,
            KeyCode::Char('q') if filter.focus == BulkFilterFocus::Results => {
                return Ok(AppAction::Quit);
            }
            KeyCode::Tab => filter.focus = filter.focus.next(false),
            KeyCode::BackTab => filter.focus = filter.focus.next(true),
            KeyCode::Char('j') | KeyCode::Down if filter.focus == BulkFilterFocus::Results => {
                filter.selected =
                    (filter.selected + 1).min(filter.results.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up if filter.focus == BulkFilterFocus::Results => {
                filter.selected = filter.selected.saturating_sub(1);
            }
            KeyCode::Char('g') if filter.focus == BulkFilterFocus::Results => {
                filter.selected = 0;
            }
            KeyCode::Char('G') if filter.focus == BulkFilterFocus::Results => {
                filter.selected = filter.results.len().saturating_sub(1);
            }
            KeyCode::Left if filter.focus == BulkFilterFocus::Period => {
                filter.period = filter.period.cycle(-1);
                recompute = true;
            }
            KeyCode::Right | KeyCode::Char(' ') if filter.focus == BulkFilterFocus::Period => {
                filter.period = filter.period.cycle(1);
                recompute = true;
            }
            KeyCode::Char('7') if filter.focus == BulkFilterFocus::Period => {
                filter.period = crate::bulk::FilterPeriod::SevenDays;
                recompute = true;
            }
            KeyCode::Char('3') if filter.focus == BulkFilterFocus::Period => {
                filter.period = crate::bulk::FilterPeriod::ThirtyDays;
                recompute = true;
            }
            KeyCode::Char('a') if filter.focus == BulkFilterFocus::Period => {
                filter.period = crate::bulk::FilterPeriod::All;
                recompute = true;
            }
            KeyCode::Backspace if filter.focus == BulkFilterFocus::Agent => {
                filter.agent.pop();
                recompute = true;
            }
            KeyCode::Backspace if filter.focus == BulkFilterFocus::Body => {
                filter.body.pop();
                recompute = true;
            }
            KeyCode::Char(character)
                if filter.focus == BulkFilterFocus::Agent
                    && plain_text_key(key)
                    && !matches!(character, 'M' | 'E' | '?') =>
            {
                filter.agent.push(character);
                recompute = true;
            }
            KeyCode::Char(character)
                if filter.focus == BulkFilterFocus::Body
                    && plain_text_key(key)
                    && !matches!(character, 'M' | 'E' | '?') =>
            {
                filter.body.push(character);
                recompute = true;
            }
            KeyCode::Char('M') => {
                let targets = mark_read_targets(filter.messages().cloned());
                if targets.is_empty() {
                    self.status = StatusLine {
                        text: "filter has no unread messages".to_owned(),
                        is_error: false,
                    };
                } else {
                    self.bulk_modal = Some(BulkModal::Preview {
                        kind: BulkKind::MarkRead,
                        targets,
                        confirm: String::new(),
                        scroll: 0,
                    });
                }
            }
            KeyCode::Char('E') => {
                if filter.results.is_empty() {
                    self.status = validation_status("filter has no results".to_owned());
                } else {
                    self.bulk_modal = Some(BulkModal::ExportFormat {
                        selected: ExportFormat::Markdown,
                    });
                }
            }
            KeyCode::Char('?') => {
                self.help_return_screen = Screen::BulkFilter;
                self.help_scroll = 0;
                self.screen = Screen::Help;
            }
            _ => {}
        }
        if recompute {
            filter.recompute(chrono::Utc::now());
        }
        Ok(AppAction::None)
    }

    fn handle_bulk_overlay_key(&mut self, key: KeyEvent) -> Result<Option<AppAction>> {
        if self.bulk_operation.is_some() {
            return self.handle_bulk_operation_key(key).map(Some);
        }
        if self.bulk_modal.is_some() {
            return self.handle_bulk_modal_key(key).map(Some);
        }
        Ok(None)
    }

    fn handle_bulk_modal_key(&mut self, key: KeyEvent) -> Result<AppAction> {
        let Some(mut modal) = self.bulk_modal.take() else {
            return Ok(AppAction::None);
        };
        let mut keep_open = true;
        let mut action = AppAction::None;
        match &mut modal {
            BulkModal::Preview {
                kind,
                targets,
                confirm,
                scroll,
            } => match key.code {
                KeyCode::Esc => keep_open = false,
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    *scroll = (*scroll + 1).min(targets.len().saturating_sub(1));
                }
                KeyCode::Backspace => {
                    confirm.pop();
                }
                KeyCode::Enter if confirm == "YES" => {
                    let kind = *kind;
                    let targets = targets.clone();
                    keep_open = false;
                    action = self.begin_bulk_operation(kind, targets)?;
                }
                KeyCode::Enter => {
                    self.status = validation_status("type YES exactly".to_owned());
                }
                KeyCode::Char(character) if plain_text_key(key) => confirm.push(character),
                _ => {}
            },
            BulkModal::ExportFormat { selected } => match key.code {
                KeyCode::Esc => keep_open = false,
                KeyCode::Left | KeyCode::Right => {
                    *selected = match selected {
                        ExportFormat::Markdown => ExportFormat::Json,
                        ExportFormat::Json => ExportFormat::Markdown,
                    };
                }
                KeyCode::Char('m') | KeyCode::Char('k') | KeyCode::Up => {
                    *selected = ExportFormat::Markdown;
                }
                KeyCode::Char('j') | KeyCode::Down => *selected = ExportFormat::Json,
                KeyCode::Enter => {
                    action = AppAction::ExportBulk(*selected);
                    keep_open = false;
                }
                _ => {}
            },
            BulkModal::RenameEdit {
                targets,
                selected,
                editing,
            } => match key.code {
                KeyCode::Esc => keep_open = false,
                KeyCode::Up => {
                    *selected = selected.saturating_sub(1);
                    *editing = false;
                }
                KeyCode::Down | KeyCode::Tab => {
                    *selected = (*selected + 1).min(targets.len().saturating_sub(1));
                    *editing = false;
                }
                KeyCode::BackTab => {
                    *selected = selected.saturating_sub(1);
                    *editing = false;
                }
                KeyCode::Backspace => {
                    *editing = true;
                    if let Some(target) = targets.get_mut(*selected) {
                        target.new.pop();
                    }
                }
                KeyCode::Enter => match self.validate_bulk_renames(targets) {
                    Ok(()) => {
                        self.bulk_modal = Some(BulkModal::RenameConfirm {
                            targets: targets.clone(),
                            confirm: String::new(),
                            scroll: 0,
                        });
                        keep_open = false;
                    }
                    Err(error) => self.status = validation_status(error),
                },
                KeyCode::Char(character) if plain_text_key(key) => {
                    if let Some(target) = targets.get_mut(*selected) {
                        if !*editing {
                            target.new.clear();
                            *editing = true;
                        }
                        target.new.push(character);
                    }
                }
                _ => {}
            },
            BulkModal::RenameConfirm {
                targets,
                confirm,
                scroll,
            } => match key.code {
                KeyCode::Esc => keep_open = false,
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    *scroll = (*scroll + 1).min(targets.len().saturating_sub(1));
                }
                KeyCode::Backspace => {
                    confirm.pop();
                }
                KeyCode::Enter if confirm == "YES" => {
                    let targets = targets
                        .iter()
                        .cloned()
                        .map(BulkTarget::Rename)
                        .collect();
                    keep_open = false;
                    action = self.begin_bulk_operation(BulkKind::Rename, targets)?;
                }
                KeyCode::Enter => self.status = validation_status("type YES exactly".to_owned()),
                KeyCode::Char(character) if plain_text_key(key) => confirm.push(character),
                _ => {}
            },
        }
        if keep_open {
            self.bulk_modal = Some(modal);
        }
        Ok(action)
    }

    fn handle_bulk_operation_key(&mut self, key: KeyEvent) -> Result<AppAction> {
        let Some(state) = self
            .bulk_operation
            .as_ref()
            .map(|operation| operation.state.clone())
        else {
            return Ok(AppAction::None);
        };
        match state {
            BulkRunState::Running => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    if let Some(operation) = self.bulk_operation.as_mut() {
                        operation.cancel_requested = true;
                    }
                    if let Some(cancel) = self.bulk_cancel.as_ref() {
                        let _ = cancel.send(true);
                    }
                    self.status = StatusLine {
                        text: "bulk abort requested; cancelling current command".to_owned(),
                        is_error: false,
                    };
                }
                Ok(AppAction::None)
            }
            BulkRunState::AwaitDecision { .. } => match key.code {
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    if let Some(operation) = self.bulk_operation.as_mut() {
                        operation.skip_failed_and_continue();
                    }
                    self.next_bulk_action_or_finish(false)
                }
                KeyCode::Char('a') | KeyCode::Char('A') | KeyCode::Esc => {
                    if let Some(operation) = self.bulk_operation.as_mut() {
                        operation.skip_failed_and_continue();
                    }
                    self.finish_bulk_operation(true)?;
                    Ok(AppAction::None)
                }
                _ => Ok(AppAction::None),
            },
            BulkRunState::AwaitForce { .. } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('f') => {
                    if let Some(operation) = self.bulk_operation.as_mut() {
                        operation.retry_force();
                    }
                    Ok(self.current_bulk_action().unwrap_or(AppAction::None))
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    if let Some(operation) = self.bulk_operation.as_mut() {
                        operation.skip_failed_and_continue();
                    }
                    self.finish_bulk_operation(true)?;
                    Ok(AppAction::None)
                }
                _ => Ok(AppAction::None),
            },
            BulkRunState::Complete { .. } => {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                        self.bulk_operation = None;
                        self.bulk_cancel = None;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if let Some(operation) = self.bulk_operation.as_mut() {
                            operation.results_cursor = (operation.results_cursor + 1)
                                .min(operation.results.len().saturating_sub(1));
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if let Some(operation) = self.bulk_operation.as_mut() {
                            operation.results_cursor = operation.results_cursor.saturating_sub(1);
                        }
                    }
                    KeyCode::Char('g') => {
                        if let Some(operation) = self.bulk_operation.as_mut() {
                            operation.results_cursor = 0;
                        }
                    }
                    KeyCode::Char('G') => {
                        if let Some(operation) = self.bulk_operation.as_mut() {
                            operation.results_cursor = operation.results.len().saturating_sub(1);
                        }
                    }
                    _ => {}
                }
                Ok(AppAction::None)
            }
        }
    }

    fn begin_bulk_operation(
        &mut self,
        kind: BulkKind,
        targets: Vec<BulkTarget>,
    ) -> Result<AppAction> {
        anyhow::ensure!(!targets.is_empty(), "bulk target is empty");
        if self.in_flight.is_some() {
            self.update_spinner_status();
            return Ok(AppAction::None);
        }
        let (cancel, _) = watch::channel(false);
        self.bulk_cancel = Some(cancel);
        self.bulk_operation = Some(BulkOperation::new(kind, targets));
        Ok(self.current_bulk_action().unwrap_or(AppAction::None))
    }

    fn current_bulk_action(&self) -> Option<AppAction> {
        let operation = self.bulk_operation.as_ref()?;
        Some(AppAction::RunBulk {
            target: operation.current_target()?.clone(),
            force_despawn: operation.force_despawn,
        })
    }

    pub fn bulk_cancel_receiver(&self) -> Option<watch::Receiver<bool>> {
        self.bulk_cancel.as_ref().map(watch::Sender::subscribe)
    }

    fn next_bulk_action_or_finish(&mut self, aborted: bool) -> Result<AppAction> {
        let has_next = self
            .bulk_operation
            .as_ref()
            .is_some_and(|operation| operation.current_target().is_some());
        if has_next && !aborted {
            return Ok(self.current_bulk_action().unwrap_or(AppAction::None));
        }
        self.finish_bulk_operation(aborted)?;
        Ok(AppAction::None)
    }

    pub fn complete_bulk_target(
        &mut self,
        target: BulkTarget,
        result: Result<CommandResult, String>,
    ) -> Result<AppAction> {
        let cancel_requested = self
            .bulk_operation
            .as_ref()
            .is_some_and(|operation| operation.cancel_requested);
        let succeeded = result.as_ref().is_ok_and(|result| result.success);
        if cancel_requested {
            if succeeded {
                let result = result.as_ref().expect("checked above");
                if let Some(operation) = self.bulk_operation.as_mut() {
                    operation.record_success(target, result);
                }
            } else if let Some(operation) = self.bulk_operation.as_mut() {
                operation.record_cancelled(target);
            }
            self.finish_bulk_operation(true)?;
            return Ok(AppAction::None);
        }
        if succeeded {
            let result = result.as_ref().expect("checked above");
            if let Some(operation) = self.bulk_operation.as_mut() {
                operation.record_success(target, result);
            }
        } else {
            let detail = match result {
                Ok(result) => command_failure_detail(&result),
                Err(error) => error,
            };
            if let Some(operation) = self.bulk_operation.as_mut() {
                operation.record_failure(target, detail.clone());
            }
            self.status = StatusLine {
                text: detail,
                is_error: true,
            };
            return Ok(AppAction::None);
        }

        self.next_bulk_action_or_finish(false)
    }

    fn finish_bulk_operation(&mut self, aborted: bool) -> Result<()> {
        let Some(operation) = self.bulk_operation.as_mut() else {
            return Ok(());
        };
        operation.finish(aborted);
        let kind = operation.kind;
        let completed = operation.completed_units;
        let failed = operation.failed_units;
        self.status = StatusLine {
            text: match kind {
                BulkKind::MarkRead => {
                    format!("marked {completed} msgs as read ({failed} failed)")
                }
                BulkKind::Reset => format!("reset {completed} identities ({failed} failed)"),
                BulkKind::Rename => format!("renamed {completed} identities ({failed} failed)"),
                BulkKind::Despawn => format!("despawned {completed} agents ({failed} failed)"),
            },
            is_error: failed > 0,
        };
        self.refresh_team_summaries()?;
        self.reload_selected_team()?;
        if matches!(kind, BulkKind::Reset | BulkKind::Rename | BulkKind::Despawn) {
            self.reload_agents()?;
            self.refresh_audit_analytics()?;
        }
        if kind == BulkKind::MarkRead {
            let messages = self.database.all_messages()?;
            if let Some(filter) = self.bulk_filter.as_mut() {
                filter.all_messages = messages;
                filter.recompute(chrono::Utc::now());
            }
            if self.audit_report.is_some() {
                self.refresh_audit_analytics()?;
            }
        }
        Ok(())
    }

    pub fn export_bulk_filter(&mut self, format: ExportFormat) -> Result<()> {
        let filter = self.bulk_filter.as_ref().context("bulk filter is not loaded")?;
        let messages = filter.messages().cloned().collect::<Vec<_>>();
        anyhow::ensure!(!messages.is_empty(), "filter has no results");
        let path = export_bulk_messages(
            &self.paths.report_dir,
            &messages,
            format,
            chrono::Utc::now(),
        )?;
        self.status = StatusLine {
            text: format!("exported: {}", path.display()),
            is_error: false,
        };
        Ok(())
    }

    fn open_bulk_reset_preview(&mut self) {
        if let Some(operation) = &self.bulk_operation
            && !matches!(operation.state, BulkRunState::Complete { .. })
        {
            self.status = validation_status("bulk operation already running".to_owned());
            return;
        }
        if self.agent_teams.is_empty() && let Err(error) = self.reload_agents() {
            self.set_error(&error);
            return;
        }
        let candidates = self
            .zombies
            .iter()
            .map(|row| (row.team.clone(), row.agent.clone()))
            .chain(
                self.stale_unreads
                    .iter()
                    .map(|row| (row.team.clone(), row.to_agent.clone())),
            )
            .collect::<HashSet<_>>();
        let mut seen = HashSet::new();
        let targets = self
            .agent_teams
            .iter()
            .flat_map(|team| &team.identities)
            .filter(|identity| candidates.contains(&(identity.team.clone(), identity.name.clone())))
            .filter(|identity| self.current_identity.as_deref() != Some(&identity.name))
            .filter(|identity| {
                seen.insert((
                    identity.team.clone(),
                    identity.name.clone(),
                    identity.agent_type.clone(),
                    identity.project.clone(),
                ))
            })
            .map(|identity| {
                BulkTarget::Reset(ResetTarget {
                    team: identity.team.clone(),
                    agent: identity.name.clone(),
                    agent_type: identity.agent_type.clone(),
                    project: identity.project.clone(),
                })
            })
            .collect::<Vec<_>>();
        if targets.is_empty() {
            self.status = validation_status("no resettable stale/zombie identities".to_owned());
            return;
        }
        self.bulk_operation = None;
        self.bulk_cancel = None;
        self.bulk_modal = Some(BulkModal::Preview {
            kind: BulkKind::Reset,
            targets,
            confirm: String::new(),
            scroll: 0,
        });
    }

    fn open_bulk_rename_wizard(&mut self) {
        if self.agent_teams.is_empty() && let Err(error) = self.reload_agents() {
            self.set_error(&error);
            return;
        }
        let targets = naming_violations(&self.agent_teams);
        if targets.is_empty() {
            self.status = StatusLine {
                text: "no naming violations".to_owned(),
                is_error: false,
            };
            return;
        }
        self.bulk_operation = None;
        self.bulk_cancel = None;
        self.bulk_modal = Some(BulkModal::RenameEdit {
            targets,
            selected: 0,
            editing: false,
        });
    }

    fn validate_bulk_renames(&self, targets: &[RenameTarget]) -> Result<(), String> {
        let mut proposed = HashSet::new();
        let replaced = targets
            .iter()
            .map(|target| (target.team.clone(), target.old.clone()))
            .collect::<HashSet<_>>();
        for target in targets {
            validate_agent_name(&target.new, &target.agent_type)?;
            if target.new == target.old {
                return Err(format!("{}: proposed name is unchanged", target.old));
            }
            if !proposed.insert((target.team.clone(), target.new.clone())) {
                return Err(format!("duplicate proposed name: {}/{}", target.team, target.new));
            }
            let collides = self.agent_teams.iter().any(|team| {
                team.name == target.team
                    && team.identities.iter().any(|identity| {
                        identity.name == target.new
                            && !replaced.contains(&(target.team.clone(), identity.name.clone()))
                    })
            });
            if collides {
                return Err(format!("name already exists: {}/{}", target.team, target.new));
            }
        }
        Ok(())
    }

    fn open_despawn_preview(&mut self) {
        if self.agent_focus != AgentFocus::Identities {
            self.status = validation_status(
                "D: switch to identity focus (Tab) and select a target".to_owned(),
            );
            return;
        }
        let Some(identity) = self.selected_agent_identity().cloned() else {
            self.status = validation_status("no identity selected".to_owned());
            return;
        };
        let from = self
            .current_identity
            .as_ref()
            .filter(|name| {
                name.as_str() != identity.name
                    && self.selected_agent_team().is_some_and(|team| {
                        team.identities
                            .iter()
                            .any(|row| row.name == name.as_str())
                    })
            })
            .cloned()
            .or_else(|| {
                self.selected_agent_team().and_then(|team| {
                    team.identities
                        .iter()
                        .find(|row| row.name != identity.name)
                        .map(|row| row.name.clone())
                })
            })
            .unwrap_or_else(|| identity.name.clone());
        self.bulk_operation = None;
        self.bulk_cancel = None;
        self.bulk_modal = Some(BulkModal::Preview {
            kind: BulkKind::Despawn,
            targets: vec![BulkTarget::Despawn(DespawnTarget {
                team: identity.team,
                from,
                name: identity.name,
            })],
            confirm: String::new(),
            scroll: 0,
        });
    }

    fn refresh_audit_analytics(&mut self) -> Result<()> {
        self.pair_matrices = self.database.pair_matrices(self.audit_pair_window_days)?;
        self.zombies = self
            .database
            .zombie_identities(&self.paths.teams_dir, ANALYTICS_WINDOW_DAYS)?;
        self.stale_unreads = self.database.stale_unreads(STALE_UNREAD_DAYS)?;
        self.audit_team_index = self
            .audit_team_index
            .min(self.pair_matrices.len().saturating_sub(1));
        self.audit_selected = self
            .audit_selected
            .min(self.audit_item_count().saturating_sub(1));
        Ok(())
    }

    fn refresh_audit_insights(&mut self) -> Result<()> {
        self.audit_history = read_audit_history(&self.paths.audit_history)?;
        self.body_size_distribution = self.database.body_size_distribution()?;
        Ok(())
    }

    fn cycle_audit_pair_window(&mut self) -> Result<()> {
        let selected_team = self.current_pair_matrix().map(|matrix| matrix.team.clone());
        self.audit_pair_window_days = match self.audit_pair_window_days {
            7 => 30,
            30 => 90,
            _ => 7,
        };
        self.pair_matrices = self.database.pair_matrices(self.audit_pair_window_days)?;
        self.audit_team_index = selected_team
            .as_ref()
            .and_then(|team| {
                self.pair_matrices
                    .iter()
                    .position(|matrix| &matrix.team == team)
            })
            .unwrap_or_else(|| {
                self.audit_team_index
                    .min(self.pair_matrices.len().saturating_sub(1))
            });
        self.status = StatusLine {
            text: format!("pair matrix window: {}d", self.audit_pair_window_days),
            is_error: false,
        };
        Ok(())
    }

    fn show_reset_command(&mut self) {
        let command = match self.selected_audit_item() {
            Some(AuditSelection::Zombie(zombie)) => {
                Database::agent_registration(&self.paths.teams_dir, &zombie.team, &zombie.agent)
                    .ok()
                    .flatten()
                    .map(|registration| {
                        reset_command_display(
                            &self.paths.scripts_dir,
                            &registration.project,
                            &registration.agent_type,
                            &zombie.agent,
                        )
                    })
            }
            _ => None,
        };
        self.status = StatusLine {
            text: command.unwrap_or_else(|| "D is available for zombie identities".to_owned()),
            is_error: false,
        };
    }

    fn mark_stale_action(&mut self) -> Result<AppAction> {
        if self.in_flight.is_some() {
            self.update_spinner_status();
            return Ok(AppAction::None);
        }
        match self.selected_audit_item() {
            Some(AuditSelection::Stale(stale)) => {
                let team = stale.team.clone();
                let recipient = stale.to_agent.clone();
                let unread_count = self
                    .database
                    .unread_count_for_recipient(&team, &recipient)?;
                Ok(AppAction::MarkRecipient {
                    team,
                    recipient,
                    unread_count,
                })
            }
            _ => {
                self.status = StatusLine {
                    text: "M is available for stale unread messages".to_owned(),
                    is_error: false,
                };
                Ok(AppAction::None)
            }
        }
    }

    fn show_audit_detail(&mut self) {
        self.audit_detail = match self.selected_audit_item() {
            Some(AuditSelection::Zombie(zombie)) => Some(format!(
                "Zombie identity\n\nteam: {}\nagent: {}\nreason: no traffic in {} days\n\nD shows its reset.sh command. B previews and batch-confirms every stale/zombie reset before execution.",
                zombie.team, zombie.agent, ANALYTICS_WINDOW_DAYS
            )),
            Some(AuditSelection::Stale(stale)) => Some(format!(
                "Stale unread #{}\n\nteam: {}\nfrom: {}\nto: {}\ncreated: {}\n\n{}\n\nM delegates read marking to inbox.sh. B includes its recipient in the reset preview when registered.",
                stale.id,
                stale.team,
                stale.from_agent,
                stale.to_agent,
                stale.created_at,
                stale.body
            )),
            None => None,
        };
    }

    fn focus_next(&self) -> Focus {
        match self.focus {
            Focus::Teams => Focus::Members,
            Focus::Members => Focus::Room,
            Focus::Room => Focus::Teams,
        }
    }

    fn focus_previous(&self) -> Focus {
        match self.focus {
            Focus::Teams => Focus::Room,
            Focus::Members => Focus::Teams,
            Focus::Room => Focus::Members,
        }
    }

    fn move_down(&mut self) {
        match self.focus {
            Focus::Teams => {
                self.selected_team =
                    (self.selected_team + 1).min(self.teams.len().saturating_sub(1));
            }
            Focus::Members => {
                self.selected_member =
                    (self.selected_member + 1).min(self.members.len().saturating_sub(1));
            }
            Focus::Room => {
                if let Some(next) = self.next_matching_message(self.selected_message, 1) {
                    self.selected_message = next;
                }
            }
        }
    }

    fn move_up(&mut self) -> Result<()> {
        match self.focus {
            Focus::Teams => self.selected_team = self.selected_team.saturating_sub(1),
            Focus::Members => self.selected_member = self.selected_member.saturating_sub(1),
            Focus::Room if self.selected_message == 0 && self.has_more_history => {
                self.load_older_history()?;
            }
            Focus::Room => {
                if let Some(previous) = self.next_matching_message(self.selected_message, -1) {
                    self.selected_message = previous;
                }
            }
        }
        Ok(())
    }

    /// Walks `self.messages` from `from` in `step` direction (+1/-1) to the
    /// next index that passes the active MEMBER and search filters.
    fn next_matching_message(&self, from: usize, step: isize) -> Option<usize> {
        let len = self.messages.len();
        if len == 0 {
            return None;
        }
        let mut index = from as isize;
        loop {
            index += step;
            if index < 0 || index as usize >= len {
                return None;
            }
            let candidate = index as usize;
            if self.message_matches_filters(&self.messages[candidate]) {
                return Some(candidate);
            }
        }
    }

    fn move_first(&mut self) {
        match self.focus {
            Focus::Teams => self.selected_team = 0,
            Focus::Members => self.selected_member = 0,
            Focus::Room => {
                if let Some(first) = self
                    .messages
                    .iter()
                    .position(|message| self.message_matches_filters(message))
                {
                    self.selected_message = first;
                }
            }
        }
    }

    fn move_last(&mut self) {
        match self.focus {
            Focus::Teams => self.selected_team = self.teams.len().saturating_sub(1),
            Focus::Members => self.selected_member = self.members.len().saturating_sub(1),
            Focus::Room => {
                if let Some(last) = self
                    .messages
                    .iter()
                    .rposition(|message| self.message_matches_filters(message))
                {
                    self.selected_message = last;
                }
            }
        }
    }

    fn move_half_page(&mut self, step: isize) -> Result<()> {
        if self.focus != Focus::Room {
            return Ok(());
        }
        let selected_id = self.selected_message().map(|message| message.id);
        if step < 0 {
            loop {
                let older_matches = self
                    .messages
                    .iter()
                    .filter(|message| self.message_matches_filters(message))
                    .take_while(|message| Some(message.id) != selected_id)
                    .count();
                if older_matches >= HALF_PAGE_MESSAGES || !self.has_more_history {
                    break;
                }
                let previous_len = self.messages.len();
                self.load_older_history()?;
                if let Some(selected_id) = selected_id
                    && let Some(index) = self
                        .messages
                        .iter()
                        .position(|message| message.id == selected_id)
                {
                    self.selected_message = index;
                }
                if self.messages.len() == previous_len {
                    break;
                }
            }
        }
        let visible: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| self.message_matches_filters(message).then_some(index))
            .collect();
        if visible.is_empty() {
            return Ok(());
        }
        let current = visible
            .iter()
            .position(|&index| index == self.selected_message)
            .unwrap_or_else(|| {
                visible
                    .partition_point(|&index| index < self.selected_message)
                    .saturating_sub(usize::from(step < 0))
                    .min(visible.len() - 1)
            });
        let target = if step < 0 {
            current.saturating_sub(HALF_PAGE_MESSAGES)
        } else {
            (current + HALF_PAGE_MESSAGES).min(visible.len() - 1)
        };
        self.selected_message = visible[target];
        Ok(())
    }

    fn move_to_next_unread(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let mut unread: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| {
                (message.read_at.is_none() && self.message_matches_filters(message))
                    .then_some(index)
            })
            .collect();
        unread.sort_unstable();
        if unread.is_empty() {
            self.status = StatusLine {
                text: "no unread messages".to_owned(),
                is_error: false,
            };
            return;
        }
        self.selected_message = unread
            .iter()
            .copied()
            .find(|&index| index > self.selected_message)
            .unwrap_or(unread[0]);
    }

    fn cycle_team(&mut self, step: isize) -> Result<()> {
        if self.teams.is_empty() {
            return Ok(());
        }
        self.selected_team = if step < 0 {
            self.selected_team
                .checked_sub(1)
                .unwrap_or(self.teams.len() - 1)
        } else {
            (self.selected_team + 1) % self.teams.len()
        };
        self.reload_selected_team()
    }

    fn activate_selection(&mut self) -> Result<()> {
        match self.focus {
            Focus::Teams => self.reload_selected_team()?,
            Focus::Room => self.toggle_message_fold(),
            Focus::Members => self.open_composer_for_member()?,
        }
        Ok(())
    }

    /// Shared by Enter (Room focus) and the dedicated `X` key — both mean
    /// "toggle this message's fold state", so they share one code path
    /// instead of two copies of the same threshold check.
    fn toggle_message_fold(&mut self) {
        let Some(message) = self.selected_message() else {
            self.status = validation_status("no message selected".to_owned());
            return;
        };
        if message.body.chars().count() <= FOLD_CHAR_THRESHOLD {
            self.status = StatusLine {
                text: "message too short to fold".to_owned(),
                is_error: false,
            };
            return;
        }
        let team = message.team.clone();
        let id = message.id;
        let expanded = self.expanded_messages.entry(team).or_default();
        if !expanded.remove(&id) {
            expanded.insert(id);
        }
    }

    /// `f`: fold or unfold every foldable message in the currently loaded
    /// team at once, instead of toggling them one at a time with `x`/`X`.
    /// Mirrors `toggle_message_fold`'s per-message logic but decides the
    /// target state up front from whether *any* foldable message in the
    /// team is still collapsed — so one `f` press always converges the
    /// whole list to a single state rather than alternating row-by-row.
    fn toggle_fold_all(&mut self) {
        let Some(team) = self.selected_team_name().map(str::to_owned) else {
            self.status = validation_status("no team selected".to_owned());
            return;
        };
        let foldable_ids: Vec<i64> = self
            .messages
            .iter()
            .filter(|message| message.body.chars().count() > FOLD_CHAR_THRESHOLD)
            .map(|message| message.id)
            .collect();
        if foldable_ids.is_empty() {
            self.status = StatusLine {
                text: "no foldable messages in this team".to_owned(),
                is_error: false,
            };
            return;
        }
        let expanded = self.expanded_messages.entry(team).or_default();
        let any_collapsed = foldable_ids.iter().any(|id| !expanded.contains(id));
        if any_collapsed {
            expanded.extend(foldable_ids.iter().copied());
            self.status = StatusLine {
                text: format!("unfolded {} messages", foldable_ids.len()),
                is_error: false,
            };
        } else {
            for id in &foldable_ids {
                expanded.remove(id);
            }
            self.status = StatusLine {
                text: format!("folded {} messages", foldable_ids.len()),
                is_error: false,
            };
        }
    }

    /// `s`: jump to the nearest other message (by index distance, forward
    /// preferred on ties) sent by the same `from_agent` as the currently
    /// selected message — a quick way to skim one sender's messages without
    /// scrolling past everyone else's.
    fn jump_to_sender(&mut self) {
        let Some(current) = self.selected_message() else {
            self.status = validation_status("no message selected".to_owned());
            return;
        };
        let sender = current.from_agent.clone();
        let current_index = self.selected_message;
        let nearest = self
            .messages
            .iter()
            .enumerate()
            .filter(|(index, message)| *index != current_index && message.from_agent == sender)
            .min_by_key(|(index, _)| {
                let distance = index.abs_diff(current_index);
                // Ties (equally near before/after) prefer forward, matching
                // n/N's forward-biased tie-break.
                (distance, *index < current_index)
            })
            .map(|(index, _)| index);
        match nearest {
            Some(index) => {
                self.selected_message = index;
                self.status = StatusLine {
                    text: format!("jumped to {sender}"),
                    is_error: false,
                };
            }
            None => {
                self.status = StatusLine {
                    text: format!("no other message from {sender}"),
                    is_error: false,
                };
            }
        }
    }

    /// Enter on the MEMBER column: open the composer pre-addressed to the
    /// selected member, defaulting `from` to `current_identity` when it's
    /// present in the roster (falls back to roster[0], same default `c`
    /// already uses for an unset `to`).
    fn open_composer_for_member(&mut self) -> Result<()> {
        let Some(team) = self.selected_team_name().map(str::to_owned) else {
            return Ok(());
        };
        if self.restore_draft(&team) {
            return Ok(());
        }
        let Some(member) = self.members.get(self.selected_member) else {
            return Ok(());
        };
        let member_name = member.name.clone();
        let roster = Database::load_roster(&self.paths.teams_dir, &team)?;
        if roster.is_empty() {
            self.status = StatusLine {
                text: "selected team has no roster".to_owned(),
                is_error: true,
            };
            return Ok(());
        }
        let to_index = roster
            .iter()
            .position(|name| name == &member_name)
            .unwrap_or(0);
        let from_index = self
            .current_identity
            .as_deref()
            .and_then(|identity| roster.iter().position(|name| name == identity))
            .unwrap_or(0);
        self.composer = Some(ComposerState {
            roster,
            from_index,
            to_index,
            body: String::new(),
            cursor: 0,
        });
        self.screen = Screen::Composer;
        Ok(())
    }

    /// `F` on the MEMBER column: filter the room to messages where the
    /// selected member is sender or recipient. Re-pressing on the same
    /// member clears it — a toggle, not a stack.
    fn toggle_member_filter(&mut self) {
        let Some(member) = self.members.get(self.selected_member) else {
            return;
        };
        let name = member.name.clone();
        if self.member_filter.as_deref() == Some(name.as_str()) {
            self.member_filter = None;
            self.status = StatusLine {
                text: "filter cleared".to_owned(),
                is_error: false,
            };
        } else {
            self.status = StatusLine {
                text: format!("filter: {name}"),
                is_error: false,
            };
            self.member_filter = Some(name);
        }
    }

    /// `M` on the MEMBER column: mark every unread message addressed to the
    /// selected member as read, delegated to inbox.sh like every other
    /// mark-read path in this app (memory: agmsg write-path discipline).
    fn mark_member_action(&mut self) -> Result<AppAction> {
        if self.in_flight.is_some() {
            self.update_spinner_status();
            return Ok(AppAction::None);
        }
        let Some(team) = self.selected_team_name().map(str::to_owned) else {
            self.status = validation_status("no team selected".to_owned());
            return Ok(AppAction::None);
        };
        let Some(member) = self.members.get(self.selected_member) else {
            self.status = validation_status("no member selected".to_owned());
            return Ok(AppAction::None);
        };
        let name = member.name.clone();
        if member.unread_count == 0 {
            self.status = StatusLine {
                text: format!("{name} has no unread"),
                is_error: false,
            };
            return Ok(AppAction::None);
        }
        Ok(AppAction::MarkRecipient {
            team,
            recipient: name,
            unread_count: member.unread_count,
        })
    }

    /// `I` on the MEMBER column: build (or close, if already open) the info
    /// popup. Registration details come from `config.json`; traffic/unread
    /// counts come straight from the DB — same sources `member_summaries`
    /// and `agent_registration` already expose, just assembled for display.
    fn toggle_member_info(&mut self) -> Result<()> {
        if self.member_info.is_some() {
            self.member_info = None;
            return Ok(());
        }
        let Some(team) = self.selected_team_name().map(str::to_owned) else {
            return Ok(());
        };
        let Some(member) = self.members.get(self.selected_member).cloned() else {
            return Ok(());
        };
        let registration = Database::agent_registration(&self.paths.teams_dir, &team, &member.name)?;
        let (sent, received) = self
            .database
            .agent_traffic_counts(&team, &member.name, ANALYTICS_WINDOW_DAYS)?;
        let (agent_type, project) = registration
            .map(|registration| (registration.agent_type, registration.project))
            .unwrap_or_else(|| ("-".to_owned(), "-".to_owned()));
        self.member_info = Some(format!(
            "Agent: {}\nCli-type: {}\nProject: {}\nLast seen: {}\nSent (30d): {}\nReceived (30d): {}\nUnread (all-time): {}",
            member.name,
            agent_type,
            project,
            member.last_message_at.as_deref().unwrap_or("-"),
            sent,
            received,
            member.unread_count,
        ));
        Ok(())
    }

    /// L-3: `Enter` on an Identities-focus Agents-screen row. Same
    /// open/close toggle shape as `toggle_member_info`, but the summary is
    /// already fully populated on `AgentIdentitySummary` (it backs the
    /// IDENTITIES list itself), so this needs no extra DB round-trip.
    fn toggle_agent_identity_info(&mut self) -> Result<()> {
        if self.agent_identity_info.is_some() {
            self.agent_identity_info = None;
            return Ok(());
        }
        let Some(identity) = self.selected_agent_identity().cloned() else {
            self.status = validation_status("no identity selected".to_owned());
            return Ok(());
        };
        let run_dir = self
            .paths
            .scripts_dir
            .parent()
            .map(|path| path.join("run"))
            .unwrap_or_else(|| self.paths.scripts_dir.join("../run"));
        let lock = match actas_lock_status(&run_dir, &identity.team, &identity.name) {
            LockStatus::Unlocked => "unlocked".to_owned(),
            LockStatus::Owned(owner) => owner,
            LockStatus::ParseError => "lock parse error".to_owned(),
        };
        self.agent_identity_info = Some(format!(
            "Agent: {}\nTeam: {}\nCli-type: {}\nProject: {}\nActas lock: {}\nLast seen: {}\nSent (30d): {}\nReceived (30d): {}",
            identity.name,
            identity.team,
            identity.agent_type,
            identity.project,
            lock,
            identity.last_seen_at.as_deref().unwrap_or("-"),
            identity.sent_30d,
            identity.received_30d,
        ));
        Ok(())
    }

    fn open_composer(&mut self) -> Result<()> {
        let Some(team) = self.selected_team_name().map(str::to_owned) else {
            return Ok(());
        };
        if self.restore_draft(&team) {
            return Ok(());
        }
        let roster = Database::load_roster(&self.paths.teams_dir, &team)?;
        if roster.is_empty() {
            self.status = StatusLine {
                text: "selected team has no roster".to_owned(),
                is_error: true,
            };
            return Ok(());
        }
        let to_index = self
            .members
            .get(self.selected_member)
            .and_then(|member| roster.iter().position(|name| name == &member.name))
            .unwrap_or_else(|| usize::from(roster.len() > 1));
        // M-3: this used to hardcode `from_index: 0`, so `c` silently sent as
        // roster[0] even when AGMSG_IDENTITY named a different registered
        // agent — a from-spoofing footgun that open_composer_for_member
        // (the MEMBER-pane Enter path) never had. Same resolution both ways
        // now; when it can't resolve to the current identity, say so instead
        // of leaving the mismatch invisible.
        let from_index = self
            .current_identity
            .as_deref()
            .and_then(|identity| roster.iter().position(|name| name == identity));
        if from_index.is_none() {
            self.status = StatusLine {
                text: format!(
                    "sending as {} (AGMSG_IDENTITY not set or not in roster)",
                    roster[0]
                ),
                is_error: false,
            };
        }
        self.composer = Some(ComposerState {
            roster,
            from_index: from_index.unwrap_or(0),
            to_index,
            body: String::new(),
            cursor: 0,
        });
        self.screen = Screen::Composer;
        Ok(())
    }

    fn restore_draft(&mut self, team: &str) -> bool {
        let Some(mut draft) = self.drafts.remove(team) else {
            return false;
        };
        let from = draft.from_agent().map(str::to_owned);
        let to = draft.to_agent().map(str::to_owned);
        if let Ok(roster) = Database::load_roster(&self.paths.teams_dir, team)
            && !roster.is_empty()
        {
            draft.from_index = from
                .as_ref()
                .and_then(|name| roster.iter().position(|entry| entry == name))
                .unwrap_or_default();
            draft.to_index = to
                .as_ref()
                .and_then(|name| roster.iter().position(|entry| entry == name))
                .unwrap_or_else(|| usize::from(roster.len() > 1));
            draft.roster = roster;
        }
        self.composer = Some(draft);
        self.screen = Screen::Composer;
        self.status = StatusLine {
            text: "draft restored (Ctrl-K to clear)".to_owned(),
            is_error: false,
        };
        true
    }

    fn mark_selected_action(&mut self) -> Result<AppAction> {
        if self.in_flight.is_some() {
            self.update_spinner_status();
            return Ok(AppAction::None);
        }
        let Some(message) = self.selected_message() else {
            self.status = validation_status("no message selected".to_owned());
            return Ok(AppAction::None);
        };
        if message.read_at.is_some() {
            self.status = StatusLine {
                text: "message is already read".to_owned(),
                is_error: false,
            };
            return Ok(AppAction::None);
        }
        let team = message.team.clone();
        let recipient = message.to_agent.clone();
        let unread_count = self
            .database
            .unread_count_for_recipient(&team, &recipient)?;
        Ok(AppAction::MarkRecipient {
            team,
            recipient,
            unread_count,
        })
    }

    fn mark_team_action(&mut self) -> Result<AppAction> {
        if self.in_flight.is_some() {
            self.update_spinner_status();
            return Ok(AppAction::None);
        }
        let Some(team) = self.selected_team_name().map(str::to_owned) else {
            self.status = validation_status("no team selected".to_owned());
            return Ok(AppAction::None);
        };
        let unread_count = self
            .teams
            .get(self.selected_team)
            .map(|summary| summary.unread_count)
            .unwrap_or_default();
        Ok(AppAction::MarkTeam {
            recipients: self.database.unread_recipients(&team)?,
            team,
            unread_count,
        })
    }

    fn load_older_history(&mut self) -> Result<()> {
        let Some(team) = self.selected_team_name().map(str::to_owned) else {
            return Ok(());
        };
        let before_id = self.messages.first().map(|message| message.id);
        let page = self.database.history(&team, before_id, HISTORY_PAGE_SIZE)?;
        let inserted = page.messages.len();
        if inserted > 0 {
            let mut messages = page.messages;
            messages.append(&mut self.messages);
            self.messages = messages;
            self.selected_message = inserted.saturating_sub(1);
        }
        self.has_more_history = page.has_more;
        Ok(())
    }

    fn refresh_team_summaries(&mut self) -> Result<()> {
        let selected = self
            .teams
            .get(self.selected_team)
            .map(|team| team.name.clone());
        self.teams = self.database.team_summaries(&self.paths.teams_dir)?;
        if let Some(selected) = selected
            && let Some(index) = self.teams.iter().position(|team| team.name == selected)
        {
            self.selected_team = index;
        }
        self.selected_team = self.selected_team.min(self.teams.len().saturating_sub(1));
        Ok(())
    }

    fn refresh_members(&mut self) -> Result<()> {
        let Some(team) = self.selected_team_name().map(str::to_owned) else {
            self.members.clear();
            self.selected_member = 0;
            return Ok(());
        };
        self.members = self
            .database
            .member_summaries(&self.paths.teams_dir, &team)?;
        self.selected_member = self
            .selected_member
            .min(self.members.len().saturating_sub(1));
        Ok(())
    }

    fn reload_selected_team(&mut self) -> Result<()> {
        self.active_team = self
            .teams
            .get(self.selected_team)
            .map(|team| team.name.clone());
        self.refresh_members()?;
        let Some(team) = self.selected_team_name().map(str::to_owned) else {
            self.messages.clear();
            self.selected_message = 0;
            self.has_more_history = false;
            return Ok(());
        };
        let page = self.database.history(&team, None, HISTORY_PAGE_SIZE)?;
        self.messages = page.messages;
        self.has_more_history = page.has_more;
        self.selected_message = self.messages.len().saturating_sub(1);
        Ok(())
    }

    fn begin_search(&mut self) {
        self.input_mode = InputMode::Search;
        self.search_query.get_or_insert_with(String::new);
        self.update_search_status();
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Result<AppAction> {
        match key.code {
            KeyCode::Esc => {
                self.clear_search();
                return Ok(AppAction::None);
            }
            KeyCode::Enter => {
                self.input_mode = InputMode::Normal;
                if self.search_query.as_deref().is_none_or(str::is_empty) {
                    self.clear_search();
                    return Ok(AppAction::None);
                }
                self.load_database_search_matches()?;
                if let Some(first) = self
                    .messages
                    .iter()
                    .position(|message| self.message_matches_filters(message))
                {
                    self.selected_message = first;
                    self.update_match_status();
                } else {
                    self.update_search_status();
                }
                return Ok(AppAction::None);
            }
            KeyCode::Backspace => {
                if let Some(query) = self.search_query.as_mut() {
                    query.pop();
                }
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.search_query
                    .get_or_insert_with(String::new)
                    .push(character);
            }
            _ => return Ok(AppAction::None),
        }
        self.update_search_status();
        Ok(AppAction::None)
    }

    fn load_database_search_matches(&mut self) -> Result<()> {
        let Some(team) = self.selected_team_name().map(str::to_owned) else {
            return Ok(());
        };
        let Some(query) = self.search_query.as_deref() else {
            return Ok(());
        };
        let target_id = self
            .database
            .first_search_match_id(&team, query, self.member_filter.as_deref())?;
        let Some(target_id) = target_id else {
            return Ok(());
        };
        while self.has_more_history
            && !self.messages.iter().any(|message| message.id == target_id)
        {
            let previous_len = self.messages.len();
            self.load_older_history()?;
            if self.messages.len() == previous_len {
                break;
            }
        }
        Ok(())
    }

    fn clear_search(&mut self) {
        self.input_mode = InputMode::Normal;
        self.search_query = None;
        self.status = StatusLine {
            text: "search cleared".to_owned(),
            is_error: false,
        };
    }

    fn update_search_status(&mut self) {
        let query = self.search_query.as_deref().unwrap_or_default();
        let has_match = query.is_empty()
            || self
                .messages
                .iter()
                .any(|message| self.message_matches_filters(message));
        self.status = StatusLine {
            text: if has_match {
                format!("search: {query}_")
            } else {
                format!("no match: {query}")
            },
            is_error: !has_match,
        };
    }

    fn cycle_search_match(&mut self, step: isize) {
        let Some(query) = self.search_query.as_deref() else {
            self.status = StatusLine {
                text: "no active search (press /)".to_owned(),
                is_error: false,
            };
            return;
        };
        if query.is_empty() {
            self.status = StatusLine {
                text: "no active search (press /)".to_owned(),
                is_error: false,
            };
            return;
        }
        let matches: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| self.message_matches_filters(message).then_some(index))
            .collect();
        if matches.is_empty() {
            self.update_search_status();
            return;
        }
        let next_position = match matches
            .iter()
            .position(|&index| index == self.selected_message)
        {
            Some(position) if step > 0 => (position + 1) % matches.len(),
            Some(position) => position.checked_sub(1).unwrap_or(matches.len() - 1),
            None if step > 0 => 0,
            None => matches.len() - 1,
        };
        self.selected_message = matches[next_position];
        self.update_match_status();
    }

    fn update_match_status(&mut self) {
        let Some(query) = self.search_query.as_deref() else {
            return;
        };
        let matches: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter_map(|(index, message)| self.message_matches_filters(message).then_some(index))
            .collect();
        let position = matches
            .iter()
            .position(|&index| index == self.selected_message)
            .unwrap_or_default();
        self.status = StatusLine {
            text: format!("match {}/{}: {query}", position + 1, matches.len()),
            is_error: false,
        };
    }
}

fn next_index(current: usize, length: usize) -> usize {
    if length == 0 {
        0
    } else {
        (current + 1) % length
    }
}

fn move_bounded(current: usize, length: usize, step: isize) -> usize {
    if length == 0 {
        return 0;
    }
    if step < 0 {
        current.saturating_sub(step.unsigned_abs()).min(length - 1)
    } else {
        current.saturating_add(step as usize).min(length - 1)
    }
}

fn plain_text_key(key: KeyEvent) -> bool {
    !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

fn validation_status(message: String) -> StatusLine {
    StatusLine {
        text: message,
        is_error: true,
    }
}

fn command_failure_detail(result: &CommandResult) -> String {
    if !result.stderr.is_empty() {
        result.stderr.clone()
    } else if !result.stdout.is_empty() {
        result.stdout.clone()
    } else {
        format!("agent script failed (exit {:?})", result.exit_code)
    }
}

/// First `FOLD_PREVIEW_LINES` lines of a folded body, plus how many
/// characters were left out. Splits on raw `\n` (not `highlight_body`'s
/// output) so the char count in the trim note matches what `X` actually
/// reveals, independent of how syntect/ratatui end up wrapping it.
pub fn fold_preview(body: &str) -> (String, usize) {
    let preview: String = body
        .split('\n')
        .take(FOLD_PREVIEW_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed_chars = body.chars().count().saturating_sub(preview.chars().count());
    (preview, trimmed_chars)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::{
        App, AppAction, BodySizeLevel, ComposerState, FOLD_CHAR_THRESHOLD, Focus,
        InFlightOperation, InputMode, Screen, StatusLine,
    };
    use crate::agents::{AgentFocus, AgentModal, AgentOperation};
    use crate::bulk::{
        BulkKind, BulkRunState, BulkTarget, DespawnTarget, FilterPeriod, MarkReadTarget,
    };
    use crate::config::Paths;
    use crate::db::Message;
    use crate::exec::CommandResult;
    use crate::notify::PendingNotification;

    fn fixture_app_with_messages(rows: &[(&str, &str, &str)]) -> (TempDir, App) {
        let temp = tempfile::tempdir().expect("tempdir");
        let db = temp.path().join("messages.db");
        let connection = Connection::open(&db).expect("db");
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
                );",
            )
            .expect("schema");
        for (index, (from, to, body)) in rows.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO messages (team, from_agent, to_agent, body, created_at)
                     VALUES ('ops', ?1, ?2, ?3, ?4)",
                    rusqlite::params![from, to, body, format!("2026-01-01T00:{index:02}:00Z")],
                )
                .expect("insert message");
        }
        drop(connection);
        let teams_dir = temp.path().join("teams");
        fs::create_dir_all(teams_dir.join("ops")).expect("team dir");
        fs::write(
            teams_dir.join("ops/config.json"),
            r#"{"agents":{"claude-main":{"registrations":[{"type":"claude-code","project":"/tmp/ops"}]},"codex-worker":{"registrations":[{"type":"codex","project":"/tmp/ops"}]},"cursor-1":{"registrations":[{"type":"cursor","project":"/tmp/ops"}]}}}"#,
        )
        .expect("config");
        let app = App::load(Paths {
            db,
            teams_dir,
            scripts_dir: temp.path().join("scripts"),
            audit_script: temp.path().join("agmsg-audit"),
            audit_history: temp.path().join("audit.jsonl"),
            report_dir: temp.path().join("reports"),
            state_file: temp.path().join("state.json"),
        })
        .expect("app");
        (temp, app)
    }

    fn fixture_app() -> (TempDir, App) {
        let temp = tempfile::tempdir().expect("tempdir");
        let db = temp.path().join("messages.db");
        Connection::open(&db)
            .expect("db")
            .execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    team TEXT NOT NULL,
                    from_agent TEXT NOT NULL,
                    to_agent TEXT NOT NULL,
                    body TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    read_at TEXT
                );",
            )
            .expect("schema");
        let teams_dir = temp.path().join("teams");
        fs::create_dir_all(teams_dir.join("ops")).expect("team dir");
        fs::write(
            teams_dir.join("ops/config.json"),
            r#"{"agents":{"claude-main":{"registrations":[{"type":"claude-code","project":"/tmp/ops"}]},"codex-worker":{"registrations":[{"type":"codex","project":"/tmp/ops"}]}}}"#,
        )
        .expect("config");
        let app = App::load(Paths {
            db,
            teams_dir,
            scripts_dir: temp.path().join("scripts"),
            audit_script: temp.path().join("agmsg-audit"),
            audit_history: temp.path().join("audit.jsonl"),
            report_dir: temp.path().join("reports"),
            state_file: temp.path().join("state.json"),
        })
        .expect("app");
        (temp, app)
    }

    #[test]
    fn composer_preflight_uses_utf8_bytes_and_boundary_levels() {
        let mut composer = ComposerState {
            roster: Vec::new(),
            from_index: 0,
            to_index: 0,
            body: "a".repeat(2_048),
            cursor: 0,
        };
        assert_eq!(composer.body_size_level(), BodySizeLevel::Normal);
        composer.body.push('あ');
        assert_eq!(composer.body_bytes(), 2_051);
        assert_eq!(composer.body_size_level(), BodySizeLevel::Warning);
        composer.body = "a".repeat(4_097);
        assert_eq!(composer.body_size_level(), BodySizeLevel::Blocked);
    }

    #[test]
    fn composer_blocks_send_above_hard_limit() {
        let (_temp, mut app) = fixture_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
            .expect("compose");
        app.composer.as_mut().expect("composer").body = "a".repeat(4_097);
        let action = app
            .handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .expect("send");
        assert!(matches!(action, AppAction::None));
        assert_eq!(app.screen, Screen::Composer);
        assert!(app.status.is_error);
    }

    #[test]
    fn audit_navigation_requests_once_and_tab_returns_main() {
        let (_temp, mut app) = fixture_app();
        let action = app
            .handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
            .expect("audit");
        assert!(matches!(action, AppAction::RefreshAudit));
        assert_eq!(app.screen, Screen::Audit);
        assert!(app.audit_loading);
        let duplicate = app
            .handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
            .expect("duplicate refresh");
        assert!(matches!(duplicate, AppAction::None));
        app.complete_audit_error("failed".to_owned());
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .expect("back");
        assert_eq!(app.screen, Screen::Main);
    }

    #[test]
    fn member_filter_toggle_restricts_room_navigation_to_matching_messages() {
        let (_temp, mut app) = fixture_app_with_messages(&[
            ("claude-main", "codex-worker", "hello"),
            ("cursor-1", "codex-worker", "unrelated"),
            ("codex-worker", "claude-main", "reply"),
        ]);
        app.focus = Focus::Members;
        app.selected_member = app
            .members
            .iter()
            .position(|member| member.name == "cursor-1")
            .expect("cursor-1 present in roster");
        app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::NONE))
            .expect("toggle filter on");
        assert_eq!(app.member_filter.as_deref(), Some("cursor-1"));

        app.focus = Focus::Room;
        app.selected_message = 0;
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("move down under filter");
        let selected = app.selected_message().expect("a message stays selected");
        assert!(
            selected.from_agent == "cursor-1" || selected.to_agent == "cursor-1",
            "j should skip messages that don't involve the filtered member"
        );

        app.focus = Focus::Members;
        app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::NONE))
            .expect("toggle filter off");
        assert!(app.member_filter.is_none(), "re-pressing F clears the filter");
    }

    #[test]
    fn body_fold_state_toggles_via_x_key_and_tracks_by_message_id() {
        let long_body = format!("{}{}", "line\n".repeat(30), "x".repeat(600));
        let (_temp, mut app) =
            fixture_app_with_messages(&[("claude-main", "codex-worker", &long_body)]);
        let message_id = app.selected_message().expect("seed message").id;
        assert!(app.body_is_folded(app.selected_message().expect("seed message")));

        let (preview, trimmed) = super::fold_preview(&long_body);
        assert!(trimmed > 0, "a >500-char body must report trimmed chars");
        assert!(preview.split('\n').count() <= super::FOLD_PREVIEW_LINES);

        app.focus = Focus::Room;
        app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE))
            .expect("expand via X");
        assert!(
            app.expanded_messages
                .get("ops")
                .is_some_and(|ids| ids.contains(&message_id))
        );
        assert!(!app.body_is_folded(app.selected_message().expect("seed message")));

        app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE))
            .expect("collapse via X");
        assert!(
            !app.expanded_messages
                .get("ops")
                .is_some_and(|ids| ids.contains(&message_id))
        );
    }

    #[test]
    fn incremental_search_is_case_insensitive_and_n_cycles_across_fields() {
        let (_temp, mut app) = fixture_app_with_messages(&[
            ("claude-main", "codex-worker", "ordinary"),
            ("REViewer", "codex-worker", "from hit"),
            ("cursor-1", "review-bot", "to hit"),
            ("cursor-1", "codex-worker", "body has RevISION"),
        ]);
        app.focus = Focus::Room;
        for key in [
            KeyCode::Char('/'),
            KeyCode::Char('r'),
            KeyCode::Char('e'),
            KeyCode::Char('v'),
            KeyCode::Enter,
        ] {
            app.handle_key(KeyEvent::new(key, KeyModifiers::NONE))
                .expect("search input");
        }

        assert_eq!(app.input_mode, InputMode::Normal);
        assert_eq!(app.search_query.as_deref(), Some("rev"));
        assert_eq!(app.selected_message, 1, "Enter jumps to the first hit");
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            .expect("next match");
        assert_eq!(app.selected_message, 2, "n advances to the to-agent hit");
        app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE))
            .expect("previous match");
        assert_eq!(app.selected_message, 1, "N returns to the previous hit");

        app.member_filter = Some("codex-worker".to_owned());
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            .expect("next match with member filter");
        assert_eq!(
            app.selected_message, 3,
            "search and member filters must compose with AND"
        );
    }

    #[test]
    fn escape_clears_search_and_restores_unfiltered_navigation() {
        let (_temp, mut app) = fixture_app_with_messages(&[
            ("claude-main", "codex-worker", "first"),
            ("cursor-1", "codex-worker", "ordinary"),
            ("claude-main", "codex-worker", "needle"),
        ]);
        app.focus = Focus::Room;
        for key in [
            KeyCode::Char('/'),
            KeyCode::Char('n'),
            KeyCode::Char('e'),
            KeyCode::Char('e'),
            KeyCode::Enter,
        ] {
            app.handle_key(KeyEvent::new(key, KeyModifiers::NONE))
                .expect("search input");
        }
        assert_eq!(app.selected_message, 2);

        let action = app
            .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("clear search");
        assert!(matches!(action, AppAction::None));
        assert!(app.search_query.is_none());
        assert_eq!(app.screen, Screen::Main);
        app.selected_message = 0;
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("unfiltered move");
        assert_eq!(app.selected_message, 1);
    }

    #[test]
    fn main_escape_does_not_quit_and_composer_draft_is_restored() {
        let (_temp, mut app) = fixture_app();
        let action = app
            .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("guarded escape");
        assert!(matches!(action, AppAction::None));
        assert_eq!(app.screen, Screen::Main);

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
            .expect("open composer");
        for character in "draft body".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("type draft");
        }
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("save draft");
        assert_eq!(app.screen, Screen::Main);
        assert!(app.composer.is_none());

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
            .expect("restore draft");
        assert_eq!(app.screen, Screen::Composer);
        assert_eq!(app.composer.as_ref().expect("composer").body, "draft body");
        assert_eq!(app.status.text, "draft restored (Ctrl-K to clear)");
    }

    #[test]
    fn yank_source_is_full_body_even_when_message_is_folded() {
        let long_body = format!("{}{}", "line\n".repeat(30), "tail".repeat(150));
        let (_temp, mut app) =
            fixture_app_with_messages(&[("claude-main", "codex-worker", &long_body)]);
        app.focus = Focus::Room;
        assert!(app.body_is_folded(app.selected_message().expect("seed message")));
        assert_eq!(app.selected_message_body(), Some(long_body.as_str()));

        let action = app
            .handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("yank action");
        match action {
            AppAction::Yank(body) => assert_eq!(body, long_body),
            _ => panic!("Room y must request a full-body yank"),
        }
    }

    #[test]
    fn agents_toggle_is_lazy_and_preserves_main_selection_state() {
        let (_temp, mut app) = fixture_app();
        app.focus = Focus::Members;
        app.selected_member = 1;
        assert!(app.agent_teams.is_empty(), "inventory must be lazy");

        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        assert_eq!(app.screen, Screen::Agents);
        assert_eq!(app.agent_teams.len(), 1);
        assert_eq!(app.agent_teams[0].identities.len(), 2);

        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE))
            .expect("agents help");
        assert_eq!(app.screen, Screen::Help);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close agents help");
        assert_eq!(app.screen, Screen::Agents);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close agents");
        assert_eq!(app.screen, Screen::Main);
        assert_eq!(app.focus, Focus::Members);
        assert_eq!(app.selected_member, 1);
    }

    #[test]
    fn members_pane_n_opens_spawn_wizard_with_team_default() {
        let (_temp, mut app) = fixture_app();
        app.focus = Focus::Members;
        assert!(app.agent_teams.is_empty(), "inventory must be lazy");

        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            .expect("open spawn wizard from members pane");

        assert_eq!(app.screen, Screen::Agents);
        match &app.agent_modal {
            Some(AgentModal::Spawn(state)) => {
                assert_eq!(state.team_index, app.agent_team_index);
                assert_eq!(app.agent_teams[state.team_index].name, "ops");
            }
            other => panic!("expected spawn modal, got {other:?}"),
        }
    }

    #[test]
    fn members_pane_r_opens_rename_modal_for_selected_member() {
        let (_temp, mut app) = fixture_app();
        app.focus = Focus::Members;
        app.selected_member = 0;
        let expected_name = app.members[0].name.clone();

        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
            .expect("open rename modal from members pane");

        assert_eq!(app.screen, Screen::Agents);
        match &app.agent_modal {
            Some(AgentModal::Rename {
                target,
                input,
                confirming,
                ..
            }) => {
                assert_eq!(target.name, expected_name);
                assert!(input.is_empty());
                assert!(!confirming);
            }
            other => panic!("expected rename modal, got {other:?}"),
        }
    }

    #[test]
    fn rename_requires_enter_then_y_and_marks_self_for_bridge_restart() {
        let (_temp, mut app) = fixture_app();
        app.current_identity = Some("claude-main".to_owned());
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
            .expect("open rename");
        assert!(matches!(
            app.agent_modal,
            Some(AgentModal::Rename {
                self_rename: true,
                ..
            })
        ));

        let premature = app
            .handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("premature y");
        assert!(matches!(premature, AppAction::None));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("cancel rename");
        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
            .expect("reopen rename");
        for character in "claude-code-lead".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("type rename");
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm stage");
        let action = app
            .handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("confirm rename");
        assert!(matches!(
            action,
            AppAction::ManageAgent(AgentOperation::Rename {
                self_rename: true,
                ..
            })
        ));
    }

    #[test]
    fn reset_refuses_self_and_requires_exact_uppercase_yes() {
        let (_temp, mut app) = fixture_app();
        app.current_identity = Some("claude-main".to_owned());
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE))
            .expect("self reset");
        assert!(matches!(
            app.agent_modal,
            Some(AgentModal::Reset { blocked: true, .. })
        ));
        let blocked = app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("blocked enter");
        assert!(matches!(blocked, AppAction::None));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close blocked modal");
        app.current_identity = Some("other-agent".to_owned());
        app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE))
            .expect("open reset");
        for character in "yes".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("type lowercase");
        }
        let rejected = app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("reject lowercase");
        assert!(matches!(rejected, AppAction::None));
        for _ in 0..3 {
            app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE))
                .expect("clear confirm");
        }
        for character in "YES".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("type uppercase");
        }
        let accepted = app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("accept reset");
        assert!(matches!(
            accepted,
            AppAction::ManageAgent(AgentOperation::Reset { .. })
        ));
    }

    /// `L` on an identity row used to be silently swallowed (the handler
    /// only matched `AgentFocus::Teams`); it now opens the leave modal
    /// pre-filled with whichever identity is under the cursor.
    #[test]
    fn identity_focus_leave_opens_modal_with_selected_agent() {
        let (_temp, mut app) = fixture_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        assert_eq!(app.agent_focus, AgentFocus::Identities);
        // fixture_app registers claude-main / codex-worker; identities are
        // sorted alphabetically, so index 0 is claude-main.
        app.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE))
            .expect("identity focus leave");
        assert!(matches!(
            &app.agent_modal,
            Some(AgentModal::Leave { team, agent, .. })
                if team == "ops" && agent == "claude-main"
        ));
        for character in "YES".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("type YES");
        }
        let action = app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("leave");
        assert!(matches!(
            action,
            AppAction::ManageAgent(AgentOperation::Leave { .. })
        ));
    }

    /// Teams focus with no `AGMSG_IDENTITY` set previously rejected the leave
    /// outright ("leave requires AGMSG_IDENTITY"). It now defaults to the
    /// team's first identity, since `leave.sh` never needed the env var.
    #[test]
    fn team_focus_leave_defaults_to_first_identity_when_env_unset() {
        let (_temp, mut app) = fixture_app();
        assert!(app.current_identity.is_none());
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("team focus");
        app.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE))
            .expect("open leave");
        assert!(matches!(
            &app.agent_modal,
            Some(AgentModal::Leave { team, agent, .. })
                if team == "ops" && agent == "claude-main"
        ));
    }

    /// When `AGMSG_IDENTITY` is set and registered in the selected team, it
    /// still wins over the first-identity default (matches session intent).
    #[test]
    fn team_focus_leave_uses_env_identity_when_set() {
        let (_temp, mut app) = fixture_app();
        app.current_identity = Some("codex-worker".to_owned());
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("team focus");
        assert_eq!(app.agent_focus, AgentFocus::Teams);
        app.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE))
            .expect("open leave");
        assert!(matches!(
            &app.agent_modal,
            Some(AgentModal::Leave { team, agent, .. })
                if team == "ops" && agent == "codex-worker"
        ));
    }

    /// A team with zero registered identities has nothing to default to;
    /// the leave attempt must surface an error status instead of panicking
    /// or opening an empty modal.
    #[test]
    fn team_focus_leave_errors_when_team_has_no_identities() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db = temp.path().join("messages.db");
        Connection::open(&db)
            .expect("db")
            .execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    team TEXT NOT NULL,
                    from_agent TEXT NOT NULL,
                    to_agent TEXT NOT NULL,
                    body TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    read_at TEXT
                );",
            )
            .expect("schema");
        let teams_dir = temp.path().join("teams");
        fs::create_dir_all(teams_dir.join("empty-team")).expect("team dir");
        fs::write(
            teams_dir.join("empty-team/config.json"),
            r#"{"agents":{}}"#,
        )
        .expect("config");
        let mut app = App::load(Paths {
            db,
            teams_dir,
            scripts_dir: temp.path().join("scripts"),
            audit_script: temp.path().join("agmsg-audit"),
            audit_history: temp.path().join("audit.jsonl"),
            report_dir: temp.path().join("reports"),
            state_file: temp.path().join("state.json"),
        })
        .expect("app");

        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("team focus");
        app.handle_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE))
            .expect("open leave on empty team");
        assert!(app.agent_modal.is_none());
        assert!(app.status.is_error);
        assert_eq!(app.status.text, "empty-team has no identities to leave");
    }

    /// Phase 10.6 H-1: `R` used to only fire from Identities focus (the same
    /// silent-guard shape `L` had before Phase 10.5). Renaming isn't
    /// destructive, so Teams focus now resolves a target too instead of
    /// requiring a Tab first.
    #[test]
    fn rename_identity_from_teams_focus_opens_modal() {
        let (_temp, mut app) = fixture_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("team focus");
        assert_eq!(app.agent_focus, AgentFocus::Teams);
        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
            .expect("rename from teams focus");
        assert!(matches!(
            &app.agent_modal,
            Some(AgentModal::Rename { target, .. }) if target.name == "claude-main"
        ));
    }

    /// Phase 10.6 H-1: `T` used to only fire from Teams focus; Identities
    /// focus now resolves the currently selected team instead of no-op'ing.
    #[test]
    fn rename_team_from_identity_focus_opens_modal() {
        let (_temp, mut app) = fixture_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        assert_eq!(app.agent_focus, AgentFocus::Identities);
        app.handle_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::NONE))
            .expect("rename team from identity focus");
        assert!(matches!(
            &app.agent_modal,
            Some(AgentModal::RenameTeam { old, .. }) if old == "ops"
        ));
    }

    /// Phase 10.6 H-1: reset is destructive, so unlike R/T it deliberately
    /// keeps *not* acting from Teams focus — but it must say why instead of
    /// silently doing nothing.
    #[test]
    fn reset_from_teams_focus_sets_guidance_status() {
        let (_temp, mut app) = fixture_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("team focus");
        app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE))
            .expect("reset from teams focus");
        assert!(app.agent_modal.is_none());
        assert!(app.status.is_error);
        assert_eq!(
            app.status.text,
            "X: switch to identity focus (Tab) and select a target"
        );
    }

    /// Phase 10.6 H-2: `y` used to only yank from `Focus::Room`, silently
    /// doing nothing from Teams/Members even though a message was already
    /// selected in the room behind them.
    #[test]
    fn yank_from_teams_focus_returns_yank_action() {
        let (_temp, mut app) = fixture_app_with_messages(&[("claude-main", "codex-worker", "hi")]);
        assert_eq!(app.focus, Focus::Teams);
        let action = app
            .handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("yank from teams focus");
        assert!(matches!(action, AppAction::Yank(body) if body == "hi"));
    }

    /// Phase 10.6 H-2: with no message selected (empty room), `y` must set a
    /// status hint rather than silently doing nothing.
    #[test]
    fn yank_without_message_sets_status() {
        let (_temp, mut app) = fixture_app_with_messages(&[]);
        let action = app
            .handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("yank with no message");
        assert!(matches!(action, AppAction::None));
        assert!(app.status.is_error);
        assert_eq!(app.status.text, "no message selected");
    }

    /// Phase 10.6 H-3: without `AGMSG_IDENTITY`, the self-reset guard, the
    /// own-message marker, and the composer `from` default all quietly
    /// degrade — surface that once at startup instead of leaving it
    /// undiscoverable.
    #[test]
    fn startup_without_identity_sets_warning_status() {
        let (_temp, app) = fixture_app();
        assert!(app.current_identity.is_none());
        assert!(app.status.text.starts_with("⚠ AGMSG_IDENTITY unset"));
        assert!(!app.status.is_error);
    }

    /// Phase 10.6 M-3: `c` used to hardcode `from_index: 0`, so it silently
    /// sent as roster[0] even when `AGMSG_IDENTITY` named a different
    /// registered agent. It must resolve the same way the MEMBER-pane Enter
    /// composer path (`open_composer_for_member`) already does.
    #[test]
    fn composer_c_defaults_from_to_current_identity() {
        let (_temp, mut app) = fixture_app();
        app.current_identity = Some("codex-worker".to_owned());
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
            .expect("open composer");
        let composer = app.composer.as_ref().expect("composer open");
        assert_eq!(composer.from_agent(), Some("codex-worker"));
    }

    #[test]
    fn agent_command_errors_fall_back_to_stdout_and_reset_noop_warns() {
        let (_temp, mut app) = fixture_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        let rename = AgentOperation::Rename {
            team: "ops".to_owned(),
            old: "codex-worker".to_owned(),
            new: "codex-review".to_owned(),
            self_rename: false,
        };
        app.complete_agent_operation(
            &rename,
            &CommandResult {
                success: false,
                exit_code: Some(1),
                stdout: "Agent codex-worker not in team".to_owned(),
                stderr: String::new(),
            },
        )
        .expect("failed rename completion");
        assert_eq!(app.status.text, "Agent codex-worker not in team");
        assert!(app.status.is_error);

        let join = AgentOperation::Join {
            team: "new-team".to_owned(),
            agent: "codex-worker".to_owned(),
            agent_type: "codex".to_owned(),
            project: "/tmp/ops".to_owned(),
        };
        app.complete_agent_operation(
            &join,
            &CommandResult {
                success: false,
                exit_code: Some(1),
                stdout: String::new(),
                stderr: "'codex-worker' was renamed to 'codex-review'".to_owned(),
            },
        )
        .expect("join tombstone completion");
        assert!(matches!(app.agent_modal, Some(AgentModal::JoinForce { .. })));
        let force = app
            .handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("force join confirmation");
        assert!(matches!(
            force,
            AppAction::ManageAgent(AgentOperation::JoinForce { .. })
        ));

        let reset = AgentOperation::Reset {
            project: "/tmp/ops".to_owned(),
            agent_type: "codex".to_owned(),
            agent: "codex-worker".to_owned(),
        };
        app.complete_agent_operation(
            &reset,
            &CommandResult {
                success: true,
                exit_code: Some(0),
                stdout: "No registrations removed.".to_owned(),
                stderr: String::new(),
            },
        )
        .expect("reset no-op completion");
        assert_eq!(app.status.text, "⚠ No registrations removed.");
        assert!(!app.status.is_error);
    }

    fn unread_message(id: i64, from: &str) -> Message {
        Message {
            id,
            team: "ops".to_owned(),
            from_agent: from.to_owned(),
            to_agent: "claude-main".to_owned(),
            body: format!("ping {id}"),
            created_at: "2026-01-01T01:00:00Z".to_owned(),
            read_at: None,
        }
    }

    #[test]
    fn new_unread_message_for_selected_team_queues_bell_and_desktop_notification() {
        let (_temp, mut app) = fixture_app();
        app.receive_new_messages(vec![unread_message(1, "codex-worker")])
            .expect("receive");
        let queued = app.drain_pending_notifications();
        assert_eq!(
            queued,
            vec![
                PendingNotification::Bell,
                PendingNotification::Desktop {
                    from: "codex-worker".to_owned(),
                    body: "ping 1".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn muted_bell_setting_suppresses_only_the_bell_notification() {
        let (_temp, mut app) = fixture_app();
        app.notify_settings.bell = false;
        app.receive_new_messages(vec![unread_message(1, "codex-worker")])
            .expect("receive");
        let queued = app.drain_pending_notifications();
        assert_eq!(
            queued,
            vec![PendingNotification::Desktop {
                from: "codex-worker".to_owned(),
                body: "ping 1".to_owned(),
            }]
        );
    }

    #[test]
    fn ctrl_b_toggles_bell_mute_and_reports_status() {
        let (_temp, mut app) = fixture_app();
        assert!(app.notify_settings.bell);
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .expect("mute");
        assert!(!app.notify_settings.bell);
        assert_eq!(app.status.text, "terminal bell muted");
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .expect("unmute");
        assert!(app.notify_settings.bell);
        assert_eq!(app.status.text, "terminal bell unmuted");
    }

    #[test]
    fn ctrl_n_opens_popup_and_enter_toggles_selected_row() {
        let (_temp, mut app) = fixture_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
            .expect("open popup");
        assert_eq!(app.notify_popup, Some(0));
        assert!(app.notify_settings.desktop);

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .expect("move down");
        assert_eq!(app.notify_popup, Some(1));
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("toggle desktop row");
        assert!(!app.notify_settings.desktop);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close popup");
        assert_eq!(app.notify_popup, None);
    }

    #[test]
    fn burst_of_messages_sets_alert_and_suppresses_bell_and_desktop() {
        let (_temp, mut app) = fixture_app();
        let now = std::time::Instant::now();
        let flood: Vec<Message> = (1..=25)
            .map(|id| unread_message(id, "codex-worker"))
            .collect();
        app.notify_for_arrivals(flood.len(), flood.last().cloned(), now);
        assert!(app.burst_alert.is_some());
        assert!(app.is_burst_active(now));
        assert!(app.drain_pending_notifications().is_empty());
    }

    #[test]
    fn burst_alert_expires_after_its_display_window() {
        let (_temp, mut app) = fixture_app();
        let now = std::time::Instant::now();
        app.notify_for_arrivals(25, Some(unread_message(1, "codex-worker")), now);
        assert!(app.is_burst_active(now));
        let later = now + crate::notify::BURST_ALERT_DURATION + std::time::Duration::from_secs(1);
        assert!(!app.is_burst_active(later));
    }

    #[test]
    fn in_flight_suppresses_duplicate_send_and_mark_and_reports_recipient_scope() {
        let (_temp, mut app) =
            fixture_app_with_messages(&[("claude-main", "codex-worker", "unread")]);
        app.focus = Focus::Room;
        let mark = app
            .handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("mark action");
        assert!(matches!(
            mark,
            AppAction::MarkRecipient {
                unread_count: 1,
                ..
            }
        ));
        assert!(app.start_operation(InFlightOperation::MarkRead));
        let duplicate = app
            .handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .expect("duplicate mark");
        assert!(matches!(duplicate, AppAction::None));
        app.finish_operation();
        app.complete_mark_read(
            &CommandResult {
                success: true,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            },
            "codex-worker",
            1,
        )
        .expect("mark completion");
        assert_eq!(
            app.status.text,
            "marked read: all for codex-worker (1 msgs)"
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
            .expect("compose");
        app.composer.as_mut().expect("composer").body = "hello".to_owned();
        let send = app
            .handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .expect("send action");
        assert!(matches!(send, AppAction::Send(_)));
        assert!(app.start_operation(InFlightOperation::Send));
        let duplicate = app
            .handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .expect("duplicate send");
        assert!(matches!(duplicate, AppAction::None));
        assert!(app.advance_spinner());
        assert!(app.status.text.starts_with("sending... "));
    }

    #[test]
    fn robust_navigation_and_database_search_cover_unloaded_history() {
        let (_temp, mut app) = fixture_app();
        let connection = Connection::open(&app.paths.db).expect("writable fixture db");
        for index in 0..215 {
            let from = if matches!(index, 20 | 150) {
                "cursor-1"
            } else {
                "claude-main"
            };
            let body = if index == 0 {
                "needle outside initial page"
            } else {
                "ordinary"
            };
            let read_at = (!matches!(index, 20 | 80)).then_some("2026-01-02T00:00:00Z");
            connection
                .execute(
                    "INSERT INTO messages
                     (team, from_agent, to_agent, body, created_at, read_at)
                     VALUES ('ops', ?1, 'codex-worker', ?2, ?3, ?4)",
                    rusqlite::params![from, body, format!("2026-01-01T00:{index:03}:00Z"), read_at],
                )
                .expect("insert history");
        }
        drop(connection);
        app.reload_selected_team().expect("reload history");
        assert_eq!(app.messages.len(), 200);

        app.focus = Focus::Room;
        app.member_filter = Some("cursor-1".to_owned());
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE))
            .expect("filtered first");
        assert_eq!(
            app.selected_message().expect("first").from_agent,
            "cursor-1"
        );
        let first_cursor_id = app.selected_message().expect("first").id;
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .expect("filtered half page up");
        assert_eq!(
            app.selected_message().expect("filtered first stays selected").id,
            first_cursor_id
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE))
            .expect("filtered last");
        assert_eq!(app.selected_message().expect("last").from_agent, "cursor-1");
        assert_ne!(app.selected_message().expect("last").id, first_cursor_id);

        app.member_filter = None;
        app.selected_message = 0;
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .expect("half page down");
        assert_eq!(app.selected_message, 10);
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .expect("half page up");
        assert_eq!(app.selected_message, 0);
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
            .expect("next unread");
        assert_eq!(app.selected_message().expect("unread").read_at, None);

        for key in [
            KeyCode::Char('/'),
            KeyCode::Char('n'),
            KeyCode::Char('e'),
            KeyCode::Char('e'),
            KeyCode::Char('d'),
            KeyCode::Char('l'),
            KeyCode::Char('e'),
            KeyCode::Enter,
        ] {
            app.handle_key(KeyEvent::new(key, KeyModifiers::NONE))
                .expect("database search");
        }
        assert_eq!(
            app.selected_message().expect("global hit").body,
            "needle outside initial page"
        );
        assert!(app.messages.len() > 200);
    }

    #[test]
    fn fold_state_survives_team_round_trip_and_app_state_restores() {
        let long_body = "x".repeat(600);
        let (_temp, mut app) =
            fixture_app_with_messages(&[("claude-main", "codex-worker", &long_body)]);
        fs::create_dir_all(app.paths.teams_dir.join("zeta")).expect("zeta team");
        fs::write(
            app.paths.teams_dir.join("zeta/config.json"),
            r#"{"agents":{"claude-main":{},"codex-worker":{}}}"#,
        )
        .expect("zeta config");
        Connection::open(&app.paths.db)
            .expect("fixture db")
            .execute(
                "INSERT INTO messages (team, from_agent, to_agent, body, created_at)
                 VALUES ('zeta', 'claude-main', 'codex-worker', 'zeta', '2026-01-02T00:00:00Z')",
                [],
            )
            .expect("zeta message");
        app.refresh_team_summaries().expect("refresh teams");

        app.focus = Focus::Room;
        let expanded_id = app.selected_message().expect("ops message").id;
        app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE))
            .expect("expand");
        app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE))
            .expect("next team");
        assert_eq!(app.selected_team_name(), Some("zeta"));
        app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE))
            .expect("previous team");
        assert_eq!(app.selected_team_name(), Some("ops"));
        assert!(
            app.expanded_messages
                .get("ops")
                .is_some_and(|ids| ids.contains(&expanded_id))
        );

        app.sidebar_pct = 47;
        app.notify_settings.desktop = false;
        app.drafts.insert(
            "ops".to_owned(),
            ComposerState {
                roster: vec!["claude-main".to_owned(), "codex-worker".to_owned()],
                from_index: 0,
                to_index: 1,
                body: "saved for ops".to_owned(),
                cursor: 13,
            },
        );
        app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE))
            .expect("persist zeta as last team");
        app.save_state().expect("save app state");

        let restored = App::load(app.paths.clone()).expect("restore app state");
        assert_eq!(restored.sidebar_pct, 47);
        assert_eq!(restored.selected_team_name(), Some("zeta"));
        assert!(!restored.notify_settings.desktop);
        assert_eq!(
            restored.drafts.get("ops").map(|draft| draft.body.as_str()),
            Some("saved for ops")
        );
    }

    /// M-1: ManageAgent scripts used to `.await` directly inside
    /// `execute_action`, freezing key input/poll/render for up to
    /// SCRIPT_TIMEOUT. Confirming a modal must hand back an `AppAction`
    /// synchronously (never block `handle_key` itself), and once the
    /// caller marks the operation in flight via `start_operation`, a second
    /// confirm attempt must be dropped instead of firing a second script —
    /// same re-entrancy shape Send/MarkRead already have.
    #[test]
    fn agent_operation_start_returns_control_immediately_and_blocks_reentry() {
        let (_temp, mut app) = fixture_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE))
            .expect("team focus");
        app.handle_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::NONE))
            .expect("open rename team modal");
        for character in "ops2".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("type new team name");
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm stage");
        let action = app
            .handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("confirm rename team");
        // handle_key returned the action to run synchronously — it never
        // blocked on the script itself.
        assert!(matches!(
            action,
            AppAction::ManageAgent(AgentOperation::RenameTeam { .. })
        ));

        assert!(app.start_operation(InFlightOperation::Agent));
        let reentry = app
            .handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            .expect("reentrant key while agent op in flight");
        assert!(matches!(reentry, AppAction::None));
        assert!(
            app.agent_modal.is_none(),
            "reentry must not open a second modal while the first op is running"
        );
        assert!(app.advance_spinner());
        assert!(app.status.text.starts_with("running agent op... "));

        app.finish_operation();
        assert!(app.start_operation(InFlightOperation::Agent));
    }

    /// M-2: `f` used to be entirely unbound. First press folds nothing (all
    /// foldable messages start collapsed by default), so it should unfold
    /// everything in the team at once; pressing again re-collapses them.
    #[test]
    fn fold_all_toggles_every_foldable_message_in_the_team() {
        let long_body = "x".repeat(FOLD_CHAR_THRESHOLD + 10);
        let (_temp, mut app) = fixture_app_with_messages(&[
            ("claude-main", "codex-worker", &long_body),
            ("codex-worker", "claude-main", &long_body),
            ("claude-main", "codex-worker", "short"),
        ]);
        let team = app.selected_team_name().unwrap().to_owned();
        assert!(
            !app.expanded_messages.contains_key(&team),
            "starts collapsed"
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .expect("fold-all unfolds first");
        assert_eq!(app.expanded_messages.get(&team).map(|ids| ids.len()), Some(2));
        assert!(app.status.text.starts_with("unfolded"));

        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .expect("fold-all collapses second");
        assert!(
            app.expanded_messages
                .get(&team)
                .is_none_or(|ids| ids.is_empty())
        );
        assert!(app.status.text.starts_with("folded"));
    }

    /// M-2: `s` used to be entirely unbound. It should jump the selection
    /// to the nearest other message from the same sender as the currently
    /// selected one.
    #[test]
    fn jump_to_sender_selects_nearest_message_from_same_sender() {
        let (_temp, mut app) = fixture_app_with_messages(&[
            ("claude-main", "codex-worker", "one"),
            ("codex-worker", "claude-main", "two"),
            ("claude-main", "codex-worker", "three"),
        ]);
        app.focus = Focus::Room;
        app.selected_message = 0;
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("jump to sender");
        assert_eq!(app.selected_message, 2);
        assert_eq!(app.status.text, "jumped to claude-main");
    }

    /// M-2: `s` with no other message from that sender must say so instead
    /// of silently leaving the selection unchanged.
    #[test]
    fn jump_to_sender_without_another_match_sets_status() {
        let (_temp, mut app) = fixture_app_with_messages(&[("claude-main", "codex-worker", "one")]);
        app.focus = Focus::Room;
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .expect("jump to sender with no other match");
        assert_eq!(app.selected_message, 0);
        assert_eq!(app.status.text, "no other message from claude-main");
    }

    /// M-2: audit `x` (export) and `g` (jump to top) were both listed in the
    /// requirements doc but unbound in the implementation.
    #[test]
    fn audit_x_exports_and_g_jumps_to_top() {
        let (_temp, mut app) = fixture_app();
        app.screen = Screen::Audit;
        let export = app
            .handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("audit x");
        assert!(matches!(export, AppAction::ExportReport));

        app.audit_selected = 3;
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE))
            .expect("audit g");
        assert_eq!(app.audit_selected, 0);
    }

    /// M-4: the MEMBER pane's `R` used to silently no-op with no member
    /// selected (e.g. an empty roster) — only the *downstream* failure
    /// modes (team not found, identity not registered) surfaced a status.
    #[test]
    fn member_rename_without_selection_sets_status() {
        let (_temp, mut app) = fixture_app_with_messages(&[]);
        assert!(!app.members.is_empty(), "fixture roster seeds MEMBER rows");
        // Out-of-bounds cursor stands in for "nothing selected" — the same
        // shape a freshly-emptied roster or a stale index after a reload
        // would produce.
        app.selected_member = app.members.len();
        app.focus = Focus::Members;
        app.handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE))
            .expect("rename with no member selected");
        assert_eq!(app.screen, Screen::Main);
        assert!(app.status.is_error);
        assert_eq!(app.status.text, "no member selected");
    }

    /// M-5: `n`/`N` used to no-op with no active search, giving no hint that
    /// `/` needs to be pressed first.
    #[test]
    fn search_next_without_query_sets_hint() {
        let (_temp, mut app) = fixture_app_with_messages(&[("claude-main", "codex-worker", "hi")]);
        app.focus = Focus::Room;
        let next = app
            .handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE))
            .expect("n without active search");
        assert!(matches!(next, AppAction::None));
        assert!(!app.status.is_error);
        assert_eq!(app.status.text, "no active search (press /)");

        let previous = app
            .handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE))
            .expect("N without active search");
        assert!(matches!(previous, AppAction::None));
        assert_eq!(app.status.text, "no active search (press /)");
    }

    /// L-1: `x`/`X` on a message short enough that it was never foldable
    /// used to no-op with no explanation.
    #[test]
    fn fold_toggle_on_short_message_sets_status() {
        let (_temp, mut app) = fixture_app_with_messages(&[("claude-main", "codex-worker", "short")]);
        app.focus = Focus::Room;
        app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE))
            .expect("fold toggle on short message");
        assert!(!app.status.is_error);
        assert_eq!(app.status.text, "message too short to fold");
    }

    /// L-1: `x`/`X` with no message selected (empty room) used to no-op.
    #[test]
    fn fold_toggle_without_message_sets_status() {
        let (_temp, mut app) = fixture_app_with_messages(&[]);
        app.focus = Focus::Room;
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .expect("fold toggle with no message");
        assert!(app.status.is_error);
        assert_eq!(app.status.text, "no message selected");
    }

    /// L-3: Enter on an Identities-focus Agents-screen row used to do
    /// nothing at all — there was no equivalent of the MEMBER pane's `I`
    /// info popup on this screen. It now opens/closes one, and reports a
    /// status instead of no-op'ing when nothing is selected.
    #[test]
    fn identities_focus_enter_opens_and_closes_identity_info_popup() {
        let (_temp, mut app) = fixture_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("open agents");
        assert_eq!(app.agent_focus, AgentFocus::Identities);

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("open identity info");
        let info = app.agent_identity_info.clone().expect("info popup open");
        assert!(info.contains("Agent: claude-main"));

        // While open, other Agents-screen keys are swallowed except close.
        app.handle_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::NONE))
            .expect("swallowed while popup open");
        assert!(app.agent_modal.is_none());

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("close identity info");
        assert!(app.agent_identity_info.is_none());
    }

    /// L-4: `pbcopy` failing used to be swallowed inside `clipboard::yank`,
    /// so a broken clipboard bridge left the "yanked N chars" status even
    /// though nothing actually reached the clipboard. `complete_yank_fallback`
    /// is what `main.rs` now calls on that path — it must log the body and
    /// say so instead of claiming success.
    #[test]
    fn complete_yank_fallback_logs_body_and_reports_path() {
        let (_temp, mut app) = fixture_app();
        app.complete_yank_fallback("fallback body");
        assert!(!app.status.is_error);
        assert!(app.status.text.starts_with("clipboard unavailable, message logged to"));
        let path = app
            .paths
            .report_dir
            .join("agmsg-tui-clipboard-fallback.txt");
        assert_eq!(fs::read_to_string(path).expect("fallback file"), "fallback body");
    }

    /// L-4: bell/OSC 9/title notification IO used to be dropped outright
    /// (`let _ = ...`) in `main.rs`. `warn_notify_failure_once` is the
    /// method it now calls on failure — verify it fires exactly once so a
    /// broken terminal doesn't spam the status line on every message.
    #[test]
    fn warn_notify_failure_once_fires_a_single_time() {
        let (_temp, mut app) = fixture_app();
        app.status = StatusLine {
            text: "ready".to_owned(),
            is_error: false,
        };
        app.warn_notify_failure_once();
        assert_eq!(app.status.text, "⚠ notification unavailable");
        app.status = StatusLine {
            text: "something else".to_owned(),
            is_error: false,
        };
        app.warn_notify_failure_once();
        assert_eq!(app.status.text, "something else", "second warning must be muted");
    }

    #[test]
    fn bulk_mark_read_requires_exact_yes_then_tracks_message_progress() {
        let (_temp, mut app) = fixture_app_with_messages(&[(
            "claude-main",
            "codex-worker",
            "bulk me",
        )]);
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .expect("open bulk");
        let filter = app.bulk_filter.as_mut().expect("filter loaded");
        filter.period = FilterPeriod::All;
        filter.recompute(chrono::Utc::now());
        app.handle_key(KeyEvent::new(KeyCode::Char('M'), KeyModifiers::NONE))
            .expect("preview");
        for character in ['Y', 'E', 'S'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                .expect("confirm text");
        }
        let action = app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("confirm");
        assert!(matches!(
            action,
            AppAction::RunBulk {
                target: BulkTarget::MarkRead(_),
                force_despawn: false
            }
        ));
        let target = app
            .bulk_operation
            .as_ref()
            .and_then(|operation| operation.current_target())
            .cloned()
            .expect("target");
        let next = app
            .complete_bulk_target(
                target,
                Ok(CommandResult {
                    success: true,
                    exit_code: Some(0),
                    stdout: "1 new message(s)".to_owned(),
                    stderr: String::new(),
                }),
            )
            .expect("completion");
        assert!(matches!(next, AppAction::None));
        assert_eq!(app.status.text, "marked 1 msgs as read (0 failed)");
        assert!(matches!(
            app.bulk_operation.as_ref().map(|operation| &operation.state),
            Some(BulkRunState::Complete { aborted: false })
        ));
    }

    #[test]
    fn bulk_failure_waits_for_continue_before_dispatching_next_target() {
        let (_temp, mut app) = fixture_app();
        app.screen = Screen::BulkFilter;
        let first = BulkTarget::MarkRead(MarkReadTarget {
            team: "ops".to_owned(),
            recipient: "codex-worker".to_owned(),
            message_count: 2,
        });
        let second = BulkTarget::MarkRead(MarkReadTarget {
            team: "ops".to_owned(),
            recipient: "claude-main".to_owned(),
            message_count: 1,
        });
        app.begin_bulk_operation(BulkKind::MarkRead, vec![first.clone(), second.clone()])
            .expect("begin");
        app.complete_bulk_target(
            first,
            Ok(CommandResult {
                success: false,
                exit_code: Some(2),
                stdout: String::new(),
                stderr: "inbox failed".to_owned(),
            }),
        )
        .expect("failed step");
        assert!(matches!(
            app.bulk_operation.as_ref().map(|operation| &operation.state),
            Some(BulkRunState::AwaitDecision { .. })
        ));
        let continue_action = app
            .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE))
            .expect("continue");
        assert!(matches!(
            continue_action,
            AppAction::RunBulk { target, .. } if target == second
        ));
        assert_eq!(
            app.bulk_operation
                .as_ref()
                .map(|operation| operation.failed_units),
            Some(2)
        );
    }

    #[test]
    fn graceful_despawn_failure_requires_explicit_force_choice() {
        let (_temp, mut app) = fixture_app();
        app.screen = Screen::Agents;
        let target = BulkTarget::Despawn(DespawnTarget {
            team: "ops".to_owned(),
            from: "claude-main".to_owned(),
            name: "codex-worker".to_owned(),
        });
        app.begin_bulk_operation(BulkKind::Despawn, vec![target.clone()])
            .expect("begin");
        app.complete_bulk_target(
            target.clone(),
            Ok(CommandResult {
                success: false,
                exit_code: Some(3),
                stdout: "status=timeout".to_owned(),
                stderr: "retry with --force".to_owned(),
            }),
        )
        .expect("graceful failure");
        assert!(matches!(
            app.bulk_operation.as_ref().map(|operation| &operation.state),
            Some(BulkRunState::AwaitForce { .. })
        ));
        let force = app
            .handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("force choice");
        assert!(matches!(
            force,
            AppAction::RunBulk {
                target: force_target,
                force_despawn: true
            } if force_target == target
        ));
    }

    #[test]
    fn escape_signals_bulk_cancel_channel_and_finishes_aborted() {
        let (_temp, mut app) = fixture_app();
        app.screen = Screen::BulkFilter;
        let target = BulkTarget::MarkRead(MarkReadTarget {
            team: "ops".to_owned(),
            recipient: "codex-worker".to_owned(),
            message_count: 1,
        });
        app.begin_bulk_operation(BulkKind::MarkRead, vec![target.clone()])
            .expect("begin");
        let receiver = app.bulk_cancel_receiver().expect("cancel receiver");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .expect("cancel key");
        assert!(*receiver.borrow());
        app.complete_bulk_target(target, Err("cancelled: inbox.sh".to_owned()))
            .expect("cancel completion");
        assert!(matches!(
            app.bulk_operation.as_ref().map(|operation| &operation.state),
            Some(BulkRunState::Complete { aborted: true })
        ));
        assert_eq!(
            app.bulk_operation
                .as_ref()
                .map(|operation| operation.failed_units),
            Some(0)
        );
    }

    #[test]
    fn identity_info_reports_actas_lock_owner() {
        let (temp, mut app) = fixture_app_with_messages(&[(
            "claude-main",
            "codex-worker",
            "hello",
        )]);
        let run_dir = temp.path().join("run");
        fs::create_dir_all(&run_dir).expect("run dir");
        fs::write(
            run_dir.join("actas.ops__codex-worker.session"),
            "session-owner.123\n",
        )
        .expect("lock");
        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
            .expect("agents");
        app.agent_focus = AgentFocus::Identities;
        app.agent_identity_index = app
            .selected_agent_team()
            .and_then(|team| {
                team.identities
                    .iter()
                    .position(|identity| identity.name == "codex-worker")
            })
            .expect("codex identity");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("info");
        assert!(
            app.agent_identity_info
                .as_deref()
                .is_some_and(|info| info.contains("Actas lock: session-owner.123"))
        );
    }
}
