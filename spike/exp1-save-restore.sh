#!/usr/bin/env bash
# Experiment 1: save and restore timings for the largest guest the host fits, with
# multifd + mapped-ram, without and with direct-io, idle and with most RAM dirtied.
#
# One source VM takes four checkpoints in a row (idle and dirty, each without and with
# direct-io), then quits; each checkpoint is then restored on its own with a cold host
# page cache. "Dirty" fills a tmpfs in the guest with random data, so the pages are
# resident and non-zero; its checksum after restore proves the RAM came back intact.
#
# Environment: EXP1_MEM (default 8G), EXP1_SMP (default 4), EXP1_FILL_MIB (default
# 6656, about 80% of 8 GiB).
source "$(dirname "$0")/lib.sh"

MEM=${EXP1_MEM:-8G}
FILL_MIB=${EXP1_FILL_MIB:-6656}
export SMP=${EXP1_SMP:-4}
ck=$WORK/ckpt

boot exp1-src "$MEM"

# Idle: a booted guest after the design's pre-checkpoint hook (page cache dropped).
gexec exp1-src 'sync; echo 3 >/proc/sys/vm/drop_caches'
used_idle=$(gexec exp1-src "free -b | awk '/^Mem:/ { print \$3 }'")
for dio in false true; do
  drop_host_caches
  checkpoint exp1-src "$ck/exp1-idle-$dio" "$dio"
done

log "dirtying $FILL_MIB MiB of guest RAM"
sum=$(gexec exp1-src "mkdir -p /mnt/fill && mount -t tmpfs -o size=$((FILL_MIB + 64))m fill /mnt/fill &&
  dd if=/dev/urandom of=/mnt/fill/data bs=1M count=$FILL_MIB status=none && b2sum /mnt/fill/data | cut -c1-32" 900)
used_dirty=$(gexec exp1-src "free -b | awk '/^Mem:/ { print \$3 }'")
read -r src_rss src_ram <<<"$(vm_rss exp1-src)"
for dio in false true; do
  drop_host_caches
  checkpoint exp1-src "$ck/exp1-dirty-$dio" "$dio"
done
vm_quit exp1-src

declare -A R
for state in idle dirty; do
  for dio in false true; do
    c=$ck/exp1-$state-$dio
    drop_host_caches
    # shellcheck disable=SC1091
    source "$c/info"
    restore exp1-dst "$MEM" "$layer" "$c/memory" "$dio"
    # shellcheck disable=SC1091
    source "$(vmdir exp1-dst)/restore-info"
    read -r rss ram <<<"$(vm_rss exp1-dst)"
    R[$state-$dio]="$start_s|$load_s|$agent_s|$total_s|$(gib $((rss * 1024)))|$(gib $(((rss - ram) * 1024)))"
    if [ "$state" = dirty ]; then
      got=$(gexec exp1-dst 'b2sum /mnt/fill/data | cut -c1-32' 900)
      R[sum-$dio]=$got
    fi
    vm_quit exp1-dst
    rm -f "$c/memory"
  done
done

md "## Experiment 1: save and restore, $MEM guest, $SMP vCPUs, multifd ($CHANNELS channels) + mapped-ram"
md ""
md "Guest RAM in use (\`free\`): idle $(gib "$used_idle"), dirty $(gib "$used_dirty") ($FILL_MIB MiB of random data in a tmpfs)."
md "Source QEMU resident while dirty: $(gib $((src_rss * 1024))), of which outside guest RAM $(gib $(((src_rss - src_ram) * 1024)))."
md ""
md "Save (the VM is paused from \`stop\` to \`cont\`; the host page cache was dropped before each save):"
md ""
md "| State | direct-io | Pause total (s) | blockdev-snapshot (s) | migrate (s) | fsync (s) | QEMU total-time (ms) | Non-zero pages | Write rate | RAM file apparent | RAM file allocated |"
md "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
for state in idle dirty; do
  for dio in false true; do
    # shellcheck disable=SC1091
    source "$ck/exp1-$state-$dio/info"
    rate=$(awk -v b="$allocated" -v s="$migrate_s" -v f="$fsync_s" 'BEGIN { printf "%.0f MiB/s", b / 1048576 / (s + f) }')
    md "| $state | $dio | $pause_s | $snapshot_s | $migrate_s | $fsync_s | $qemu_total_ms | $normal_pages | $rate | $(gib "$apparent") | $(gib "$allocated") |"
  done
done
md ""
md "Restore (cold host page cache; \`-incoming defer -S\`, migrate-incoming, cont):"
md ""
md "| State | direct-io | QEMU start + caps (s) | migrate-incoming (s) | cont → guest agent answers (s) | Total incl. qemu-img create (s) | QEMU resident | QEMU outside guest RAM |"
md "| --- | --- | --- | --- | --- | --- | --- | --- |"
for state in idle dirty; do
  for dio in false true; do
    IFS='|' read -r a b c d e f <<<"${R[$state-$dio]}"
    md "| $state | $dio | $a | $b | $c | $d | $e | $f |"
  done
done
verdict_header
for dio in false true; do
  verdict "dirty RAM intact after restore (direct-io $dio)" "$(is [ "${R[sum-$dio]}" = "$sum" ])" "b2sum of $FILL_MIB MiB tmpfs file: saved \`$sum\`, restored \`${R[sum-$dio]}\`"
done
# shellcheck disable=SC1091
source "$ck/exp1-idle-true/info"
verdict "idle RAM file is sparse" "$(is [ "$allocated" -lt $((apparent / 4)) ])" "apparent $(gib "$apparent"), allocated $(gib "$allocated")"
