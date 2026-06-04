//! Host-side API boundaries for Petri.
//!
//! This crate models the public lifecycle and dispatch surface before platform
//! VM backends are implemented. Backends plug in through [`HostBackend`].

pub mod backend;
pub mod cli;
pub mod dispatch;
pub mod error;
pub mod instance;

pub use backend::{HostBackend, MacosBackend, PetriBackend, StubBackend};
pub use dispatch::{DispatchRequest, DispatchResult, ErrorFrame, RequestLimits, Status};
pub use error::{PetriError, Result};
pub use instance::{InstanceConfig, InstanceHandle, InstanceId, LifecycleState};
