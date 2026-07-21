use crate::db::AgentIdentitySummary;

pub const CLI_TYPES: [&str; 10] = [
    "claude-code",
    "codex",
    "cursor",
    "gemini",
    "copilot",
    "opencode",
    "kimi",
    "hermes",
    "antigravity",
    "grok-build",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentFocus {
    Teams,
    Identities,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnStep {
    Team,
    NewTeam,
    CliType,
    Name,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnModalState {
    pub step: SpawnStep,
    pub team_index: usize,
    pub new_team: bool,
    pub team_input: String,
    pub type_index: usize,
    pub name: String,
}

impl SpawnModalState {
    pub fn new(team_index: usize) -> Self {
        Self {
            step: SpawnStep::Team,
            team_index,
            new_team: false,
            team_input: String::new(),
            type_index: 0,
            name: String::new(),
        }
    }

    pub fn agent_type(&self) -> &'static str {
        CLI_TYPES[self.type_index.min(CLI_TYPES.len() - 1)]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentModal {
    Spawn(SpawnModalState),
    Rename {
        target: AgentIdentitySummary,
        input: String,
        confirming: bool,
        self_rename: bool,
    },
    RenameTeam {
        old: String,
        input: String,
        confirming: bool,
    },
    Reset {
        target: AgentIdentitySummary,
        confirm: String,
        blocked: bool,
    },
    Leave {
        team: String,
        agent: String,
        confirm: String,
    },
    JoinForce {
        team: String,
        agent: String,
        agent_type: String,
        project: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentOperation {
    Spawn {
        team: String,
        agent_type: String,
        name: String,
    },
    Join {
        team: String,
        agent: String,
        agent_type: String,
        project: String,
    },
    JoinForce {
        team: String,
        agent: String,
        agent_type: String,
        project: String,
    },
    Rename {
        team: String,
        old: String,
        new: String,
        self_rename: bool,
    },
    RenameTeam {
        old: String,
        new: String,
    },
    Reset {
        project: String,
        agent_type: String,
        agent: String,
    },
    Leave {
        team: String,
        agent: String,
    },
}

pub fn validate_team_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("team name is required".to_owned());
    }
    if matches!(name, "." | "..") {
        return Err("'.' and '..' are forbidden".to_owned());
    }
    if name.starts_with('-') {
        return Err("team name cannot start with '-'".to_owned());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("team name cannot contain '/' or '\\'".to_owned());
    }
    if name.chars().any(char::is_control) {
        return Err("team name cannot contain control characters".to_owned());
    }
    Ok(())
}

pub fn validate_agent_name(name: &str, agent_type: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("agent name is required".to_owned());
    }
    if matches!(name, "." | "..") {
        return Err("'.' and '..' are forbidden".to_owned());
    }
    if name.starts_with('-') {
        return Err("agent name cannot start with '-'".to_owned());
    }
    if name
        .chars()
        .any(|character| character.is_control() || ".\\/\"[]".contains(character))
    {
        return Err("agent name contains a forbidden character".to_owned());
    }

    if name == agent_type {
        return Ok(());
    }
    let Some(role) = name
        .strip_prefix(agent_type)
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return Err(format!("use {agent_type}[-role]"));
    };
    if role.is_empty()
        || !role
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        || !role.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
    {
        return Err(format!("use {agent_type}[-role] with lowercase letters"));
    }
    if role.chars().all(|character| character.is_ascii_digit()) {
        return Err("numbered roles are forbidden; use a role such as -review".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_agent_name;

    #[test]
    fn agent_name_validation_enforces_type_role_and_blacklist() {
        assert!(validate_agent_name("codex-worker", "codex").is_ok());
        assert!(validate_agent_name("claude", "claude-code").is_err());
        assert!(validate_agent_name("claude-code-2", "claude-code").is_err());
        assert!(validate_agent_name("claude-code-review", "claude-code").is_ok());
        assert!(validate_agent_name("a.b", "codex").is_err());
    }
}
