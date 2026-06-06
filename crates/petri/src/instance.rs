use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::{PetriError, Result};

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
            (Self::Provisioning, Self::Booting)
                | (Self::Provisioning, Self::Failed)
                | (Self::Provisioning, Self::TornDown)
                | (Self::Booting, Self::Ready)
                | (Self::Booting, Self::Stopping)
                | (Self::Booting, Self::Failed)
                | (Self::Booting, Self::TornDown)
                | (Self::Ready, Self::RunningDispatch)
                | (Self::Ready, Self::Stopping)
                | (Self::Ready, Self::Failed)
                | (Self::Ready, Self::TornDown)
                | (Self::RunningDispatch, Self::Ready)
                | (Self::RunningDispatch, Self::Failed)
                | (Self::Stopping, Self::Failed)
                | (Self::Stopping, Self::TornDown)
                | (Self::Failed, Self::TornDown)
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
        }
    }

    pub fn with_image(mut self, image: impl Into<PathBuf>) -> Self {
        self.image = Some(image.into());
        self
    }

    pub fn validate(&self) -> Result<()> {
        validate_path("workspace", &self.workspace)?;
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
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InstanceHandle {
    pub id: InstanceId,
    pub backend: String,
    pub state: LifecycleState,
}

impl InstanceHandle {
    pub fn configured(config: &InstanceConfig) -> Self {
        Self {
            id: config.id.clone(),
            backend: config.backend.clone(),
            state: LifecycleState::Provisioning,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
