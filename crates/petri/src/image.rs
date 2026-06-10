//! The `petri image` subsystem: named, layered VM images built on
//! `petri-nbd`'s content-addressed layer store.
//!
//! Each named image lives under `<images-root>/<name>/`:
//!
//! ```text
//! <images-root>/<name>/
//!   meta.json          registry for this image (see [`ImageMeta`])
//!   scratch.data       the mutable ScratchLayer append-log (when present)
//!   layers/            LayerStore root (sealed layers + .staging/)
//! ```
//!
//! A *tag* names a point in the image's history. The reserved tag `scratch`
//! always refers to the current mutable overlay; every other tag names a frozen
//! (sealed) immutable layer. Reads compose `base layers (bottom-first) + scratch`
//! into a single [`petri_nbd::LayeredDisk`] at serve time.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use petri_nbd::{
    Geometry, ImmutableLayer, LayerId, LayerStore, LayeredDisk, NbdHandle, NbdServer, ScratchLayer,
    ServeOpts,
};

use crate::error::{PetriError, Result};

/// The reserved tag naming the current mutable scratch overlay.
pub const SCRATCH_TAG: &str = "scratch";

/// On-disk registry for one named image (`meta.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageMeta {
    pub name: String,
    /// The current mutable scratch overlay, or `None` once it has been deleted.
    #[serde(default)]
    pub scratch: Option<ScratchMeta>,
    /// Frozen (sealed) layers, in creation order.
    #[serde(default)]
    pub layers: Vec<LayerMeta>,
}

/// Metadata for the mutable scratch overlay sitting on top of the layer chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScratchMeta {
    /// Virtual disk size in bytes (shared geometry with the layer chain).
    pub size_bytes: u64,
    /// `LayerId` hex of the sealed layer this scratch sits on, or `None` for a
    /// blank scratch with no parent.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Loopback TCP port of the live `NbdServer` exporting this scratch, set
    /// while a sandbox has it attached and cleared on stop/kill.
    #[serde(default)]
    pub nbd_port: Option<u16>,
    /// Sandbox IDs currently holding this scratch open over NBD.
    #[serde(default)]
    pub running_sandboxes: Vec<String>,
}

/// Metadata for one frozen, sealed immutable layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerMeta {
    /// `LayerId::to_hex()` returned by `LayerStore::seal_into`.
    pub id: String,
    pub tag: String,
    /// `id` of the layer below this one, or `None` for a base layer.
    #[serde(default)]
    pub parent_id: Option<String>,
    pub size_bytes: u64,
    pub created_at: String,
    /// Full text of the provision script used to build this layer, if any.
    #[serde(default)]
    pub provision_script: Option<String>,
}

impl ImageMeta {
    /// Find a frozen layer by tag.
    pub fn layer_by_tag(&self, tag: &str) -> Option<&LayerMeta> {
        self.layers.iter().find(|layer| layer.tag == tag)
    }

    /// Find a frozen layer by its content-id hex.
    pub fn layer_by_id(&self, id: &str) -> Option<&LayerMeta> {
        self.layers.iter().find(|layer| layer.id == id)
    }

    /// Resolve a parent id to a human-friendly label: the parent layer's tag if
    /// it is one of our own layers, else the first 12 hex chars, else `-`.
    pub fn parent_label(&self, parent_id: Option<&str>) -> String {
        match parent_id {
            None => "-".to_string(),
            Some(id) => match self.layer_by_id(id) {
                Some(layer) => layer.tag.clone(),
                None => short_id(id),
            },
        }
    }
}

/// Split a `<name>:<tag>` reference. A bare `<name>` (no tag) is always an
/// error: every image argument must be tagged.
pub fn parse_image_ref(s: &str) -> Result<(String, String)> {
    let mut parts = s.splitn(2, ':');
    let name = parts.next().unwrap_or("");
    let tag = parts.next().unwrap_or("");
    if name.is_empty() || tag.is_empty() {
        return Err(PetriError::invalid_argument(format!(
            "image reference '{s}' must include a tag (e.g. '{s}:scratch')"
        )));
    }
    Ok((name.to_string(), tag.to_string()))
}

/// Validate a tag supplied to `--tag` (freeze/rebuild): non-empty, no `:`, and
/// not the reserved `scratch`.
pub fn validate_freeze_tag(tag: &str) -> Result<()> {
    if tag.is_empty() {
        return Err(PetriError::invalid_argument("--tag must be a non-empty value"));
    }
    if tag.contains(':') {
        return Err(PetriError::invalid_argument(format!(
            "tag '{tag}' must not contain ':'"
        )));
    }
    if tag == SCRATCH_TAG {
        return Err(PetriError::invalid_argument(
            "\"scratch\" is a reserved tag and cannot be used",
        ));
    }
    Ok(())
}

// --- on-disk layout helpers -------------------------------------------------

/// Root directory holding every named image. Mirrors the macOS backend's state
/// directory (`~/.petri`), with images alongside `instances/`. Overridable via
/// `PETRI_IMAGES_DIR` (primarily for tests).
pub fn images_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("PETRI_IMAGES_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".petri").join("images"))
        .unwrap_or_else(|| std::env::temp_dir().join("petri").join("images"))
}

/// Filesystem paths for one named image under `<images-root>/<name>/`.
pub struct ImagePaths {
    pub dir: PathBuf,
}

impl ImagePaths {
    pub fn new(images_root: &Path, name: &str) -> Self {
        Self {
            dir: images_root.join(name),
        }
    }

    pub fn meta(&self) -> PathBuf {
        self.dir.join("meta.json")
    }

    pub fn scratch_data(&self) -> PathBuf {
        self.dir.join("scratch.data")
    }

    /// LayerStore root for this image. `LayerStore::open` manages `layers/` and
    /// `.staging/` underneath it.
    pub fn layers_root(&self) -> PathBuf {
        self.dir.join("layers")
    }

    pub fn exists(&self) -> bool {
        self.dir.exists()
    }

    pub fn open_store(&self) -> Result<LayerStore> {
        LayerStore::open(&self.layers_root()).map_err(|source| PetriError::Io {
            path: self.layers_root(),
            source,
        })
    }
}

/// Load an image's `meta.json`, erroring if the image does not exist.
pub fn load_meta(paths: &ImagePaths) -> Result<ImageMeta> {
    let path = paths.meta();
    let input = fs::read_to_string(&path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            PetriError::Cli(format!(
                "image \"{}\" does not exist",
                paths
                    .dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
            ))
        } else {
            PetriError::Io { path, source }
        }
    })?;
    serde_json::from_str(&input)
        .map_err(|err| PetriError::Cli(format!("failed to parse {}: {err}", paths.meta().display())))
}

/// Persist an image's `meta.json` (pretty-printed, matching backend.rs).
pub fn save_meta(paths: &ImagePaths, meta: &ImageMeta) -> Result<()> {
    fs::create_dir_all(&paths.dir).map_err(|source| PetriError::Io {
        path: paths.dir.clone(),
        source,
    })?;
    let payload = serde_json::to_string_pretty(meta)
        .map_err(|err| PetriError::Cli(format!("failed to encode image metadata: {err}")))?;
    let path = paths.meta();
    fs::write(&path, payload).map_err(|source| PetriError::Io { path, source })
}

// --- geometry / size helpers ------------------------------------------------

/// Bytes in one GiB.
pub const GIB: u64 = 1024 * 1024 * 1024;

/// Build a geometry for a freshly created scratch of `size_bytes` using the
/// default 64 KiB block size.
pub fn default_geometry(size_bytes: u64) -> Result<Geometry> {
    Geometry::new(size_bytes, Geometry::DEFAULT_BLOCK_SIZE).map_err(|source| PetriError::Io {
        path: PathBuf::from("<geometry>"),
        source,
    })
}

/// First 12 hex chars of a content id (or the whole thing if shorter).
pub fn short_id(id: &str) -> String {
    id.chars().take(12).collect()
}

/// Human-readable byte size, e.g. `8.0 GiB`, `3.4 GiB`, `512 B`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Create a fresh empty scratch overlay file at `path` with `geometry`.
pub fn create_scratch(path: &Path, geometry: Geometry) -> Result<ScratchLayer> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| PetriError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    ScratchLayer::create(path, geometry).map_err(|source| PetriError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Current UTC time as an RFC 3339 string (`2026-06-09T12:00:00Z`), computed
/// from `SystemTime` without pulling in a date library.
pub fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339_from_unix(secs)
}

fn rfc3339_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert a day count since the Unix epoch into a `(year, month, day)` civil
/// date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

// --- operations -------------------------------------------------------------

/// `petri image create <name> [--base <name>:<tag>] [--size <gb>]`.
///
/// Creates a named image and its initial scratch overlay. With `--base`, the
/// scratch inherits the base layer's geometry and records it as its parent (the
/// base layers are stacked at serve time, not copied here); `--size` is ignored
/// in that case. Without `--base`, the scratch is `size_gib` GiB (default 8).
pub fn create(
    images_root: &Path,
    name: &str,
    base: Option<(&str, &str)>,
    size_gib: Option<u64>,
) -> Result<String> {
    let paths = ImagePaths::new(images_root, name);
    if paths.exists() {
        return Err(PetriError::Cli(format!("image \"{name}\" already exists")));
    }

    match base {
        None => {
            let size_bytes = size_gib.unwrap_or(8) * GIB;
            let geometry = default_geometry(size_bytes)?;
            // Materialize the empty scratch overlay (also creates <name>/).
            create_scratch(&paths.scratch_data(), geometry)?;
            let meta = ImageMeta {
                name: name.to_string(),
                scratch: Some(ScratchMeta {
                    size_bytes,
                    parent_id: None,
                    nbd_port: None,
                    running_sandboxes: Vec::new(),
                }),
                layers: Vec::new(),
            };
            save_meta(&paths, &meta)?;
            Ok(format!(
                "created image '{name}' (scratch, {} GiB)",
                size_bytes / GIB
            ))
        }
        Some((base_name, base_tag)) => {
            // `scratch`, an unknown tag, or anything not in the layers array is
            // not a frozen layer and cannot serve as a base.
            if base_tag == SCRATCH_TAG {
                return Err(PetriError::Cli(format!(
                    "\"{base_name}:{base_tag}\" is not frozen and cannot be used as a base"
                )));
            }
            let base_paths = ImagePaths::new(images_root, base_name);
            let base_meta = load_meta(&base_paths)?;
            let base_layer = base_meta.layer_by_tag(base_tag).ok_or_else(|| {
                PetriError::Cli(format!(
                    "\"{base_name}:{base_tag}\" is not frozen and cannot be used as a base"
                ))
            })?;
            let base_id = LayerId::from_hex(&base_layer.id).ok_or_else(|| {
                PetriError::Cli(format!(
                    "image \"{base_name}\" layer '{}' has a malformed id",
                    base_layer.tag
                ))
            })?;

            // Inherit the base layer's geometry: the derived scratch must share
            // geometry with the chain it overlays.
            let base_store = base_paths.open_store()?;
            let base_immutable =
                base_store
                    .open_layer(&base_id)
                    .map_err(|source| PetriError::Io {
                        path: base_paths.layers_root(),
                        source,
                    })?;
            let geometry = base_immutable.geometry();

            // Create the (initially empty) derived store and a blank scratch.
            paths.open_store()?;
            create_scratch(&paths.scratch_data(), geometry)?;
            let meta = ImageMeta {
                name: name.to_string(),
                scratch: Some(ScratchMeta {
                    size_bytes: geometry.virtual_size,
                    parent_id: Some(base_layer.id.clone()),
                    nbd_port: None,
                    running_sandboxes: Vec::new(),
                }),
                layers: Vec::new(),
            };
            save_meta(&paths, &meta)?;
            Ok(format!(
                "created image '{name}:scratch' based on '{base_name}:{base_tag}'"
            ))
        }
    }
}

/// `petri image list`: a table of every named image's scratch and frozen
/// layers.
pub fn list(images_root: &Path) -> Result<String> {
    let metas = load_all_metas(images_root)?;
    if metas.is_empty() {
        return Ok("no images".to_string());
    }

    let mut lines = vec![format!(
        "{:<26}{:<11}{:<15}{:<15}{:<18}{}",
        "NAME", "TAG", "ID", "PARENT", "STATE", "SIZE"
    )];
    for meta in &metas {
        if let Some(scratch) = &meta.scratch {
            let mut state = "mutable".to_string();
            if scratch.nbd_port.is_some() {
                state.push_str(" (running)");
            }
            lines.push(format!(
                "{:<26}{:<11}{:<15}{:<15}{:<18}{}",
                meta.name,
                SCRATCH_TAG,
                "-",
                meta.parent_label(scratch.parent_id.as_deref()),
                state,
                human_size(scratch.size_bytes),
            ));
        }
        for layer in &meta.layers {
            lines.push(format!(
                "{:<26}{:<11}{:<15}{:<15}{:<18}{}",
                meta.name,
                layer.tag,
                short_id(&layer.id),
                meta.parent_label(layer.parent_id.as_deref()),
                "frozen",
                human_size(layer.size_bytes),
            ));
        }
    }
    Ok(lines.join("\n"))
}

/// `petri image inspect <name>:<tag>`: full metadata for one scratch or layer.
pub fn inspect(images_root: &Path, name: &str, tag: &str) -> Result<String> {
    let paths = ImagePaths::new(images_root, name);
    let meta = load_meta(&paths)?;

    if tag == SCRATCH_TAG {
        let scratch = meta.scratch.as_ref().ok_or_else(|| {
            PetriError::Cli(format!("image \"{name}\" has no scratch"))
        })?;
        let nbd = match scratch.nbd_port {
            Some(port) => format!("running (port {port})"),
            None => "(not running)".to_string(),
        };
        Ok(format!(
            "image:    {name}\n\
             tag:      scratch\n\
             state:    mutable\n\
             size:     {} ({} bytes)\n\
             parent:   {}\n\
             nbd:      {nbd}",
            human_size(scratch.size_bytes),
            scratch.size_bytes,
            meta.parent_label(scratch.parent_id.as_deref()),
        ))
    } else {
        let layer = meta
            .layer_by_tag(tag)
            .ok_or_else(|| PetriError::Cli(format!("image \"{name}\" has no tag '{tag}'")))?;
        let provision = layer.provision_script.as_deref().unwrap_or("(none)");
        Ok(format!(
            "image:      {name}\n\
             tag:        {tag}\n\
             state:      frozen\n\
             id:         {}\n\
             parent:     {}\n\
             created_at: {}\n\
             size:       {} ({} bytes)\n\
             provision_script:\n{provision}",
            layer.id,
            meta.parent_label(layer.parent_id.as_deref()),
            layer.created_at,
            human_size(layer.size_bytes),
            layer.size_bytes,
        ))
    }
}

/// `petri image freeze <name>:scratch --tag <tag> [--provision <path>] [--force]`.
///
/// Sealing a scratch can only happen **in-process while its `NbdHandle` is
/// live** (`ScratchLayer` keeps its block index in memory; `scratch.data` has no
/// on-disk index — design doc §7 defers an index journal). The real freeze path
/// is `petri sandbox create --bootstrap <name>:scratch ... --auto-freeze`, which
/// calls [`record_frozen_layer`] with the live-sealed `LayerId`.
///
/// This standalone CLI command therefore cannot reseal a cold scratch yet; it
/// validates its arguments and points the caller at `--auto-freeze`.
pub fn freeze(
    images_root: &Path,
    name: &str,
    tag: &str,
    _provision: Option<&Path>,
    force: bool,
) -> Result<String> {
    validate_freeze_tag(tag)?;
    let paths = ImagePaths::new(images_root, name);
    let meta = load_meta(&paths)?;
    let scratch = meta
        .scratch
        .as_ref()
        .ok_or_else(|| PetriError::Cli(format!("image \"{name}\" has no scratch to freeze")))?;

    if !force && meta.layer_by_tag(tag).is_some() {
        return Err(PetriError::Cli(format!(
            "tag '{tag}' already exists for image \"{name}\"; pass --force to overwrite"
        )));
    }

    // TODO: cold freeze via server control channel — connect to the running NBD
    // server for this scratch and have it seal in-process (NbdHandle::seal_scratch
    // while the index is live), then call record_frozen_layer with the LayerId.
    if scratch.nbd_port.is_none() {
        return Err(PetriError::Cli(format!(
            "{name}:scratch has no running NBD server — freeze must happen while the server is live.\n\
             Start a sandbox with --bootstrap {name}:scratch and use --auto-freeze, or attach and freeze manually via the running server."
        )));
    }
    Err(PetriError::Cli(
        "cold freeze not yet implemented; use --auto-freeze when creating the sandbox".to_string(),
    ))
}

/// `petri image stop <name>:scratch`: clear a stale `nbd_port` left behind by a
/// sandbox that died without cleaning up. There is no persistent NBD daemon, so
/// this is bookkeeping cleanup rather than a live shutdown. Idempotent.
pub fn stop(images_root: &Path, name: &str) -> Result<String> {
    let paths = ImagePaths::new(images_root, name);
    let mut meta = load_meta(&paths)?;
    let scratch = meta
        .scratch
        .as_mut()
        .ok_or_else(|| PetriError::Cli(format!("image \"{name}\" has no scratch")))?;
    if scratch.nbd_port.is_none() {
        return Ok("(already stopped)".to_string());
    }
    // No live handle to reach (the server lived in the now-gone sandbox process):
    // clear the recorded port and the stale attached-sandbox list.
    scratch.nbd_port = None;
    scratch.running_sandboxes.clear();
    save_meta(&paths, &meta)?;
    Ok(format!("stopped '{name}:scratch'"))
}

/// `petri image delete <name>:<tag>`: remove a scratch overlay or a frozen
/// layer, refusing to orphan a layer that is still a parent.
pub fn delete(images_root: &Path, name: &str, tag: &str, force: bool) -> Result<String> {
    let paths = ImagePaths::new(images_root, name);
    let mut meta = load_meta(&paths)?;
    if tag == SCRATCH_TAG {
        delete_scratch(&paths, &mut meta, name, force)
    } else {
        delete_layer(&paths, &mut meta, name, tag)
    }
}

fn delete_scratch(
    paths: &ImagePaths,
    meta: &mut ImageMeta,
    name: &str,
    force: bool,
) -> Result<String> {
    let scratch = meta
        .scratch
        .as_ref()
        .ok_or_else(|| PetriError::Cli(format!("image \"{name}\" has no scratch")))?;
    if let Some(port) = scratch.nbd_port {
        return Err(PetriError::Cli(format!(
            "{name}:scratch has a running NBD server (port {port}), call 'petri image stop {name}:scratch' first"
        )));
    }
    if let Some(sandbox) = scratch.running_sandboxes.first() {
        return Err(PetriError::Cli(format!(
            "{name}:scratch is attached to sandbox \"{sandbox}\", stop or kill it first"
        )));
    }

    // A scratch that has written blocks is real data; require --force to drop it.
    let data = paths.scratch_data();
    let non_empty = fs::metadata(&data).map(|m| m.len() > 0).unwrap_or(false);
    if non_empty && !force {
        return Err(PetriError::Cli(format!(
            "{name}:scratch has written data; pass --force to delete"
        )));
    }

    if data.exists() {
        fs::remove_file(&data).map_err(|source| PetriError::Io { path: data, source })?;
    }
    meta.scratch = None;
    if meta.layers.is_empty() {
        remove_image_dir(paths)?;
        return Ok(format!("deleted '{name}:scratch' and removed image \"{name}\""));
    }
    save_meta(paths, meta)?;
    Ok(format!("deleted '{name}:scratch'"))
}

fn delete_layer(paths: &ImagePaths, meta: &mut ImageMeta, name: &str, tag: &str) -> Result<String> {
    let layer_id = meta
        .layer_by_tag(tag)
        .ok_or_else(|| PetriError::Cli(format!("image \"{name}\" has no tag '{tag}'")))?
        .id
        .clone();

    // Refuse to orphan a layer that another layer in this image derives from.
    if let Some(child) = meta
        .layers
        .iter()
        .find(|layer| layer.tag != tag && layer.parent_id.as_deref() == Some(layer_id.as_str()))
    {
        return Err(PetriError::Cli(format!(
            "layer {} is the parent of {name}:{} and cannot be deleted",
            short_id(&layer_id),
            child.tag
        )));
    }
    // ...or that the current scratch sits on.
    if meta
        .scratch
        .as_ref()
        .and_then(|scratch| scratch.parent_id.as_deref())
        == Some(layer_id.as_str())
    {
        return Err(PetriError::Cli(format!(
            "layer {} is the parent of {name}:scratch and cannot be deleted",
            short_id(&layer_id)
        )));
    }

    let id = LayerId::from_hex(&layer_id)
        .ok_or_else(|| PetriError::Cli(format!("layer '{tag}' has a malformed id")))?;
    let store = paths.open_store()?;
    store.delete(&id).map_err(|source| PetriError::Io {
        path: paths.layers_root(),
        source,
    })?;
    meta.layers.retain(|layer| layer.tag != tag);

    if meta.layers.is_empty() && !paths.scratch_data().exists() {
        remove_image_dir(paths)?;
        return Ok(format!("deleted '{name}:{tag}' and removed image \"{name}\""));
    }
    save_meta(paths, meta)?;
    Ok(format!("deleted '{name}:{tag}'"))
}

fn remove_image_dir(paths: &ImagePaths) -> Result<()> {
    fs::remove_dir_all(&paths.dir).map_err(|source| PetriError::Io {
        path: paths.dir.clone(),
        source,
    })
}

/// `petri image show-provision <name>:<tag>`: print the stored provision script
/// for a frozen layer, erroring if it has none.
pub fn show_provision(images_root: &Path, name: &str, tag: &str) -> Result<String> {
    let paths = ImagePaths::new(images_root, name);
    let meta = load_meta(&paths)?;
    if tag == SCRATCH_TAG {
        return Err(PetriError::Cli(format!(
            "{name}:scratch is a mutable overlay and has no provision script"
        )));
    }
    let layer = meta
        .layer_by_tag(tag)
        .ok_or_else(|| PetriError::Cli(format!("image \"{name}\" has no tag '{tag}'")))?;
    layer.provision_script.clone().ok_or_else(|| {
        PetriError::Cli(format!("{name}:{tag} has no stored provision script"))
    })
}

// --- rebuild helpers --------------------------------------------------------

/// Return the stored provision script for a frozen `<name>:<tag>`, or the
/// rebuild-specific error if the layer is missing or has no script.
pub fn provision_for_rebuild(images_root: &Path, name: &str, tag: &str) -> Result<String> {
    let paths = ImagePaths::new(images_root, name);
    let meta = load_meta(&paths)?;
    let layer = meta.layer_by_tag(tag).ok_or_else(|| {
        PetriError::Cli(format!(
            "{name}:{tag} has no stored provision script and cannot be rebuilt"
        ))
    })?;
    layer.provision_script.clone().ok_or_else(|| {
        PetriError::Cli(format!(
            "{name}:{tag} has no stored provision script and cannot be rebuilt"
        ))
    })
}

/// Ensure `<name>` has a scratch overlay sitting on the frozen `<base>:<tag>`
/// layer, creating one if absent. Errors if a scratch already exists over a
/// *different* parent (the caller must delete it first) — this is the rebuild
/// "fresh scratch over base" precondition.
pub fn reset_scratch_over_base(
    images_root: &Path,
    name: &str,
    base_name: &str,
    base_tag: &str,
) -> Result<()> {
    if base_tag == SCRATCH_TAG {
        return Err(PetriError::Cli(format!(
            "\"{base_name}:{base_tag}\" is not frozen and cannot be used as a base"
        )));
    }
    let base_paths = ImagePaths::new(images_root, base_name);
    let base_meta = load_meta(&base_paths)?;
    let base_layer = base_meta.layer_by_tag(base_tag).ok_or_else(|| {
        PetriError::Cli(format!(
            "\"{base_name}:{base_tag}\" is not frozen and cannot be used as a base"
        ))
    })?;
    let base_id = LayerId::from_hex(&base_layer.id)
        .ok_or_else(|| PetriError::Cli(format!("image \"{base_name}\" layer has a malformed id")))?;
    let base_store = base_paths.open_store()?;
    let geometry = base_store
        .open_layer(&base_id)
        .map_err(|source| PetriError::Io {
            path: base_paths.layers_root(),
            source,
        })?
        .geometry();

    let paths = ImagePaths::new(images_root, name);
    let mut meta = load_meta(&paths)?;
    if let Some(scratch) = &meta.scratch {
        if scratch.parent_id.as_deref() == Some(base_layer.id.as_str()) {
            return Ok(()); // already a fresh-enough scratch over this base
        }
        return Err(PetriError::Cli(format!(
            "{name}:scratch already exists over a different parent; delete it first ('petri image delete {name}:scratch --force')"
        )));
    }
    create_scratch(&paths.scratch_data(), geometry)?;
    meta.scratch = Some(ScratchMeta {
        size_bytes: geometry.virtual_size,
        parent_id: Some(base_layer.id.clone()),
        nbd_port: None,
        running_sandboxes: Vec::new(),
    });
    save_meta(&paths, &meta)
}

// --- live serving + in-process freeze ---------------------------------------

/// Start an in-process `NbdServer` exporting this image's current scratch
/// overlay stacked on its (cross-image) layer chain, over loopback TCP. The
/// returned [`NbdHandle`] keeps the server alive until it is dropped; its
/// `url()` is what gets handed to `petri-vz --data-disk`.
///
/// The scratch overlay starts empty: an unsealed scratch's block index lives
/// only in memory (see [`freeze`]), so a serve session begins fresh on top of
/// the sealed chain and accumulates writes for the life of this handle.
pub fn serve_scratch(images_root: &Path, name: &str) -> Result<NbdHandle> {
    let paths = ImagePaths::new(images_root, name);
    let meta = load_meta(&paths)?;
    let scratch_meta = meta
        .scratch
        .as_ref()
        .ok_or_else(|| PetriError::Cli(format!("image \"{name}\" has no scratch")))?;

    let lower = lower_layers_for(images_root, scratch_meta.parent_id.as_deref())?;
    let geometry = match lower.first() {
        Some(layer) => layer.geometry(),
        None => default_geometry(scratch_meta.size_bytes)?,
    };
    let scratch = create_scratch(&paths.scratch_data(), geometry)?;
    let disk = LayeredDisk::new(lower, scratch).map_err(|source| PetriError::Io {
        path: paths.scratch_data(),
        source,
    })?;
    NbdServer::serve(disk, ServeOpts::loopback()).map_err(|source| PetriError::Io {
        path: paths.dir.clone(),
        source,
    })
}

/// Resolve the immutable layer chain a scratch sits on, bottom-first, walking
/// `parent_id` edges across *every* image store (a `--base`-derived image's
/// parent layer physically lives in the base image's store; content-addressing
/// makes the file identical wherever it lives).
fn lower_layers_for(
    images_root: &Path,
    scratch_parent: Option<&str>,
) -> Result<Vec<ImmutableLayer>> {
    let mut chain = Vec::new(); // top-first while building
    let mut cursor = scratch_parent.map(str::to_string);
    while let Some(hex) = cursor {
        let layer = open_layer_anywhere(images_root, &hex)?;
        cursor = layer.parent_ids().first().map(LayerId::to_hex);
        chain.push(layer);
    }
    chain.reverse(); // bottom-first for LayeredDisk
    Ok(chain)
}

/// Open a sealed layer by content-id hex from whichever image store holds it.
fn open_layer_anywhere(images_root: &Path, hex: &str) -> Result<ImmutableLayer> {
    let path = find_layer_file(images_root, hex).ok_or_else(|| {
        PetriError::Cli(format!(
            "layer {} not found in any image store",
            short_id(hex)
        ))
    })?;
    ImmutableLayer::open_sealed(&path).map_err(|source| PetriError::Io { path, source })
}

/// Scan every image's `LayerStore` for a sealed layer file named `hex`.
fn find_layer_file(images_root: &Path, hex: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(images_root).ok()?;
    for entry in entries.flatten() {
        // LayerStore root is <image>/layers; files live at <root>/layers/<hex>.
        let candidate = entry.path().join("layers").join("layers").join(hex);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Seal the live scratch behind `handle` into a frozen layer and record it in
/// `meta.json`. This is the real freeze path: it must run while the `NbdHandle`
/// is in scope (the scratch index is in memory), i.e. from the in-process
/// `--auto-freeze` builder after the VM has stopped writing.
pub fn auto_freeze(
    images_root: &Path,
    name: &str,
    handle: &NbdHandle,
    tag: &str,
    provision_script: Option<String>,
) -> Result<String> {
    validate_freeze_tag(tag)?;
    let paths = ImagePaths::new(images_root, name);
    let meta = load_meta(&paths)?;
    let parent_hex = meta.scratch.as_ref().and_then(|s| s.parent_id.clone());
    let parents: Vec<LayerId> = match &parent_hex {
        Some(hex) => vec![LayerId::from_hex(hex).ok_or_else(|| {
            PetriError::Cli(format!("scratch parent id '{hex}' is malformed"))
        })?],
        None => Vec::new(),
    };

    let store = paths.open_store()?;
    let staging = paths
        .layers_root()
        .join(".staging")
        .join(unique_seal_name());
    let sealed = handle
        .seal_scratch(&staging, &parents)
        .map_err(|source| PetriError::Io {
            path: staging.clone(),
            source,
        })?;
    let geometry = sealed.geometry();
    let size_bytes = fs::metadata(&staging).map(|m| m.len()).unwrap_or(0);
    drop(sealed); // release the handle on the staging file before adopting it

    let id = store
        .adopt_sealed(&staging)
        .map_err(|source| PetriError::Io {
            path: paths.layers_root(),
            source,
        })?;
    record_frozen_layer(
        images_root,
        name,
        &id,
        size_bytes,
        geometry,
        provision_script,
        tag,
    )
}

/// Append a freshly sealed layer to `meta.json` and roll a new empty scratch on
/// top of it (freeze steps 9–11). Shared by the in-process freeze paths.
pub fn record_frozen_layer(
    images_root: &Path,
    name: &str,
    sealed_id: &LayerId,
    size_bytes: u64,
    geometry: Geometry,
    provision_script: Option<String>,
    tag: &str,
) -> Result<String> {
    let paths = ImagePaths::new(images_root, name);
    let mut meta = load_meta(&paths)?;
    let parent_id = meta.scratch.as_ref().and_then(|s| s.parent_id.clone());

    // Overwrite any same-tag entry (freeze --force / rebuild reuse).
    meta.layers.retain(|layer| layer.tag != tag);
    meta.layers.push(LayerMeta {
        id: sealed_id.to_hex(),
        tag: tag.to_string(),
        parent_id,
        size_bytes,
        created_at: rfc3339_now(),
        provision_script,
    });

    // Roll a fresh scratch on top of the layer we just sealed.
    let data = paths.scratch_data();
    if data.exists() {
        let _ = fs::remove_file(&data);
    }
    create_scratch(&data, geometry)?;
    meta.scratch = Some(ScratchMeta {
        size_bytes: geometry.virtual_size,
        parent_id: Some(sealed_id.to_hex()),
        nbd_port: None,
        running_sandboxes: Vec::new(),
    });
    save_meta(&paths, &meta)?;

    Ok(format!(
        "frozen '{name}:{tag}' (id: {})\nnew scratch created from '{name}:{tag}'",
        sealed_id.to_hex()
    ))
}

/// Record that a sandbox is serving this image's scratch over NBD on `port`.
pub fn mark_serving(images_root: &Path, name: &str, port: u16, sandbox_id: &str) -> Result<()> {
    let paths = ImagePaths::new(images_root, name);
    let mut meta = load_meta(&paths)?;
    let scratch = meta
        .scratch
        .as_mut()
        .ok_or_else(|| PetriError::Cli(format!("image \"{name}\" has no scratch")))?;
    scratch.nbd_port = Some(port);
    if !scratch.running_sandboxes.iter().any(|s| s == sandbox_id) {
        scratch.running_sandboxes.push(sandbox_id.to_string());
    }
    save_meta(&paths, &meta)
}

/// Clear the NBD serving bookkeeping for one sandbox (server stopped/torn down).
pub fn clear_serving(images_root: &Path, name: &str, sandbox_id: &str) -> Result<()> {
    let paths = ImagePaths::new(images_root, name);
    let mut meta = load_meta(&paths)?;
    if let Some(scratch) = meta.scratch.as_mut() {
        scratch.running_sandboxes.retain(|s| s != sandbox_id);
        if scratch.running_sandboxes.is_empty() {
            scratch.nbd_port = None;
        }
        save_meta(&paths, &meta)?;
    }
    Ok(())
}

/// Extract the loopback port from an `nbd://127.0.0.1:<port>/...` URL.
pub fn nbd_port_from_url(url: &str) -> Option<u16> {
    let rest = url.strip_prefix("nbd://")?;
    let host_port = rest.split('/').next()?;
    host_port.rsplit(':').next()?.parse().ok()
}

/// A staging file name unique without `Date`/random (mirrors store.rs).
fn unique_seal_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("auto-seal-{}-{n}", std::process::id())
}

/// Load every image's `meta.json` under `images_root`, sorted by name. Returns
/// an empty vec if the root does not exist yet.
pub fn load_all_metas(images_root: &Path) -> Result<Vec<ImageMeta>> {
    let mut names: Vec<String> = Vec::new();
    match fs::read_dir(images_root) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|source| PetriError::Io {
                    path: images_root.to_path_buf(),
                    source,
                })?;
                if entry.path().join("meta.json").is_file()
                    && let Some(name) = entry.file_name().to_str()
                {
                    names.push(name.to_string());
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(PetriError::Io {
                path: images_root.to_path_buf(),
                source,
            });
        }
    }
    names.sort();
    names
        .iter()
        .map(|name| load_meta(&ImagePaths::new(images_root, name)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_image_ref_splits_name_and_tag() {
        assert_eq!(
            parse_image_ref("debian:trixie").unwrap(),
            ("debian".to_string(), "trixie".to_string())
        );
        assert_eq!(
            parse_image_ref("debian:scratch").unwrap(),
            ("debian".to_string(), "scratch".to_string())
        );
    }

    #[test]
    fn parse_image_ref_requires_a_tag() {
        let err = parse_image_ref("debian").unwrap_err().to_string();
        assert_eq!(
            err,
            "image reference 'debian' must include a tag (e.g. 'debian:scratch')"
        );
        // A trailing colon with an empty tag is still an error.
        assert!(parse_image_ref("debian:").is_err());
        assert!(parse_image_ref("").is_err());
    }

    #[test]
    fn validate_freeze_tag_rejects_reserved_and_colon() {
        assert!(validate_freeze_tag("trixie").is_ok());
        assert_eq!(
            validate_freeze_tag("scratch").unwrap_err().to_string(),
            "\"scratch\" is a reserved tag and cannot be used"
        );
        assert!(validate_freeze_tag("a:b").is_err());
        assert!(validate_freeze_tag("").is_err());
    }

    #[test]
    fn rfc3339_formats_known_epoch() {
        // 2026-06-01T12:00:00Z
        assert_eq!(rfc3339_from_unix(1_780_315_200), "2026-06-01T12:00:00Z");
        // Unix epoch itself.
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(8 * GIB), "8.0 GiB");
        assert_eq!(human_size(512), "512 B");
    }

    #[test]
    fn short_id_takes_twelve() {
        assert_eq!(short_id("abc123def456789"), "abc123def456");
        assert_eq!(short_id("abc"), "abc");
    }

    // --- operation tests ---------------------------------------------------

    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_root() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("petri-img-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn create_without_base_makes_blank_scratch() {
        let root = temp_root();
        let msg = create(&root, "rootfs", None, None).unwrap();
        assert_eq!(msg, "created image 'rootfs' (scratch, 8 GiB)");

        let paths = ImagePaths::new(&root, "rootfs");
        assert!(paths.scratch_data().is_file());
        let meta = load_meta(&paths).unwrap();
        assert!(meta.layers.is_empty());
        let scratch = meta.scratch.unwrap();
        assert_eq!(scratch.size_bytes, 8 * GIB);
        assert_eq!(scratch.parent_id, None);
        assert_eq!(scratch.nbd_port, None);
    }

    #[test]
    fn create_honours_size_in_gib() {
        let root = temp_root();
        let msg = create(&root, "big", None, Some(16)).unwrap();
        assert_eq!(msg, "created image 'big' (scratch, 16 GiB)");
        let meta = load_meta(&ImagePaths::new(&root, "big")).unwrap();
        assert_eq!(meta.scratch.unwrap().size_bytes, 16 * GIB);
    }

    #[test]
    fn create_rejects_duplicate_name() {
        let root = temp_root();
        create(&root, "dup", None, None).unwrap();
        let err = create(&root, "dup", None, None).unwrap_err().to_string();
        assert_eq!(err, "image \"dup\" already exists");
    }

    #[test]
    fn list_empty_root_reports_no_images() {
        let root = temp_root();
        assert_eq!(list(&root).unwrap(), "no images");
    }

    #[test]
    fn list_shows_scratch_row_with_columns() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        let out = list(&root).unwrap();
        assert!(out.contains("NAME"), "{out}");
        assert!(out.contains("rootfs"), "{out}");
        assert!(out.contains("scratch"), "{out}");
        assert!(out.contains("mutable"), "{out}");
        assert!(!out.contains("(running)"), "{out}");
    }

    #[test]
    fn inspect_scratch_reports_not_running() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        let out = inspect(&root, "rootfs", "scratch").unwrap();
        assert!(out.contains("state:    mutable"), "{out}");
        assert!(out.contains("(not running)"), "{out}");
        assert!(out.contains("8589934592 bytes"), "{out}");
    }

    #[test]
    fn inspect_unknown_tag_errors() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        let err = inspect(&root, "rootfs", "nope").unwrap_err().to_string();
        assert_eq!(err, "image \"rootfs\" has no tag 'nope'");
    }

    #[test]
    fn freeze_cold_scratch_points_at_auto_freeze() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        let err = freeze(&root, "rootfs", "v1", None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no running NBD server"), "{err}");
        assert!(err.contains("--auto-freeze"), "{err}");
    }

    #[test]
    fn freeze_rejects_reserved_tag() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        let err = freeze(&root, "rootfs", "scratch", None, false)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "\"scratch\" is a reserved tag and cannot be used");
    }

    #[test]
    fn freeze_rejects_duplicate_tag_without_force() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        // Hand-craft a meta with an existing frozen tag and a running server so
        // the duplicate-tag guard (which runs before the server check) fires.
        let paths = ImagePaths::new(&root, "rootfs");
        let mut meta = load_meta(&paths).unwrap();
        meta.layers.push(LayerMeta {
            id: "a".repeat(64),
            tag: "v1".to_string(),
            parent_id: None,
            size_bytes: 0,
            created_at: rfc3339_now(),
            provision_script: None,
        });
        if let Some(scratch) = meta.scratch.as_mut() {
            scratch.nbd_port = Some(4321);
        }
        save_meta(&paths, &meta).unwrap();

        let err = freeze(&root, "rootfs", "v1", None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
        // With --force the duplicate guard is bypassed and we fall through to the
        // not-yet-implemented warm path.
        let err = freeze(&root, "rootfs", "v1", None, true)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "cold freeze not yet implemented; use --auto-freeze when creating the sandbox"
        );
    }

    #[test]
    fn stop_is_idempotent_and_clears_port() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        // Freshly created: not running.
        assert_eq!(stop(&root, "rootfs").unwrap(), "(already stopped)");

        // Simulate a sandbox that died without cleanup.
        let paths = ImagePaths::new(&root, "rootfs");
        let mut meta = load_meta(&paths).unwrap();
        let scratch = meta.scratch.as_mut().unwrap();
        scratch.nbd_port = Some(5000);
        scratch.running_sandboxes.push("sbx-1".to_string());
        save_meta(&paths, &meta).unwrap();

        assert_eq!(stop(&root, "rootfs").unwrap(), "stopped 'rootfs:scratch'");
        let meta = load_meta(&paths).unwrap();
        let scratch = meta.scratch.unwrap();
        assert_eq!(scratch.nbd_port, None);
        assert!(scratch.running_sandboxes.is_empty());
    }

    #[test]
    fn delete_scratch_with_running_server_errors() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        let paths = ImagePaths::new(&root, "rootfs");
        let mut meta = load_meta(&paths).unwrap();
        meta.scratch.as_mut().unwrap().nbd_port = Some(6000);
        save_meta(&paths, &meta).unwrap();

        let err = delete(&root, "rootfs", "scratch", false)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "rootfs:scratch has a running NBD server (port 6000), call 'petri image stop rootfs:scratch' first"
        );
    }

    #[test]
    fn delete_scratch_attached_to_sandbox_errors() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        let paths = ImagePaths::new(&root, "rootfs");
        let mut meta = load_meta(&paths).unwrap();
        meta.scratch
            .as_mut()
            .unwrap()
            .running_sandboxes
            .push("sbx-7".to_string());
        save_meta(&paths, &meta).unwrap();

        let err = delete(&root, "rootfs", "scratch", false)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "rootfs:scratch is attached to sandbox \"sbx-7\", stop or kill it first"
        );
    }

    #[test]
    fn delete_empty_scratch_removes_image_when_no_layers() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        let paths = ImagePaths::new(&root, "rootfs");
        let msg = delete(&root, "rootfs", "scratch", false).unwrap();
        assert!(msg.contains("removed image"), "{msg}");
        assert!(!paths.exists());
    }

    #[test]
    fn delete_frozen_layer_blocked_when_it_is_a_parent() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        let paths = ImagePaths::new(&root, "rootfs");
        let mut meta = load_meta(&paths).unwrap();
        // base <- child chain, both in this image.
        meta.layers.push(LayerMeta {
            id: "b".repeat(64),
            tag: "base".to_string(),
            parent_id: None,
            size_bytes: 0,
            created_at: rfc3339_now(),
            provision_script: None,
        });
        meta.layers.push(LayerMeta {
            id: "c".repeat(64),
            tag: "child".to_string(),
            parent_id: Some("b".repeat(64)),
            size_bytes: 0,
            created_at: rfc3339_now(),
            provision_script: None,
        });
        // Scratch should not block deleting `base` here.
        meta.scratch.as_mut().unwrap().parent_id = Some("c".repeat(64));
        save_meta(&paths, &meta).unwrap();

        let err = delete(&root, "rootfs", "base", false)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!(
                "layer {} is the parent of rootfs:child and cannot be deleted",
                short_id(&"b".repeat(64))
            )
        );
    }

    #[test]
    fn delete_frozen_layer_blocked_when_scratch_sits_on_it() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        let paths = ImagePaths::new(&root, "rootfs");
        let mut meta = load_meta(&paths).unwrap();
        meta.layers.push(LayerMeta {
            id: "d".repeat(64),
            tag: "base".to_string(),
            parent_id: None,
            size_bytes: 0,
            created_at: rfc3339_now(),
            provision_script: None,
        });
        meta.scratch.as_mut().unwrap().parent_id = Some("d".repeat(64));
        save_meta(&paths, &meta).unwrap();

        let err = delete(&root, "rootfs", "base", false)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!(
                "layer {} is the parent of rootfs:scratch and cannot be deleted",
                short_id(&"d".repeat(64))
            )
        );
    }

    #[test]
    fn show_provision_returns_script_or_errors() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        let paths = ImagePaths::new(&root, "rootfs");
        let mut meta = load_meta(&paths).unwrap();
        meta.layers.push(LayerMeta {
            id: "e".repeat(64),
            tag: "with".to_string(),
            parent_id: None,
            size_bytes: 0,
            created_at: rfc3339_now(),
            provision_script: Some("#!/bin/sh\necho hi\n".to_string()),
        });
        meta.layers.push(LayerMeta {
            id: "f".repeat(64),
            tag: "without".to_string(),
            parent_id: None,
            size_bytes: 0,
            created_at: rfc3339_now(),
            provision_script: None,
        });
        save_meta(&paths, &meta).unwrap();

        assert_eq!(
            show_provision(&root, "rootfs", "with").unwrap(),
            "#!/bin/sh\necho hi\n"
        );
        let err = show_provision(&root, "rootfs", "without")
            .unwrap_err()
            .to_string();
        assert_eq!(err, "rootfs:without has no stored provision script");
        let err = show_provision(&root, "rootfs", "scratch")
            .unwrap_err()
            .to_string();
        assert!(err.contains("mutable overlay"), "{err}");
    }

    /// Seal real data into `name`'s scratch as a frozen layer `tag`, using only
    /// public petri-nbd APIs (mirrors what `auto_freeze` does in-process, minus
    /// the NBD wire). Returns the sealed `LayerId`.
    fn freeze_with_data(root: &Path, name: &str, tag: &str, writes: &[(u64, Vec<u8>)]) -> LayerId {
        let paths = ImagePaths::new(root, name);
        let meta = load_meta(&paths).unwrap();
        let scratch_meta = meta.scratch.clone().unwrap();
        let lower = lower_layers_for(root, scratch_meta.parent_id.as_deref()).unwrap();
        let geometry = match lower.first() {
            Some(layer) => layer.geometry(),
            None => default_geometry(scratch_meta.size_bytes).unwrap(),
        };
        let parents: Vec<LayerId> = scratch_meta
            .parent_id
            .as_deref()
            .map(|hex| LayerId::from_hex(hex).unwrap())
            .into_iter()
            .collect();
        let scratch = ScratchLayer::create(&paths.scratch_data(), geometry).unwrap();
        let mut disk = LayeredDisk::new(lower, scratch).unwrap();
        for (offset, bytes) in writes {
            disk.write_at(*offset, bytes).unwrap();
        }
        let store = paths.open_store().unwrap();
        let staging = paths
            .layers_root()
            .join(".staging")
            .join(format!("seal-{tag}"));
        let sealed = disk.seal_scratch(&staging, &parents).unwrap();
        let geom = sealed.geometry();
        let size = fs::metadata(&staging).map(|m| m.len()).unwrap();
        drop(sealed);
        let id = store.adopt_sealed(&staging).unwrap();
        record_frozen_layer(root, name, &id, size, geom, None, tag).unwrap();
        id
    }

    #[test]
    fn nbd_port_from_url_parses_loopback() {
        assert_eq!(nbd_port_from_url("nbd://127.0.0.1:5921/petri"), Some(5921));
        assert_eq!(nbd_port_from_url("nbd+unix:///petri?socket=/x"), None);
    }

    #[test]
    fn record_frozen_layer_appends_and_rolls_scratch() {
        let root = temp_root();
        create(&root, "rootfs", None, None).unwrap();
        let id = freeze_with_data(&root, "rootfs", "v1", &[(0, vec![0xAB; 100])]);

        let meta = load_meta(&ImagePaths::new(&root, "rootfs")).unwrap();
        let layer = meta.layer_by_tag("v1").expect("v1 layer recorded");
        assert_eq!(layer.id, id.to_hex());
        assert_eq!(layer.parent_id, None);
        // A fresh scratch now sits on the sealed layer.
        let scratch = meta.scratch.unwrap();
        assert_eq!(scratch.parent_id, Some(id.to_hex()));
        assert_eq!(scratch.nbd_port, None);
    }

    #[test]
    fn cross_store_chain_reads_base_data_through_derived() {
        let root = temp_root();
        create(&root, "base", None, None).unwrap();
        freeze_with_data(&root, "base", "v1", &[(0, vec![0xAB; 100])]);

        // Derived image whose scratch sits on base:v1 (no layer files copied).
        let msg = create(&root, "derived", Some(("base", "v1")), None).unwrap();
        assert!(msg.contains("based on 'base:v1'"), "{msg}");

        // Resolve the derived scratch's chain across stores and read base data.
        let derived = load_meta(&ImagePaths::new(&root, "derived")).unwrap();
        let parent = derived.scratch.unwrap().parent_id;
        let lower = lower_layers_for(&root, parent.as_deref()).unwrap();
        assert_eq!(lower.len(), 1, "base:v1 resolved as the single lower layer");
        let geometry = lower[0].geometry();
        let scratch =
            ScratchLayer::create(&ImagePaths::new(&root, "derived").scratch_data(), geometry)
                .unwrap();
        let mut disk = LayeredDisk::new(lower, scratch).unwrap();
        let mut buf = vec![0u8; 100];
        disk.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, vec![0xAB; 100], "base data shows through the chain");
    }

    #[test]
    fn auto_freeze_seals_live_scratch_and_rolls_new() {
        let root = temp_root();
        create(&root, "blank", None, None).unwrap();
        let handle = serve_scratch(&root, "blank").unwrap();
        // The served URL carries a real loopback port.
        assert!(nbd_port_from_url(handle.url()).is_some(), "{}", handle.url());

        let out = auto_freeze(&root, "blank", &handle, "snap", Some("#!/bin/sh\n".into())).unwrap();
        assert!(out.contains("frozen 'blank:snap'"), "{out}");
        assert!(out.contains("new scratch created"), "{out}");
        drop(handle);

        let meta = load_meta(&ImagePaths::new(&root, "blank")).unwrap();
        let layer = meta.layer_by_tag("snap").expect("snap layer recorded").clone();
        assert!(layer.provision_script.is_some());
        assert_eq!(meta.scratch.unwrap().parent_id, Some(layer.id));
    }

    #[test]
    fn provision_for_rebuild_requires_a_script() {
        let root = temp_root();
        create(&root, "app", None, None).unwrap();
        let paths = ImagePaths::new(&root, "app");
        let mut meta = load_meta(&paths).unwrap();
        meta.layers.push(LayerMeta {
            id: "a".repeat(64),
            tag: "v1".to_string(),
            parent_id: None,
            size_bytes: 0,
            created_at: rfc3339_now(),
            provision_script: Some("echo hi".to_string()),
        });
        meta.layers.push(LayerMeta {
            id: "b".repeat(64),
            tag: "bare".to_string(),
            parent_id: None,
            size_bytes: 0,
            created_at: rfc3339_now(),
            provision_script: None,
        });
        save_meta(&paths, &meta).unwrap();

        assert_eq!(provision_for_rebuild(&root, "app", "v1").unwrap(), "echo hi");
        assert_eq!(
            provision_for_rebuild(&root, "app", "bare")
                .unwrap_err()
                .to_string(),
            "app:bare has no stored provision script and cannot be rebuilt"
        );
        assert_eq!(
            provision_for_rebuild(&root, "app", "missing")
                .unwrap_err()
                .to_string(),
            "app:missing has no stored provision script and cannot be rebuilt"
        );
    }

    #[test]
    fn reset_scratch_over_base_requires_clean_scratch() {
        let root = temp_root();
        create(&root, "app", None, None).unwrap();
        let base_id = freeze_with_data(&root, "app", "base", &[(0, vec![0x01; 50])]);
        // Now app has a scratch over base. Freeze again so the scratch sits over v1.
        freeze_with_data(&root, "app", "v1", &[(0, vec![0x02; 50])]);

        // Scratch sits over v1, not base -> conflict.
        let err = reset_scratch_over_base(&root, "app", "app", "base")
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists over a different parent"), "{err}");

        // Delete the scratch, then reset over base succeeds.
        delete(&root, "app", "scratch", true).unwrap();
        reset_scratch_over_base(&root, "app", "app", "base").unwrap();
        let meta = load_meta(&ImagePaths::new(&root, "app")).unwrap();
        assert_eq!(meta.scratch.unwrap().parent_id, Some(base_id.to_hex()));
    }

    #[test]
    fn create_base_must_be_frozen_not_scratch() {
        let root = temp_root();
        create(&root, "parent", None, None).unwrap();
        // parent has only scratch, no frozen layers yet.
        let err = create(&root, "child", Some(("parent", "scratch")), None)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "\"parent:scratch\" is not frozen and cannot be used as a base"
        );
        let err = create(&root, "child", Some(("parent", "v1")), None)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "\"parent:v1\" is not frozen and cannot be used as a base"
        );
    }
}
