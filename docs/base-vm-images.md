# Base VM Images

Petri base images are Linux VM bundles with `petri-guest` installed and wired to
the host contract used by the macOS MVP backend. The backend consumes a bundle
directory through `petri create --image <bundle>`.

For custom bundle requirements and upgrade guidance, see
[Custom Image Compatibility](custom-image-compatibility.md).

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
The default base image is a direct Linux boot bundle with `vmlinuz`, optional
`initrd.img`, and `root.img`. `build-info.json` is audit metadata and is not
required to boot the VM.

## Rebuild On Linux

Install the image build prerequisites on a Linux builder:

```sh
sudo apt-get install -y e2fsprogs mmdebstrap python3
rustup target add aarch64-unknown-linux-musl
```

Build the default macOS-compatible ARM64 image through the Petri CLI:

```sh
petri image build
```

The default config lives at `images/base/petri-base-image.toml`. It currently
builds a Debian trixie ARM64 base image and pins the Debian suite and mirrors,
guest Rust target, dispatch port, installed package set, and disk size. To
build a local variant:

```sh
petri image build \
  --config images/base/petri-base-image.toml \
  --out-dir target/petri-images/base-dev \
  --disk-size 4G
```

The CLI delegates to `scripts/build-base-image.sh`. Set
`PETRI_IMAGE_BUILD_SCRIPT=/path/to/build-base-image.sh` if the script is
installed outside the source checkout.

## Rebuild On macOS With A Builder VM

On macOS, `petri image build --builder vm` runs the same Linux image build
script inside a Petri-managed Linux builder VM. The builder VM is intentionally
kept slim: it needs Linux image-building tools, `bash`, `git`, and
`petri-guest`, but it does not need Rust, Cargo, or rustup. The host builds
`petri-guest` for the configured Linux target and passes that binary into the
builder script with `--skip-guest-build`.

Provide a prepared builder bundle with `--builder-image` or
`PETRI_BUILDER_IMAGE`:

```sh
petri image build \
  --builder vm \
  --builder-image target/petri-builder \
  --out-dir target/petri-images/base-dev
```

`--builder auto` is the default. It selects the direct Linux path on Linux and
the VM builder path on macOS. On macOS, auto mode still requires
`--builder-image` or `PETRI_BUILDER_IMAGE`.

The VM builder mounts the source checkout as `/workspace`, runs
`scripts/build-base-image.sh` inside the guest, stages the bundle under
`target/petri-builder-output`, then copies the completed bundle to the requested
macOS `--out-dir`. Paths passed to `--config` must live under the source
checkout so they are visible to the builder VM.

Petri tears down the transient builder VM after each build. Staged output is
left under `target/petri-builder-output` only while the build is running; the
requested `--out-dir` is replaced after the guest build exits successfully. If
the guest exits non-zero, Petri reports the dispatch status, exit code, stdout,
and stderr from the builder command.

Prepare the reusable builder bundle on macOS with:

```sh
./scripts/build-image-builder.sh
```

By default Petri downloads the official Debian 12 Bookworm ARM64 NoCloud raw
image from `cloud.debian.org` for the reusable builder VM, verifies it against
the upstream `SHA512SUMS`, expands the copy to the requested `--disk-size` or
16 GiB, and provisions it on first boot. Downloads are cached in
`target/petri-builder-cache`; override the source with
`--builder-source <url-or-path>` and provide
`--builder-source-sha256 <hex>` or `--builder-source-checksums <path-or-url>`
for non-default sources.

The script builds `crates/petri-vz`, builds the `petri` host CLI, sets
`PETRI_VZ_BIN` to the freshly built helper, then runs
`petri image build --prepare-builder --builder-image target/petri-builder`.

The prepared builder bundle is a slim EFI-boot image provisioned with:

- `petri-guest` plus the systemd units/mounts needed for Petri dispatch
- `bash`, `e2fsprogs`, `git`, `mmdebstrap`, `python3`, and `sha256sum`
- network access for Debian package downloads during image builds

Petri stages the bundle next to the requested output path, boots and validates
the staged image, removes the transient NoCloud seed disk, writes
`build-info.json` and `SHA256SUMS`, then atomically replaces the output bundle.
On failure, the previous output bundle is left untouched and the staging path is
reported for debugging.

Do not install Rust tooling into the default builder bundle unless you
explicitly want in-guest guest-agent compilation. Keeping Rust on the host avoids
adding multiple gigabytes to the reusable builder image.

The reusable builder image acts as the cache boundary. Debian package caches,
guest-side tool caches, and any other persistent builder state should live on
that image; the source checkout mount should be treated as build input/output
only.

## Guest Installation

The pipeline builds `petri-guest` for the configured musl target and installs it
at `/usr/local/bin/petri-guest`. The image enables these systemd units:

- `workspace.mount` mounts the host workspace virtio-fs share at `/workspace`.
- `run-petri.mount` mounts the read-only config share at `/run/petri`.
- `petri-guest.service` starts the agent with
  `--policy /run/petri/policy.toml --transport vsock --vsock-port 7777`. When LSP
  support is enabled the unit also passes `--lsp-config /etc/petri/lsp.toml`.

Those paths match the host backend constants in `crates/petri/src/backend.rs`.

## Language Server Pre-Install

When the image config enables `[lsp]`, the build script provisions the
configured language servers into the rootfs and bakes a runtime config at
`/etc/petri/lsp.toml` (the `enabled` flag plus each server's `language`,
`binary`, and `args` — the build-only `install` and `apt_packages` keys are
dropped). The default base image ships servers for Rust (`rust-analyzer`),
TypeScript/JavaScript (`typescript-language-server`), Python (`pylsp`), Go
(`gopls`), and C/C++ (`clangd`).

Each server's `install` command runs inside the target rootfs, so this stage of
the build requires network access and the ability to execute the target
architecture (natively or via binfmt/qemu) under `chroot`. Server binaries are
placed on `PATH` at `/usr/local/bin`. The guest exposes them through the
`lsp_*` tools documented in
[Vsock Dispatch Protocol](vsock-dispatch-protocol.md#lsp-tools); set
`enabled = false` (or omit the `[lsp]` section) to build an image without LSP
support, in which case every `lsp_*` request degrades gracefully.

## Auditability And Verification

Every build writes `SHA256SUMS` for boot artifacts and `build-info.json` with:

- source git revision and dirty-worktree flag
- config path
- Debian suite and mirrors
- guest Rust target and guest binary SHA-256
- disk size and build timestamp

For release builds, run from a clean worktree and archive the entire bundle
directory. Consumers should verify the checksums and inspect the build metadata
before use:

```sh
cd target/petri-images/base
sha256sum -c SHA256SUMS
cat build-info.json
```

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
