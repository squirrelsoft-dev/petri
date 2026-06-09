//! `petri-nbd`: layered block storage for Petri VM images.
//!
//! The crate composes immutable, shareable lower layers with one writable
//! per-run scratch overlay into a single addressable virtual disk
//! ([`LayeredDisk`]). Reads resolve top-down (first populated block wins,
//! holes read as zeroes) and writes always land in the scratch overlay; lower
//! layers are never mutated. See `docs/petri-nbd-layered-storage.md` for the
//! full design and invariants.
//!
//! Milestone 1 (this module set) covers the local block stack. The NBD server
//! (`server`/`protocol`) and sealing/store (`store`) land in later milestones.

mod layer;
mod protocol;
mod server;
mod stack;

pub use layer::{ImmutableLayer, ScratchLayer};
pub use server::{BindMode, NbdHandle, NbdServer, ServeOpts};
pub use stack::{Geometry, LayeredDisk};
