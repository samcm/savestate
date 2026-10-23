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
#   --restore-dir DIR working directory on RESTORE_HOST if different (needed when both
#                     hosts are the same machine, as in the workflow's self-test)
#   --image PATH      prepared guest image (NAME.qcow2 next to NAME.vmlinuz and
#                     NAME.initrd, from prepare-guest.sh) on SAVE_HOST, inside DIR when
#                     QEMU runs in a container (default: built there by
#                     prepare-guest.sh, which needs internet access and root)
#   --runtime R       how QEMU runs on the hosts: docker (default) or podman, both
#                     through sudo, in the launcher image's QEMU (see --qemu-image), so
#                     both hosts run the identical build; or host, which uses the hosts'
#                     own QEMU 11.1, qemu-img, python3, jq, curl, genisoimage, iproute2
#                     and nftables (run as root)
#   --qemu-image IMG  container image with the QEMU to test, already present on this
#                     machine (default: build the `base-amd64` stage of this checkout's
#                     Dockerfile, i.e. the launcher's QEMU 11.1.2). It is copied to both
#                     hosts with `docker save | ssh HOST sudo RUNTIME load`.
#   --direct-io BOOL  migrate with direct-io (default true; false if DIR's filesystem
#                     does not support O_DIRECT)
#   --keep            keep DIR on both hosts afterwards
#
# Each host needs /dev/kvm, sshd, and either sudo plus docker/podman or the tools above.
# This machine needs ssh, and docker for the default runtimes.
# Exit status 0 means the restore passed every check.
set -euo pipefail

FILES=(cross-cpu.sh lib.sh net.sh qmp.py prepare-guest.sh install-tools.sh)

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

qemu_build() { echo "$(qemu-system-x86_64 --version | head -1), sha256 $(sha256sum "$(command -v qemu-system-x86_64)" | cut -c1-16)"; }

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
  if [ -n "$image" ]; then
    cp "$image" "$LAYERS/base.qcow2"
    cp "${image%.qcow2}.vmlinuz" "$LAYERS/base.vmlinuz"
    cp "${image%.qcow2}.initrd" "$LAYERS/base.initrd"
  else
    "$dir/prepare-guest.sh" "$LAYERS/base.qcow2"
  fi
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
  local new_errors
  new_errors=$(gexec xc-r "dmesg --level=emerg,alert,crit,err | tail -n +$((kernel_errors + 1))")
  # shellcheck disable=SC1091
  source "$(vmdir xc-r)/restore-info"
  vm_quit xc-r
  row() { printf '| %s | %s | %s | %s |\n' "$1" "$2" "$3" "$4"; }
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
  } >"$OUT/cross-cpu.md"
  cat "$OUT/cross-cpu.md"
  [ -z "$new_errors" ] || printf 'Kernel errors logged since the checkpoint:\n%s\n' "$new_errors"
  if grep -q '| FAIL |$' "$OUT/cross-cpu.md"; then echo "RESULT: FAIL"; exit 1; fi
  echo "RESULT: PASS"
}

case "${1:-}" in
  _probe) shift; inner_probe "$@"; exit ;;
  _save) shift; inner_save "$@"; exit ;;
  _restore) shift; inner_restore "$@"; exit ;;
esac

# --- on the coordinating machine --------------------------------------------------

cpu=EPYC-Rome-v4 mem=2G dir=/var/tmp/savestate-cross-cpu rdir='' image='' runtime=docker dio=true keep='' qemu_image=''
while [ $# -gt 2 ]; do
  case "$1" in
    --cpu) cpu=$2; shift 2 ;;
    --mem) mem=$2; shift 2 ;;
    --dir) dir=$2; shift 2 ;;
    --restore-dir) rdir=$2; shift 2 ;;
    --image) image=$2; shift 2 ;;
    --runtime) runtime=$2; shift 2 ;;
    --qemu-image) qemu_image=$2; shift 2 ;;
    --direct-io) dio=$2; shift 2 ;;
    --keep) keep=1; shift ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
[ $# -eq 2 ] || { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 2; }
save_host=$1 restore_host=$2
rdir=${rdir:-$dir}
spike=$(cd "$(dirname "$0")" && pwd)

# on HOST DIR ARGS...: run ARGS in DIR on HOST, inside a fresh QEMU container unless
# --runtime host. Only the spike's own commands need the tools installed first.
on() {
  local host=$1 d=$2
  shift 2
  local cmd prep=
  cmd=$(printf '%q ' "$@")
  [ "$1" = ./cross-cpu.sh ] && prep='./install-tools.sh >&2 && '
  case "$runtime" in
    host) ssh "$host" "cd $(printf %q "$d") && $cmd" ;;
    docker | podman)
      ssh "$host" "sudo $runtime run --rm -i --privileged -v $(printf %q "$d:$d") -w $(printf %q "$d") \
        $qemu_image bash -c $(printf %q "$prep$cmd")" ;;
    *) echo "unknown runtime $runtime" >&2; exit 2 ;;
  esac
}

if [ "$runtime" != host ]; then
  if [ -z "$qemu_image" ]; then
    qemu_image=savestate-qemu:cross-cpu
    echo "== building the launcher's QEMU ($qemu_image) from $(dirname "$spike")/Dockerfile"
    docker build -q --target base-amd64 -t "$qemu_image" -f "$(dirname "$spike")/Dockerfile" "$(dirname "$spike")"
  fi
  for h in "$save_host" "$restore_host"; do
    echo "== loading $qemu_image on $h"
    docker save "$qemu_image" | ssh "$h" "sudo $runtime load -q"
  done
fi

for hd in "$save_host $dir" "$restore_host $rdir"; do
  read -r h d <<<"$hd"
  echo "== copying the spike scripts to $h:$d"
  tar -C "$spike" -cf - "${FILES[@]}" | ssh "$h" "mkdir -p $(printf %q "$d") && tar -C $(printf %q "$d") -xf -"
  echo "== $h: can it run -cpu $cpu,enforce?"
  on "$h" "$d" ./cross-cpu.sh _probe "$cpu"
done

echo "== saving on $save_host"
on "$save_host" "$dir" ./cross-cpu.sh _save "$dir" "$mem" "$cpu" "$dio" "$image"

echo "== streaming the checkpoint (RAM file and layer chain) $save_host -> $restore_host"
on "$save_host" "$dir" tar -C work -cSf - ckpt/xc layers |
  on "$restore_host" "$rdir" bash -c 'mkdir -p work && tar -C work -xSf -'

echo "== restoring on $restore_host"
status=0
on "$restore_host" "$rdir" ./cross-cpu.sh _restore "$rdir" "$dio" || status=$?

if [ -z "$keep" ]; then
  on "$save_host" "$dir" rm -rf work out || true
  on "$restore_host" "$rdir" rm -rf work out || true
fi
exit "$status"
