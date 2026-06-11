#!/bin/bash
set -e

# Fix DNS and wait for network.
systemctl stop systemd-resolved
rm -f /etc/resolv.conf
echo "nameserver 8.8.8.8" > /etc/resolv.conf
systemctl start systemd-networkd
systemctl start systemd-networkd-wait-online
until curl -s --max-time 5 http://deb.debian.org > /dev/null 2>&1; do
    sleep 2
done

# Copy petri-guest out of the artifacts share before mounting /dev/vdb, because
# mounting at /mnt may shadow the already-mounted /mnt/petri-artifacts virtiofs.
PETRI_GUEST_TMP=
if [ -f /mnt/petri-artifacts/petri-guest ]; then
    PETRI_GUEST_TMP=$(mktemp /tmp/petri-guest.XXXXXX)
    cp /mnt/petri-artifacts/petri-guest "$PETRI_GUEST_TMP"
fi

apt-get update -y
apt-get install -y mmdebstrap
mkfs.ext4 -F -L root /dev/vdb
mount /dev/vdb /mnt
mmdebstrap --variant=minbase \
  --include=systemd,systemd-sysv,udev,ca-certificates,iproute2,linux-image-cloud-arm64 \
  trixie /mnt http://deb.debian.org/debian
echo "LABEL=root / ext4 defaults 0 1" > /mnt/etc/fstab

# systemd-networkd DHCP so the guest can reach the network.
mkdir -p /mnt/etc/systemd/network
cat > /mnt/etc/systemd/network/20-dhcp.network <<'NETWORK'
[Match]
Name=en* eth*
[Network]
DHCP=yes
NETWORK
chroot /mnt systemctl enable systemd-networkd

# Install petri-guest and its systemd units if the binary was staged.
if [ -n "$PETRI_GUEST_TMP" ] && [ -f "$PETRI_GUEST_TMP" ]; then
    install -m 0755 "$PETRI_GUEST_TMP" /mnt/usr/local/bin/petri-guest
    rm -f "$PETRI_GUEST_TMP"

    mkdir -p /mnt/workspace /mnt/run/petri

    cat > /mnt/etc/systemd/system/workspace.mount <<'UNIT'
[Unit]
Description=Petri workspace virtiofs mount
[Mount]
What=workspace
Where=/workspace
Type=virtiofs
Options=defaults
[Install]
WantedBy=multi-user.target
UNIT

    cat > /mnt/etc/systemd/system/run-petri.mount <<'UNIT'
[Unit]
Description=Petri config virtiofs mount
[Mount]
What=petri-config
Where=/run/petri
Type=virtiofs
Options=defaults
[Install]
WantedBy=multi-user.target
UNIT

    cat > /mnt/etc/systemd/system/petri-guest.service <<'UNIT'
[Unit]
Description=Petri guest dispatch service
After=workspace.mount run-petri.mount network-online.target
Requires=workspace.mount run-petri.mount
[Service]
ExecStart=/usr/local/bin/petri-guest --policy /run/petri/policy.toml --transport vsock --vsock-port 7777
Restart=always
RestartSec=1
[Install]
WantedBy=multi-user.target
UNIT

    chroot /mnt systemctl enable workspace.mount run-petri.mount petri-guest.service
fi

# Extract the kernel/initrd so the host can store them with the sealed layer and
# boot it as a sandbox via Linux direct boot (the host can't read ext4). The
# workspace virtiofs share (tag "workspace") is mounted read-write by petri-vz.
KVER=$(basename /mnt/boot/vmlinuz-* | sed 's/^vmlinuz-//')
if [ -f "/mnt/boot/vmlinuz-$KVER" ] && [ -f "/mnt/boot/initrd.img-$KVER" ]; then
    mkdir -p /mnt-out
    if mount -t virtiofs workspace /mnt-out; then
        cp "/mnt/boot/vmlinuz-$KVER" /mnt-out/vmlinuz
        cp "/mnt/boot/initrd.img-$KVER" /mnt-out/initrd
        printf '%s\n' "$KVER" > /mnt-out/kernel-version
        umount /mnt-out
    fi
fi

umount /mnt
