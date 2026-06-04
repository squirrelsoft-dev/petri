# petri

Host-side CLI and library for Petri.

The public library exposes lifecycle and dispatch boundaries through a backend
trait. The binary routes commands to the selected backend and defaults to the
macOS MVP backend.

## macOS MVP Backend

The `macos` backend boots a Linux guest with Apple's Virtualization.framework
through the `petri-vz` helper. It attaches the host workspace with virtio-fs tag
`workspace`, exposes the immutable policy through a read-only `petri-config`
share, and dispatches protocol frames to `petri-guest` over vsock port `7777`.

Runtime state is written to `$PETRI_STATE_DIR` when set, otherwise
`$HOME/.petri/instances`. Set `PETRI_VZ_BIN` when `petri-vz` is not on `PATH`.

The macOS backend requires `--image <image-bundle>`. The bundle must contain a
`petri-image.json` manifest with relative paths to the Linux kernel and root
disk image:

```json
{
  "architecture": "aarch64",
  "kernel": "vmlinuz",
  "disk": "root.img",
  "initrd": "initrd.img",
  "kernel_command_line": "console=hvc0 root=/dev/vda rw",
  "dispatch_port": 7777
}
```

The `initrd` field is optional. Image build reproducibility is owned by the
base image pipeline; this backend consumes a compatible bundle.

For local development only, `PETRI_MACOS_BACKEND_FALLBACK=loopback` keeps the
previous non-VM path that starts `petri-guest` as a host process. That fallback
is not the normal `macos` backend behavior.

## CLI

```text
petri create --id <id> --workspace <path> --policy <path> [--image <path>] [--backend macos|stub]
petri dispatch --id <id> --command <name> --cwd <path> [--request-id <id>] [--arg <value>]... [--timeout-ms <ms>] [--max-output-bytes <bytes>]
petri stop --id <id>
petri teardown --id <id>
```

Example local smoke test:

```sh
swift build --package-path crates/petri-vz
cargo build
PETRI_VZ_BIN=crates/petri-vz/.build/debug/petri-vz target/debug/petri create \
  --id dev-1 \
  --workspace /absolute/workspace \
  --policy /absolute/policy.toml \
  --image /absolute/petri-image-bundle \
  --backend macos
target/debug/petri dispatch \
  --id dev-1 \
  --request-id req-1 \
  --command printf \
  --arg hello \
  --cwd /absolute/workspace
target/debug/petri teardown --id dev-1
```

`dispatch` currently models protocol version 1 `bash_command` requests. Backend
implementations are responsible for opening the guest transport, sending the
NDJSON frame, and returning the structured result.
