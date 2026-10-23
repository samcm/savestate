# savestate design

savestate runs QEMU/KVM virtual machines on Kubernetes, saves their complete state
(RAM, CPU and device state, disk) as named checkpoints, and starts any number of new
VMs from a checkpoint. It is a Kubernetes operator written in Rust that drives stock
QEMU over QMP. It shares its QEMU, QMP and guest-agent code with
[orbd](https://github.com/samcm/orbd).

Status: the engine runs: controller, node agent, launcher and CLI boot, checkpoint
and restore sandboxes end to end (transfers are phase 4). This document is the
contract every part of the implementation follows; change it in the same pull request
as any change to the behaviour it describes.

## User contract

- A **sandbox** is one Linux VM (for example 4 vCPU, 8 GiB RAM, 60 GiB disk) on one
  node, booted from a disk image or started from a checkpoint. It runs until it is
  deleted. You run commands in it and copy files in and out.
- A **checkpoint** is a named, read-only, complete picture of a sandbox at one
  instant: RAM, CPU and device state, and disk. Taking one pauses the sandbox while
  its RAM is written out, then it carries on. The pause grows with the RAM the guest
  is using: 1–2 s for an idle 8 GiB guest; for a busy one, about its used RAM
  divided by the node disk's write rate (18 s for 6.8 GiB at 400 MB/s, about 7 s at
  1 GB/s). A checkpoint is the only kind of saved state.
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
| Randomness | Every restored sandbox gets fresh kernel entropy (new VM generation ID, which makes the guest kernel reseed, plus an explicit reseed from the guest agent). The kernel tells userspace nothing about a restore; programs that must react use the after-restore hooks. Secrets that programs already derived stay identical across siblings. |
| Identity | Each restored sandbox gets a new machine-id, hostname and SSH host keys. Its in-guest IP and MAC are unchanged, which is safe because every sandbox has its own isolated network namespace. |
| Memory | Every running sandbox costs its full memory size. Nothing shares or overcommits guest RAM; KSM stays off. |
| Placement | A sandbox from a checkpoint starts on a node that holds the checkpoint. Checkpoints live on node-local disk. Copies to other nodes are explicit `Transfer`s (see below). |
| Lifetime | A checkpoint lives until its TTL expires or it is deleted, and cannot be deleted while a sandbox's disk still builds on it. |
| Not provided | Network connections to the outside surviving a restore; restoring on a different QEMU build (the build is pinned per checkpoint automatically). |

## API

API group `savestate.dev`, version `v1alpha1` (placeholder until the group is final).
Kubernetes 1.32 to 1.37 is supported; CI runs the API and controller tests on kind
at both ends of that range. All three kinds are namespaced and
refer to each other by name within their namespace. Validation uses CEL rules and schema constraints in the CRDs; there are no
admission webhooks. The Rust types are `crates/savestate-api`; `savestate crds` prints
the CRDs, which are committed under `deploy/crds/`.

```yaml
kind: Sandbox            # short name sbx
metadata: {name: web-1}
spec:
  image: ghcr.io/example/base@sha256:…   # or: from: {checkpoint: web-1-parked}
  vcpus: 4               # vcpus, memory, disk: required with image, refused with from
  memory: 8Gi
  disk: 60Gi
  # clock: stepped       # frozen (default) | stepped; only with from
  ttl: 7d
status:
  phase: Running         # Pending | WaitingForTransfer | Booting | Restoring | Running | Failed
  node: node-a
  from: {checkpoint: web-1-parked}
  message: …             # why it is in this phase, mainly for Failed
---
kind: Checkpoint         # short name ckpt
metadata: {name: web-1-parked}
spec: {sandbox: web-1, ttl: 48h}
status:
  phase: Ready           # Pending | Saving | Ready | Failed
  nodes: [node-a]        # every node holding a complete copy
  vcpus: 4               # shape of the checkpointed VM; sandboxes from it get
  memory: 8Gi            #   the same
  disk: 60Gi
  memoryBytes: …         # bytes the sparse RAM file occupies
  diskBytes: …           # bytes of the disk layer this checkpoint adds
  guestTime: …           # guest wall clock at the checkpoint
  parent: …              # checkpoint this one's sandbox was started from, if any
  fingerprint:           # what a restore must run exactly
    launcherImage: …     # by digest
    qemu: …              # QEMU version
    machineType: pc-q35-11.1
    cpuModel: …          # host, or a named model
    hostCpu: …           # with host: the node CPU (model, flags digest)
    devices: …           # digest of the device configuration
  message: …
---
kind: Transfer           # short name xfer; after the MVP
metadata: {name: parked-to-node-b}
spec:
  checkpoint: web-1-parked
  to: {node: node-b}     # later: {store: …} for off-cluster export
status:
  phase: Copying         # Pending | Copying | Ready | Failed
  from: node-a
  bytesDone: …
  bytesTotal: …
  message: …
```

- A sandbox has exactly one of `image` and `from`. An image is pinned by digest. A
  sandbox from an image sets `vcpus`, `memory` and `disk`; one started from a
  checkpoint inherits image, vCPUs, memory and disk from it and setting them is
  rejected (admission cannot see the checkpoint, so equal values are refused too).
  `clock` is only accepted with `from`; unset means `frozen`.
- Specs are immutable except `ttl`: changing a sandbox's source, shape or clock, a
  checkpoint's sandbox, or anything in a transfer is rejected.
- A TTL is whole numbers each followed by `d`, `h`, `m` or `s` (`7d`, `48h`,
  `1d12h`), counted from the object's creation. The installation config can set a
  default and a maximum per kind.
- The controller writes Sandbox status; the node agent doing the work writes
  Checkpoint and Transfer status (the controller marks them `Failed` when they cannot
  start). `Failed` is terminal for every kind.
- `kubectl get savestate` lists all three kinds. Field selectors work on
  `status.node` and `spec.from.checkpoint` (sandboxes), `spec.sandbox`
  (checkpoints), and `spec.checkpoint`, `spec.to.node` and `status.from`
  (transfers), so node agents watch only their own node's objects.
- A completed `Transfer` adds its node to the checkpoint's `status.nodes`. The copy
  belongs to the checkpoint; deleting the `Transfer` afterwards only deletes the
  record. Deleting a `Transfer` that is still copying cancels it and removes partial
  files. Each node runs at most `transfers.perNode` transfers at a time (default one).
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
                                 │ images, GC, transfers,   │
                                 │ device plugin, seccomp   │
                                 └────────────┬─────────────┘
                                   unix socket│
┌─────────────────────────────────────────────▼──────────────┐
│ VM pod (Guaranteed, not privileged): savestate launcher →  │
│ qemu (QMP) · no service-account token · /dev/kvm,          │
│ /dev/net/tun, /dev/vhost-vsock from the node agent's       │
│ device plugin · seccomp profile savestate/vm.json          │
│ tap + nftables NAT inside the pod's network namespace      │
│        └─ vsock ─▶ savestate-guest (exec, files, fixups)    │
└────────────────────────────────────────────────────────────┘
```

One static `savestate` binary provides every role (`controller`, `node`, `launcher`
and the CLI); `savestate-guest` runs inside the VM.

- **Controller** (Deployment, one replica): creates one Pod per Sandbox, in the
  sandbox's namespace, pins sandboxes from checkpoints to a holding node, enforces
  TTLs, creates and schedules Transfers, keeps finalizers that protect checkpoints
  still in use. VM pods run the launcher image from the installation config, by
  default the controller's own image at the digest its pod runs.
- **Node agent** (DaemonSet): owns `/var/lib/savestate` on its node; caches base images
  by digest; tells launchers on its node to boot, restore or checkpoint; garbage-collects
  layers, checkpoints and run directories whose objects are gone; serves and fetches
  transfers; reports disk use. As a kubelet device plugin it hands VM pods their
  devices, and it installs their seccomp profile. Only it holds API credentials on
  the node.
- **Launcher** (one per VM pod): starts and supervises QEMU, talks QMP, does the
  guest-agent work around boots, restores and checkpoints, streams the serial console
  to the pod log. It has no Kubernetes credentials; the node agent drives it over a
  unix socket (gRPC, `proto/savestate/launcher/v1/launcher.proto`). Exec and file copy
  reach the guest through `savestate guest`, which the CLI runs in the VM pod with
  `pods/exec` and which talks to the guest agent over vsock.
- **Guest agent** (`savestate-guest`, inside the VM, built on orbd's guest library):
  exec, file copy, the pre-checkpoint hook, and post-restore fixups (clock, entropy,
  machine-id, hostname, SSH host keys, user hooks in `/etc/savestate/after-restore.d/`).
  It speaks savestate's own guest protocol (`proto/savestate/guest/v1/guest.proto`) on
  vsock port 1024, one operation per connection; the launcher uses its host-side client
  (`savestate-guest-proto`).

The guest kernel is the latest stable Linux, configured from orbd's guest config plus a
small fragment, and the base disk image is a generic
Debian 13 with systemd and the agent that each installation builds its own image on
(`guest/README.md`, which also states the devices and kernel command line a guest
expects).

savestate acts only in the sandbox namespaces the installation config lists; objects
elsewhere are ignored. The controller and node agent get their permissions there
through RoleBindings; cluster-wide, the controller only reads nodes.

Capacity accounting is plain Kubernetes: each VM is a Guaranteed pod whose memory
request is guest RAM plus the installation's memory overhead (`vm.memoryOverhead`,
default 512 MiB), so the scheduler and ResourceQuota work unchanged. The spike
measured QEMU's own resident memory outside guest RAM at 30–60 MiB. In the end-to-end
test the VM pod's cgroup peak (QEMU, the launcher and KVM's kernel memory) was about
250 MiB for a 1 GiB guest whose memory file held about 170 MiB, so everything outside
the guest's RAM stayed under 100 MiB; the default keeps a wide margin.

## Controller

The controller runs one sandbox and one checkpoint controller (kube-rs `Controller`)
per configured namespace. It writes only through server-side apply (field manager
`savestate-controller`) or create, and reports what a user would want to know as
Kubernetes Events on the Sandbox or Checkpoint.

**VM pods.** Each Sandbox gets exactly one Pod, named after it, in its namespace,
owned by it (deleting the sandbox deletes the pod) and labelled
`app.kubernetes.io/name: savestate` and `app.kubernetes.io/component: vm`:

- one container, `launcher`, running `savestate launcher` from the launcher image:
  the installation's (`launcherImage`, else the controller's own image by digest)
  for a sandbox from an image, the checkpoint's `fingerprint.launcherImage` for a
  sandbox from a checkpoint. The pod spec is the record of which image a sandbox
  runs; the controller never changes an existing pod.
- Guaranteed QoS: CPU requests and limits are `vcpus`; memory requests and limits are
  guest memory plus `vm.memoryOverhead`; and one `savestate.dev/vm`, the node agent's
  device plugin resource that brings `/dev/kvm`, `/dev/net/tun` and
  `/dev/vhost-vsock`. For a sandbox from a checkpoint, shape comes from the
  checkpoint's status.
- `automountServiceAccountToken: false`, `enableServiceLinks: false`,
  `restartPolicy: Never`, `vm.nodeSelector` and `vm.tolerations`. The container is
  never privileged. Its security context:

  ```yaml
  runAsUser: 0
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: true
  capabilities: {drop: [ALL], add: [NET_ADMIN, CHOWN, SETUID, SETGID]}
  seccompProfile: {type: Localhost, localhostProfile: savestate/vm.json}
  ```

  The capabilities are what the launcher uses as root before it drops to the QEMU
  user for good (see "Network and security"); the seccomp profile is the node
  agent's (see "Devices and seccomp").
- arguments `--egress-allow` and `--dns` from `network`, the node's addresses
  (`status.hostIPs`) in `SAVESTATE_HOST_IPS`, and `terminationMessagePolicy:
  FallbackToLogsOnError` (the launcher writes why its VM stopped to its termination
  log).
- for a sandbox from a checkpoint, required node affinity on `metadata.name` in the
  checkpoint's `status.nodes`.
- host paths: `stateDir` at the same path, and the sandbox's run directory
  `/run/savestate/<namespace>/<sandbox>/<sandbox UID>/` at `/run/savestate`. The UID
  keeps a later sandbox of the same name from meeting an earlier one's files.

Before creating the pod the controller refuses (sandbox `Failed`) an image outside
`images.registries`, a shape not in `vm.shapes`, and a checkpoint that is `Failed`,
being deleted, or `Ready` without fingerprint, vCPUs or memory. It waits (sandbox
`Pending` with a message) while the checkpoint does not exist, is not yet `Ready` or
is on no node. A pod of the same name owned by an earlier sandbox is waited out.
Transfers are not created yet (phase 4).

**Sandbox status** is derived from the pod alone:

| Pod | Sandbox |
| --- | --- |
| absent, not yet placed | `Pending` (message says what it waits for) |
| not scheduled | `Pending`, message from the scheduler's `PodScheduled` condition |
| on a node, not Ready | `Booting` (from an image) or `Restoring` (from a checkpoint); `node` set; message from a waiting container (e.g. an image that cannot be pulled) |
| Ready | `Running`, and it stays `Running` while the pod runs |
| `Failed` or `Succeeded`, being deleted, or gone after placement | `Failed` with the launcher's exit code or the pod's reason |

`Failed` is terminal: the pod is kept for its logs and never replaced.

**TTL.** A Sandbox or Checkpoint is deleted (with a UID precondition) once
`creationTimestamp` + TTL has passed, where TTL is its own `ttl`, else the configured
`ttl.<kind>.default`, capped at `ttl.<kind>.max` (which also applies to objects with
no TTL). An `Expired` event records it.

**Checkpoints that cannot start.** A `Pending` checkpoint whose sandbox does not
exist, is being deleted or is not `Running` (read fresh from the API server) is
marked `Failed` with a message and a `Failed` event; the apply carries the
checkpoint's resource version, so it loses to a node agent that has taken the
checkpoint meanwhile. The node agent on the sandbox's node takes the rest.

**Checkpoint finalizer.** Every Checkpoint carries the finalizer
`savestate.dev/in-use`. While it is being deleted, the controller removes the
finalizer only once no Sandbox has `spec.from.checkpoint` naming it or a checkpoint
descended from it (following `status.parent`), reading both lists fresh from the API
server; until then an `InUse` event names the sandboxes. Once the object is gone, the
node agent removes the checkpoint's files.

**Contract with the node agent and launcher.**

| Who | Writes |
| --- | --- |
| Controller | VM pods; Sandbox `status` (all of it); TTL deletions; the Checkpoint finalizer; `Failed` on Checkpoints that cannot start; later Transfers |
| Node agent | Checkpoint `status` (`phase`, `nodes`, `vcpus`, `memory`, `disk`, sizes, `guestTime`, `parent`, `fingerprint`, `message`); Transfer `status`; files under `stateDir`; removes run directories of sandboxes that are gone |
| Launcher | listens on `/run/savestate/launcher.sock` in its pod (the run directory on the host); creates `/run/savestate/ready` once the VM runs and never removes it while the VM lives. A readiness probe (`test -e /run/savestate/ready`, every 2 s) turns that into the pod's Ready condition, which the controller reads. It exits non-zero when QEMU exits or the VM cannot start, and zero when its pod is deleted. |

The node agent finds a sandbox's launcher at
`/run/savestate/<namespace>/<sandbox>/<sandbox UID>/launcher.sock` on its node, and a
VM pod's image (for a checkpoint's `fingerprint.launcherImage`) in the pod spec.

## Node agent and launcher

**Starting a VM.** The node agent watches the VM pods on its node. Once a pod's
launcher listens and waits, it allocates the VM's vsock context id (context ids are
global on a host: the lowest free one from 100000, kept in the run directory as
`vsock-cid`, so it lives exactly as long as the sandbox; a restore gets its own), makes
sure the base image or checkpoint is on the node, and sends `Start`: boot an image
with the sandbox's shape and `vm.cpuModel`, or restore a checkpoint with its clock.
Starts that cannot happen (an image that cannot be pulled, a checkpoint not on this
node) are sent as `Fail`, and the launcher exits with that message, so the sandbox
fails with it. A booted guest gets its address, route, DNS and hostname from the
launcher through the guest agent; a restored one keeps what its checkpoint had and
gets the post-restore fixups.

**Base images.** A sandbox's `image` is a container image whose filesystem holds
`/rootfs.qcow2` (the `disk` stage of `guest/rootfs/Dockerfile`, or an installation's
equivalent). The node agent pulls it by digest (credentials from `images.auth`, plain
HTTP for `images.plainHttp`), checks the manifests against their digests, extracts the
disk, refuses one with a backing file, and publishes it read-only by rename.

**Taking a checkpoint.** The node agent takes `Pending` checkpoints of sandboxes
`Running` on its node: it creates `checkpoints/<UID>.tmp/`, then sets the phase to
`Saving` with the checkpoint's resource version (so it never overrides the
controller's `Failed`), so every `Saving` checkpoint has a directory on its node. The
launcher runs the checkpoint steps below, writing the memory file and
`manifest.json` into that directory; the node agent publishes it by rename and writes
`Ready` with the manifest's shape, sizes, guest time, parent and fingerprint (the
launcher image is the VM pod's), and a `Saved` event with the pause. A save that fails
(including because its sandbox is deleted meanwhile) is discarded and `Failed`.
After a node agent restart, a `Saving` checkpoint with a directory on the node is
finished from disk: published, or complete (manifest written) and published now, is
`Ready`; anything else is discarded and `Failed`.

**Garbage collection.** Every minute the node agent lists its files, then the
Sandboxes and Checkpoints of its namespaces (fresh), and removes checkpoints (and
saves) whose object is gone, layers no existing sandbox owns and no remaining
checkpoint's chain (including a complete save's) uses, images no sandbox boots and no
chain uses, and run directories of sandboxes that are gone. Files are listed first, so
a file created meanwhile is never taken for garbage. It reports bytes per kind and
free bytes as metrics (`savestate_node_disk_bytes`, `savestate_node_disk_free_bytes`)
on `--metrics-listen` (default `0.0.0.0:9090`).

**Devices and seccomp.** The node agent is a kubelet device plugin (device plugin API
v1beta1, `proto/kubelet/deviceplugin/v1beta1/api.proto`, copied from Kubernetes). At
start-up it first installs the VM pods' seccomp profile as
`<kubeletDir>/seccomp/savestate/vm.json` (written to a temporary file, fsynced and
renamed), then serves the plugin on `<kubeletDir>/device-plugins/savestate.sock`
and registers the resource `savestate.dev/vm` with the kubelet, so no VM pod is
admitted on a node before its profile is there. It advertises `vm.slotsPerNode`
healthy slots (default 110, the kubelet's default pod limit, so CPU and memory
requests decide placement unless an installation caps VMs per node lower), and
allocating any slot hands the container `/dev/kvm`, `/dev/net/tun` and
`/dev/vhost-vsock` (read-write; the same device nodes for every VM). The kubelet
removes every plugin socket when it restarts; the node agent notices its socket is
gone and registers again. While no node agent runs on a node, its kubelet stops
offering slots there, so no new VM pod starts where nothing would start its VM;
running VMs are unaffected.

The profile is containerd's default (`contrib/seccomp.DefaultProfile`, v2.4.2, for
amd64; the VM pod's capabilities add no rules to it) with one rule added:
`socket()` may create `AF_VSOCK` sockets, the guest agent's channel, which the
default refuses. Nothing else is widened. QEMU additionally applies its own filter
(`-sandbox on`).

**CPU model.** With a named `vm.cpuModel`, an init container of the node agent's pod
(privileged, for `/dev/kvm`) starts QEMU once with `-cpu <vm.cpuModel>,enforce`; on a
node that cannot run the model it fails with the features the node lacks as its
termination message, and the node agent does not run there. With `host` it only
records that guests get the node's CPU.

The controller serves Prometheus metrics (`--metrics-listen`, default
`0.0.0.0:9090`, at `/metrics`): reconcile counts, errors and durations per kind,
sandbox phase transitions and TTL deletions. Logs are JSON lines from `tracing`.

## QEMU mechanics

Pinned by the launcher image: stock QEMU 11.1 on Debian 13 (built from the upstream
release until Debian 13 backports ships 11.1 for amd64) and an explicit
versioned machine type (`pc-q35-11.1`). The guest CPU model is installation
configuration (`vm.cpuModel`), one of:

- `host`: guests get the node's own CPU. The simple choice while checkpoints stay
  on the node that took them, as they do until transfers exist. The checkpoint's
  fingerprint records the node's CPU (`hostCpu`: model name and a digest of its
  feature flags), and a restore on a node whose CPU differs is refused.
- a named model that every node in the cluster can run (for example `EPYC-Rome-v4`
  where Zen 2 and Zen 3 hosts mix), without `invtsc`: what lets checkpoints move
  between nodes later. The launcher passes `-cpu <model>,enforce`, so a host that
  lacks a feature refuses to start rather than silently weakening the model; the
  node agent probes the configured model that way at start-up and reports a node
  that cannot run it. (Nested hosts often hide speculation
control features: on nested Zen 3 and Zen 4 hosts `EPYC-Rome-v4` is refused.) The
launcher image digest and the CPU model are part of every checkpoint's fingerprint,
and restores run that exact image. Measured numbers are in `spike/RESULTS.md`.

Checkpoint (sandbox keeps running):

1. Guest agent pre-checkpoint hook (drops the page cache so the RAM file is smaller).
2. `stop`.
3. `blockdev-snapshot`: a new overlay becomes the sandbox's active disk; the previous
   top layer is now immutable and belongs to the checkpoint. The overlay is created
   with `qemu-img create -f qcow2 -u -b <top layer> -F qcow2 <new> <size>` (`-u` and
   an explicit size, so `qemu-img` does not open the layer the running QEMU has open)
   and added with `blockdev-add` (`"backing": null`) before `blockdev-snapshot`.
4. Migrate to a file with `multifd` + `mapped-ram` (and `direct-io`), with
   `max-bandwidth` raised far above the disk's rate: QEMU's default of 128 MiB/s
   applies to a save to file too and otherwise dominates the pause. The RAM file is
   sparse (only non-zero pages are written) and written in parallel; the save runs at
   the node disk's write rate.
5. On `completed`: fsync, write `manifest.json`, publish by rename.
6. `cont`. (After a completed migration `cont` reactivates the block devices and
   restarts the guest; see `qmp_cont` in QEMU.)

Restore (any number of times, concurrently):

1. `qemu-img create -f qcow2 -b <checkpoint layer> -F qcow2 <new overlay>`.
2. Start QEMU with the checkpoint's exact machine arguments, the new overlay,
   `-device vmgenid,guid=auto`, a vsock context id of its own and `-incoming defer -S`
   (`-S` keeps the guest stopped after the incoming migration until the launcher's
   `cont`).
3. Set the same migration capabilities, then `migrate-incoming` from the RAM file
   (opened read-only, so concurrent restores share one file) with
   `exit-on-error: false`.
4. On `completed`: `cont`, then guest-agent fixups, in this order: stepped clock if
   asked, host entropy credited to the kernel, new machine-id, hostname, SSH host keys
   (where present), hooks (with `SAVESTATE_SANDBOX`, `SAVESTATE_CHECKPOINT` and
   `SAVESTATE_CLOCK`). Connections open at the checkpoint are reset by the restore; the
   launcher reconnects.

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
  images/<hex>.qcow2                 base images by manifest digest, read-only
  layers/<sandbox UID>-<n>.qcow2     disk layers: a sandbox's nth, frozen (read-only)
                                     once a checkpoint takes it
  checkpoints/<UID>/manifest.json    shape, layer chain, machine arguments, guest time,
                                     sizes, parent, fingerprint
  checkpoints/<UID>/memory           mapped-ram file (sparse)
  checkpoints/<UID>.tmp/             save in progress; complete once manifest.json exists
  trash/                             delete-by-rename
```

Checkpoint directories are named by the Checkpoint's UID. A layer names its backing
file relative to its own directory (`<id>.qcow2`, `../images/<hex>.qcow2`), and a
manifest lists its chain relative to the state directory. Every publication is
build-then-rename and fsynced; the node agent reconciles what is on disk at start-up
(a complete save is published, an incomplete one discarded, interrupted image pulls
removed), so a crash at any point converges (the orbd pattern). The node agent owns
the directory; `layers/` and each save's `.tmp` directory belong to the QEMU user, who
writes them. Chains grow by one layer per checkpoint; capping their depth and
flattening them in the background is phase 3.

## Network and security

- Each VM pod is its own network namespace: QEMU attaches a tap device (`tap0`,
  `10.0.2.2/24`); nftables in the pod (table `inet savestate`) masquerades guest
  traffic, drops traffic to private, shared, loopback, link-local, multicast and
  reserved ranges and to the node's own addresses, except `network.egressAllow` and
  port 53 of the guest's DNS servers, drops IPv6, and lets nothing reach the guest or
  the pod from it unsolicited. Every sandbox sees the same guest address
  (`10.0.2.15/24` via `10.0.2.2`, MAC `52:54:00:12:34:56`); DNS is `network.dns`, else
  the VM pod's own resolvers. Forwarding is enabled per interface through netlink.
- The launcher builds that network as root, hands its run directory to the QEMU user
  (uid and gid 65532), and then drops to that user for good, keeping only the groups
  that own `/dev/kvm`, `/dev/net/tun` and `/dev/vhost-vsock`. QEMU runs as that user
  with `-sandbox on`; the launcher has no service-account token. Its container is
  never privileged: devices come from the node agent's device plugin, and the
  container keeps only `NET_ADMIN` (the tap device, forwarding and nftables in its
  own network namespace), `CHOWN` (handing the run directory to the QEMU user),
  `SETUID` and `SETGID` (becoming it), under savestate's seccomp profile.
- Exec and file copy use Kubernetes `pods/exec` RBAC; there is no separate API server.
  `savestate exec` and `cp` run `savestate guest` in the VM pod, which reaches the
  guest agent over vsock.
- VM pods need `NET_ADMIN` in their own network namespace and `hostPath` volumes
  (the node's state directory and their run directory). The baseline Pod Security
  level forbids both (a Localhost seccomp profile and device-plugin devices would
  pass it), so sandbox namespaces need the privileged level. They therefore hold
  only sandboxes; users get the `savestate-user` role
  (savestate objects, read pods, `pods/exec`) and never pod creation there. A
  NetworkPolicy refuses all ingress to VM pods (`app.kubernetes.io/component: vm`):
  exec goes through the kubelet and the node agent uses a unix socket.

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
error, and unknown fields are refused. The schema is `savestate_api::config::Config`:
`namespaces` (required), `stateDir`, `kubeletDir`, `launcherImage`, `vm` (`cpuModel`
required, `memoryOverhead`, `shapes`, `nodeSelector`, `tolerations`,
`slotsPerNode`), `network`
(`egressAllow`, `dns`), `images` (`registries`, `auth`), `transfers.perNode` and
`ttl` (`default` and `max` per kind). `deploy/base` is a kustomize base with
placeholder values and `deploy/README.md` lists what an installation supplies.

## Phases

| Phase | Scope |
| --- | --- |
| 0. Spike | Measure and prove the QEMU mechanics above on real KVM hosts: save/restore timings for an 8 GiB guest, parent `cont` after checkpoint, three concurrent restores from one RAM file and one frozen layer (image locking), frozen and stepped clocks, VM generation ID change, MAC behaviour, baseline CPU model on Zen 2 and Zen 3, cross-CPU restore. |
| 1. Engine | Shared crates with orbd; launcher, layer store, checkpoint and restore flows, guest agent fixups, CLI creating VM pods directly. |
| 2. Operator | CRDs, controller, node agent, GC and retention, metrics, install manifests. |
| 3. Hardening | Failure injection, disk-full, node reboot, upgrades, soak. |
| 4. Transfers | `Transfer` kind, node-to-node copy, export to an off-cluster store. |
| Later | `Migration`, lazy restore, optional KSM. |
