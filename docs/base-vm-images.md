# Base VM Images

Petri base images are Linux VM bundles with `petri-guest` installed and wired to
the host contract used by the macOS MVP backend. The backend consumes a bundle
directory through `petri create --image <bundle>`.

## Bundle Layout

`scripts/build-base-image.sh` writes this layout:

```text
target/petri-images/base/
  petri-image.json
  root.img
  vmlinuz
  initrd.img        # optional
  build-info.json
  SHA256SUMS
```

`petri-image.json` is the runtime manifest consumed by `crates/petri`. It keeps
paths relative to the bundle so the whole directory can move as one artifact.
`build-info.json` is audit metadata and is not required to boot the VM.

## Rebuild

Install the image build prerequisites on a Linux builder:

```sh
sudo apt-get install -y jq mmdebstrap libguestfs-tools
rustup target add aarch64-unknown-linux-musl
```

Build the default macOS-compatible ARM64 image through the Petri CLI:

```sh
petri image build
```

The default config lives at
`images/base/petri-base-image.toml`. It pins the Debian suite, snapshot mirror,
guest Rust target, dispatch port, installed package set, and disk size. To build
a local variant:

```sh
petri image build \
  --config images/base/petri-base-image.toml \
  --out-dir target/petri-images/base-dev \
  --disk-size 4G
```

The CLI delegates to `scripts/build-base-image.sh`. Set
`PETRI_IMAGE_BUILD_SCRIPT=/path/to/build-base-image.sh` if the script is
installed outside the source checkout.

## Guest Installation

The pipeline builds `petri-guest` for the configured musl target and installs it
at `/usr/local/bin/petri-guest`. The image enables these systemd units:

- `workspace.mount` mounts the host workspace virtio-fs share at `/workspace`.
- `run-petri.mount` mounts the read-only config share at `/run/petri`.
- `petri-guest.service` starts the agent with
  `--policy /run/petri/policy.toml --transport vsock --vsock-port 7777`.

Those paths match the host backend constants in `crates/petri/src/backend.rs`.

## Auditability

Every build writes `SHA256SUMS` for boot artifacts and `build-info.json` with:

- source git revision and dirty-worktree flag
- config path
- Debian suite and snapshot mirrors
- guest Rust target and guest binary SHA-256
- disk size and build timestamp

For release builds, run from a clean worktree and archive the entire bundle
directory. Consumers should verify `sha256sum -c SHA256SUMS` before use.

## Use With Host MVP

After building the bundle, pass it to `petri create`:

```sh
petri create \
  --id dev-1 \
  --workspace /absolute/workspace \
  --policy /absolute/policy.toml \
  --image target/petri-images/base \
  --backend macos
```

The VM boots the bundled kernel and disk, mounts the workspace and immutable
policy config, then waits for dispatch requests over vsock port `7777`.
