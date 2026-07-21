use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::app::ComposerState;
use crate::notify::NotifySettings;
use crate::ui::SIDEBAR_DEFAULT_PCT;

const STATE_SCHEMA_VERSION: u32 = 1;

/// ユーザー操作で変わる表示状態だけを保存する。DB/config の write path とは分離する。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistentState {
    pub schema_version: u32,
    pub sidebar_pct: u16,
    pub last_team: Option<String>,
    #[serde(default)]
    pub drafts: HashMap<String, ComposerState>,
    #[serde(default)]
    pub notify_settings: NotifySettings,
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            sidebar_pct: SIDEBAR_DEFAULT_PCT,
            last_team: None,
            drafts: HashMap::new(),
            notify_settings: NotifySettings::default(),
        }
    }
}

impl PersistentState {
    /// 壊れたJSONや将来schemaは起動を止めず、全項目をdefaultへ戻す。
    pub fn load(path: &Path) -> Self {
        let Ok(content) = fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(state) = serde_json::from_str::<Self>(&content) else {
            return Self::default();
        };
        if state.schema_version != STATE_SCHEMA_VERSION {
            return Self::default();
        }
        state
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("state file の親ディレクトリがありません")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("stateディレクトリを作成できません: {}", parent.display()))?;
        let temporary = path.with_extension("json.tmp");
        let content = serde_json::to_string_pretty(self).context("state JSONを生成できません")?;
        fs::write(&temporary, content)
            .with_context(|| format!("stateを書き込めません: {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("stateを置換できません: {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::PersistentState;

    #[test]
    fn state_round_trip_and_schema_mismatch_fallback() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config/agmsg-tui/state.json");
        let state = PersistentState {
            sidebar_pct: 47,
            last_team: Some("ops-hub".to_owned()),
            ..PersistentState::default()
        };
        state.save(&path).expect("save state");
        assert_eq!(PersistentState::load(&path), state);

        fs::write(
            &path,
            r#"{"schema_version":99,"sidebar_pct":55,"last_team":"stale"}"#,
        )
        .expect("future state");
        assert_eq!(PersistentState::load(&path), PersistentState::default());
    }
}
