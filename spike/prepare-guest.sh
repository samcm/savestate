#!/usr/bin/env bash
# Build the guest disk for the spike: the Debian 13 generic cloud image, booted once
# with cloud-init to install qemu-guest-agent (and hwclock, which guest-set-time runs),
# turn off time synchronisation (savestate owns guest time) and background timers, then
# disable cloud-init so later boots are quick. The result is a standalone qcow2.
#
# Usage: prepare-guest.sh OUTPUT.qcow2      (does nothing if OUTPUT exists)
# Environment: CPU_MODEL (default EPYC-Rome-v4), ACCEL (default kvm)
set -euo pipefail

out=$1
[ -s "$out" ] && { echo "guest image $out already present"; exit 0; }
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
genisoimage -quiet -output seed.iso -volid cidata -joliet -rock user-data meta-data

echo "first boot with cloud-init (installs qemu-guest-agent, then powers off)"
timeout 900 qemu-system-x86_64 -machine "pc-q35-11.1,accel=${ACCEL:-kvm}" -cpu "${CPU_MODEL:-EPYC-Rome-v4}" \
  -smp 2 -m 2G -nodefaults -no-user-config -display none -no-reboot -serial file:serial.log \
  -drive "file=$img,if=virtio,format=qcow2" -drive file=seed.iso,if=virtio,format=raw,readonly=on \
  -netdev user,id=net0 -device virtio-net-pci,netdev=net0 ||
  { tail -50 serial.log; exit 1; }
grep -q 'Cloud-init.*finished' serial.log || { tail -50 serial.log; echo "cloud-init did not finish" >&2; exit 1; }

mkdir -p "$(dirname "$out")"
qemu-img convert -O qcow2 "$img" "$out.tmp"
mv "$out.tmp" "$out"
qemu-img info "$out"
