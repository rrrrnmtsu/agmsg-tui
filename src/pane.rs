//! Phase 14A: split-view pane state.
//!
//! Two-team side-by-side viewing reuses the single-pane `App` fields for
//! whichever pane is "active" (see `App::swap_active_pane` in `app.rs`)
//! instead of moving every existing nav/query method onto a duplicated
//! struct up front — that full pane-0 extraction is explicitly deferred by
//! the phase 14 plan doc (14A section) because it would churn every
//! existing test and mouse-hit-testing path in one phase. `PaneState` here
//! only holds the *inactive* pane's frozen copy of those same fields, and
//! `PaneView` is the read-only facade the renderer uses so `main_screen.rs`
//! never has to match on which storage (live `App` fields vs. a stored
//! `PaneState`) a given pane's data actually lives in.

use std::collections::{HashMap, HashSet};

use crate::app::Focus;
use crate::db::{MemberSummary, Message, TeamSummary};

/// Per-pane view state duplicated when split-view is active. Mirrors the
/// subset of `App` fields that are genuinely per-team: which team/member/
/// message is selected, what's loaded, and which of TEAMS/MEMBERS/ROOM has
/// keyboard focus within that pane.
///
/// Search (`search_query`) and the MEMBER filter (`member_filter`) are
/// deliberately *not* here — the 14A plan's non-goals list "independent
/// search state per pane" out; both stay global `App` fields that only ever
/// apply to whichever pane is currently active.
#[derive(Clone, Debug)]
pub struct PaneState {
    pub selected_team: usize,
    pub active_team: Option<String>,
    pub active_team_host: String,
    pub members: Vec<MemberSummary>,
    pub selected_member: usize,
    pub messages: Vec<Message>,
    pub selected_message: usize,
    pub focus: Focus,
    pub has_more_history: bool,
}

/// Which physical slot (left/top vs. right/bottom) a pane occupies. Slot
/// identity is stable across `Tab` presses — only which slot is "active"
/// changes, not which slot a given team's view is drawn in (see
/// `App::swap_active_pane`'s doc comment for why this reads more naturally
/// than repositioning panes on every switch).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneIdx {
    First,
    Second,
}

impl PaneIdx {
    #[must_use]
    pub fn other(self) -> Self {
        match self {
            PaneIdx::First => PaneIdx::Second,
            PaneIdx::Second => PaneIdx::First,
        }
    }
}

/// Main-screen split state. `Off` is the default and the only state that
/// existed before 14A — every render/nav path must stay byte-identical to
/// pre-14A behavior in that variant (the "Off-mode golden regression").
#[derive(Clone, Debug, Default)]
pub enum SplitMode {
    #[default]
    Off,
    Split {
        second: PaneState,
        active: PaneIdx,
    },
}

/// Read-only rendering facade over one pane's data, built fresh each frame
/// from either the live `App` fields (the active pane, via
/// `App::pane_view_active`) or a stored `PaneState` (the inactive pane, via
/// `App::pane_view_of`). `main_screen.rs`'s render helpers take this instead
/// of `&App` so the same drawing code runs for both panes without knowing
/// which storage backs the one it was handed.
pub struct PaneView<'a> {
    pub teams: &'a [TeamSummary],
    pub selected_team: usize,
    pub active_team: Option<&'a str>,
    pub members: &'a [MemberSummary],
    pub selected_member: usize,
    pub messages: &'a [Message],
    pub selected_message: usize,
    pub focus: Focus,
    pub has_more_history: bool,
    /// `Some` only on the pane that's currently active — non-goal per the
    /// 14A plan doc for the inactive pane to keep its own filter.
    pub member_filter: Option<&'a str>,
    /// Same active-pane-only rule as `member_filter`.
    pub search_query: Option<&'a str>,
    pub expanded_messages: &'a HashMap<String, HashSet<i64>>,
    pub current_identity: Option<&'a str>,
}

impl<'a> PaneView<'a> {
    pub fn selected_team_name(&self) -> Option<&str> {
        self.active_team
    }

    pub fn selected_message(&self) -> Option<&Message> {
        self.messages.get(self.selected_message)
    }

    pub fn message_matches_filter(&self, message: &Message) -> bool {
        match self.member_filter {
            None => true,
            Some(name) => message.from_agent == name || message.to_agent == name,
        }
    }

    pub fn message_matches_search(&self, message: &Message) -> bool {
        let Some(query) = self.search_query else {
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

    pub fn body_is_folded(&self, message: &Message) -> bool {
        crate::width::display_width(&message.body) > crate::app::FOLD_CHAR_THRESHOLD
            && !self
                .expanded_messages
                .get(&message.team)
                .is_some_and(|ids| ids.contains(&message.id))
    }

    pub fn is_own_message(&self, message: &Message) -> bool {
        self.current_identity == Some(message.from_agent.as_str())
    }
}
