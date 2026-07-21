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

// This crate converts u64 disk offsets and metadata lengths to usize in the
// block-resolution and layer-decoding paths. Those conversions are lossless
// only where usize is at least 64 bits wide. Rather than thread fallible
// conversions through the hot path, the assumption is asserted once here, at
// compile time — a 32-bit target fails to build instead of silently
// truncating an offset at runtime.
const _: () = assert!(
    usize::BITS >= u64::BITS,
    "petri-nbd requires a 64-bit target: u64 disk offsets are converted to usize"
);

mod layer;
mod protocol;
mod server;
mod stack;
mod store;

pub use layer::{ImmutableLayer, LayerId, ScratchLayer};
pub use server::{BindMode, NbdHandle, NbdServer, ServeOpts};
pub use stack::{Geometry, LayeredDisk};
pub use store::LayerStore;
