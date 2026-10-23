#!/usr/bin/env bash
# Build the guest for the spike: the Debian 13 generic cloud image, booted once with
# cloud-init to install qemu-guest-agent (and hwclock, which guest-set-time runs), turn
# off time synchronisation (savestate owns guest time) and background timers, then
# disable cloud-init so later boots are quick.
#
# The guest boots its own kernel and initrd directly (-kernel/-initrd), copied out of
# the image's root partition: on the GitHub-hosted runners the image's GRUB hands over
# to the kernel and the VM resets at once, while the same kernel boots fine directly.
# Mounting the partition and the tap network (net.sh) need root. The image gets the
# static address net.sh expects.
#
# Usage: prepare-guest.sh OUTPUT.qcow2
#   writes OUTPUT.qcow2, OUTPUT.vmlinuz, OUTPUT.initrd and OUTPUT.info (what it is);
#   does nothing if they exist
# Environment: CPU_MODEL (default EPYC-Rome-v4), ACCEL (default kvm)
set -euo pipefail
# shellcheck source=spike/net.sh
source "$(dirname "$0")/net.sh"

out=$1
base=${out%.qcow2}
[ -s "$out" ] && [ -s "$base.vmlinuz" ] && [ -s "$base.initrd" ] && [ -s "$base.info" ] && { echo "guest $out already present"; exit 0; }
url=https://cloud.debian.org/images/cloud/trixie/latest
img=debian-13-genericcloud-amd64.qcow2
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"

curl -fsSLO "$url/$img"
curl -fsSL "$url/SHA512SUMS" | grep " $img\$" | sha512sum -c -

cat >meta-data <<'EOF'
instance-id: savestate-spike
local-hostname: guest
EOF
cat >user-data <<'EOF'
#cloud-config
package_update: true
packages: [qemu-guest-agent, util-linux-extra]
runcmd:
  - systemctl mask systemd-timesyncd.service chrony.service apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service man-db.timer e2scrub_all.timer fstrim.timer || true
  - apt-get clean
  - touch /etc/cloud/cloud-init.disabled
power_state: {mode: poweroff, condition: true}
EOF
cat >network-config <<EOF
version: 2
ethernets:
  nic:
    match: {driver: virtio_net}
    addresses: [10.0.2.15/24]
    routes: [{to: default, via: 10.0.2.2}]
    nameservers: {addresses: [$(net_dns)]}
EOF
genisoimage -quiet -output seed.iso -volid cidata -joliet -rock user-data meta-data network-config

# Copy the kernel and initrd out of the root partition (partition 1).
qemu-img convert -O raw "$img" raw.img
mkdir mnt
mount -o loop,ro,offset=$(($(partx -g -o START -n 1 raw.img) * 512)) raw.img mnt
cp "$(ls -v mnt/boot/vmlinuz-* | tail -1)" vmlinuz
cp "$(ls -v mnt/boot/initrd.img-* | tail -1)" initrd
echo "Debian $(cat mnt/etc/debian_version) generic cloud image (sha512 $(sha512sum "$img" | cut -c1-16)), kernel $(ls -v mnt/boot/vmlinuz-* | tail -1 | sed 's/.*vmlinuz-//')" >info
umount mnt
rm raw.img

echo "first boot with cloud-init (installs qemu-guest-agent, then powers off)"
timeout 900 qemu-system-x86_64 -machine "pc-q35-11.1,accel=${ACCEL:-kvm}" -cpu "${CPU_MODEL:-EPYC-Rome-v4}" \
  -smp 2 -m 2G -nodefaults -no-user-config -display none -no-reboot -serial file:serial.log \
  -kernel vmlinuz -initrd initrd -append "root=/dev/vda1 console=ttyS0,115200" \
  -drive "file=$img,if=virtio,format=qcow2" -drive file=seed.iso,if=virtio,format=raw,readonly=on \
  -netdev "$(NET=tap net_args prep)" -device virtio-net-pci,netdev=net0 ||
  { tail -50 serial.log; exit 1; }
grep -q 'Cloud-init.*finished' serial.log || { tail -50 serial.log; echo "cloud-init did not finish" >&2; exit 1; }

mkdir -p "$(dirname "$out")"
qemu-img convert -O qcow2 "$img" "$out.tmp"
cp vmlinuz "$base.vmlinuz"
cp initrd "$base.initrd"
cp info "$base.info"
mv "$out.tmp" "$out"
qemu-img info "$out"
