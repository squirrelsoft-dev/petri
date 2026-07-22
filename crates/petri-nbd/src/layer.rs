//! Layer representations: immutable read-only lower layers and the writable
//! scratch overlay.
//!
//! A *block* is the fixed-size unit of composition (64 KiB by default, carried
//! explicitly in [`Geometry`]). A layer only knows how to answer "do you have
//! block N, and if so what are its bytes?" — read resolution across a stack
//! lives in [`crate::stack::LayeredDisk`].

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::stack::Geometry;

/// Content-addressed identity of a sealed layer: a 32-byte SHA-256 digest over
/// the layer's canonical content (see design §8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LayerId([u8; 32]);

impl LayerId {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            // write! formats in place; push_str(&format!(..)) would allocate a
            // throwaway String per byte, 32 of them for every id rendered.
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    /// Parse a 64-char lowercase/uppercase hex string into a `LayerId`.
    /// Returns `None` if the string is not exactly 32 hex-encoded bytes.
    pub fn from_hex(hex: &str) -> Option<Self> {
        if hex.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Self(bytes))
    }
}

impl fmt::Display for LayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// One immutable, read-only layer. Shareable across many concurrent VMs; never
/// mutated after construction.
///
/// Milestone 1 supports a single representation: a raw disk file (e.g. an
/// existing Petri `root.img`) where every in-bounds block is considered
/// populated. Sealed packed-blob layers are added in Milestone 4.
pub struct ImmutableLayer {
    geometry: Geometry,
    kind: ImmutableKind,
    /// Present only for sealed layers (identity + provenance).
    sealed: Option<SealedMeta>,
}

struct SealedMeta {
    content_id: LayerId,
    parents: Vec<LayerId>,
}

enum ImmutableKind {
    /// A contiguous raw disk image; block N lives at `N * block_size`.
    RawBase { file: File, file_blocks: u64 },
    /// A sealed packed layer: live blocks stored densely in the file's block
    /// region (offset 0), addressed by an explicit `block -> offset` index.
    Sealed {
        file: File,
        index: BTreeMap<u64, u64>,
    },
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
        let bs = u64::from(geometry.block_size);
        // Round up: a partial trailing block still counts as present.
        let file_blocks = len.div_ceil(bs);
        Ok(Self {
            geometry,
            kind: ImmutableKind::RawBase { file, file_blocks },
            sealed: None,
        })
    }

    /// Reopen a previously sealed layer from its single self-describing file.
    ///
    /// The file is laid out as `[ packed blocks | metadata blob | fixed footer ]`;
    /// the trailing footer locates and checksums the metadata, so the layer
    /// carries its own provenance with no external sidecar (see `ScratchLayer::seal`).
    pub fn open_sealed(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).open(path)?;
        let meta = read_footer_meta(&file)?;
        let geometry = Geometry::new(meta.virtual_size, meta.block_size)?;
        Ok(Self {
            geometry,
            kind: ImmutableKind::Sealed {
                file,
                index: meta.index,
            },
            sealed: Some(SealedMeta {
                content_id: meta.content_id,
                parents: meta.parents,
            }),
        })
    }

    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// Content ID, for sealed layers only.
    pub fn content_id(&self) -> Option<LayerId> {
        self.sealed.as_ref().map(|m| m.content_id)
    }

    /// Parent layer IDs recorded at seal time (empty for a raw base).
    pub fn parent_ids(&self) -> &[LayerId] {
        self.sealed
            .as_ref()
            .map_or(&[][..], |m| m.parents.as_slice())
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
                let bs = u64::from(self.geometry.block_size);
                let offset = block * bs;
                // A trailing partial block reads short; zero-fill the remainder.
                out.fill(0);
                read_exact_at_short(file, offset, out)?;
                Ok(true)
            }
            ImmutableKind::Sealed { file, index } => match index.get(&block) {
                Some(&offset) => {
                    read_exact_at(file, offset, out)?;
                    Ok(true)
                }
                None => Ok(false),
            },
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

    /// Seal this overlay into an immutable, self-describing layer file at `path`.
    ///
    /// The output is a single portable file laid out as
    /// `[ packed block payloads | metadata blob | fixed footer ]`: live blocks
    /// are compacted densely in block-number order (dropping the append-log
    /// garbage from overwrites), then the metadata (geometry, content ID,
    /// parents, and the block index) and a fixed-size footer are appended. The
    /// footer at the tail carries a magic, the metadata length, and a CRC so the
    /// file reopens with no external sidecar — copy it anywhere and it stays
    /// self-describing. A stable content ID is computed over the canonical
    /// content (§8.1); `parents` records the immutable layers this overlay sat
    /// on top of, bottom-first.
    ///
    /// Takes `&self` (a read-only snapshot of the live overlay): the scratch
    /// remains usable afterward, so an overlay can be sealed without tearing
    /// down the stack that is currently serving it.
    pub fn seal(&self, path: &Path, parents: &[LayerId]) -> io::Result<ImmutableLayer> {
        // The on-disk format stores the parent count as u16 and the reader
        // parses it as one, so a longer chain cannot round-trip. Reject it
        // here rather than letting the cast wrap silently: a wrapped count
        // would corrupt the metadata *and* feed a wrong length into the
        // content hash below, which would silently collide content ids.
        let parent_count = u16::try_from(parents.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "layer has {} parents, exceeding the {} the format can store",
                    parents.len(),
                    u16::MAX
                ),
            )
        })?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bs = self.geometry.block_size as usize;
        let out = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        // Canonical content pre-image: domain + geometry + parents + per block.
        let mut content = Sha256::new();
        content.update(b"petri-nbd-layer-v1\0");
        content.update(self.geometry.block_size.to_le_bytes());
        content.update(self.geometry.virtual_size.to_le_bytes());
        content.update(parent_count.to_le_bytes());
        for parent in parents {
            content.update(parent.as_bytes());
        }

        let mut new_index = BTreeMap::new();
        let mut buf = vec![0u8; bs];
        let mut offset = 0u64;
        // BTreeMap iterates in ascending block-number order — the canonical order.
        for (&block, &scratch_offset) in &self.index {
            read_exact_at(&self.data, scratch_offset, &mut buf)?;
            write_all_at(&out, offset, &buf)?;
            new_index.insert(block, offset);
            content.update(block.to_le_bytes());
            content.update(sha256(&buf));
            offset += bs as u64;
        }
        let content_id = LayerId(content.finalize().into());

        // Append the metadata blob and the fixed footer right after the packed
        // block region (which ends at `offset`), then fsync the whole file.
        let meta = encode_meta(
            &self.geometry,
            &content_id,
            parents,
            parent_count,
            &new_index,
        );
        write_all_at(&out, offset, &meta)?;
        let footer = encode_footer(meta.len() as u64, crc32(&meta));
        write_all_at(&out, offset + meta.len() as u64, &footer)?;
        out.sync_all()?;

        let file = OpenOptions::new().read(true).open(path)?;
        Ok(ImmutableLayer {
            geometry: self.geometry,
            kind: ImmutableKind::Sealed {
                file,
                index: new_index,
            },
            sealed: Some(SealedMeta {
                content_id,
                parents: parents.to_vec(),
            }),
        })
    }
}

/// Decoded layer metadata: the blob embedded immediately ahead of the footer.
struct Meta {
    block_size: u32,
    virtual_size: u64,
    content_id: LayerId,
    parents: Vec<LayerId>,
    index: BTreeMap<u64, u64>,
}

/// Footer magic; the trailing byte is the on-disk format version of the
/// self-describing layer file (`\x02` = embedded footer, superseding the v1
/// `layer.meta` sidecar).
const FOOTER_MAGIC: &[u8; 8] = b"PNBDLYR\x02";
/// Fixed footer: `metadata_len u64 | metadata_crc u32 | version u16 | flags u16 | magic [8]`.
/// Fixed trailer size in bytes. Declared as usize because it sizes array
/// literals, with a u64 twin for the file-offset arithmetic, so neither use
/// needs a cast. The assertion keeps the two in step.
const FOOTER_SIZE: usize = 24;
const FOOTER_SIZE_U64: u64 = 24;
const _: () = assert!(FOOTER_SIZE == 24 && FOOTER_SIZE_U64 == 24);
const FORMAT_VERSION: u16 = 2;
const HASH_ALGO_SHA256: u8 = 1;

/// Encode the metadata blob (everything but the trailing fixed footer).
/// `parent_count` is `parents.len()` already validated to fit in u16 by the
/// caller, so the count written here and the one hashed into the content id
/// cannot disagree.
fn encode_meta(
    geometry: &Geometry,
    content_id: &LayerId,
    parents: &[LayerId],
    parent_count: u16,
    index: &BTreeMap<u64, u64>,
) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(&geometry.block_size.to_le_bytes());
    m.extend_from_slice(&geometry.virtual_size.to_le_bytes());
    m.push(HASH_ALGO_SHA256);
    m.extend_from_slice(content_id.as_bytes());
    debug_assert_eq!(usize::from(parent_count), parents.len());
    m.extend_from_slice(&parent_count.to_le_bytes());
    for parent in parents {
        m.extend_from_slice(parent.as_bytes());
    }
    m.extend_from_slice(&(index.len() as u64).to_le_bytes());
    for (&block, &offset) in index {
        m.extend_from_slice(&block.to_le_bytes());
        m.extend_from_slice(&offset.to_le_bytes());
    }
    m
}

/// Encode the fixed-size footer that terminates a sealed layer file.
fn encode_footer(metadata_len: u64, metadata_crc: u32) -> [u8; FOOTER_SIZE] {
    let mut f = [0u8; FOOTER_SIZE];
    f[0..8].copy_from_slice(&metadata_len.to_le_bytes());
    f[8..12].copy_from_slice(&metadata_crc.to_le_bytes());
    f[12..14].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    f[14..16].copy_from_slice(&0u16.to_le_bytes()); // flags (reserved)
    f[16..24].copy_from_slice(FOOTER_MAGIC);
    f
}

/// Read and validate a sealed layer's trailing footer, then its metadata blob.
fn read_footer_meta(file: &File) -> io::Result<Meta> {
    let file_len = file.metadata()?.len();
    if file_len < FOOTER_SIZE_U64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file shorter than layer footer",
        ));
    }
    let mut footer = [0u8; FOOTER_SIZE];
    read_exact_at(file, file_len - FOOTER_SIZE_U64, &mut footer)?;
    if &footer[16..24] != FOOTER_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad layer footer magic",
        ));
    }
    let version = u16::from_le_bytes(footer[12..14].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported layer format version {version}"),
        ));
    }
    let metadata_len = u64::from_le_bytes(footer[0..8].try_into().unwrap());
    let metadata_crc = u32::from_le_bytes(footer[8..12].try_into().unwrap());
    let meta_offset = (file_len - FOOTER_SIZE_U64)
        .checked_sub(metadata_len)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "layer metadata length overflows file",
            )
        })?;
    // metadata_len was just bounded by checked_sub against the real file
    // length, so it cannot exceed the file; combined with the 64-bit target
    // assertion in lib.rs the conversion is lossless.
    #[allow(clippy::cast_possible_truncation)]
    let metadata_len_usize = metadata_len as usize;
    let mut bytes = vec![0u8; metadata_len_usize];
    read_exact_at(file, meta_offset, &mut bytes)?;
    if crc32(&bytes) != metadata_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "layer metadata checksum mismatch",
        ));
    }
    decode_meta(&bytes)
}

/// Decode a metadata blob produced by [`encode_meta`].
fn decode_meta(bytes: &[u8]) -> io::Result<Meta> {
    let mut r = MetaReader { bytes, pos: 0 };
    let block_size = r.u32()?;
    let virtual_size = r.u64()?;
    let hash_algo = r.u8()?;
    if hash_algo != HASH_ALGO_SHA256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported layer hash algorithm {hash_algo}"),
        ));
    }
    let content_id = LayerId(r.array32()?);
    let parent_count = r.u16()? as usize;
    let mut parents = Vec::with_capacity(parent_count);
    for _ in 0..parent_count {
        parents.push(LayerId(r.array32()?));
    }
    let index_count = r.u64()?;
    let mut index = BTreeMap::new();
    for _ in 0..index_count {
        let block = r.u64()?;
        let offset = r.u64()?;
        index.insert(block, offset);
    }
    Ok(Meta {
        block_size,
        virtual_size,
        content_id,
        parents,
        index,
    })
}

/// CRC-32 (IEEE 802.3, polynomial `0xEDB88320`) over `bytes` — detects
/// corruption of the embedded metadata blob without pulling in a dependency.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Minimal little-endian byte reader with bounds checks for the metadata blob.
struct MetaReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> MetaReader<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.bytes.len());
        match end {
            Some(end) => {
                let slice = &self.bytes[self.pos..end];
                self.pos = end;
                Ok(slice)
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated layer.meta",
            )),
        }
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn array32(&mut self) -> io::Result<[u8; 32]> {
        Ok(self.take(32)?.try_into().unwrap())
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

#[cfg(test)]
// Tests build offsets from the small `BS` block-size constant, so the widening
// conversions here are scaffolding rather than production arithmetic.
#[allow(clippy::cast_lossless)]
mod tests {
    use super::*;
    use crate::stack::LayeredDisk;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const BS: u32 = 16;
    const VSIZE: u64 = 160; // 10 blocks

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("petri-nbd-seal-{}-{}", std::process::id(), n));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
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

    /// Base image covering 5 blocks with byte value `0xB0 + block`.
    fn write_base(dir: &TestDir) -> PathBuf {
        let path = dir.path("base.raw");
        let mut bytes = Vec::new();
        for block in 0u8..5 {
            bytes.extend(std::iter::repeat_n(0xB0 + block, BS as usize));
        }
        fs::write(&path, &bytes).unwrap();
        path
    }

    fn block(byte: u8) -> Vec<u8> {
        vec![byte; BS as usize]
    }

    #[test]
    fn seal_then_compose_shadows_base() {
        let dir = TestDir::new();
        let base_path = write_base(&dir);

        let mut scratch = ScratchLayer::create(&dir.path("scratch.data"), geometry()).unwrap();
        scratch.write_block(1, &block(0xAA)).unwrap(); // shadows base 0xB1
        scratch.write_block(7, &block(0x77)).unwrap(); // over a hole (base has 5 blocks)
        let sealed = scratch.seal(&dir.path("sealed"), &[]).unwrap();

        let base = ImmutableLayer::open_raw_base(&base_path, geometry()).unwrap();
        let fresh = ScratchLayer::create(&dir.path("fresh.data"), geometry()).unwrap();
        let mut disk = LayeredDisk::new(vec![base, sealed], fresh).unwrap();

        let mut buf = block(0);
        disk.read_at(BS as u64, &mut buf).unwrap();
        assert_eq!(buf, block(0xAA)); // sealed shadows base
        disk.read_at(7 * BS as u64, &mut buf).unwrap();
        assert_eq!(buf, block(0x77)); // sealed over former hole
        disk.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, block(0xB0)); // base shows through
        disk.read_at(2 * BS as u64, &mut buf).unwrap();
        assert_eq!(buf, block(0xB2)); // base shows through
    }

    fn seal_with(dir: &TestDir, name: &str, byte: u8, parents: &[LayerId]) -> ImmutableLayer {
        let mut scratch =
            ScratchLayer::create(&dir.path(&format!("{name}.data")), geometry()).unwrap();
        scratch.write_block(1, &block(byte)).unwrap();
        scratch.seal(&dir.path(name), parents).unwrap()
    }

    #[test]
    fn seal_rejects_more_parents_than_the_format_can_store() {
        // The format stores the parent count as u16 and the reader parses it
        // as one. Before this was validated, `parents.len() as u16` wrapped:
        // 65536 parents encoded as 0, producing metadata that could not
        // round-trip and a content id hashed over the wrong count.
        let dir = TestDir::new();
        let mut scratch = ScratchLayer::create(&dir.path("overflow.data"), geometry()).unwrap();
        scratch.write_block(1, &block(0x5A)).unwrap();

        let too_many = vec![LayerId([0u8; 32]); usize::from(u16::MAX) + 1];
        match scratch.seal(&dir.path("overflow"), &too_many) {
            Ok(_) => panic!("seal must reject a parent chain the format cannot encode"),
            Err(err) => {
                assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
                assert!(
                    err.to_string().contains("exceeding"),
                    "unexpected error: {err}"
                );
            }
        }

        // The boundary case must still succeed.
        let at_limit = vec![LayerId([0u8; 32]); usize::from(u16::MAX)];
        assert!(
            scratch.seal(&dir.path("at-limit"), &at_limit).is_ok(),
            "u16::MAX parents must still seal"
        );
    }

    #[test]
    fn content_id_is_stable_and_distinct() {
        let dir = TestDir::new();
        let a = seal_with(&dir, "a", 0xAA, &[]).content_id().unwrap();
        let b = seal_with(&dir, "b", 0xAA, &[]).content_id().unwrap();
        assert_eq!(a, b, "identical content + parents must yield the same ID");

        let with_parent = seal_with(&dir, "c", 0xAA, &[a]).content_id().unwrap();
        assert_ne!(a, with_parent, "different parents must change the ID");

        let different = seal_with(&dir, "d", 0xBB, &[]).content_id().unwrap();
        assert_ne!(a, different, "different content must change the ID");
    }

    #[test]
    fn open_sealed_reloads_identity_and_data() {
        let dir = TestDir::new();
        let base_path = write_base(&dir);

        let parent = seal_with(&dir, "parent", 0x10, &[]).content_id().unwrap();
        let mut scratch = ScratchLayer::create(&dir.path("scratch.data"), geometry()).unwrap();
        scratch.write_block(1, &block(0xAA)).unwrap();
        let original = scratch.seal(&dir.path("sealed"), &[parent]).unwrap();
        let original_id = original.content_id().unwrap();
        drop(original);

        let reopened = ImmutableLayer::open_sealed(&dir.path("sealed")).unwrap();
        assert_eq!(reopened.content_id(), Some(original_id));
        assert_eq!(reopened.parent_ids(), &[parent]);

        // Data is intact after reload.
        let base = ImmutableLayer::open_raw_base(&base_path, geometry()).unwrap();
        let fresh = ScratchLayer::create(&dir.path("fresh.data"), geometry()).unwrap();
        let mut disk = LayeredDisk::new(vec![base, reopened], fresh).unwrap();
        let mut buf = block(0);
        disk.read_at(BS as u64, &mut buf).unwrap();
        assert_eq!(buf, block(0xAA));
    }

    #[test]
    fn sealed_layer_is_a_single_self_describing_file() {
        let dir = TestDir::new();
        let mut scratch = ScratchLayer::create(&dir.path("scratch.data"), geometry()).unwrap();
        scratch.write_block(1, &block(0xAA)).unwrap();
        let layer_path = dir.path("layer.bin");
        let sealed = scratch.seal(&layer_path, &[]).unwrap();
        let id = sealed.content_id().unwrap();
        drop(sealed);

        // The output is one regular file — no directory, no `.meta` sidecar.
        assert!(layer_path.is_file());
        assert!(!dir.path("layer.bin.meta").exists());
        assert!(!dir.path("layer.meta").exists());

        // It is portable: copied anywhere, it reopens with identity + data intact.
        let moved = dir.path("moved-elsewhere.bin");
        fs::copy(&layer_path, &moved).unwrap();
        let reopened = ImmutableLayer::open_sealed(&moved).unwrap();
        assert_eq!(reopened.content_id(), Some(id));
        let mut buf = block(0);
        assert!(reopened.read_block(1, &mut buf).unwrap());
        assert_eq!(buf, block(0xAA));
    }

    #[test]
    fn corrupt_footer_and_metadata_are_rejected() {
        let dir = TestDir::new();
        let mut scratch = ScratchLayer::create(&dir.path("scratch.data"), geometry()).unwrap();
        scratch.write_block(1, &block(0xAA)).unwrap();
        let layer_path = dir.path("layer.bin");
        scratch.seal(&layer_path, &[]).unwrap();

        // Truncating below the footer is rejected.
        let truncated = dir.path("truncated.bin");
        let bytes = fs::read(&layer_path).unwrap();
        fs::write(&truncated, &bytes[..(FOOTER_SIZE - 1)]).unwrap();
        assert!(ImmutableLayer::open_sealed(&truncated).is_err());

        // Clobbering the magic (last 8 bytes) is rejected.
        let bad_magic = dir.path("bad-magic.bin");
        let mut m = bytes.clone();
        let n = m.len();
        m[n - 1] ^= 0xFF;
        fs::write(&bad_magic, &m).unwrap();
        assert!(ImmutableLayer::open_sealed(&bad_magic).is_err());

        // Flipping a metadata byte trips the CRC check.
        let bad_meta = dir.path("bad-meta.bin");
        let mut m = bytes;
        let meta_byte = m.len() - FOOTER_SIZE - 1; // last byte of the metadata blob
        m[meta_byte] ^= 0xFF;
        fs::write(&bad_meta, &m).unwrap();
        assert!(ImmutableLayer::open_sealed(&bad_meta).is_err());
    }

    #[test]
    fn layer_id_hex_roundtrips_length() {
        let dir = TestDir::new();
        let id = seal_with(&dir, "x", 0x42, &[]).content_id().unwrap();
        assert_eq!(id.to_hex().len(), 64);
        assert_eq!(id.to_hex(), format!("{id}"));
    }
}
