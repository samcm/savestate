#!/usr/bin/env bash
# Run the whole spike in order and collect the tables into OUT/results.md.
#
# Usage: run-all.sh WORK OUT [EXPERIMENT...]
#   WORK  scratch directory (needs about 20 GB free on ext4 or xfs for an 8 GiB guest)
#   OUT   results directory
# Environment: GUEST_IMAGE (prepared guest image, built here if missing), plus the
# per-experiment variables described at the top of each exp*.sh.
set -euo pipefail

export WORK=$1 OUT=$2
shift 2
spike=$(cd "$(dirname "$0")" && pwd)
exps=("$@")
[ ${#exps[@]} -gt 0 ] || exps=(exp1-save-restore exp2-checkpoint-running exp3-concurrent-restores exp4-clock exp5-vmgenid-mac exp6-cpu-model)
mkdir -p "$WORK/layers" "$OUT"
: >"$OUT/verdicts.txt"

{
  echo "## Host"
  echo
  echo "| | |"
  echo "| --- | --- |"
  echo "| CPU | $(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo), $(nproc) threads |"
  echo "| Memory | $(free -g | awk '/^Mem:/ { print $2 }') GiB |"
  echo "| Kernel | $(uname -r) |"
  echo "| KVM nested | $(cat /sys/module/kvm_amd/parameters/nested /sys/module/kvm_intel/parameters/nested 2>/dev/null | head -1) |"
  echo "| QEMU | $(qemu-system-x86_64 --version | head -1); $(cat /etc/qemu-spike-source 2>/dev/null || echo 'source unknown') |"
  echo "| Scratch filesystem | $(df -hT "$WORK" | awk 'NR == 2 { print $2 ", " $5 " free of " $3 }') |"
} >"$OUT/host.md"
cat "$OUT/host.md"

export CPU_MODEL
CPU_MODEL=$("$spike/select-cpu.sh")
image=${GUEST_IMAGE:-$WORK/guest.qcow2}
"$spike/prepare-guest.sh" "$image"
cp "$image" "$WORK/layers/base.qcow2"
cp "${image%.qcow2}.vmlinuz" "$WORK/layers/base.vmlinuz"
cp "${image%.qcow2}.initrd" "$WORK/layers/base.initrd"
chmod a-w "$WORK/layers/base.qcow2"

failed=()
for e in "${exps[@]}"; do
  echo "::group::$e"
  "$spike/$e.sh" || failed+=("$e")
  echo "::endgroup::"
done

{
  cat "$OUT/host.md"
  echo
  cat "$OUT/select-cpu.md"
  for e in "${exps[@]}"; do
    echo
    cat "$OUT/$e.md" 2>/dev/null || true
  done
} >"$OUT/results.md"
echo
echo "Verdicts:"
cat "$OUT/verdicts.txt"
[ ${#failed[@]} -eq 0 ] || { echo "experiments that did not complete: ${failed[*]}" >&2; exit 1; }
