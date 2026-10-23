#!/usr/bin/env bash
# Experiment 4: guest clocks across save and restore, with `-rtc clock=vm`.
#
# Frozen: read the guest clocks, checkpoint, quit, wait N seconds, restore, read again.
# The guest's clocks should advance only by the time it actually ran between the two
# readings (before `stop` and after `cont`), not by N.
# Stepped: restore the same checkpoint again, then set the guest's wall clock from the
# host with the guest agent's guest-set-time; CLOCK_REALTIME should match the host and
# CLOCK_MONOTONIC must not jump.
#
# Environment: EXP4_MEM (default 2G), CLOCK_WAIT seconds (default 60).
source "$(dirname "$0")/lib.sh"

MEM=${EXP4_MEM:-2G}
WAIT=${CLOCK_WAIT:-60}
c=$WORK/ckpt/exp4-ckpt

# read_clocks VM: sets RT MONO BOOT RTC (guest) and HOST (host time at the reading).
read_clocks() {
  local a b
  a=$(now)
  read -r RT MONO BOOT RTC <<<"$(gexec "$1" "python3 -c 'import time as t; print(*(\"%.6f\" % t.clock_gettime(c) for c in (t.CLOCK_REALTIME, t.CLOCK_MONOTONIC, t.CLOCK_BOOTTIME)), open(\"/sys/class/rtc/rtc0/since_epoch\").read().strip())'")"
  b=$(now)
  HOST=$(awk -v a="$a" -v b="$b" 'BEGIN { printf "%.6f", (a + b) / 2 }')
}

boot exp4-p "$MEM"
clocksource=$(gexec exp4-p 'cat /sys/devices/system/clocksource/clocksource0/current_clocksource')
ntp=$(gexec exp4-p '{ timedatectl show -p NTP --value; systemctl is-active systemd-timesyncd chrony; } 2>/dev/null | paste -sd" "' || true)
read_clocks exp4-p
rt0=$RT mono0=$MONO boot0=$BOOT rtc0=$RTC host0=$HOST
checkpoint exp4-p "$c" true nocont
vm_quit exp4-p
# shellcheck disable=SC1091
source "$c/info"
t_stop0=$t_stop
log "waiting $WAIT s between save and restore"
sleep "$WAIT"

# Frozen.
restore exp4-f "$MEM" "$layer" "$c/memory" true
# shellcheck disable=SC1091
source "$(vmdir exp4-f)/restore-info"
read_clocks exp4-f
ran=$(awk -v s="$t_stop0" -v h0="$host0" -v h1="$HOST" -v c="$t_cont" 'BEGIN { printf "%.3f", (s - h0) + (h1 - c) }')
host_el=$(sub "$host0" "$HOST")
f_rt=$(sub "$rt0" "$RT") f_mono=$(sub "$mono0" "$MONO") f_boot=$(sub "$boot0" "$BOOT")
f_rtc=$((RTC - rtc0))
f_off=$(sub "$RT" "$HOST")
vm_quit exp4-f

# Stepped.
restore exp4-s "$MEM" "$layer" "$c/memory" true
read_clocks exp4-s
s_rt_before=$RT s_mono_before=$MONO s_off_before=$(sub "$RT" "$HOST")
set_err=$(qga exp4-s guest-set-time "{\"time\":$(date +%s%N)}" 2>&1 >/dev/null) || true
read_clocks exp4-s
s_off=$(sub "$RT" "$HOST")
s_mono_step=$(sub "$s_mono_before" "$MONO")
s_rt_step=$(sub "$s_rt_before" "$RT")
s_mono_total=$(sub "$mono0" "$MONO")
s_rtc_off=$(awk -v r="$RTC" -v h="$HOST" 'BEGIN { printf "%.0f", r - h }')

abs() { awk -v x="$1" 'BEGIN { print (x < 0 ? -x : x) }'; }
lt() { awk -v a="$(abs "$1")" -v b="$2" 'BEGIN { exit !(a < b) }'; }

md "## Experiment 4: clocks across a ${WAIT} s gap between save and restore (\`-rtc base=utc,clock=vm\`)"
md ""
md "Guest clocksource: \`$clocksource\`; time sync in the guest (timedatectl NTP, systemd-timesyncd, chrony): \`$ntp\`."
md ""
md "Frozen (plain restore). The guest ran for about $ran s between the two readings; the host advanced $host_el s."
md ""
md "| Clock | Guest advance (s) | Advance beyond the time the guest ran (s) |"
md "| --- | --- | --- |"
md "| CLOCK_REALTIME | $f_rt | $(sub "$ran" "$f_rt") |"
md "| CLOCK_MONOTONIC | $f_mono | $(sub "$ran" "$f_mono") |"
md "| CLOCK_BOOTTIME | $f_boot | $(sub "$ran" "$f_boot") |"
md "| RTC (/sys/class/rtc/rtc0/since_epoch, 1 s resolution) | $f_rtc | $(sub "$ran" "$f_rtc") |"
md ""
md "Guest wall clock behind the host after the frozen restore: $f_off s."
md ""
md "Stepped (restore, then guest-set-time with the host's time):"
md ""
md "| Measurement | Value |"
md "| --- | --- |"
md "| Guest behind host before guest-set-time (s) | $s_off_before |"
md "| Guest behind host after guest-set-time (s) | $s_off |"
md "| CLOCK_REALTIME step across guest-set-time (s) | $s_rt_step |"
md "| CLOCK_MONOTONIC advance across guest-set-time (s) | $s_mono_step |"
md "| CLOCK_MONOTONIC advance since the pre-checkpoint reading (s) | $s_mono_total |"
md "| RTC minus host after guest-set-time (s) | $s_rtc_off |"
md "| guest-set-time error | ${set_err:-none} |"
verdict_header
verdict "frozen: CLOCK_REALTIME does not advance while saved" "$(is lt "$(sub "$ran" "$f_rt")" 1)" "advanced $f_rt s over $host_el s of host time"
verdict "frozen: CLOCK_MONOTONIC does not advance while saved" "$(is lt "$(sub "$ran" "$f_mono")" 1)" "advanced $f_mono s"
verdict "frozen: RTC (clock=vm) consistent with the guest clock" "$(is lt "$(sub "$f_rtc" "$f_rt")" 2)" "RTC advanced $f_rtc s, CLOCK_REALTIME $f_rt s"
verdict "stepped: wall clock matches the host after guest-set-time" "$(is lt "$s_off" 0.5)" "guest behind host by $s_off s (was $s_off_before s)"
verdict "stepped: CLOCK_MONOTONIC does not jump" "$(is lt "$s_mono_step" 1)" "monotonic advanced $s_mono_step s while realtime stepped $s_rt_step s"
