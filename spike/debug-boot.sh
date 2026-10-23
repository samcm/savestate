#!/usr/bin/env bash
# temporary: why does the cloud image not boot?
set -x
cd /tmp
curl -fsSLO https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2
for cpu in host EPYC-Rome-v4 EPYC-Genoa qemu64 max; do
  for m in pc-q35-11.1 pc-q35-10.0; do
  qemu-img create -q -f qcow2 -b debian-13-genericcloud-amd64.qcow2 -F qcow2 ov.qcow2
  timeout 25 qemu-system-x86_64 -machine $m,accel=kvm -cpu $cpu -smp 2 -m 2G -nodefaults -display none -no-reboot \
    -serial file:s.log -drive file=ov.qcow2,if=virtio -d cpu_reset,guest_errors,unimp -D d.log; echo "exit $?"
  echo "=== $cpu $m"; tail -c 1500 s.log; echo; head -c 3000 d.log; echo
  done
done
