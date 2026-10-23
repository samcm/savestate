# Phase 0 results: the QEMU mechanics on real KVM

This spike tests the "QEMU mechanics" section of [docs/design.md](../docs/design.md)
on a real KVM host and records the numbers. The verdicts and measurement tables come
from one run of [`.github/workflows/spike.yml`](../.github/workflows/spike.yml), run
[39908830258](https://github.com/samcm/savestate/actions/runs/39908830258), copied unchanged
from its `spike-results` artifact, as is the cross-CPU self-test; the bandwidth
comparison and the Intel run are named where they appear. That artifact also holds the
command lines (`<exp>.log`) and every QMP and guest-agent exchange (`<exp>.qmp.log`).

**Result: all 25 checks pass.** The mechanics work as designed. Six
points in the design needed changing (a seventh, direct kernel boot, is already
there); they are listed [at the end](#where-the-design-needs-to-change) and applied to
`docs/design.md` in the same pull request. The ones that matter most:

- **Bandwidth limit:** QEMU's default `max-bandwidth` of 128 MiB/s also throttles a
  save to file. The launcher must raise it.
- **Pause length:** the checkpoint pause is about the guest's non-zero RAM divided by
  the disk's write rate, so "a few seconds" holds only for mostly idle guests or fast
  disks.
- **CPU model:** `EPYC-Rome-v4` does not run under `enforce` on the nested Zen 3 and
  Zen 4 runners.

## Setup

- **Host:** a GitHub-hosted `ubuntu-24.04` runner. It is a VM, so this is nested KVM.
  The Host table below gives its CPU, memory and scratch-disk speed.
- **QEMU:** the launcher's own build, stock QEMU 11.1.2 from the upstream release,
  built by the `base-amd64` stage of the repository's [`Dockerfile`](../Dockerfile).
  The experiments run inside that image (`--privileged`, for `/dev/kvm`, tap devices
  and dropping the host page cache); [`install-tools.sh`](install-tools.sh) adds the
  spike's tools and checks the version.
- **Guest:** the current Debian 13 generic cloud image (the Host table records its
  version and kernel). It is prepared once with cloud-init to install
  `qemu-guest-agent`, turn off time sync and disable cloud-init. Its kernel and
  initrd boot directly (point 5). savestate's own guest kernel and base image are not
  used: they did not exist when the spike started, and the mechanics do not depend on
  them.
- **Machine:** `-machine pc-q35-11.1,accel=kvm`, `-cpu <model>,enforce`,
  `-rtc base=utc,clock=vm`, virtio-blk on `-blockdev` qcow2, virtio-net,
  `-device vmgenid,guid=auto`, guest agent on virtio-serial.
  [`lib.sh`](lib.sh) `vm_start` builds the full command line, and every `<exp>.log`
  records it.
- **Network:** the launcher's QEMU has no user-mode (slirp) backend, so
  [`net.sh`](net.sh) gives a guest that needs the network a tap device on a bridge
  with nftables NAT, as the design does. VMs that need no network get a NIC on an
  empty hub; their guests log virtio-net transmit timeouts, which are harmless here.
- **Migration:** capabilities `multifd` and `mapped-ram`; parameters
  `multifd-channels 4`, `direct-io` as stated per row, and `max-bandwidth 100 GiB/s`
  (point 1).

To reproduce on any KVM host with Docker:

```sh
docker build --target base-amd64 -t savestate-qemu -f Dockerfile .
docker run --rm --privileged -v "$PWD:/repo" -v /big/disk:/work -w /repo savestate-qemu \
  bash -c 'spike/install-tools.sh && spike/run-all.sh /work /repo/spike-out'
```

## The sequences that were measured

Checkpoint while the VM keeps running ([`lib.sh`](lib.sh) `checkpoint`). The QMP
exchange below is from `exp2-checkpoint-running.qmp.log`, with replies trimmed:

```text
-> migrate-set-capabilities {multifd: true, mapped-ram: true}
-> migrate-set-parameters {multifd-channels: 4, direct-io: true, max-bandwidth: 107374182400}
-> stop                                                   <- event STOP
   $ qemu-img create -f qcow2 -u -b <top>.qcow2 -F qcow2 <new>.qcow2 <virtual size>
-> blockdev-add {driver: qcow2, node-name: ovl-1, file: {driver: file, filename: <new>}, backing: null}
-> blockdev-snapshot {node: disk, overlay: ovl-1}         # <top> is now the frozen layer
-> migrate {uri: "file:<ckpt>/memory"}
   poll query-migrate until status "completed"
   fsync(<ckpt>/memory)
-> cont                                                   <- event RESUME
```

Restore ([`lib.sh`](lib.sh) `restore`):

```text
   $ qemu-img create -f qcow2 -b <frozen>.qcow2 -F qcow2 <overlay>.qcow2
   $ qemu-system-x86_64 <same machine arguments> -blockdev ...<overlay>... -incoming defer -S
-> migrate-set-capabilities {multifd: true, mapped-ram: true}
-> migrate-set-parameters {multifd-channels: 4, direct-io: true, ...}
-> migrate-incoming {uri: "file:<ckpt>/memory", exit-on-error: false}
   poll query-migrate until status "completed"
-> cont
   guest-ping (guest agent) until it answers
```

## Verdicts

| Experiment | Check | Verdict |
| --- | --- | --- |
| exp1-save-restore | dirty RAM intact after restore (direct-io false) | **PASS** |
| exp1-save-restore | dirty RAM intact after restore (direct-io true) | **PASS** |
| exp1-save-restore | idle RAM file is sparse | **PASS** |
| exp2-checkpoint-running | original keeps running after cont | **PASS** |
| exp2-checkpoint-running | new writes land in the new overlay | **PASS** |
| exp2-checkpoint-running | frozen layer unchanged | **PASS** |
| exp2-checkpoint-running | checkpoint holds the disk at the checkpoint | **PASS** |
| exp3-concurrent-restores | qemu-img create -b <frozen> works while the parent runs | **PASS** |
| exp3-concurrent-restores | image locking allows three restores plus the running parent | **PASS** |
| exp3-concurrent-restores | copies are independent | **PASS** |
| exp3-concurrent-restores | restores resume mid-flight | **PASS** |
| exp3-concurrent-restores | frozen layer unchanged | **PASS** |
| exp3-concurrent-restores | every VM has its own generation ID | **PASS** |
| exp4-clock | frozen: CLOCK_REALTIME does not advance while saved | **PASS** |
| exp4-clock | frozen: CLOCK_MONOTONIC does not advance while saved | **PASS** |
| exp4-clock | frozen: RTC (clock=vm) consistent with the guest clock | **PASS** |
| exp4-clock | stepped: wall clock matches the host after guest-set-time | **PASS** |
| exp4-clock | stepped: CLOCK_MONOTONIC does not jump | **PASS** |
| exp5-vmgenid-mac | generation ID differs per restore | **PASS** |
| exp5-vmgenid-mac | guest kernel is notified of the new generation ID | **PASS** |
| exp5-vmgenid-mac | MAC unchanged after restore | **PASS** |
| exp6-cpu-model | boots with a named model under enforce | **PASS** |
| exp6-cpu-model | restore presents the same CPU | **PASS** |
| exp6-cpu-model | no invariant TSC in the guest | **PASS** |
| exp6-cpu-model | work runs after restore | **PASS** |

## Measurements

Copied from the run's `results.md`.

### Host

| | |
| --- | --- |
| CPU | AMD EPYC 7763 64-Core Processor, 4 threads |
| Memory | 15 GiB |
| Kernel | 6.17.0-1022-azure |
| KVM nested | 1 |
| QEMU | QEMU emulator version 11.1.2; QEMU emulator version 11.1.2, upstream source built by the Dockerfile's qemu stage, sha256 c0dcd3cf93f475f5 |
| Scratch filesystem | ext4, 84G free of 145G |
| Scratch disk, dd O_DIRECT | 4 GiB in 4 MiB blocks: write 422 MB/s, read 413 MB/s; 2 GiB in 512 KiB blocks: write 405 MB/s |
| Guest | Debian 13.7 generic cloud image (sha512 16e0965312c56feb), kernel 6.12.113+deb13-cloud-amd64 |

### CPU model selection

Host CPU: AMD EPYC 7763 64-Core Processor (AuthenticAMD).

| Model (`-cpu MODEL,enforce`) | Result |
| --- | --- |
| EPYC-Rome-v4 | refused, lacks: amd-ssbd amd-stibp ibpb ibrs perfctr-core wbnoinvd |
| EPYC-Rome-v3 | refused, lacks: amd-ssbd amd-stibp ibpb ibrs perfctr-core wbnoinvd |
| EPYC-Rome | refused, lacks: amd-stibp ibpb perfctr-core wbnoinvd |
| EPYC-v4 | refused, lacks: ibpb perfctr-core |
| EPYC | runs |

Chosen: `EPYC`.

### Experiment 1: save and restore, 8G guest, 4 vCPUs, multifd (4 channels) + mapped-ram

Guest RAM in use (`free`): idle 0.26 GiB, dirty 6.76 GiB (6656 MiB of random data in a tmpfs).
Source QEMU resident while dirty: 6.98 GiB, of which outside guest RAM 0.06 GiB.

Save (the VM is paused from `stop` to `cont`; the host page cache was dropped before each save):

| State | direct-io | Pause total (s) | blockdev-snapshot (s) | migrate (s) | fsync (s) | QEMU total-time (ms) | Non-zero pages | Write rate | RAM file apparent | RAM file allocated |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| idle | false | 1.295 | 0.058 | 0.883 | 0.287 | 528 | 95017 | 318 MiB/s | 8.01 GiB | 0.36 GiB |
| idle | true | 1.583 | 0.058 | 1.444 | 0.013 | 1412 | 95017 | 255 MiB/s | 8.01 GiB | 0.36 GiB |
| dirty | false | 17.929 | 0.059 | 17.451 | 0.323 | 14477 | 1786273 | 393 MiB/s | 8.01 GiB | 6.81 GiB |
| dirty | true | 17.920 | 0.059 | 17.747 | 0.013 | 17702 | 1786262 | 393 MiB/s | 8.01 GiB | 6.81 GiB |

Restore (cold host page cache; `-incoming defer -S`, migrate-incoming, cont):

| State | direct-io | QEMU start + caps (s) | migrate-incoming (s) | cont → guest agent answers (s) | Total incl. qemu-img create (s) | QEMU resident | QEMU outside guest RAM |
| --- | --- | --- | --- | --- | --- | --- | --- |
| idle | false | 0.172 | 0.693 | 0.126 | 1.054 | 0.44 GiB | 0.03 GiB |
| idle | true | 0.171 | 0.680 | 0.102 | 1.027 | 0.44 GiB | 0.03 GiB |
| dirty | false | 0.167 | 17.466 | 0.118 | 17.826 | 6.89 GiB | 0.03 GiB |
| dirty | true | 0.176 | 17.481 | 0.028 | 17.762 | 6.89 GiB | 0.03 GiB |

| Check | Verdict | Evidence |
| --- | --- | --- |
| dirty RAM intact after restore (direct-io false) | **PASS** | b2sum of 6656 MiB tmpfs file: saved `3dcf23a4f104812e45e8fbf2d41b234b`, restored `3dcf23a4f104812e45e8fbf2d41b234b` |
| dirty RAM intact after restore (direct-io true) | **PASS** | b2sum of 6656 MiB tmpfs file: saved `3dcf23a4f104812e45e8fbf2d41b234b`, restored `3dcf23a4f104812e45e8fbf2d41b234b` |
| idle RAM file is sparse | **PASS** | apparent 8.01 GiB, allocated 0.36 GiB |

### Experiment 2: checkpoint while the VM keeps running (4G guest, direct-io true)

| Step | Result |
| --- | --- |
| Pause, `stop` → `cont` returned (excludes the checksum taken before cont) | 1.076 s (blockdev-snapshot 0.061 s, migrate 0.927 s, fsync 0.015 s, cont 0.042 s) |
| RAM file | apparent 4.01 GiB, allocated 0.29 GiB |
| Run state after cont | running, guest uptime 5.30 s and counting |
| Active node after blockdev-snapshot | `ovl-1` (exp2-p-1.qcow2) |
| Frozen layer node read-only (qcow2 node / file node) | true / false |
| Guest CLOCK_REALTIME advance across the pause | 0.326 s while the host advanced 1.407 s; the guest ran for about 0.299 s of that |
| New overlay allocated before → after a 64 MiB write | 0.00 GiB → 0.07 GiB |
| Frozen layer sha256 (prefix) before cont → after the writes | `6f637869c26d23ff` → `6f637869c26d23ff` |
| Restore from the checkpoint sees in /root | `before.txt ` |

| Check | Verdict | Evidence |
| --- | --- | --- |
| original keeps running after cont | **PASS** | query-status running; guest wrote 64 MiB and synced; I/O errors in dmesg: 0 |
| new writes land in the new overlay | **PASS** | overlay grew by 0.07 GiB |
| frozen layer unchanged | **PASS** | sha256 6f637869c26d23ff = 6f637869c26d23ff |
| checkpoint holds the disk at the checkpoint | **PASS** | restore lists: before.txt ; original lists: after.bin after.txt before.txt  |

### Experiment 3: three concurrent restores from one RAM file and one frozen layer (2G guests, direct-io true)

Parent paused 0.926 s for the checkpoint, then kept running. Counter in the parent just before the checkpoint: 21 (ticks every 0.1 s).
All three restores (qemu-img create, QEMU start, migrate-incoming, cont, guest agent answers) took 1.904 s wall time together.

| VM | Restore total (s) | migrate-incoming (s) | Counter right after restore | VM generation ID | /root after every VM wrote its own file |
| --- | --- | --- | --- | --- | --- |
| parent | - | - | - | `7a6d5b7c-4f20-4bd6-a69c-bc43f243c455` | `before parent-after ` |
| exp3-c1 | 1.671 | 1.315 | 26 | `a2f77299-9311-4d84-a44b-b6371c256f52` | `before exp3-c1 ` |
| exp3-c2 | 1.825 | 1.453 | 26 | `056c4027-4822-4098-9d90-0acb9bd484a0` | `before exp3-c2 ` |
| exp3-c3 | 1.889 | 1.594 | 27 | `eca47950-4a30-4937-a704-2360d85740b8` | `before exp3-c3 ` |

Frozen layer sha256 (prefix) before the restores → after all writes: `da14f3f85e8cdce3` → `da14f3f85e8cdce3`.

| Check | Verdict | Evidence |
| --- | --- | --- |
| qemu-img create -b <frozen> works while the parent runs | **PASS** | three overlays created without -u |
| image locking allows three restores plus the running parent | **PASS** | three QEMUs opened the frozen layer as backing while the parent held it |
| copies are independent | **PASS** | each VM sees only its own file plus the checkpoint's |
| restores resume mid-flight | **PASS** | counter 21 before the checkpoint; 26, 26, 27 right after the restores, and still counting |
| frozen layer unchanged | **PASS** | sha256 da14f3f85e8cdce3 = da14f3f85e8cdce3 |
| every VM has its own generation ID | **PASS** | 4 distinct IDs across parent and three restores |

### Experiment 4: clocks across a 60 s gap between save and restore (`-rtc base=utc,clock=vm`)

Guest clocksource: `kvm-clock`; time sync in the guest (timedatectl NTP, systemd-timesyncd, chrony): `no inactive inactive`.

Frozen (plain restore). The guest ran for about 0.305 s between the two readings; the host advanced 61.832 s.

| Clock | Guest advance (s) | Advance beyond the time the guest ran (s) |
| --- | --- | --- |
| CLOCK_REALTIME | 0.340 | 0.035 |
| CLOCK_MONOTONIC | 0.338 | 0.033 |
| CLOCK_BOOTTIME | 0.338 | 0.033 |
| RTC (/sys/class/rtc/rtc0/since_epoch, 1 s resolution) | 1 | 0.695 |

Guest wall clock behind the host after the frozen restore: 61.705 s.

Stepped (restore, then guest-set-time with the host's time):

| Measurement | Value |
| --- | --- |
| Guest behind host before guest-set-time (s) | 62.602 |
| Guest behind host after guest-set-time (s) | 0.046 |
| CLOCK_REALTIME step across guest-set-time (s) | 63.160 |
| CLOCK_MONOTONIC advance across guest-set-time (s) | 0.609 |
| CLOCK_MONOTONIC advance since the pre-checkpoint reading (s) | 0.948 |
| RTC minus host after guest-set-time (s) | -1 |
| guest-set-time error | none |

| Check | Verdict | Evidence |
| --- | --- | --- |
| frozen: CLOCK_REALTIME does not advance while saved | **PASS** | advanced 0.340 s over 61.832 s of host time |
| frozen: CLOCK_MONOTONIC does not advance while saved | **PASS** | advanced 0.338 s |
| frozen: RTC (clock=vm) consistent with the guest clock | **PASS** | RTC advanced 1 s, CLOCK_REALTIME 0.340 s |
| stepped: wall clock matches the host after guest-set-time | **PASS** | guest behind host by 0.046 s (was 62.602 s) |
| stepped: CLOCK_MONOTONIC does not jump | **PASS** | monotonic advanced 0.609 s while realtime stepped 63.160 s |

### Experiment 5: VM generation ID and MAC address across restores (2G guests)

vmgenid driver bound in the guest: `/sys/bus/platform/drivers/vmgenid/QEMUVGID:00`; ACPI device and kernel log: `QEMUVGID:00;[    0.000990] ACPI: SSDT 0x000000007FFE12C6 0000CA (v01 BOCHS  VMGENID  00000001 BXPC 00000001)`.

| VM | NIC MAC on the command line | VM generation ID (QMP) | NEW_VMGENID uevents / kernel “virtual machine fork” reseeds in the guest | MAC inside the guest | MAC in QEMU's receive filter | DNS through the NIC |
| --- | --- | --- | --- | --- | --- | --- |
| parent (before checkpoint) | 52:54:00:12:34:56 | `ce09111f-fd09-41b4-ac0f-214980207c5e` | 0 / 0 | 52:54:00:12:34:56 | - | - |
| restore 1 | 52:54:00:12:34:56 | `40e38021-6f70-4d1f-8630-67aa8180b9cf` | 0 / 1 | 52:54:00:12:34:56 | 52:54:00:12:34:56 | ok |
| restore 2 | 52:54:00:ab:cd:ef | `5cdb3e90-35e1-45b2-8331-615b0cf5ba3e` | 0 / 1 | 52:54:00:12:34:56 | 52:54:00:12:34:56 | ok |

| Check | Verdict | Evidence |
| --- | --- | --- |
| generation ID differs per restore | **PASS** | parent ce09111f…, restores 40e38021…, 5cdb3e90… |
| guest kernel is notified of the new generation ID | **PASS** | RNG reseeds for a VM fork: parent 0, restores 1 and 1 |
| (information) userspace notification | - | NEW_VMGENID=1 uevents: parent 0, restores 0 and 0 |
| MAC unchanged after restore | **PASS** | guest MAC 52:54:00:12:34:56 → 52:54:00:12:34:56; network ok |

### Experiment 6: named CPU model `EPYC`

Host: AMD EPYC 7763 64-Core Processor, 102 CPU flags. Guest clocksource: `kvm-clock`.

| | Guest model name | Guest CPU flags | Flags hash | TSC flags | sha256 workload |
| --- | --- | --- | --- | --- | --- |
| booted | AMD EPYC Processor | 76 | `1271d37622a25ed1` | none | `281e519df3077b55` |
| restored | AMD EPYC Processor | 76 | `1271d37622a25ed1` | none | `281e519df3077b55` |

| Check | Verdict | Evidence |
| --- | --- | --- |
| boots with a named model under enforce | **PASS** | `-cpu EPYC,enforce` |
| restore presents the same CPU | **PASS** | flags hash 1271d37622a25ed1 → 1271d37622a25ed1 |
| no invariant TSC in the guest | **PASS** | TSC flags: none |
| work runs after restore | **PASS** | 281e519df3077b55 = 281e519df3077b55 |


## Effect of QEMU's default bandwidth limit

QEMU's `max-bandwidth` defaults to 128 MiB/s, and the limit also applies to a
migration to file from a stopped VM. The spike's first runs left it at the default,
with everything else the same as experiment 1 (8 GiB guest, 4 channels). Run
[39902683479](https://github.com/samcm/savestate/actions/runs/39902683479) used
Debian's build of the same QEMU 11.1.2 release (`1:11.1.2+ds-3`):

| State | direct-io | Pause, default limit (s) | Write rate, default limit | Pause, limit lifted, this run (s) | Write rate, limit lifted, this run |
| --- | --- | --- | --- | --- | --- |
| idle | false | 3.337 | 119 MiB/s | 1.295 | 318 MiB/s |
| idle | true | 2.749 | 144 MiB/s | 1.583 | 255 MiB/s |
| dirty | false | 48.439 | 145 MiB/s | 17.929 | 393 MiB/s |
| dirty | true | 46.585 | 151 MiB/s | 17.920 | 393 MiB/s |

With the limit lifted, a save writes at the disk's rate (`dd` with `O_DIRECT`: about
420 MB/s, roughly 400 MiB/s), and restores read at the same rate. On this disk,
`direct-io` makes no measurable difference to either. It does keep the 7 GiB file
out of the host page cache.

## Cross-CPU restore

[`cross-cpu.sh`](cross-cpu.sh) saves a running guest on one host and restores it on
another. Its default run builds the launcher's QEMU from this checkout's `Dockerfile`
and loads that image on both hosts, so both run the identical binary. The restore
refuses to start if the QEMU binary's sha256 differs. It then checks that:

- the guest agent answers
- the counter process continues
- the guest sees the same model name and CPU flags
- a workload gives the same results
- the kernel logs no new errors

```sh
spike/cross-cpu.sh --cpu EPYC-Rome-v4 zen2-host zen3-host   # then swap the hosts
```

The workflow runs it with this runner as both hosts (over ssh to localhost, `-cpu
EPYC`). That self-test passes, which proves the script works but says nothing about
crossing CPU generations:

| Check | Saved | Restored | Verdict |
| --- | --- | --- | --- |
| host CPU | AMD EPYC 7763 64-Core Processor | AMD EPYC 7763 64-Core Processor | PASS |
| QEMU build, guest CPU model | QEMU emulator version 11.1.2, sha256 c0dcd3cf93f475f5, EPYC | QEMU emulator version 11.1.2, sha256 c0dcd3cf93f475f5, EPYC | PASS |
| guest agent answers after cont | - | after 0.101 s (restore total 0.992 s, checkpoint pause 0.892 s) | PASS |
| guest model name and CPU flags hash | AMD EPYC Processor, flags 1271d37622a25ed1 | AMD EPYC Processor, flags 1271d37622a25ed1 | PASS |
| workload result | 341aacac661ccb21 3982835174 305585115 | 341aacac661ccb21 3982835174 305585115 | PASS |
| file written before the checkpoint | saved-on-the-first-host | saved-on-the-first-host | PASS |
| counter continues (10 s apart) | 27 | 31 → 130 | PASS |
| kernel error lines (dmesg) | 2 | 2 | PASS |

**Not yet done:** a Zen 2 ↔ Zen 3 run on real hardware. Phase 0's cross-CPU item
stays open until someone runs the script there.

## Where the design needs to change

1. **Set `max-bandwidth` on every checkpoint.** QEMU's default of 128 MiB/s throttles
   a save to file even while the VM is stopped. On this host, lifting the limit cut
   the pause for a 6.8 GiB dirty guest from 46.6 s to 17.920 s. **Fix:** in
   checkpoint step 4, set `max-bandwidth` explicitly (the spike uses 100 GiB/s) along
   with the capabilities.
2. **The pause is not always "a few seconds".** It is about (non-zero guest RAM) ÷
   (disk write rate):
   - 8 GiB idle guest (0.36 GiB non-zero after dropping the page cache): 1.3–1.6 s.
   - 4 GiB idle guest: 1.076 s.
   - 8 GiB guest with 6.8 GiB dirty: 17.920 s on a ~400 MB/s disk.

   **Fix:** the user contract should say the pause grows with the RAM the guest is
   using: roughly 1 s per GiB at 1 GB/s, so a busy 8 GiB sandbox needs an NVMe-class
   node disk to stay at a few seconds. Make node disk bandwidth a sizing input. Keep
   the pre-checkpoint page-cache drop; it matters.
3. **Restore must start QEMU with `-S`.** Without `-S`, QEMU starts the guest as soon
   as the incoming migration completes, before the launcher's step 4 (`cont`). This
   is QEMU's documented autostart behaviour; the spike always passed `-S` and did not
   test without it. **Fix:** restore step 2 should read `-incoming defer -S`.
4. **Spell out how the checkpoint overlay is made.** The spike runs
   `qemu-img create -f qcow2 -u -b <top> -F qcow2 <new> <virtual size>` (`-u` and an
   explicit size, so `qemu-img` does not open the layer the running QEMU has open),
   then `blockdev-add` with `"backing": null`, then `blockdev-snapshot`. It did not
   try without `-u`. The restore side needs nothing extra: a plain
   `qemu-img create -b <frozen> -F qcow2` works while the parent runs. **Fix:** say so
   in checkpoint step 3.
5. **Boot the kernel directly** (already the design). On the GitHub runners, the
   Debian cloud image's GRUB handed over to the kernel and the VM reset at once, with
   every CPU model and both machine types tried; the same kernel boots fine with
   `-kernel/-initrd`, so the spike does that. The root cause was not investigated.
   savestate's own guest (`guest/README.md`) already boots its kernel directly, so
   **no change** is needed; do not add a firmware boot path without testing it on
   nested hosts.
6. **The CPU model must be checked on every node.** Under `enforce`, `EPYC-Rome-v4`
   does not run on the nested runners, neither the Zen 3 (EPYC 7763) nor the Zen 4
   (EPYC 9V74) ones: the outer hypervisor hides `perfctr-core`, `amd-ssbd`,
   `amd-stibp`, `ibpb`, `ibrs` and `wbnoinvd`. Only `EPYC` (v1) runs, and every
   experiment here used it. On an Intel runner (Xeon Platinum 8573C) every newer
   model lacks at least `spec-ctrl`, and `Broadwell` (v1) is the newest that runs;
   all 25 checks also pass there (run
   [39907986333](https://github.com/samcm/savestate/actions/runs/39907986333)). If no
   candidate runs, [`select-cpu.sh`](select-cpu.sh) takes the one lacking the fewest
   features with those turned off. Bare-metal hosts are not expected to hide
   features like this. **Fix:**
   - The launcher always uses `-cpu <model>,enforce`, so a model is never silently
     weakened.
   - The node agent probes the configured model with `enforce` at start-up and
     reports nodes that cannot run it, rather than failing restores.
   - Confirm `EPYC-Rome-v4` on real Zen 2 and Zen 3 hosts with `cross-cpu.sh`.
7. **Userspace hears nothing of a restore.** The design's own mechanism works: each
   restore gets a fresh generation ID from `guid=auto`, and the Linux 6.12 guest
   kernel reseeds its RNG ("crng reseeded due to virtual machine fork"). But no
   uevent follows (the 6.12 `vmgenid` driver only calls `add_vmfork_randomness`), so
   programs in the guest cannot see the fork. The guest agent's post-restore hooks are
   the only signal userspace gets. **Fix:** say so in the Randomness row.

Notes that need no change to the design:

- **Memory overhead is far below the estimate.** QEMU's resident memory outside guest
  RAM was 30–60 MiB, against the design's default margin of 512 MiB. A restored
  guest is resident only for its non-zero pages: 0.44 GiB right after restoring an
  idle 8 GiB guest. Keep the Guaranteed request at guest RAM plus a margin, and measure the pod
  cgroup (which also counts KVM kernel memory) in phase 1.
- **The MAC lives in the device state.** The virtio-net MAC travels in the migrated
  device state: a restore launched with a different `mac=` still had the saved MAC
  in both the guest and QEMU's receive filter, and its network worked. The launcher
  can neither change nor break it.
- **The frozen layer stays open read-write at the file level.** After
  `blockdev-snapshot`, the frozen layer's qcow2 node is read-only but its file node
  stays open read-write. No locking conflict resulted: three restores and the running
  parent shared it, and its checksum never changed.
- **Time the save from the outside.** QEMU's reported `total-time` can be shorter than
  the time until `query-migrate` says `completed` (528 ms against 0.88 s for the idle
  save without direct-io, where writes are still buffered).
  The launcher should time to `completed`.
- **QEMU source.** Debian 13 backports still has no amd64 QEMU 11.1 binary: its amd64
  archive has `1:11.0.2+ds-2~bpo13+1`, and the 11.1.2 backport fails to build on
  amd64. `docs/design.md` and the `Dockerfile` already cover this by building 11.1.2
  from the upstream release, and every number above is from that build. Earlier runs
  on Debian's own 11.1.2 package gave the same verdicts and, with the bandwidth limit
  lifted, the same timings.
- **The launcher's QEMU has no user-mode network.** It is built without slirp, so
  even test guests need a tap device; the spike uses a bridge with nftables NAT.
