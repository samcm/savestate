#!/usr/bin/env bash
# Save a running guest on one host and restore it on another host with a different CPU
# generation, using a named guest CPU model.
#
# Usage: spike/cross-cpu.sh [options] SAVE_HOST RESTORE_HOST
#
# Run it from a checkout of this repository on a machine that can ssh to both hosts
# (anything ssh accepts: user@host, or a Host from ~/.ssh/config). It copies the spike
# scripts to both hosts, checks the CPU model runs on both with `enforce`, boots a
# guest on SAVE_HOST and checkpoints it (the same stop / blockdev-snapshot / migrate
# sequence as the experiments), streams the checkpoint (RAM file plus layer chain) to
# RESTORE_HOST through this machine, restores it there and checks that the guest
# carries on: guest agent answers, a counter process continues, same CPU flags, same
# results from a workload, no new kernel errors. Swap the hosts for the other direction.
#
# Options:
#   --cpu MODEL       guest CPU model (default EPYC-Rome-v4)
#   --mem SIZE        guest RAM (default 2G)
#   --dir DIR         working directory on both hosts (default /var/tmp/savestate-cross-cpu)
#   --image PATH      prepared guest image on SAVE_HOST, inside DIR when QEMU runs in
#                     a container (default: built there by prepare-guest.sh from the
#                     Debian 13 cloud image, which needs internet access)
#   --runtime R       how QEMU runs on the hosts: docker (default) or podman, both run
#                     through sudo in a debian:trixie container with install-qemu.sh, so
#                     both hosts get the same QEMU build; or host, which uses the hosts'
#                     own QEMU 11.1, qemu-img, python3, jq, curl and genisoimage
#   --direct-io BOOL  migrate with direct-io (default true; false if DIR's filesystem
#                     does not support O_DIRECT)
#   --keep            keep DIR on both hosts afterwards
#
# Each host needs /dev/kvm, sshd, and either sudo plus docker/podman or the tools above.
# Exit status 0 means the restore passed every check.
set -euo pipefail

IMAGE_REF=debian:trixie@sha256:4c25275446ca371766b8b093c696782eabce06263fccacb8d5d813b3dc44b308
FILES=(cross-cpu.sh lib.sh qmp.py prepare-guest.sh install-qemu.sh)

# --- inside a host (or its container): save and restore --------------------------

# Commands run in the guest, the same on both sides.
CPUINFO='grep -m1 "^model name" /proc/cpuinfo | cut -d: -f2- | sed "s/^ //"; grep -m1 "^flags" /proc/cpuinfo | cut -d: -f2- | tr " " "\n" | sed "/^$/d" | sort | sha256sum | cut -c1-16'
WORKLOAD='python3 -c "import hashlib, zlib; d = bytes(range(256)) * 65536; print(hashlib.sha256(d).hexdigest()[:16], zlib.crc32(zlib.compress(d, 9)), sum(sorted(d[::7])))"'
KERNEL_ERRORS='dmesg --level=emerg,alert,crit,err | wc -l'

inner_setup() {
  dir=$1
  export WORK=$dir/work OUT=$dir/out EXP=$2 PREFIX=xc-none-
  # shellcheck source=spike/lib.sh
  source "$dir/lib.sh"
  trap 'for p in "$WORK"/vm/*/pid; do [ -f "$p" ] && kill -9 "$(cat "$p")" 2>/dev/null; done; true' EXIT
}

qemu_build() { qemu-system-x86_64 --version | head -1; }

inner_probe() {
  local cpu=$1
  echo "host CPU: $(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo)"
  echo "QEMU: $(qemu_build)"
  echo '{"execute":"qmp_capabilities"} {"execute":"quit"}' |
    qemu-system-x86_64 -machine pc-q35-11.1,accel=kvm -cpu "$cpu,enforce" -nodefaults -display none -S -qmp stdio >/dev/null
  echo "-cpu $cpu,enforce: runs"
}

inner_save() {
  local dir=$1 mem=$2 cpu=$3 dio=$4 image=$5
  inner_setup "$dir" save
  export CPU_MODEL=$cpu
  if [ -n "$image" ]; then cp "$image" "$LAYERS/base.qcow2"; else "$dir/prepare-guest.sh" "$LAYERS/base.qcow2"; fi
  boot xc-p "$mem"
  gexec xc-p 'echo saved-on-the-first-host >/root/marker && sync &&
    systemd-run --unit=counter sh -c "i=0; while :; do i=\$((i+1)); echo \$i >/run/counter; sleep 0.1; done"' >/dev/null
  sleep 2
  local cpuinfo work errs count
  cpuinfo=$(gexec xc-p "$CPUINFO" | paste -sd'|')
  work=$(gexec xc-p "$WORKLOAD")
  errs=$(gexec xc-p "$KERNEL_ERRORS")
  count=$(gexec xc-p 'cat /run/counter')
  checkpoint xc-p "$WORK/ckpt/xc" "$dio" nocont
  vm_quit xc-p
  # shellcheck disable=SC1091
  source "$WORK/ckpt/xc/info"
  cat >"$WORK/ckpt/xc/manifest" <<EOF
qemu_build='$(qemu_build)'
cpu_model='$cpu'
mem='$mem'
layer='$(basename "$layer")'
save_host_cpu='$(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo)'
guest_cpuinfo='$cpuinfo'
workload='$work'
kernel_errors='$errs'
counter='$count'
pause_s='$pause_s'
EOF
  cat "$WORK/ckpt/xc/manifest"
}

inner_restore() {
  local dir=$1 dio=$2
  inner_setup "$dir" restore
  # shellcheck disable=SC1091
  source "$WORK/ckpt/xc/manifest"
  local build
  build=$(qemu_build)
  [ "$build" = "$qemu_build" ] || { echo "FAIL: QEMU build differs: saved with '$qemu_build', this host has '$build'" >&2; exit 1; }
  export CPU_MODEL=$cpu_model
  restore xc-r "$mem" "$LAYERS/$layer" "$WORK/ckpt/xc/memory" "$dio"
  local count1 count2 cpuinfo work errs marker
  count1=$(gexec xc-r 'cat /run/counter')
  sleep 10
  count2=$(gexec xc-r 'cat /run/counter')
  cpuinfo=$(gexec xc-r "$CPUINFO" | paste -sd'|')
  work=$(gexec xc-r "$WORKLOAD")
  errs=$(gexec xc-r "$KERNEL_ERRORS")
  marker=$(gexec xc-r 'cat /root/marker')
  # shellcheck disable=SC1091
  source "$(vmdir xc-r)/restore-info"
  vm_quit xc-r
  local fail=0
  row() { printf '| %s | %s | %s | %s |\n' "$1" "$2" "$3" "$4"; [ "$4" = PASS ] || fail=1; }
  v() { if [ "$1" = "$2" ]; then echo PASS; else echo FAIL; fi; }
  {
    echo "| Check | Saved | Restored | Verdict |"
    echo "| --- | --- | --- | --- |"
    row "host CPU" "$save_host_cpu" "$(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo)" PASS
    row "QEMU build, guest CPU model" "$qemu_build, $cpu_model" "$build, $cpu_model" PASS
    row "guest agent answers after cont" "-" "after $agent_s s (restore total $total_s s, checkpoint pause $pause_s s)" PASS
    row "guest model name and CPU flags hash" "$guest_cpuinfo" "$cpuinfo" "$(v "$guest_cpuinfo" "$cpuinfo")"
    row "workload result" "$workload" "$work" "$(v "$workload" "$work")"
    row "file written before the checkpoint" "saved-on-the-first-host" "$marker" "$(v saved-on-the-first-host "$marker")"
    row "counter continues (10 s apart)" "$counter" "$count1 → $count2" "$([ "$count1" -ge "$counter" ] && [ "$count2" -gt $((count1 + 50)) ] && echo PASS || echo FAIL)"
    row "kernel error lines (dmesg)" "$kernel_errors" "$errs" "$(v "$kernel_errors" "$errs")"
  } | tee "$OUT/cross-cpu.md"
  if [ "$fail" = 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL"; exit 1; fi
}

case "${1:-}" in
  _probe) shift; inner_probe "$@"; exit ;;
  _save) shift; inner_save "$@"; exit ;;
  _restore) shift; inner_restore "$@"; exit ;;
esac

# --- on the coordinating machine --------------------------------------------------

cpu=EPYC-Rome-v4 mem=2G dir=/var/tmp/savestate-cross-cpu image='' runtime=docker dio=true keep=''
while [ $# -gt 2 ]; do
  case "$1" in
    --cpu) cpu=$2; shift 2 ;;
    --mem) mem=$2; shift 2 ;;
    --dir) dir=$2; shift 2 ;;
    --image) image=$2; shift 2 ;;
    --runtime) runtime=$2; shift 2 ;;
    --direct-io) dio=$2; shift 2 ;;
    --keep) keep=1; shift ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
[ $# -eq 2 ] || { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 2; }
save_host=$1 restore_host=$2
spike=$(cd "$(dirname "$0")" && pwd)

# on HOST ARGS...: run ARGS in DIR on HOST, inside a fresh QEMU container unless
# --runtime host. Only the spike's own commands need QEMU installed first.
on() {
  local host=$1
  shift
  local cmd prep=
  cmd=$(printf '%q ' "$@")
  [ "$1" = ./cross-cpu.sh ] && prep='./install-qemu.sh >&2 && '
  case "$runtime" in
    host) ssh "$host" "cd $(printf %q "$dir") && $cmd" ;;
    docker | podman)
      ssh "$host" "sudo $runtime run --rm -i --privileged -v $(printf %q "$dir:$dir") -w $(printf %q "$dir") \
        $IMAGE_REF bash -c $(printf %q "$prep$cmd")" ;;
    *) echo "unknown runtime $runtime" >&2; exit 2 ;;
  esac
}

for h in "$save_host" "$restore_host"; do
  echo "== copying the spike scripts to $h:$dir"
  tar -C "$spike" -cf - "${FILES[@]}" | ssh "$h" "mkdir -p $(printf %q "$dir") && tar -C $(printf %q "$dir") -xf -"
  echo "== $h: can it run -cpu $cpu,enforce?"
  on "$h" ./cross-cpu.sh _probe "$cpu"
done

echo "== saving on $save_host"
on "$save_host" ./cross-cpu.sh _save "$dir" "$mem" "$cpu" "$dio" "$image"

echo "== streaming the checkpoint (RAM file and layer chain) $save_host -> $restore_host"
on "$save_host" tar -C work -cSf - ckpt/xc layers | on "$restore_host" bash -c 'mkdir -p work && tar -C work -xSf -'

echo "== restoring on $restore_host"
status=0
on "$restore_host" ./cross-cpu.sh _restore "$dir" "$dio" || status=$?

if [ -z "$keep" ]; then
  for h in "$save_host" "$restore_host"; do on "$h" rm -rf work out || true; done
fi
exit "$status"
