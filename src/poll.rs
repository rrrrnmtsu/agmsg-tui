use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use tokio::time::Instant;

use crate::db::{Database, Message};

const NORMAL_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(30);

pub struct LivePoller {
    next_due: Instant,
    last_seen_id: i64,
    next_retry: Duration,
    offline: bool,
    recovered: bool,
}

impl LivePoller {
    pub fn new(last_seen_id: i64) -> Self {
        Self {
            next_due: Instant::now() + NORMAL_POLL_INTERVAL,
            last_seen_id,
            next_retry: NORMAL_POLL_INTERVAL,
            offline: false,
            recovered: false,
        }
    }

    pub async fn poll_if_due(
        &mut self,
        database: &Database,
        teams_dir: &Path,
    ) -> Result<Option<Vec<Message>>> {
        if Instant::now() < self.next_due {
            return Ok(None);
        }

        let result = (|| {
            let mut messages = Vec::new();
            for team in database.team_names(teams_dir)? {
                messages.extend(database.new_messages_for_team(&team, self.last_seen_id)?);
            }
            messages.sort_by_key(|message| message.id);
            Ok::<_, anyhow::Error>(messages)
        })();

        match result {
            Ok(messages) => {
                if let Some(last) = messages.last() {
                    self.last_seen_id = last.id;
                }
                self.record_success(Instant::now());
                Ok(Some(messages))
            }
            Err(error) => {
                self.record_failure(Instant::now());
                Err(error)
            }
        }
    }

    pub fn take_recovered(&mut self) -> bool {
        std::mem::take(&mut self.recovered)
    }

    fn record_failure(&mut self, now: Instant) -> Duration {
        let scheduled = self.next_retry;
        self.next_due = now + scheduled;
        self.next_retry = (self.next_retry * 2).min(MAX_RETRY_INTERVAL);
        self.offline = true;
        self.recovered = false;
        scheduled
    }

    fn record_success(&mut self, now: Instant) {
        self.next_due = now + NORMAL_POLL_INTERVAL;
        self.next_retry = NORMAL_POLL_INTERVAL;
        self.recovered = self.offline;
        self.offline = false;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::LivePoller;

    #[test]
    fn failures_back_off_to_thirty_seconds_and_success_recovers() {
        let mut poller = LivePoller::new(0);
        let now = tokio::time::Instant::now();
        let delays: Vec<u64> = (0..7)
            .map(|_| poller.record_failure(now).as_secs())
            .collect();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30]);

        poller.record_success(now + Duration::from_secs(31));
        assert!(poller.take_recovered());
        assert!(!poller.take_recovered());
        assert_eq!(poller.record_failure(now).as_secs(), 1);
    }
}
