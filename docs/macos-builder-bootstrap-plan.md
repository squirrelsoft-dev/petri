# macOS Builder Bootstrap Implementation Plan

## Goal

Create the missing bootstrap implementation for `petri image build --prepare-builder`
so a macOS developer can create the first reusable Petri builder VM image from a
small upstream Linux VM artifact, then use that builder image for future
`petri image build --builder vm` runs.

The already implemented VM build path expects a prepared builder bundle. This
plan covers everything still needed to produce that bundle.

## Desired User Flow

```sh
petri image build --prepare-builder --builder-image target/petri-builder
petri image build --builder vm --builder-image target/petri-builder
```

Defaults:

- `--prepare-builder` writes a builder bundle to `--builder-image`, or to
  `$PETRI_BUILDER_IMAGE` when the flag is omitted.
- The bootstrap source is an official Debian ARM64 RAW cloud/nocloud image.
- The default builder image does not install Rust, Cargo, or rustup.
- The host remains responsible for building `petri-guest`; the builder VM only
  runs Linux image-building tools.

## Bundle Contract

The prepared builder bundle should look like a normal Petri image bundle so the
existing macOS backend can boot it:

```text
target/petri-builder/
  petri-image.json
  root.img
  build-info.json
  SHA256SUMS
```

Unlike the current base image bundle, the builder bundle may boot from disk
firmware instead of a host-supplied kernel/initrd. If that route is used,
`petri-image.json` needs an explicit boot mode field, for example:

```json
{
  "architecture": "aarch64",
  "boot_mode": "efi",
  "disk": "root.img",
  "dispatch_port": 7777
}
```

If Petri instead extracts and supplies the distro kernel/initrd from the
bootstrap image, the existing `kernel`, `initrd`, and `kernel_command_line`
fields can remain mandatory for Linux-boot bundles. The EFI path is preferred
for cloud images because it avoids host-side kernel extraction.

## Implementation Work

### CLI And Orchestration

- Replace the current `--prepare-builder` placeholder error with a real
  preparation path.
- Require an output bundle location from `--builder-image` or
  `PETRI_BUILDER_IMAGE`; fail clearly if neither is set.
- Add optional bootstrap-source inputs:
  - `--builder-source <url-or-path>` for the Debian RAW image.
  - `--builder-source-sha256 <hex>` or checksum-file support.
  - Environment equivalents only if useful for CI.
- Store downloads and intermediate files under `target/petri-builder-cache` by
  default.
- Make preparation idempotent:
  - Reuse a verified cached upstream image.
  - Replace the output builder bundle atomically after successful provisioning.
  - Leave the old builder bundle untouched on failure.

### Source Image Acquisition

- Choose and document the default Debian ARM64 image channel.
- Download over HTTPS when the source is a URL.
- Verify checksum before booting or copying the image.
- Reject unsupported formats unless conversion support is implemented.
- Prefer RAW images because Apple Virtualization supports RAW and ASIF disk
  image attachments directly.

### Disk Preparation

- Copy the verified upstream RAW image to the builder bundle as `root.img`.
- Expand or sparse-allocate it to the configured builder disk size.
- Decide whether expansion is done on macOS before boot or inside the first boot.
- Preserve enough free space for:
  - `mmdebstrap`
  - `libguestfs-tools`
  - package cache
  - temporary rootfs/image output during builds
- Write `build-info.json` with:
  - upstream image URL/path
  - upstream checksum
  - provisioned package list
  - Petri git revision and dirty flag
  - builder image version/schema
  - build timestamp

### Virtualization.framework Boot Support

- Extend `petri-vz` to support EFI/disk boot for builder bundles.
- Extend `ImageManifest` parsing to accept either:
  - existing Linux direct boot: `kernel`, optional `initrd`,
    `kernel_command_line`, `disk`
  - new EFI boot: `boot_mode = efi`, `disk`
- Configure `VZEFIBootLoader` for EFI bundles.
- Add any required EFI variable store handling.
- Keep the existing kernel/initrd boot path unchanged for current base images.

### First-Boot Provisioning

- Provide first-boot configuration to the Debian image. Preferred options:
  - NoCloud/cloud-init seed attached as a second disk.
  - A temporary virtio-fs config share consumed by a first-boot service.
- Provision the builder with:
  - `petri-guest`
  - systemd units for `workspace.mount`, `run-petri.mount`, and
    `petri-guest.service`
  - `bash`
  - `git`
  - `jq`
  - `mmdebstrap`
  - `libguestfs-tools`
  - `sha256sum`/`coreutils`
  - required virtio-fs and vsock kernel/module support
- Do not install Rust tooling in the default image.
- Enable network access for package installation during preparation and for
  future Debian package downloads during builds.
- Mark provisioning completion with a durable file such as
  `/var/lib/petri-builder/provisioned.json`.

### Host/Guest Readiness

- Boot the copied source image with provisioning config attached.
- Wait for the provisioning completion signal.
- Verify the builder can accept Petri dispatch over vsock.
- Dispatch a small validation command through `petri-guest`, for example:
  `bash -lc 'command -v mmdebstrap jq virt-make-fs git sha256sum'`.
- Shut down the builder cleanly and write final bundle metadata/checksums.

### Cache And Cleanup

- Keep the reusable builder image as the main cache boundary.
- Keep downloaded upstream images in `target/petri-builder-cache`.
- Remove transient seed images, temporary VM state, sockets, and staging
  directories after successful preparation.
- On failure, keep logs and the failed staging directory long enough for
  debugging, and print their paths.
- Add a documented cleanup command or manual cleanup instructions for:
  - `target/petri-builder-cache`
  - stale `target/petri-builder-*` staging directories
  - Petri runtime state under `$PETRI_STATE_DIR` or `$HOME/.petri/instances`

## Failure Modes To Handle

- macOS host lacks Virtualization.framework support.
- `petri-vz` is missing or cannot be resolved.
- No `--builder-image` or `PETRI_BUILDER_IMAGE` was provided.
- Download fails or checksum does not match.
- Source image format is unsupported.
- EFI boot fails or cloud-init never completes.
- Package installation fails due to network or mirror issues.
- `petri-guest` does not start or vsock dispatch is unavailable.
- Builder validation command fails.
- Output bundle replacement fails.

Each failure should include the operation, the path or URL involved, and any log
path the user can inspect.

## Tests

- CLI tests:
  - `--prepare-builder` accepts `--builder-image`.
  - Missing builder output path is rejected.
  - Source URL/path/checksum flags parse correctly.
- Manifest tests:
  - Existing kernel/initrd bundles still load.
  - EFI builder bundles load and reject missing disk paths.
  - Absolute or escaping bundle member paths are rejected.
- Swift helper tests where practical:
  - EFI boot mode argument parsing.
  - Linux boot mode remains compatible.
- Orchestration tests with fake commands:
  - Download/cache decisions.
  - Atomic output replacement.
  - Checksum verification failure.
  - Provisioning failure preserves diagnostics.
- Manual macOS acceptance test:
  - Run `petri image build --prepare-builder --builder-image target/petri-builder`.
  - Verify `SHA256SUMS`.
  - Run `petri image build --builder vm --builder-image target/petri-builder`.
  - Boot the produced base image with `petri create --image`.

## Open Decisions

- Exact default Debian image URL and version pin.
- Whether to support only RAW input for v1 or add local conversion for compressed
  RAW artifacts.
- Whether EFI boot support should be generalized in the image manifest now or
  hidden behind a builder-only manifest field.
- Whether the builder disk should use RAW or ASIF after preparation.
- How much of the first-boot provisioning log should be copied into the final
  bundle.
