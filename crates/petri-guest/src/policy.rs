use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub network_enabled: bool,
    pub allowed_commands: HashSet<String>,
    pub max_runtime_secs: u64,
    pub max_output_bytes: u64,
    pub workspace_path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyDocument {
    policy: RawPolicy,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    network_enabled: bool,
    allowed_commands: Vec<String>,
    max_runtime_secs: u64,
    max_output_bytes: u64,
    workspace_path: PathBuf,
}

impl Policy {
    pub fn load(mut reader: impl Read) -> Result<Self, PolicyError> {
        let mut input = String::new();
        reader.read_to_string(&mut input)?;
        Self::from_toml_str(&input)
    }

    pub fn from_toml_str(input: &str) -> Result<Self, PolicyError> {
        let document: PolicyDocument = toml::from_str(input)?;
        Self::validate(document.policy)
    }

    pub fn allows_command(&self, command: &str) -> bool {
        self.allowed_commands.contains(command)
    }

    pub fn cwd_is_in_workspace(&self, cwd: &Path) -> bool {
        cwd.is_absolute() && cwd.starts_with(&self.workspace_path)
    }

    fn validate(raw: RawPolicy) -> Result<Self, PolicyError> {
        if raw.max_runtime_secs == 0 {
            return Err(PolicyError::Invalid(
                "max_runtime_secs must be positive".to_string(),
            ));
        }

        if raw.max_output_bytes == 0 {
            return Err(PolicyError::Invalid(
                "max_output_bytes must be positive".to_string(),
            ));
        }

        if !raw.workspace_path.is_absolute() {
            return Err(PolicyError::Invalid(
                "workspace_path must be absolute".to_string(),
            ));
        }

        let mut allowed_commands = HashSet::new();
        for command in raw.allowed_commands {
            validate_command_name(&command)?;
            if !allowed_commands.insert(command.clone()) {
                return Err(PolicyError::Invalid(format!(
                    "duplicate allowed command '{command}'"
                )));
            }
        }

        Ok(Self {
            network_enabled: raw.network_enabled,
            allowed_commands,
            max_runtime_secs: raw.max_runtime_secs,
            max_output_bytes: raw.max_output_bytes,
            workspace_path: raw.workspace_path,
        })
    }
}

fn validate_command_name(command: &str) -> Result<(), PolicyError> {
    if command.is_empty() {
        return Err(PolicyError::Invalid(
            "allowed command entries must be non-empty".to_string(),
        ));
    }

    let invalid = command
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '/' | '\\' | '|' | '&' | ';' | '<' | '>'));
    if invalid {
        return Err(PolicyError::Invalid(format!(
            "allowed command '{command}' must be an executable name"
        )));
    }

    Ok(())
}

#[derive(Debug)]
pub enum PolicyError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    Invalid(String),
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "failed to read policy: {err}"),
            Self::Parse(err) => write!(f, "failed to parse policy: {err}"),
            Self::Invalid(message) => write!(f, "invalid policy: {message}"),
        }
    }
}

impl std::error::Error for PolicyError {}

impl From<std::io::Error> for PolicyError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<toml::de::Error> for PolicyError {
    fn from(err: toml::de::Error) -> Self {
        Self::Parse(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_POLICY: &str = r#"
[policy]
network_enabled = false
allowed_commands = ["cargo", "git"]
max_runtime_secs = 60
max_output_bytes = 1048576
workspace_path = "/workspace"
"#;

    #[test]
    fn loads_valid_policy() {
        let policy = Policy::from_toml_str(VALID_POLICY).unwrap();

        assert!(!policy.network_enabled);
        assert!(policy.allows_command("cargo"));
        assert_eq!(policy.max_runtime_secs, 60);
        assert_eq!(policy.workspace_path, PathBuf::from("/workspace"));
    }

    #[test]
    fn rejects_relative_workspace() {
        let input = VALID_POLICY.replace("/workspace", "workspace");

        let err = Policy::from_toml_str(&input).unwrap_err().to_string();

        assert!(err.contains("workspace_path must be absolute"));
    }

    #[test]
    fn rejects_duplicate_commands() {
        let input = VALID_POLICY.replace("[\"cargo\", \"git\"]", "[\"cargo\", \"cargo\"]");

        let err = Policy::from_toml_str(&input).unwrap_err().to_string();

        assert!(err.contains("duplicate allowed command"));
    }

    #[test]
    fn rejects_shell_snippet_commands() {
        let input = VALID_POLICY.replace("[\"cargo\", \"git\"]", "[\"cargo test\"]");

        let err = Policy::from_toml_str(&input).unwrap_err().to_string();

        assert!(err.contains("must be an executable name"));
    }
}
