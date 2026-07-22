//! A content-addressed layer store with reference tracking and garbage
//! collection (design §5 `store.rs`, Milestone 5 "cache cleanup and layer
//! reference tracking").
//!
//! Each sealed layer is a single self-describing file at
//! `<root>/layers/<content-id-hex>` (packed blocks + embedded metadata footer).
//! Because the file name *is* the content ID, sealing identical content
//! twice is automatic dedupe: the second seal collapses onto the first. GC
//! keeps every layer reachable from a caller-supplied set of roots by following
//! recorded parent edges, and removes the rest.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::layer::{ImmutableLayer, LayerId, ScratchLayer};

/// A directory-backed store of immutable, content-addressed layers.
pub struct LayerStore {
    root: PathBuf,
}

impl LayerStore {
    /// Open (creating if needed) a store rooted at `root`.
    pub fn open(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root.join("layers"))?;
        fs::create_dir_all(root.join(".staging"))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn layers_dir(&self) -> PathBuf {
        self.root.join("layers")
    }

    fn layer_path(&self, id: &LayerId) -> PathBuf {
        self.layers_dir().join(id.to_hex())
    }

    /// Seal `scratch` into the store and return its content ID. If a layer with
    /// the same content already exists, the new copy is discarded (dedupe) and
    /// the existing ID is returned.
    pub fn seal_into(&self, scratch: ScratchLayer, parents: &[LayerId]) -> io::Result<LayerId> {
        // Seal to a private staging file first so the content-addressed move is
        // the commit point.
        let staging = self.root.join(".staging").join(unique_name());
        if staging.exists() {
            fs::remove_file(&staging)?;
        }
        let sealed = scratch.seal(&staging, parents)?;
        let id = sealed.content_id().expect("sealed layer has a content id");
        drop(sealed); // release the file handle on the staging path

        let dest = self.layer_path(&id);
        if dest.exists() {
            // Already stored with identical content — dedupe.
            fs::remove_file(&staging)?;
        } else {
            fs::rename(&staging, &dest)?;
        }
        Ok(id)
    }

    /// Adopt an already-sealed layer file (e.g. produced by
    /// [`crate::NbdHandle::seal_scratch`]) into the store under its content id.
    /// Dedupes: if a layer with the same content already exists, the incoming
    /// file is removed and the existing id returned. The source file is consumed
    /// (renamed into the store) on success.
    pub fn adopt_sealed(&self, sealed_path: &Path) -> io::Result<LayerId> {
        let layer = ImmutableLayer::open_sealed(sealed_path)?;
        let id = layer.content_id().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "adopted file is not a sealed layer",
            )
        })?;
        drop(layer); // release the handle before moving the file
        let dest = self.layer_path(&id);
        if dest.exists() {
            fs::remove_file(sealed_path)?;
        } else {
            fs::rename(sealed_path, &dest)?;
        }
        Ok(id)
    }

    /// Open a stored layer by ID.
    pub fn open_layer(&self, id: &LayerId) -> io::Result<ImmutableLayer> {
        let path = self.layer_path(id);
        if !path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("layer {id} not in store"),
            ));
        }
        ImmutableLayer::open_sealed(&path)
    }

    pub fn contains(&self, id: &LayerId) -> bool {
        self.layer_path(id).exists()
    }

    /// Remove a stored layer file by id. Idempotent: a no-op if the layer is
    /// already absent. The caller is responsible for not deleting a layer that
    /// is still referenced as a parent (the store does not track child edges).
    pub fn delete(&self, id: &LayerId) -> io::Result<()> {
        match fs::remove_file(self.layer_path(id)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// List the content IDs of every stored layer.
    pub fn list(&self) -> io::Result<Vec<LayerId>> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(self.layers_dir())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            if let Some(id) = name.to_str().and_then(LayerId::from_hex) {
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Read just the parent edges of a stored layer (cheap; opens the layer).
    fn parents_of(&self, id: &LayerId) -> io::Result<Vec<LayerId>> {
        Ok(self.open_layer(id)?.parent_ids().to_vec())
    }

    /// Garbage-collect: remove every stored layer not reachable from `roots`
    /// (following parent edges). Returns the removed IDs.
    pub fn gc(&self, roots: &[LayerId]) -> io::Result<Vec<LayerId>> {
        // Mark: everything reachable from the roots via parent edges.
        let mut reachable = BTreeSet::new();
        let mut stack: Vec<LayerId> = roots.to_vec();
        while let Some(id) = stack.pop() {
            if !self.contains(&id) || !reachable.insert(id) {
                continue;
            }
            for parent in self.parents_of(&id)? {
                stack.push(parent);
            }
        }

        // Sweep: remove anything stored but unreachable.
        let mut removed = Vec::new();
        for id in self.list()? {
            if !reachable.contains(&id) {
                fs::remove_file(self.layer_path(&id))?;
                removed.push(id);
            }
        }
        removed.sort();
        Ok(removed)
    }
}

/// A staging-directory name that is unique without relying on `Date`/random
/// (unavailable in some harness contexts): pid + a monotonic counter.
fn unique_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("seal-{}-{}", std::process::id(), n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::Geometry;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const BS: u32 = 16;
    const VSIZE: u64 = 160;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("petri-nbd-store-{}-{}", std::process::id(), n));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
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

    /// Seal a one-block scratch (block 1 = `byte`) into the store.
    fn seal_byte(
        store: &LayerStore,
        dir: &Path,
        tag: &str,
        byte: u8,
        parents: &[LayerId],
    ) -> LayerId {
        let mut scratch =
            ScratchLayer::create(&dir.join(format!("{tag}.data")), geometry()).unwrap();
        scratch.write_block(1, &vec![byte; BS as usize]).unwrap();
        store.seal_into(scratch, parents).unwrap()
    }

    #[test]
    fn seal_into_is_content_addressed_dedupe() {
        let dir = TestDir::new();
        let store = LayerStore::open(&dir.0.join("store")).unwrap();
        let a = seal_byte(&store, &dir.0, "a", 0xAA, &[]);
        let b = seal_byte(&store, &dir.0, "b", 0xAA, &[]);
        assert_eq!(a, b, "identical content yields the same ID");
        assert_eq!(
            store.list().unwrap().len(),
            1,
            "duplicate content is deduped to one stored layer"
        );
    }

    #[test]
    fn open_layer_roundtrips() {
        let dir = TestDir::new();
        let store = LayerStore::open(&dir.0.join("store")).unwrap();
        let id = seal_byte(&store, &dir.0, "x", 0x5A, &[]);
        let layer = store.open_layer(&id).unwrap();
        assert_eq!(layer.content_id(), Some(id));
        let mut buf = vec![0u8; BS as usize];
        assert!(layer.read_block(1, &mut buf).unwrap()); // pub(crate), same-crate test
        assert_eq!(buf, vec![0x5A; BS as usize]);
        assert!(!store.contains(&LayerId::from_hex(&"0".repeat(64)).unwrap()));
    }

    #[test]
    fn adopt_sealed_imports_and_dedupes() {
        let dir = TestDir::new();
        let store = LayerStore::open(&dir.0.join("store")).unwrap();

        // Seal a scratch to a loose path, then adopt it into the store.
        let mut scratch = ScratchLayer::create(&dir.0.join("loose.data"), geometry()).unwrap();
        scratch.write_block(1, &vec![0x5A; BS as usize]).unwrap();
        let loose = dir.0.join("loose.layer");
        let sealed = scratch.seal(&loose, &[]).unwrap();
        let expected = sealed.content_id().unwrap();
        drop(sealed);

        let id = store.adopt_sealed(&loose).unwrap();
        assert_eq!(id, expected);
        assert!(store.contains(&id));
        assert!(!loose.exists(), "adopt consumes the source file");

        // Adopting identical content again dedupes onto the existing layer.
        let mut scratch2 = ScratchLayer::create(&dir.0.join("loose2.data"), geometry()).unwrap();
        scratch2.write_block(1, &vec![0x5A; BS as usize]).unwrap();
        let loose2 = dir.0.join("loose2.layer");
        scratch2.seal(&loose2, &[]).unwrap();
        let id2 = store.adopt_sealed(&loose2).unwrap();
        assert_eq!(id2, id);
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(!loose2.exists());
    }

    #[test]
    fn delete_removes_layer_and_is_idempotent() {
        let dir = TestDir::new();
        let store = LayerStore::open(&dir.0.join("store")).unwrap();
        let id = seal_byte(&store, &dir.0, "x", 0x5A, &[]);
        assert!(store.contains(&id));
        store.delete(&id).unwrap();
        assert!(!store.contains(&id));
        // Deleting an absent layer is a no-op.
        store.delete(&id).unwrap();
    }

    #[test]
    fn gc_keeps_roots_and_ancestors_removes_the_rest() {
        let dir = TestDir::new();
        let store = LayerStore::open(&dir.0.join("store")).unwrap();

        // Chain: base <- mid <- tip ; plus an unrelated orphan.
        let base = seal_byte(&store, &dir.0, "base", 0x10, &[]);
        let mid = seal_byte(&store, &dir.0, "mid", 0x20, &[base]);
        let tip = seal_byte(&store, &dir.0, "tip", 0x30, &[mid]);
        let orphan = seal_byte(&store, &dir.0, "orphan", 0x40, &[]);
        assert_eq!(store.list().unwrap().len(), 4);

        // Root = tip; expect base+mid+tip kept, orphan removed.
        let removed = store.gc(&[tip]).unwrap();
        assert_eq!(removed, vec![orphan]);
        assert!(store.contains(&base) && store.contains(&mid) && store.contains(&tip));
        assert!(!store.contains(&orphan));
    }
}
