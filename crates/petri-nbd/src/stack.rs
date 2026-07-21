//! [`LayeredDisk`]: a composed, addressable virtual disk built from read-only
//! lower layers plus one writable scratch overlay.
//!
//! Read resolution is top-down per block (scratch first, then lower layers from
//! the top down); the first layer that populates a block wins, and a block
//! populated by no layer reads as zeroes. Writes always land in the scratch
//! overlay — lower layers are never mutated.

use std::io::{self, Error, ErrorKind};
use std::path::Path;

use crate::layer::{ImmutableLayer, LayerId, ScratchLayer};

/// Virtual disk geometry shared by every layer in a stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub virtual_size: u64,
    pub block_size: u32,
}

impl Geometry {
    /// Default v0 stored block size: 64 KiB (see design §3.3).
    pub const DEFAULT_BLOCK_SIZE: u32 = 64 * 1024;

    /// Build a geometry, validating that `block_size` is non-zero and divides
    /// `virtual_size` evenly (v0 uses whole fixed-size blocks).
    pub fn new(virtual_size: u64, block_size: u32) -> io::Result<Self> {
        if block_size == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "block_size must be non-zero",
            ));
        }
        if !virtual_size.is_multiple_of(u64::from(block_size)) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "virtual_size must be a multiple of block_size",
            ));
        }
        Ok(Self {
            virtual_size,
            block_size,
        })
    }

    /// Number of blocks spanning the virtual disk.
    pub fn block_count(&self) -> u64 {
        self.virtual_size / u64::from(self.block_size)
    }

    /// Byte offset of `pos` within the block that contains it.
    ///
    /// The result is `pos % block_size`, so it is always strictly less than
    /// `block_size` — a u32. It is therefore representable in usize on every
    /// supported target and the conversion cannot truncate. Centralizing it
    /// here keeps that argument in one place instead of at each call site.
    #[allow(clippy::cast_possible_truncation)]
    fn block_offset(&self, pos: u64) -> usize {
        (pos % u64::from(self.block_size)) as usize
    }

    /// `len` is a u64 byte count because callers work in disk offsets; taking
    /// usize here only forced a cast back to u64 on the first line.
    fn check_range(&self, offset: u64, len: u64) -> io::Result<()> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "offset + len overflows"))?;
        if end > self.virtual_size {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "access past end of virtual disk",
            ));
        }
        Ok(())
    }
}

/// A composed virtual disk: lower immutable layers (bottom-first) plus one
/// writable scratch overlay on top.
pub struct LayeredDisk {
    geometry: Geometry,
    /// Bottom-first; the last entry is the highest-priority immutable layer.
    lower: Vec<ImmutableLayer>,
    scratch: ScratchLayer,
    /// One block of scratch space for read-modify-write and resolution.
    block_buf: Vec<u8>,
}

impl LayeredDisk {
    /// Compose a stack from `lower` layers (bottom-first) and a writable
    /// `scratch` overlay. Every layer must share the same geometry.
    pub fn new(lower: Vec<ImmutableLayer>, scratch: ScratchLayer) -> io::Result<Self> {
        let geometry = scratch.geometry();
        for layer in &lower {
            if layer.geometry() != geometry {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "every layer in a stack must share the same geometry",
                ));
            }
        }
        let block_buf = vec![0u8; geometry.block_size as usize];
        Ok(Self {
            geometry,
            lower,
            scratch,
            block_buf,
        })
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    pub fn virtual_size(&self) -> u64 {
        self.geometry.virtual_size
    }

    pub fn block_size(&self) -> u32 {
        self.geometry.block_size
    }

    /// Resolve a full block into `out` (which must be one block long): the
    /// highest layer that populates it wins; a hole reads as zeroes.
    fn resolve_block(&self, block: u64, out: &mut [u8]) -> io::Result<()> {
        if self.scratch.read_block(block, out)? {
            return Ok(());
        }
        for layer in self.lower.iter().rev() {
            if layer.read_block(block, out)? {
                return Ok(());
            }
        }
        out.fill(0);
        Ok(())
    }

    /// Read `buf.len()` bytes starting at `offset`. Supports arbitrary
    /// (unaligned) offset and length; holes are zero-filled.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.geometry.check_range(offset, buf.len() as u64)?;
        let bs = self.geometry.block_size as usize;
        let mut done = 0usize;
        let mut pos = offset;
        while done < buf.len() {
            let block = pos / bs as u64;
            let in_block = self.geometry.block_offset(pos);
            let n = (bs - in_block).min(buf.len() - done);
            // Borrow the scratch block buffer without aliasing `self`.
            let mut tmp = std::mem::take(&mut self.block_buf);
            let res = self.resolve_block(block, &mut tmp);
            buf[done..done + n].copy_from_slice(&tmp[in_block..in_block + n]);
            self.block_buf = tmp;
            res?;
            done += n;
            pos += n as u64;
        }
        Ok(())
    }

    /// Write `buf` at `offset` into the scratch overlay only. Sub-block writes
    /// do read-modify-write against the resolved stack.
    pub fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.geometry.check_range(offset, buf.len() as u64)?;
        let bs = self.geometry.block_size as usize;
        let mut done = 0usize;
        let mut pos = offset;
        while done < buf.len() {
            let block = pos / bs as u64;
            let in_block = self.geometry.block_offset(pos);
            let n = (bs - in_block).min(buf.len() - done);
            if in_block == 0 && n == bs {
                self.scratch.write_block(block, &buf[done..done + bs])?;
            } else {
                let mut tmp = std::mem::take(&mut self.block_buf);
                let res = self.resolve_block(block, &mut tmp);
                if res.is_ok() {
                    tmp[in_block..in_block + n].copy_from_slice(&buf[done..done + n]);
                    self.scratch.write_block(block, &tmp)?;
                }
                self.block_buf = tmp;
                res?;
            }
            done += n;
            pos += n as u64;
        }
        Ok(())
    }

    /// Zero a region by writing zeroes into the scratch overlay. Full blocks
    /// are written as zero blocks; partial blocks do read-modify-write.
    pub fn write_zeroes(&mut self, offset: u64, len: u64) -> io::Result<()> {
        self.geometry.check_range(offset, len)?;
        let bs = self.geometry.block_size as usize;
        let mut remaining = len;
        let mut pos = offset;
        while remaining > 0 {
            let block = pos / bs as u64;
            let in_block = self.geometry.block_offset(pos);
            let n = (bs - in_block).min(usize::try_from(remaining).unwrap_or(usize::MAX));
            let mut tmp = std::mem::take(&mut self.block_buf);
            let res = if in_block == 0 && n == bs {
                tmp.fill(0);
                self.scratch.write_block(block, &tmp)
            } else {
                self.resolve_block(block, &mut tmp).and_then(|()| {
                    tmp[in_block..in_block + n].fill(0);
                    self.scratch.write_block(block, &tmp)
                })
            };
            self.block_buf = tmp;
            res?;
            remaining -= n as u64;
            pos += n as u64;
        }
        Ok(())
    }

    /// Discard a region from the scratch overlay. Fully-covered blocks drop
    /// back to "hole" and fall through to lower layers again; partially-covered
    /// blocks are zeroed in scratch (their covered range becomes undefined →
    /// zero) so the untouched remainder still shadows lower layers.
    pub fn trim(&mut self, offset: u64, len: u64) -> io::Result<()> {
        self.geometry.check_range(offset, len)?;
        let bs = self.geometry.block_size as usize;
        let mut remaining = len;
        let mut pos = offset;
        while remaining > 0 {
            let block = pos / bs as u64;
            let in_block = self.geometry.block_offset(pos);
            let n = (bs - in_block).min(usize::try_from(remaining).unwrap_or(usize::MAX));
            if in_block == 0 && n == bs {
                self.scratch.forget_block(block);
            } else {
                let mut tmp = std::mem::take(&mut self.block_buf);
                let res = self.resolve_block(block, &mut tmp).and_then(|()| {
                    tmp[in_block..in_block + n].fill(0);
                    self.scratch.write_block(block, &tmp)
                });
                self.block_buf = tmp;
                res?;
            }
            remaining -= n as u64;
            pos += n as u64;
        }
        Ok(())
    }

    /// Make prior scratch writes durable.
    pub fn flush(&mut self) -> io::Result<()> {
        self.scratch.flush()
    }

    /// Seal this stack's scratch overlay into a self-describing immutable layer
    /// file at `path` (design §9 step 7). The scratch remains usable afterward.
    pub fn seal_scratch(&self, path: &Path, parents: &[LayerId]) -> io::Result<ImmutableLayer> {
        self.scratch.seal(path, parents)
    }
}

#[cfg(test)]
// Tests build offsets from the small `BS` block-size constant, so the widening
// conversions here are scaffolding rather than production arithmetic.
#[allow(clippy::cast_lossless)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Small geometry keeps block math easy to reason about in tests.
    const BS: u32 = 16;
    const VSIZE: u64 = 160; // 10 blocks

    /// Unique temp dir without relying on Date/random (unavailable in some
    /// harness contexts): process id + a monotonic counter.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("petri-nbd-{}-{}", std::process::id(), n));
            fs::create_dir_all(&dir).unwrap();
            TestDir(dir)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn geometry() -> Geometry {
        Geometry::new(VSIZE, BS).unwrap()
    }

    /// Base image covering the first 5 blocks (80 bytes) with byte value
    /// `0xB0 + block`; the remaining 5 blocks are absent (holes).
    fn write_base(dir: &TestDir) -> PathBuf {
        let path = dir.path("base.raw");
        let mut bytes = Vec::new();
        for block in 0u8..5 {
            bytes.extend(std::iter::repeat_n(0xB0 + block, BS as usize));
        }
        fs::write(&path, &bytes).unwrap();
        path
    }

    fn disk(dir: &TestDir) -> LayeredDisk {
        let base = ImmutableLayer::open_raw_base(&write_base(dir), geometry()).unwrap();
        let scratch = ScratchLayer::create(&dir.path("scratch.data"), geometry()).unwrap();
        LayeredDisk::new(vec![base], scratch).unwrap()
    }

    #[test]
    fn geometry_rejects_unaligned_size() {
        assert!(Geometry::new(100, 64).is_err());
        assert!(Geometry::new(128, 0).is_err());
        assert_eq!(geometry().block_count(), 10);
    }

    #[test]
    fn aligned_read_sees_base() {
        let dir = TestDir::new();
        let mut d = disk(&dir);
        let mut buf = [0u8; BS as usize];
        d.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0xB0; BS as usize]);
        d.read_at(4 * BS as u64, &mut buf).unwrap();
        assert_eq!(buf, [0xB4; BS as usize]);
    }

    #[test]
    fn missing_regions_read_as_zeroes() {
        let dir = TestDir::new();
        let mut d = disk(&dir);
        // Block 5 onward is past the base file → holes → zeroes.
        let mut buf = [0xFFu8; BS as usize];
        d.read_at(5 * BS as u64, &mut buf).unwrap();
        assert_eq!(buf, [0u8; BS as usize]);
    }

    #[test]
    fn overlay_shadows_base() {
        let dir = TestDir::new();
        let mut d = disk(&dir);
        // Overwrite block 1 via scratch; base still has 0xB1 underneath.
        d.write_at(BS as u64, &[0xAA; BS as usize]).unwrap();
        let mut buf = [0u8; BS as usize];
        d.read_at(BS as u64, &mut buf).unwrap();
        assert_eq!(buf, [0xAA; BS as usize]);
        // Neighbouring blocks unaffected.
        d.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0xB0; BS as usize]);
        d.read_at(2 * BS as u64, &mut buf).unwrap();
        assert_eq!(buf, [0xB2; BS as usize]);
    }

    #[test]
    fn unaligned_write_does_read_modify_write() {
        let dir = TestDir::new();
        let mut d = disk(&dir);
        // Write 4 bytes straddling the block-2/block-3 boundary.
        let off = 3 * BS as u64 - 2; // 2 bytes in block 2, 2 in block 3
        d.write_at(off, &[1, 2, 3, 4]).unwrap();
        let mut buf = [0u8; 8];
        d.read_at(off - 2, &mut buf).unwrap();
        // First two bytes are untouched base (block 2 = 0xB2), then payload,
        // then untouched base (block 3 = 0xB3).
        assert_eq!(buf, [0xB2, 0xB2, 1, 2, 3, 4, 0xB3, 0xB3]);
    }

    #[test]
    fn unaligned_read_spans_blocks() {
        let dir = TestDir::new();
        let mut d = disk(&dir);
        let mut buf = [0u8; 4];
        // Straddle block-0/block-1 boundary.
        d.read_at(BS as u64 - 2, &mut buf).unwrap();
        assert_eq!(buf, [0xB0, 0xB0, 0xB1, 0xB1]);
    }

    #[test]
    fn flush_persists_overlay_state() {
        let dir = TestDir::new();
        let mut d = disk(&dir);
        d.write_at(7 * BS as u64, &[0x77; BS as usize]).unwrap();
        d.flush().unwrap();
        let mut buf = [0u8; BS as usize];
        d.read_at(7 * BS as u64, &mut buf).unwrap();
        assert_eq!(buf, [0x77; BS as usize]);
        // Scratch data file actually grew on disk.
        let data_len = fs::metadata(dir.path("scratch.data")).unwrap().len();
        assert!(data_len >= BS as u64);
    }

    #[test]
    fn write_zeroes_shadows_base() {
        let dir = TestDir::new();
        let mut d = disk(&dir);
        d.write_zeroes(0, BS as u64).unwrap();
        let mut buf = [0xFFu8; BS as usize];
        d.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, [0u8; BS as usize]); // shadows base 0xB0
    }

    #[test]
    fn trim_full_block_falls_through_to_base() {
        let dir = TestDir::new();
        let mut d = disk(&dir);
        d.write_at(2 * BS as u64, &[0xCC; BS as usize]).unwrap();
        let mut buf = [0u8; BS as usize];
        d.read_at(2 * BS as u64, &mut buf).unwrap();
        assert_eq!(buf, [0xCC; BS as usize]);
        // Trim the whole block → base shows through again.
        d.trim(2 * BS as u64, BS as u64).unwrap();
        d.read_at(2 * BS as u64, &mut buf).unwrap();
        assert_eq!(buf, [0xB2; BS as usize]);
    }

    #[test]
    fn out_of_bounds_access_is_rejected() {
        let dir = TestDir::new();
        let mut d = disk(&dir);
        let mut buf = [0u8; BS as usize];
        assert!(d.read_at(VSIZE, &mut buf).is_err());
        assert!(d.read_at(VSIZE - 1, &mut buf).is_err());
        assert!(d.write_at(VSIZE - 1, &buf).is_err());
    }

    #[test]
    fn geometry_mismatch_is_rejected() {
        let dir = TestDir::new();
        let base = ImmutableLayer::open_raw_base(&write_base(&dir), geometry()).unwrap();
        let other = Geometry::new(VSIZE, 32).unwrap();
        let scratch = ScratchLayer::create(&dir.path("s.data"), other).unwrap();
        assert!(LayeredDisk::new(vec![base], scratch).is_err());
    }
}
