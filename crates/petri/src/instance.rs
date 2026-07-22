use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{PetriError, Result};

pub const GUEST_WORKSPACE_PATH: &str = "/workspace";

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct InstanceId(String);

impl InstanceId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let valid = !value.is_empty()
            && value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'));

        if valid {
            Ok(Self(value))
        } else {
            Err(PetriError::InvalidInstanceId(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Provisioning,
    #[serde(alias = "starting")]
    Booting,
    Ready,
    RunningDispatch,
    Stopping,
    Failed,
    #[serde(alias = "idle", alias = "stopped")]
    TornDown,
}

impl LifecycleState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Booting => "booting",
            Self::Ready => "ready",
            Self::RunningDispatch => "running_dispatch",
            Self::Stopping => "stopping",
            Self::Failed => "failed",
            Self::TornDown => "torn_down",
        }
    }

    pub fn transition(self, to: Self, operation: &'static str) -> Result<Self> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(PetriError::LifecycleTransition {
                operation,
                from: self.as_str(),
                to: to.as_str(),
            })
        }
    }

    pub fn can_transition_to(self, to: Self) -> bool {
        matches!(
            (self, to),
            (
                Self::Provisioning,
                Self::Booting | Self::Failed | Self::TornDown
            ) | (Self::Booting | Self::RunningDispatch, Self::Ready)
                | (Self::Booting | Self::Ready, Self::Stopping)
                | (
                    Self::Booting | Self::Ready | Self::RunningDispatch | Self::Stopping,
                    Self::Failed
                )
                | (
                    Self::Booting | Self::Ready | Self::Stopping | Self::Failed,
                    Self::TornDown
                )
                | (Self::Ready, Self::RunningDispatch)
        )
    }

    pub fn is_running(self) -> bool {
        matches!(self, Self::Ready | Self::RunningDispatch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceConfig {
    pub id: InstanceId,
    pub backend: String,
    pub image: Option<PathBuf>,
    pub workspace: PathBuf,
    pub policy: PathBuf,
    /// Free-form key/value metadata persisted with the instance so it can be
    /// surfaced and filtered by `sandbox list --metadata`.
    pub metadata: BTreeMap<String, String>,
    /// When set, boot directly from an NBD-served layer (Linux direct boot)
    /// instead of an image bundle. Used by `sandbox create --base`, where the
    /// disk is a per-sandbox scratch served by a detached NBD daemon.
    pub direct_boot: Option<DirectBoot>,
}

/// Linux direct-boot parameters for a sandbox booting from a petri layer: the
/// NBD boot-disk URL plus the kernel/initrd/cmdline extracted from the layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectBoot {
    pub nbd_url: String,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub cmdline: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceContract {
    pub host_path: PathBuf,
    pub guest_path: PathBuf,
    pub persists_after_teardown: bool,
}

impl InstanceConfig {
    pub fn new(
        id: InstanceId,
        backend: impl Into<String>,
        workspace: impl Into<PathBuf>,
        policy: impl Into<PathBuf>,
    ) -> Self {
        Self {
            id,
            backend: backend.into(),
            image: None,
            workspace: workspace.into(),
            policy: policy.into(),
            metadata: BTreeMap::new(),
            direct_boot: None,
        }
    }

    pub fn with_image(mut self, image: impl Into<PathBuf>) -> Self {
        self.image = Some(image.into());
        self
    }

    pub fn with_direct_boot(mut self, direct_boot: DirectBoot) -> Self {
        self.direct_boot = Some(direct_boot);
        self
    }

    pub fn with_metadata(mut self, metadata: BTreeMap<String, String>) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn validate(&self) -> Result<()> {
        self.workspace_contract()?;
        validate_path("policy", &self.policy)?;

        if self.backend.is_empty() {
            return Err(PetriError::InvalidConfig(
                "backend must be non-empty".to_string(),
            ));
        }

        if let Some(image) = &self.image {
            validate_path("image", image)?;
        }

        Ok(())
    }

    pub fn workspace_contract(&self) -> Result<WorkspaceContract> {
        Ok(WorkspaceContract {
            host_path: validate_workspace_path(&self.workspace)?,
            guest_path: PathBuf::from(GUEST_WORKSPACE_PATH),
            persists_after_teardown: true,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InstanceHandle {
    pub id: InstanceId,
    pub backend: String,
    pub state: LifecycleState,
    /// Free-form metadata supplied at creation. Empty when none was set.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl InstanceHandle {
    pub fn configured(config: &InstanceConfig) -> Self {
        Self {
            id: config.id.clone(),
            backend: config.backend.clone(),
            state: LifecycleState::Provisioning,
            metadata: config.metadata.clone(),
        }
    }

    /// Whether every `filter` entry is present in this handle's metadata with a
    /// matching value. An empty filter matches every handle.
    pub fn matches_metadata(&self, filter: &BTreeMap<String, String>) -> bool {
        filter
            .iter()
            .all(|(key, value)| self.metadata.get(key) == Some(value))
    }
}

fn validate_path(name: &str, path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        Err(PetriError::InvalidConfig(format!(
            "{name} path is required"
        )))
    } else {
        Ok(())
    }
}

fn validate_workspace_path(path: &Path) -> Result<PathBuf> {
    validate_path("workspace", path)?;

    if !path.is_absolute() {
        return Err(PetriError::InvalidConfig(
            "workspace path must be absolute".to_string(),
        ));
    }

    let metadata = fs::metadata(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            PetriError::InvalidConfig(format!("workspace path does not exist: {}", path.display()))
        } else {
            PetriError::Io {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;

    if !metadata.is_dir() {
        return Err(PetriError::InvalidConfig(format!(
            "workspace path must be a directory: {}",
            path.display()
        )));
    }

    fs::canonicalize(path).map_err(|source| PetriError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "petri-instance-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn config_with_workspace(workspace: PathBuf) -> InstanceConfig {
        InstanceConfig::new(
            InstanceId::new("dev-1").unwrap(),
            "macos",
            workspace,
            PathBuf::from("policy.toml"),
        )
    }

    #[test]
    fn lifecycle_accepts_expected_vm_flow() {
        let state = LifecycleState::Provisioning
            .transition(LifecycleState::Booting, "create")
            .unwrap()
            .transition(LifecycleState::Ready, "create")
            .unwrap()
            .transition(LifecycleState::RunningDispatch, "dispatch")
            .unwrap()
            .transition(LifecycleState::Ready, "dispatch")
            .unwrap()
            .transition(LifecycleState::Stopping, "stop")
            .unwrap()
            .transition(LifecycleState::TornDown, "stop")
            .unwrap();

        assert_eq!(state, LifecycleState::TornDown);
    }

    #[test]
    fn lifecycle_rejects_invalid_transitions() {
        let err = LifecycleState::Provisioning
            .transition(LifecycleState::RunningDispatch, "dispatch")
            .unwrap_err();

        assert_eq!(
            err.to_string(),
            "invalid lifecycle transition during dispatch: provisioning -> running_dispatch"
        );
        assert!(!LifecycleState::TornDown.can_transition_to(LifecycleState::Ready));
        assert!(!LifecycleState::RunningDispatch.can_transition_to(LifecycleState::Stopping));
    }

    #[test]
    fn workspace_contract_maps_absolute_host_directory_to_guest_workspace() {
        let workspace = temp_dir("workspace-contract");
        let config = config_with_workspace(workspace.clone());

        let contract = config.workspace_contract().unwrap();

        assert_eq!(contract.host_path, fs::canonicalize(workspace).unwrap());
        assert_eq!(contract.guest_path, PathBuf::from(GUEST_WORKSPACE_PATH));
        assert!(contract.persists_after_teardown);
    }

    #[test]
    fn workspace_contract_rejects_relative_workspace() {
        let err = config_with_workspace(PathBuf::from("relative-workspace"))
            .workspace_contract()
            .unwrap_err()
            .to_string();

        assert_eq!(
            err,
            "invalid instance config: workspace path must be absolute"
        );
    }

    #[test]
    fn workspace_contract_rejects_missing_workspace() {
        let missing =
            std::env::temp_dir().join(format!("petri-missing-workspace-{}", std::process::id()));
        let err = config_with_workspace(missing.clone())
            .workspace_contract()
            .unwrap_err()
            .to_string();

        assert_eq!(
            err,
            format!(
                "invalid instance config: workspace path does not exist: {}",
                missing.display()
            )
        );
    }

    #[test]
    fn workspace_contract_rejects_file_workspace() {
        let dir = temp_dir("workspace-file");
        let workspace = dir.join("not-a-directory");
        fs::write(&workspace, b"file").unwrap();

        let err = config_with_workspace(workspace.clone())
            .workspace_contract()
            .unwrap_err()
            .to_string();

        assert_eq!(
            err,
            format!(
                "invalid instance config: workspace path must be a directory: {}",
                workspace.display()
            )
        );
    }
}
