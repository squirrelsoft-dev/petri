use crate::dispatch::{DispatchRequest, DispatchResult};
use crate::error::{PetriError, Result};
use crate::instance::{InstanceConfig, InstanceHandle, InstanceId};

pub trait HostBackend {
    fn name(&self) -> &str;
    fn create(&self, config: InstanceConfig) -> Result<InstanceHandle>;
    fn dispatch(
        &self,
        instance_id: &InstanceId,
        request: DispatchRequest,
    ) -> Result<DispatchResult>;
    fn stop(&self, instance_id: &InstanceId) -> Result<()>;
    fn teardown(&self, instance_id: &InstanceId) -> Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StubBackend;

impl HostBackend for StubBackend {
    fn name(&self) -> &str {
        "stub"
    }

    fn create(&self, config: InstanceConfig) -> Result<InstanceHandle> {
        config.validate()?;
        Err(self.unavailable("instance creation"))
    }

    fn dispatch(
        &self,
        _instance_id: &InstanceId,
        _request: DispatchRequest,
    ) -> Result<DispatchResult> {
        Err(self.unavailable("dispatch"))
    }

    fn stop(&self, _instance_id: &InstanceId) -> Result<()> {
        Err(self.unavailable("stop"))
    }

    fn teardown(&self, _instance_id: &InstanceId) -> Result<()> {
        Err(self.unavailable("teardown"))
    }
}

impl StubBackend {
    fn unavailable(&self, operation: &'static str) -> PetriError {
        PetriError::BackendUnavailable {
            backend: self.name().to_string(),
            operation,
        }
    }
}
