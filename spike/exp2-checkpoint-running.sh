#!/usr/bin/env bash
# Experiment 2: checkpoint while the VM keeps running. stop, blockdev-snapshot onto a
# new overlay, migrate to file, cont. The original must keep running, its new writes
# must land in the new overlay, and the frozen layer must not change (checksummed
# before cont and after the writes). A restore from the checkpoint must see the disk as
# it was at the checkpoint.
#
# Environment: EXP2_MEM (default 4G), DIRECT_IO (default true).
source "$(dirname "$0")/lib.sh"

MEM=${EXP2_MEM:-4G}
DIO=${DIRECT_IO:-true}
c=$WORK/ckpt/exp2-ckpt

boot exp2-p "$MEM"
gexec exp2-p 'echo checkpointed >/root/before.txt && sync'
h0=$(now)
read -r rt0 _ _ <<<"$(guest_clocks exp2-p)"
h0=$(awk -v a="$h0" -v b="$(now)" 'BEGIN { printf "%.6f", (a + b) / 2 }')

checkpoint exp2-p "$c" "$DIO" nocont
# shellcheck disable=SC1091
source "$c/info"
frozen=$layer
sum_frozen=$(sha256sum "$frozen" | cut -c1-16)
t=$(now)
qmp exp2-p cont >/dev/null
t_cont=$(now)
cont_s=$(sub "$t" "$t_cont")
pause=$(awk -v a="$pause_s" -v b="$cont_s" 'BEGIN { printf "%.3f", a + b }')

status=$(qmp exp2-p query-status | jq -r .status)
nodes=$(qmp exp2-p query-named-block-nodes)
active=$(qmp exp2-p query-block | jq -r '.[] | select(.inserted) | .inserted."node-name"')
frozen_ro=$(jq -r --arg f "$frozen" '.[] | select(.file == $f and .drv == "qcow2") | .ro' <<<"$nodes")
frozen_file_ro=$(jq -r --arg f "$frozen" '.[] | select(.file == $f and .drv == "file") | .ro' <<<"$nodes")

# Guest clock across the pause, read right after cont.
h1=$(now)
read -r rt1 _ _ <<<"$(guest_clocks exp2-p)"
h1=$(awk -v a="$h1" -v b="$(now)" 'BEGIN { printf "%.6f", (a + b) / 2 }')
host_elapsed=$(sub "$h0" "$h1")
guest_elapsed=$(sub "$rt0" "$rt1")
ran=$(awk -v a="$t_stop" -v h0="$h0" -v h1="$h1" -v c="$t_cont" 'BEGIN { printf "%.3f", (a - h0) + (h1 - c) }')

top=$(cat "$(vmdir exp2-p)/top-file")
alloc() { qemu-img map -U --output=json "$1" | jq '[.[] | select(.data and .depth == 0) | .length] | add // 0'; }
top_before=$(alloc "$top")
gexec exp2-p 'dd if=/dev/urandom of=/root/after.bin bs=1M count=64 status=none && sync && echo written >/root/after.txt && sync'
top_after=$(alloc "$top")
sum_frozen2=$(sha256sum "$frozen" | cut -c1-16)
ioerr=$(gexec exp2-p 'dmesg | grep -ci "i/o error" || true')
up=$(gexec exp2-p 'cat /proc/uptime | cut -d" " -f1')

# A restore from the checkpoint sees the disk at the checkpoint.
restore exp2-r "$MEM" "$frozen" "$c/memory" "$DIO"
seen=$(gexec exp2-r 'sync; echo 3 >/proc/sys/vm/drop_caches; ls /root | tr "\n" " "')
parent_has=$(gexec exp2-p 'ls /root | tr "\n" " "')

md "## Experiment 2: checkpoint while the VM keeps running ($MEM guest, direct-io $DIO)"
md ""
md "| Step | Result |"
md "| --- | --- |"
md "| Pause, \`stop\` → \`cont\` returned (excludes the checksum taken before cont) | $pause s (blockdev-snapshot $snapshot_s s, migrate $migrate_s s, fsync $fsync_s s, cont $cont_s s) |"
md "| RAM file | apparent $(gib "$apparent"), allocated $(gib "$allocated") |"
md "| Run state after cont | $status, guest uptime $up s and counting |"
md "| Active node after blockdev-snapshot | \`$active\` ($(basename "$top")) |"
md "| Frozen layer node read-only (qcow2 node / file node) | $frozen_ro / $frozen_file_ro |"
md "| Guest CLOCK_REALTIME advance across the pause | $guest_elapsed s while the host advanced $host_elapsed s; the guest ran for about $ran s of that |"
md "| New overlay allocated before → after a 64 MiB write | $(gib "$top_before") → $(gib "$top_after") |"
md "| Frozen layer sha256 (prefix) before cont → after the writes | \`$sum_frozen\` → \`$sum_frozen2\` |"
md "| Restore from the checkpoint sees in /root | \`$seen\` |"
verdict_header
verdict "original keeps running after cont" "$(is [ "$status" = running ])" "query-status $status; guest wrote 64 MiB and synced; I/O errors in dmesg: $ioerr"
verdict "new writes land in the new overlay" "$(is [ "$((top_after - top_before))" -ge $((64 << 20)) ])" "overlay grew by $(gib $((top_after - top_before)))"
verdict "frozen layer unchanged" "$(is [ "$sum_frozen" = "$sum_frozen2" ])" "sha256 $sum_frozen = $sum_frozen2"
verdict "checkpoint holds the disk at the checkpoint" "$(is [ "$seen" = "before.txt " ])" "restore lists: $seen; original lists: $parent_has"
