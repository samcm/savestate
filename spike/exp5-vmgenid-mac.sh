#!/usr/bin/env bash
# Experiment 5: `-device vmgenid,guid=auto` and the NIC's MAC across restores.
#
# The parent starts a kernel uevent monitor, then is checkpointed and quits. Two
# restores follow: one with the same command line, one with a different MAC on the
# command line. Each restore should get a new generation ID (QMP
# query-vm-generation-id) and the guest kernel should be told: its vmgenid driver
# reseeds the RNG and logs "crng reseeded due to virtual machine fork". The uevent
# monitor (restored mid-flight) shows whether userspace hears about it too.
#
# Environment: EXP5_MEM (default 2G).
source "$(dirname "$0")/lib.sh"

MEM=${EXP5_MEM:-2G}
c=$WORK/ckpt/exp5-ckpt
MAC1=52:54:00:12:34:56
MAC2=52:54:00:ab:cd:ef
export NET=tap

guest_mac='cat /sys/class/net/$(ls /sys/class/net | grep -v "^lo$" | head -1)/address'
uevents='grep -c NEW_VMGENID=1 /run/uevents.log || true'
reseeds='dmesg | grep -c "virtual machine fork" || true'
net='getent ahosts deb.debian.org >/dev/null && echo ok || echo failed'

MAC=$MAC1 boot exp5-p "$MEM"
driver=$(gexec exp5-p 'ls -d /sys/bus/*/drivers/vmgenid/QEMUVGID* 2>/dev/null || echo "not bound"')
diag=$(gexec exp5-p 'ls /sys/bus/acpi/devices | grep -i vgid; dmesg | grep -i -E "vmgenid|vgid"; true' | paste -sd";")
gexec exp5-p 'systemd-run --unit=uevents sh -c "exec stdbuf -oL udevadm monitor --kernel --property >/run/uevents.log"' >/dev/null
sleep 1
gen_p=$(qmp exp5-p query-vm-generation-id | jq -r .guid)
ev_p=$(gexec exp5-p "$uevents")
rs_p=$(gexec exp5-p "$reseeds")
mac_p=$(gexec exp5-p "$guest_mac")
checkpoint exp5-p "$c" true nocont
vm_quit exp5-p
# shellcheck disable=SC1091
source "$c/info"

declare -A gen ev rs mac rx netok
for r in 1 2; do
  v=exp5-r$r
  m=MAC$r
  MAC=${!m} restore "$v" "$MEM" "$layer" "$c/memory" true
  sleep 1
  gen[$r]=$(qmp "$v" query-vm-generation-id | jq -r .guid)
  ev[$r]=$(gexec "$v" "$uevents")
  rs[$r]=$(gexec "$v" "$reseeds")
  mac[$r]=$(gexec "$v" "$guest_mac")
  rx[$r]=$(qmp "$v" query-rx-filter '{"name":"nic0"}' | jq -r '.[0]."main-mac"')
  netok[$r]=$(gexec "$v" "$net" 60)
  vm_quit "$v"
done

md "## Experiment 5: VM generation ID and MAC address across restores ($MEM guests)"
md ""
md "vmgenid driver bound in the guest: \`$driver\`; ACPI device and kernel log: \`$diag\`."
md ""
md "| VM | NIC MAC on the command line | VM generation ID (QMP) | NEW_VMGENID uevents / kernel “virtual machine fork” reseeds in the guest | MAC inside the guest | MAC in QEMU's receive filter | DNS through the NIC |"
md "| --- | --- | --- | --- | --- | --- | --- |"
md "| parent (before checkpoint) | $MAC1 | \`$gen_p\` | $ev_p / $rs_p | $mac_p | - | - |"
md "| restore 1 | $MAC1 | \`${gen[1]}\` | ${ev[1]} / ${rs[1]} | ${mac[1]} | ${rx[1]} | ${netok[1]} |"
md "| restore 2 | $MAC2 | \`${gen[2]}\` | ${ev[2]} / ${rs[2]} | ${mac[2]} | ${rx[2]} | ${netok[2]} |"
verdict_header
verdict "generation ID differs per restore" "$(is [ "$gen_p" != "${gen[1]}" -a "${gen[1]}" != "${gen[2]}" -a "$gen_p" != "${gen[2]}" ])" "parent ${gen_p:0:8}…, restores ${gen[1]:0:8}…, ${gen[2]:0:8}…"
verdict "guest kernel is notified of the new generation ID" "$(is [ "$rs_p" = 0 -a "${rs[1]}" -ge 1 -a "${rs[2]}" -ge 1 ])" "RNG reseeds for a VM fork: parent $rs_p, restores ${rs[1]} and ${rs[2]}"
md "| (information) userspace notification | - | NEW_VMGENID=1 uevents: parent $ev_p, restores ${ev[1]} and ${ev[2]} |"
verdict "MAC unchanged after restore" "$(is [ "${mac[1]}" = "$mac_p" -a "${netok[1]}" = ok ])" "guest MAC $mac_p → ${mac[1]}; network ${netok[1]}"
