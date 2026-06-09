# petri-nbd: Layered Block Storage for VM Images

This document is the design for `petri-nbd`, a layered block-device backend that
lets Petri run many VM agents from shared immutable image layers plus a
per-agent writable scratch layer. It is the design deliverable named in
[issue #22](https://github.com/sbeardsley/petri/issues/22) and the storage
companion to the macOS Virtualization.framework backend described in
[ADR 0001](adr/0001-petri-architecture.md).

The goal of this document is to fix the **boundary and on-disk semantics** so the
Rust layer store, the NBD server, and the Swift AVF integration can be built
independently against a stable contract. It does not lock the final block
format; it locks the interfaces and the invariants the format must satisfy.

Status: **Draft / design phase.** No production code depends on this yet.

---

## 1. Motivation

Petri boots agents from base VM image bundles
([Base VM Images](base-vm-images.md)). Today each run consumes a `root.img`
disk attached to the guest as a `VZVirtioBlockDeviceConfiguration` backed by a
`VZDiskImageStorageDeviceAttachment` (see
`crates/petri-vz/Sources/petri-vz/main.swift`). To run an agent against a clean
base today, the host must either share one mutable disk (unsafe, not isolated)
or copy the whole `root.img` per run (slow, space-hungry).

The workflow we want:

- keep exactly **one** local copy of each base / toolchain / runtime layer,
- give each agent an **isolated writable scratch disk**,
- optionally **seal** a scratch disk into a new immutable layer,
- run future agents from `base + builder + runtime + snapshot + fresh scratch`,
- eventually distribute layers as content-addressed artifacts or OCI-style
  registry objects.

Apple's Virtualization.framework does **not** expose Parallels-style
linked-clone or differencing-disk APIs for `VZDiskImageStorageDeviceAttachment`.
It does support attaching a disk through
`VZNetworkBlockDeviceStorageDeviceAttachment`, where the guest acts as an NBD
client and Petri supplies the backing block semantics in Rust. That makes NBD a
natural boundary for putting layer composition under our control.

---

## 2. Boundary and ownership

| Side | Owns |
|---|---|
| **Rust (`petri-nbd`)** | The layer store, block composition (read resolution + write routing), scratch lifecycle, sealing, the NBD server, and later registry integration. |
| **Swift (`petri-vz`)** | `VZVirtualMachineConfiguration` and attaching the NBD endpoint via `VZNetworkBlockDeviceStorageDeviceAttachment`. It is a dumb NBD *client host*: it is handed a URL and attaches it. |
| **Rust (`petri`)** | Orchestration: resolve layers, create a scratch overlay, start the NBD server, launch `petri-vz` with the NBD URL, tear down, and discard or seal the scratch. |

The contract between Rust and Swift is a **URL plus a read-only flag**, nothing
more. Swift never learns about layers, extents, sealing, or content hashes.
This mirrors the existing host/guest split: the backend hands the helper a
flat set of CLI arguments and the helper attaches devices
(`crates/petri/src/backend.rs`).

```text
petri (orchestrator, Rust)
  | resolve layers, create scratch
  v
petri-nbd server (Rust)            <--- NBD --->   guest (NBD client in VM)
  ^                                                     ^
  | spawns + passes nbd URL                             | attaches NBD disk
  |                                                      |
  +------------------ petri-vz (Swift) -----------------+
                      VZNetworkBlockDeviceStorageDeviceAttachment
```

---

## 3. Layer model

### 3.1 Stack

A **stack** is an ordered list of layers. Lower layers are immutable and
shareable; the top layer is a per-run writable scratch overlay.

```text
base.raw / base layer          read-only, shared       (bottom)
builder layer                  read-only, shared
runtime layer                  read-only, shared
scratch overlay                writable, per agent run  (top)
```

Every layer in a stack describes the **same virtual disk** (same virtual size
and block size). A layer only stores the blocks it actually populates; the rest
are "holes" that fall through to the layer below.

### 3.2 Read / write semantics (invariants)

These are the invariants every implementation of the format must uphold:

1. **Top-down read resolution.** A read for a block walks the stack from the top
   layer down. The first layer that has populated that block wins. This is
   resolved per block, not per request: a single read can be satisfied by a mix
   of layers.
2. **Holes read as zeroes.** A block populated by no layer reads as all zeroes.
3. **Writes go to the top.** Writes always land in the top writable scratch
   layer. Lower layers are never modified during a run.
4. **Lower layers are immutable and shareable.** Many concurrent VMs may map the
   same immutable layer files read-only. The format must make this safe (no
   in-place mutation, no exclusive locks on read-only layers).
5. **Flush is durable for scratch.** A flush/`fsync` from the guest must make
   prior scratch writes durable to the extent the crash-consistency model
   (§7) promises.

### 3.3 Block size and addressing

v0 uses **fixed-size blocks**, not variable extents. Fixed blocks keep read
resolution and the populated-block index simple, which matters more than space
efficiency for the prototype.

- **Block size: 64 KiB** for v0 stored blocks. Rationale: large enough to keep
  per-block metadata small for multi-GiB disks (a 16 GiB disk is 256 K blocks),
  while still a clean multiple of the 4 KiB guest page / filesystem block size.
- NBD requests arrive at arbitrary offset and length. The server translates each
  request into the set of 64 KiB blocks it touches and performs read-modify-write
  on the scratch layer for sub-block writes.
- Reads and writes that are not block-aligned are first-class, not an error path.
  They are tested explicitly (Milestone 1).

> Open question retained: whether to move to variable extents or a smaller 4 KiB
> block once real boot/build workloads are measured (§9, §11 Milestone 5). The
> on-disk format carries an explicit `block_size`, so a later layer can choose a
> different size without breaking the reader.

---

## 4. On-disk format

A layer is a directory (or a pair of files) with **separate metadata and data**.
The durable format is **not** an in-memory `HashMap<u64, Block>` — that is
acceptable only inside unit tests.

```text
<layer-id>/
  layer.meta        # metadata: versioned, small, fsync'd on seal
  layer.data        # block payloads: sparse file or packed blob
```

### 4.1 `layer.meta`

```text
schema_version       u32     format version of this metadata
virtual_size         u64     virtual disk size in bytes (same across the stack)
block_size           u32     stored block size in bytes (64 KiB for v0)
parent_ids           [ID]    content IDs of the layers this was sealed on top of
content_id           ID      stable content identity of this layer (sealed only)
state                enum    { writable, sealed }
block_index          ...     map: virtual block number -> location in layer.data
created_at, builder  ...     creation metadata (informational, excluded from ID)
```

The **block index** is the populated-block map. For v0 it is a sorted list of
`(virtual_block_number -> data_offset)` entries; absence means "hole, fall
through." A sorted index gives `O(log n)` lookup and streams cleanly to disk. A
writable scratch layer keeps this index in memory and appends to `layer.data`;
sealing writes the final index out and fsyncs.

### 4.2 `layer.data`

Two viable representations; v0 picks one and the format records which:

- **Sparse raw file** — `layer.data` is a virtual-size-shaped file with holes;
  block N lives at offset `N * block_size`. Pros: trivial mapping, OS-level
  sparseness, `pread`/`pwrite` directly. Cons: relies on filesystem hole support
  (APFS supports sparse files); `du` vs apparent size can confuse tooling.
- **Packed blob** — populated blocks are appended densely and the `block_index`
  maps virtual block -> packed offset. Pros: compact, explicit, portable to a
  content-addressed chunk store later. Cons: needs the index to read anything.

**v0 decision:** scratch overlays use a **packed blob with an append log**
(simple to grow and to seal), and the base raw image is consumed as a **sparse
raw file** directly (it already exists as `root.img`). Sealing a scratch
produces a packed-blob immutable layer. This keeps the writable path append-only
(good for crash consistency, §7) and lets the base image be used untouched.

---

## 5. Public Rust API (v0)

Crate layout:

```text
crates/petri-nbd/
  src/
    lib.rs          # crate root, re-exports
    layer.rs        # immutable layer + writable overlay representations
    stack.rs        # LayeredDisk: top-down read resolution, top-layer writes
    server.rs       # NBD server lifecycle (bind, accept, serve, shutdown)
    protocol.rs     # NBD protocol handling or adapter to an existing crate
    store.rs        # layer store: resolve by ID, seal, ref-tracking (later)
```

The core composition type is `LayeredDisk`, which is the unit Milestone 1 builds
and tests independently of NBD:

```rust
/// A composed, addressable virtual disk: read-only lower layers plus one
/// writable scratch overlay on top.
pub struct LayeredDisk { /* ... */ }

impl LayeredDisk {
    /// Compose a stack: lower layers (bottom-first) plus a writable top.
    pub fn new(lower: Vec<ImmutableLayer>, scratch: ScratchLayer) -> io::Result<Self>;

    pub fn virtual_size(&self) -> u64;
    pub fn block_size(&self) -> u32;

    /// Read `buf.len()` bytes at `offset`. Holes are zero-filled.
    /// Arbitrary (unaligned) offset/length are supported.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;

    /// Write `buf` at `offset` into the scratch overlay only.
    /// Sub-block writes do read-modify-write against the resolved stack.
    pub fn write_at(&mut self, offset: u64, buf: &[u8]) -> io::Result<()>;

    /// Make prior scratch writes durable per the crash-consistency model.
    pub fn flush(&mut self) -> io::Result<()>;

    /// Optional NBD ops, gated on backend support (§6).
    pub fn write_zeroes(&mut self, offset: u64, len: u64) -> io::Result<()>;
    pub fn trim(&mut self, offset: u64, len: u64) -> io::Result<()>;
}
```

Layer lifecycle:

```rust
pub struct ScratchLayer { /* append-log packed blob + in-memory index */ }

impl ScratchLayer {
    /// Create a fresh empty writable overlay for a given virtual geometry.
    pub fn create(dir: &Path, virtual_size: u64, block_size: u32,
                  parents: &[LayerId]) -> io::Result<Self>;

    /// Seal: fsync data + metadata, mark immutable, compute content_id,
    /// and return the resulting immutable layer.
    pub fn seal(self) -> io::Result<ImmutableLayer>;
}
```

NBD server:

```rust
pub struct NbdServer { /* ... */ }

impl NbdServer {
    /// Bind a localhost endpoint and serve `disk` until shutdown.
    /// `read_only` exports a fully-immutable stack with no scratch.
    pub fn serve(disk: LayeredDisk, opts: ServeOpts) -> io::Result<NbdHandle>;
}

pub struct ServeOpts { pub bind: BindMode, pub export_name: String, pub read_only: bool }
pub enum BindMode { LoopbackTcp(/* port: 0 = auto */ u16), UnixSocket(PathBuf) }

pub struct NbdHandle { /* ... */ }
impl NbdHandle {
    pub fn url(&self) -> String;     // e.g. "nbd://127.0.0.1:<port>/<export>"
    pub fn shutdown(self) -> io::Result<()>;
}
```

---

## 6. NBD protocol surface

`petri-nbd` must handle real block-device behavior, not just happy-path
read/write.

Required:

- **Reads** at arbitrary offset and length.
- **Writes** routed to scratch only; sub-block writes do read-modify-write.
- **Flush / FUA** — honor `NBD_CMD_FLUSH` and the FUA flag to drive `flush()`.
- **Disconnect** — `NBD_CMD_DISC` and abrupt socket close both tear the
  connection down cleanly without corrupting scratch.
- **Bounds checking** — out-of-range offset/length is rejected with the correct
  NBD error, never an out-of-bounds host access.
- **Read-only exports** — a fully-immutable stack (no scratch) advertises
  read-only; writes to it are rejected.

Optional, advertised only if the chosen backend supports them:

- **Write zeroes** (`NBD_CMD_WRITE_ZEROES`) — punch a zeroed region in scratch.
- **Trim / discard** (`NBD_CMD_TRIM`) — drop scratch blocks back to "hole."

### 6.1 Implementation strategy

Milestone 2 chooses between:

- **An existing Rust NBD crate**, if one cleanly supports a custom block backend
  (a `read_at` / `write_at` / `flush` device trait), newstyle negotiation, and
  the flush/trim commands above. Preferred if it exists and is maintained.
- **A minimal in-tree NBD server**, if the protocol surface we need is small.
  The newstyle handshake plus `READ` / `WRITE` / `FLUSH` / `DISC` (and optionally
  `WRITE_ZEROES` / `TRIM`) is a contained surface. An in-tree server keeps the
  device trait identical to `LayeredDisk` and avoids an external dependency's
  threading model.

The `protocol.rs` / `server.rs` split exists so the backend choice does not leak
into composition (`stack.rs`).

### 6.2 Decision (Milestone 2): minimal in-tree synchronous server

**Chosen: a minimal in-tree NBD server.** Rationale from the crate survey:

- The one actively-maintained Rust NBD *server* framework, `tokio-nbd`, is
  async-only. This workspace **deliberately avoids tokio** — `petri-guest`
  documents "std without tokio" and there is no tokio anywhere in `Cargo.lock`.
  Pulling in a full async runtime for one localhost block server contradicts the
  project's lean, synchronous-std posture.
- The remaining synchronous crate (`nbd` / vi's `rust-nbd`) is old, explicitly
  server-incomplete, and models a device as a single `Read + Write + Seek`
  object — which does not map cleanly onto our per-block layered device or its
  flush/trim semantics.
- The protocol surface we actually need is small: the fixed-newstyle handshake
  (`NBD_OPT_EXPORT_NAME` and `NBD_OPT_GO`) plus the simple-reply transmission
  commands `READ` / `WRITE` / `FLUSH` / `DISC`, with optional `WRITE_ZEROES` /
  `TRIM`. An in-tree server keeps the device interface identical to
  `LayeredDisk`, advertises exactly the commands we implement (important for the
  Milestone 3 AVF experiment), and adds zero dependencies.

The server is **thread-per-connection, blocking IO** (`std::net` + `std::thread`),
consistent with the rest of the host. The composed disk is shared behind a
`Mutex`; writes serialize against reads, which is correct for a single-client
block export. If profiling later shows the global lock is a bottleneck, the lock
can be narrowed without changing the protocol layer.

---

## 7. Crash consistency

The writable scratch layer is the only thing that mutates during a run, so it is
the only crash-consistency concern. Immutable layers are read-only and need no
recovery.

Model for v0:

- Scratch `layer.data` is **append-only**: a write appends a fresh block payload
  and updates the in-memory index to point at it. Earlier versions of an
  overwritten block remain in the file as garbage until seal/compaction. This
  means a torn write damages at most the in-flight block, never a previously
  durable one.
- `flush()` (driven by `NBD_CMD_FLUSH`/FUA) fsyncs `layer.data` and then persists
  the index increment (index journal or rewrite-and-fsync). The guest filesystem
  already issues flushes at its own barriers; we honor them.
- On unclean shutdown without a final seal, the scratch is treated as
  **disposable**. v0 does not promise to recover a half-written scratch into a
  consistent reusable overlay; it promises that lower immutable layers are
  untouched and that a fresh scratch yields a clean base. This is acceptable
  because scratch is per-run and cheap to recreate.
- **Crash behavior is documented, not silently best-effort.** Sealing is the
  durability boundary: only `seal()` fsyncs data + metadata and publishes a
  content ID. An unsealed scratch carries no durability guarantee across a host
  crash.

A future version may add an index journal that makes an unsealed scratch
crash-recoverable; the append-only data layout is chosen so that upgrade does
not require rewriting the data format.

---

## 8. Sealing and content identity

`seal_scratch()` converts a writable overlay into an immutable layer:

1. Flush and fsync `layer.data`.
2. Write the final `block_index` and metadata, then fsync `layer.meta`.
3. Compute a **stable content ID** over the layer's content.
4. Mark the layer `sealed` (immutable) and move it to its content-addressed
   location in the store.

### 8.1 Content ID stability

The content ID must depend only on **what the layer means as a disk**, so that
identical content yields an identical ID and informational metadata churn does
not destabilize identity.

The ID is computed over:

- `virtual_size`, `block_size`,
- the **parent content IDs** (in order),
- the populated set as a canonical sequence of `(virtual_block_number, block_hash)`
  sorted by block number, where `block_hash` is a hash of the block payload.

The ID **excludes** `created_at`, builder identity, file mtimes, and the
physical packing order in `layer.data`. Two seals of byte-identical content on
top of identical parents produce the same content ID. This is the property that
later makes content-addressed distribution and dedupe possible (§10).

Hash function: a single, explicit choice (e.g. BLAKE3) recorded in the format,
with the algorithm tagged in the ID so it can evolve.

---

## 9. CLI and `petri run` integration

Do **not** start with registry push/pull. First prove local layered boot.

Sketch of commands after v0:

```sh
petri layer ls
petri run --layer base --layer builder --layer node:20 -- <command>
petri snapshot <sandbox-id> --tag my-snapshot
```

Internal `petri run` flow:

```text
1. resolve local layers (bottom-first) and validate shared geometry
2. create a fresh writable scratch overlay on top
3. start the petri-nbd server for the composed stack -> get nbd URL
4. launch petri-vz with the NBD disk URL (disk_mode = nbd)
5. wait for the VM lifecycle to complete
6. stop the NBD server
7. discard the scratch, or seal it into a new immutable layer
```

This slots into the existing backend (`crates/petri/src/backend.rs`), which today
resolves a bundle and passes `--disk` to the helper. The NBD path adds an
alternative disk mode rather than replacing the bundle path.

---

## 10. Swift / AVF integration

Extend `petri-vz` to support an NBD-backed disk mode using
`VZNetworkBlockDeviceStorageDeviceAttachment`, alongside the existing
`VZDiskImageStorageDeviceAttachment` path in
`crates/petri-vz/Sources/petri-vz/main.swift`.

Expected helper input (exact CLI/JSON schema TBD, modeled on current flags):

```json
{
  "disk_mode": "nbd",
  "nbd_url": "nbd://127.0.0.1:<port>/<export>",
  "memory_bytes": 2147483648,
  "cpu_count": 2
}
```

Constraints:

- **Verify the accepted URI forms first.** Do not assume a Unix socket path is
  valid for `VZNetworkBlockDeviceStorageDeviceAttachment` on the target macOS
  versions. The `BindMode` enum in §5 exists so the server can offer loopback
  TCP and/or a Unix socket and the helper can use whichever AVF accepts. This is
  resolved empirically in Milestone 3 (§9, §11) and recorded in §12.
- **Entitlements.** The helper currently holds only
  `com.apple.security.virtualization` (`crates/petri-vz/petri-vz.entitlements`).
  An NBD client over the network attachment will likely also need
  `com.apple.security.network.client`. Add it when the smoke test requires it,
  not speculatively.
- **VM-queue discipline.** All `VZVirtualMachine` and socket interactions must
  happen on the VM queue (currently the main queue), consistent with the
  existing helper. The NBD attachment is configured during VM configuration on
  that queue like every other device.

---

## 11. Prototype milestones

### Milestone 1 — Local block stack
- Implement `LayeredDisk` over one read-only raw base plus one writable sparse
  overlay.
- Unit tests for aligned **and** unaligned reads/writes.
- Overlay reads shadow base reads.
- Missing regions read as zeroes.
- Flush persists overlay state.

### Milestone 2 — NBD export
- Pick the NBD strategy (existing crate vs. minimal in-tree server, §6.1).
- Export `LayeredDisk` over localhost NBD.
- Integration tests with an NBD client where available.

### Milestone 3 — AVF boot smoke test
- Extend `petri-vz` to attach `VZNetworkBlockDeviceStorageDeviceAttachment`.
- Boot an existing raw Petri image via NBD with a scratch overlay.
- Confirm guest writes land in scratch, not base.
- Reboot with the same scratch -> persistence.
- Reboot with fresh scratch -> clean base.
- Record which NBD URI form AVF accepted and which entitlements were required.

### Milestone 4 — Seal snapshot
- Add `seal_scratch()` to convert a writable overlay into an immutable layer.
- Store parent IDs and content ID.
- Boot from `base + sealed + fresh scratch` and verify sealed changes are visible.

### Milestone 5 — Performance and cleanup
- Measure boot time and an image-build workload vs. raw disk attachment.
- Add cache cleanup and layer reference tracking.
- Document when APFS clone-based local copies beat NBD layering (§13).

---

## 12. Open questions

Tracked here and resolved as milestones land; answers are folded back into the
relevant section above.

| # | Question | Resolved in |
|---|---|---|
| 1 | Which NBD URI forms does `VZNetworkBlockDeviceStorageDeviceAttachment` accept on target macOS? | Milestone 3 (§10) |
| 2 | Fixed-size blocks, variable extents, or both? | v0 fixed (§3.3); revisit Milestone 5 |
| 3 | v0 block size: 4 KiB, 64 KiB, or NBD-request-aligned? | v0 64 KiB (§3.3); revisit Milestone 5 |
| 4 | Immutable layers: sparse files, packed blobs, or content-addressed chunks? | v0 packed blob seal + sparse base (§4.2); chunks later (§10/§13) |
| 5 | Crash consistency for the writable scratch layer? | v0 disposable scratch + seal boundary (§7) |
| 6 | How are layer IDs computed so metadata churn does not destabilize identity? | §8.1 |
| 7 | How much registry/OCI compatibility after the local design is proven? | Deferred; not in v0 (§9) |

---

## 13. Tradeoffs vs. APFS clones

Before expanding into registry support, the local design must be justified
against the simpler alternative: `clonefile()` / APFS copy-on-write clones of
`root.img`.

| Dimension | APFS clone of `root.img` | petri-nbd layered stack |
|---|---|---|
| Per-run setup | One `clonefile()`, near-instant, CoW | Create scratch + start NBD server |
| Space | CoW until divergence; one clone per run | One copy per immutable layer, shared across runs; scratch is per run |
| Multi-layer composition | None — flat image per clone | Native: base + builder + runtime + snapshot |
| Sealing a derived image | Manual: copy + freeze a whole disk | `seal_scratch()` -> content-addressed layer |
| Distribution later | Whole-image artifacts | Content-addressed layers, OCI-style (future) |
| Runtime cost | Native block device, no server | NBD hop + server process per run |
| Platform reach | macOS/APFS only | NBD is portable to other backends |

The honest read: for a **single flat base with no composition**, an APFS clone is
simpler and faster at runtime, and Petri should keep using direct disk
attachment there. `petri-nbd` earns its place when there is **real layer
composition** (shared toolchain/runtime layers, sealed snapshots reused across
many agents) or a need for a portable, content-addressed distribution story.
Milestone 5 measures the runtime cost so this tradeoff is quantified, not
asserted.

---

## 14. Acceptance criteria (issue #22)

- [x] A design document exists for local NBD layered disks. *(this document)*
- [ ] A prototype boots a Petri VM from `base raw + scratch overlay` through AVF NBD.
- [ ] Guest writes are isolated to scratch.
- [ ] A scratch overlay can be discarded for a clean run.
- [ ] A scratch overlay can be sealed and reused as a read-only top layer.
- [ ] Tradeoffs vs. APFS clones are documented before expanding into registry support. *(§13)*
