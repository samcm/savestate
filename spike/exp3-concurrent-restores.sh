#!/usr/bin/env bash
# Experiment 3: three concurrent restores from one RAM file and one frozen layer while
# the parent keeps running, each on its own new overlay made with the design's
# `qemu-img create -f qcow2 -b <frozen> -F qcow2` (no -u: it opens the frozen layer
# while the parent holds it). Checks that image locking allows this and that the four
# VMs are independent: each writes a distinct file, and nobody sees anyone else's.
# A counter process started before the checkpoint shows the restores resume
# mid-flight.
#
# Environment: EXP3_MEM (default 2G), DIRECT_IO (default true).
source "$(dirname "$0")/lib.sh"

MEM=${EXP3_MEM:-2G}
DIO=${DIRECT_IO:-true}
c=$WORK/ckpt/exp3-ckpt
kids="exp3-c1 exp3-c2 exp3-c3"

boot exp3-p "$MEM"
gexec exp3-p 'echo parent >/root/before && sync &&
  systemd-run --unit=counter sh -c "i=0; while :; do i=\$((i+1)); echo \$i >/run/counter; sleep 0.1; done"' >/dev/null
sleep 2
count_before=$(gexec exp3-p 'cat /run/counter')
checkpoint exp3-p "$c" "$DIO"
# shellcheck disable=SC1091
source "$c/info"
frozen=$layer
sum0=$(sha256sum "$frozen" | cut -c1-16)

log "starting three restores at once"
t0=$(now)
pids=()
for k in $kids; do
  restore "$k" "$MEM" "$frozen" "$c/memory" "$DIO" &
  pids+=($!)
done
ok=PASS
for p in "${pids[@]}"; do wait "$p" || ok=FAIL; done
wall=$(sub "$t0" "$(now)")
[ "$ok" = PASS ] || { verdict "three concurrent restores start" FAIL "see $EXP.log"; exit 1; }

declare -A count gen
for k in $kids; do
  count[$k]=$(gexec "$k" 'cat /run/counter')
  gen[$k]=$(qmp "$k" query-vm-generation-id | jq -r .guid)
done
gen[exp3-p]=$(qmp exp3-p query-vm-generation-id | jq -r .guid)
sleep 1
for k in $kids; do
  count2=$(gexec "$k" 'cat /run/counter')
  [ "$count2" -gt "${count[$k]}" ] || count[$k]="${count[$k]} (stuck)"
done

# Distinct writes, then each VM drops its page cache and lists what its disk holds.
for k in $kids; do gexec "$k" "echo $k >/root/$k && sync"; done
gexec exp3-p 'echo parent >/root/parent-after && sync'
declare -A seen
for v in exp3-p $kids; do
  seen[$v]=$(gexec "$v" 'sync; echo 3 >/proc/sys/vm/drop_caches; ls /root | tr "\n" " "')
done
sum1=$(sha256sum "$frozen" | cut -c1-16)
ngen=$(printf '%s\n' "${gen[@]}" | sort -u | wc -l)

md "## Experiment 3: three concurrent restores from one RAM file and one frozen layer ($MEM guests, direct-io $DIO)"
md ""
md "Parent paused $pause_s s for the checkpoint, then kept running. Counter in the parent just before the checkpoint: $count_before (ticks every 0.1 s)."
md "All three restores (qemu-img create, QEMU start, migrate-incoming, cont, guest agent answers) took $wall s wall time together."
md ""
md "| VM | Restore total (s) | migrate-incoming (s) | Counter right after restore | VM generation ID | /root after every VM wrote its own file |"
md "| --- | --- | --- | --- | --- | --- |"
md "| parent | - | - | - | \`${gen[exp3-p]}\` | \`${seen[exp3-p]}\` |"
indep=PASS
for k in $kids; do
  # shellcheck disable=SC1091
  source "$(vmdir "$k")/restore-info"
  md "| $k | $total_s | $load_s | ${count[$k]} | \`${gen[$k]}\` | \`${seen[$k]}\` |"
  [ "${seen[$k]}" = "before $k " ] || indep=FAIL
done
[ "${seen[exp3-p]}" = "before parent-after " ] || indep=FAIL
md ""
md "Frozen layer sha256 (prefix) before the restores → after all writes: \`$sum0\` → \`$sum1\`."
verdict_header
verdict "qemu-img create -b <frozen> works while the parent runs" PASS "three overlays created without -u"
verdict "image locking allows three restores plus the running parent" "$ok" "three QEMUs opened the frozen layer as backing while the parent held it"
verdict "copies are independent" "$indep" "each VM sees only its own file plus the checkpoint's"
resumed=PASS
for k in $kids; do
  n=${count[$k]}
  [[ $n =~ ^[0-9]+$ ]] && [ "$n" -ge "$count_before" ] && [ "$n" -lt $((count_before + 100)) ] || resumed=FAIL
done
verdict "restores resume mid-flight" "$resumed" "counter $count_before before the checkpoint; ${count[exp3-c1]}, ${count[exp3-c2]}, ${count[exp3-c3]} right after the restores, and still counting"
verdict "frozen layer unchanged" "$(is [ "$sum0" = "$sum1" ])" "sha256 $sum0 = $sum1"
verdict "every VM has its own generation ID" "$(is [ "$ngen" = 4 ])" "$ngen distinct IDs across parent and three restores"
