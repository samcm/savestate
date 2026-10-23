#!/usr/bin/env bash
# temporary: why does the cloud image not boot?
set -x
cd /tmp
apt-get install -y -qq --no-install-recommends linux-image-cloud-amd64 >/dev/null 2>&1
k=$(ls /boot/vmlinuz-* | head -1); echo "kernel $k"
dmesg 2>/dev/null | grep -i -E 'kvm|svm|hyper' | tail -20
for cpu in host EPYC-Rome-v4; do
  for smp in 1 2; do
    timeout 20 qemu-system-x86_64 -machine pc-q35-11.1,accel=kvm -cpu $cpu -smp $smp -m 2G -nodefaults -display none -no-reboot \
      -serial file:s.log -kernel $k -append "console=ttyS0 earlyprintk=serial,ttyS0,115200 ignore_loglevel panic=-1"; echo "exit $?"
    echo "=== $cpu smp=$smp"; head -c 2500 s.log; echo ...; tail -c 2500 s.log; echo
  done
done
timeout 20 qemu-system-x86_64 -machine pc,accel=kvm -cpu host -smp 1 -m 1G -nodefaults -display none -no-reboot \
      -serial file:s.log -kernel $k -append "console=ttyS0 earlyprintk=serial,ttyS0,115200 ignore_loglevel panic=-1 nokaslr"; echo "exit $?"
echo "=== pc nokaslr"; tail -c 2500 s.log
