# Custom Image Compatibility

Custom Petri image bundles are supported when they satisfy the host-side image
manifest contract and the guest runtime contract used by the current backend.
This document explains how to build a custom bundle, what the host accepts at
runtime, how to verify the artifact, and when to rebuild it.

## Build Paths

The recommended path is to fork or copy
`images/base/petri-base-image.toml`, make the smallest change needed for your
team, and build it with the Petri CLI:

```sh
petri image build \
  --config images/base/petri-base-image.toml \
  --out-dir target/petri-images/team-base
```

The config controls the image name, Petri architecture, Debian architecture,
Debian suite and mirrors, guest Rust target, guest install path, policy path,
workspace path, dispatch port, package set, and disk size. The default base
image is Debian trixie for `aarch64`/`arm64`, installs `petri-guest` at
`/usr/local/bin/petri-guest`, mounts the workspace at `/workspace`, reads policy
from `/run/petri/policy.toml`, and listens on vsock port `7777`.

Use CLI overrides for one-off variants:

```sh
petri image build \
  --config images/base/petri-base-image.toml \
  --out-dir target/petri-images/team-base-dev \
  --disk-size 4G
```

On Linux, the CLI delegates to `scripts/build-base-image.sh` directly. On
macOS, use `petri image build --builder vm` or the default `--builder auto`
mode with a prepared builder image; see [Base VM Images](base-vm-images.md).

If the scripted builder does not fit your image pipeline, you may hand-build a
bundle. A hand-built bundle is compatible only if it has a valid
`petri-image.json`, all files referenced by that manifest are inside the bundle
directory, and the guest runtime contract below is installed and enabled.

## Bundle Manifest

Every runtime bundle must contain `petri-image.json`. Paths are relative to the
bundle directory, must be non-empty, and are canonicalized by the host before
boot.

Current host fields:

| Field | Required | Default | Description |
|---|---:|---:|---|
| `architecture` | yes | none | Non-empty Petri architecture label, currently `aarch64` for the macOS MVP image. |
| `boot_mode` | no | `linux` | `linux` for direct kernel boot or `efi` for disk firmware boot. |
| `disk` | yes | none | Root disk image path. |
| `kernel` | for `linux` | none | Kernel image path for direct Linux boot. |
| `initrd` | no | none | Initial ramdisk path for direct Linux boot. |
| `kernel_command_line` | for `linux` | none | Kernel command line for direct Linux boot. |
| `dispatch_port` | no | `7777` | Positive vsock port where `petri-guest` listens. |
| `ready_timeout_secs` | no | `90` | Positive guest-ready timeout used while waiting for the agent. |
| `auxiliary_disks` | no | `[]` | Additional disk image paths attached after the root disk. |

Direct Linux boot bundles look like this:

```json
{
  "architecture": "aarch64",
  "boot_mode": "linux",
  "kernel": "vmlinuz",
  "disk": "root.img",
  "initrd": "initrd.img",
  "kernel_command_line": "root=/dev/vda1 rootwait rw console=hvc0 systemd.unit=multi-user.target",
  "dispatch_port": 7777
}
```

`boot_mode` may be omitted for Linux bundles because `linux` is the default.
Linux bundles must set both `kernel` and `kernel_command_line`. EFI bundles must
not set `kernel`, `initrd`, or `kernel_command_line`:

```json
{
  "architecture": "aarch64",
  "boot_mode": "efi",
  "disk": "root.img",
  "dispatch_port": 7777
}
```

The macOS helper boots Linux manifests with `VZLinuxBootLoader` and EFI
manifests with `VZEFIBootLoader`. EFI bundles get an instance-local EFI
variable store; the store is runtime state, not part of the portable bundle.

## Guest Runtime Contract

A compatible image must start `petri-guest` before the host guest-ready timeout
expires. The default image uses systemd, but the contract is behavioral rather
than systemd-specific:

- `petri-guest` must be installed for the guest architecture and executable.
- The host workspace virtio-fs share with tag `workspace` must be mounted
  writable at `/workspace`.
- The host config virtio-fs share with tag `petri-config` must be mounted
  read-only at `/run/petri`.
- The guest policy file must be read from `/run/petri/policy.toml`.
- The guest must run with `--transport vsock --vsock-port <dispatch_port>`.
- The policy `workspace_path` should resolve to `/workspace`.
- Dispatch protocol version `1` must be supported.

LSP support is optional. To serve the `lsp_*` tools, install the language
servers and pass `--lsp-config <path>` to an `[lsp]` config (the default image
uses `/etc/petri/lsp.toml`); see
[Vsock Dispatch Protocol](vsock-dispatch-protocol.md#lsp-tools). Omitting the
flag is fully compatible — the guest serves `bash_command` normally and degrades
every `lsp_*` request gracefully.

The default service command is:

```text
/usr/local/bin/petri-guest --policy /run/petri/policy.toml --transport vsock --vsock-port 7777
```

If you change `dispatch_port` in `petri-image.json`, the service command inside
the image must use the same port. If you change the guest policy path or
workspace path in a custom image, ensure the mounted paths and policy file still
match the host contracts documented in [Workspace Mounting Contract](workspace-contract.md)
and [Immutable Policy Config](policy-config.md).

## Version Pinning

Custom bundles should record enough information to reproduce and audit the
guest:

- Pin the Petri commit used to build `petri-guest`.
- Build `petri-guest` from the same commit or an explicitly compatible commit.
- Pin the distro suite, mirror, snapshot source, and package list when
  reproducibility matters.
- Record the guest binary SHA-256.
- Record the config path or config digest, build timestamp, target triple,
  architecture, disk size, and dirty-worktree state.

`petri image build` writes this information to `build-info.json`. If you
hand-build a bundle, keep the same file for consumers even though the host does
not require it to boot.

## Verification

Before publishing or consuming a bundle, verify the artifact:

```sh
cd target/petri-images/team-base
sha256sum -c SHA256SUMS
cat build-info.json
```

Inspect `petri-image.json` and confirm the manifest matches the boot style and
guest service configuration:

```sh
cat petri-image.json
```

Smoke test the bundle with a policy that permits a harmless command:

```sh
petri sandbox create base \
  --id image-smoke \
  --workspace /absolute/workspace \
  --policy /absolute/policy.toml \
  --image target/petri-images/team-base

petri sandbox exec image-smoke ls /workspace
petri sandbox kill image-smoke
```

The smoke test proves that the host can parse the bundle, boot it, mount the
workspace and policy config, connect to the declared vsock port, and receive a
dispatch protocol v1 result.

## Upgrade Guidance

Rebuild custom images whenever any of these contracts change:

- `petri-image.json` manifest fields or boot mode rules
- vsock dispatch protocol version or request/result schema
- policy TOML schema
- workspace mount path, tag, or visibility semantics
- config mount path, tag, or read-only expectations
- `petri-guest` startup flags or readiness behavior
- host backend boot requirements for the target platform

To determine whether an old bundle is still compatible, compare its
`petri-image.json` and `build-info.json` with the current docs and code. The
host accepts old bundles when the manifest still validates, all referenced files
exist inside the bundle, the boot mode is still supported, and the guest starts
a `petri-guest` binary that understands the host's dispatch protocol and policy
schema.

When in doubt, rebuild from the current Petri commit and rerun the verification
steps. A bundle that passes `sha256sum -c SHA256SUMS` but fails the smoke test
is intact but not compatible with the current host/guest runtime contract.
