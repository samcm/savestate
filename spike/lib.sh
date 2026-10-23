# Shared helpers for the spike experiments; sourced by every exp*.sh.
#
# Environment:
#   WORK              scratch directory on a filesystem that supports O_DIRECT (ext4, xfs)
#   OUT               results directory: <exp>.md (tables), <exp>.log (command lines,
#                     timings), <exp>.qmp.log (every QMP and guest-agent exchange)
#   CPU_MODEL         guest CPU model (default: $WORK/cpu-model, written by select-cpu.sh)
#   MACHINE           machine type (default pc-q35-11.1)
#   MULTIFD_CHANNELS  multifd channels for save and restore (default 4)
#   ACCEL             accelerator (default kvm; tcg only for dry runs of the QMP flows)
set -euo pipefail

SPIKE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
: "${WORK:?set WORK}" "${OUT:?set OUT}"
MACHINE=${MACHINE:-pc-q35-11.1}
CPU_MODEL=${CPU_MODEL:-$(cat "$WORK/cpu-model" 2>/dev/null || echo EPYC-Rome-v4)}
CHANNELS=${MULTIFD_CHANNELS:-4}
# Every disk layer lives in one directory and names its backing file by bare file
# name, so a checkpoint's layer chain can be copied elsewhere as a directory.
LAYERS=$WORK/layers
mkdir -p "$OUT" "$LAYERS" "$WORK/vm" "$WORK/ckpt"

EXP=${EXP:-$(basename "$0" .sh)}
LOG=$OUT/$EXP.log
MD=$OUT/$EXP.md
export QMP_LOG=$OUT/$EXP.qmp.log

# --- output -------------------------------------------------------------------------

now() { date +%s.%N; }
sub() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.3f", b - a }'; }
gib() { awk -v b="$1" 'BEGIN { printf "%.2f GiB", b / 1073741824 }'; }
log() { echo "[$(date -u +%T)] $*" | tee -a "$LOG" >&2; }
md() { printf '%s\n' "$*" >>"$MD"; }
is() { if "$@"; then echo PASS; else echo FAIL; fi; }
# verdict NAME PASS|FAIL DETAIL: one row of the experiment's verdict table.
verdict() {
  md "| $1 | **$2** | $3 |"
  echo "$EXP | $1 | $2 | $3" >>"$OUT/verdicts.txt"
  log "VERDICT $2: $1 ($3)"
}
verdict_header() { md "" "| Check | Verdict | Evidence |" "| --- | --- | --- |"; }

# --- VMs ----------------------------------------------------------------------------

vmdir() { echo "$WORK/vm/$1"; }
qmp() { local n=$1; shift; python3 "$SPIKE/qmp.py" qmp "$(vmdir "$n")/qmp.sock" "$@"; }
qga() { local n=$1; shift; python3 "$SPIKE/qmp.py" qga "$(vmdir "$n")/qga.sock" "$@"; }
gexec() { local n=$1; shift; python3 "$SPIKE/qmp.py" exec "$(vmdir "$n")/qga.sock" "$@"; }
gping() { python3 "$SPIKE/qmp.py" ping "$(vmdir "$1")/qga.sock" "${2:-300}"; }

# vm_start NAME MEM DISK [QEMU ARGS...]: start QEMU (daemonized) on DISK. A restore
# uses the same function, so a restored VM gets exactly the same machine arguments.
# $MAC overrides the NIC's MAC address, $SMP the vCPU count.
vm_start() {
  local name=$1 mem=$2 disk=$3
  shift 3
  local d
  d=$(vmdir "$name")
  rm -rf "$d"
  mkdir -p "$d"
  local cmd=(
    qemu-system-x86_64 -name "$name"
    -machine "$MACHINE,accel=${ACCEL:-kvm}" -cpu "$CPU_MODEL,enforce" -smp "${SMP:-2}" -m "$mem"
    -rtc base=utc,clock=vm
    -nodefaults -no-user-config -display none
    -serial "file:$d/serial.log"
    -qmp "unix:$d/qmp.sock,server=on,wait=off"
    -chardev "socket,id=qga0,path=$d/qga.sock,server=on,wait=off"
    -device virtio-serial-pci,id=vser0
    -device virtserialport,chardev=qga0,name=org.qemu.guest_agent.0
    -blockdev "driver=file,filename=$disk,node-name=disk-file"
    -blockdev "driver=qcow2,file=disk-file,node-name=disk"
    -device virtio-blk-pci,drive=disk,id=vd0
    -netdev user,id=net0
    -device "virtio-net-pci,netdev=net0,id=nic0,mac=${MAC:-52:54:00:12:34:56}"
    -device vmgenid,guid=auto,id=vmgenid0
    -pidfile "$d/pid" -daemonize
    "$@"
  )
  log "\$ ${cmd[*]}"
  "${cmd[@]}"
  echo disk >"$d/top-node"
  echo "$disk" >"$d/top-file"
  echo 0 >"$d/gen"
}

vm_pid() { cat "$(vmdir "$1")/pid"; }

vm_quit() {
  local pid
  pid=$(vm_pid "$1")
  qmp "$1" quit >/dev/null 2>&1 || true
  for _ in $(seq 100); do kill -0 "$pid" 2>/dev/null || return 0; sleep 0.1; done
  kill -9 "$pid" 2>/dev/null || true
}

# boot NAME MEM: a fresh VM on a new overlay of the base image; waits for the agent.
boot() {
  local name=$1 mem=$2
  shift 2
  local ov=$LAYERS/$name-0.qcow2 t0
  rm -f "$ov"
  qemu-img create -q -f qcow2 -b base.qcow2 -F qcow2 "$ov"
  t0=$(now)
  vm_start "$name" "$mem" "$ov" "$@"
  gping "$name" 300
  log "boot $name: guest agent up after $(sub "$t0" "$(now)") s"
}

# vm_rss NAME: "TOTAL_KIB GUEST_RAM_KIB" resident; guest RAM is the largest mapping.
vm_rss() {
  awk '/^Size:/ { size = $2 } /^Rss:/ { total += $2; if (size > max) { max = size; ram = $2 } }
       END { print total, ram }' "/proc/$(vm_pid "$1")/smaps"
}

# set_migration NAME DIRECT_IO: the capabilities and parameters save and restore share.
set_migration() {
  qmp "$1" migrate-set-capabilities \
    '{"capabilities":[{"capability":"multifd","state":true},{"capability":"mapped-ram","state":true}]}' >/dev/null
  qmp "$1" migrate-set-parameters "{\"multifd-channels\":$CHANNELS,\"direct-io\":$2}" >/dev/null
}

drop_host_caches() {
  sync
  echo 3 >/proc/sys/vm/drop_caches 2>/dev/null || log "cannot drop the host page cache (not privileged)"
}

# Guest clocks as "REALTIME MONOTONIC BOOTTIME" in seconds.
guest_clocks() {
  gexec "$1" "python3 -c 'import time as t; print(*(\"%.6f\" % t.clock_gettime(c) for c in (t.CLOCK_REALTIME, t.CLOCK_MONOTONIC, t.CLOCK_BOOTTIME)))'"
}

# checkpoint NAME DIR DIRECT_IO [cont|nocont]: the design's checkpoint while the VM
# keeps running: stop, blockdev-snapshot onto a new overlay (the old top layer is frozen
# and belongs to the checkpoint), migrate RAM and device state to DIR/memory with
# multifd + mapped-ram, fsync, cont. Writes the timings to DIR/info (shell syntax).
checkpoint() {
  local name=$1 ckpt=$2 dio=$3 cont=${4:-cont}
  local d top_node top_file n new size
  d=$(vmdir "$name")
  top_node=$(cat "$d/top-node")
  top_file=$(cat "$d/top-file")
  n=$(($(cat "$d/gen") + 1))
  new=$LAYERS/$name-$n.qcow2
  rm -rf "$ckpt"
  mkdir -p "$ckpt"
  size=$(qemu-img info -U --output=json "$top_file" | jq '."virtual-size"')
  set_migration "$name" "$dio"
  log "checkpoint $name -> $ckpt (direct-io=$dio, frozen layer $(basename "$top_file"))"
  local t0 t1 t2 t3 t4 t5
  t0=$(now)
  qmp "$name" stop >/dev/null
  t1=$(now)
  # -u: do not open the backing file (it is in use); the size is given instead.
  qemu-img create -q -f qcow2 -u -b "$(basename "$top_file")" -F qcow2 "$new" "$size"
  qmp "$name" blockdev-add "{\"driver\":\"qcow2\",\"node-name\":\"ovl-$n\",\"file\":{\"driver\":\"file\",\"filename\":\"$new\"},\"backing\":null}" >/dev/null
  qmp "$name" blockdev-snapshot "{\"node\":\"$top_node\",\"overlay\":\"ovl-$n\"}" >/dev/null
  t2=$(now)
  qmp "$name" migrate "{\"uri\":\"file:$ckpt/memory\"}" >/dev/null
  python3 "$SPIKE/qmp.py" wait-migrate "$d/qmp.sock" >"$ckpt/migrate.json"
  t3=$(now)
  python3 -c 'import os, sys; os.fsync(os.open(sys.argv[1], os.O_RDONLY))' "$ckpt/memory"
  t4=$(now)
  [ "$cont" = cont ] && qmp "$name" cont >/dev/null
  t5=$(now)
  echo "ovl-$n" >"$d/top-node"
  echo "$new" >"$d/top-file"
  echo "$n" >"$d/gen"
  cat >"$ckpt/info" <<EOF
layer=$top_file
t_stop=$t0
t_cont=$t5
snapshot_s=$(sub "$t1" "$t2")
migrate_s=$(sub "$t2" "$t3")
fsync_s=$(sub "$t3" "$t4")
pause_s=$(sub "$t0" "$t5")
qemu_total_ms=$(jq '."total-time"' "$ckpt/migrate.json")
normal_pages=$(jq '.ram.normal' "$ckpt/migrate.json")
zero_pages=$(jq '.ram.duplicate' "$ckpt/migrate.json")
apparent=$(stat -c %s "$ckpt/memory")
allocated=$(($(stat -c %b "$ckpt/memory") * $(stat -c %B "$ckpt/memory")))
EOF
  log "checkpoint $name: $(tr '\n' ' ' <"$ckpt/info")"
}

# restore NAME MEM LAYER MEMFILE DIRECT_IO [QEMU ARGS...]: the design's restore: a new
# overlay on the frozen layer, QEMU with the same machine arguments plus
# `-incoming defer -S`, the same migration capabilities, migrate-incoming from the RAM
# file, cont, wait for the guest agent. Writes the timings to $(vmdir NAME)/restore-info.
restore() {
  local name=$1 mem=$2 layer=$3 memfile=$4 dio=$5
  shift 5
  local ov=$LAYERS/$name-0.qcow2 t0 t1 t2 t3 t4 t5 d
  rm -f "$ov"
  t0=$(now)
  qemu-img create -q -f qcow2 -b "$(basename "$layer")" -F qcow2 "$ov"
  t1=$(now)
  vm_start "$name" "$mem" "$ov" -incoming defer -S "$@"
  d=$(vmdir "$name")
  set_migration "$name" "$dio"
  t2=$(now)
  qmp "$name" migrate-incoming "{\"uri\":\"file:$memfile\",\"exit-on-error\":false}" >/dev/null
  python3 "$SPIKE/qmp.py" wait-migrate "$d/qmp.sock" >"$d/incoming.json"
  t3=$(now)
  qmp "$name" cont >/dev/null
  t4=$(now)
  gping "$name" 120
  t5=$(now)
  cat >"$d/restore-info" <<EOF
create_s=$(sub "$t0" "$t1")
start_s=$(sub "$t1" "$t2")
load_s=$(sub "$t2" "$t3")
agent_s=$(sub "$t4" "$t5")
total_s=$(sub "$t0" "$t5")
t_cont=$t4
EOF
  log "restore $name: $(tr '\n' ' ' <"$d/restore-info")"
}

# Stop this experiment's VMs and delete its layers and checkpoints on exit.
cleanup() {
  local p
  for p in "$WORK"/vm/"$PREFIX"*/pid; do
    [ -f "$p" ] && kill -9 "$(cat "$p")" 2>/dev/null || true
  done
  rm -rf "$WORK"/vm/"$PREFIX"* "$WORK"/ckpt/"$PREFIX"* "$LAYERS"/"$PREFIX"*
}
PREFIX=${PREFIX:-${EXP%%-*}-}
trap cleanup EXIT
: >"$MD"
: >"$LOG"
: >"$QMP_LOG"
