# savestate design

savestate runs QEMU/KVM virtual machines on Kubernetes, saves their complete state
(RAM, CPU and device state, disk) as named checkpoints, and starts any number of new
VMs from a checkpoint. It is a Kubernetes operator written in Rust that drives stock
QEMU over QMP. It shares its QEMU, QMP and guest-agent code with
[orbd](https://github.com/samcm/orbd).

Status: design agreed, implementation starting. This document is the contract every
part of the implementation follows; change it in the same pull request as any change
to the behaviour it describes.

## User contract

- A **sandbox** is one Linux VM (for example 4 vCPU, 8 GiB RAM, 60 GiB disk) on one
  node, booted from a disk image or started from a checkpoint. It runs until it is
  deleted. You run commands in it and copy files in and out.
- A **checkpoint** is a named, read-only, complete picture of a sandbox at one
  instant: RAM, CPU and device state, and disk. Taking one pauses the sandbox for a
  few seconds; it then carries on. A checkpoint is the only kind of saved state.
- **Starting a sandbox from a checkpoint** gives a VM that wakes exactly where the
  checkpoint was: same processes mid-flight, same files, same in-guest network
  address. Any number of sandboxes can start from one checkpoint; each is fully
  independent of the others and of the original. Resuming a parked sandbox and
  branching ("forking") one are the same operation.
- Parking is explicit: checkpoint, delete the sandbox, later create a sandbox (the
  same name if you like) from the checkpoint.
- A sandbox whose VM crashes becomes `Failed`. Nothing restarts it implicitly.

Guarantees and limits:

| Topic | Behaviour |
| --- | --- |
| Clock | `frozen` (default): the guest's clock does not advance while it is paused or saved; a restored guest believes no time passed. `stepped`: after a restore the guest's wall clock is set to the host's current time. The monotonic clock never jumps. |
| Randomness | Every restored sandbox gets fresh kernel entropy (new VM generation ID plus an explicit reseed from the guest agent). Secrets that programs already derived stay identical across siblings. |
| Identity | Each restored sandbox gets a new machine-id, hostname and SSH host keys. Its in-guest IP and MAC are unchanged, which is safe because every sandbox has its own isolated network namespace. |
| Memory | Every running sandbox costs its full memory size. Nothing shares or overcommits guest RAM; KSM stays off. |
| Placement | A sandbox from a checkpoint starts on a node that holds the checkpoint. Checkpoints live on node-local disk. Copies to other nodes are explicit `Transfer`s (see below). |
| Lifetime | A checkpoint lives until its TTL expires or it is deleted, and cannot be deleted while a sandbox's disk still builds on it. |
| Not provided | Network connections to the outside surviving a restore; restoring on a different QEMU build (the build is pinned per checkpoint automatically). |

## API

API group `savestate.dev`, version `v1alpha1` (placeholder until the group is final).
Validation uses CEL rules in the CRDs; there are no admission webhooks.

```yaml
kind: Sandbox
metadata: {name: web-1}
spec:
  image: ghcr.io/example/base@sha256:…   # or: from: {checkpoint: web-1-parked}
  vcpus: 4
  memory: 8Gi
  disk: 60Gi
  clock: frozen          # frozen | stepped; only meaningful with `from`
  ttl: 7d
status:
  phase: Running         # Pending | WaitingForTransfer | Booting | Restoring | Running | Failed
  node: node-a
  from: {checkpoint: web-1-parked}
---
kind: Checkpoint
metadata: {name: web-1-parked}
spec: {sandbox: web-1, ttl: 48h}
status:
  phase: Ready           # Pending | Saving | Ready | Failed
  nodes: [node-a]        # every node holding a complete copy
  memoryBytes: …
  diskBytes: …
  guestTime: …           # guest wall clock at the checkpoint
  parent: …              # checkpoint this one's sandbox was started from, if any
  fingerprint: …         # launcher image, QEMU build, machine type, CPU model, device set
---
kind: Transfer           # after the MVP
metadata: {name: parked-to-node-b}
spec:
  checkpoint: web-1-parked
  to: {node: node-b}     # later: {store: …} for off-cluster export
status:
  phase: Copying         # Pending | Copying | Ready | Failed
  from: node-a
  bytesDone: …
  bytesTotal: …
```

- A sandbox started from a checkpoint inherits image, vCPUs, memory and disk from it;
  setting them differently is rejected.
- A completed `Transfer` adds its node to the checkpoint's `status.nodes`. The copy
  belongs to the checkpoint; deleting the `Transfer` afterwards only deletes the
  record. Deleting a `Transfer` that is still copying cancels it and removes partial
  files. The controller runs at most one transfer per node at a time.
- When a sandbox from a checkpoint fits on no node holding it, the controller creates
  a `Transfer` owned by that sandbox and the sandbox waits in `WaitingForTransfer`.
- Later, a `Migration` kind may move a running sandbox between nodes with QEMU live
  migration (for draining nodes). Not part of the MVP.

The CLI is shorthand over these objects: `savestate create`, `checkpoint`, `fork NAME
-n 3` (one checkpoint plus three sandboxes), `exec NAME -- CMD`, `cp`, `copy CKPT --to
NODE`, `delete`.

## Architecture

```text
┌────────────────────┐   CRDs    ┌──────────────────────────┐
│ savestate          │◀─────────▶│ kube-apiserver           │
│ controller         │           └────────────▲─────────────┘
│ Sandbox → Pod,     │                        │ watch (own node only)
│ TTL, transfers     │           ┌────────────┴─────────────┐
└────────────────────┘           │ savestate node           │
                                 │ (DaemonSet): layer store,│
                                 │ images, GC, transfers    │
                                 └────────────┬─────────────┘
                                   unix socket│
┌─────────────────────────────────────────────▼──────────────┐
│ VM pod (Guaranteed): savestate launcher → qemu (QMP)       │
│ no service-account token · /dev/kvm, /dev/net/tun          │
│ tap + nftables NAT inside the pod's network namespace      │
│        └─ vsock ─▶ savestate-guest (exec, files, fixups)    │
└────────────────────────────────────────────────────────────┘
```

One static `savestate` binary provides every role (`controller`, `node`, `launcher`
and the CLI); `savestate-guest` runs inside the VM.

- **Controller** (Deployment, one replica): creates one Pod per Sandbox, pins
  sandboxes from checkpoints to a holding node, enforces TTLs, creates and schedules
  Transfers, keeps finalizers that protect checkpoints still in use.
- **Node agent** (DaemonSet): owns `/var/lib/savestate` on its node; caches base images
  by digest; tells launchers on its node to checkpoint or restore; garbage-collects
  layers and checkpoints whose objects are gone; serves and fetches transfers;
  reports disk use. Only it holds API credentials on the node.
- **Launcher** (one per VM pod): starts and supervises QEMU, talks QMP, relays exec and
  file traffic to the guest over vsock, streams the serial console to the pod log.
  It has no Kubernetes credentials; the node agent drives it over a unix socket.
- **Guest agent** (inside the VM, from orbd): exec, file copy, and post-restore fixups
  (clock, entropy, machine-id, hostname, SSH host keys, user hooks in
  `/etc/savestate/after-restore.d/`).

Capacity accounting is plain Kubernetes: each VM is a Guaranteed pod whose memory
request is guest RAM plus QEMU overhead (measure; expect roughly 0.5 GiB), so the
scheduler and ResourceQuota work unchanged.

## QEMU mechanics

Pinned by the launcher image: stock QEMU 11.1 on Debian 13 (built from the upstream
release until Debian 13 backports ships 11.1 for amd64) and an explicit
versioned machine type (`pc-q35-11.1`). The guest CPU model is installation
configuration: a named model that every node in the cluster can run (for example
`EPYC-Rome-v4` where Zen 2 and Zen 3 hosts mix), never `-cpu host`, and without
`invtsc`, so checkpoints can later move between nodes. The launcher image digest and
the CPU model are part of every checkpoint's fingerprint, and restores run that exact
image.

Checkpoint (sandbox keeps running):

1. Guest agent pre-checkpoint hook (drops the page cache so the RAM file is smaller).
2. `stop`.
3. `blockdev-snapshot`: a new overlay becomes the sandbox's active disk; the previous
   top layer is now immutable and belongs to the checkpoint.
4. Migrate to a file with `multifd` + `mapped-ram` (and `direct-io`): the RAM file is
   sparse and written in parallel.
5. On `completed`: fsync, write `manifest.json`, publish by rename.
6. `cont`. (After a completed migration `cont` reactivates the block devices and
   restarts the guest; see `qmp_cont` in QEMU.)

Restore (any number of times, concurrently):

1. `qemu-img create -f qcow2 -b <checkpoint layer> -F qcow2 <new overlay>`.
2. Start QEMU with the checkpoint's exact machine arguments, the new overlay,
   `-device vmgenid,guid=auto` and `-incoming defer`.
3. Set the same migration capabilities, then `migrate-incoming` from the RAM file
   (opened read-only, so concurrent restores share one file) with
   `exit-on-error: false`.
4. On `completed`: `cont`, then guest-agent fixups (stepped clock if asked, entropy,
   identity, hooks).

Clock: QEMU's kvmclock restores its saved value without adding elapsed host time, so
`frozen` is QEMU's natural behaviour; `-rtc clock=vm` keeps the RTC consistent with it.
Guests run without NTP; savestate owns guest time. `stepped` sets the wall clock from
the host after `cont`.

Not used: CPR modes (same-host QEMU upgrade only), `background-snapshot` (incompatible
with multifd), `x-ignore-shared` (cannot give private copies), internal qcow2
snapshots (tie RAM to one image). Later: lazy restore (mapped-ram with postcopy on the
destination, in QEMU master after 11.1).

## Storage layout (per node)

```text
/var/lib/savestate/
  images/<digest>.qcow2           base images, read-only
  layers/<id>.qcow2               disk layers; sandboxes and checkpoints reference chains
  checkpoints/<id>/manifest.json  fingerprint, layer chain, guest time, sizes, lineage
  checkpoints/<id>/memory         mapped-ram file (sparse)
  checkpoints/<id>.tmp/           save in progress, discarded on recovery
  trash/                          delete-by-rename
```

Every publication is build-then-rename and fsynced; every component reconciles what
is on disk at start-up, so a crash at any point converges (the orbd pattern). Layer
chains are capped in depth and flattened in the background.

## Network and security

- Each VM pod is its own network namespace: QEMU attaches a tap device; nftables in the
  pod masquerades guest traffic and drops traffic to cluster, node, link-local and
  private ranges except an allowlist. Every sandbox sees the same guest address.
- QEMU runs as non-root with `-sandbox on`; the launcher has no service-account token;
  devices come from a device plugin rather than a privileged container where the
  cluster allows it.
- Exec and file copy use Kubernetes `pods/exec` RBAC; there is no separate API server.

## Configuration boundary

The repository holds behaviour, schemas, defaults that are right for most clusters,
and examples with placeholder values. Anything that describes one particular
installation is supplied at install time and never committed here:

| In the repository | Supplied by each installation |
| --- | --- |
| CRD schemas, controller and node behaviour | Namespaces, RBAC subjects, quotas |
| Generic defaults (state directory, transfer concurrency, TTL limits) and their config schema | Node selection, tolerations, which nodes run VMs |
| Install manifests with placeholders (kustomize base, example values) | Guest CPU model, VM shapes offered, memory overhead margin |
| Guest image build recipe (kernel config, minimal rootfs, guest agent) | Base images and what is preinstalled in them, registries, pull secrets |
| Test fixtures | Egress allowlists, DNS, off-cluster stores, credentials |

Installation config is one YAML file (operator and node agent) plus the usual
Kubernetes objects; every field has a documented default or is required with a clear
error.

## Phases

| Phase | Scope |
| --- | --- |
| 0. Spike | Measure and prove the QEMU mechanics above on real KVM hosts: save/restore timings for an 8 GiB guest, parent `cont` after checkpoint, three concurrent restores from one RAM file and one frozen layer (image locking), frozen and stepped clocks, VM generation ID change, MAC behaviour, baseline CPU model on Zen 2 and Zen 3, cross-CPU restore. |
| 1. Engine | Shared crates with orbd; launcher, layer store, checkpoint and restore flows, guest agent fixups, CLI creating VM pods directly. |
| 2. Operator | CRDs, controller, node agent, GC and retention, metrics, install manifests. |
| 3. Hardening | Failure injection, disk-full, node reboot, upgrades, soak. |
| 4. Transfers | `Transfer` kind, node-to-node copy, export to an off-cluster store. |
| Later | `Migration`, lazy restore, optional KSM. |
