use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub network_enabled: bool,
    pub command: CommandPolicy,
    pub max_runtime_secs: u64,
    pub max_output_bytes: u64,
    pub workspace_path: PathBuf,
    /// Whether the guest drops each workload process to the unprivileged `agent`
    /// user before exec (ADR 0002). Defaults to `true` — the secure posture for
    /// untrusted sandboxes. Trusted provisioning contexts (the image builder)
    /// set it `false` so commands run with the guest agent's privileges (root),
    /// which they require to write `/etc`, install packages, etc.
    pub drop_privileges: bool,
}

/// Ordered levels of the `command` capability axis. Higher levels grant
/// strictly more authority. See [ADR 0002](../../../docs/adr/0002-policy-modes-and-runtime-mode-switching.md).
///
/// The derived ordering relies on declaration order: `None < ReadOnly < Edit < Yolo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommandLevel {
    None,
    ReadOnly,
    Edit,
    Yolo,
}

impl CommandLevel {
    /// Parse a wire-form level name (`"none"`, `"read_only"`, `"edit"`, `"yolo"`).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::None),
            "read_only" => Some(Self::ReadOnly),
            "edit" => Some(Self::Edit),
            "yolo" => Some(Self::Yolo),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ReadOnly => "read_only",
            Self::Edit => "edit",
            Self::Yolo => "yolo",
        }
    }
}

/// The `command` capability axis: curated command sets per level, the
/// boot-default active level, and the escalation ceiling (`max`). A live VM may
/// move its active level up to `max` via `set_mode`, never past it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPolicy {
    pub default: CommandLevel,
    pub max: CommandLevel,
    pub read_only: HashSet<String>,
    pub edit: HashSet<String>,
}

impl CommandPolicy {
    /// Whether `command` may launch when the active level is `level`. Levels are
    /// cumulative: `edit` includes every `read_only` command, and `yolo` allows
    /// any executable.
    pub fn allows(&self, level: CommandLevel, command: &str) -> bool {
        match level {
            CommandLevel::None => false,
            CommandLevel::ReadOnly => self.read_only.contains(command),
            CommandLevel::Edit => self.read_only.contains(command) || self.edit.contains(command),
            CommandLevel::Yolo => true,
        }
    }
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
    /// Legacy flat allowlist. Mutually exclusive with `command`.
    #[serde(default)]
    allowed_commands: Option<Vec<String>>,
    /// Capability-lattice command axis. Mutually exclusive with `allowed_commands`.
    #[serde(default)]
    command: Option<RawCommandPolicy>,
    max_runtime_secs: u64,
    max_output_bytes: u64,
    workspace_path: PathBuf,
    /// See [`Policy::drop_privileges`]. Defaults to `true` (drop) when omitted.
    #[serde(default = "default_drop_privileges")]
    drop_privileges: bool,
}

fn default_drop_privileges() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommandPolicy {
    default: String,
    max: String,
    #[serde(default)]
    read_only: Vec<String>,
    #[serde(default)]
    edit: Vec<String>,
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

        let command = build_command_policy(raw.allowed_commands, raw.command)?;

        Ok(Self {
            network_enabled: raw.network_enabled,
            command,
            max_runtime_secs: raw.max_runtime_secs,
            max_output_bytes: raw.max_output_bytes,
            workspace_path: raw.workspace_path,
            drop_privileges: raw.drop_privileges,
        })
    }
}

/// Build the command axis from the two mutually exclusive config shapes. A
/// legacy flat `allowed_commands` list maps to a fixed `edit` level with no
/// escalation room (`default == max == edit`), preserving its original meaning.
fn build_command_policy(
    legacy: Option<Vec<String>>,
    modern: Option<RawCommandPolicy>,
) -> Result<CommandPolicy, PolicyError> {
    match (legacy, modern) {
        (Some(_), Some(_)) => Err(PolicyError::Invalid(
            "set either allowed_commands or [policy.command], not both".to_string(),
        )),
        (None, None) => Err(PolicyError::Invalid(
            "policy must set allowed_commands or [policy.command]".to_string(),
        )),
        (Some(commands), None) => Ok(CommandPolicy {
            default: CommandLevel::Edit,
            max: CommandLevel::Edit,
            read_only: HashSet::new(),
            edit: validate_command_set(commands, "allowed_commands")?,
        }),
        (None, Some(raw)) => {
            let default = CommandLevel::parse(&raw.default).ok_or_else(|| {
                PolicyError::Invalid(format!("unknown command level '{}'", raw.default))
            })?;
            let max = CommandLevel::parse(&raw.max).ok_or_else(|| {
                PolicyError::Invalid(format!("unknown command level '{}'", raw.max))
            })?;
            if default > max {
                return Err(PolicyError::Invalid(
                    "command default level must not exceed max level".to_string(),
                ));
            }
            Ok(CommandPolicy {
                default,
                max,
                read_only: validate_command_set(raw.read_only, "command.read_only")?,
                edit: validate_command_set(raw.edit, "command.edit")?,
            })
        }
    }
}

fn validate_command_set(
    commands: Vec<String>,
    field: &str,
) -> Result<HashSet<String>, PolicyError> {
    let mut set = HashSet::new();
    for command in commands {
        validate_command_name(&command)?;
        if !set.insert(command.clone()) {
            return Err(PolicyError::Invalid(format!(
                "duplicate command '{command}' in {field}"
            )));
        }
    }
    Ok(set)
}

fn validate_command_name(command: &str) -> Result<(), PolicyError> {
    if command.is_empty() {
        return Err(PolicyError::Invalid(
            "command entries must be non-empty".to_string(),
        ));
    }

    let invalid = command
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '/' | '\\' | '|' | '&' | ';' | '<' | '>'));
    if invalid {
        return Err(PolicyError::Invalid(format!(
            "command '{command}' must be an executable name"
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

    const MODES_POLICY: &str = r#"
[policy]
network_enabled = false
max_runtime_secs = 60
max_output_bytes = 1048576
workspace_path = "/workspace"

[policy.command]
default = "read_only"
max = "yolo"
read_only = ["ls", "cat"]
edit = ["sed", "tee"]
"#;

    #[test]
    fn loads_valid_legacy_policy() {
        let policy = Policy::from_toml_str(VALID_POLICY).unwrap();

        assert!(!policy.network_enabled);
        // Legacy allowlists map to a fixed `edit` level with no escalation room.
        assert_eq!(policy.command.default, CommandLevel::Edit);
        assert_eq!(policy.command.max, CommandLevel::Edit);
        assert!(policy.command.allows(CommandLevel::Edit, "cargo"));
        assert_eq!(policy.max_runtime_secs, 60);
        assert_eq!(policy.workspace_path, PathBuf::from("/workspace"));
    }

    #[test]
    fn loads_command_modes_policy() {
        let policy = Policy::from_toml_str(MODES_POLICY).unwrap();

        assert_eq!(policy.command.default, CommandLevel::ReadOnly);
        assert_eq!(policy.command.max, CommandLevel::Yolo);
        // read_only only sees its own set.
        assert!(policy.command.allows(CommandLevel::ReadOnly, "ls"));
        assert!(!policy.command.allows(CommandLevel::ReadOnly, "sed"));
        // edit is cumulative over read_only.
        assert!(policy.command.allows(CommandLevel::Edit, "ls"));
        assert!(policy.command.allows(CommandLevel::Edit, "sed"));
        // yolo allows anything.
        assert!(policy.command.allows(CommandLevel::Yolo, "anything"));
        // none allows nothing.
        assert!(!policy.command.allows(CommandLevel::None, "ls"));
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

        assert!(err.contains("duplicate command"));
    }

    #[test]
    fn rejects_shell_snippet_commands() {
        let input = VALID_POLICY.replace("[\"cargo\", \"git\"]", "[\"cargo test\"]");

        let err = Policy::from_toml_str(&input).unwrap_err().to_string();

        assert!(err.contains("must be an executable name"));
    }

    #[test]
    fn rejects_both_command_shapes() {
        let input = MODES_POLICY.replace(
            "max_output_bytes = 1048576",
            "max_output_bytes = 1048576\nallowed_commands = [\"git\"]",
        );

        let err = Policy::from_toml_str(&input).unwrap_err().to_string();

        assert!(err.contains("not both"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_missing_command_config() {
        let input = VALID_POLICY.replace("allowed_commands = [\"cargo\", \"git\"]\n", "");

        let err = Policy::from_toml_str(&input).unwrap_err().to_string();

        assert!(err.contains("must set allowed_commands or [policy.command]"));
    }

    #[test]
    fn rejects_default_above_max() {
        let input = MODES_POLICY.replace("max = \"yolo\"", "max = \"read_only\"");
        let input = input.replace("default = \"read_only\"", "default = \"edit\"");

        let err = Policy::from_toml_str(&input).unwrap_err().to_string();

        assert!(err.contains("default level must not exceed max level"));
    }

    #[test]
    fn rejects_unknown_command_level() {
        let input = MODES_POLICY.replace("max = \"yolo\"", "max = \"superuser\"");

        let err = Policy::from_toml_str(&input).unwrap_err().to_string();

        assert!(err.contains("unknown command level 'superuser'"));
    }
}
