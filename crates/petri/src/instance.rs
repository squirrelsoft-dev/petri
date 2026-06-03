use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::{PetriError, Result};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    Configured,
    Starting,
    Ready,
    RunningDispatch,
    Idle,
    Stopping,
    Stopped,
    Failed,
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
            state: LifecycleState::Configured,
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
