//! Layer representations: immutable read-only lower layers and the writable
//! scratch overlay.
//!
//! A *block* is the fixed-size unit of composition (64 KiB by default, carried
//! explicitly in [`Geometry`]). A layer only knows how to answer "do you have
//! block N, and if so what are its bytes?" — read resolution across a stack
//! lives in [`crate::stack::LayeredDisk`].

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::stack::Geometry;

/// One immutable, read-only layer. Shareable across many concurrent VMs; never
/// mutated after construction.
///
/// Milestone 1 supports a single representation: a raw disk file (e.g. an
/// existing Petri `root.img`) where every in-bounds block is considered
/// populated. Sealed packed-blob layers are added in Milestone 4.
pub struct ImmutableLayer {
    geometry: Geometry,
    kind: ImmutableKind,
}

enum ImmutableKind {
    /// A contiguous raw disk image; block N lives at `N * block_size`.
    RawBase { file: File, file_blocks: u64 },
}

impl ImmutableLayer {
    /// Open a raw disk image as an immutable base layer for the given geometry.
    ///
    /// The file may be shorter than the virtual size (a sparse/truncated
    /// image); blocks past the end of the file are treated as unpopulated and
    /// fall through to lower layers (or read as zeroes at the bottom).
    pub fn open_raw_base(path: &Path, geometry: Geometry) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        let len = file.metadata()?.len();
        let bs = geometry.block_size as u64;
        // Round up: a partial trailing block still counts as present.
        let file_blocks = len.div_ceil(bs);
        Ok(Self {
            geometry,
            kind: ImmutableKind::RawBase { file, file_blocks },
        })
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// Read block `block` into `out` (which must be exactly one block long).
    /// Returns `true` if this layer populates the block (and filled `out`),
    /// `false` if the block is a hole here.
    pub(crate) fn read_block(&self, block: u64, out: &mut [u8]) -> io::Result<bool> {
        debug_assert_eq!(out.len(), self.geometry.block_size as usize);
        match &self.kind {
            ImmutableKind::RawBase { file, file_blocks } => {
                if block >= *file_blocks {
                    return Ok(false);
                }
                let bs = self.geometry.block_size as u64;
                let offset = block * bs;
                // A trailing partial block reads short; zero-fill the remainder.
                out.fill(0);
                read_exact_at_short(file, offset, out)?;
                Ok(true)
            }
        }
    }
}

/// The writable per-run scratch overlay: the top of every stack.
///
/// Backed by an append-log packed blob (`layer.data`) plus an in-memory block
/// index. Writing a block appends its payload and points the index at the new
/// offset; earlier copies of an overwritten block remain as garbage until the
/// layer is sealed/compacted. This append-only data layout keeps a torn write
/// from damaging a previously durable block (see design §7).
pub struct ScratchLayer {
    geometry: Geometry,
    data: File,
    /// virtual block number -> byte offset of its payload in `data`.
    index: BTreeMap<u64, u64>,
    /// Append cursor: current length of `data` in bytes.
    tail: u64,
}

impl ScratchLayer {
    /// Create a fresh empty scratch overlay backed by `data_path`.
    pub fn create(data_path: &Path, geometry: Geometry) -> io::Result<Self> {
        let data = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(data_path)?;
        Ok(Self {
            geometry,
            data,
            index: BTreeMap::new(),
            tail: 0,
        })
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// Number of distinct populated blocks (for tests/observability).
    pub fn populated_blocks(&self) -> usize {
        self.index.len()
    }

    /// Read block `block` into `out` (exactly one block long). Returns `true`
    /// if the scratch populates the block.
    pub(crate) fn read_block(&self, block: u64, out: &mut [u8]) -> io::Result<bool> {
        debug_assert_eq!(out.len(), self.geometry.block_size as usize);
        match self.index.get(&block) {
            Some(&offset) => {
                read_exact_at(&self.data, offset, out)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Write a full block's worth of bytes into the scratch overlay.
    pub(crate) fn write_block(&mut self, block: u64, payload: &[u8]) -> io::Result<()> {
        debug_assert_eq!(payload.len(), self.geometry.block_size as usize);
        let offset = self.tail;
        write_all_at(&self.data, offset, payload)?;
        self.tail += payload.len() as u64;
        self.index.insert(block, offset);
        Ok(())
    }

    /// Drop a block back to "hole" (trim/discard), so reads fall through to
    /// lower layers again.
    pub(crate) fn forget_block(&mut self, block: u64) {
        self.index.remove(&block);
    }

    /// Make prior scratch writes durable.
    pub fn flush(&mut self) -> io::Result<()> {
        self.data.flush()?;
        self.data.sync_all()
    }
}

// --- positioned IO helpers (portable; no platform pread/pwrite dependency) ---

fn read_exact_at(mut file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(buf)
}

/// Like [`read_exact_at`] but tolerates a short read at EOF, leaving the
/// untouched tail of `buf` as-is (callers pre-zero it).
fn read_exact_at_short(mut file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(())
}

fn write_all_at(mut file: &File, offset: u64, buf: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(buf)
}
