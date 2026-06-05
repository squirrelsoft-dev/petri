#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
usage: scripts/build-base-image.sh [options]

Build a Petri base VM image bundle containing petri-guest.

Options:
  --config <path>        Image config TOML (default: images/base/petri-base-image.toml)
  --out-dir <path>       Output directory (default: target/petri-images/base)
  --arch <arch>          Override Petri manifest architecture from config
  --debian-arch <arch>   Override Debian package architecture from config
  --target <triple>      Override petri-guest Rust target from config
  --disk-size <size>     Override disk size from config
  --skip-guest-build     Use an existing petri-guest binary from --guest-binary
  --guest-binary <path>  Existing petri-guest binary to install
  --help                 Show this help

The output bundle contains petri-image.json, root.img, vmlinuz, optional
initrd.img, SHA256SUMS, and build-info.json.
USAGE
}

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
config="$repo_root/images/base/petri-base-image.toml"
out_dir="$repo_root/target/petri-images/base"
arch_override=""
debian_arch_override=""
target_override=""
disk_size_override=""
skip_guest_build=0
guest_binary=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --config)
      config="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"
      shift 2
      ;;
    --out-dir)
      out_dir="$(mkdir -p "$2" && cd "$2" && pwd)"
      shift 2
      ;;
    --arch)
      arch_override="$2"
      shift 2
      ;;
    --debian-arch)
      debian_arch_override="$2"
      shift 2
      ;;
    --target)
      target_override="$2"
      shift 2
      ;;
    --disk-size)
      disk_size_override="$2"
      shift 2
      ;;
    --skip-guest-build)
      skip_guest_build=1
      shift
      ;;
    --guest-binary)
      guest_binary="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

need_tool() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required tool: $1" >&2
    exit 1
  fi
}

read_toml_value() {
  local key="$1"
  awk -F= -v key="$key" '
    $1 ~ "^[[:space:]]*" key "[[:space:]]*$" {
      value=$2
      sub(/^[[:space:]]*/, "", value)
      sub(/[[:space:]]*$/, "", value)
      gsub(/^"|"$/, "", value)
      print value
      exit
    }
  ' "$config"
}

read_toml_array() {
  local key="$1"
  awk -v key="$key" '
    $0 ~ "^[[:space:]]*" key "[[:space:]]*=" { in_array=1; next }
    in_array && /\]/ { exit }
    in_array {
      line=$0
      gsub(/[",]/, "", line)
      sub(/^[[:space:]]*/, "", line)
      sub(/[[:space:]]*$/, "", line)
      if (length(line) > 0) print line
    }
  ' "$config"
}

need_tool awk
need_tool mmdebstrap
need_tool mke2fs
need_tool python3
need_tool sha256sum

if [ ! -f "$config" ]; then
  echo "config not found: $config" >&2
  exit 1
fi

name="$(read_toml_value name)"
arch="${arch_override:-$(read_toml_value architecture)}"
debian_arch="${debian_arch_override:-$(read_toml_value debian_arch)}"
suite="$(read_toml_value suite)"
mirror="$(read_toml_value mirror)"
security_mirror="$(read_toml_value security_mirror)"
target="${target_override:-$(read_toml_value target)}"
disk_size="${disk_size_override:-$(read_toml_value disk_size)}"
install_path="$(read_toml_value install_path)"
policy_path="$(read_toml_value policy_path)"
workspace_path="$(read_toml_value workspace_path)"
dispatch_port="$(read_toml_value dispatch_port)"
packages="$(read_toml_array base | paste -sd, -)"

if [ -z "$name" ] || [ -z "$arch" ] || [ -z "$debian_arch" ] || [ -z "$suite" ] || [ -z "$mirror" ]; then
  echo "config is missing required [image] fields" >&2
  exit 1
fi

if [ "$skip_guest_build" -eq 0 ]; then
  need_tool cargo
  need_tool rustup
  rustup target add "$target"
  cargo build -p petri-guest --release --target "$target"
  guest_binary="$repo_root/target/$target/release/petri-guest"
elif [ -z "$guest_binary" ]; then
  echo "--skip-guest-build requires --guest-binary <path>" >&2
  exit 1
fi

if [ ! -x "$guest_binary" ]; then
  echo "petri-guest binary is not executable: $guest_binary" >&2
  exit 1
fi

rm -rf "$out_dir"
mkdir -p "$out_dir"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/petri-base-image.XXXXXX")"
trap 'rm -rf "$work_dir"' EXIT

rootfs="$work_dir/rootfs"
mkdir -p "$rootfs"

mmdebstrap \
  --architectures="$debian_arch" \
  --variant=minbase \
  --components=main \
  --include="$packages" \
  --aptopt='Acquire::Check-Valid-Until "false"' \
  "$suite" \
  "$rootfs" \
  "$mirror" \
  "deb $security_mirror $suite-security main"

install -Dm0755 "$guest_binary" "$rootfs$install_path"
install -d "$rootfs$workspace_path" "$rootfs/run/petri" "$rootfs/etc/systemd/system" "$rootfs/etc/modules-load.d"

cat > "$rootfs/etc/modules-load.d/petri.conf" <<'EOF'
virtiofs
vsock
vmw_vsock_virtio_transport
EOF

cat > "$rootfs/etc/systemd/system/workspace.mount" <<EOF
[Unit]
Description=Petri workspace virtio-fs mount
DefaultDependencies=no
Before=local-fs.target petri-guest.service

[Mount]
What=workspace
Where=$workspace_path
Type=virtiofs
Options=rw

[Install]
WantedBy=local-fs.target
EOF

cat > "$rootfs/etc/systemd/system/run-petri.mount" <<EOF
[Unit]
Description=Petri immutable config virtio-fs mount
DefaultDependencies=no
Before=local-fs.target petri-guest.service

[Mount]
What=petri-config
Where=/run/petri
Type=virtiofs
Options=ro

[Install]
WantedBy=local-fs.target
EOF

cat > "$rootfs/etc/systemd/system/petri-guest.service" <<EOF
[Unit]
Description=Petri guest agent
After=workspace.mount run-petri.mount
Requires=workspace.mount run-petri.mount

[Service]
Type=simple
ExecStart=$install_path --policy $policy_path --transport vsock --vsock-port $dispatch_port
Restart=always
RestartSec=1
NoNewPrivileges=yes

[Install]
WantedBy=multi-user.target
EOF

ln -s ../workspace.mount "$rootfs/etc/systemd/system/local-fs.target.wants/workspace.mount" 2>/dev/null || {
  mkdir -p "$rootfs/etc/systemd/system/local-fs.target.wants"
  ln -s ../workspace.mount "$rootfs/etc/systemd/system/local-fs.target.wants/workspace.mount"
}
ln -s ../run-petri.mount "$rootfs/etc/systemd/system/local-fs.target.wants/run-petri.mount"
mkdir -p "$rootfs/etc/systemd/system/multi-user.target.wants"
ln -s ../petri-guest.service "$rootfs/etc/systemd/system/multi-user.target.wants/petri-guest.service"

kernel="$(find "$rootfs/boot" -maxdepth 1 -type f -name 'vmlinuz-*' | sort | tail -n 1)"
initrd="$(find "$rootfs/boot" -maxdepth 1 -type f -name 'initrd.img-*' | sort | tail -n 1 || true)"

if [ -z "$kernel" ]; then
  echo "no kernel found in rootfs /boot" >&2
  exit 1
fi

cp "$kernel" "$out_dir/vmlinuz"
if [ -n "$initrd" ]; then
  cp "$initrd" "$out_dir/initrd.img"
fi

truncate -s "$disk_size" "$out_dir/root.img"
mke2fs -t ext4 -F -d "$rootfs" "$out_dir/root.img"

kernel_command_line="console=hvc0 root=/dev/vda rw systemd.unit=multi-user.target"
initrd_manifest=""
if [ -f "$out_dir/initrd.img" ]; then
  initrd_manifest="initrd.img"
fi

python3 - "$out_dir/petri-image.json" "$arch" "vmlinuz" "root.img" "$initrd_manifest" "$kernel_command_line" "$dispatch_port" <<'PY'
import json
import sys

path, architecture, kernel, disk, initrd, kernel_command_line, dispatch_port = sys.argv[1:]
payload = {
    "architecture": architecture,
    "kernel": kernel,
    "disk": disk,
    "kernel_command_line": kernel_command_line,
    "dispatch_port": int(dispatch_port),
}
if initrd:
    payload["initrd"] = initrd
with open(path, "w", encoding="utf-8") as output:
    json.dump(payload, output, indent=2)
    output.write("\n")
PY

guest_sha="$(sha256sum "$guest_binary" | awk '{print $1}')"
if command -v git >/dev/null 2>&1; then
  git_rev="$(git -C "$repo_root" rev-parse HEAD)"
  git_dirty="$(git -C "$repo_root" status --porcelain)"
else
  git_rev="unknown"
  git_dirty="unknown"
fi

python3 - "$out_dir/build-info.json" \
  "$name" \
  "$(realpath "$config")" \
  "$git_rev" \
  "$git_dirty" \
  "$arch" \
  "$suite" \
  "$debian_arch" \
  "$mirror" \
  "$security_mirror" \
  "$target" \
  "$guest_sha" \
  "$disk_size" \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" <<'PY'
import json
import sys

(
    path,
    name,
    config,
    git_revision,
    git_dirty,
    architecture,
    suite,
    debian_arch,
    mirror,
    security_mirror,
    rust_target,
    petri_guest_sha256,
    disk_size,
    build_time_utc,
) = sys.argv[1:]

payload = {
    "name": name,
    "config": config,
    "source": {
        "git_revision": git_revision,
        "git_dirty": None if git_dirty == "unknown" else bool(git_dirty),
    },
    "image": {
        "architecture": architecture,
        "debian_arch": debian_arch,
        "suite": suite,
        "mirror": mirror,
        "security_mirror": security_mirror,
        "disk_size": disk_size,
    },
    "guest": {
        "rust_target": rust_target,
        "sha256": petri_guest_sha256,
    },
    "build_time_utc": build_time_utc,
}
with open(path, "w", encoding="utf-8") as output:
    json.dump(payload, output, indent=2)
    output.write("\n")
PY

(
  cd "$out_dir"
  sha256sum petri-image.json root.img vmlinuz build-info.json ${initrd:+initrd.img} > SHA256SUMS
)

echo "wrote Petri image bundle: $out_dir"
