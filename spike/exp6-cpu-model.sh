#!/usr/bin/env bash
# Experiment 6: boot, save and restore with the named CPU model select-cpu.sh chose
# (every other experiment runs on it too), and compare what the guest sees.
#
# Environment: EXP6_MEM (default 2G).
source "$(dirname "$0")/lib.sh"

MEM=${EXP6_MEM:-2G}
c=$WORK/ckpt/exp6-ckpt
cpuinfo='grep -m1 "^model name" /proc/cpuinfo | cut -d: -f2- | sed "s/^ //"; grep -m1 "^flags" /proc/cpuinfo | cut -d: -f2- | tr " " "\n" | sed "/^$/d" | sort | tee /tmp/flags | wc -l; sha256sum </tmp/flags | cut -c1-16; grep -wo -e constant_tsc -e nonstop_tsc /tmp/flags | tr "\n" " "; echo'
# A little work that uses the vector units, and its result.
work='python3 -c "import hashlib; d = bytes(range(256)) * 4096; print(hashlib.sha256(d * 64).hexdigest()[:16])"'

boot exp6-p "$MEM"
mapfile -t P < <(gexec exp6-p "$cpuinfo")
w_p=$(gexec exp6-p "$work")
cs=$(gexec exp6-p 'cat /sys/devices/system/clocksource/clocksource0/current_clocksource')
checkpoint exp6-p "$c" true nocont
vm_quit exp6-p
# shellcheck disable=SC1091
source "$c/info"
restore exp6-r "$MEM" "$layer" "$c/memory" true
mapfile -t R < <(gexec exp6-r "$cpuinfo")
w_r=$(gexec exp6-r "$work")
host_flags=$(grep -m1 '^flags' /proc/cpuinfo | wc -w)

md "## Experiment 6: named CPU model \`$CPU_MODEL\`"
md ""
md "Host: $(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo), $((host_flags - 2)) CPU flags. Guest clocksource: \`$cs\`."
md ""
md "| | Guest model name | Guest CPU flags | Flags hash | TSC flags | sha256 workload |"
md "| --- | --- | --- | --- | --- | --- |"
md "| booted | ${P[0]} | ${P[1]} | \`${P[2]}\` | ${P[3]:-none} | \`$w_p\` |"
md "| restored | ${R[0]} | ${R[1]} | \`${R[2]}\` | ${R[3]:-none} | \`$w_r\` |"
verdict_header
verdict "boots with a named model under enforce" PASS "\`-cpu $CPU_MODEL,enforce\`"
verdict "restore presents the same CPU" "$(is [ "${P[2]}" = "${R[2]}" -a "${P[0]}" = "${R[0]}" ])" "flags hash ${P[2]} → ${R[2]}"
verdict "no invariant TSC in the guest" "$(is [ "$(echo "${P[3]}" | grep -c nonstop_tsc)" = 0 ])" "TSC flags: ${P[3]:-none}"
verdict "work runs after restore" "$(is [ "$w_p" = "$w_r" ])" "$w_p = $w_r"
