#!/usr/bin/env bash
# Pick the guest CPU model: the first named model in the list below that this host
# runs with `enforce` (every feature of the model present, nothing silently dropped).
# EPYC-Rome-v4 comes first because it is the baseline the design names for mixed Zen 2
# and Zen 3 hosts; the Intel list runs from newer to older generations. Each list ends
# with v1 models, which lack the speculation-control flags nested hosts often hide.
#
# Usage: select-cpu.sh   (writes $WORK/cpu-model and $OUT/select-cpu.md)
set -euo pipefail
: "${WORK:?}" "${OUT:?}"
mkdir -p "$WORK" "$OUT"

vendor=$(awk -F': ' '/^vendor_id/ { print $2; exit }' /proc/cpuinfo)
case "$vendor" in
  AuthenticAMD) candidates="EPYC-Rome-v4 EPYC-Rome-v3 EPYC-Rome EPYC-v4 EPYC" ;;
  *) candidates="SapphireRapids-v3 Icelake-Server-v7 Cascadelake-Server-v5 Skylake-Server-v5 Skylake-Server Broadwell-v4 Broadwell Haswell" ;;
esac

md=$OUT/select-cpu.md
{
  echo "## CPU model selection"
  echo
  echo "Host CPU: $(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo) ($vendor)."
  echo
  echo "| Model (\`-cpu MODEL,enforce\`) | Result |"
  echo "| --- | --- |"
} >"$md"

chosen=
for m in $candidates; do
  if err=$(echo '{"execute":"qmp_capabilities"} {"execute":"quit"}' |
    qemu-system-x86_64 -machine pc-q35-11.1,accel="${ACCEL:-kvm}" -cpu "$m,enforce" \
      -nodefaults -display none -S -qmp stdio 2>&1 >/dev/null); then
    echo "| $m | runs |" >>"$md"
    chosen=$m
    break
  fi
  echo "| $m | refused: $(echo "$err" | grep -o "doesn't support requested feature: [^ ]*" | sort -u | sed 's/.*: //' | tr '\n' ' ') |" >>"$md"
done
[ -n "$chosen" ] || { echo "no candidate CPU model runs on this host" >&2; exit 1; }
echo "$chosen" >"$WORK/cpu-model"
printf '\nChosen: `%s`.\n' "$chosen" >>"$md"
echo "$chosen"
