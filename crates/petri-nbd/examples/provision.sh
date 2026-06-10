#!/bin/bash
set -e
systemctl stop systemd-resolved
rm -f /etc/resolv.conf
echo "nameserver 8.8.8.8" > /etc/resolv.conf
systemctl start systemd-networkd
systemctl start systemd-networkd-wait-online
until curl -s --max-time 5 http://deb.debian.org > /dev/null 2>&1; do
    sleep 2
done
apt-get update -y
apt-get install -y mmdebstrap
mkfs.ext4 -F /dev/vdb
mount /dev/vdb /mnt
mmdebstrap --variant=minbase \
  --include=systemd,systemd-sysv,udev,ca-certificates,iproute2,linux-image-cloud-arm64 \
  trixie /mnt http://deb.debian.org/debian
echo "LABEL=root / ext4 defaults 0 1" > /mnt/etc/fstab
umount /mnt
