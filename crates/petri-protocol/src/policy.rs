use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub network_enabled: bool,
    pub network: NetworkPolicy,
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

/// Ordered levels of the `network` capability axis. Higher levels grant strictly
/// more egress. See [ADR 0002](../../../docs/adr/0002-policy-modes-and-runtime-mode-switching.md).
///
/// The derived ordering relies on declaration order: `None < Allowlist < Full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NetworkLevel {
    None,
    Allowlist,
    Full,
}

impl NetworkLevel {
    /// Parse a wire-form level name (`"none"`, `"allowlist"`, `"full"`).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::None),
            "allowlist" => Some(Self::Allowlist),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Allowlist => "allowlist",
            Self::Full => "full",
        }
    }
}

/// The `network` capability axis: the boot-default active level, the escalation
/// ceiling (`max`), and the destinations permitted at the `allowlist` level. A
/// live VM may move its active level up to `max` via `set_mode`, never past it.
/// Enforced in-guest via nftables. The axis is layered on the immutable
/// `network_enabled` boot gate: with no device attached it is pinned at `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkPolicy {
    pub default: NetworkLevel,
    pub max: NetworkLevel,
    /// Destinations allowed at the `allowlist` level: IPs, CIDR blocks, and
    /// domain names. Classification (IP/CIDR vs domain) and enforcement happen
    /// in the guest's nftables/DNS layer.
    pub allowlist: Vec<String>,
}

impl NetworkPolicy {
    /// The axis with no egress, used when `network_enabled = false` (no device).
    pub fn disabled() -> Self {
        Self {
            default: NetworkLevel::None,
            max: NetworkLevel::None,
            allowlist: Vec::new(),
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
    /// Capability-lattice network axis. Requires `network_enabled = true`. When
    /// omitted, network is governed solely by the `network_enabled` boolean.
    #[serde(default)]
    network: Option<RawNetworkPolicy>,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetworkPolicy {
    default: String,
    max: String,
    #[serde(default)]
    allowlist: Vec<String>,
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
        let network = build_network_policy(raw.network_enabled, raw.network)?;

        Ok(Self {
            network_enabled: raw.network_enabled,
            network,
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

/// Build the network axis. When `[policy.network]` is omitted, the axis is
/// derived from the immutable `network_enabled` boolean (the legacy shape):
/// `true` means full egress, `false` means none — both fixed (`default == max`).
/// When present it requires `network_enabled = true`, since the level can only
/// filter egress over an attached device.
fn build_network_policy(
    network_enabled: bool,
    modern: Option<RawNetworkPolicy>,
) -> Result<NetworkPolicy, PolicyError> {
    match modern {
        None => Ok(if network_enabled {
            NetworkPolicy {
                default: NetworkLevel::Full,
                max: NetworkLevel::Full,
                allowlist: Vec::new(),
            }
        } else {
            NetworkPolicy::disabled()
        }),
        Some(raw) => {
            if !network_enabled {
                return Err(PolicyError::Invalid(
                    "[policy.network] requires network_enabled = true".to_string(),
                ));
            }
            let default = NetworkLevel::parse(&raw.default).ok_or_else(|| {
                PolicyError::Invalid(format!("unknown network level '{}'", raw.default))
            })?;
            let max = NetworkLevel::parse(&raw.max).ok_or_else(|| {
                PolicyError::Invalid(format!("unknown network level '{}'", raw.max))
            })?;
            if default > max {
                return Err(PolicyError::Invalid(
                    "network default level must not exceed max level".to_string(),
                ));
            }
            let allowlist = validate_allowlist(raw.allowlist)?;
            Ok(NetworkPolicy {
                default,
                max,
                allowlist,
            })
        }
    }
}

/// Light validation of allowlist entries: non-empty, no whitespace or shell
/// metacharacters. Strict IP/CIDR/domain classification happens in the guest's
/// enforcement layer, which must parse them anyway.
fn validate_allowlist(entries: Vec<String>) -> Result<Vec<String>, PolicyError> {
    let mut seen = HashSet::new();
    for entry in &entries {
        if entry.is_empty() {
            return Err(PolicyError::Invalid(
                "network allowlist entries must be non-empty".to_string(),
            ));
        }
        if entry.chars().any(char::is_whitespace) {
            return Err(PolicyError::Invalid(format!(
                "network allowlist entry '{entry}' must not contain whitespace"
            )));
        }
        if !seen.insert(entry.clone()) {
            return Err(PolicyError::Invalid(format!(
                "duplicate network allowlist entry '{entry}'"
            )));
        }
    }
    Ok(entries)
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

    const NETWORK_POLICY: &str = r#"
[policy]
network_enabled = true
allowed_commands = ["curl"]
max_runtime_secs = 60
max_output_bytes = 1048576
workspace_path = "/workspace"

[policy.network]
default = "none"
max = "allowlist"
allowlist = ["1.1.1.1", "8.8.8.0/24", "*.crates.io"]
"#;

    #[test]
    fn defaults_network_axis_from_boolean_when_block_absent() {
        // network_enabled = false -> axis pinned at none.
        let off = Policy::from_toml_str(VALID_POLICY).unwrap();
        assert_eq!(off.network.default, NetworkLevel::None);
        assert_eq!(off.network.max, NetworkLevel::None);

        // network_enabled = true with no [policy.network] -> full egress, fixed.
        let on = Policy::from_toml_str(
            &VALID_POLICY.replace("network_enabled = false", "network_enabled = true"),
        )
        .unwrap();
        assert_eq!(on.network.default, NetworkLevel::Full);
        assert_eq!(on.network.max, NetworkLevel::Full);
    }

    #[test]
    fn loads_network_axis() {
        let policy = Policy::from_toml_str(NETWORK_POLICY).unwrap();
        assert_eq!(policy.network.default, NetworkLevel::None);
        assert_eq!(policy.network.max, NetworkLevel::Allowlist);
        assert_eq!(
            policy.network.allowlist,
            vec!["1.1.1.1", "8.8.8.0/24", "*.crates.io"]
        );
    }

    #[test]
    fn rejects_network_block_without_enabled() {
        let input = NETWORK_POLICY.replace("network_enabled = true", "network_enabled = false");

        let err = Policy::from_toml_str(&input).unwrap_err().to_string();

        assert!(
            err.contains("requires network_enabled = true"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_network_default_above_max() {
        let input = NETWORK_POLICY.replace("default = \"none\"", "default = \"full\"");

        let err = Policy::from_toml_str(&input).unwrap_err().to_string();

        assert!(
            err.contains("network default level must not exceed max level"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_unknown_network_level() {
        let input = NETWORK_POLICY.replace("max = \"allowlist\"", "max = \"wide_open\"");

        let err = Policy::from_toml_str(&input).unwrap_err().to_string();

        assert!(
            err.contains("unknown network level 'wide_open'"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_duplicate_allowlist_entry() {
        let input = NETWORK_POLICY.replace(
            "[\"1.1.1.1\", \"8.8.8.0/24\", \"*.crates.io\"]",
            "[\"1.1.1.1\", \"1.1.1.1\"]",
        );

        let err = Policy::from_toml_str(&input).unwrap_err().to_string();

        assert!(
            err.contains("duplicate network allowlist entry"),
            "got: {err}"
        );
    }
}
